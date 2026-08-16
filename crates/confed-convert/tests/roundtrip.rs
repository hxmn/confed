//! Round trips through Markdown and back, and the lossless-fence guarantee.

mod corpus;

use confed_convert::blockmap::{slice_lines, BlockKind};
use confed_convert::{markdown_to_storage, storage_to_markdown, ConvertError, ConvertOptions};

/// `confluence` fences go back up exactly as they came down. This is the
/// property that lets confed be trusted with macros it does not understand.
#[test]
fn preserved_fences_survive_markdown_to_storage_byte_for_byte() {
    let mut checked = 0usize;
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        for (i, b) in c.block_map.blocks.iter().enumerate() {
            if b.kind != BlockKind::Preserved {
                continue;
            }
            let md = c.block_map.block_markdown(i, &c.markdown).unwrap();
            let out = markdown_to_storage(&md, &corpus::options())
                .unwrap_or_else(|e| panic!("{} block {i}: {e}", f.name));
            assert_eq!(
                out,
                &f.storage[b.storage_span.0..b.storage_span.1],
                "{}: preserved block {i} did not survive",
                f.name
            );
            checked += 1;
        }
    }
    assert!(checked >= 8, "corpus should exercise preserved blocks; saw {checked}");
}

/// Storage → Markdown → storage over a whole document. Not byte-identical by
/// construction (that is what `patch` is for), but it must be *stable*: the
/// second trip through Markdown reproduces the first.
#[test]
fn markdown_rendering_is_idempotent() {
    for f in corpus::load() {
        let first = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        let regenerated = markdown_to_storage(&first.markdown, &corpus::options())
            .unwrap_or_else(|e| panic!("{}: {e}", f.name));
        let second = storage_to_markdown(&regenerated, &corpus::options())
            .unwrap_or_else(|e| panic!("{} (second pass): {e}", f.name));
        assert_eq!(
            first.markdown, second.markdown,
            "{}: rendering is not a fixed point\n--- regenerated storage ---\n{}",
            f.name, regenerated
        );
    }
}

#[test]
fn generated_storage_is_always_well_formed() {
    for f in corpus::load() {
        let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
        // `markdown_to_storage` validates internally and errors if not.
        markdown_to_storage(&c.markdown, &corpus::options())
            .unwrap_or_else(|e| panic!("{}: {e}", f.name));
    }
}

#[test]
fn a_corrupted_fence_is_refused_with_the_offending_line() {
    let md = "intro paragraph\n\nsecond paragraph\n\n```confluence\n<ac:structured-macro ac:name=\"jira\">\n```\n";
    match markdown_to_storage(md, &ConvertOptions::default()) {
        Err(ConvertError::InvalidPreservedBlock { line, detail }) => {
            assert_eq!(line, 5, "should point at the fence opener");
            assert!(!detail.is_empty());
        }
        other => panic!("expected InvalidPreservedBlock, got {other:?}"),
    }
}

#[test]
fn an_edited_but_still_valid_fence_is_accepted() {
    let md = "```confluence\n<ac:structured-macro ac:name=\"jira\"><ac:parameter ac:name=\"key\">PROJ-999</ac:parameter></ac:structured-macro>\n```\n";
    let out = markdown_to_storage(md, &ConvertOptions::default()).unwrap();
    assert!(out.contains("PROJ-999"), "{out}");
}

/// Deleting the fence deletes the macro — the documented way to remove
/// something confed cannot otherwise edit.
#[test]
fn deleting_a_fence_deletes_the_macro() {
    let f = corpus::load().into_iter().find(|f| f.name == "23-unknown-macros").expect("fixture");
    let c = storage_to_markdown(&f.storage, &corpus::options()).unwrap();
    let victim = c.block_map.blocks.iter().position(|b| b.kind == BlockKind::Preserved).unwrap();
    let mut new_md = String::new();
    for (i, b) in c.block_map.blocks.iter().enumerate() {
        if i == victim {
            continue;
        }
        new_md.push_str(&slice_lines(&c.markdown, b.md_span.0, b.md_span.1));
    }
    let out = confed_convert::markdown_to_storage_patched(
        &f.storage,
        &c.block_map,
        &c.markdown,
        &new_md,
        &corpus::options(),
    )
    .unwrap();
    let raw = &f.storage
        [c.block_map.blocks[victim].storage_span.0..c.block_map.blocks[victim].storage_span.1];
    assert!(!out.contains(raw), "the deleted macro is still there");
}

#[test]
fn comment_fragments_render_without_options() {
    let out = confed_convert::storage_fragment_to_markdown(
        "<p>Looks good to me, <strong>ship it</strong>.</p>",
    )
    .unwrap();
    assert_eq!(out, "Looks good to me, **ship it**.\n");
}
