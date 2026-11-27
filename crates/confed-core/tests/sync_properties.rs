//! Random sequences of local edits, remote edits, pulls, pushes and
//! resolutions against the stateful mock, checked after every step against
//! what must always hold:
//!
//! 1. After a pull, a page that reads `unchanged` is based on the server's
//!    current version, and its file is that version's rendering.
//! 2. No body the server holds ever carries conflict markers.
//! 3. The base of every page is a version the server has, exactly as the
//!    server has it: the base only advances to a state the server confirmed.

use confed_api::{ConfluenceClient, Flavor, MockClient, PageId, SpaceId};
use confed_core::merge;
use confed_core::sync::{page_convert_options, PullOptions, PushOptions, SyncEngine};
use confed_core::workspace::Workspace;
use confed_core::worktree::{self, PageState};
use proptest::prelude::*;
use std::sync::Arc;

const PAGES: [&str; 3] = ["1001", "1002", "1003"];
const PARAGRAPHS: usize = 3;

#[derive(Clone, Debug)]
enum Step {
    /// Add a word to a paragraph of the page's file.
    LocalEdit {
        page: usize,
        paragraph: usize,
        word: &'static str,
    },
    /// Add a word to a paragraph of the page on the server.
    RemoteEdit {
        page: usize,
        paragraph: usize,
        word: &'static str,
    },
    Pull,
    Push,
    /// Resolve a conflicted page, keeping one side.
    Resolve {
        page: usize,
        ours: bool,
    },
}

fn step() -> impl Strategy<Value = Step> {
    let word = prop::sample::select(vec!["alpha", "beta", "gamma", "ещё"]);
    prop_oneof![
        3 => (0..PAGES.len(), 0..PARAGRAPHS, word.clone())
            .prop_map(|(page, paragraph, word)| Step::LocalEdit { page, paragraph, word }),
        3 => (0..PAGES.len(), 0..PARAGRAPHS, word)
            .prop_map(|(page, paragraph, word)| Step::RemoteEdit { page, paragraph, word }),
        2 => Just(Step::Pull),
        2 => Just(Step::Push),
        1 => (0..PAGES.len(), any::<bool>()).prop_map(|(page, ours)| Step::Resolve { page, ours }),
    ]
}

struct World {
    _dir: tempfile::TempDir,
    ws: Workspace,
    mock: Arc<MockClient>,
    engine: SyncEngine,
}

impl World {
    fn new(flavor: Flavor) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::create(dir.path()).unwrap();
        ws.state().set_meta("space_key", "DOCS").unwrap();
        ws.state().set_meta("base_url", "https://mock.test").unwrap();
        ws.state().set_meta("flavor", flavor.as_str()).unwrap();
        let mock = Arc::new(MockClient::new(flavor));
        for (i, id) in PAGES.iter().enumerate() {
            let body: String =
                (0..PARAGRAPHS).map(|j| format!("<p>Page {i} line {j}.</p>")).collect();
            mock.seed_page(id, &format!("Page {i}"), None, &body);
        }
        let engine = SyncEngine::new(
            mock.clone() as Arc<dyn ConfluenceClient>,
            SpaceId { key: "DOCS".into(), numeric: Some("1001".into()) },
            2,
        );
        Self { _dir: dir, ws, mock, engine }
    }

    fn file(&self, page: usize) -> String {
        self.ws.absolute(&format!("Page {page}.md")).to_string_lossy().into_owned()
    }

    async fn run(&mut self, step: &Step) {
        match step {
            Step::LocalEdit { page, paragraph, word } => {
                let path = self.file(*page);
                let Ok(text) = std::fs::read_to_string(&path) else { return };
                let needle = format!("Page {page} line {paragraph}");
                let edited: String = text
                    .split_inclusive('\n')
                    .map(|line| {
                        if line.contains(&needle) && !merge::has_conflict_markers(line) {
                            format!("{} {word}\n", line.trim_end_matches('\n'))
                        } else {
                            line.to_string()
                        }
                    })
                    .collect();
                std::fs::write(&path, edited).unwrap();
            }
            Step::RemoteEdit { page, paragraph, word } => {
                let id = PAGES[*page];
                let body = self.mock.page_body(id).unwrap();
                let needle = format!("Page {page} line {paragraph}");
                let Some(at) = body.find(&needle) else { return };
                let end = at + body[at..].find("</p>").unwrap();
                let edited = format!("{} {word}{}", &body[..end], &body[end..]);
                self.mock.remote_edit(id, &edited);
            }
            Step::Pull => {
                let _ = self.engine.pull(&mut self.ws, &PullOptions::everything()).await;
                self.check_after_pull();
            }
            Step::Push => {
                let _ = self.engine.push(&mut self.ws, &PushOptions::default()).await;
            }
            Step::Resolve { page, ours } => {
                let id = PAGES[*page];
                let Some(record) = self.ws.state().get_page(id).unwrap() else { return };
                if record.sync_state != confed_core::state::SyncState::Conflicted {
                    return;
                }
                let path = self.ws.absolute(&record.local_path);
                let original = std::fs::read_to_string(&path).unwrap();
                let resolved = confed_core::resolve::take_side(&original, *ours);
                let _ = confed_core::resolve::finish_resolution(
                    &mut self.ws,
                    &record,
                    &original,
                    &resolved,
                );
            }
        }
        self.check_always().await;
    }

    /// Invariant 1.
    fn check_after_pull(&self) {
        let scan = worktree::scan(&self.ws).unwrap();
        for status in scan.pages.iter().filter(|s| s.state == PageState::Unchanged) {
            let id = status.page_id.as_deref().unwrap();
            let base = self.ws.state().get_page(id).unwrap().unwrap();
            assert_eq!(
                Some(base.version),
                self.mock.page_version(id),
                "{}: unchanged after a pull, but not on the server's version",
                status.path
            );
            let text = std::fs::read_to_string(self.ws.absolute(&status.path)).unwrap();
            let file = confed_core::frontmatter::parse(&text, &status.path).unwrap();
            let opts = page_convert_options(&self.ws, &status.path).unwrap();
            let rendered =
                confed_converter::storage_to_markdown(&self.mock.page_body(id).unwrap(), &opts)
                    .unwrap();
            assert_eq!(
                file.body.trim_end(),
                rendered.markdown.trim_end(),
                "{}: unchanged after a pull, but not the server's version",
                status.path
            );
        }
    }

    /// Invariants 2 and 3.
    async fn check_always(&self) {
        for id in PAGES {
            let body = self.mock.page_body(id).unwrap();
            assert!(
                !merge::has_conflict_markers(&body),
                "page {id} on the server holds markers:\n{body}"
            );
            let Some(base) = self.ws.state().get_page(id).unwrap() else { continue };
            let at =
                self.mock.get_page_at_version(&PageId::new(id), base.version).await.unwrap_or_else(
                    |e| {
                        panic!("page {id}: base version {} is not on the server: {e}", base.version)
                    },
                );
            assert_eq!(
                base.storage_body, at.body_storage,
                "page {id}: the base of version {} is not what the server has",
                base.version
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn sync_keeps_its_invariants(
        flavor in prop_oneof![Just(Flavor::Cloud), Just(Flavor::DataCenter)],
        steps in prop::collection::vec(step(), 1..14),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut world = World::new(flavor);
            world.engine.pull(&mut world.ws, &PullOptions::everything()).await.unwrap();
            world.check_always().await;
            for step in &steps {
                world.run(step).await;
            }
        });
    }
}
