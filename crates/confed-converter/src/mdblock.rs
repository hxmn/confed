//! One definition of "what is a Markdown block", shared by both directions.
//!
//! [`crate::to_markdown`] emits blocks separated by a blank line and records the
//! line range of each; this module recovers exactly those ranges from the text
//! alone. Push depends on the two agreeing: if re-splitting the base Markdown
//! did not reproduce the stored block map, an untouched block would look edited
//! and get regenerated (losing whatever the converter cannot model), which is
//! precisely what design 03 §3 promises never happens.

use comrak::nodes::AstNode;
use comrak::{parse_document, Arena, Options};

/// The parser configuration confed uses everywhere. GFM plus alerts, which is
/// what the storage → Markdown renderer emits.
pub fn options() -> Options<'static> {
    let mut o = Options::default();
    o.extension.table = true;
    o.extension.strikethrough = true;
    o.extension.tasklist = true;
    o.extension.autolink = true;
    o.extension.alerts = true;
    o
}

/// Line ranges (0-based, half-open) of the top-level blocks in `markdown`.
///
/// Ranges are contiguous and cover the whole document: a block owns the blank
/// line that separates it from the next one. That matches how `to_markdown`
/// assigns `md_span`s.
pub fn split_blocks(markdown: &str) -> Vec<(usize, usize)> {
    let total = line_count(markdown);
    if total == 0 {
        return Vec::new();
    }
    let arena = Arena::new();
    let root = parse_document(&arena, markdown, &options());
    let mut starts: Vec<usize> =
        root.children().map(|c| c.data.borrow().sourcepos.start.line.saturating_sub(1)).collect();
    if starts.is_empty() {
        return Vec::new();
    }
    // Leading blank lines belong to the first block rather than to nothing.
    starts[0] = 0;
    starts.dedup();
    (0..starts.len()).map(|i| (starts[i], starts.get(i + 1).copied().unwrap_or(total))).collect()
}

/// Parse once and hand back both the AST root and the block line ranges, so
/// callers that need the nodes do not pay for a second parse.
pub fn parse<'a>(arena: &'a Arena<AstNode<'a>>, markdown: &str) -> &'a AstNode<'a> {
    parse_document(arena, markdown, &options())
}

pub fn line_count(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_contiguous_and_cover_the_document() {
        let md = "# Title\n\npara one\n\n- a\n- b\n";
        assert_eq!(split_blocks(md), vec![(0, 2), (2, 4), (4, 6)]);
    }

    #[test]
    fn fenced_blocks_with_blank_lines_stay_whole() {
        let md = "```rust\nfn a() {\n\n}\n```\n\nafter\n";
        assert_eq!(split_blocks(md), vec![(0, 6), (6, 7)]);
    }

    #[test]
    fn alerts_are_one_block() {
        let md = "> [!NOTE]\n> **T**\n>\n> body\n\nafter\n";
        assert_eq!(split_blocks(md), vec![(0, 5), (5, 6)]);
    }

    #[test]
    fn tables_are_one_block() {
        let md = "| a | b |\n| --- | --- |\n| 1 | 2 |\n\nafter\n";
        assert_eq!(split_blocks(md), vec![(0, 4), (4, 5)]);
    }

    #[test]
    fn alternating_bullets_keep_adjacent_lists_apart() {
        assert_eq!(split_blocks("- a\n\n* b\n"), vec![(0, 2), (2, 3)]);
        // …and the same marker really does fuse them, which is why the renderer
        // alternates.
        assert_eq!(split_blocks("- a\n\n- b\n"), vec![(0, 3)]);
    }

    #[test]
    fn empty_input_has_no_blocks() {
        assert!(split_blocks("").is_empty());
    }

    #[test]
    fn details_html_block_is_one_block() {
        let md = "<details><summary>T</summary>\nbody\n</details>\n\nafter\n";
        assert_eq!(split_blocks(md), vec![(0, 4), (4, 5)]);
    }
}
