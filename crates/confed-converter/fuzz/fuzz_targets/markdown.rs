//! Anything a person could type into a page file. Converting it must not
//! panic, and storage the converter wrote must render again.

#![no_main]

use confed_converter::{markdown_to_storage, storage_to_markdown, ConvertOptions};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|markdown: &str| {
    let opts = ConvertOptions { attachment_dir: ".page".into(), ..Default::default() };
    let Ok(storage) = markdown_to_storage(markdown, &opts) else { return };
    storage_to_markdown(&storage, &opts).expect("storage confed wrote renders");
});
