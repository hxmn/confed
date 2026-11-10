use super::*;

#[test]
fn scope_patterns_select_paths_and_subtrees() {
    assert!(path_matches("Handbook/Onboarding.md", "Handbook/Onboarding.md"));
    assert!(path_matches("Handbook/**", "Handbook/Onboarding.md"));
    assert!(path_matches("Handbook/**", "Handbook/Deep/Nested.md"));
    assert!(path_matches("*.md", "Root.md"));
    assert!(!path_matches("Other/**", "Handbook/Onboarding.md"));
}

#[test]
fn atomic_writes_leave_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/Page.md");
    write_atomic(&path, "hello\n").unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("confed-tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn a_remote_deletion_with_local_edits_is_blocked_unless_forced() {
    let remote = RemotePage {
        page_id: "1".into(),
        title: "T".into(),
        parent_id: None,
        position: None,
        version: 3,
        status: "current".into(),
        labels: vec![],
        author: None,
        created_at: None,
        updated_at: None,
        storage_body: None,
        storage_hash: None,
        fetched_at: now(),
        deleted: true,
    };
    let status = PageStatus {
        page_id: Some("1".into()),
        path: "T.md".into(),
        title: "T".into(),
        state: PageState::RemoteDeleted,
        base_version: Some(3),
        remote_version: Some(3),
        local_dirty: true,
        remote_ahead: false,
        field_changes: Default::default(),
        moved_from: None,
        tampering: vec![],
        comment_drafts: 0,
    };

    let blocked = decide_pull(&remote, Some(&status), None, &PullOptions::default(), false);
    assert!(matches!(blocked, PullAction::Blocked(_)));

    let forced = decide_pull(
        &remote,
        Some(&status),
        None,
        &PullOptions { force: true, ..Default::default() },
        false,
    );
    assert_eq!(forced, PullAction::Delete);
}

#[test]
fn diverged_pages_merge_by_default_and_block_with_no_merge() {
    let remote = RemotePage {
        page_id: "1".into(),
        title: "T".into(),
        parent_id: None,
        position: None,
        version: 9,
        status: "current".into(),
        labels: vec![],
        author: None,
        created_at: None,
        updated_at: None,
        storage_body: Some("<p>x</p>".into()),
        storage_hash: None,
        fetched_at: now(),
        deleted: false,
    };
    let status = PageStatus {
        page_id: Some("1".into()),
        path: "T.md".into(),
        title: "T".into(),
        state: PageState::Diverged,
        base_version: Some(7),
        remote_version: Some(9),
        local_dirty: true,
        remote_ahead: true,
        field_changes: Default::default(),
        moved_from: None,
        tampering: vec![],
        comment_drafts: 0,
    };

    assert_eq!(
        decide_pull(&remote, Some(&status), None, &PullOptions::default(), false),
        PullAction::Merge
    );
    assert!(matches!(
        decide_pull(
            &remote,
            Some(&status),
            None,
            &PullOptions { no_merge: true, ..Default::default() },
            false
        ),
        PullAction::Blocked(_)
    ));
    assert_eq!(
        decide_pull(
            &remote,
            Some(&status),
            None,
            &PullOptions { force: true, ..Default::default() },
            false
        ),
        PullAction::Overwrite
    );
}

#[test]
fn a_locally_modified_page_is_left_alone_by_pull() {
    let remote = RemotePage {
        page_id: "1".into(),
        title: "T".into(),
        parent_id: None,
        position: None,
        version: 7,
        status: "current".into(),
        labels: vec![],
        author: None,
        created_at: None,
        updated_at: None,
        storage_body: Some("<p>x</p>".into()),
        storage_hash: None,
        fetched_at: now(),
        deleted: false,
    };
    let status = PageStatus {
        page_id: Some("1".into()),
        path: "T.md".into(),
        title: "T".into(),
        state: PageState::Modified,
        base_version: Some(7),
        remote_version: Some(7),
        local_dirty: true,
        remote_ahead: false,
        field_changes: Default::default(),
        moved_from: None,
        tampering: vec![],
        comment_drafts: 0,
    };
    assert_eq!(
        decide_pull(&remote, Some(&status), None, &PullOptions::default(), false),
        PullAction::Nothing
    );
}

/// The cached conversion context follows every change it is built from:
/// a page renamed, a page added, a setting changed — and only those.
#[test]
fn the_conversion_context_is_rebuilt_when_its_index_moves() {
    use crate::state::{hash_str, now, PageRecord, SyncState};
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::create(dir.path()).unwrap();
    let record = |id: &str, title: &str, path: &str| PageRecord {
        page_id: id.into(),
        title: title.into(),
        slug: title.into(),
        local_path: path.into(),
        parent_id: None,
        position: None,
        version: 1,
        status: "current".into(),
        labels: Vec::new(),
        author: None,
        created_at: None,
        updated_at: None,
        storage_body: "<p>x</p>".into(),
        storage_hash: hash_str("<p>x</p>"),
        markdown_hash: String::new(),
        block_map: None,
        sync_state: SyncState::Clean,
        synced_at: now(),
        render_key: String::new(),
    };
    ws.state().upsert_page(&record("1001", "Team Handbook", "Team Handbook.md")).unwrap();
    let opts = page_convert_options(&ws, "Team Handbook.md").unwrap();
    assert_eq!(opts.page_titles.get("1001").map(String::as_str), Some("Team Handbook"));

    // Nothing indexed changed: the same context, not rebuilt.
    let before = ws.state().index_generation();
    let mut bumped = record("1001", "Team Handbook", "Team Handbook.md");
    bumped.version = 2;
    ws.state().upsert_page(&bumped).unwrap();
    assert_eq!(ws.state().index_generation(), before);

    // A rename and a new page are seen.
    ws.state().upsert_page(&record("1001", "Onboarding", "Onboarding.md")).unwrap();
    ws.state().upsert_page(&record("1002", "Фраза", "Фраза.md")).unwrap();
    let opts = page_convert_options(&ws, "Onboarding.md").unwrap();
    assert_eq!(opts.page_titles.get("1001").map(String::as_str), Some("Onboarding"));
    assert_eq!(opts.page_links.get("1002").map(String::as_str), Some("Фраза.md"));

    ws.state().set_meta("space_key", "ENG").unwrap();
    assert_eq!(page_convert_options(&ws, "Onboarding.md").unwrap().space_key, "ENG");
}
