//! Moving a page, with the pages under it, to another space of the same
//! instance (`confed instance mv`). Design note: docs/design/07-instance-roots.md.
//!
//! The server does the move: one call on the subtree's root, after which
//! Confluence has moved every descendant with it, and ids, versions, history,
//! comments, attachments and labels are what they were. What confed adds is
//! everything around that call:
//!
//! - a **plan** ([`plan`]), made with every check done before anything is
//!   written: the pages are in sync, no title collides in the destination,
//!   the user may do it, nothing special is in the way, and which links will
//!   need fixing;
//! - the **local half**: each page's file, sidecar and state rows go from the
//!   source workspace to the destination, re-keyed to their new paths, without
//!   downloading anything again;
//! - **links**: a link by title means "this space" when it names no space,
//!   so moving a page breaks every such link to it from the space it left,
//!   and every such link from it to a page left behind. [`rewrite_links`]
//!   fixes them in storage, touching nothing but `ri:space-key` and
//!   `ri:content-title`;
//! - a **journal** (`.confed-instance.db`), written before each step, so an
//!   interrupted move is finished by [`execute`] from where it stopped, every
//!   step asking the server or the disk what is already done rather than
//!   assuming.

use crate::error::{ConfedError, Result};
use crate::frontmatter;
use crate::instance::{Instance, Step};
use crate::paths;
use crate::state::{now, PageRecord, RemotePage};
use crate::sync::{pending_comment_work, prune_empty_dirs, PullOptions, PushOptions, SyncEngine};
use crate::workspace::Workspace;
use crate::worktree::{self, PageState};
use confed_api::{
    ApiError, BodyFormat, ConfluenceClient, MovePosition, PageId, PageUpdate, SpaceId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// The journal's name for this operation.
pub const OP: &str = "mv";

// ------------------------------------------------------------------ plan ---

/// A space taking part in a move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceRef {
    pub key: String,
    /// The member's directory, relative to the instance root.
    pub dir: String,
    pub numeric: Option<String>,
}

impl SpaceRef {
    fn id(&self) -> SpaceId {
        SpaceId { key: self.key.clone(), numeric: self.numeric.clone() }
    }
}

/// One page of the moved subtree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedPage {
    pub page_id: String,
    pub title: String,
    /// The title it gets in the destination, when it has to change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_title: Option<String>,
    pub parent_id: Option<String>,
    /// 0 for the subtree's root.
    pub depth: usize,
    /// Where it is in the source workspace.
    pub from_path: String,
    /// Where it will be in the destination workspace.
    pub to_path: String,
    pub comments: usize,
    pub attachments: usize,
    /// Operations restricted on the page itself; they move with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restricted: Vec<String>,
}

impl PlannedPage {
    pub fn final_title(&self) -> &str {
        self.new_title.as_deref().unwrap_or(&self.title)
    }
}

/// A page whose storage links to the moved pages in a way the move breaks,
/// or that is moved and links to a page left behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkFix {
    pub page_id: String,
    pub space: String,
    pub path: String,
    pub title: String,
    /// Links in it that need changing.
    pub count: usize,
    /// Whether the page is one of the moved ones.
    pub moved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub space: String,
    pub path: String,
}

/// A page as the move left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovedPage {
    pub page_id: String,
    pub title: String,
    pub from: Location,
    pub to: Location,
    pub version: u32,
}

/// A page whose links confed rewrote, as a new version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRewrite {
    pub page_id: String,
    pub space: String,
    pub count: usize,
    pub version: u32,
}

/// Everything a move needs, from the checks to the last step. It is what the
/// journal stores, so a move can be resumed by a later process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MovePlan {
    pub source: SpaceRef,
    pub dest: SpaceRef,
    pub root_id: String,
    /// How the subtree's root is placed relative to `target_id` on the server.
    pub position: MovePosition,
    pub target_id: String,
    /// The root's parent afterwards: `None` for a top-level page.
    pub new_parent_id: Option<String>,
    /// Parent first.
    pub pages: Vec<PlannedPage>,
    pub links: Vec<LinkFix>,
    pub rewrite_links: bool,
    pub relativize: bool,
    pub message: Option<String>,
    /// The source's rules page, when it is in the subtree.
    #[serde(default)]
    pub rules_page: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,

    // Filled in as the steps run.
    /// Pages whose title change has been made on the server.
    #[serde(default)]
    pub renamed: Vec<String>,
    #[serde(default)]
    pub moved: Vec<MovedPage>,
    #[serde(default)]
    pub links_rewritten: Vec<LinkRewrite>,
    /// Pages the plan expected to fix that the server had already fixed.
    #[serde(default)]
    pub links_by_server: Vec<String>,
}

impl MovePlan {
    pub fn page_ids(&self) -> Vec<String> {
        self.pages.iter().map(|p| p.page_id.clone()).collect()
    }

    /// Every space whose workspace the move writes to.
    pub fn spaces_written(&self) -> Vec<String> {
        let mut keys: Vec<String> = vec![self.source.key.clone(), self.dest.key.clone()];
        if self.rewrite_links {
            keys.extend(self.links.iter().map(|l| l.space.clone()));
        }
        keys.sort();
        keys.dedup();
        keys
    }

    /// Old `(space, title)` → new `(space, title)` of every moved page.
    pub fn moves(&self) -> HashMap<(String, String), (String, String)> {
        self.pages
            .iter()
            .map(|p| {
                (
                    (self.source.key.clone(), p.title.clone()),
                    (self.dest.key.clone(), p.final_title().to_string()),
                )
            })
            .collect()
    }
}

/// What to do about a title that is taken in the destination.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OnConflict {
    /// Refuse the move, naming every collision.
    #[default]
    Fail,
    /// Append ` (KEY)`, the source space's key.
    Suffix,
    /// New titles, by page id or by current title.
    Map(HashMap<String, String>),
}

/// What the user asked for, already resolved to ids.
#[derive(Clone, Debug)]
pub struct MoveRequest {
    pub root_id: String,
    pub source: SpaceRef,
    pub dest: SpaceRef,
    pub position: MovePosition,
    pub target_id: String,
    /// The root's parent afterwards, as the position implies it.
    pub new_parent_id: Option<String>,
    /// A new title for the root (a destination given as a new path).
    pub new_title: Option<String>,
    pub on_conflict: OnConflict,
    pub rewrite_links: bool,
    pub relativize: bool,
    pub message: Option<String>,
}

/// Everything wrong with a move, found before anything is written.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Refusal {
    pub problems: Vec<String>,
}

impl Refusal {
    fn into_error(self, summary: &str) -> ConfedError {
        let list = self.problems.iter().map(|p| format!("\n  - {p}")).collect::<String>();
        ConfedError::state_with_hint(
            format!("{summary}:{list}"),
            "nothing was changed; fix these and run the move again (`--dry-run` shows the plan)",
        )
    }
}

/// Check everything and work out the move, writing nothing.
///
/// `spaces` holds every registered workspace that is open, keyed by space
/// key; both sides of the move must be among them, freshly fetched. Every
/// problem is collected before the answer, so the error names all of them.
pub async fn plan(
    client: &Arc<dyn ConfluenceClient>,
    spaces: &BTreeMap<String, Workspace>,
    req: &MoveRequest,
) -> Result<MovePlan> {
    let src = spaces.get(&req.source.key).ok_or_else(|| missing_space(&req.source.key))?;
    let dst = spaces.get(&req.dest.key).ok_or_else(|| missing_space(&req.dest.key))?;
    if req.source.key == req.dest.key {
        return Err(ConfedError::usage_with_hint(
            format!("the page is already in space {}", req.source.key),
            "use `confed mv` to move a page within its space",
        ));
    }

    let mut refusal = Refusal::default();
    let mut warnings = Vec::new();

    let root = src.state().get_page(&req.root_id)?.ok_or_else(|| {
        ConfedError::NotFound(format!(
            "page {} is not tracked in {}; `confed -C {} pull` it first",
            req.root_id, req.source.dir, req.source.dir
        ))
    })?;

    // Home pages stay where they are: a space without one is broken.
    let source_space = client.get_space(&req.source.key).await?;
    if source_space.homepage_id.as_ref().is_some_and(|h| h.0 == req.root_id) {
        refusal
            .problems
            .push(format!("{} is the home page of space {}", root.local_path, req.source.key));
    }

    // The subtree, as the server last listed it.
    let remote = src.state().all_remote()?;
    let mut children: HashMap<&str, Vec<&RemotePage>> = HashMap::new();
    for page in remote.iter().filter(|r| !r.deleted) {
        if let Some(parent) = &page.parent_id {
            children.entry(parent.as_str()).or_default().push(page);
        }
    }
    let mut subtree: Vec<(String, usize)> = vec![(req.root_id.clone(), 0)];
    let mut index = 0;
    while index < subtree.len() {
        let (id, depth) = subtree[index].clone();
        let mut kids = children.get(id.as_str()).cloned().unwrap_or_default();
        kids.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));
        for kid in kids {
            subtree.push((kid.page_id.clone(), depth + 1));
        }
        index += 1;
    }
    let ids: HashSet<String> = subtree.iter().map(|(id, _)| id.clone()).collect();

    // In sync: nothing local to lose, nothing remote not yet pulled.
    let scan = worktree::scan(src)?;
    let status_of = |id: &str| scan.pages.iter().find(|s| s.page_id.as_deref() == Some(id));
    let mut pages = Vec::new();
    for (id, depth) in &subtree {
        let base = src.state().get_page(id)?;
        let remote_page = remote.iter().find(|r| &r.page_id == id);
        let title = base
            .as_ref()
            .map(|b| b.title.clone())
            .or_else(|| remote_page.map(|r| r.title.clone()))
            .unwrap_or_default();
        let Some(base) = base else {
            refusal.problems.push(format!(
                "\"{title}\" ({id}) has not been pulled into {}; run `confed -C {} pull`",
                req.source.dir, req.source.dir
            ));
            continue;
        };
        match status_of(id) {
            Some(status) if status.is_clean() => {}
            Some(status) => refusal.problems.push(format!(
                "{}: {}{}",
                base.local_path,
                describe_state(status.state),
                if status.tampering.is_empty() { "" } else { " (frontmatter edited)" }
            )),
            None => refusal.problems.push(format!("{}: not on disk", base.local_path)),
        }
        let drafts = pending_comment_work(src, &base.local_path);
        if drafts > 0 {
            refusal
                .problems
                .push(format!("{}: {drafts} comment draft(s) not pushed yet", base.local_path));
        }
        pages.push(PlannedPage {
            page_id: id.clone(),
            title: base.title.clone(),
            new_title: None,
            parent_id: base.parent_id.clone(),
            depth: *depth,
            from_path: base.local_path.clone(),
            to_path: String::new(),
            comments: src.state().page_comments(id)?.len(),
            attachments: src.state().page_attachments(id)?.len(),
            restricted: Vec::new(),
        });
    }

    // New pages that were never pushed, filed under a moved page.
    let dirs: Vec<String> = pages.iter().map(|p| paths::children_dir(&p.from_path)).collect();
    for status in &scan.pages {
        if status.state == PageState::LocalNew
            && dirs.iter().any(|d| status.path.starts_with(&format!("{d}/")))
        {
            refusal.problems.push(format!("{}: a new page, not pushed yet", status.path));
        }
    }

    // Attachments added, replaced or removed locally.
    let engine = SyncEngine::new(client.clone(), req.source.id(), 0);
    let attachment_work = engine.plan_push(
        src,
        &PushOptions {
            scope: ids.iter().cloned().collect(),
            with_attachments: true,
            with_comments: false,
            dry_run: true,
            ..Default::default()
        },
    )?;
    for op in &attachment_work.attachment_ops {
        refusal.problems.push(format!("{}: attachment change not pushed yet", op.file));
    }

    // The new parent is in sync too.
    if let Some(parent) = &req.new_parent_id {
        match dst.state().get_page(parent)? {
            None => refusal.problems.push(format!(
                "the destination parent ({parent}) is not tracked in {}; `confed -C {} pull` first",
                req.dest.dir, req.dest.dir
            )),
            Some(record) => {
                let dscan = worktree::scan(dst)?;
                if let Some(status) = dscan
                    .pages
                    .iter()
                    .find(|s| s.page_id.as_deref() == Some(parent.as_str()))
                    .filter(|s| !s.is_clean())
                {
                    refusal.problems.push(format!(
                        "{}:{}: {}",
                        req.dest.key,
                        record.local_path,
                        describe_state(status.state)
                    ));
                }
            }
        }
    }

    // Titles are unique per space: check against the server, not the mirror.
    let taken: HashMap<String, String> = client
        .list_pages(&req.dest.id())
        .await?
        .into_iter()
        .map(|p| (p.title.to_lowercase(), p.id.0))
        .collect();
    for page in &mut pages {
        if page.depth == 0 {
            page.new_title = req.new_title.clone().filter(|t| t != &page.title);
        }
        if let OnConflict::Map(map) = &req.on_conflict {
            if let Some(title) = map.get(&page.page_id).or_else(|| map.get(&page.title)) {
                page.new_title = Some(title.clone()).filter(|t| t != &page.title);
            }
        }
        let wanted = page.final_title().to_string();
        if taken.contains_key(&wanted.to_lowercase()) {
            match req.on_conflict {
                OnConflict::Suffix => {
                    let suffixed = format!("{wanted} ({})", req.source.key);
                    if taken.contains_key(&suffixed.to_lowercase()) {
                        refusal.problems.push(format!(
                            "\"{wanted}\" and \"{suffixed}\" both exist in space {}",
                            req.dest.key
                        ));
                    }
                    page.new_title = Some(suffixed);
                }
                _ => refusal.problems.push(format!(
                    "title \"{wanted}\" ({}) is already used in space {}",
                    page.from_path, req.dest.key
                )),
            }
        }
    }

    // Permissions, where the server says.
    let mut unchecked = false;
    match client.page_operations(&PageId::new(&req.root_id)).await {
        Ok(Some(ops)) => {
            for needed in ["update", "delete"] {
                if !ops.iter().any(|o| o == needed) {
                    refusal.problems.push(format!(
                        "you may not {needed} {} in space {}",
                        root.local_path, req.source.key
                    ));
                }
            }
        }
        Ok(None) | Err(_) => unchecked = true,
    }
    let landing = req.new_parent_id.clone().unwrap_or_else(|| req.target_id.clone());
    match client.page_operations(&PageId::new(&landing)).await {
        Ok(Some(ops)) if !ops.iter().any(|o| o == "create") => {
            refusal.problems.push(format!("you may not add pages in space {}", req.dest.key))
        }
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => unchecked = true,
    }
    if unchecked {
        warnings.push(
            "the server did not say what you may do, so permissions were not checked; \
             a refusal from the server stops the move before anything else changes"
                .to_string(),
        );
    }

    // Restrictions move with the pages; say so.
    for page in &mut pages {
        if let Ok(restricted) = client.page_restrictions(&PageId::new(&page.page_id)).await {
            if !restricted.is_empty() {
                warnings.push(format!(
                    "{} has {} restrictions, which move with it",
                    page.from_path,
                    restricted.join(" and ")
                ));
                page.restricted = restricted;
            }
        }
    }

    let rules_page = src.rules_page_id()?.filter(|id| ids.contains(id));
    if let Some(id) = &rules_page {
        warnings.push(format!(
            "page {id} is the rules page of {}; after the move it is in {}, and {} has none",
            req.source.key, req.dest.key, req.source.key
        ));
    }

    // Where each page will be in the destination, and that nothing is in the way.
    let placements = simulate_placements(src, dst, &pages, req)?;
    for page in &mut pages {
        let Some(placement) = placements.get(&page.page_id) else { continue };
        page.to_path = placement.path.clone();
        for path in [placement.path.clone(), placement.sidecar.clone()] {
            if dst.absolute(&path).exists() {
                refusal.problems.push(format!(
                    "{}:{path} already exists, and the move would put {} there",
                    req.dest.key, page.from_path
                ));
            }
        }
    }

    // Links the move breaks, in every registered workspace.
    let mut plan = MovePlan {
        source: req.source.clone(),
        dest: req.dest.clone(),
        root_id: req.root_id.clone(),
        position: req.position,
        target_id: req.target_id.clone(),
        new_parent_id: req.new_parent_id.clone(),
        pages,
        links: Vec::new(),
        rewrite_links: req.rewrite_links,
        relativize: req.relativize,
        message: req.message.clone(),
        rules_page,
        warnings,
        renamed: Vec::new(),
        moved: Vec::new(),
        links_rewritten: Vec::new(),
        links_by_server: Vec::new(),
    };
    let moves = plan.moves();
    for (key, ws) in spaces {
        // Read the working tree only for a space that has a page to check.
        let mut scan: Option<worktree::Scan> = None;
        for page in ws.state().all_pages()? {
            let moved = key == &req.source.key && ids.contains(&page.page_id);
            let ctx = LinkContext {
                space_before: key,
                space_after: if moved { &req.dest.key } else { key },
                moves: &moves,
                relativize: req.relativize,
            };
            let (_, count) = rewrite_links(&page.storage_body, &ctx);
            if count == 0 {
                continue;
            }
            if req.rewrite_links && !moved {
                if scan.is_none() {
                    scan = Some(worktree::scan(ws)?);
                }
                let dirty = scan
                    .as_ref()
                    .expect("read above")
                    .pages
                    .iter()
                    .find(|s| s.page_id.as_deref() == Some(page.page_id.as_str()))
                    .is_some_and(|s| !s.is_clean());
                if dirty {
                    refusal.problems.push(format!(
                        "{key}:{} links to a moved page and has local changes; push them \
                         first, or pass --no-rewrite-links",
                        page.local_path
                    ));
                }
            }
            plan.links.push(LinkFix {
                page_id: page.page_id.clone(),
                space: key.clone(),
                path: page.local_path.clone(),
                title: page.title.clone(),
                count,
                moved,
            });
        }
    }

    if !refusal.problems.is_empty() {
        return Err(refusal
            .into_error(&format!("cannot move {} to space {}", root.local_path, req.dest.key)));
    }
    Ok(plan)
}

/// What a page's status means for a move, in a few words.
fn describe_state(state: PageState) -> &'static str {
    match state {
        PageState::Unchanged => "frontmatter edited by hand",
        PageState::Modified => "has local changes; push them first",
        PageState::Behind => "changed on the server; pull first",
        PageState::Diverged => "changed locally and on the server; pull and push first",
        PageState::Conflicted => "has an unresolved conflict",
        PageState::LocalDeleted => "deleted locally",
        PageState::RemoteDeleted => "deleted on the server",
        PageState::RemoteNew => "not pulled yet",
        PageState::LocalNew => "not pushed yet",
        PageState::Untracked => "not tracked",
    }
}

fn missing_space(key: &str) -> ConfedError {
    ConfedError::usage_with_hint(
        format!("space {key} is not registered in this instance"),
        format!("`confed instance add {key}` or `confed instance clone {key}` registers it"),
    )
}

/// Where the destination's own placement rules put each moved page: the
/// same answer the transfer (and every later pull) will give.
fn simulate_placements(
    src: &Workspace,
    dst: &Workspace,
    pages: &[PlannedPage],
    req: &MoveRequest,
) -> Result<HashMap<String, paths::Placement>> {
    let mut remote = dst.state().all_remote()?;
    let mut base = dst.state().all_pages()?;
    for page in pages {
        let Some(record) = src.state().get_page(&page.page_id)? else { continue };
        let parent =
            if page.depth == 0 { req.new_parent_id.clone() } else { page.parent_id.clone() };
        remote.push(RemotePage {
            page_id: page.page_id.clone(),
            title: page.final_title().to_string(),
            parent_id: parent.clone(),
            position: record.position,
            version: record.version,
            status: record.status.clone(),
            labels: record.labels.clone(),
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: None,
            storage_hash: None,
            fetched_at: now(),
            deleted: false,
        });
        base.push(PageRecord { parent_id: parent, ..record });
    }
    let placements = paths::plan_from_records(&remote, &base);
    Ok(pages
        .iter()
        .filter_map(|p| placements.get(&p.page_id).map(|pl| (p.page_id.clone(), pl.clone())))
        .collect())
}

// -------------------------------------------------------------- execute ---

/// Run the move's remaining steps, from the one after `done` to the end,
/// recording each in the journal as it finishes.
///
/// `spaces` must hold every workspace in [`MovePlan::spaces_written`], each
/// locked by the caller. Every step checks what is already true before it
/// acts, so running this again after an interruption — `confed instance
/// resume` — reaches the same end as an uninterrupted run.
pub async fn execute(
    instance: &Instance,
    journal_id: &str,
    client: &Arc<dyn ConfluenceClient>,
    spaces: &mut BTreeMap<String, Workspace>,
    plan: &mut MovePlan,
    done: Step,
    concurrency: usize,
) -> Result<()> {
    let steps = [
        Step::ServerMoved,
        Step::LocalDstWritten,
        Step::LocalSrcRemoved,
        Step::LinksRewritten,
        Step::Done,
    ];
    for step in steps.into_iter().filter(|s| *s > done) {
        let result = match step {
            Step::ServerMoved => server_move(client, plan).await,
            Step::LocalDstWritten => write_destination(client, spaces, plan).await,
            Step::LocalSrcRemoved => remove_from_source(spaces, plan),
            Step::LinksRewritten => fix_links(client, plan).await,
            Step::Done => settle(client, spaces, plan, concurrency).await,
            _ => Ok(()),
        };
        if let Err(e) = result {
            let _ = instance.journal_fail(journal_id, &e.to_string());
            return Err(e);
        }
        instance.journal_advance(journal_id, step, &serde_json::to_value(&*plan)?)?;
    }
    Ok(())
}

/// Undo what can be undone before the server moved anything: the title
/// changes made for the move. Allowed only before [`Step::ServerMoved`].
pub async fn abort(
    instance: &Instance,
    journal_id: &str,
    client: &Arc<dyn ConfluenceClient>,
    plan: &mut MovePlan,
    done: Step,
) -> Result<()> {
    if done != Step::Planned {
        return Err(ConfedError::state_with_hint(
            format!(
                "move {journal_id} is past `{}`: the server has moved the pages",
                done.as_str()
            ),
            "finish it with `confed instance resume`; to undo it, move the pages back with \
             `confed instance mv`",
        ));
    }
    let renamed = std::mem::take(&mut plan.renamed);
    for id in &renamed {
        let Some(page) = plan.pages.iter().find(|p| &p.page_id == id) else { continue };
        let current = client.get_page(&PageId::new(id), BodyFormat::Storage).await?;
        if current.summary.title == page.title {
            continue;
        }
        client
            .update_page(
                &PageId::new(id),
                &PageUpdate {
                    title: page.title.clone(),
                    body_storage: None,
                    version: current.summary.version + 1,
                    parent_id: None,
                    status: None,
                    message: Some("confed: cross-space move aborted".into()),
                },
            )
            .await?;
    }
    instance.journal_advance(journal_id, Step::Aborted, &serde_json::to_value(&*plan)?)?;
    Ok(())
}

/// Rename what has to be renamed, then move the subtree's root.
async fn server_move(client: &Arc<dyn ConfluenceClient>, plan: &mut MovePlan) -> Result<()> {
    let root = PageId::new(&plan.root_id);
    let current = client.get_page(&root, BodyFormat::Storage).await?;
    let already = current.summary.space_key == plan.dest.key
        && current.summary.parent_id.as_ref().map(|p| p.0.clone()) == plan.new_parent_id;

    // Titles first: the destination would refuse a duplicate.
    if !already {
        let renames: Vec<(String, String)> = plan
            .pages
            .iter()
            .filter(|p| !plan.renamed.contains(&p.page_id))
            .filter_map(|p| p.new_title.clone().map(|t| (p.page_id.clone(), t)))
            .collect();
        for (id, title) in renames {
            let page = client.get_page(&PageId::new(&id), BodyFormat::Storage).await?;
            if page.summary.title != title {
                client
                    .update_page(
                        &PageId::new(&id),
                        &PageUpdate {
                            title,
                            body_storage: None,
                            version: page.summary.version + 1,
                            parent_id: None,
                            status: None,
                            message: plan.message.clone(),
                        },
                    )
                    .await?;
            }
            plan.renamed.push(id);
        }
    }

    if !already {
        let target = PageId::new(&plan.target_id);
        if let Err(e) = client.move_page_to(&root, plan.position, &target).await {
            return Err(move_error(client, plan, e).await);
        }
    }

    let after = client.get_page(&root, BodyFormat::Storage).await?;
    if after.summary.space_key != plan.dest.key {
        return Err(ConfedError::Unsupported(format!(
            "the server accepted the move but page {} is still in space {}; this server \
             does not move pages between spaces through its API, and confed will not copy \
             and delete instead (that would lose ids and history)",
            plan.root_id, after.summary.space_key
        )));
    }
    Ok(())
}

/// A failed move call, said plainly.
async fn move_error(
    client: &Arc<dyn ConfluenceClient>,
    plan: &MovePlan,
    e: ApiError,
) -> ConfedError {
    match e {
        ApiError::Auth(message) if message.contains("403") => ConfedError::Auth(format!(
            "the server refused to move page {} to space {} (HTTP 403): you need permission \
             to remove the pages from {} and to add pages in {}",
            plan.root_id, plan.dest.key, plan.source.key, plan.dest.key
        )),
        ApiError::NotFound(message) => {
            // Gone pages, or a server without the endpoint?
            let root = client.get_page(&PageId::new(&plan.root_id), BodyFormat::Storage).await;
            let target = client.get_page(&PageId::new(&plan.target_id), BodyFormat::Storage).await;
            if root.is_ok() && target.is_ok() {
                ConfedError::Unsupported(
                    "this server has no page move endpoint (rest/api/content/{id}/move answered \
                     404 for pages that exist); move the pages in the browser, then run \
                     `confed instance doctor`"
                        .into(),
                )
            } else {
                ConfedError::NotFound(format!("the page or its destination is gone: {message}"))
            }
        }
        ApiError::Conflict(message) => ConfedError::Conflict(format!(
            "the server refused the move as a conflict (a title already in use in {}?): {message}",
            plan.dest.key
        )),
        other => other.into(),
    }
}

/// Bring the moved pages' state, files and cache into the destination.
async fn write_destination(
    client: &Arc<dyn ConfluenceClient>,
    spaces: &mut BTreeMap<String, Workspace>,
    plan: &mut MovePlan,
) -> Result<()> {
    let mut src = spaces.remove(&plan.source.key).ok_or_else(|| missing_space(&plan.source.key))?;
    let mut dst = match spaces.remove(&plan.dest.key) {
        Some(dst) => dst,
        None => {
            spaces.insert(plan.source.key.clone(), src);
            return Err(missing_space(&plan.dest.key));
        }
    };
    let result = transfer(client, &mut src, &mut dst, plan).await;
    spaces.insert(plan.source.key.clone(), src);
    spaces.insert(plan.dest.key.clone(), dst);
    result
}

/// Copy rows and move files from `src` to `dst`, in the order a resume can
/// pick up from: whatever is already in the destination is left as it is.
pub async fn transfer(
    client: &Arc<dyn ConfluenceClient>,
    src: &mut Workspace,
    dst: &mut Workspace,
    plan: &mut MovePlan,
) -> Result<()> {
    let ids = plan.page_ids();
    // Where the server has each page now.
    let listed: HashMap<String, confed_api::PageSummary> = client
        .list_pages(&plan.dest.id())
        .await?
        .into_iter()
        .filter(|p| ids.contains(&p.id.0))
        .map(|p| (p.id.0.clone(), p))
        .collect();

    // The cache first: it is never wrong to have more of it.
    if let (Ok(from), Ok(to)) = (src.page_store(), dst.page_store()) {
        from.copy_pages_to(&to, &ids)?;
    }

    // Rows, in one transaction.
    let dst_state = dst.state();
    dst_state.conn().execute_batch("BEGIN IMMEDIATE")?;
    let copied: Result<()> = (|| {
        for user in src.state().all_users()? {
            if dst_state.get_user(&user.id)?.is_none() {
                dst_state.upsert_user(&user)?;
            }
        }
        for page in &plan.pages {
            if dst_state.get_page(&page.page_id)?.is_some() {
                continue;
            }
            let Some(mut record) = src.state().get_page(&page.page_id)? else {
                return Err(ConfedError::state(format!(
                    "page {} is in neither workspace; run `confed instance doctor`",
                    page.page_id
                )));
            };
            if page.depth == 0 {
                record.parent_id = plan.new_parent_id.clone();
            }
            // Rendered again by the closing pull, in the destination's context.
            record.render_key = String::new();
            dst_state.upsert_page(&record)?;

            let mut remote = src.state().get_remote(&page.page_id)?.unwrap_or(RemotePage {
                page_id: record.page_id.clone(),
                title: record.title.clone(),
                parent_id: record.parent_id.clone(),
                position: record.position,
                version: record.version,
                status: record.status.clone(),
                labels: record.labels.clone(),
                author: record.author.clone(),
                created_at: record.created_at.clone(),
                updated_at: record.updated_at.clone(),
                storage_body: None,
                storage_hash: None,
                fetched_at: now(),
                deleted: false,
            });
            if let Some(summary) = listed.get(&page.page_id) {
                remote.title = summary.title.clone();
                remote.parent_id = summary.parent_id.as_ref().map(|p| p.0.clone());
                remote.position = summary.position;
                remote.labels = summary.labels.clone();
                if summary.version != remote.version {
                    // Changed beyond the move: the closing pull fetches it.
                    remote.storage_body = None;
                    remote.storage_hash = None;
                }
                remote.version = summary.version;
            }
            if remote.storage_body.is_none() && remote.version == record.version {
                remote.storage_body = Some(record.storage_body.clone());
                remote.storage_hash = Some(record.storage_hash.clone());
            }
            dst_state.upsert_remote(&remote)?;
            for attachment in src.state().page_attachments(&page.page_id)? {
                dst_state.upsert_attachment(&attachment)?;
            }
            for comment in src.state().page_comments(&page.page_id)? {
                dst_state.upsert_comment(&comment)?;
            }
            for removed in src.state().removed_attachments(&page.page_id)? {
                dst_state.note_removed_attachment(
                    &removed.page_id,
                    &removed.filename,
                    removed.sha256.as_deref(),
                )?;
            }
        }
        Ok(())
    })();
    match copied {
        Ok(()) => dst_state.conn().execute_batch("COMMIT")?,
        Err(e) => {
            let _ = dst_state.conn().execute_batch("ROLLBACK");
            return Err(e);
        }
    }

    // Files, where the destination's rules put them.
    let placements =
        paths::plan_from_records(&dst.state().all_remote()?, &dst.state().all_pages()?);
    let mut moved = Vec::new();
    for page in &mut plan.pages {
        let Some(placement) = placements.get(&page.page_id) else {
            return Err(ConfedError::state(format!(
                "no place for page {} in {}",
                page.page_id, plan.dest.dir
            )));
        };
        move_page_files(src, dst, &page.from_path, &placement.path)?;
        page.to_path = placement.path.clone();

        // The tool-managed fields the move changed; nothing else.
        let path = dst.absolute(&placement.path);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ConfedError::io(format!("reading {}", path.display()), e))?;
        let mut file = frontmatter::parse(&text, &placement.path)?;
        if let Some(managed) = file.frontmatter.managed.as_mut() {
            managed.space_key = plan.dest.key.clone();
        }
        if page.depth == 0 {
            file.frontmatter.parent_id = plan.new_parent_id.clone();
        }
        let rendered = file.render()?;
        if rendered != text {
            crate::sync::write_atomic(&path, &rendered)?;
        }
        if let Some(mut record) = dst.state().get_page(&page.page_id)? {
            record.local_path = placement.path.clone();
            record.slug = placement.slug.clone();
            record.markdown_hash = file.content_hash();
            dst.state().upsert_page(&record)?;
            moved.push(MovedPage {
                page_id: page.page_id.clone(),
                title: page.final_title().to_string(),
                from: Location { space: plan.source.key.clone(), path: page.from_path.clone() },
                to: Location { space: plan.dest.key.clone(), path: placement.path.clone() },
                version: listed.get(&page.page_id).map(|s| s.version).unwrap_or(record.version),
            });
        }
    }
    plan.moved = moved;
    Ok(())
}

/// Move a page's file and sidecar between workspaces. Already moved is fine;
/// missing from both is not.
fn move_page_files(src: &Workspace, dst: &Workspace, from: &str, to: &str) -> Result<()> {
    let pairs = [
        (from.to_string(), to.to_string(), true),
        (paths::sidecar_for(from), paths::sidecar_for(to), false),
    ];
    for (from, to, required) in pairs {
        let source = src.absolute(&from);
        let target = dst.absolute(&to);
        if !source.exists() {
            // Moved already, by a run that was interrupted — or, for a
            // sidecar, never there.
            if target.exists() || !required {
                continue;
            }
            return Err(ConfedError::state(format!(
                "{from} is missing from {} and {to} from {}",
                src.root().display(),
                dst.root().display()
            )));
        }
        if target.exists() && source.is_dir() {
            merge_dir(&source, &target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
        }
        rename_or_copy(&source, &target)?;
    }
    Ok(())
}

/// `rename`, or a copy and delete where the two are on different filesystems.
fn rename_or_copy(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    if from.is_dir() {
        merge_dir(from, to)
    } else {
        std::fs::copy(from, to)
            .map_err(|e| ConfedError::io(format!("copying {}", from.display()), e))?;
        std::fs::remove_file(from)
            .map_err(|e| ConfedError::io(format!("removing {}", from.display()), e))
    }
}

/// Move everything in `from` into `to`, keeping what `to` already has.
fn merge_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)
        .map_err(|e| ConfedError::io(format!("creating {}", to.display()), e))?;
    let entries = std::fs::read_dir(from)
        .map_err(|e| ConfedError::io(format!("reading {}", from.display()), e))?;
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        if target.exists() && !entry.path().is_dir() {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        if target.exists() {
            merge_dir(&entry.path(), &target)?;
        } else {
            rename_or_copy(&entry.path(), &target)?;
        }
    }
    let _ = std::fs::remove_dir(from);
    Ok(())
}

/// Forget the moved pages in the source, and tidy the directories they left.
fn remove_from_source(spaces: &mut BTreeMap<String, Workspace>, plan: &MovePlan) -> Result<()> {
    let src = spaces.get_mut(&plan.source.key).ok_or_else(|| missing_space(&plan.source.key))?;
    let ids = plan.page_ids();
    let state = src.state();
    state.conn().execute_batch("BEGIN IMMEDIATE")?;
    let removed: Result<()> = (|| {
        for id in &ids {
            state.delete_page(id)?;
            state.delete_remote(id)?;
            state.clear_page_attachments(id)?;
            state.clear_page_comments(id)?;
            for removed in state.removed_attachments(id)? {
                state.forget_removed_attachment(id, &removed.filename)?;
            }
            state.conn().execute("DELETE FROM fetch_queue WHERE page_id = ?1", [id])?;
            state.log("instance-mv", Some(id), None, None, "ok", Some(&plan.dest.key))?;
        }
        Ok(())
    })();
    match removed {
        Ok(()) => state.conn().execute_batch("COMMIT")?,
        Err(e) => {
            let _ = state.conn().execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    if let Ok(store) = src.page_store() {
        store.forget_pages(&ids)?;
    }
    let root = src.root().to_path_buf();
    // Deepest first, so a parent directory is empty by the time it is tried.
    let mut dirs: Vec<String> = plan
        .pages
        .iter()
        .flat_map(|p| {
            [paths::children_dir(&p.from_path), parent_dir(&p.from_path).unwrap_or_default()]
        })
        .filter(|d| !d.is_empty())
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for dir in dirs {
        prune_empty_dirs(&root, &root.join(dir));
    }
    Ok(())
}

fn parent_dir(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(d, _)| d.to_string())
}

/// Fix, on the server, the links the move broke that Confluence left broken.
async fn fix_links(client: &Arc<dyn ConfluenceClient>, plan: &mut MovePlan) -> Result<()> {
    if !plan.rewrite_links {
        return Ok(());
    }
    let moves = plan.moves();
    let fixes = plan.links.clone();
    for fix in fixes {
        if plan.links_rewritten.iter().any(|r| r.page_id == fix.page_id)
            || plan.links_by_server.contains(&fix.page_id)
        {
            continue;
        }
        let id = PageId::new(&fix.page_id);
        let page = client.get_page(&id, BodyFormat::Storage).await?;
        let own = page.summary.space_key.clone();
        let before = if fix.moved { plan.source.key.clone() } else { own.clone() };
        let ctx = LinkContext {
            space_before: &before,
            space_after: &own,
            moves: &moves,
            relativize: plan.relativize,
        };
        let (rewritten, count) = rewrite_links(&page.body_storage, &ctx);
        if count == 0 {
            plan.links_by_server.push(fix.page_id.clone());
            continue;
        }
        let updated = client
            .update_page(
                &id,
                &PageUpdate {
                    title: page.summary.title.clone(),
                    body_storage: Some(rewritten),
                    version: page.summary.version + 1,
                    parent_id: None,
                    status: None,
                    message: plan.message.clone(),
                },
            )
            .await?;
        plan.links_rewritten.push(LinkRewrite {
            page_id: fix.page_id.clone(),
            space: own,
            count,
            version: updated.summary.version,
        });
    }
    if !plan.links_by_server.is_empty() {
        plan.warnings.push(format!(
            "the server had already updated the links in {} page(s) itself",
            plan.links_by_server.len()
        ));
    }
    Ok(())
}

/// Pull what the move touched in every workspace it wrote to, so each file
/// is rendered in its own space's context and matches the server.
async fn settle(
    client: &Arc<dyn ConfluenceClient>,
    spaces: &mut BTreeMap<String, Workspace>,
    plan: &mut MovePlan,
    concurrency: usize,
) -> Result<()> {
    let mut scopes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    scopes.entry(plan.dest.key.clone()).or_default().extend(plan.page_ids());
    scopes.entry(plan.source.key.clone()).or_default();
    for fix in &plan.links {
        let space = if fix.moved { plan.dest.key.clone() } else { fix.space.clone() };
        scopes.entry(space).or_default().push(fix.page_id.clone());
    }
    for (key, scope) in scopes {
        let Some(ws) = spaces.get_mut(&key) else { continue };
        let space = SpaceId { key: key.clone(), numeric: ws.space_numeric_id()? };
        let engine = SyncEngine::new(client.clone(), space, concurrency);
        let outcome = engine
            .pull(
                ws,
                &PullOptions {
                    scope: if scope.is_empty() { vec![plan.root_id.clone()] } else { scope },
                    with_attachments: true,
                    with_comments: true,
                    ..Default::default()
                },
            )
            .await?;
        plan.warnings.extend(outcome.warnings.into_iter().map(|w| format!("{key}: {w}")));
        if !outcome.conflicted.is_empty() {
            return Err(ConfedError::Conflict(format!(
                "{key}: {} page(s) were left with conflict markers by the closing pull",
                outcome.conflicted.len()
            )));
        }
    }

    // Afterwards: each moved page is tracked once, in the destination, clean.
    let src = spaces.get(&plan.source.key).ok_or_else(|| missing_space(&plan.source.key))?;
    let dst = spaces.get(&plan.dest.key).ok_or_else(|| missing_space(&plan.dest.key))?;
    let dscan = worktree::scan(dst)?;
    for moved in &mut plan.moved {
        if src.state().get_page(&moved.page_id)?.is_some() {
            plan.warnings
                .push(format!("{} is still tracked in {}", moved.page_id, plan.source.key));
        }
        match dst.state().get_page(&moved.page_id)? {
            Some(record) => {
                moved.to.path = record.local_path.clone();
                moved.version = record.version;
                moved.title = record.title.clone();
                let clean = dscan
                    .pages
                    .iter()
                    .find(|s| s.page_id.as_deref() == Some(moved.page_id.as_str()))
                    .is_some_and(|s| s.is_clean());
                if !clean {
                    plan.warnings.push(format!(
                        "{}:{} is not clean after the move",
                        plan.dest.key, record.local_path
                    ));
                }
            }
            None => plan.warnings.push(format!(
                "{} is not tracked in {} after the move",
                moved.page_id, plan.dest.key
            )),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- links ---

/// How the links in one page are read and what they should become.
pub struct LinkContext<'a> {
    /// The space a link that names none meant before the move: the page's
    /// space then.
    pub space_before: &'a str,
    /// The page's space after the move.
    pub space_after: &'a str,
    /// Old `(space, title)` → new `(space, title)`.
    pub moves: &'a HashMap<(String, String), (String, String)>,
    /// Drop `ri:space-key` from a link that ends up naming the page's own
    /// space, rather than writing that space's key.
    pub relativize: bool,
}

/// Rewrite the `ri:page` links in `storage` so each one still names the page
/// it named before the move. Returns the new storage and how many links
/// changed.
///
/// Only `ri:space-key` and `ri:content-title` are ever touched, and only on
/// links that would otherwise resolve to the wrong page or none. A link by
/// id (`ri:content-id`, as Cloud writes them) survives a move as it is. The
/// rest of the storage — macros, their `ac:macro-id`, and every byte between
/// the links — comes out exactly as it went in.
pub fn rewrite_links(storage: &str, ctx: &LinkContext<'_>) -> (String, usize) {
    let mut out = String::with_capacity(storage.len());
    let mut changed = 0;
    let mut rest = storage;
    while let Some(start) = find_page_tag(rest) {
        out.push_str(&rest[..start]);
        let tag_and_after = &rest[start..];
        let Some(end) = tag_end(tag_and_after) else {
            out.push_str(tag_and_after);
            return (out, changed);
        };
        let tag = &tag_and_after[..end];
        match rewrite_tag(tag, ctx) {
            Some(new) => {
                changed += 1;
                out.push_str(&new);
            }
            None => out.push_str(tag),
        }
        rest = &tag_and_after[end..];
    }
    out.push_str(rest);
    (out, changed)
}

const PAGE_TAG: &str = "<ri:page";

fn find_page_tag(text: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(at) = text[offset..].find(PAGE_TAG) {
        let start = offset + at;
        let next = text[start + PAGE_TAG.len()..].chars().next();
        if matches!(next, Some(c) if c.is_whitespace() || c == '/' || c == '>') {
            return Some(start);
        }
        offset = start + PAGE_TAG.len();
    }
    None
}

/// The length of the tag at the start of `text`, through its `>`.
fn tag_end(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in text.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i + 1),
            None => {}
        }
    }
    None
}

/// One attribute of a tag: where it is (with its leading whitespace) and its
/// raw value.
struct Attr {
    name: String,
    value: String,
    quote: char,
    span: std::ops::Range<usize>,
    value_span: std::ops::Range<usize>,
}

fn attributes(tag: &str) -> Vec<Attr> {
    let bytes: Vec<(usize, char)> = tag.char_indices().collect();
    let mut attrs = Vec::new();
    let mut i = PAGE_TAG.chars().count();
    while i < bytes.len() {
        let ws_start = bytes[i].0;
        while i < bytes.len() && bytes[i].1.is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || matches!(bytes[i].1, '/' | '>') {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].1.is_whitespace()
            && !matches!(bytes[i].1, '=' | '/' | '>')
        {
            i += 1;
        }
        let name: String = bytes[name_start..i].iter().map(|(_, c)| *c).collect();
        while i < bytes.len() && bytes[i].1.is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i].1 != '=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].1.is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || !matches!(bytes[i].1, '"' | '\'') {
            break;
        }
        let quote = bytes[i].1;
        i += 1;
        let value_start = i;
        while i < bytes.len() && bytes[i].1 != quote {
            i += 1;
        }
        let value_end = i;
        i += 1;
        let byte = |k: usize| bytes.get(k).map_or(tag.len(), |(b, _)| *b);
        attrs.push(Attr {
            name,
            value: tag[byte(value_start)..byte(value_end)].to_string(),
            quote,
            span: ws_start..byte(i),
            value_span: byte(value_start)..byte(value_end),
        });
    }
    attrs
}

fn rewrite_tag(tag: &str, ctx: &LinkContext<'_>) -> Option<String> {
    let attrs = attributes(tag);
    let title_attr = attrs.iter().find(|a| a.name == "ri:content-title")?;
    let space_attr = attrs.iter().find(|a| a.name == "ri:space-key");
    let title = unescape(&title_attr.value);
    let named_space = space_attr.map(|a| unescape(&a.value));
    let meant = named_space.clone().unwrap_or_else(|| ctx.space_before.to_string());

    let (target_space, target_title) =
        ctx.moves.get(&(meant.clone(), title.clone())).cloned().unwrap_or((meant, title.clone()));

    // What the tag must say for the link to resolve to the target from here.
    let wanted_space: Option<String> = if target_space == ctx.space_after {
        match &named_space {
            Some(_) if ctx.relativize => None,
            Some(_) => Some(target_space.clone()),
            None => None,
        }
    } else {
        Some(target_space.clone())
    };
    if wanted_space == named_space && target_title == title {
        return None;
    }

    // Edit from the end, so earlier spans stay valid.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    if target_title != title {
        edits.push((title_attr.value_span.clone(), escape(&target_title, title_attr.quote)));
    }
    match (space_attr, &wanted_space) {
        (Some(attr), Some(space)) if Some(space) != named_space.as_ref() => {
            edits.push((attr.value_span.clone(), escape(space, attr.quote)));
        }
        (Some(attr), None) => edits.push((attr.span.clone(), String::new())),
        (None, Some(space)) => {
            let at = PAGE_TAG.len();
            edits.push((at..at, format!(r#" ri:space-key="{}""#, escape(space, '"'))));
        }
        _ => {}
    }
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut out = tag.to_string();
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    Some(out)
}

fn escape(value: &str, quote: char) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote == '"' => out.push_str("&quot;"),
            '\'' if quote == '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(end) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

// ----------------------------------------------------------------- doctor ---

/// A page tracked by more than one registered workspace.
#[derive(Clone, Debug, Serialize)]
pub struct DuplicatePage {
    pub page_id: String,
    pub spaces: Vec<String>,
}

/// Pages tracked in two workspaces at once — what an interrupted move, or a
/// workspace copied by hand, leaves behind.
pub fn duplicates(spaces: &BTreeMap<String, Workspace>) -> Result<Vec<DuplicatePage>> {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, ws) in spaces {
        for page in ws.state().all_pages()? {
            seen.entry(page.page_id).or_default().push(key.clone());
        }
    }
    Ok(seen
        .into_iter()
        .filter(|(_, spaces)| spaces.len() > 1)
        .map(|(page_id, spaces)| DuplicatePage { page_id, spaces })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moves(entries: &[(&str, &str, &str, &str)]) -> HashMap<(String, String), (String, String)> {
        entries
            .iter()
            .map(|(s, t, ns, nt)| {
                ((s.to_string(), t.to_string()), (ns.to_string(), nt.to_string()))
            })
            .collect()
    }

    fn rewrite(
        storage: &str,
        before: &str,
        after: &str,
        moved: &[(&str, &str, &str, &str)],
    ) -> (String, usize) {
        let map = moves(moved);
        rewrite_links(
            storage,
            &LinkContext {
                space_before: before,
                space_after: after,
                moves: &map,
                relativize: false,
            },
        )
    }

    #[test]
    fn a_link_left_behind_gets_the_new_space() {
        let src = r#"<p>See <ac:link><ri:page ri:content-title="Onboarding" /></ac:link>.</p>"#;
        let (out, n) = rewrite(src, "DOCS", "DOCS", &[("DOCS", "Onboarding", "ENG", "Onboarding")]);
        assert_eq!(n, 1);
        assert_eq!(
            out,
            r#"<p>See <ac:link><ri:page ri:space-key="ENG" ri:content-title="Onboarding" /></ac:link>.</p>"#
        );
    }

    #[test]
    fn a_moved_page_keeps_its_links_to_pages_left_behind() {
        let src = r#"<ac:link><ri:page ri:content-title="Team Handbook" /></ac:link><ac:link><ri:page ri:content-title="Checklist" /></ac:link>"#;
        let (out, n) = rewrite(
            src,
            "DOCS",
            "ENG",
            &[
                ("DOCS", "Onboarding", "ENG", "Onboarding"),
                ("DOCS", "Checklist", "ENG", "Checklist"),
            ],
        );
        assert_eq!(n, 1, "the link inside the moved tree needs nothing");
        assert!(out.contains(r#"<ri:page ri:space-key="DOCS" ri:content-title="Team Handbook" />"#));
        assert!(out.contains(r#"<ri:page ri:content-title="Checklist" />"#));
    }

    #[test]
    fn explicit_keys_follow_the_page_and_can_be_dropped() {
        let src = r#"<ri:page ri:space-key="DOCS" ri:content-title="Onboarding"/>"#;
        let map = moves(&[("DOCS", "Onboarding", "ENG", "Onboarding")]);
        let (kept, _) = rewrite_links(
            src,
            &LinkContext {
                space_before: "ENG",
                space_after: "ENG",
                moves: &map,
                relativize: false,
            },
        );
        assert_eq!(kept, r#"<ri:page ri:space-key="ENG" ri:content-title="Onboarding"/>"#);
        let (dropped, n) = rewrite_links(
            src,
            &LinkContext { space_before: "ENG", space_after: "ENG", moves: &map, relativize: true },
        );
        assert_eq!(n, 1);
        assert_eq!(dropped, r#"<ri:page ri:content-title="Onboarding"/>"#);

        // From a third space, the key is all that changes.
        let (out, _) = rewrite(src, "OPS", "OPS", &[("DOCS", "Onboarding", "ENG", "Onboarding")]);
        assert_eq!(out, r#"<ri:page ri:space-key="ENG" ri:content-title="Onboarding"/>"#);
    }

    #[test]
    fn a_renamed_page_is_linked_by_its_new_title() {
        let src = r#"<ri:page ri:content-title="Onboarding" />"#;
        let (out, _) =
            rewrite(src, "DOCS", "DOCS", &[("DOCS", "Onboarding", "ENG", "Onboarding (DOCS)")]);
        assert_eq!(out, r#"<ri:page ri:space-key="ENG" ri:content-title="Onboarding (DOCS)" />"#);
    }

    #[test]
    fn unrelated_links_and_everything_else_are_left_byte_for_byte() {
        let src = concat!(
            r#"<ac:structured-macro ac:name="info" ac:schema-version="1" ac:macro-id="0f4a-77"><ac:rich-text-body>"#,
            r#"<p><ac:link><ri:page ri:content-title="Other" /></ac:link></p>"#,
            r#"<p><ac:link><ri:page ri:content-id="900000098561" /></ac:link></p>"#,
            r#"<p><ri:pageish ri:content-title="Onboarding"/></p>"#,
            r#"</ac:rich-text-body></ac:structured-macro>"#
        );
        let (out, n) = rewrite(src, "DOCS", "DOCS", &[("DOCS", "Onboarding", "ENG", "Onboarding")]);
        assert_eq!(n, 0);
        assert_eq!(out, src);

        // A link inside a macro changes; the macro and its id do not.
        let inside = src.replace("Other", "Onboarding");
        let (out, n) =
            rewrite(&inside, "DOCS", "DOCS", &[("DOCS", "Onboarding", "ENG", "Onboarding")]);
        assert_eq!(n, 1);
        assert!(out.contains(r#"ac:macro-id="0f4a-77""#));
        assert_eq!(out.len(), inside.len() + r#" ri:space-key="ENG""#.len());
    }

    #[test]
    fn cyrillic_titles_and_escaped_characters_are_matched_as_text() {
        let src = r#"<ri:page ri:content-title="План работ &amp; схема" /><ri:page ri:content-title='Фраза &apos;1&apos;'/>"#;
        let (out, n) = rewrite(
            src,
            "DOCS",
            "DOCS",
            &[
                ("DOCS", "План работ & схема", "ENG", "План работ & схема"),
                ("DOCS", "Фраза '1'", "ENG", "Фраза '1'"),
            ],
        );
        assert_eq!(n, 2);
        assert_eq!(
            out,
            r#"<ri:page ri:space-key="ENG" ri:content-title="План работ &amp; схема" /><ri:page ri:space-key="ENG" ri:content-title='Фраза &apos;1&apos;'/>"#
        );
    }

    #[test]
    fn rewriting_twice_changes_nothing_the_second_time() {
        let src = r#"<ri:page ri:content-title="Onboarding" /><ri:page ri:space-key="DOCS" ri:content-title="Onboarding" />"#;
        let map = moves(&[("DOCS", "Onboarding", "ENG", "Onboarding (DOCS)")]);
        let ctx = LinkContext {
            space_before: "DOCS",
            space_after: "DOCS",
            moves: &map,
            relativize: false,
        };
        let (once, n) = rewrite_links(src, &ctx);
        assert_eq!(n, 2);
        let (twice, n) = rewrite_links(&once, &ctx);
        assert_eq!(n, 0);
        assert_eq!(once, twice);
    }

    #[test]
    fn a_truncated_tag_is_left_alone() {
        let src = r#"<p><ri:page ri:content-title="Onboarding""#;
        let (out, n) = rewrite(src, "DOCS", "DOCS", &[("DOCS", "Onboarding", "ENG", "Onboarding")]);
        assert_eq!((out.as_str(), n), (src, 0));
    }
}
