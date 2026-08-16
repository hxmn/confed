//! Shared loader for the fixture corpus.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use confed_convert::ConvertOptions;

pub struct Fixture {
    pub name: String,
    pub storage: String,
}

pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Every `.xml` fixture, in a stable order.
pub fn load() -> Vec<Fixture> {
    let mut out: Vec<Fixture> = std::fs::read_dir(dir())
        .expect("fixtures directory")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "xml"))
        .map(|e| Fixture {
            name: e.path().file_stem().unwrap().to_string_lossy().into_owned(),
            storage: std::fs::read_to_string(e.path()).expect("fixture is UTF-8"),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(out.len() >= 25, "corpus must cover the mapping table; found {}", out.len());
    out
}

/// The options the tests convert with: a sidecar directory, one known page link
/// and a site URL, so every link path in the mapping table is exercised.
pub fn options() -> ConvertOptions {
    let mut page_links = HashMap::new();
    page_links.insert("Runbook".to_string(), "runbook.md".to_string());
    let mut link_targets = HashMap::new();
    link_targets.insert("runbook.md".to_string(), "Runbook".to_string());
    ConvertOptions {
        attachment_dir: ".page".to_string(),
        page_links,
        link_targets,
        base_url: "https://wiki.example.test".to_string(),
        space_key: "TEAM".to_string(),
    }
}
