//! How long `status` takes on a large workspace. Ignored by default: run with
//! `cargo test --release -p confed-core --test scale -- --ignored --nocapture`.
//! The goal (plan 09) is `status` on 5,000 pages in under a second.
//!
//! Measured on 0.23.0 (release build, an 8 KB body per page): about 195 ms, of
//! which 75 ms went on reading — and decompressing — every stored body. Read
//! without them, and with the per-page lookups hashed: 105–180 ms.

use confed_core::frontmatter::{Frontmatter, Managed, MarkdownFile};
use confed_core::state::{hash_str, now, PageRecord, RemotePage, SyncState};
use confed_core::workspace::Workspace;
use confed_core::worktree;
use std::time::Instant;

const PAGES: usize = 5_000;

#[test]
#[ignore = "a measurement, not a check: run it with --ignored --release"]
fn status_on_five_thousand_pages() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::create(dir.path()).unwrap();
    ws.state().set_meta("space_key", "DOCS").unwrap();
    // A page body of a realistic size: about 8 KB of storage.
    let body: String =
        (0..120).map(|i| format!("<p>Paragraph {i} of the page, a line of text.</p>")).collect();

    ws.state()
        .atomically(|state| {
            for i in 0..PAGES {
                let id = format!("{}", 100_000 + i);
                let path = format!("Section {}/Page {i}.md", i % 50);
                let file = MarkdownFile::new(
                    Frontmatter {
                        title: format!("Page {i}"),
                        labels: vec![],
                        parent_id: None,
                        managed: Some(Managed {
                            schema: confed_core::frontmatter::SCHEMA_VERSION,
                            page_id: id.clone(),
                            space_key: "DOCS".into(),
                            version: 3,
                            status: "current".into(),
                            position: None,
                            created: None,
                            updated: None,
                            author: None,
                            attachments: vec![],
                        }),
                        extra: Default::default(),
                    },
                    format!("Paragraph {i}.\n"),
                );
                let abs = dir.path().join(&path);
                std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
                std::fs::write(&abs, file.render().unwrap()).unwrap();
                state.upsert_page(&PageRecord {
                    page_id: id.clone(),
                    title: format!("Page {i}"),
                    slug: format!("Page {i}"),
                    local_path: path,
                    parent_id: None,
                    position: None,
                    version: 3,
                    status: "current".into(),
                    labels: vec![],
                    author: None,
                    created_at: None,
                    updated_at: None,
                    storage_body: body.clone(),
                    storage_hash: hash_str(&body),
                    markdown_hash: file.content_hash(),
                    block_map: Some("{\"blocks\":[]}".into()),
                    sync_state: SyncState::Clean,
                    synced_at: now(),
                    render_key: String::new(),
                })?;
                state.upsert_remote(&RemotePage {
                    page_id: id,
                    title: format!("Page {i}"),
                    parent_id: None,
                    position: None,
                    version: 3,
                    status: "current".into(),
                    labels: vec![],
                    author: None,
                    created_at: None,
                    updated_at: None,
                    storage_hash: Some(hash_str(&body)),
                    storage_body: Some(body.clone()),
                    fetched_at: now(),
                    deleted: false,
                })?;
            }
            Ok(())
        })
        .unwrap();

    let started = Instant::now();
    let (base, remote) = (ws.state().all_pages().unwrap(), ws.state().all_remote().unwrap());
    println!("reading the state ({} + {} rows): {:?}", base.len(), remote.len(), started.elapsed());
    let started = Instant::now();
    let (files, _) = worktree::read_working_files(&ws).unwrap();
    println!("reading {} files: {:?}", files.len(), started.elapsed());

    let started = Instant::now();
    let scan = worktree::scan(&ws).unwrap();
    let took = started.elapsed();
    assert_eq!(scan.pages.len(), PAGES);
    println!("status on {PAGES} pages: {took:?}");
}
