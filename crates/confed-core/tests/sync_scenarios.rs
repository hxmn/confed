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
/// source of a conversion is always inspectable without a round trip.
#[tokio::test]
async fn pull_saves_the_confluence_markup_beside_each_page() {
    let mut h = Harness::new(Flavor::Cloud);
    let body = "<p>Intro.</p><ac:structured-macro ac:name=\"jira\"><ac:parameter \
                ac:name=\"key\">PROJ-1</ac:parameter></ac:structured-macro>";
    h.mock.seed_page("1001", "Onboarding", None, body);
    h.mock.seed_page("1002", "Nested", Some("1001"), "<p>Child.</p>");
    h.pull().await;

    // Byte-for-byte what the server holds, not a re-serialization of it.
    assert_eq!(h.read(".Onboarding/storage.xml"), body);
    assert_eq!(h.read("Onboarding/.Nested/storage.xml"), "<p>Child.</p>");
}

/// The copy tracks the page: it is refreshed by pull, refreshed again by push,
/// and never uploaded back as an attachment.
#[tokio::test]
async fn the_markup_copy_follows_the_page_and_is_not_an_attachment() {
    let mut h = Harness::new(Flavor::Cloud);
    h.mock.seed_page("1001", "Notes", None, "<p>First.</p>");
    h.pull().await;
    assert_eq!(h.read(".Notes/storage.xml"), "<p>First.</p>");

    // A remote edit refreshes it.
    h.mock.remote_edit("1001", "<p>Second, from the server.</p>");
    h.pull().await;
    assert_eq!(h.read(".Notes/storage.xml"), "<p>Second, from the server.</p>");

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
    assert_eq!(on_disk, h.mock.page_body("1001").unwrap(), "it matches what the server now has");
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
