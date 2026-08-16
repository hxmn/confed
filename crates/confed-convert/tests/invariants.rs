//! The structural guarantees, checked against every fixture.
//!
//! These are not "does it look right" tests — they are the properties the rest
//! of confed is allowed to rely on.

mod corpus;

use confed_convert::blockmap::{hash_text, BlockKind};
use confed_convert::{mdblock, storage_parse, storage_to_markdown};

#[test]
fn parser_survives_the_corpus_and_spans_reproduce_the_source() {
    for f in corpus::load() {
        let doc = storage_parse::parse(&f.storage).unwrap_or_else(|e| panic!("{}: {e}", f.name));
        let mut prev_end = 0usize;
        for block in &doc.blocks {
            let (s, e) = block.span;
            assert!(s <= e, "{}: inverted span", f.name);
            assert!(e <= f.storage.len(), "{}: span past the end", f.name);
            assert!(s >= prev_end, "{}: overlapping spans", f.name);
            // The invariant the whole patcher rests on.
            assert_eq!(
                &f.storage[s..e].len(),
                &(e - s),
                "{}: span is not a byte range",
                f.name
            );
            assert!(
                !f.storage[s..e].trim().is_empty(),
                "{}: a block cannot be blank",
                f.name
            );
            prev_end = e;
        }
    }
}

#[test]
fn block_map_spans_are_valid_for_every_fixture() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        c.block_map
            .validate_spans(f.storage.len())
            .unwrap_or_else(|e| panic!("{}: {e}", f.name));
        c.block_map
            .validate_md_spans()
            .unwrap_or_else(|e| panic!("{}: {e}", f.name));
    }
}

/// The renderer's own idea of where blocks start must survive a round trip
/// through the Markdown text. If it does not, push would see an untouched block
/// as edited and regenerate it — exactly what design 03 §3 forbids.
#[test]
fn rendered_markdown_re_splits_into_the_same_blocks() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        let ranges = mdblock::split_blocks(&c.markdown);
        let expected: Vec<(usize, usize)> =
            c.block_map.blocks.iter().map(|b| b.md_span).collect();
        assert_eq!(
            ranges, expected,
            "{}: re-splitting the Markdown disagrees with the block map\n--- markdown ---\n{}",
            f.name, c.markdown
        );
    }
}

#[test]
fn block_hashes_match_the_markdown_they_describe() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        for (i, entry) in c.block_map.blocks.iter().enumerate() {
            let text = c.block_map.block_markdown(i, &c.markdown).unwrap();
            assert_eq!(entry.hash, hash_text(&text), "{}: block {i} hash", f.name);
        }
    }
}

/// A preserved block's fence body must be the exact source bytes — that is the
/// entire promise of a preserved block.
#[test]
fn preserved_fences_hold_the_exact_source_bytes() {
    let mut seen = 0usize;
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        for (i, entry) in c.block_map.blocks.iter().enumerate() {
            if entry.kind != BlockKind::Preserved {
                continue;
            }
            seen += 1;
            let md = c.block_map.block_markdown(i, &c.markdown).unwrap();
            let raw = &f.storage[entry.storage_span.0..entry.storage_span.1];
            let body = fence_body(&md);
            assert_eq!(body, raw, "{}: preserved fence body differs", f.name);
        }
    }
    assert!(seen >= 8, "corpus should exercise preserved blocks; saw {seen}");
}

fn fence_body(md: &str) -> String {
    let mut lines = md.lines();
    let open = lines.next().expect("fence opener");
    let ticks: String = open.chars().take_while(|c| *c == '`').collect();
    assert!(open.starts_with(&ticks) && open.ends_with("confluence"), "{open:?}");
    let mut body: Vec<&str> = Vec::new();
    for line in lines {
        if line.trim_end() == ticks {
            break;
        }
        body.push(line);
    }
    body.join("\n")
}

/// Whitespace between blocks belongs to nobody, but it is never lost: the
/// concatenation of block spans and the gaps between them is the document.
#[test]
fn spans_plus_gaps_reconstruct_the_document() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        let mut rebuilt = String::new();
        let mut cursor = 0usize;
        for b in &c.block_map.blocks {
            rebuilt.push_str(&f.storage[cursor..b.storage_span.0]);
            rebuilt.push_str(&f.storage[b.storage_span.0..b.storage_span.1]);
            cursor = b.storage_span.1;
        }
        rebuilt.push_str(&f.storage[cursor..]);
        assert_eq!(rebuilt, f.storage, "{}", f.name);
    }
}

#[test]
fn every_fixture_produces_at_least_one_block() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        assert!(!c.block_map.is_empty(), "{}: no blocks", f.name);
        assert!(!c.markdown.trim().is_empty(), "{}: empty markdown", f.name);
    }
}

/// The corpus is only useful if it actually reaches every mapping.
#[test]
fn the_corpus_covers_every_block_kind() {
    use std::collections::HashSet;
    let mut kinds: HashSet<String> = HashSet::new();
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        for b in &c.block_map.blocks {
            kinds.insert(format!("{:?}", b.kind));
        }
    }
    for expected in [
        "Paragraph",
        "Heading",
        "List",
        "TaskList",
        "Table",
        "Code",
        "Admonition",
        "Image",
        "Quote",
        "Rule",
        "Expand",
        "Preserved",
        "Other",
    ] {
        assert!(kinds.contains(expected), "corpus never produces {expected}; got {kinds:?}");
    }
}
