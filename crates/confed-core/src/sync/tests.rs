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
