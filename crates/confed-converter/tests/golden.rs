//! Golden snapshots for storage → Markdown over the whole corpus.
//!
//! Reviewing a diff here is how a change to the mapping table gets noticed.

mod corpus;

use confed_converter::storage_to_markdown;

#[test]
fn corpus_renders_to_markdown() {
    for fixture in corpus::load() {
        let converted = storage_to_markdown(&fixture.storage, &corpus::options())
            .unwrap_or_else(|e| panic!("{}: {e}", fixture.name));
        insta::with_settings!({
            snapshot_suffix => fixture.name.clone(),
            omit_expression => true,
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(converted.markdown);
        });
    }
}

#[test]
fn corpus_block_maps_are_snapshotted() {
    for fixture in corpus::load() {
        let converted = storage_to_markdown(&fixture.storage, &corpus::options())
            .unwrap_or_else(|e| panic!("{}: {e}", fixture.name));
        // Kind + line range, not byte offsets: the point is the *shape* of the
        // block decomposition, which is what a reviewer needs to eyeball.
        let summary: Vec<String> = converted
            .block_map
            .blocks
            .iter()
            .map(|b| {
                format!(
                    "{:?} md {}..{} storage {}..{}",
                    b.kind, b.md_span.0, b.md_span.1, b.storage_span.0, b.storage_span.1
                )
            })
            .collect();
        insta::with_settings!({
            snapshot_suffix => fixture.name.clone(),
            omit_expression => true,
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!(summary.join("\n"));
        });
    }
}
