//! Ctrl-C between pages. In a file of its own: the request is process-wide,
//! and no other test may run while it is raised.

use confed_api::{Flavor, MockClient, SpaceId};
use confed_core::sync::{PullOptions, PushOptions, SyncEngine};
use confed_core::workspace::Workspace;
use confed_core::worktree::{self, PageState};
use confed_core::{interrupt, ExitCode};
use std::sync::Arc;

#[tokio::test]
async fn an_interrupt_stops_between_pages_and_leaves_each_page_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = Workspace::create(dir.path()).unwrap();
    ws.state().set_meta("space_key", "DOCS").unwrap();
    ws.state().set_meta("base_url", "https://wiki.example.test").unwrap();
    ws.state().set_meta("flavor", "dc").unwrap();
    let mock = Arc::new(MockClient::new(Flavor::DataCenter));
    mock.seed_page("1001", "Team Handbook", None, "<p>Handbook.</p>");
    mock.seed_page("1002", "Onboarding", None, "<p>Onboarding.</p>");
    let engine = SyncEngine::new(
        mock.clone() as Arc<dyn confed_api::ConfluenceClient>,
        SpaceId { key: "DOCS".into(), numeric: Some("1001".into()) },
        2,
    );
    engine.pull(&mut ws, &PullOptions::everything()).await.unwrap();

    // Both edited, then Ctrl-C before the push starts its first page.
    for file in ["Team Handbook.md", "Onboarding.md"] {
        let path = dir.path().join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}\nФраза 1.\n")).unwrap();
    }
    interrupt::request();
    let err = engine.push(&mut ws, &PushOptions::default()).await.unwrap_err();
    assert_eq!(err.exit_code(), ExitCode::Interrupted);
    assert_eq!(err.exit_code().as_i32(), 130);
    assert!(err.to_string().contains("0 of 2"), "{err}");
    assert!(mock.mutating_calls().is_empty(), "nothing was sent: {:?}", mock.mutating_calls());

    // The same for a pull: nothing is half-written, and the next run goes on.
    mock.remote_edit("1001", "<p>Changed on the server.</p>");
    let err = engine.pull(&mut ws, &PullOptions::everything()).await.unwrap_err();
    assert_eq!(err.exit_code(), ExitCode::Interrupted);
    interrupt::clear();

    let scan = worktree::scan(&ws).unwrap();
    assert_eq!(
        scan.find("1002").unwrap().state,
        PageState::Modified,
        "edit kept, not taken as pushed"
    );
    let outcome = engine.push(&mut ws, &PushOptions::default()).await.unwrap();
    assert_eq!(outcome.pushed.len(), 1, "the unchanged-on-server page goes up: {outcome:?}");
}
