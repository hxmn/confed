//! Anything a person could type into a page file. Converting it must not
//! panic, and what it writes must behave as storage from the server does:
//! it renders, with a block map that fits, an unedited push sends it back
//! unchanged, and the Markdown it renders to converts again.

#![no_main]

use confed_converter::{
    markdown_to_storage, markdown_to_storage_patched, storage_to_markdown, ConvertOptions,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|markdown: &str| {
    let opts = ConvertOptions { attachment_dir: ".page".into(), ..Default::default() };
    let Ok(storage) = markdown_to_storage(markdown, &opts) else { return };
    let down = storage_to_markdown(&storage, &opts).expect("storage confed wrote renders");
    down.block_map.validate_spans(&storage).expect("the block map fits the body");
    down.block_map.validate_md_spans().expect("the block map fits the Markdown");
    let patched =
        markdown_to_storage_patched(&storage, &down.block_map, &down.markdown, &down.markdown, &opts)
            .expect("an unedited page patches");
    assert_eq!(patched, storage, "an unedited page patches to itself");
    markdown_to_storage(&down.markdown, &opts).expect("the Markdown it renders to converts");
});
