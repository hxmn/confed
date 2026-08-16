//! Confluence storage format (XHTML + macros) ⇄ Markdown.
//!
//! Two guarantees shape everything here:
//!
//! 1. **Nothing is lost.** Elements the converter does not model are preserved
//!    verbatim inside ```` ```confluence ```` fences and re-emitted byte-identically.
//! 2. **Only edited blocks are regenerated.** [`markdown_to_storage_patched`] copies
//!    the original storage bytes for every top-level block the user did not touch,
//!    so lossiness is confined to blocks that actually changed.

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

/// Convenience for comment bodies, which are short storage fragments.
pub fn storage_fragment_to_markdown(storage: &str) -> ConvertResult<String> {
    let opts = ConvertOptions::default();
    Ok(storage_to_markdown(storage, &opts)?.markdown)
}
