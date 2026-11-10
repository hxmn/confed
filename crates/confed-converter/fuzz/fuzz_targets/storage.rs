//! Any storage body a server could send. Whatever the converter makes of it,
//! it must not panic, and what it accepts it must describe correctly: a block
//! map that fits the body, and an unedited patch that gives the body back.

#![no_main]

use confed_converter::{
    markdown_to_storage, markdown_to_storage_patched, storage_to_markdown, ConvertOptions,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|storage: &str| {
    let opts = ConvertOptions { attachment_dir: ".page".into(), ..Default::default() };
    let Ok(converted) = storage_to_markdown(storage, &opts) else { return };
    converted.block_map.validate_spans(storage).expect("the block map fits the body");
    converted.block_map.validate_md_spans().expect("the block map fits the Markdown");
    if let Ok(patched) = markdown_to_storage_patched(
        storage,
        &converted.block_map,
        &converted.markdown,
        &converted.markdown,
        &opts,
    ) {
        assert_eq!(patched, storage, "an unedited page patches to itself");
    }
    let _ = markdown_to_storage(&converted.markdown, &opts);
});
