//! End-to-end sync scenarios against the stateful mock server.
//!
//! Each scenario runs against both Confluence flavors, because the whole point
//! of the client abstraction is that the sync engine cannot tell them apart.

use confed_api::{Flavor, MockClient, SpaceId};
use confed_core::state::SyncState;
use confed_core::sync::{PullOptions, PushOptions, SyncEngine};
use confed_core::workspace::Workspace;
use confed_core::worktree::{self, PageState};
use std::sync::Arc;

struct Harness {
    _dir: tempfile::TempDir,
    ws: Workspace,
    mock: Arc<MockClient>,
    engine: SyncEngine,
}

impl Harness {
    fn new(flavor: Flavor) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = Workspace::create(dir.path()).expect("workspace");
        ws.state().set_meta("space_key", "DOCS").unwrap();
        ws.state().set_meta("base_url", "https://mock.test").unwrap();
        ws.state().set_meta("flavor", flavor.as_str()).unwrap();

        let mock = Arc::new(MockClient::new(flavor));
        let engine = SyncEngine::new(
            mock.clone() as Arc<dyn confed_api::ConfluenceClient>,
            SpaceId { key: "DOCS".into(), numeric: Some("1001".into()) },
            2,
        );
        Self { _dir: dir, ws, mock, engine }
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.ws.root().join(relative)
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative))
            .unwrap_or_else(|e| panic!("reading {relative}: {e}"))
    }

    fn write(&self, relative: &str, content: &str) {
        std::fs::write(self.path(relative), content).expect("writing file");
    }

    /// Edit a page's body while leaving its frontmatter alone.
    fn edit_body(&self, relative: &str, addition: &str) {
        let mut content = self.read(relative);
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(addition);
        self.write(relative, &content);
    }

    fn comment_drafts(&self, page_id: &str) -> usize {
        worktree::scan(&self.ws).expect("scan").find(page_id).expect("status").comment_drafts
    }

    fn status(&self, page_id: &str) -> PageState {
        worktree::scan(&self.ws)
            .expect("scan")
            .find(page_id)
            .unwrap_or_else(|| panic!("no status for page {page_id}"))
            .state
    }

    async fn pull(&mut self) -> confed_core::sync::PullOutcome {
        self.engine.pull(&mut self.ws, &PullOptions::everything()).await.expect("pull")
    }

    async fn push(&mut self) -> confed_core::sync::PushOutcome {
        self.engine
            .push(&mut self.ws, &PushOptions { with_comments: true, ..Default::default() })
            .await
            .expect("push")
    }
}

/// Run a scenario against Cloud and Data Center.
macro_rules! both_flavors {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            for flavor in [Flavor::Cloud, Flavor::DataCenter] {
                let test: fn(Harness) -> _ = $body;
                test(Harness::new(flavor)).await;
            }
        }
    };
}

both_flavors!(pull_materializes_the_hierarchy, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Team Handbook", None, "<p>Welcome to the team.</p>");
    h.mock.seed_page("1002", "Onboarding", Some("1001"), "<p>First week checklist.</p>");
    h.mock.seed_page("1003", "Week One", Some("1002"), "<p>Day by day.</p>");

    let outcome = h.pull().await;
    assert_eq!(outcome.created.len(), 3, "every page becomes a file");

    // Children live in a directory named after their parent.
    assert!(h.path("Team Handbook.md").exists());
    assert!(h.path("Team Handbook/Onboarding.md").exists());
    assert!(h.path("Team Handbook/Onboarding/Week One.md").exists());

    let file = h.read("Team Handbook/Onboarding.md");
    assert!(file.contains("title: Onboarding"));
    assert!(file.contains("page_id: '1002'") || file.contains("page_id: \"1002\""));
    assert!(file.contains("First week checklist"));

    // A second pull with no server changes is a no-op.
    let again = h.pull().await;
    assert!(again.is_empty(), "re-pulling should change nothing: {again:?}");
    assert_eq!(h.status("1002"), PageState::Unchanged);
});

both_flavors!(a_local_edit_pushes_and_advances_the_version, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Runbook", None, "<p>Original text.</p>");
    h.pull().await;
    assert_eq!(h.status("1001"), PageState::Unchanged);

    h.edit_body("Runbook.md", "\nA new paragraph.\n");
    assert_eq!(h.status("1001"), PageState::Modified);

    let outcome = h.push().await;
    assert_eq!(outcome.pushed.len(), 1);
    assert_eq!(outcome.pushed[0].to_version, Some(2));

    let body = h.mock.page_body("1001").expect("page still exists");
    assert!(body.contains("A new paragraph"), "the edit reached the server: {body}");
    assert!(body.contains("Original text"), "untouched content survives: {body}");

    // The file's managed frontmatter now records the new base version.
    assert!(h.read("Runbook.md").contains("version: 2"));
    assert_eq!(h.status("1001"), PageState::Unchanged, "push leaves the page clean");
});

both_flavors!(untouched_blocks_are_byte_identical_after_a_push, |mut h: Harness| async move {
    // The macro is something confed cannot model, so it must survive verbatim.
    let macro_block = "<ac:structured-macro ac:name=\"jira\" ac:macro-id=\"abc-123\">\
                       <ac:parameter ac:name=\"key\">PROJ-142</ac:parameter>\
                       </ac:structured-macro>";
    let original = format!("<p>Intro paragraph.</p>{macro_block}<p>Closing paragraph.</p>");
    h.mock.seed_page("1001", "Page", None, &original);
    h.pull().await;

    let file = h.read("Page.md");
    assert!(file.contains("```confluence"), "unknown macros are preserved in a fence");

    // Edit only the first paragraph.
    let edited = file.replace("Intro paragraph.", "Intro paragraph, revised.");
    assert_ne!(edited, file, "the test edit must actually apply");
    h.write("Page.md", &edited);
    h.push().await;

    let body = h.mock.page_body("1001").unwrap();
    assert!(body.contains(macro_block), "the untouched macro is re-emitted byte for byte: {body}");
    assert!(body.contains("Intro paragraph, revised."));
    assert!(body.contains("<p>Closing paragraph.</p>"), "the untouched paragraph is unchanged");
});

both_flavors!(a_remote_edit_leaves_the_page_behind_until_pulled, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Notes", None, "<p>One.</p>");
    h.pull().await;

    h.mock.remote_edit("1001", "<p>One.</p><p>Two, added on the server.</p>");
    h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    assert_eq!(h.status("1001"), PageState::Behind);

    h.pull().await;
    assert!(h.read("Notes.md").contains("Two, added on the server"));
    assert_eq!(h.status("1001"), PageState::Unchanged);
});

both_flavors!(edits_on_both_sides_merge_when_they_do_not_overlap, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Doc", None, "<p>First.</p><p>Second.</p><p>Third.</p>");
    h.pull().await;

    // We change the last paragraph; the server changes the first.
    let file = h.read("Doc.md").replace("Third.", "Third, edited locally.");
    h.write("Doc.md", &file);
    h.mock.remote_edit("1001", "<p>First, edited on the server.</p><p>Second.</p><p>Third.</p>");

    h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    assert_eq!(h.status("1001"), PageState::Diverged);

    let outcome = h.pull().await;
    assert_eq!(outcome.merged.len(), 1, "a non-overlapping divergence merges cleanly");
    assert!(outcome.conflicted.is_empty());

    let merged = h.read("Doc.md");
    assert!(merged.contains("Third, edited locally."), "our edit survives");
    assert!(merged.contains("First, edited on the server."), "their edit is taken");
    assert!(!merged.contains("<<<<<<<"), "no conflict markers: {merged}");
});

both_flavors!(overlapping_edits_conflict_and_block_push, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Doc", None, "<p>Shared sentence.</p>");
    h.pull().await;

    let file = h.read("Doc.md").replace("Shared sentence.", "Our version of the sentence.");
    h.write("Doc.md", &file);
    h.mock.remote_edit("1001", "<p>Their version of the sentence.</p>");

    h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    let outcome = h.pull().await;
    assert_eq!(outcome.conflicted.len(), 1, "an overlapping edit conflicts");

    let conflicted = h.read("Doc.md");
    assert!(conflicted.contains("<<<<<<< local"), "{conflicted}");
    assert!(conflicted.contains("||||||| base"));
    assert!(conflicted.contains(">>>>>>> remote"));
    assert_eq!(h.status("1001"), PageState::Conflicted);

    // push must refuse a conflicted page rather than upload markers.
    let before = h.mock.page_body("1001").unwrap();
    let push = h.push().await;
    assert!(push.pushed.is_empty(), "nothing is uploaded while conflicted");
    assert_eq!(push.skipped.len(), 1);
    assert!(push.skipped[0].reason.contains("conflict"));
    assert_eq!(h.mock.page_body("1001").unwrap(), before, "the server is untouched");
});

both_flavors!(a_stale_base_is_refused_until_the_page_is_pulled, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Doc", None, "<p>Text.</p>");
    h.pull().await;

    // Edit locally, then let the server move ahead without pulling.
    h.edit_body("Doc.md", "\nLocal addition.\n");
    h.mock.remote_edit("1001", "<p>Text changed elsewhere.</p>");
    h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");

    let push = h.push().await;
    assert!(push.pushed.is_empty(), "a stale base must not overwrite newer content");
    assert_eq!(push.skipped.len(), 1);
    assert!(
        push.skipped[0].reason.contains("version"),
        "the reason names the version gap: {}",
        push.skipped[0].reason
    );
    assert!(h.mock.page_body("1001").unwrap().contains("changed elsewhere"));
});

both_flavors!(a_new_local_file_is_created_on_the_server, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Parent", None, "<p>Parent page.</p>");
    h.pull().await;

    // A child of the pulled page: parent comes from the directory.
    std::fs::create_dir_all(h.path("Parent")).unwrap();
    h.write(
        "Parent/Child.md",
        "---\ntitle: Child\nlabels: []\n---\n\n# Child\n\nBrand new content.\n",
    );
    assert_eq!(
        worktree::scan(&h.ws).unwrap().find_path("Parent/Child.md").unwrap().state,
        PageState::LocalNew
    );

    let outcome = h.push().await;
    assert_eq!(outcome.created.len(), 1);
    let page_id = &outcome.created[0].page_id;
    assert!(h.mock.page_exists(page_id));
    assert!(h.mock.page_body(page_id).unwrap().contains("Brand new content"));

    // The file gains its managed block, so it is no longer "new".
    let written = h.read("Parent/Child.md");
    assert!(written.contains(&format!("page_id: '{page_id}'")) || written.contains(page_id));
    assert_eq!(h.status(page_id), PageState::Unchanged);
});

both_flavors!(a_title_change_renames_the_page, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Old Title", None, "<p>Body.</p>");
    h.pull().await;

    let file = h.read("Old Title.md").replace("title: Old Title", "title: New Title");
    h.write("Old Title.md", &file);

    let outcome = h.push().await;
    assert_eq!(outcome.pushed.len(), 1);
    assert!(outcome.pushed[0].ops.iter().any(|o| o == "title"));
    assert_eq!(h.mock.page_title("1001").as_deref(), Some("New Title"));
});

both_flavors!(deletions_need_an_explicit_opt_in, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Doomed", None, "<p>Body.</p>");
    h.pull().await;
    std::fs::remove_file(h.path("Doomed.md")).unwrap();
    assert_eq!(h.status("1001"), PageState::LocalDeleted);

    // Without --allow-delete the page is reported, not deleted: an accidental
    // `rm -rf` must not take a Confluence subtree with it.
    let guarded = h.push().await;
    assert!(guarded.deleted.is_empty());
    assert_eq!(guarded.skipped.len(), 1);
    assert!(guarded.skipped[0].reason.contains("--allow-delete"));
    assert!(h.mock.page_exists("1001"), "the page survives a plain push");

    let outcome = h
        .engine
        .push(&mut h.ws, &PushOptions { allow_delete: true, ..Default::default() })
        .await
        .expect("push");
    assert_eq!(outcome.deleted.len(), 1);
    assert!(!h.mock.page_exists("1001"));
});

both_flavors!(dry_run_uploads_nothing, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Doc", None, "<p>Text.</p>");
    h.pull().await;
    h.edit_body("Doc.md", "\nAn edit that should stay local.\n");

    let before = h.mock.page_body("1001").unwrap();
    let outcome = h
        .engine
        .push(&mut h.ws, &PushOptions { dry_run: true, ..Default::default() })
        .await
        .expect("push");

    assert!(outcome.dry_run);
    assert_eq!(outcome.pushed.len(), 1, "the plan still reports what would happen");
    assert_eq!(h.mock.page_body("1001").unwrap(), before);
    assert!(
        !h.mock.mutating_calls().iter().any(|c| c.starts_with("update_page")),
        "a dry run must make no mutating calls: {:?}",
        h.mock.mutating_calls()
    );
    assert_eq!(h.status("1001"), PageState::Modified, "the page is still pending");
});

both_flavors!(a_page_deleted_on_the_server_is_removed_locally, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Keeper", None, "<p>Stays.</p>");
    h.mock.seed_page("1002", "Goner", None, "<p>Goes away.</p>");
    h.pull().await;
    assert!(h.path("Goner.md").exists());

    h.mock.delete_page_directly("1002");
    let outcome = h.pull().await;

    assert_eq!(outcome.deleted.len(), 1);
    assert!(!h.path("Goner.md").exists(), "the file follows the server");
    assert!(h.path("Keeper.md").exists());
});

both_flavors!(pull_refuses_to_clobber_a_locally_edited_deleted_page, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Contested", None, "<p>Body.</p>");
    h.pull().await;
    h.edit_body("Contested.md", "\nWork I have not pushed.\n");
    h.mock.delete_page_directly("1001");

    let err = h
        .engine
        .pull(&mut h.ws, &PullOptions::everything())
        .await
        .expect_err("pull must stop rather than discard local work");
    assert_eq!(err.exit_code(), confed_core::ExitCode::State);
    assert!(h.path("Contested.md").exists(), "nothing is deleted while we refuse");
    assert!(h.read("Contested.md").contains("Work I have not pushed"));

    // --force is the explicit way through.
    let forced = h
        .engine
        .pull(&mut h.ws, &PullOptions { force: true, ..PullOptions::everything() })
        .await
        .expect("forced pull");
    assert_eq!(forced.deleted.len(), 1);
    assert!(!h.path("Contested.md").exists());
});

both_flavors!(labels_sync_in_both_directions, |mut h: Harness| async move {
    let id = h.mock.seed_page("1001", "Labelled", None, "<p>Body.</p>");
    h.engine.client().add_label(&id, "existing").await.unwrap();
    h.pull().await;
    assert!(h.read("Labelled.md").contains("existing"));

    let file = h.read("Labelled.md").replace("- existing", "- existing\n- added");
    h.write("Labelled.md", &file);

    let outcome = h.push().await;
    assert!(outcome.pushed[0].ops.iter().any(|o| o == "labels"));
    let labels = h.engine.client().get_labels(&id).await.unwrap();
    assert!(labels.contains(&"added".to_string()), "got {labels:?}");
    assert!(labels.contains(&"existing".to_string()));
});

both_flavors!(comments_are_written_to_the_sidecar, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Discussed", None, "<p>Body.</p>");
    h.mock.seed_comment(
        "1001",
        "<p>Should this mention the VPN?</p>",
        confed_api::CommentKind::Footer,
    );
    h.pull().await;

    let sidecar = h.read(".Discussed/comments.md");
    assert!(sidecar.contains("# Comments — Discussed"));
    assert!(sidecar.contains("Should this mention the VPN?"));
    assert!(sidecar.contains("confed:comment"));
});

both_flavors!(a_comment_draft_is_posted_on_push, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Discussed", None, "<p>Body.</p>");
    h.mock.seed_comment("1001", "<p>Existing thread.</p>", confed_api::CommentKind::Footer);
    h.pull().await;

    let mut sidecar = h.read(".Discussed/comments.md");
    sidecar.push_str("\n<!-- confed:new -->\nReviewed for Q3.\n");
    h.write(".Discussed/comments.md", &sidecar);

    let outcome = h.push().await;
    assert_eq!(outcome.comments_added.len(), 1, "the draft is posted");

    let posted = h.engine.client().list_comments(&confed_api::PageId::new("1001")).await.unwrap();
    assert!(posted.iter().any(|c| c.body_storage.contains("Reviewed for Q3")));
});

both_flavors!(an_interrupted_fetch_resumes_where_it_stopped, |mut h: Harness| async move {
    for i in 0..5 {
        h.mock.seed_page(&format!("100{i}"), &format!("Page {i}"), None, "<p>Body.</p>");
    }
    // Simulate a fetch that listed everything but only stored some bodies.
    h.ws.state().enqueue_fetch("1000", &["body"]).unwrap();
    h.ws.state().enqueue_fetch("1001", &["body"]).unwrap();
    h.ws.state().mark_fetch_done("1000").unwrap();

    let outcome = h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    assert!(outcome.resumed, "an outstanding queue means this run is a resume");
    assert_eq!(outcome.failed.len(), 0);
    assert_eq!(h.ws.state().pending_fetches().unwrap().len(), 0, "the queue drains");
});

/// Conflicts survive a restart, so a resolution is never silently lost.
#[tokio::test]
async fn a_conflicted_page_stays_conflicted_across_reopen() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Doc", None, "<p>Shared.</p>");
    h.pull().await;

    let file = h.read("Doc.md").replace("Shared.", "Ours.");
    h.write("Doc.md", &file);
    h.mock.remote_edit("1001", "<p>Theirs.</p>");
    h.engine.fetch(&mut h.ws, &Default::default()).await.unwrap();
    h.pull().await;

    // Open a second handle to the same directory, as a fresh process would.
    let root = h.ws.root().to_path_buf();
    let reopened = Workspace::open(&root).expect("reopen");
    let record = reopened.state().get_page("1001").unwrap().unwrap();
    assert_eq!(record.sync_state, SyncState::Conflicted);
}

/// An inline comment's anchor is re-located after the page text moves, and
/// flagged rather than mis-attached when the text is gone.
#[tokio::test]
async fn inline_anchors_are_refreshed_when_the_body_changes() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page(
        "1001",
        "Onboarding",
        None,
        "<p>Read this during your first week checklist and then ask questions.</p>",
    );
    // The mock seeds an inline anchor on "first week checklist".
    h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    // This scenario is about the text-search fallback, which is what the body
    // relies on when marks are switched off.
    h.ws.state().set_meta(confed_core::sync::MARKS_MODE_KEY, "off").unwrap();
    h.pull().await;

    let stored_anchor = |h: &Harness| -> confed_api::InlineAnchor {
        let record =
            h.ws.state()
                .page_comments("1001")
                .unwrap()
                .into_iter()
                .find(|c| c.kind == "inline")
                .expect("inline comment");
        serde_json::from_str(record.anchor.as_deref().expect("anchor")).expect("anchor json")
    };
    assert!(!stored_anchor(&h).orphaned, "the anchor starts out attached");

    // Move the anchored text into a different paragraph: still findable.
    let moved = h.read("Onboarding.md").replace(
        "Read this during your first week checklist and then ask questions.",
        "A new opening line.\n\nLater: first week checklist.",
    );
    h.write("Onboarding.md", &moved);
    h.engine
        .pull(&mut h.ws, &PullOptions { no_fetch: true, ..PullOptions::everything() })
        .await
        .expect("pull");
    assert!(!stored_anchor(&h).orphaned, "moved text is re-anchored, not orphaned");

    // Delete the anchored text entirely: flagged, never mis-attached.
    h.write(
        "Onboarding.md",
        &h.read("Onboarding.md").replace("Later: first week checklist.", "Nothing relevant here."),
    );
    h.engine
        .pull(&mut h.ws, &PullOptions { no_fetch: true, ..PullOptions::everything() })
        .await
        .expect("pull");

    let anchor = stored_anchor(&h);
    assert!(anchor.orphaned, "a vanished anchor is flagged");
    assert_eq!(anchor.text, "first week checklist", "the original text is kept for a human");
}

/// Attachments placed in a page's sidecar are uploaded, and re-uploaded when
/// their bytes change.
#[tokio::test]
async fn attachments_are_uploaded_from_the_sidecar() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>See the diagram.</p>");
    h.pull().await;

    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/diagram.png"), b"first version").unwrap();

    let opts = PushOptions { with_attachments: true, ..Default::default() };
    let outcome = h.engine.push(&mut h.ws, &opts).await.expect("push");
    assert_eq!(outcome.attachments_uploaded.len(), 1, "the new file is uploaded");

    let remote =
        h.engine.client().list_attachments(&confed_api::PageId::new("1001")).await.unwrap();
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].filename, "diagram.png");
    let first_version = remote[0].version;

    // Pushing again with no change must not re-upload.
    let again = h.engine.push(&mut h.ws, &opts).await.expect("push");
    assert!(again.attachments_uploaded.is_empty(), "unchanged bytes are left alone");

    // Changed bytes become a new version of the same attachment.
    std::fs::write(h.path(".Diagrams/diagram.png"), b"second version, longer").unwrap();
    let changed = h.engine.push(&mut h.ws, &opts).await.expect("push");
    assert_eq!(changed.attachments_uploaded.len(), 1);

    let remote =
        h.engine.client().list_attachments(&confed_api::PageId::new("1001")).await.unwrap();
    assert_eq!(remote.len(), 1, "still one attachment, not a duplicate");
    assert!(remote[0].version > first_version, "it gained a version");
}

/// Removing an attachment locally needs the same explicit opt-in as deleting a
/// page: it destroys content on the server.
#[tokio::test]
async fn deleting_an_attachment_requires_allow_delete() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>See the diagram.</p>");
    h.pull().await;

    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/diagram.png"), b"content").unwrap();
    h.engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");

    std::fs::remove_file(h.path(".Diagrams/diagram.png")).unwrap();

    h.engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");
    let still_there =
        h.engine.client().list_attachments(&confed_api::PageId::new("1001")).await.unwrap();
    assert_eq!(still_there.len(), 1, "a plain push must not delete server content");

    h.engine
        .push(
            &mut h.ws,
            &PushOptions { with_attachments: true, allow_delete: true, ..Default::default() },
        )
        .await
        .expect("push");
    let gone = h.engine.client().list_attachments(&confed_api::PageId::new("1001")).await.unwrap();
    assert!(gone.is_empty(), "with --allow-delete it is removed");
}

/// A pull that dies mid-download leaves a `*.confed-part` scratch file in the
/// sidecar. It is confed's own, not page content: push must not offer it, and a
/// dry run must not hide that it would.
#[tokio::test]
async fn a_partial_download_is_never_a_push_candidate() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>See the video.</p>");
    h.pull().await;

    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/video.webm"), b"the whole thing").unwrap();
    // Both namings: the hidden one confed writes now, and the bare one older
    // versions left behind and existing workspaces still hold.
    std::fs::write(h.path(".Diagrams/.video.webm.confed-part"), b"the whole").unwrap();
    std::fs::write(h.path(".Diagrams/video.webm.confed-part"), b"the whole").unwrap();

    let opts = PushOptions { with_attachments: true, ..Default::default() };

    let plan = h.engine.plan_push(&h.ws, &opts).expect("plan");
    let planned: Vec<String> = plan.attachment_ops.iter().map(|op| op.file.clone()).collect();
    assert_eq!(
        planned,
        vec![".Diagrams/video.webm".to_string()],
        "--dry-run lists the real attachment and nothing else: {planned:?}"
    );

    let outcome = h.engine.push(&mut h.ws, &opts).await.expect("push");
    assert_eq!(outcome.attachments_uploaded, vec![".Diagrams/video.webm".to_string()]);

    let remote =
        h.engine.client().list_attachments(&confed_api::PageId::new("1001")).await.unwrap();
    let names: Vec<&str> = remote.iter().map(|a| a.filename.as_str()).collect();
    assert_eq!(names, vec!["video.webm"], "no scratch file reached Confluence");
}

/// The partials themselves are swept on the next pull of the page, so an
/// interrupted download does not linger in a directory of user content.
#[tokio::test]
async fn pull_sweeps_partial_downloads_left_behind() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>See the diagram.</p>");
    h.pull().await;

    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/diagram.png"), b"content").unwrap();
    h.engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");

    std::fs::write(h.path(".Diagrams/.diagram.png.confed-part"), b"conte").unwrap();
    std::fs::write(h.path(".Diagrams/video.webm.confed-part"), b"half a video").unwrap();

    h.mock.remote_edit("1001", "<p>See the diagram, again.</p>");
    h.pull().await;

    assert!(!h.path(".Diagrams/.diagram.png.confed-part").exists());
    assert!(!h.path(".Diagrams/video.webm.confed-part").exists());
    assert!(h.path(".Diagrams/diagram.png").exists(), "the real attachment is untouched");
}

/// A dry run that says nothing about attachments reads as a promise that
/// nothing else will happen. It has to name the same work the push does.
#[tokio::test]
async fn a_dry_run_reports_the_attachment_work_it_would_do() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>See the diagram.</p>");
    h.pull().await;

    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/diagram.png"), b"content").unwrap();

    let dry = h
        .engine
        .push(
            &mut h.ws,
            &PushOptions { with_attachments: true, dry_run: true, ..Default::default() },
        )
        .await
        .expect("dry run");
    assert_eq!(dry.attachments_uploaded, vec![".Diagrams/diagram.png".to_string()]);
    assert!(
        h.engine
            .client()
            .list_attachments(&confed_api::PageId::new("1001"))
            .await
            .unwrap()
            .is_empty(),
        "a dry run uploads nothing"
    );

    let real = h
        .engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");
    assert_eq!(real.attachments_uploaded, dry.attachments_uploaded, "the plan was kept");
}

/// Skipping a deletion for want of `--allow-delete` is a decision, not a
/// non-event: push says so, rather than exiting clean as if nothing was asked.
#[tokio::test]
async fn a_refused_attachment_deletion_is_reported_not_dropped() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>See the diagram.</p>");
    h.pull().await;

    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/diagram.png"), b"content").unwrap();
    h.engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");

    std::fs::remove_file(h.path(".Diagrams/diagram.png")).unwrap();

    let refused = h
        .engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");
    assert!(refused.attachments_deleted.is_empty(), "nothing was deleted");
    let blocked = refused
        .skipped
        .iter()
        .find(|s| s.path == ".Diagrams/diagram.png")
        .unwrap_or_else(|| panic!("the refusal is reported: {refused:?}"));
    assert!(blocked.reason.contains("--allow-delete"), "and it says what to do: {blocked:?}");

    // The narrower opt-in deletes the attachment without licensing page deletes.
    let deleted = h
        .engine
        .push(
            &mut h.ws,
            &PushOptions {
                with_attachments: true,
                allow_attachment_delete: true,
                ..Default::default()
            },
        )
        .await
        .expect("push");
    assert_eq!(deleted.attachments_deleted, vec![".Diagrams/diagram.png".to_string()]);
    assert!(deleted.skipped.iter().all(|s| s.path != ".Diagrams/diagram.png"));
    assert!(h
        .engine
        .client()
        .list_attachments(&confed_api::PageId::new("1001"))
        .await
        .unwrap()
        .is_empty());
}

/// Two pages swapping titles makes each want the path the other still holds.
#[tokio::test]
async fn pages_that_swap_titles_still_pull() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Alpha", None, "<p>First page.</p>");
    h.mock.seed_page("1002", "Beta", None, "<p>Second page.</p>");
    h.pull().await;
    assert!(h.path("Alpha.md").exists() && h.path("Beta.md").exists());

    h.mock.rename_page("1001", "Beta");
    h.mock.rename_page("1002", "Alpha");

    let outcome = h.pull().await;
    assert!(outcome.skipped_dirty.is_empty(), "nothing is blocked: {outcome:?}");
    assert_eq!(outcome.moved.len(), 2, "both files follow their page's new title");

    // Whatever names they end up with, both pages exist exactly once and the
    // bodies followed their page ids rather than their filenames.
    let alpha = h.ws.state().get_page("1001").unwrap().unwrap();
    let beta = h.ws.state().get_page("1002").unwrap().unwrap();
    assert_ne!(alpha.local_path, beta.local_path, "two pages cannot share a file");
    assert!(h.path(&alpha.local_path).exists(), "{} is missing", alpha.local_path);
    assert!(h.path(&beta.local_path).exists(), "{} is missing", beta.local_path);
    assert!(h.read(&alpha.local_path).contains("First page."));
    assert!(h.read(&beta.local_path).contains("Second page."));
}

/// A server-side rename moves the file, unless the user renamed it locally
/// first — in which case their choice of filename wins and only the title
/// changes.
#[tokio::test]
async fn a_locally_renamed_file_keeps_its_name_through_a_server_rename() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Original", None, "<p>Body.</p>");
    h.pull().await;

    // The user renames the file (not the title) — a local naming preference.
    std::fs::rename(h.path("Original.md"), h.path("My Preferred Name.md")).unwrap();
    h.engine
        .pull(&mut h.ws, &PullOptions { no_fetch: true, ..PullOptions::everything() })
        .await
        .expect("pull");
    // confed re-associates by page_id, so the record now points at the new file.
    let record = h.ws.state().get_page("1001").unwrap().unwrap();
    assert_eq!(record.local_path, "My Preferred Name.md");

    h.mock.rename_page("1001", "Renamed On Server");
    h.pull().await;

    assert!(h.path("My Preferred Name.md").exists(), "the chosen filename survives");
    assert!(!h.path("Renamed On Server.md").exists());
    assert!(
        h.read("My Preferred Name.md").contains("title: Renamed On Server"),
        "but the title follows the server"
    );
}

/// A page renamed on the server takes its attachments and comments with it.
#[tokio::test]
async fn a_renamed_page_brings_its_sidecar_along() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Before", None, "<p>Body.</p>");
    h.mock.seed_comment("1001", "<p>A comment.</p>", confed_api::CommentKind::Footer);
    h.pull().await;
    assert!(h.path(".Before/comments.md").exists());

    h.mock.rename_page("1001", "After");
    h.pull().await;

    assert!(h.path("After.md").exists());
    assert!(!h.path("Before.md").exists(), "the old file is cleaned up");
    assert!(h.path(".After/comments.md").exists(), "the sidecar followed the page");
    assert!(!h.path(".Before").exists(), "no orphaned sidecar is left behind");
}

/// A file naming a page confed has no record of — the shape of a fresh git
/// clone, since `.state.db` is git-ignored — must not be silently overwritten.
#[tokio::test]
async fn pull_refuses_to_overwrite_a_page_it_has_no_record_of() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Notes", None, "<p>Server text.</p>");
    h.pull().await;

    // Simulate a rebuilt state database: the file and its page_id survive, the
    // base record does not.
    h.edit_body("Notes.md", "\nUnpushed local work.\n");
    h.ws.state().delete_page("1001").unwrap();
    assert_eq!(h.status("1001"), PageState::Untracked);

    let err = h
        .engine
        .pull(&mut h.ws, &PullOptions::everything())
        .await
        .expect_err("pull must stop rather than overwrite work it cannot compare");
    assert_eq!(err.exit_code(), confed_core::ExitCode::State);
    assert!(
        h.read("Notes.md").contains("Unpushed local work."),
        "the local edit survives the refusal"
    );

    // --force is the way through, and it takes the server's copy.
    h.engine
        .pull(&mut h.ws, &PullOptions { force: true, ..PullOptions::everything() })
        .await
        .expect("forced pull");
    assert!(!h.read("Notes.md").contains("Unpushed local work."));
    assert!(h.read("Notes.md").contains("Server text."));
}

/// The engine reports what it is doing, so a long pull is not a blank screen.
#[tokio::test]
async fn pull_reports_progress() {
    let recorder = std::sync::Arc::new(confed_core::progress::RecordingProgress::new());
    let mut h = Harness::new(Flavor::Cloud);
    h.engine = h
        .engine
        .with_progress(std::sync::Arc::clone(&recorder) as confed_core::progress::ProgressRef);

    h.mock.seed_page("1001", "Alpha", None, "<p>One.</p>");
    h.mock.seed_page("1002", "Beta", None, "<p>Two.</p>");
    h.mock.seed_page("1003", "Gamma", Some("1001"), "<p>Three.</p>");
    h.pull().await;

    let stages = recorder.stage_names();
    assert!(stages.contains(&"Listing pages".to_string()), "got {stages:?}");
    assert!(stages.contains(&"Fetching".to_string()), "got {stages:?}");
    assert!(stages.contains(&"Writing".to_string()), "got {stages:?}");

    // The fetch stage knows its total up front; the listing stage cannot.
    let fetching = recorder.stages().into_iter().find(|(name, _)| name == "Fetching").unwrap();
    assert_eq!(fetching.1, Some(3), "the page count is known before fetching bodies");

    // Every page is reported once by the fetch and once by the write pass.
    let items = recorder.items();
    assert!(items.iter().any(|i| i == "Alpha"), "fetch reports titles: {items:?}");
    assert!(items.iter().any(|i| i == "Alpha.md"), "writes report paths: {items:?}");
    assert!(recorder.finish_count() >= 1, "the display is cleared when done");
}

/// A dry run says so rather than claiming to write.
#[tokio::test]
async fn a_dry_run_reports_checking_rather_than_writing() {
    let recorder = std::sync::Arc::new(confed_core::progress::RecordingProgress::new());
    let mut h = Harness::new(Flavor::Cloud);
    h.engine = h
        .engine
        .with_progress(std::sync::Arc::clone(&recorder) as confed_core::progress::ProgressRef);

    h.mock.seed_page("1001", "Alpha", None, "<p>One.</p>");
    h.engine
        .pull(&mut h.ws, &PullOptions { dry_run: true, ..PullOptions::everything() })
        .await
        .expect("pull");

    let stages = recorder.stage_names();
    assert!(stages.contains(&"Checking".to_string()), "got {stages:?}");
    assert!(!stages.contains(&"Writing".to_string()), "nothing was written");
}

/// Every page keeps a copy of its Confluence markup beside its Markdown, so the
/// source of a conversion is always inspectable without a round trip. Confluence
/// ships a body as one line, so the copy is laid out to be read — and that is
/// the only thing that changes about it.
#[tokio::test]
async fn pull_saves_the_confluence_markup_beside_each_page() {
    let mut h = Harness::new(Flavor::Cloud);
    let body = "<p>Intro.</p><ac:structured-macro ac:name=\"jira\"><ac:parameter \
                ac:name=\"key\">PROJ-1</ac:parameter></ac:structured-macro>";
    h.mock.seed_page("1001", "Onboarding", None, body);
    h.mock.seed_page("1002", "Nested", Some("1001"), "<p>Child.</p>");
    h.pull().await;

    let on_disk = h.read(".Onboarding/storage.xml");
    assert_eq!(
        on_disk,
        concat!(
            "<p>Intro.</p>\n",
            "<ac:structured-macro ac:name=\"jira\">\n",
            "  <ac:parameter ac:name=\"key\">PROJ-1</ac:parameter>\n",
            "</ac:structured-macro>\n"
        )
    );
    assert_eq!(
        confed_convert::pretty::minify(&on_disk),
        confed_convert::pretty::minify(body),
        "the copy is the server's markup, not a re-serialization of it"
    );
    assert_eq!(h.read("Onboarding/.Nested/storage.xml"), "<p>Child.</p>\n");
}

/// The copy tracks the page: it is refreshed by pull, refreshed again by push,
/// and never uploaded back as an attachment.
#[tokio::test]
async fn the_markup_copy_follows_the_page_and_is_not_an_attachment() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Notes", None, "<p>First.</p>");
    h.pull().await;
    assert_eq!(h.read(".Notes/storage.xml"), "<p>First.</p>\n");

    // A remote edit refreshes it.
    h.mock.remote_edit("1001", "<p>Second, from the server.</p>");
    h.pull().await;
    assert_eq!(h.read(".Notes/storage.xml"), "<p>Second, from the server.</p>\n");

    // So does a local edit that gets pushed.
    h.edit_body("Notes.md", "\nA local addition.\n");
    let outcome = h
        .engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");
    assert_eq!(outcome.pushed.len(), 1);
    assert!(outcome.attachments_uploaded.is_empty(), "the copy is not an attachment");

    let on_disk = h.read(".Notes/storage.xml");
    assert_eq!(
        confed_convert::pretty::minify(&on_disk),
        confed_convert::pretty::minify(&h.mock.page_body("1001").unwrap()),
        "it matches what the server now has"
    );
    assert!(on_disk.contains("A local addition"));

    let remote_attachments =
        h.engine.client().list_attachments(&confed_api::PageId::new("1001")).await.unwrap();
    assert!(remote_attachments.is_empty(), "nothing was uploaded to Confluence");
}

/// Deleting a page takes its markup copy with it.
#[tokio::test]
async fn the_markup_copy_is_removed_with_its_page() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Doomed", None, "<p>Body.</p>");
    h.pull().await;
    assert!(h.path(".Doomed/storage.xml").exists());

    h.mock.delete_page_directly("1001");
    h.pull().await;
    assert!(!h.path(".Doomed").exists(), "no orphaned sidecar is left behind");
}

/// The markup copy appears for pages that are already up to date, so a
/// workspace pulled by an older confed is backfilled by the next pull rather
/// than only as each page happens to change.
#[tokio::test]
async fn pull_backfills_the_markup_copy_for_unchanged_pages() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Alpha", None, "<p>One.</p>");
    h.mock.seed_page("1002", "Beta", None, "<p>Two.</p>");
    h.pull().await;

    // Stand in for a workspace synced before confed kept the copy.
    std::fs::remove_file(h.path(".Alpha/storage.xml")).unwrap();
    std::fs::remove_file(h.path(".Beta/storage.xml")).unwrap();

    // Nothing has changed on either side, so pull writes no page files at all…
    let outcome = h.pull().await;
    assert!(outcome.is_empty(), "no page needed rewriting: {outcome:?}");

    // …and the copies come back anyway.
    assert_eq!(h.read(".Alpha/storage.xml"), "<p>One.</p>\n");
    assert_eq!(h.read(".Beta/storage.xml"), "<p>Two.</p>\n");
}

/// A stale or hand-edited copy is corrected, and an already-correct one is left
/// alone rather than rewritten on every pull.
#[tokio::test]
async fn the_markup_copy_is_repaired_but_not_needlessly_rewritten() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Notes", None, "<p>Body.</p>");
    h.pull().await;

    std::fs::write(h.path(".Notes/storage.xml"), "<p>Someone edited this.</p>").unwrap();
    h.pull().await;
    assert_eq!(
        h.read(".Notes/storage.xml"),
        "<p>Body.</p>\n",
        "the copy mirrors the server, so a hand edit is corrected"
    );

    let before = std::fs::metadata(h.path(".Notes/storage.xml")).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    h.pull().await;
    let after = std::fs::metadata(h.path(".Notes/storage.xml")).unwrap().modified().unwrap();
    assert_eq!(before, after, "an up-to-date copy is not rewritten");
}

/// Scope still applies: pulling one subtree does not touch another's files.
#[tokio::test]
async fn backfilling_respects_the_pull_scope() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Wanted", None, "<p>One.</p>");
    h.mock.seed_page("1002", "Untouched", None, "<p>Two.</p>");
    h.pull().await;

    std::fs::remove_file(h.path(".Wanted/storage.xml")).unwrap();
    std::fs::remove_file(h.path(".Untouched/storage.xml")).unwrap();

    h.engine
        .pull(
            &mut h.ws,
            &PullOptions { scope: vec!["Wanted.md".into()], ..PullOptions::everything() },
        )
        .await
        .expect("pull");

    assert!(h.path(".Wanted/storage.xml").exists(), "the page in scope is backfilled");
    assert!(!h.path(".Untouched/storage.xml").exists(), "the page out of scope is left alone");
}

/// A mention renders as the person's name linked to their profile, and the
/// opaque key Confluence uses never reaches the reader.
#[tokio::test]
async fn mentions_render_as_named_profile_links() {
    for flavor in [Flavor::Cloud, Flavor::DataCenter] {
        let mut h = Harness::new(flavor);
        h.mock.seed_user("6cb6d404f61e0043d34f805b8eca16d6", "alice.ng", "Alice Ng");
        h.mock.seed_page(
            "1001",
            "Onboarding",
            None,
            "<p>Ask <ac:link><ri:user ri:userkey=\"6cb6d404f61e0043d34f805b8eca16d6\"/>\
             </ac:link> about it.</p>",
        );
        h.pull().await;

        let body = h.read("Onboarding.md");
        assert!(body.contains("[@Alice Ng]"), "{flavor}: got {body}");
        assert!(
            !body.contains("[6cb6d404"),
            "{flavor}: the reader sees a name, not the opaque key: {body}"
        );

        match flavor {
            // Data Center profiles are keyed by username, not by the mention id.
            Flavor::DataCenter => assert!(
                body.contains("/display/~alice.ng"),
                "{flavor}: expected a tilde-username profile URL, got {body}"
            ),
            Flavor::Cloud => assert!(
                body.contains("/people/6cb6d404f61e0043d34f805b8eca16d6"),
                "{flavor}: expected an account-id profile URL, got {body}"
            ),
        }
    }
}

/// Somebody confed cannot resolve is not linked to the wrong place: the block
/// keeps its original markup, and heals once the lookup succeeds.
#[tokio::test]
async fn an_unresolvable_mention_keeps_its_markup_and_heals_later() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page(
        "1001",
        "Onboarding",
        None,
        "<p>Ask <ac:link><ri:user ri:userkey=\"unknown-key\"/></ac:link> about it.</p>",
    );
    h.pull().await;

    let body = h.read("Onboarding.md");
    assert!(body.contains("```confluence"), "the block is preserved: {body}");
    assert!(body.contains("ri:userkey=\"unknown-key\""));
    assert!(!body.contains("display/~unknown-key"), "no link to a profile that does not exist");

    // The person becomes resolvable, and the next pull renders them properly.
    h.mock.seed_user("unknown-key", "bob.kaur", "Bob Kaur");
    h.pull().await;

    let body = h.read("Onboarding.md");
    assert!(body.contains("[@Bob Kaur](https://wiki.mock.test/display/~bob.kaur)"), "got {body}");
    assert!(!body.contains("```confluence"));
}

/// Editing a paragraph that mentions somebody sends the mention back as a
/// mention, not as an ordinary link.
#[tokio::test]
async fn an_edited_mention_survives_the_round_trip() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_user("key-1", "alice.ng", "Alice Ng");
    h.mock.seed_page(
        "1001",
        "Notes",
        None,
        "<p>Ask <ac:link><ri:user ri:userkey=\"key-1\"/></ac:link> about it.</p>",
    );
    h.pull().await;

    let edited = h.read("Notes.md").replace("about it.", "about the rollout.");
    h.write("Notes.md", &edited);
    h.push().await;

    let body = h.mock.page_body("1001").unwrap();
    assert!(body.contains(r#"<ri:user ri:userkey="key-1""#), "the mention is intact: {body}");
    assert!(body.contains("about the rollout"));
}

/// An improved converter reaches pages that were already synced: pull
/// re-renders them instead of waiting for each page to change on the server.
#[tokio::test]
async fn pull_re_renders_pages_left_by_an_older_converter() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_user("key-1", "alice.ng", "Alice Ng");
    h.mock.seed_page("1001", "Notes", None, "<p>Body.</p>");
    h.pull().await;

    // Stand in for a file written before the rendering rules changed.
    h.write("Notes.md", &h.read("Notes.md").replace("Body.", "STALE RENDERING"));
    let mut record = h.ws.state().get_page("1001").unwrap().unwrap();
    record.render_key = "rendered-by-an-older-confed".into();
    record.markdown_hash =
        confed_core::frontmatter::parse(&h.read("Notes.md"), "Notes.md").unwrap().content_hash();
    h.ws.state().upsert_page(&record).unwrap();
    assert_eq!(h.status("1001"), PageState::Unchanged, "neither side has changed");

    let outcome = h.pull().await;
    assert_eq!(outcome.updated.len(), 1, "the page is re-rendered: {outcome:?}");
    assert!(h.read("Notes.md").contains("Body."), "it now matches the current rules");
    assert_ne!(
        h.ws.state().get_page("1001").unwrap().unwrap().render_key,
        "rendered-by-an-older-confed",
        "the fingerprint is refreshed, so it does not re-render forever"
    );

    // And it does not keep re-rendering every pull.
    assert!(h.pull().await.is_empty(), "a page at the current version is left alone");
}

/// Re-rendering must never be an excuse to discard someone's edits.
#[tokio::test]
async fn a_stale_rendering_is_not_re_rendered_over_local_edits() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Notes", None, "<p>Body.</p>");
    h.pull().await;

    h.edit_body("Notes.md", "\nWork I have not pushed.\n");
    let mut record = h.ws.state().get_page("1001").unwrap().unwrap();
    record.render_key = "rendered-by-an-older-confed".into();
    h.ws.state().upsert_page(&record).unwrap();
    assert_eq!(h.status("1001"), PageState::Modified);

    h.pull().await;
    assert!(
        h.read("Notes.md").contains("Work I have not pushed."),
        "a modified page keeps its edits, stale rendering or not"
    );
}

/// `--reset` puts every tracked page back to what the server has, whatever
/// state it was in locally.
#[tokio::test]
async fn reset_restores_every_tracked_page_from_the_server() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Edited", None, "<p>Server text for one.</p>");
    h.mock.seed_page("1002", "Deleted", None, "<p>Server text for two.</p>");
    h.mock.seed_page("1003", "Conflicted", None, "<p>Shared.</p>");
    h.pull().await;

    // Three different kinds of local divergence.
    h.edit_body("Edited.md", "\nLocal edit.\n");
    std::fs::remove_file(h.path("Deleted.md")).unwrap();
    h.write("Conflicted.md", &h.read("Conflicted.md").replace("Shared.", "Ours."));
    h.mock.remote_edit("1003", "<p>Theirs.</p>");
    h.pull().await;
    assert_eq!(h.status("1003"), PageState::Conflicted);

    let outcome = h
        .engine
        .pull(&mut h.ws, &PullOptions { reset: true, ..PullOptions::everything() })
        .await
        .expect("reset");

    assert!(h.read("Edited.md").contains("Server text for one."));
    assert!(!h.read("Edited.md").contains("Local edit."));
    assert!(h.path("Deleted.md").exists(), "a locally deleted page comes back");
    assert!(h.read("Conflicted.md").contains("Theirs."));
    assert!(!h.read("Conflicted.md").contains("<<<<<<<"), "conflict markers are gone");

    for id in ["1001", "1002", "1003"] {
        assert_eq!(h.status(id), PageState::Unchanged, "page {id} matches the server");
    }
    assert!(outcome.skipped_dirty.is_empty(), "a reset never blocks");
    assert!(!outcome.discarded.is_empty(), "and it says what it threw away");
    assert!(outcome.discarded.iter().any(|p| p.path == "Edited.md"));
}

/// A file that exists only locally is not a tracked page, so a reset leaves it
/// alone — the same line git draws between `reset --hard` and `clean`.
#[tokio::test]
async fn reset_leaves_local_only_files_alone() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Tracked", None, "<p>From the server.</p>");
    h.pull().await;

    h.write("Draft.md", "---\ntitle: Draft\nlabels: []\n---\n\nNot pushed yet.\n");
    h.write("notes.txt", "not a page at all\n");

    h.engine
        .pull(&mut h.ws, &PullOptions { reset: true, ..PullOptions::everything() })
        .await
        .expect("reset");

    assert!(h.path("Draft.md").exists(), "an unpushed page is not the server's to reset");
    assert!(h.read("Draft.md").contains("Not pushed yet."));
    assert!(h.path("notes.txt").exists());
}

/// Comment drafts and attachments are local state too.
#[tokio::test]
async fn reset_discards_comment_drafts_and_restores_attachments() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Discussed", None, "<p>Body.</p>");
    h.mock.seed_comment("1001", "<p>A real comment.</p>", confed_api::CommentKind::Footer);
    h.pull().await;

    let mut sidecar = h.read(".Discussed/comments.md");
    sidecar.push_str("\n<!-- confed:new -->\nAn unpushed draft.\n");
    h.write(".Discussed/comments.md", &sidecar);

    h.engine
        .pull(&mut h.ws, &PullOptions { reset: true, ..PullOptions::everything() })
        .await
        .expect("reset");

    let sidecar = h.read(".Discussed/comments.md");
    assert!(sidecar.contains("A real comment."), "the server's comments stay");
    assert!(!sidecar.contains("An unpushed draft."), "a draft is a local change");
}

/// Nothing is written until the user asks for it.
#[tokio::test]
async fn a_dry_run_reset_reports_without_discarding() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Notes", None, "<p>Server text.</p>");
    h.pull().await;
    h.edit_body("Notes.md", "\nWork I might still want.\n");

    let outcome = h
        .engine
        .pull(&mut h.ws, &PullOptions { reset: true, dry_run: true, ..PullOptions::everything() })
        .await
        .expect("dry run");

    assert!(outcome.dry_run);
    assert!(
        outcome.discarded.iter().any(|p| p.path == "Notes.md"),
        "it says what a real reset would discard: {outcome:?}"
    );
    assert!(
        h.read("Notes.md").contains("Work I might still want."),
        "but nothing is actually discarded"
    );
}

/// A version already downloaded is never downloaded again: fetch asks which
/// versions exist and takes the rest from `.pages.db`.
#[tokio::test]
async fn a_version_already_seen_is_served_from_the_cache() {
    let mut h = Harness::new(Flavor::Cloud);
    for i in 0..3 {
        h.mock.seed_page(&format!("100{i}"), &format!("Page {i}"), None, "<p>Body.</p>");
    }
    h.pull().await;
    assert!(h.path(".pages.db").exists(), "the cache is written next to the state");

    // Throwing away the sync state is what a fresh clone looks like. The cache
    // survives it, so nothing has to be downloaded again.
    let before = h.mock.calls().len();
    h.ws.state().conn().execute("DELETE FROM remote_pages", []).unwrap();
    h.ws.state().clear_fetch_queue().unwrap();

    let outcome = h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    assert_eq!(outcome.fetched, 0, "nothing was downloaded");
    assert_eq!(outcome.from_cache, 3, "every page came from the cache");
    assert_eq!(h.mock.calls().len(), before, "and no extra call was made");

    // The restored state is complete enough to materialize from.
    for i in 0..3 {
        let remote = h.ws.state().get_remote(&format!("100{i}")).unwrap().unwrap();
        assert_eq!(remote.storage_body.as_deref(), Some("<p>Body.</p>"));
    }
}

/// A new version is downloaded, and then cached in its turn.
#[tokio::test]
async fn a_new_version_is_fetched_once_and_then_cached() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Notes", None, "<p>One.</p>");
    h.pull().await;

    h.mock.remote_edit("1001", "<p>Two.</p>");
    let outcome = h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    assert_eq!(outcome.fetched, 1, "the new version is downloaded");
    assert_eq!(outcome.from_cache, 0);

    // Both versions are now cached, so a revert costs nothing either.
    let cache = h.ws.page_store().unwrap();
    assert_eq!(cache.body("1001", 1).unwrap().as_deref(), Some("<p>One.</p>"));
    assert_eq!(cache.body("1001", 2).unwrap().as_deref(), Some("<p>Two.</p>"));

    h.ws.state().conn().execute("DELETE FROM remote_pages", []).unwrap();
    let outcome = h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    assert_eq!(outcome.fetched, 0);
    assert_eq!(outcome.from_cache, 1);
}

/// Losing the cache costs bandwidth, not correctness.
#[tokio::test]
async fn a_missing_cache_just_means_fetching_again() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Notes", None, "<p>Body.</p>");
    h.pull().await;

    h.ws.page_store().unwrap().clear().unwrap();
    h.ws.state().conn().execute("DELETE FROM remote_pages", []).unwrap();

    let outcome = h.engine.fetch(&mut h.ws, &Default::default()).await.expect("fetch");
    assert_eq!(outcome.fetched, 1, "with nothing cached it is downloaded again");
    assert_eq!(
        h.ws.state().get_remote("1001").unwrap().unwrap().storage_body.as_deref(),
        Some("<p>Body.</p>")
    );
}

/// A reset restores an attachment that was changed or deleted locally, and
/// leaves one that already matches the server alone.
#[tokio::test]
async fn reset_only_downloads_attachments_that_actually_differ() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>See the diagrams.</p>");
    h.pull().await;

    // Two attachments, both uploaded from the sidecar and then pulled back.
    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/kept.png"), b"unchanged bytes").unwrap();
    std::fs::write(h.path(".Diagrams/edited.png"), b"original bytes").unwrap();
    h.engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");
    h.pull().await;

    // One is modified locally; the other is untouched.
    std::fs::write(h.path(".Diagrams/edited.png"), b"locally modified").unwrap();

    let before = h.mock.calls().len();
    let outcome = h
        .engine
        .pull(&mut h.ws, &PullOptions { reset: true, ..PullOptions::everything() })
        .await
        .expect("reset");

    assert_eq!(
        outcome.attachments_downloaded, 1,
        "only the modified attachment is downloaded again"
    );
    assert_eq!(
        std::fs::read(h.path(".Diagrams/edited.png")).unwrap(),
        b"original bytes",
        "the local change is undone"
    );
    assert!(
        h.mock.calls().len() - before <= 2,
        "the untouched attachment costs no download: {:?}",
        h.mock.calls()
    );
}

/// A reset still restores an attachment somebody deleted.
#[tokio::test]
async fn reset_restores_a_deleted_attachment() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Diagrams", None, "<p>Body.</p>");
    h.pull().await;
    std::fs::create_dir_all(h.path(".Diagrams")).unwrap();
    std::fs::write(h.path(".Diagrams/diagram.png"), b"content").unwrap();
    h.engine
        .push(&mut h.ws, &PushOptions { with_attachments: true, ..Default::default() })
        .await
        .expect("push");
    h.pull().await;

    std::fs::remove_file(h.path(".Diagrams/diagram.png")).unwrap();
    h.engine
        .pull(&mut h.ws, &PullOptions { reset: true, ..PullOptions::everything() })
        .await
        .expect("reset");

    assert_eq!(std::fs::read(h.path(".Diagrams/diagram.png")).unwrap(), b"content");
}

/// Re-rendering the base must reproduce the file on disk exactly.
///
/// confed compares those two renderings constantly — `diff` shows the result,
/// and push's block patcher decides what to upload from it — so any option that
/// one path passes and another forgets shows up as a change the user never made.
/// A mention and a page link are the two things that need context to render.
#[tokio::test]
async fn the_base_re_renders_to_exactly_what_is_on_disk() {
    for flavor in [Flavor::Cloud, Flavor::DataCenter] {
        let mut h = Harness::new(flavor);
        h.mock.seed_user("key-1", "alice.ng", "Alice Ng");
        h.mock.seed_page("1001", "Target", None, "<p>The target page.</p>");
        h.mock.seed_page(
            "1002",
            "Source",
            None,
            "<p>Ask <ac:link><ri:user ri:userkey=\"key-1\"/></ac:link> about \
             <ac:link><ri:page ri:content-title=\"Target\"/></ac:link>.</p>",
        );
        h.pull().await;

        for page_id in ["1001", "1002"] {
            let record = h.ws.state().get_page(page_id).unwrap().unwrap();
            let on_disk =
                confed_core::frontmatter::parse(&h.read(&record.local_path), &record.local_path)
                    .unwrap();

            let options = confed_core::sync::page_convert_options(&h.ws, &record.local_path);
            let rendered = confed_convert::storage_to_markdown(&record.storage_body, &options)
                .unwrap()
                .markdown;

            assert_eq!(
                rendered.trim_end(),
                on_disk.body.trim_end(),
                "{flavor}: re-rendering {} differs from the file confed wrote, \
                 which would show as a phantom diff",
                record.local_path
            );
        }

        // And the page that needed context really did get it, rather than both
        // sides agreeing on an unresolved fallback.
        let source = h.read(&h.ws.state().get_page("1002").unwrap().unwrap().local_path);
        assert!(source.contains("[@Alice Ng]"), "{flavor}: the mention resolved: {source}");
        assert!(!source.contains("```confluence"), "{flavor}: nothing fell back to raw markup");
    }
}

// ------------------------------------------------------------ marks ----

const COMMENTED: &str = "<p>Read this during your <ac:inline-comment-marker ac:ref=\"marker-1\">first week checklist</ac:inline-comment-marker> and then ask questions.</p>";

// An open inline thread is shown at its span, and never counts as an edit.
both_flavors!(inline_comments_are_shown_as_marks_in_the_body, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Onboarding", None, COMMENTED);
    let id =
        h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    h.pull().await;

    let file = h.read("Onboarding.md");
    let expected = format!(
        "Read this during your <!--c {id} Alice Ng: Link the template?-->first week checklist<!--/c {id}--> and then ask questions."
    );
    assert!(file.contains(&expected), "{file}");
    assert_eq!(h.status("1001"), PageState::Unchanged, "a mark is not a local change");
    assert_eq!(h.comment_drafts("1001"), 0);

    let again = h.pull().await;
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(h.read("Onboarding.md"), file, "a second pull is byte-for-byte stable");

    // Resolved on the server: the mark leaves the body on the next pull.
    h.mock.resolve_seeded(&id);
    h.mock.remote_edit("1001", COMMENTED);
    h.pull().await;
    let file = h.read("Onboarding.md");
    assert!(!file.contains("<!--c"), "resolved threads are not shown: {file}");
    assert!(file.contains("first week checklist"), "the text itself stays");
    assert_eq!(h.status("1001"), PageState::Unchanged);
});

// `ids` mode drops the preview; `off` restores the old behaviour.
both_flavors!(marks_mode_controls_what_is_written, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Onboarding", None, COMMENTED);
    let id =
        h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);

    h.ws.state().set_meta(confed_core::sync::MARKS_MODE_KEY, "ids").unwrap();
    h.pull().await;
    assert!(h
        .read("Onboarding.md")
        .contains(&format!("<!--c {id}-->first week checklist<!--/c {id}-->")));

    h.ws.state().set_meta(confed_core::sync::MARKS_MODE_KEY, "off").unwrap();
    h.pull().await;
    assert!(!h.read("Onboarding.md").contains("<!--c"), "off means no marks at all");
    assert_eq!(h.status("1001"), PageState::Unchanged);
});

/// A `new` mark becomes an inline comment on push, at the right occurrence of
/// repeated text, and the base learns about the marker the server added.
#[tokio::test]
async fn a_new_mark_pushes_as_an_inline_comment() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page(
        "1001",
        "Teams",
        None,
        "<p>The platform team owns it.</p><p>The platform team is small.</p>",
    );
    h.pull().await;

    // Comment on the *second* "platform team".
    let file = h.read("Teams.md").replace(
        "The platform team is small.",
        "The <!--c new Still the right team?-->platform team<!--/c new--> is small.",
    );
    h.write("Teams.md", &file);
    assert_eq!(h.status("1001"), PageState::Unchanged, "a draft is comment work, not page work");
    assert_eq!(h.comment_drafts("1001"), 1);

    let plan = h
        .engine
        .plan_push(&h.ws, &PushOptions { with_comments: true, ..Default::default() })
        .unwrap();
    assert_eq!(plan.comment_ops.len(), 1, "{plan:?}");
    assert!(plan.comment_ops[0].contains("platform team"));

    let outcome = h.push().await;
    assert_eq!(outcome.comments_added.len(), 1);
    let id = &outcome.comments_added[0];

    let file = h.read("Teams.md");
    assert!(!file.contains("<!--c new"), "the draft was rewritten: {file}");
    assert!(file.contains(&format!("<!--c {id} ")), "…with its id: {file}");
    assert!(file.contains(&format!("platform team<!--/c {id}--> is small")), "{file}");
    assert_eq!(h.comment_drafts("1001"), 0);
    assert_eq!(h.status("1001"), PageState::Unchanged);

    let comments = h.engine.client().list_comments(&confed_api::PageId::new("1001")).await.unwrap();
    let anchor = comments[0].anchor.as_ref().expect("inline anchor");
    assert_eq!(anchor.text, "platform team");
    assert_eq!(anchor.match_index, Some(1), "the second occurrence was meant");
    assert_eq!(anchor.match_count, Some(2));

    // The server put a marker into the body without bumping the version, and
    // the base followed, so a later unrelated push does not wipe it out.
    let marker = format!("<ac:inline-comment-marker ac:ref=\"marker-{id}\">platform team</ac:inline-comment-marker> is small");
    assert!(h.mock.page_body("1001").unwrap().contains(&marker));
    let base = h.ws.state().get_page("1001").unwrap().unwrap();
    assert!(
        base.storage_body.contains(&marker),
        "the base carries the marker: {}",
        base.storage_body
    );
    assert!(h.read(".Teams/storage.xml").contains(&marker));

    h.edit_body("Teams.md", "\nAn unrelated paragraph.\n");
    h.push().await;
    let body = h.mock.page_body("1001").unwrap();
    assert!(body.contains(&marker), "the marker survived an unrelated push: {body}");
    assert!(body.contains("An unrelated paragraph"));

    // Pushing again posts nothing twice.
    let again = h.push().await;
    assert!(again.comments_added.is_empty());
}

// Editing the paragraph a thread sits in keeps the thread attached.
both_flavors!(an_edited_commented_paragraph_keeps_its_marker, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Onboarding", None, COMMENTED);
    let id =
        h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    h.pull().await;

    let file = h.read("Onboarding.md").replace("ask questions", "ask many questions");
    h.write("Onboarding.md", &file);
    assert_eq!(h.status("1001"), PageState::Modified);
    h.push().await;

    let body = h.mock.page_body("1001").unwrap();
    assert!(body.contains("ask many questions"), "{body}");
    assert!(
        body.contains("<ac:inline-comment-marker ac:ref=\"marker-1\">first week checklist</ac:inline-comment-marker>"),
        "the regenerated paragraph still carries the marker: {body}"
    );
    let file = h.read("Onboarding.md");
    assert!(file.contains(&format!("<!--c {id} ")), "the mark stays after push: {file}");
    assert_eq!(h.status("1001"), PageState::Unchanged);
});

// A mark an editor stripped comes back on the next pull; the user's edits do not
// get touched in the process.
both_flavors!(a_deleted_mark_is_re_placed_without_touching_edits, |mut h: Harness| async move {
    h.mock.seed_page("1001", "Onboarding", None, COMMENTED);
    let id =
        h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    h.pull().await;

    let raw = h.read("Onboarding.md");
    let stripped = confed_convert::marks::strip(&raw).body + "\nA local addition.\n";
    h.write("Onboarding.md", &stripped);
    assert!(!h.read("Onboarding.md").contains("<!--c"));

    h.engine
        .pull(&mut h.ws, &PullOptions { no_fetch: true, ..PullOptions::everything() })
        .await
        .expect("pull");
    let file = h.read("Onboarding.md");
    assert!(file.contains(&format!("<!--c {id} ")), "re-placed by text search: {file}");
    assert!(file.contains("A local addition."), "edits kept: {file}");
    assert_eq!(h.status("1001"), PageState::Modified);
});

// ------------------------------------------------- Data Center inline ----
//
// Data Center creates inline comments through its private plugin API, and
// unlike Cloud each one saves a new page version. The mock does the same.

/// A Data Center that saves a page version for each inline comment, as some
/// releases do (9.5.4 does not; see `data_center_without_a_version_bump`).
fn dc_with_page(storage: &str) -> Harness {
    let h = Harness::new(Flavor::DataCenter);
    h.mock.inline_comments_bump_version(true);
    h.mock.seed_page("1001", "Onboarding", None, storage);
    h
}

/// DC 9.5.4 wraps the marker without a new page version: the base takes the
/// new storage, the version stays, and the page is unchanged.
#[tokio::test]
async fn data_center_without_a_version_bump() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Onboarding", None, COMMENTED);
    h.pull().await;
    let file = h
        .read("Onboarding.md")
        .replace("ask questions", "ask <!--c new Who?-->questions<!--/c new-->");
    h.write("Onboarding.md", &file);
    let outcome = h.push().await;
    assert_eq!(outcome.comments_added.len(), 1);
    let id = &outcome.comments_added[0];
    assert_eq!(h.mock.page_version("1001"), Some(1));
    let base = h.ws.state().get_page("1001").unwrap().unwrap();
    assert_eq!(base.version, 1);
    assert!(base.storage_body.contains(&format!("marker-{id}")), "the base has the marker");
    assert_eq!(h.status("1001"), PageState::Unchanged);
    assert!(h.read("Onboarding.md").contains(&format!("questions<!--/c {id}-->")));
}

fn dc_push_opts() -> PushOptions {
    PushOptions { with_comments: true, ..Default::default() }
}

/// A body draft posts on DC; the version the comment created is adopted, so
/// the page is unchanged afterwards and a push has nothing to upload.
#[tokio::test]
async fn data_center_posts_a_body_draft_and_adopts_the_new_version() {
    let mut h = dc_with_page(COMMENTED);
    h.pull().await;
    let file = h
        .read("Onboarding.md")
        .replace("ask questions", "ask <!--c new Who?-->questions<!--/c new-->");
    h.write("Onboarding.md", &file);

    let outcome = h.push().await;
    assert_eq!(outcome.comments_added.len(), 1, "{outcome:?}");
    let id = &outcome.comments_added[0];
    assert_eq!(h.mock.page_version("1001"), Some(2), "DC saved a version for the comment");

    let file = h.read("Onboarding.md");
    assert!(file.contains(&format!("<!--c {id} ")), "the draft became the comment: {file}");
    assert!(file.contains(&format!("questions<!--/c {id}-->")), "{file}");
    assert!(file.contains("version: 2"), "the file names the adopted version: {file}");
    assert_eq!(h.status("1001"), PageState::Unchanged);

    let base = h.ws.state().get_page("1001").unwrap().unwrap();
    assert_eq!(base.version, 2);
    assert!(base.storage_body.contains(&format!("ac:ref=\"marker-{id}\"")), "base has the marker");

    let plan = h.engine.plan_push(&h.ws, &dc_push_opts()).unwrap();
    assert!(plan.ops.is_empty(), "nothing to upload: {:?}", plan.ops);
    let again = h.push().await;
    assert!(again.pushed.is_empty() && again.comments_added.is_empty(), "{again:?}");
}

/// After a comment, an edit elsewhere on the page uploads the commented block
/// byte for byte, marker included.
#[tokio::test]
async fn data_center_keeps_the_new_marker_through_an_unrelated_edit() {
    let storage = format!("{COMMENTED}<p>Second paragraph.</p>");
    let mut h = dc_with_page(&storage);
    h.pull().await;
    let file = h
        .read("Onboarding.md")
        .replace("ask questions", "ask <!--c new Who?-->questions<!--/c new-->");
    h.write("Onboarding.md", &file);
    h.push().await;
    let commented = h.mock.page_body("1001").unwrap();
    let block_end = commented.find("<p>Second").unwrap();

    h.write("Onboarding.md", &h.read("Onboarding.md").replace("Second paragraph.", "Edited."));
    let outcome = h.push().await;
    assert_eq!(outcome.pushed.len(), 1, "{outcome:?}");
    let after = h.mock.page_body("1001").unwrap();
    assert_eq!(&after[..block_end], &commented[..block_end], "the commented block is untouched");
    assert!(after.contains("<p>Edited.</p>"));
}

/// Text that is in the file but not on the server is an unpushed edit. When
/// the page itself cannot be pushed (the server moved on), the comment is
/// refused with exit 7 instead of letting the server reject the selection.
#[tokio::test]
async fn data_center_refuses_a_draft_on_an_unpushed_edit() {
    let mut h = dc_with_page(COMMENTED);
    h.pull().await;
    h.mock.remote_edit("1001", &format!("{COMMENTED}<p>Added elsewhere.</p>"));
    let file = h.read("Onboarding.md").replace(
        "and then ask questions.",
        "and then ask <!--c new Who?-->other questions<!--/c new-->.",
    );
    h.write("Onboarding.md", &file);

    let err = h.engine.push(&mut h.ws, &dc_push_opts()).await.expect_err("refused");
    assert_eq!(err.exit_code(), confed_core::error::ExitCode::State, "{err}");
    assert!(err.to_string().contains("unpushed edits"), "{err}");
    assert!(h.mock.calls().iter().all(|c| !c.starts_with("add_inline_comment")));
}

/// With the edit pushed first, the same draft goes through in one push.
#[tokio::test]
async fn data_center_pushes_the_edit_then_the_comment_on_it() {
    let mut h = dc_with_page(COMMENTED);
    h.pull().await;
    let file = h.read("Onboarding.md").replace(
        "and then ask questions.",
        "and then ask <!--c new Who?-->other questions<!--/c new-->.",
    );
    h.write("Onboarding.md", &file);
    let outcome = h.push().await;
    assert_eq!(outcome.pushed.len(), 1);
    assert_eq!(outcome.comments_added.len(), 1);
    assert_eq!(h.mock.page_version("1001"), Some(3), "the edit, then the comment");
    assert_eq!(h.status("1001"), PageState::Unchanged);
}

/// A sidecar draft takes an occurrence; replies to an inline thread go to the
/// inline API; resolving works; the sidecar does not post anything twice.
#[tokio::test]
async fn data_center_sidecar_drafts_replies_and_resolve() {
    let mut h = dc_with_page("<p>team one</p><p>team two</p>");
    let root = h.mock.seed_comment("1001", "<p>Root.</p>", confed_api::CommentKind::Inline);
    h.pull().await;

    let mut sidecar = h.read(".Onboarding/comments.md");
    sidecar.push_str(&format!(
        "\n<!-- confed:new anchor=\"team\" occurrence=2 -->\nSecond team?\n\
         \n<!-- confed:new reply-to={root} -->\nAgreed.\n\
         \n<!-- confed:resolve id={root} -->\n"
    ));
    h.write(".Onboarding/comments.md", &sidecar);

    let outcome = h.push().await;
    assert_eq!(outcome.comments_added.len(), 1, "the inline comment: {outcome:?}");
    assert_eq!(outcome.replies_added.len(), 1, "the reply: {outcome:?}");
    assert_eq!(outcome.comments_resolved, vec![root.0.clone()], "the resolve: {outcome:?}");
    let calls = h.mock.calls();
    assert!(calls.iter().any(|c| c == "add_inline_comment:1001"), "{calls:?}");
    assert!(calls.iter().any(|c| c == "add_inline_reply:1001"), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("resolve_comment:")), "{calls:?}");
    assert!(
        h.mock.page_body("1001").unwrap().contains("<p>team one</p><p><ac:inline-comment-marker"),
        "occurrence 2 is the one marked: {}",
        h.mock.page_body("1001").unwrap()
    );

    assert_eq!(h.comment_drafts("1001"), 0, "posted drafts leave the sidecar");
    let again = h.push().await;
    assert!(again.comments_added.is_empty(), "nothing is posted twice: {again:?}");
    assert_eq!(h.status("1001"), PageState::Unchanged);
}

/// When somebody else edited the page too, the comment's version is not
/// adopted: that is a remote change for `pull` to merge.
#[tokio::test]
async fn data_center_does_not_adopt_a_version_with_other_changes() {
    let mut h = dc_with_page(COMMENTED);
    h.pull().await;
    h.mock.remote_edit("1001", &COMMENTED.replace("questions", "questions twice"));
    let file = h
        .read("Onboarding.md")
        .replace("first week checklist", "<!--c new Hm-->first week checklist<!--/c new-->");
    h.write("Onboarding.md", &file);

    h.push().await;
    let base = h.ws.state().get_page("1001").unwrap().unwrap();
    assert_eq!(base.version, 1, "the base stays where it was");
    h.pull().await;
    assert!(h.read("Onboarding.md").contains("questions twice"), "pull brings the other edit");
}

/// A broken draft stops the push with its line, before anything is uploaded.
#[tokio::test]
async fn a_malformed_draft_is_refused_with_its_line() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Teams", None, "<p>One.</p><p>Two.</p>");
    h.pull().await;
    h.write("Teams.md", &h.read("Teams.md").replace("Two.", "<!--c new-->Two.<!--/c new-->"));

    let err = h
        .engine
        .push(&mut h.ws, &PushOptions { with_comments: true, ..Default::default() })
        .await
        .expect_err("an empty draft body");
    assert_eq!(err.exit_code(), confed_core::error::ExitCode::State, "{err}");
    assert!(err.to_string().contains("no comment text"), "{err}");

    h.write(
        "Teams.md",
        &h.read("Teams.md").replace("<!--c new-->Two.<!--/c new-->", "<!--c new Why?-->Two."),
    );
    let err = h
        .engine
        .push(&mut h.ws, &PushOptions { with_comments: true, ..Default::default() })
        .await
        .expect_err("an unterminated draft");
    assert!(err.to_string().contains("never closed"), "{err}");
    assert!(h.mock.page_body("1001").unwrap().contains("<p>Two.</p>"), "nothing was uploaded");
}

/// A conflicted file carries no marks for existing threads — but keeps the
/// user's drafts — and gets its marks back once the conflict is gone.
#[tokio::test]
async fn a_conflicted_page_keeps_drafts_but_no_marks() {
    let mut h = Harness::new(Flavor::Cloud);
    let body = format!("<p>Shared.</p>{COMMENTED}");
    h.mock.seed_page("1001", "Doc", None, &body);
    let id =
        h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    h.pull().await;

    let file = h
        .read("Doc.md")
        .replace("Shared.", "Ours.")
        .replace("ask questions", "ask <!--c new Who?-->questions<!--/c new-->");
    h.write("Doc.md", &file);
    h.mock.remote_edit("1001", &body.replace("Shared.", "Theirs."));
    h.pull().await;
    assert_eq!(h.status("1001"), PageState::Conflicted);

    let file = h.read("Doc.md");
    assert!(!file.contains(&format!("<!--c {id}")), "no marks in a conflicted file: {file}");
    assert!(file.contains("<!--c new Who?-->"), "the draft is kept: {file}");

    // Resolve by hand: the next pull places the marks again.
    let resolved = confed_core::merge::has_conflict_markers(&file);
    assert!(resolved, "sanity: the file had conflict hunks");
    let fixed: String = file
        .lines()
        .filter(|l| {
            !l.starts_with("<<<<<<<")
                && !l.starts_with("|||||||")
                && !l.starts_with("=======")
                && !l.starts_with(">>>>>>>")
        })
        .filter(|l| *l != "Shared." && *l != "Theirs.")
        .map(|l| format!("{l}\n"))
        .collect();
    h.write("Doc.md", &fixed);
    h.ws.state().set_sync_state("1001", SyncState::Clean).unwrap();
    h.engine
        .pull(&mut h.ws, &PullOptions { no_fetch: true, ..PullOptions::everything() })
        .await
        .expect("pull");
    let file = h.read("Doc.md");
    assert!(file.contains(&format!("<!--c {id} ")), "marks are back: {file}");
    assert!(file.contains("<!--c new Who?-->"), "{file}");
}

// A comment whose marker sits in a block kept as raw storage is listed in the
// sidecar but never marked in the body: the fence must stay byte-identical
// to the server's markup, and a reset must leave the tree clean.
both_flavors!(a_comment_inside_a_raw_block_never_marks_the_fence, |mut h: Harness| async move {
    let storage = "<p>Intro.</p><ac:structured-macro ac:name=\"mystery\"><ac:parameter ac:name=\"x\">(<ac:inline-comment-marker ac:ref=\"marker-1\">first week checklist</ac:inline-comment-marker>)</ac:parameter></ac:structured-macro>";
    h.mock.seed_page("1001", "Raw", None, storage);
    h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    h.pull().await;

    let file = h.read("Raw.md");
    assert!(file.contains("```confluence"), "the macro is kept raw: {file}");
    assert!(!file.contains("<!--c"), "no mark inside the raw block: {file}");
    assert_eq!(h.status("1001"), PageState::Unchanged);

    for _ in 0..2 {
        h.engine
            .pull(&mut h.ws, &PullOptions { reset: true, ..PullOptions::everything() })
            .await
            .expect("reset");
        assert_eq!(h.read("Raw.md"), file, "a reset converges");
        assert_eq!(h.status("1001"), PageState::Unchanged, "a reset leaves the page clean");
    }
});

// What `diff` compares the file against — the base, re-rendered — equals
// the body read from disk, for a page whose comments start a list item and
// span markers split across `<code>`.
both_flavors!(the_base_and_the_file_agree_with_marks_present, |mut h: Harness| async move {
    let storage = "<ol><li><ac:inline-comment-marker ac:ref=\"marker-1\">first week checklist</ac:inline-comment-marker>: <strong>not set</strong></li></ol><p>See <ac:inline-comment-marker ac:ref=\"marker-1\">(</ac:inline-comment-marker><code><ac:inline-comment-marker ac:ref=\"marker-1\">init time</ac:inline-comment-marker></code>) here.</p>";
    h.mock.seed_page("1001", "Split", None, storage);
    h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    h.pull().await;
    assert_eq!(h.status("1001"), PageState::Unchanged);

    let record = h.ws.state().get_page("1001").unwrap().unwrap();
    let on_disk =
        confed_core::frontmatter::parse(&h.read(&record.local_path), &record.local_path).unwrap();
    let options = confed_core::sync::page_convert_options(&h.ws, &record.local_path);
    let rendered = confed_core::sync::comparable_markdown(&record.storage_body, &options).unwrap();
    assert_eq!(
        rendered.trim_end(),
        on_disk.body.trim_end(),
        "stripped base and file must agree, or diff reports a phantom change"
    );
    assert_eq!(h.read(&record.local_path).matches("<!--c").count(), 2, "one mark per block");
});

// ------------------------------------------ drafts at a paragraph's start ----

/// Write a `new` mark the way `confed comment add --anchor` does: at the
/// anchor's offset in the stripped body, then rendered through the mark layer.
fn add_draft(h: &Harness, file: &str, anchor: &str, note: &str) {
    let content = h.read(file);
    let mut parsed = confed_core::frontmatter::parse(&content, file).unwrap();
    let start = parsed.body.find(anchor).unwrap_or_else(|| panic!("{anchor:?} not in {file}"));
    let line = parsed.body[..start].matches('\n').count() + 1;
    parsed.marks.push(confed_convert::Mark {
        id: confed_convert::MarkId::New,
        start,
        end: Some(start + anchor.len()),
        text: anchor.to_string(),
        note: note.to_string(),
        line,
    });
    h.write(file, &parsed.render().unwrap());
}

/// The anchor in the storage the comment wrapped.
fn wrapped_text(storage: &str) -> String {
    let open = storage.find("<ac:inline-comment-marker").expect("a marker");
    let inner = &storage[open..];
    let start = inner.find('>').unwrap() + 1;
    let end = inner.find("</ac:inline-comment-marker>").unwrap();
    inner[start..end].to_string()
}

/// A comment on the start of a paragraph, the whole paragraph, or text right
/// after inline markup is posted on exactly the text asked for, and every
/// report of it says so — even where the mark itself has to sit a character
/// in so the paragraph stays a paragraph.
#[tokio::test]
async fn drafts_at_a_paragraph_start_mean_the_whole_anchor() {
    struct Case {
        storage: &'static str,
        anchor: &'static str,
        /// What the mark looks like in the file.
        in_file: &'static str,
    }
    let cases = [
        // ASCII and multibyte, at the start of a paragraph but not all of it.
        Case {
            storage: "<p>Welcome to the team.</p>",
            anchor: "Welcome",
            in_file: "W<!--c new t-->elcome<!--/c new--> to the team.",
        },
        Case {
            storage: "<p>Фраза 1.</p><p>Фраза 2.</p><p>Фраза 3.</p>",
            anchor: "Фраза 2",
            in_file: "Ф<!--c new t-->раза 2<!--/c new-->.",
        },
        // The whole paragraph.
        Case {
            storage: "<p>Фраза 1.</p><p>Фраза 2.</p><p>Фраза 3.</p>",
            anchor: "Фраза 2.",
            in_file: "Ф<!--c new t-->раза 2.<!--/c new-->",
        },
        Case {
            storage: "<p>One line.</p>",
            anchor: "One line.",
            in_file: "O<!--c new t-->ne line.<!--/c new-->",
        },
        // Right after inline markup, and inside it at the paragraph's start.
        Case {
            storage: "<p><strong>bold</strong>text</p>",
            anchor: "text",
            in_file: "**bold**<!--c new t-->text<!--/c new-->",
        },
        Case {
            storage: "<p><strong>bold</strong>text</p>",
            anchor: "bold",
            in_file: "**b<!--c new t-->old**<!--/c new-->text",
        },
    ];
    for flavor in [Flavor::Cloud, Flavor::DataCenter] {
        for case in &cases {
            let mut h = Harness::new(flavor);
            h.mock.seed_page("1001", "Page", None, case.storage);
            h.pull().await;
            let md_anchor = if case.anchor == "bold" { "**bold**" } else { case.anchor };
            add_draft(&h, "Page.md", md_anchor, "t");
            let file = h.read("Page.md");
            assert!(file.contains(case.in_file), "{flavor} {:?}: {file}", case.anchor);

            let plan = h
                .engine
                .plan_push(&h.ws, &PushOptions { with_comments: true, ..Default::default() })
                .unwrap();
            assert!(
                plan.comment_ops
                    .iter()
                    .any(|op| op.contains(&format!("add inline comment on \"{}\"", case.anchor))),
                "{flavor}: the dry run names {:?}: {:?}",
                case.anchor,
                plan.comment_ops
            );

            let outcome = h.push().await;
            assert_eq!(outcome.comments_added.len(), 1, "{flavor} {:?}", case.anchor);
            let posted = h.ws.state().page_comments("1001").unwrap();
            let anchor: confed_api::InlineAnchor =
                serde_json::from_str(posted.last().unwrap().anchor.as_deref().unwrap()).unwrap();
            assert_eq!(anchor.text, case.anchor, "{flavor}: the selection posted");
            assert_eq!(
                wrapped_text(&h.mock.page_body("1001").unwrap()),
                case.anchor,
                "{flavor}: the text the server wrapped"
            );
            assert_eq!(h.status("1001"), PageState::Unchanged, "{flavor} {:?}", case.anchor);
        }
    }
}

/// A sidecar draft is listed as the inline comment it is, with its anchor and
/// occurrence; a reply as a reply.
#[tokio::test]
async fn the_dry_run_describes_sidecar_drafts() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Page", None, "<p>Фраза 2. Фраза 2.</p>");
    let root = h.mock.seed_comment("1001", "<p>Root.</p>", confed_api::CommentKind::Footer);
    h.pull().await;
    let mut sidecar = h.read(".Page/comments.md");
    sidecar.push_str(&format!(
        "\n<!-- confed:new anchor=\"Фраза 2.\" occurrence=2 -->\nt\n\
         \n<!-- confed:new reply-to={root} -->\nОк.\n"
    ));
    h.write(".Page/comments.md", &sidecar);

    let plan = h
        .engine
        .plan_push(&h.ws, &PushOptions { with_comments: true, ..Default::default() })
        .unwrap();
    assert!(
        plan.comment_ops.contains(
            &"1001: add inline comment on \"Фраза 2.\", occurrence 2 (comments.md)".to_string()
        ),
        "{:?}",
        plan.comment_ops
    );
    assert!(
        plan.comment_ops.contains(&format!("1001: reply to {root} (3 chars)")),
        "{:?}",
        plan.comment_ops
    );
}

/// A page fetch found but pull has not written yet is reported under the path
/// pull will give it, not an empty one.
#[tokio::test]
async fn a_new_remote_page_has_its_future_path_in_status() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Parent", None, "<p>p</p>");
    h.pull().await;
    h.mock.seed_page("1002", "CH-200.7", Some("1001"), "<p>new</p>");
    h.engine.fetch(&mut h.ws, &Default::default()).await.unwrap();

    let status = worktree::scan(&h.ws).unwrap();
    let page = status.find("1002").expect("listed");
    assert_eq!(page.state, PageState::RemoteNew);
    assert_eq!(page.path, "Parent/CH-200.7.md");
}

/// A page deleted on the server before it was ever pulled is not "deleted"
/// locally: there was never a file.
#[tokio::test]
async fn an_unpulled_page_deleted_on_the_server_is_not_reported() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Kept", None, "<p>k</p>");
    h.pull().await;
    h.mock.seed_page("1002", "Short-lived", None, "<p>s</p>");
    h.engine.fetch(&mut h.ws, &Default::default()).await.unwrap();
    h.mock.delete_page_directly("1002");
    let outcome = h.pull().await;
    assert!(outcome.deleted.is_empty(), "{:?}", outcome.deleted);
}

/// A posted comment can be edited and deleted from confed; the sidecar and
/// the mark layer follow.
#[tokio::test]
async fn comments_are_edited_and_deleted_on_the_server() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Onboarding", None, COMMENTED);
    let id =
        h.mock.seed_comment("1001", "<p>Link the template?</p>", confed_api::CommentKind::Inline);
    h.pull().await;
    assert!(h.read("Onboarding.md").contains(&format!("<!--c {id} Alice Ng: Link the template?")));

    h.engine.edit_comment(&mut h.ws, &id.0, "Link the **new** template?").await.unwrap();
    assert!(h.mock.calls().contains(&format!("update_comment:{id}")));
    let listed = h.engine.client().list_comments(&confed_api::PageId::new("1001")).await.unwrap();
    assert!(listed[0].body_storage.contains("<strong>new</strong>"), "{:?}", listed[0]);
    assert!(h.read(".Onboarding/comments.md").contains("Link the **new** template?"));
    assert!(h.read("Onboarding.md").contains("Link the **new** template?"), "the preview follows");

    // A reply, known locally, goes with its thread and is reported.
    let reply = h
        .engine
        .client()
        .add_inline_reply(&confed_api::PageId::new("1001"), &id, "<p>ok</p>")
        .await
        .unwrap();
    h.ws.state()
        .upsert_comment(&confed_core::state::CommentRecord {
            comment_id: reply.id.0.clone(),
            page_id: "1001".into(),
            parent_comment_id: Some(id.0.clone()),
            kind: "inline".into(),
            author: None,
            created_at: None,
            body_storage: Some("<p>ok</p>".into()),
            body_markdown: "ok".into(),
            resolved: false,
            anchor: None,
            synced_at: None,
        })
        .unwrap();
    let replies = h.engine.delete_comment(&mut h.ws, &id.0).await.unwrap();
    assert_eq!(replies, vec![reply.id.0.clone()]);
    assert!(h.ws.state().page_comments("1001").unwrap().is_empty());
    assert!(h.mock.calls().contains(&format!("delete_comment:{id}")));
    assert!(!h.read("Onboarding.md").contains("<!--c"), "its mark leaves the body");
    assert!(!h.read(".Onboarding/comments.md").contains("template"));
    assert_eq!(h.status("1001"), PageState::Unchanged);
    // Data Center leaves the marker in the page; it is now an orphan.
    let orphans = confed_core::sync::orphan_markers(&h.ws, "1001").unwrap();
    assert_eq!(orphans, vec![("marker-1".to_string(), "first week checklist".to_string())]);

    let err = h.engine.delete_comment(&mut h.ws, "999").await.unwrap_err();
    assert_eq!(err.exit_code(), confed_core::error::ExitCode::NotFound);
}

/// Comment work on a page that was deleted on the server fails for that page
/// with a message that says so, keeps the drafts, and lets other pages through.
#[tokio::test]
async fn comment_work_on_a_page_deleted_on_the_server() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Gone", None, "<p>Gone soon.</p>");
    h.mock.seed_page("1002", "Kept", None, "<p>Still here.</p>");
    h.pull().await;
    for page in ["Gone", "Kept"] {
        let path = format!(".{page}/comments.md");
        let existing = std::fs::read_to_string(h.path(&path)).unwrap_or_default();
        h.write(&path, &format!("{existing}\n<!-- confed:new -->\nA note.\n"));
    }
    h.mock.delete_page_directly("1001");

    let outcome = h.push().await;
    assert_eq!(outcome.comments_added.len(), 1, "the live page's comment is posted");
    let failure = outcome.failed.iter().find(|f| f.page_id == "1001").expect("reported");
    assert!(failure.error.contains("no longer exists on the server"), "{}", failure.error);
    assert!(h.read(".Gone/comments.md").contains("A note."), "the draft is kept");
}

/// Inline threads are one level deep on Data Center, as in its web UI: a reply
/// to a reply is posted to the thread's root. Page-comment threads nest.
#[tokio::test]
async fn a_reply_to_a_reply_goes_where_the_thread_allows() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Onboarding", None, COMMENTED);
    let inline = h.mock.seed_comment("1001", "<p>Root.</p>", confed_api::CommentKind::Inline);
    let footer = h.mock.seed_comment("1001", "<p>Page root.</p>", confed_api::CommentKind::Footer);
    use confed_api::ConfluenceClient;
    let client = h.mock.clone();
    let page = confed_api::PageId::new("1001");
    let inline_reply = client.add_inline_reply(&page, &inline, "<p>r1</p>").await.unwrap();
    let footer_reply = client.add_footer_comment(&page, "<p>f1</p>", Some(&footer)).await.unwrap();
    h.pull().await;
    // Pull refreshes comments of pages it writes; record the replies directly.
    for (reply, parent, kind) in
        [(&inline_reply, &inline, "inline"), (&footer_reply, &footer, "footer")]
    {
        h.ws.state()
            .upsert_comment(&confed_core::state::CommentRecord {
                comment_id: reply.id.0.clone(),
                page_id: "1001".into(),
                parent_comment_id: Some(parent.0.clone()),
                kind: kind.into(),
                author: None,
                created_at: None,
                body_storage: None,
                body_markdown: "r".into(),
                resolved: false,
                anchor: None,
                synced_at: None,
            })
            .unwrap();
    }

    let sidecar = h.read(".Onboarding/comments.md");
    h.write(
        ".Onboarding/comments.md",
        &format!(
            "{sidecar}\n<!-- confed:new reply-to={} -->\nOn the inline reply.\n\
             \n<!-- confed:new reply-to={} -->\nOn the page reply.\n",
            inline_reply.id, footer_reply.id
        ),
    );
    let outcome = h.push().await;
    assert_eq!(outcome.replies_added.len(), 2, "{outcome:?}");

    let all = client.list_comments(&page).await.unwrap();
    let parent_of = |id: &str| {
        all.iter().find(|c| c.id.0 == id).and_then(|c| c.parent_comment_id.clone()).unwrap()
    };
    assert_eq!(parent_of(&outcome.replies_added[0]), inline, "inline: to the thread root");
    assert_eq!(parent_of(&outcome.replies_added[1]), footer_reply.id, "page thread: nested");
}

/// A page file confed no longer tracks, whose page is gone from the server,
/// still has its comment drafts reported — not silently dropped.
#[tokio::test]
async fn comment_work_on_an_untracked_page_is_reported() {
    let mut h = Harness::new(Flavor::DataCenter);
    h.mock.seed_page("1001", "Probe", None, "<p>Probe.</p>");
    h.mock.seed_page("1002", "Kept", None, "<p>Kept.</p>");
    h.pull().await;
    for page in ["Probe", "Kept"] {
        let path = format!(".{page}/comments.md");
        let existing = std::fs::read_to_string(h.path(&path)).unwrap_or_default();
        h.write(&path, &format!("{existing}\n<!-- confed:new -->\nA note.\n"));
    }
    // As after `confed rm --push` with the file put back: no base record, and
    // no page on the server.
    h.ws.state().delete_page("1001").unwrap();
    h.mock.delete_page_directly("1001");

    let plan = h
        .engine
        .plan_push(&h.ws, &PushOptions { with_comments: true, ..Default::default() })
        .unwrap();
    assert!(plan.comment_ops.iter().any(|op| op.starts_with("1001:")), "the dry run lists it");
    let outcome = h.push().await;
    assert_eq!(outcome.comments_added.len(), 1, "the tracked page's comment goes out");
    let failure =
        outcome.failed.iter().find(|f| f.page_id == "1001").expect("reported, not dropped");
    assert!(failure.error.contains("no longer exists"), "{}", failure.error);
    assert!(h.read(".Probe/comments.md").contains("A note."), "kept");
}
