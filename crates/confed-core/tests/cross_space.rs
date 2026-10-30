//! Moving pages between the spaces of an instance root, against the stateful
//! mock server: two real workspaces on disk, one move, and everything that
//! has to be true afterwards.

use confed_api::{CommentKind, ConfluenceClient, Flavor, MockClient, MovePosition, SpaceId};
use confed_core::crossmove::{self, MovePlan, MoveRequest, OnConflict, SpaceRef};
use confed_core::instance::{Instance, Step};
use confed_core::sync::{PullOptions, SyncEngine};
use confed_core::workspace::Workspace;
use confed_core::worktree;
use std::collections::BTreeMap;
use std::sync::Arc;

const BASE: &str = "https://wiki.mock.test";

struct Harness {
    _dir: tempfile::TempDir,
    instance: Instance,
    mock: Arc<MockClient>,
    spaces: BTreeMap<String, Workspace>,
}

impl Harness {
    /// `DOCS` and `ENG`, registered under one root:
    ///
    /// ```text
    /// DOCS  Team Handbook (home)          ENG  Engineering Home (home)
    ///         Onboarding  ─ comment             Runbooks → DOCS:Week One
    ///           Week One  ─ attachment
    ///         Policies → Onboarding
    /// ```
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let instance = Instance::create(dir.path(), BASE, Flavor::DataCenter).unwrap();
        let mock = Arc::new(MockClient::new(Flavor::DataCenter));
        mock.seed_space("ENG", "Engineering", "global", None);

        mock.seed_page("1001", "Team Handbook", None, "<p>Welcome to the team.</p>");
        mock.set_homepage("DOCS", "1001");
        mock.seed_page(
            "1002",
            "Onboarding",
            Some("1001"),
            r#"<p>Start with <ac:link><ri:page ri:content-title="Team Handbook" /></ac:link>, then <ac:link><ri:page ri:content-title="Week One" /></ac:link>.</p>"#,
        );
        mock.seed_page("1003", "Week One", Some("1002"), "<p>План работ, день за днём.</p>");
        mock.seed_page(
            "1004",
            "Policies",
            Some("1001"),
            r#"<p>New here? Read <ac:link><ri:page ri:content-title="Onboarding" /></ac:link>.</p>"#,
        );
        mock.seed_comment("1002", "<p>Is this current?</p>", CommentKind::Footer);
        mock.attach_directly("1003", "plan.txt", b"day one: badge");

        mock.seed_page_in("ENG", "2001", "Engineering Home", None, "<p>Engineering.</p>");
        mock.set_homepage("ENG", "2001");
        mock.seed_page_in(
            "ENG",
            "2002",
            "Runbooks",
            Some("2001"),
            r#"<p>New starters: <ac:link><ri:page ri:space-key="DOCS" ri:content-title="Week One" /></ac:link>.</p>"#,
        );

        let mut spaces = BTreeMap::new();
        for (key, numeric) in [("DOCS", "1001"), ("ENG", "2001")] {
            let path = dir.path().join(key);
            let ws = Workspace::create(&path).unwrap();
            ws.state().set_meta("space_key", key).unwrap();
            ws.state().set_meta("space_id", numeric).unwrap();
            ws.state().set_meta("base_url", BASE).unwrap();
            ws.state().set_meta("flavor", "datacenter").unwrap();
            instance.add_member(key, Some(numeric), key).unwrap();
            spaces.insert(key.to_string(), ws);
        }
        let mut h = Self { _dir: dir, instance, mock, spaces };
        h.pull("DOCS").await;
        h.pull("ENG").await;
        h
    }

    fn client(&self) -> Arc<dyn ConfluenceClient> {
        self.mock.clone()
    }

    fn engine(&self, key: &str) -> SyncEngine {
        let numeric = self.spaces[key].space_numeric_id().unwrap();
        SyncEngine::new(self.client(), SpaceId { key: key.into(), numeric }, 2)
    }

    async fn pull(&mut self, key: &str) {
        let engine = self.engine(key);
        engine.pull(self.spaces.get_mut(key).unwrap(), &PullOptions::everything()).await.unwrap();
    }

    fn ws(&self, key: &str) -> &Workspace {
        &self.spaces[key]
    }

    fn read(&self, key: &str, path: &str) -> String {
        std::fs::read_to_string(self.ws(key).absolute(path))
            .unwrap_or_else(|e| panic!("{key}:{path}: {e}"))
    }

    fn request(&self, root: &str, target: &str) -> MoveRequest {
        MoveRequest {
            root_id: root.into(),
            source: SpaceRef {
                key: "DOCS".into(),
                dir: "DOCS".into(),
                numeric: Some("1001".into()),
            },
            dest: SpaceRef { key: "ENG".into(), dir: "ENG".into(), numeric: Some("2001".into()) },
            position: MovePosition::Append,
            target_id: target.into(),
            new_parent_id: Some(target.into()),
            new_title: None,
            on_conflict: OnConflict::Fail,
            rewrite_links: true,
            relativize: false,
            message: Some("move onboarding to engineering".into()),
            wait_secs: 5,
        }
    }

    async fn plan(&self, request: &MoveRequest) -> confed_core::Result<MovePlan> {
        crossmove::plan(&self.client(), &self.spaces, request).await
    }

    /// Plan, journal and run the whole move.
    async fn run(&mut self, request: &MoveRequest) -> (String, MovePlan) {
        let mut plan = self.plan(request).await.expect("plan");
        let id = self
            .instance
            .journal_begin(crossmove::OP, &serde_json::to_value(&plan).unwrap())
            .unwrap();
        let client = self.client();
        crossmove::execute(
            &self.instance,
            &id,
            &client,
            &mut self.spaces,
            &mut plan,
            Step::Planned,
            2,
        )
        .await
        .expect("move");
        (id, plan)
    }

    fn clean(&self, key: &str) -> bool {
        worktree::scan(self.ws(key)).unwrap().pages.iter().all(|s| s.is_clean())
    }
}

#[tokio::test]
async fn a_subtree_moves_with_its_ids_comments_and_attachments() {
    let mut h = Harness::new().await;
    let comments_before = h.ws("DOCS").state().page_comments("1002").unwrap().len();
    let attachments_before = h.ws("DOCS").state().page_attachments("1003").unwrap().len();
    assert_eq!((comments_before, attachments_before), (1, 1));

    let (id, plan) = h.run(&h.request("1002", "2002")).await;

    // The server moved the root, and its child with it.
    assert_eq!(h.mock.page_space("1002").as_deref(), Some("ENG"));
    assert_eq!(h.mock.page_space("1003").as_deref(), Some("ENG"));
    assert_eq!(h.mock.page_parent("1002").as_deref(), Some("2002"));
    assert!(h.mock.calls().iter().any(|c| c == "move_page_to:1002:append:2002"));

    // Same ids, now tracked only in the destination, with everything they had.
    for page in ["1002", "1003"] {
        assert!(h.ws("DOCS").state().get_page(page).unwrap().is_none(), "{page} left DOCS");
        assert!(h.ws("ENG").state().get_page(page).unwrap().is_some(), "{page} is in ENG");
    }
    assert_eq!(h.ws("ENG").state().page_comments("1002").unwrap().len(), comments_before);
    assert_eq!(h.ws("ENG").state().page_attachments("1003").unwrap().len(), attachments_before);

    // Files and sidecars exist only under the destination.
    let moved = "Engineering Home/Runbooks/Onboarding.md";
    let child = "Engineering Home/Runbooks/Onboarding/Week One.md";
    assert!(h.ws("ENG").absolute(moved).exists());
    assert!(h.ws("ENG").absolute(child).exists());
    assert!(h
        .ws("ENG")
        .absolute("Engineering Home/Runbooks/Onboarding/.Week One/plan.txt")
        .exists());
    assert!(!h.ws("DOCS").absolute("Team Handbook/Onboarding.md").exists());
    assert!(
        !h.ws("DOCS").absolute("Team Handbook/Onboarding").exists(),
        "the empty directory went too"
    );

    let file = h.read("ENG", moved);
    assert!(file.contains("space_key: ENG"), "{file}");
    assert!(file.contains("parent_id: '2002'"), "{file}");
    assert!(h.read("ENG", child).contains("План работ"));

    // The link to a page left behind now names its space; the one inside the
    // moved tree is still a file link.
    assert!(
        file.contains(&format!("[Team Handbook]({BASE}/display/DOCS/Team%20Handbook)")),
        "{file}"
    );
    assert!(file.contains("[Week One](<Onboarding/Week One.md>)"), "{file}");

    // Pages that linked to the moved ones still resolve on the server.
    assert!(h
        .mock
        .page_body("1004")
        .unwrap()
        .contains(r#"ri:space-key="ENG" ri:content-title="Onboarding""#));
    assert!(h
        .mock
        .page_body("2002")
        .unwrap()
        .contains(r#"ri:space-key="ENG" ri:content-title="Week One""#));
    assert!(h
        .mock
        .page_body("1002")
        .unwrap()
        .contains(r#"ri:space-key="DOCS" ri:content-title="Team Handbook""#));
    // …and in the files: an absolute link from the space left, a file link
    // from the space arrived in.
    let policies = h.read("DOCS", "Team Handbook/Policies.md");
    assert!(policies.contains(&format!("{BASE}/display/ENG/Onboarding")), "{policies}");
    let runbooks = h.read("ENG", "Engineering Home/Runbooks.md");
    assert!(runbooks.contains("Runbooks/Onboarding/Week"), "{runbooks}");

    assert!(h.clean("DOCS") && h.clean("ENG"), "both workspaces are clean");
    assert_eq!(h.instance.journal_entry(&id).unwrap().unwrap().state, Step::Done);
    assert!(crossmove::duplicates(&h.spaces).unwrap().is_empty());

    let ids: Vec<&str> = plan.moved.iter().map(|m| m.page_id.as_str()).collect();
    assert_eq!(ids, ["1002", "1003"]);
    assert_eq!(plan.links_rewritten.len(), 3, "{:?}", plan.links_rewritten);
}

#[tokio::test]
async fn links_the_server_fixed_itself_are_left_to_it() {
    let mut h = Harness::new().await;
    h.mock.refactor_links_on_move(true);
    let (_, plan) = h.run(&h.request("1002", "2002")).await;
    assert!(plan.links_rewritten.is_empty(), "{:?}", plan.links_rewritten);
    assert_eq!(plan.links_by_server.len(), 3);
    assert!(plan.warnings.iter().any(|w| w.contains("already changed the links")));
    assert!(h.clean("DOCS") && h.clean("ENG"));
    assert!(h.read("DOCS", "Team Handbook/Policies.md").contains("/display/ENG/Onboarding"));
}

#[tokio::test]
async fn an_interrupted_move_is_finished_by_running_it_again() {
    let mut h = Harness::new().await;
    let mut plan = h.plan(&h.request("1002", "2002")).await.unwrap();
    let id =
        h.instance.journal_begin(crossmove::OP, &serde_json::to_value(&plan).unwrap()).unwrap();
    let client = h.client();

    // The process dies after the server moved: here, the destination is not
    // available to write to.
    let eng = h.spaces.remove("ENG").unwrap();
    let err =
        crossmove::execute(&h.instance, &id, &client, &mut h.spaces, &mut plan, Step::Planned, 2)
            .await
            .unwrap_err();
    assert!(err.to_string().contains("ENG"), "{err}");
    let entry = h.instance.journal_entry(&id).unwrap().unwrap();
    assert_eq!(entry.state, Step::ServerMoved);
    assert!(entry.error.is_some());
    assert_eq!(h.mock.page_space("1002").as_deref(), Some("ENG"));
    h.spaces.insert("ENG".into(), eng);

    // Resume from the journal, as a new process would.
    let mut plan: MovePlan = serde_json::from_value(entry.plan).unwrap();
    let moves_before = h.mock.calls().iter().filter(|c| c.starts_with("move_page_to")).count();
    crossmove::execute(&h.instance, &id, &client, &mut h.spaces, &mut plan, entry.state, 2)
        .await
        .unwrap();
    let moves_after = h.mock.calls().iter().filter(|c| c.starts_with("move_page_to")).count();
    assert_eq!(moves_before, moves_after, "the server is not asked to move twice");

    assert!(h.ws("ENG").absolute("Engineering Home/Runbooks/Onboarding.md").exists());
    assert!(h.ws("DOCS").state().get_page("1002").unwrap().is_none());
    assert!(h.clean("DOCS") && h.clean("ENG"));
    assert!(h.instance.journal_unfinished().unwrap().is_empty());
}

#[tokio::test]
async fn a_resume_before_the_server_moved_asks_the_server_first() {
    let mut h = Harness::new().await;
    let mut plan = h.plan(&h.request("1002", "2002")).await.unwrap();
    let id =
        h.instance.journal_begin(crossmove::OP, &serde_json::to_value(&plan).unwrap()).unwrap();
    // Moved, but the journal never heard: the process died before recording it.
    h.mock.move_page_to(&"1002".into(), MovePosition::Append, &"2002".into()).await.unwrap();
    let client = h.client();
    crossmove::execute(&h.instance, &id, &client, &mut h.spaces, &mut plan, Step::Planned, 2)
        .await
        .unwrap();
    let moves = h.mock.calls().iter().filter(|c| c.starts_with("move_page_to")).count();
    assert_eq!(moves, 1);
    assert!(h.clean("DOCS") && h.clean("ENG"));
}

#[tokio::test]
async fn a_dirty_page_a_taken_title_and_the_home_page_are_all_refused_together() {
    let mut h = Harness::new().await;
    // A local edit under the moved tree…
    let week = h.ws("DOCS").absolute("Team Handbook/Onboarding/Week One.md");
    let mut text = std::fs::read_to_string(&week).unwrap();
    text.push_str("\nAn unpushed line.\n");
    std::fs::write(&week, text).unwrap();
    // …and a title the destination already has.
    h.mock.seed_page_in("ENG", "2003", "Onboarding", Some("2001"), "<p>Ours.</p>");
    h.pull("ENG").await;

    let err = h.plan(&h.request("1002", "2002")).await.unwrap_err();
    assert_eq!(err.exit_code(), confed_core::ExitCode::State);
    let message = err.to_string();
    assert!(message.contains("Week One.md: has local changes"), "{message}");
    assert!(message.contains(r#"title "Onboarding""#), "{message}");
    assert!(
        h.mock.mutating_calls().is_empty(),
        "nothing was written: {:?}",
        h.mock.mutating_calls()
    );

    let err = h.plan(&h.request("1001", "2002")).await.unwrap_err();
    assert!(err.to_string().contains("home page of space DOCS"), "{err}");
}

#[tokio::test]
async fn a_taken_title_can_take_the_source_key_instead() {
    let mut h = Harness::new().await;
    h.mock.seed_page_in("ENG", "2003", "Onboarding", Some("2001"), "<p>Ours.</p>");
    h.pull("ENG").await;
    let mut request = h.request("1002", "2002");
    request.on_conflict = OnConflict::Suffix;
    let (_, plan) = h.run(&request).await;
    assert_eq!(plan.pages[0].new_title.as_deref(), Some("Onboarding (DOCS)"));
    assert_eq!(h.mock.page_title("1002").as_deref(), Some("Onboarding (DOCS)"));
    // The link from the page left behind follows the new title.
    assert!(h.mock.page_body("1004").unwrap().contains(r#"ri:content-title="Onboarding (DOCS)""#));
    assert!(h.ws("ENG").absolute("Engineering Home/Runbooks/Onboarding (DOCS).md").exists());
    assert!(h.clean("DOCS") && h.clean("ENG"));
}

#[tokio::test]
async fn restrictions_and_permissions_are_reported_before_anything_moves() {
    let h = Harness::new().await;
    h.mock.restrict_page("1003", &["read"]);
    let plan = h.plan(&h.request("1002", "2002")).await.unwrap();
    assert!(plan.warnings.iter().any(|w| w.contains("read restrictions")), "{:?}", plan.warnings);
    assert_eq!(plan.pages[1].restricted, ["read"]);

    h.mock.deny("2002", "create");
    let err = h.plan(&h.request("1002", "2002")).await.unwrap_err();
    assert!(err.to_string().contains("may not add pages in space ENG"), "{err}");
}

#[tokio::test]
async fn a_server_that_refuses_the_move_leaves_everything_as_it_was() {
    let mut h = Harness::new().await;
    for (status, code) in
        [(403, confed_core::ExitCode::Auth), (409, confed_core::ExitCode::Conflict)]
    {
        h.mock.refuse_moves(Some(status));
        let mut plan = h.plan(&h.request("1002", "2002")).await.unwrap();
        let id =
            h.instance.journal_begin(crossmove::OP, &serde_json::to_value(&plan).unwrap()).unwrap();
        let client = h.client();
        let err = crossmove::execute(
            &h.instance,
            &id,
            &client,
            &mut h.spaces,
            &mut plan,
            Step::Planned,
            2,
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), code, "{status}: {err}");
        assert_eq!(h.instance.journal_entry(&id).unwrap().unwrap().state, Step::Planned);
        assert!(h.ws("DOCS").state().get_page("1002").unwrap().is_some());
        // Still before the server moved: abort is allowed, and closes it.
        crossmove::abort(&h.instance, &id, &client, &mut plan, Step::Planned).await.unwrap();
    }
    assert!(h.instance.journal_unfinished().unwrap().is_empty());

    // A server that cannot move: the plan is made, so a dry run can show it,
    // but it says why it cannot run, and running it is exit 9 — never a copy
    // and delete.
    h.mock.refuse_moves(None);
    h.mock.without_move_endpoint(true);
    let mut plan = h.plan(&h.request("1002", "2002")).await.unwrap();
    assert!(plan.blocked.as_deref().unwrap().contains("cannot move pages between spaces"));
    assert_eq!(plan.pages.len(), 2);
    let id =
        h.instance.journal_begin(crossmove::OP, &serde_json::to_value(&plan).unwrap()).unwrap();
    let client = h.client();
    let err =
        crossmove::execute(&h.instance, &id, &client, &mut h.spaces, &mut plan, Step::Planned, 2)
            .await
            .unwrap_err();
    assert_eq!(err.exit_code(), confed_core::ExitCode::Unsupported);
    assert!(!h
        .mock
        .calls()
        .iter()
        .any(|c| c.starts_with("create_page") || c.starts_with("delete_page")));
}

#[tokio::test]
async fn abort_is_refused_once_the_server_has_moved() {
    let mut h = Harness::new().await;
    let (id, mut plan) = h.run(&h.request("1002", "2002")).await;
    let client = h.client();
    let err = crossmove::abort(&h.instance, &id, &client, &mut plan, Step::ServerMoved)
        .await
        .unwrap_err();
    assert!(err.hint().unwrap().contains("confed instance mv"), "{err:?}");
}

#[tokio::test]
async fn state_rows_and_the_cache_move_without_downloading_anything() {
    let mut h = Harness::new().await;
    let listings_before = h.mock.attachment_listings().len();
    let cached = h.ws("DOCS").page_store().unwrap().versions("1003").unwrap();
    assert!(!cached.is_empty());

    let (_, _) = h.run(&h.request("1002", "2002")).await;
    assert_eq!(h.ws("ENG").page_store().unwrap().versions("1003").unwrap(), cached);
    assert!(h.ws("DOCS").page_store().unwrap().versions("1003").unwrap().is_empty());
    let record = h.ws("ENG").state().get_page("1003").unwrap().unwrap();
    assert_eq!(record.local_path, "Engineering Home/Runbooks/Onboarding/Week One.md");
    assert!(!record.render_key.is_empty(), "the closing pull rendered it again");
    // The attachment file came across; nothing had to be downloaded again.
    let downloaded = h.mock.attachment_listings().len() - listings_before;
    assert!(downloaded <= 4, "listed {downloaded} times");
}

#[tokio::test]
async fn a_parent_id_in_another_space_is_refused_by_push() {
    let mut h = Harness::new().await;
    let path = "Team Handbook/Policies.md";
    let text = h.read("DOCS", path).replace("parent_id: '1001'", "parent_id: '2002'");
    std::fs::write(h.ws("DOCS").absolute(path), text).unwrap();

    let engine = h.engine("DOCS");
    let outcome = engine
        .push(h.spaces.get_mut("DOCS").unwrap(), &confed_core::sync::PushOptions::default())
        .await
        .unwrap();
    assert_eq!(outcome.cross_space.len(), 1, "{outcome:?}");
    assert!(outcome.pushed.is_empty());
    let reason = &outcome.cross_space[0].reason;
    assert!(reason.contains("space ENG") && reason.contains("confed instance mv"), "{reason}");
    assert!(outcome.skipped.iter().any(|s| s.path == path));
    assert!(!h.mock.calls().iter().any(|c| c == "update_page:1004"));
    assert_eq!(h.mock.page_parent("1004").as_deref(), Some("1001"));

    // A parent in this space is a move within it, as before.
    let text = h.read("DOCS", path).replace("parent_id: '2002'", "parent_id: '1002'");
    std::fs::write(h.ws("DOCS").absolute(path), text).unwrap();
    let outcome = engine
        .push(h.spaces.get_mut("DOCS").unwrap(), &confed_core::sync::PushOptions::default())
        .await
        .unwrap();
    assert!(outcome.cross_space.is_empty());
    assert_eq!(outcome.pushed.len(), 1);
}

#[tokio::test]
async fn abort_asks_the_server_and_refuses_a_move_the_journal_missed() {
    let h = Harness::new().await;
    let mut plan = h.plan(&h.request("1002", "2002")).await.unwrap();
    let id =
        h.instance.journal_begin(crossmove::OP, &serde_json::to_value(&plan).unwrap()).unwrap();
    // Moved, but killed before the journal heard of it.
    h.mock.move_page_to(&"1002".into(), MovePosition::Append, &"2002".into()).await.unwrap();
    let client = h.client();
    let err =
        crossmove::abort(&h.instance, &id, &client, &mut plan, Step::Planned).await.unwrap_err();
    assert!(err.hint().unwrap().contains("confed instance resume"), "{err:?}");
    assert_eq!(h.instance.journal_entry(&id).unwrap().unwrap().state, Step::Planned);
}

/// Where every page link in a page's server copy leads: `(space, title,
/// found)`.
async fn links_of(mock: &MockClient, page_id: &str) -> Vec<(String, String, bool)> {
    let space = mock.page_space(page_id).unwrap();
    let body = mock.page_body(page_id).unwrap();
    let moves = std::collections::HashMap::new();
    let ctx = crossmove::LinkContext {
        space_before: &space,
        space_after: &space,
        moves: &moves,
        relativize: false,
        exists: None,
    };
    let mut out = Vec::new();
    for link in crossmove::link_targets(&body, &ctx) {
        let (space, title) = link.reaches;
        let found = mock.find_page_by_title(&space, &title).await.unwrap().is_some();
        out.push((space, title, found));
    }
    out
}

#[tokio::test]
async fn a_move_the_server_finishes_later_is_waited_for_not_called_a_failure() {
    let mut h = Harness::new().await;
    h.mock.lag_moves(6);
    let (_, plan) = h.run(&h.request("1002", "2002")).await;
    assert_eq!(plan.moved.len(), 2);
    let moves = h.mock.calls().iter().filter(|c| c.starts_with("move_page_to")).count();
    assert_eq!(moves, 1);
    assert!(h.clean("DOCS") && h.clean("ENG"));
}

#[tokio::test]
async fn a_fetch_of_the_source_between_the_move_and_the_resume_strands_nothing() {
    let mut h = Harness::new().await;
    let mut plan = h.plan(&h.request("1002", "2002")).await.unwrap();
    let id =
        h.instance.journal_begin(crossmove::OP, &serde_json::to_value(&plan).unwrap()).unwrap();
    let client = h.client();
    // Stops after the server moved, before the destination is written.
    let eng = h.spaces.remove("ENG").unwrap();
    let _ =
        crossmove::execute(&h.instance, &id, &client, &mut h.spaces, &mut plan, Step::Planned, 2)
            .await
            .unwrap_err();
    h.spaces.insert("ENG".into(), eng);
    // Anything that fetches the source now sees the pages gone from it.
    let engine = h.engine("DOCS");
    engine
        .fetch(h.spaces.get_mut("DOCS").unwrap(), &confed_core::sync::FetchOptions::default())
        .await
        .unwrap();
    assert!(h.ws("DOCS").state().get_remote("1002").unwrap().unwrap().deleted);

    // One resume finishes it: no fetch of the destination first.
    let entry = h.instance.journal_entry(&id).unwrap().unwrap();
    let mut plan: MovePlan = serde_json::from_value(entry.plan).unwrap();
    crossmove::execute(&h.instance, &id, &client, &mut h.spaces, &mut plan, entry.state, 2)
        .await
        .unwrap();
    assert!(h.ws("DOCS").state().get_page("1002").unwrap().is_none());
    assert!(h.ws("ENG").absolute("Engineering Home/Runbooks/Onboarding.md").exists());
    assert!(h.clean("DOCS") && h.clean("ENG"));
}

#[tokio::test]
async fn there_and_back_every_link_still_reaches_its_page() {
    let mut h = Harness::new().await;
    // As Data Center 9.5.4 does: the move finishes late, and fixes the links
    // around it in place, with no new version.
    h.mock.refactor_links_silently(true);
    h.mock.lag_moves(3);
    // The moved page links to a page left behind both ways: plainly, by an
    // explicit key, and from inside a macro.
    h.mock.remote_edit(
        "1002",
        concat!(
            r#"<p><ac:link><ri:page ri:content-title="Team Handbook" /></ac:link></p>"#,
            r#"<p><ac:link><ri:page ri:space-key="DOCS" ri:content-title="Team Handbook" /></ac:link></p>"#,
            r#"<ac:structured-macro ac:name="info" ac:macro-id="9000000a-1"><ac:rich-text-body>"#,
            r#"<p><ac:link><ri:page ri:content-title="Team Handbook" /></ac:link></p>"#,
            r#"</ac:rich-text-body></ac:structured-macro>"#,
            r#"<p><ac:link><ri:page ri:content-title="Week One" /></ac:link></p>"#,
        ),
    );
    h.pull("DOCS").await;

    let there = h.request("1002", "2002");
    let (_, plan) = h.run(&there).await;
    // Each page named once, however many reasons it had to be checked.
    for warning in plan.warnings.iter().filter(|w| w.contains("without a new version")) {
        let list = warning.rsplit_once('(').map(|(_, l)| l).unwrap_or_default();
        let list = list.split(')').next().unwrap_or_default();
        let ids: Vec<&str> = list.split(", ").collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "{warning}");
    }
    assert!(
        plan.warnings.iter().any(|w| w.contains("without a new version")),
        "{:?}",
        plan.warnings
    );
    for page in ["1002", "1004", "2002"] {
        for (space, title, found) in links_of(&h.mock, page).await {
            assert!(
                found,
                "after the move, page {page} links to {space}:{title}, which is not there"
            );
        }
    }
    assert!(h.mock.page_body("1002").unwrap().contains(r#"ac:macro-id="9000000a-1""#));
    // The server's silent fix is in the file, not only on the server.
    let runbooks = h.read("ENG", "Engineering Home/Runbooks.md");
    assert!(runbooks.contains("Runbooks/Onboarding/Week"), "{runbooks}");

    // Back again: home under Team Handbook.
    let mut back = h.request("1002", "1001");
    back.source = SpaceRef { key: "ENG".into(), dir: "ENG".into(), numeric: Some("2001".into()) };
    back.dest = SpaceRef { key: "DOCS".into(), dir: "DOCS".into(), numeric: Some("1001".into()) };
    h.run(&back).await;
    for page in ["1002", "1004", "2002"] {
        for (space, title, found) in links_of(&h.mock, page).await {
            assert!(
                found,
                "after moving back, page {page} links to {space}:{title}, which is not there"
            );
        }
    }
    assert!(h.clean("DOCS") && h.clean("ENG"));
}

#[tokio::test]
async fn a_link_broken_before_the_move_is_left_alone_and_reported() {
    let mut h = Harness::new().await;
    // A page in ENG whose plain link to a DOCS page was never fixed: in ENG it
    // reaches nothing.
    h.mock.seed_page_in(
        "ENG",
        "2005",
        "Stray",
        Some("2001"),
        r#"<p><ac:link><ri:page ri:content-title="Team Handbook" /></ac:link></p>"#,
    );
    h.pull("ENG").await;
    let mut request = h.request("2005", "1001");
    request.source =
        SpaceRef { key: "ENG".into(), dir: "ENG".into(), numeric: Some("2001".into()) };
    request.dest =
        SpaceRef { key: "DOCS".into(), dir: "DOCS".into(), numeric: Some("1001".into()) };
    let plan = h.plan(&request).await.unwrap();
    assert!(plan.links.is_empty(), "nothing to rewrite: {:?}", plan.links);
    h.run(&request).await;
    // Untouched — and in DOCS it now reaches its page.
    assert_eq!(
        h.mock.page_body("2005").unwrap(),
        r#"<p><ac:link><ri:page ri:content-title="Team Handbook" /></ac:link></p>"#
    );
    assert!(links_of(&h.mock, "2005").await.iter().all(|(_, _, found)| *found));
}
