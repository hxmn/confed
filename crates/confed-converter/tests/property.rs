//! The design-03 §6 property test, driven by proptest.
//!
//! The exhaustive sweeps live in `patching.rs`; this one searches the space of
//! *combinations* of edits and shrinks a counterexample to the smallest set of
//! edited blocks that breaks the guarantee.

mod corpus;

use confed_converter::blockmap::{slice_lines, BlockKind};
use confed_converter::{markdown_to_storage_patched, storage_to_markdown, Converted};
use proptest::prelude::*;

const MARK: &str = "ZZQPROP";

struct Case {
    name: String,
    storage: String,
    converted: Converted,
    editable: Vec<usize>,
}

fn cases() -> Vec<Case> {
    corpus::load()
        .into_iter()
        .map(|f| {
            let converted = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
            let editable = converted
                .block_map
                .blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| matches!(b.kind, BlockKind::Paragraph | BlockKind::Heading))
                .map(|(i, _)| i)
                .collect();
            Case { name: f.name, storage: f.storage, converted, editable }
        })
        .filter(|c: &Case| !c.editable.is_empty())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// For any document in the corpus and any subset of its editable blocks,
    /// the blocks outside that subset come back byte-identical.
    #[test]
    fn unedited_blocks_are_byte_identical(
        doc_index in 0usize..64,
        mask in any::<u64>(),
    ) {
        let all = cases();
        let case = &all[doc_index % all.len()];

        let chosen: Vec<usize> = case
            .editable
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask >> (bit % 64) & 1 == 1)
            .map(|(_, i)| *i)
            .collect();
        prop_assume!(!chosen.is_empty());

        let mut new_md = String::new();
        for (i, b) in case.converted.block_map.blocks.iter().enumerate() {
            let text = slice_lines(&case.converted.markdown, b.md_span.0, b.md_span.1);
            if chosen.contains(&i) {
                let nl = text.find('\n').unwrap_or(text.len());
                new_md.push_str(&text[..nl]);
                new_md.push(' ');
                new_md.push_str(MARK);
                new_md.push_str(&text[nl..]);
            } else {
                new_md.push_str(&text);
            }
        }

        let out = markdown_to_storage_patched(
            &case.storage,
            &case.converted.block_map,
            &case.converted.markdown,
            &new_md,
            &corpus::options(),
        )
        .unwrap_or_else(|e| panic!("{} {chosen:?}: {e}", case.name));

        prop_assert_eq!(
            out.matches(MARK).count(),
            chosen.len(),
            "{}: edits did not land one-for-one",
            case.name
        );

        let mut cursor = 0usize;
        for (i, b) in case.converted.block_map.blocks.iter().enumerate() {
            if chosen.contains(&i) {
                continue;
            }
            let raw = &case.storage[b.storage_span.0..b.storage_span.1];
            match out[cursor..].find(raw) {
                Some(at) => cursor += at + raw.len(),
                None => prop_assert!(
                    false,
                    "{}: block {i} ({:?}) was rewritten when editing {chosen:?}",
                    case.name,
                    b.kind
                ),
            }
        }
    }
}
