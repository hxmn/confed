//! Confluence storage format (XHTML + macros) ⇄ Markdown.
//!
//! Two guarantees shape everything here:
//!
//! 1. **Nothing is lost.** Elements the converter does not model are preserved
//!    inside ```` ```confluence ```` fences and re-emitted exactly as the fence
//!    holds them. The fence body is the source subtree laid out across lines by
//!    [`pretty`], which only moves whitespace no renderer can see.
//! 2. **Only edited blocks are regenerated.** [`markdown_to_storage_patched`] copies
//!    the original storage bytes for every top-level block the user did not touch,
//!    so lossiness is confined to blocks that actually changed.

/// Bumped whenever the rendering rules change in a way that makes previously
/// converted Markdown out of date. It feeds the render fingerprint confed keeps
/// per page, so an improvement reaches a workspace on the next pull without
/// waiting for each page to change on the server.
pub const CONVERTER_VERSION: u32 = 9;

// What confed-core needs and nothing else is public: each public item is a
// semver commitment once the crate is on crates.io. The modules below the
// three public ones are how conversion works, not what it offers.
mod blockmap;
mod dom;
mod error;
mod macros;
pub mod marks;
mod mdblock;
pub mod pretty;
pub mod selection;
mod storage_parse;
mod to_markdown;
mod to_storage;

#[cfg(test)]
extern crate self as confed_converter;
#[cfg(test)]
#[path = "../tests/corpus.rs"]
mod corpus;
#[cfg(test)]
mod corpus_tests;

pub use blockmap::{BlockEntry, BlockKind, BlockMap, Side, SpanError};
pub use dom::MAX_DEPTH;
pub use error::{ConvertError, ConvertResult};
pub use marks::{Mark, MarkId, MarkIssue, PlacedMark, Stripped};

use std::collections::HashMap;

/// A person mentioned in a page, resolved well enough to link to.
///
/// Confluence identifies people by an opaque key, so a mention can only be
/// written as `[@Name](profile)` once someone has looked that key up. The
/// converter does no IO, so the caller supplies the answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserLink {
    /// Shown after the `@`.
    pub display_name: String,
    /// Absolute URL of the profile page, in whatever form this site uses:
    /// `…/display/~username` on Data Center, `…/people/<account id>` on Cloud.
    pub profile_url: String,
    /// The attribute Confluence identifies them by, kept so an edited
    /// mention can be rebuilt exactly.
    pub id_attr: UserIdAttr,
    pub id_value: String,
}

/// How a mention names a person: the attribute of its `ri:user`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UserIdAttr {
    /// `ri:userkey`, Data Center's.
    #[default]
    UserKey,
    /// `ri:account-id`, Cloud's.
    AccountId,
    /// `ri:username`, older Data Center content.
    Username,
}

impl UserIdAttr {
    /// The attribute as storage writes it, without the `ri:` prefix.
    pub fn as_str(self) -> &'static str {
        match self {
            UserIdAttr::UserKey => "userkey",
            UserIdAttr::AccountId => "account-id",
            UserIdAttr::Username => "username",
        }
    }

    pub fn parse(attr: &str) -> Option<Self> {
        match attr {
            "userkey" => Some(UserIdAttr::UserKey),
            "account-id" => Some(UserIdAttr::AccountId),
            "username" => Some(UserIdAttr::Username),
            _ => None,
        }
    }
}

impl std::fmt::Display for UserIdAttr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The local file a page link can lead to, by what the link names: the
/// page's id, its title in this space, or its title in a named space. One
/// map per kind of key, so a title that happens to be a number is not
/// taken for an id.
#[derive(Clone, Debug, Default)]
pub struct PageLinks {
    /// page id → path of that page's file, relative to this page's file.
    pub by_id: HashMap<String, String>,
    /// title of a page of this space → its path.
    pub by_title: HashMap<String, String>,
    /// (space key, title) → its path.
    pub by_space_title: HashMap<(String, String), String>,
}

impl PageLinks {
    /// The path a link to page `id`, titled `title` in `space`, leads to
    /// here, if it leads anywhere here. `own` says `space` is this one, where
    /// a title alone is enough.
    pub fn find(&self, id: &str, title: &str, space: &str, own: bool) -> Option<&String> {
        let by_id = (!id.is_empty()).then(|| self.by_id.get(id)).flatten();
        let by_title = || (own && !title.is_empty()).then(|| self.by_title.get(title)).flatten();
        let qualified = || self.by_space_title.get(&(space.to_string(), title.to_string()));
        by_id.or_else(by_title).or_else(qualified)
    }
}

/// What a Markdown link to a local page file goes back up as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageTarget {
    /// The page with this id.
    Id(String),
    /// The page with this title, in this space.
    Title(String),
}

/// Context the converter needs to rewrite links and attachment references.
#[derive(Clone, Debug, Default)]
pub struct ConvertOptions {
    /// Path of the page's sidecar directory relative to the Markdown file,
    /// e.g. `.Onboarding` — attachments render as `.Onboarding/diagram.png`.
    pub attachment_dir: String,
    /// Where page links lead locally, to turn same-space `ac:link`s into
    /// relative Markdown links.
    pub page_links: PageLinks,
    /// Relative link → the page it names, for turning Markdown links back
    /// into `ac:link`.
    pub link_targets: HashMap<String, PageTarget>,
    /// page id → the page's title on the server, for page links written by title.
    pub page_titles: HashMap<String, String>,
    /// Write page links by title (`ri:content-title`), which is all Data Center
    /// resolves; Cloud takes `ri:content-id`.
    pub links_by_title: bool,
    /// Site base URL, for links confed cannot express locally.
    pub base_url: String,
    pub space_key: String,
    /// People referenced by this page, keyed by the id in the markup. A mention
    /// confed cannot resolve makes its block preserve verbatim rather than
    /// render a link to the wrong place.
    pub users: HashMap<String, UserLink>,
    /// Open inline comments to show in the body as marks (design 06), keyed by
    /// Confluence's marker ref — the `ac:ref` of the `ac:inline-comment-marker`
    /// element in storage. Empty means "render no marks".
    pub inline_marks: HashMap<String, InlineMark>,
}

/// One inline comment as the converter needs to know it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineMark {
    /// Comment id, written into the mark so readers can find the thread.
    pub id: String,
    /// One-line preview written into the opener; see [`marks::preview`].
    pub preview: String,
}

impl ConvertOptions {
    /// Comment id → marker ref, the inverse of [`ConvertOptions::inline_marks`],
    /// for putting markers back into regenerated storage.
    pub fn marker_ref_for(&self, comment_id: &str) -> Option<&str> {
        self.inline_marks.iter().find(|(_, m)| m.id == comment_id).map(|(r, _)| r.as_str())
    }
}

/// Result of rendering a storage body to Markdown.
#[derive(Clone, Debug)]
pub struct Converted {
    pub markdown: String,
    /// Maps each top-level storage block to the Markdown lines it produced.
    pub block_map: BlockMap,
}

/// Render a storage-format body to Markdown, preserving unknown constructs.
pub fn storage_to_markdown(storage: &str, opts: &ConvertOptions) -> ConvertResult<Converted> {
    let doc = storage_parse::parse(storage)?;
    to_markdown::render(&doc, storage, opts)
}

/// Generate a storage body from Markdown with no base to patch against
/// (new pages, comment bodies).
pub fn markdown_to_storage(markdown: &str, opts: &ConvertOptions) -> ConvertResult<String> {
    to_storage::generate(markdown, opts)
}

/// Block-level patch: rebuild the storage body from edited Markdown, reusing the
/// original storage bytes for every block whose Markdown is unchanged.
///
/// `base_markdown` must be the Markdown that `base_storage` produced (confed keeps
/// both, so this holds by construction).
pub fn markdown_to_storage_patched(
    base_storage: &str,
    base_map: &BlockMap,
    base_markdown: &str,
    new_markdown: &str,
    opts: &ConvertOptions,
) -> ConvertResult<String> {
    to_storage::patch(base_storage, base_map, base_markdown, new_markdown, opts)
}

/// Every person mentioned in a storage body, as `(attribute, value)` pairs —
/// for example `("userkey", "6cb6d404…")`.
///
/// The caller resolves these into [`UserLink`]s and passes them back through
/// [`ConvertOptions::users`]; a mention nobody resolved keeps its block verbatim
/// rather than linking to the wrong profile.
pub fn user_references(storage: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (attr, needle) in [
        ("account-id", "ri:account-id=\""),
        ("userkey", "ri:userkey=\""),
        ("username", "ri:username=\""),
    ] {
        let mut rest = storage;
        while let Some(start) = rest.find(needle) {
            rest = &rest[start + needle.len()..];
            let Some(end) = rest.find('"') else { break };
            let value = dom::unescape(&rest[..end]);
            if !value.is_empty() && !out.iter().any(|(_, v)| v == &value) {
                out.push((attr.to_string(), value));
            }
            rest = &rest[end + 1..];
        }
    }
    out
}

/// The files a storage body refers to as attachments of its own page: images
/// shown in it and files linked from it.
///
/// An `ri:attachment` that names another page as its container is that page's
/// file and is left out. Markup that does not parse yields nothing — this
/// answers a question about a body, it does not validate it.
pub fn attachment_references(storage: &str) -> Vec<String> {
    fn collect(nodes: &[dom::Node], out: &mut Vec<String>) {
        for element in nodes.iter().filter_map(dom::Node::as_element) {
            if element.name == "ri:attachment" {
                let own = element.child_elements().next().is_none();
                if let (true, Some(file)) = (own, element.attr("ri:filename")) {
                    if !file.is_empty() && !out.iter().any(|f| f == file) {
                        out.push(file.to_string());
                    }
                }
            } else {
                collect(&element.children, out);
            }
        }
    }
    let mut out = Vec::new();
    if storage.contains("ri:attachment") {
        if let Ok(nodes) = dom::parse_fragment(storage) {
            collect(&nodes, &mut out);
        }
    }
    out
}

/// Convenience for comment bodies, which are short storage fragments.
pub fn storage_fragment_to_markdown(storage: &str) -> ConvertResult<String> {
    let opts = ConvertOptions::default();
    Ok(storage_to_markdown(storage, &opts)?.markdown)
}

#[cfg(test)]
mod user_reference_tests {
    #[test]
    fn every_mention_is_found_once_with_its_attribute() {
        let storage = "<p>Ask <ac:link><ri:user ri:userkey=\"abc123\"/></ac:link> or \
            <ac:link><ri:user ri:account-id=\"557058:x\"/></ac:link>, and \
            <ac:link><ri:user ri:userkey=\"abc123\"/></ac:link> again.</p>";
        let mut found = super::user_references(storage);
        found.sort();

        assert_eq!(
            found,
            [
                ("account-id".to_string(), "557058:x".to_string()),
                ("userkey".to_string(), "abc123".to_string())
            ],
            "each person appears once, tagged with how they were identified"
        );
    }

    #[test]
    fn a_page_with_nobody_in_it_yields_nothing() {
        assert!(super::user_references("<p>Just prose.</p>").is_empty());
        assert!(super::user_references("").is_empty());
    }

    #[test]
    fn an_escaped_value_is_unescaped() {
        let found = super::user_references(r#"<ri:user ri:username="a&amp;b"/>"#);
        assert_eq!(found, [("username".to_string(), "a&b".to_string())]);
    }
}

#[cfg(test)]
mod attachment_reference_tests {
    use super::attachment_references;

    #[test]
    fn images_and_file_links_name_the_pages_own_attachments() {
        let storage =
            "<p>See <ac:image ac:width=\"300\"><ri:attachment ri:filename=\"chart.png\"/>\
            </ac:image> and <ac:link><ri:attachment ri:filename=\"a &amp; b.pdf\" />\
            <ac:plain-text-link-body><![CDATA[the spec]]></ac:plain-text-link-body></ac:link>, \
            then <ac:image><ri:attachment ri:filename=\"chart.png\"/></ac:image> again.</p>";
        assert_eq!(attachment_references(storage), ["chart.png", "a & b.pdf"]);
    }

    #[test]
    fn a_file_on_another_page_is_not_this_pages() {
        let storage = "<p><ac:link><ri:attachment ri:filename=\"elsewhere.pdf\">\
            <ri:page ri:content-title=\"Team Handbook\"/></ri:attachment></ac:link></p>";
        assert!(attachment_references(storage).is_empty());
    }

    #[test]
    fn a_body_without_attachments_or_without_sense_yields_nothing() {
        assert!(attachment_references("<p>Just prose.</p>").is_empty());
        assert!(attachment_references("").is_empty());
        assert!(attachment_references("<p><ri:attachment ri:filename=\"x.png\"").is_empty());
    }
}
