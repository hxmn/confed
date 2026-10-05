//! The guarantee that makes push safe: editing one block must not rewrite any
//! other block.
//!
//! Every assertion here is about *bytes*. Structural equivalence is not enough —
//! a re-serialised paragraph that happens to mean the same thing still shows up
//! as churn in the page history and still drops whatever the converter cannot
//! model.

mod corpus;

use confed_converter::blockmap::{slice_lines, BlockKind, BlockMap};
use confed_converter::{markdown_to_storage_patched, storage_to_markdown, ConvertError, Converted};

const MARK: &str = "ZZQEDITED";

/// Blocks whose Markdown a user can safely append a word to without changing
/// how the document splits into blocks.
fn editable(map: &BlockMap) -> Vec<usize> {
    map.blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b.kind, BlockKind::Paragraph | BlockKind::Heading))
        .map(|(i, _)| i)
        .collect()
}

/// Rebuild the Markdown with `perturbed` blocks edited in place.
fn perturb(c: &Converted, perturbed: &[usize]) -> String {
    let mut out = String::new();
    for (i, b) in c.block_map.blocks.iter().enumerate() {
        let text = slice_lines(&c.markdown, b.md_span.0, b.md_span.1);
        if perturbed.contains(&i) {
            match text.find('\n') {
                Some(nl) => {
                    out.push_str(&text[..nl]);
                    out.push(' ');
                    out.push_str(MARK);
                    out.push_str(&text[nl..]);
                }
                None => {
                    out.push_str(&text);
                    out.push(' ');
                    out.push_str(MARK);
                }
            }
        } else {
            out.push_str(&text);
        }
    }
    out
}

/// Assert that the original storage bytes of every block *not* in `changed`
/// appear in `patched`, in order, without overlapping.
fn assert_untouched_blocks_are_byte_identical(
    fixture: &str,
    storage: &str,
    map: &BlockMap,
    changed: &[usize],
    patched: &str,
) {
    let mut cursor = 0usize;
    for (i, b) in map.blocks.iter().enumerate() {
        if changed.contains(&i) {
            continue;
        }
        let raw = &storage[b.storage_span.0..b.storage_span.1];
        match patched[cursor..].find(raw) {
            Some(at) => cursor += at + raw.len(),
            None => panic!(
                "{fixture}: block {i} ({:?}) was rewritten.\n--- original ---\n{raw}\n--- patched output ---\n{patched}",
                b.kind
            ),
        }
    }
}

#[test]
fn no_edit_patch_reproduces_the_base_byte_for_byte() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        let out = markdown_to_storage_patched(
            &f.storage,
            &c.block_map,
            &c.markdown,
            &c.markdown,
            &corpus::options(),
        )
        .unwrap_or_else(|e| panic!("{}: {e}", f.name));
        assert_eq!(out, f.storage, "{}: identity patch changed the body", f.name);
    }
}

#[test]
fn editing_one_block_leaves_every_other_block_untouched() {
    let mut checked = 0usize;
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        for i in editable(&c.block_map) {
            let new_md = perturb(&c, &[i]);
            let out = markdown_to_storage_patched(
                &f.storage,
                &c.block_map,
                &c.markdown,
                &new_md,
                &corpus::options(),
            )
            .unwrap_or_else(|e| panic!("{} block {i}: {e}", f.name));
            assert!(out.contains(MARK), "{}: the edit did not reach the output", f.name);
            assert_eq!(out.matches(MARK).count(), 1, "{}: edit leaked", f.name);
            assert_untouched_blocks_are_byte_identical(
                &f.name,
                &f.storage,
                &c.block_map,
                &[i],
                &out,
            );
            checked += 1;
        }
    }
    assert!(checked >= 40, "expected a broad sweep; only checked {checked}");
}

#[test]
fn editing_a_random_subset_leaves_the_rest_untouched() {
    // A deterministic LCG: reproducible failures beat a wider search.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        seed >> 33
    };
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        let candidates = editable(&c.block_map);
        if candidates.is_empty() {
            continue;
        }
        for _ in 0..16 {
            let chosen: Vec<usize> =
                candidates.iter().copied().filter(|_| next() % 2 == 0).collect();
            if chosen.is_empty() {
                continue;
            }
            let new_md = perturb(&c, &chosen);
            let out = markdown_to_storage_patched(
                &f.storage,
                &c.block_map,
                &c.markdown,
                &new_md,
                &corpus::options(),
            )
            .unwrap_or_else(|e| panic!("{} {chosen:?}: {e}", f.name));
            assert_eq!(
                out.matches(MARK).count(),
                chosen.len(),
                "{}: edited {chosen:?} but the output carries a different count",
                f.name
            );
            assert_untouched_blocks_are_byte_identical(
                &f.name,
                &f.storage,
                &c.block_map,
                &chosen,
                &out,
            );
        }
    }
}

#[test]
fn editing_every_block_still_produces_well_formed_storage() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        let all = editable(&c.block_map);
        if all.is_empty() {
            continue;
        }
        let new_md = perturb(&c, &all);
        let out = markdown_to_storage_patched(
            &f.storage,
            &c.block_map,
            &c.markdown,
            &new_md,
            &corpus::options(),
        )
        .unwrap_or_else(|e| panic!("{}: {e}", f.name));
        assert_eq!(out.matches(MARK).count(), all.len(), "{}", f.name);
        // Blocks nobody touched are still verbatim.
        assert_untouched_blocks_are_byte_identical(&f.name, &f.storage, &c.block_map, &all, &out);
    }
}

#[test]
fn deleting_a_block_removes_only_that_block() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        if c.block_map.len() < 2 {
            continue;
        }
        for victim in 0..c.block_map.len() {
            let mut new_md = String::new();
            for (i, b) in c.block_map.blocks.iter().enumerate() {
                if i == victim {
                    continue;
                }
                new_md.push_str(&slice_lines(&c.markdown, b.md_span.0, b.md_span.1));
            }
            if new_md.trim().is_empty() {
                continue;
            }
            let out = markdown_to_storage_patched(
                &f.storage,
                &c.block_map,
                &c.markdown,
                &new_md,
                &corpus::options(),
            )
            .unwrap_or_else(|e| panic!("{} delete {victim}: {e}", f.name));

            // Removing a block can make its neighbours *adjacent*, and two
            // adjacent Markdown lists with the same bullet are one list — the
            // text genuinely says something different now, so those neighbours
            // count as edited. This is the one documented case where deleting
            // a block regenerates more than itself (see README, "Fusion").
            let mut changed = vec![victim];
            if confed_converter::mdblock::split_blocks(&new_md).len() < c.block_map.len() - 1 {
                if victim > 0 {
                    changed.push(victim - 1);
                }
                if victim + 1 < c.block_map.len() {
                    changed.push(victim + 1);
                }
            }
            assert_untouched_blocks_are_byte_identical(
                &f.name,
                &f.storage,
                &c.block_map,
                &changed,
                &out,
            );
        }
    }
}

#[test]
fn inserting_a_block_does_not_disturb_its_neighbours() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        for at in 0..=c.block_map.len() {
            let mut new_md = String::new();
            for (i, b) in c.block_map.blocks.iter().enumerate() {
                if i == at {
                    new_md.push_str("A brand new paragraph ZZQINSERT.\n\n");
                }
                new_md.push_str(&slice_lines(&c.markdown, b.md_span.0, b.md_span.1));
            }
            if at == c.block_map.len() {
                if !new_md.ends_with("\n\n") {
                    new_md.push('\n');
                }
                new_md.push_str("A brand new paragraph ZZQINSERT.\n");
            }
            let out = markdown_to_storage_patched(
                &f.storage,
                &c.block_map,
                &c.markdown,
                &new_md,
                &corpus::options(),
            )
            .unwrap_or_else(|e| panic!("{} insert at {at}: {e}", f.name));
            assert!(out.contains("ZZQINSERT"), "{}: insertion missing", f.name);
            assert_untouched_blocks_are_byte_identical(
                &f.name,
                &f.storage,
                &c.block_map,
                &[],
                &out,
            );
        }
    }
}

#[test]
fn a_stale_block_map_is_reported_rather_than_guessed_at() {
    let f = &corpus::load()[0];
    let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();

    // Base Markdown that the map does not describe.
    let err = markdown_to_storage_patched(
        &f.storage,
        &c.block_map,
        "completely different base\n",
        &c.markdown,
        &corpus::options(),
    )
    .unwrap_err();
    assert!(matches!(err, ConvertError::StaleBlockMap(_)), "got {err:?}");

    // A map whose hashes no longer match.
    let mut tampered = c.block_map.clone();
    tampered.blocks[0].hash = "0".repeat(64);
    let err = markdown_to_storage_patched(
        &f.storage,
        &tampered,
        &c.markdown,
        &c.markdown,
        &corpus::options(),
    )
    .unwrap_err();
    assert!(matches!(err, ConvertError::StaleBlockMap(_)), "got {err:?}");
}

#[test]
fn a_corrupted_preserved_fence_fails_the_push() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        let Some(i) = c.block_map.blocks.iter().position(|b| b.kind == BlockKind::Preserved) else {
            continue;
        };
        // Chop the closing tag off the preserved storage.
        let mut new_md = String::new();
        for (j, b) in c.block_map.blocks.iter().enumerate() {
            let text = slice_lines(&c.markdown, b.md_span.0, b.md_span.1);
            if j == i {
                new_md.push_str(&text.replacen("</ac:", "<!--broken--></XX:", 1));
            } else {
                new_md.push_str(&text);
            }
        }
        if new_md == c.markdown {
            continue;
        }
        let err = markdown_to_storage_patched(
            &f.storage,
            &c.block_map,
            &c.markdown,
            &new_md,
            &corpus::options(),
        )
        .unwrap_err();
        assert!(
            matches!(err, ConvertError::InvalidPreservedBlock { .. }),
            "{}: expected InvalidPreservedBlock, got {err:?}",
            f.name
        );
        return;
    }
    panic!("no fixture exercised a preserved block");
}
