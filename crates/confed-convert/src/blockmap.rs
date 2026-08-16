//! The block map ties storage byte ranges to the Markdown lines they produced.
//! It is what makes "only regenerate what changed" possible on push.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Paragraph,
    Heading,
    List,
    TaskList,
    Table,
    Code,
    Admonition,
    Image,
    Quote,
    Rule,
    Expand,
    /// An element confed does not model: kept verbatim in a `confluence` fence.
    Preserved,
    Other,
}

impl BlockKind {
    /// Blocks whose Markdown must never be re-parsed into storage — they carry
    /// raw storage bytes and are copied through untouched.
    pub fn is_verbatim(self) -> bool {
        matches!(self, BlockKind::Preserved)
    }
}

/// One top-level block: its bytes in the storage document and its lines in the
/// rendered Markdown.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockEntry {
    pub kind: BlockKind,
    /// Byte range in the storage body: `storage[start..end]` is this block.
    pub storage_span: (usize, usize),
    /// Line range in the Markdown body, 0-based and half-open: `[start, end)`.
    pub md_span: (usize, usize),
    /// sha256 of the Markdown text of this block, used to align base vs. local.
    pub hash: String,
}

impl BlockEntry {
    pub fn md_line_count(&self) -> usize {
        self.md_span.1.saturating_sub(self.md_span.0)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BlockMap {
    pub blocks: Vec<BlockEntry>,
}

impl BlockMap {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Storage spans must cover the document in order without overlapping.
    pub fn validate_spans(&self, storage_len: usize) -> Result<(), String> {
        let mut prev_end = 0usize;
        for (i, b) in self.blocks.iter().enumerate() {
            let (s, e) = b.storage_span;
            if s > e {
                return Err(format!("block {i}: inverted storage span {s}..{e}"));
            }
            if e > storage_len {
                return Err(format!(
                    "block {i}: storage span {s}..{e} exceeds body length {storage_len}"
                ));
            }
            if s < prev_end {
                return Err(format!("block {i}: storage span {s}..{e} overlaps previous block"));
            }
            prev_end = e;
        }
        Ok(())
    }

    /// Markdown line ranges must be ordered and non-overlapping.
    pub fn validate_md_spans(&self) -> Result<(), String> {
        let mut prev_end = 0usize;
        for (i, b) in self.blocks.iter().enumerate() {
            let (s, e) = b.md_span;
            if s > e {
                return Err(format!("block {i}: inverted md span {s}..{e}"));
            }
            if s < prev_end {
                return Err(format!("block {i}: md span {s}..{e} overlaps previous block"));
            }
            prev_end = e;
        }
        Ok(())
    }

    /// The Markdown text of a block, given the full Markdown body.
    pub fn block_markdown(&self, index: usize, markdown: &str) -> Option<String> {
        let entry = self.blocks.get(index)?;
        Some(slice_lines(markdown, entry.md_span.0, entry.md_span.1))
    }
}

/// Extract `[start, end)` lines from `text`, keeping a trailing newline when the
/// original had one.
pub fn slice_lines(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if i >= end {
            break;
        }
        if i >= start {
            out.push_str(line);
        }
    }
    out
}

pub fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    // Normalize trailing whitespace so cosmetic edits do not look like content
    // changes and force a needless block regeneration.
    hasher.update(text.trim_end().as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: BlockKind, s: (usize, usize), m: (usize, usize)) -> BlockEntry {
        BlockEntry { kind, storage_span: s, md_span: m, hash: hash_text("x") }
    }

    #[test]
    fn valid_maps_pass_validation() {
        let map = BlockMap {
            blocks: vec![
                entry(BlockKind::Paragraph, (0, 10), (0, 2)),
                entry(BlockKind::Heading, (10, 25), (2, 4)),
            ],
        };
        map.validate_spans(25).unwrap();
        map.validate_md_spans().unwrap();
    }

    #[test]
    fn overlapping_storage_spans_are_rejected() {
        let map = BlockMap {
            blocks: vec![
                entry(BlockKind::Paragraph, (0, 10), (0, 2)),
                entry(BlockKind::Paragraph, (5, 20), (2, 4)),
            ],
        };
        assert!(map.validate_spans(20).is_err());
    }

    #[test]
    fn spans_past_the_end_are_rejected() {
        let map = BlockMap { blocks: vec![entry(BlockKind::Paragraph, (0, 99), (0, 1))] };
        assert!(map.validate_spans(10).is_err());
    }

    #[test]
    fn line_slicing_is_half_open() {
        let md = "a\nb\nc\n";
        assert_eq!(slice_lines(md, 0, 1), "a\n");
        assert_eq!(slice_lines(md, 1, 3), "b\nc\n");
        assert_eq!(slice_lines(md, 2, 9), "c\n");
    }

    #[test]
    fn hashing_ignores_trailing_whitespace_only() {
        assert_eq!(hash_text("hello\n\n"), hash_text("hello"));
        assert_ne!(hash_text("hello"), hash_text("Hello"));
        assert_ne!(hash_text("a\nb"), hash_text("a b"));
    }
}
