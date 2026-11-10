//! The context every page is converted in: links, titles, people, the site.

use super::*;

/// How a comment on a page is converted: in the page's context, so a mention
/// (`[@Alice Ng](<profile>)` or `[@Alice](user:<key>)`) becomes a real mention
/// and `[Title](Other.md)` a page link, as they would in the page itself.
pub fn comment_convert_options(ws: &Workspace, page_path: &str) -> Result<ConvertOptions> {
    let mut opts = page_convert_options(ws, page_path)?;
    opts.inline_marks.clear();
    Ok(opts)
}

/// Conversion context for a page, built from what the workspace knows.
///
/// Every caller must use this (or [`ConvertContext::options`], which it
/// wraps). Rendering the same storage with different options produces
/// different Markdown, and confed compares those renderings against each
/// other — `diff` against the file on disk, and push's block patcher against
/// the base. Options assembled ad hoc at one call site show up as changes the
/// user never made.
///
/// A state database that cannot be read is an error, not an empty map: a
/// page rendered without its links would be recorded as clean with every link
/// broken.
pub fn page_convert_options(ws: &Workspace, page_path: &str) -> Result<ConvertOptions> {
    let cached = ConvertContext::cached(ws)?;
    let mut opts = cached.ctx.options(page_path, &cached.links);
    if let Some(record) = ws.state().get_page_by_path(page_path)? {
        opts.inline_marks = inline_marks_for(ws, &record.page_id)?;
    }
    Ok(opts)
}

/// A stored body rendered for comparison with a page file's body.
///
/// A file is read with its comment marks stripped (design 06 §3), so the
/// rendering is stripped too: anything that compares the two — `diff`, the
/// TUI's diff pane — must go through here, or every open inline comment shows
/// up as a change.
pub fn comparable_markdown(storage: &str, opts: &ConvertOptions) -> Result<String> {
    let markdown = confed_converter::storage_to_markdown(storage, opts)?.markdown;
    Ok(marks::strip(&markdown).body)
}

/// What converting any page of this workspace depends on beyond the page
/// itself, read once: titles, people, the site. Loading it touches no page
/// body, and an operation over many pages loads it once, not once per page.
#[derive(Clone, Debug, Default)]
pub struct ConvertContext {
    /// Titles as the server has them — not file names, which may be truncated
    /// or sanitized. Data Center links pages by title, so the same titles also
    /// find the local file for a link read from storage.
    pub(super) titles: HashMap<String, String>,
    /// People this workspace has already resolved. A mention of anyone else
    /// keeps its block verbatim rather than linking to the wrong profile.
    pub users: HashMap<String, confed_converter::UserLink>,
    pub(super) space_key: String,
    pub(super) base_url: String,
    pub(super) links_by_title: bool,
}

/// A [`ConvertContext`] with the links it was built beside, as of one
/// state of the index.
#[derive(Debug)]
pub struct CachedContext {
    generation: u64,
    pub(crate) ctx: Arc<ConvertContext>,
    pub(crate) links: Arc<HashMap<String, String>>,
}

impl ConvertContext {
    /// The context as of now, built afresh only when the index it comes
    /// from has moved since the last call: a push of many pages reads it
    /// once, not once per page.
    pub(crate) fn cached(ws: &Workspace) -> Result<CachedContext> {
        let generation = ws.state().index_generation();
        if let Some(cached) = ws.convert_cache.borrow().as_ref() {
            if cached.generation == generation {
                return Ok(CachedContext {
                    generation,
                    ctx: cached.ctx.clone(),
                    links: cached.links.clone(),
                });
            }
        }
        let ctx = ConvertContext::load(ws)?;
        let links = ctx.base_links(ws)?;
        let fresh = CachedContext { generation, ctx: Arc::new(ctx), links: Arc::new(links) };
        *ws.convert_cache.borrow_mut() =
            Some(CachedContext { generation, ctx: fresh.ctx.clone(), links: fresh.links.clone() });
        Ok(fresh)
    }

    pub fn load(ws: &Workspace) -> Result<Self> {
        let state = ws.state();
        let mut titles: HashMap<String, String> = state
            .remote_index()?
            .into_iter()
            .filter(|r| !r.deleted)
            .map(|r| (r.page_id, r.title))
            .collect();
        for page in state.page_index()? {
            titles.insert(page.page_id, page.title);
        }
        let users = state
            .all_users()?
            .into_iter()
            .map(|u| {
                (
                    u.id.clone(),
                    confed_converter::UserLink {
                        display_name: u.display_name,
                        profile_url: u.profile_url,
                        // Stored as the server wrote it; one confed does not
                        // know reads as a userkey, as an unnamed one would.
                        id_attr: confed_converter::UserIdAttr::parse(&u.id_attr)
                            .unwrap_or_default(),
                        id_value: u.id,
                    },
                )
            })
            .collect();
        Ok(Self {
            titles,
            users,
            space_key: ws.space_key().unwrap_or_default(),
            base_url: ws.base_url()?.unwrap_or_default(),
            links_by_title: ws.flavor()? == Some(confed_api::Flavor::DataCenter),
        })
    }

    /// page id → file path, for the pages the workspace has files for.
    pub fn base_links(&self, ws: &Workspace) -> Result<HashMap<String, String>> {
        Ok(ws.state().page_index()?.into_iter().map(|p| (p.page_id, p.local_path)).collect())
    }

    /// Options for the page whose file is `path`, with `links` (page id →
    /// file path) as the pages it can link to.
    pub fn options(&self, path: &str, links: &HashMap<String, String>) -> ConvertOptions {
        let mut page_links = confed_converter::PageLinks {
            by_id: links
                .iter()
                .map(|(id, target)| (id.clone(), paths::relative_link(path, target)))
                .collect(),
            ..Default::default()
        };
        let link_targets = page_links
            .by_id
            .iter()
            .map(|(id, link)| (link.clone(), confed_converter::PageTarget::Id(id.clone())))
            .collect();
        for (id, title) in &self.titles {
            if let Some(link) = page_links.by_id.get(id).cloned() {
                page_links
                    .by_space_title
                    .entry((self.space_key.clone(), title.clone()))
                    .or_insert_with(|| link.clone());
                page_links.by_title.entry(title.clone()).or_insert(link);
            }
        }
        ConvertOptions {
            attachment_dir: paths::sidecar_ref(path),
            page_links,
            link_targets,
            page_titles: self.titles.clone(),
            links_by_title: self.links_by_title,
            base_url: self.base_url.clone(),
            space_key: self.space_key.clone(),
            users: self.users.clone(),
            inline_marks: HashMap::new(),
        }
    }
}

/// Everything a pull needs to render its pages: where each will live, and the
/// context they are converted in.
pub(super) struct Rendering {
    pub(super) links: HashMap<String, String>,
    pub(super) ctx: ConvertContext,
}

impl Rendering {
    pub(super) fn options(&self, path: &str) -> ConvertOptions {
        self.ctx.options(path, &self.links)
    }
}

// --------------------------------------------------------- inline marks ----

/// Fingerprint of everything a page's Markdown was rendered from.
///
/// Two things make an already-synced file out of date without either side
/// changing: the converter's rules improve, or somebody the page mentions
/// becomes resolvable. Both belong in one value, so pull has a single question
/// to ask rather than a growing list of special cases.
pub fn render_key(storage: &str, users: &HashMap<String, confed_converter::UserLink>) -> String {
    let mut parts = vec![format!("converter={}", confed_converter::CONVERTER_VERSION)];
    let mut mentioned: Vec<String> = confed_converter::user_references(storage)
        .into_iter()
        .map(|(_, id)| match users.get(&id) {
            Some(user) => format!("{id}={}|{}", user.display_name, user.profile_url),
            None => format!("{id}=unresolved"),
        })
        .collect();
    mentioned.sort();
    parts.extend(mentioned);
    hash_str(&parts.join("\n"))
}
