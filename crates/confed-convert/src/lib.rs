//! Confluence storage format (XHTML + macros) ⇄ Markdown.
//!
//! Two guarantees shape everything here:
//!
//! 1. **Nothing is lost.** Elements the converter does not model are preserved
//!    verbatim inside ```` ```confluence ```` fences and re-emitted byte-identically.
//! 2. **Only edited blocks are regenerated.** [`markdown_to_storage_patched`] copies
//!    the original storage bytes for every top-level block the user did not touch,
//!    so lossiness is confined to blocks that actually changed.

/// Bumped whenever the rendering rules change in a way that makes previously
/// converted Markdown out of date. It feeds the render fingerprint confed keeps
/// per page, so an improvement reaches a workspace on the next pull without
/// waiting for each page to change on the server.
pub const CONVERTER_VERSION: u32 = 3;

pub mod blockmap;
pub mod dom;
pub mod error;
pub mod macros;
pub mod mdblock;
pub mod storage_parse;
pub mod to_markdown;
pub mod to_storage;

pub use blockmap::{BlockEntry, BlockKind, BlockMap};
pub use error::{ConvertError, ConvertResult};

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
    /// The attribute Confluence identifies them by — `userkey`, `account-id`
    /// or `username` — kept so an edited mention can be rebuilt exactly.
    pub id_attr: String,
    pub id_value: String,
}

/// Context the converter needs to rewrite links and attachment references.
#[derive(Clone, Debug, Default)]
pub struct ConvertOptions {
    /// Path of the page's sidecar directory relative to the Markdown file,
    /// e.g. `.Onboarding` — attachments render as `.Onboarding/diagram.png`.
    pub attachment_dir: String,
    /// page id → path of that page's Markdown file, relative to this page's file.
    /// Used to turn same-space `ac:link`s into relative Markdown links.
    pub page_links: HashMap<String, String>,
    /// Reverse of `page_links`, for turning Markdown links back into `ac:link`.
    pub link_targets: HashMap<String, String>,
    /// Site base URL, for links confed cannot express locally.
    pub base_url: String,
    pub space_key: String,
    /// People referenced by this page, keyed by the id in the markup. A mention
    /// confed cannot resolve makes its block preserve verbatim rather than
    /// render a link to the wrong place.
    pub users: HashMap<String, UserLink>,
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
