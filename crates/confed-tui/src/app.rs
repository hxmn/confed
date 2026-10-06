//! Application state and key handling.
//!
//! Everything here is synchronous and testable: keys go in, state changes come
//! out. The only asynchronous part is [`Job`], which hands the workspace to the
//! tokio runtime for the duration of a fetch/pull/push and takes it back when the
//! operation finishes — so a slow server never blocks the event loop.

use crate::conflict::{Choice, Resolver};
use crate::pane::{self, DiffSide, Line as PaneLine, LineKind};
use crate::tree::{Row, Tree};
use confed_api::ConfluenceClient;
use confed_core::error::{ConfedError, Result};
use confed_core::sync::{FetchOptions, PullOptions, PushOptions, PushPlan, SyncEngine};
use confed_core::workspace::Workspace;
use confed_core::worktree::{self, PageState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What the UI is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// Typing into the tree filter.
    Filter,
    Help,
    /// Showing the push plan, waiting for a yes or no.
    PushPlan,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Preview,
    Diff,
}

/// A long operation running on the tokio runtime.
///
/// The sync engine's futures are not `Send` — they borrow the SQLite connection
/// across awaits — so a job runs on its own thread that drives the runtime
/// through a handle, rather than as a spawned task. Cancellation is a `select!`
/// against a oneshot: the workspace always comes back, cancelled or not.
pub struct Job {
    pub label: &'static str,
    pub started: Instant,
    cancelled: bool,
    rx: Receiver<Finished>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

struct Finished {
    ws: Workspace,
    outcome: std::result::Result<String, String>,
}

enum Work {
    Fetch,
    Pull(Box<PullOptions>),
    Push(Box<PushOptions>),
}

pub struct App {
    pub space: String,
    pub tree: Tree,
    /// Index into the currently visible rows.
    pub cursor: usize,
    pub filter: String,
    pub mode: Mode,
    pub view: View,
    pub diff_side: DiffSide,
    pub content: Vec<PaneLine>,
    pub scroll: u16,
    pub status: String,
    pub plan: Option<PushPlan>,
    pub resolver: Option<Resolver>,
    pub job: Option<Job>,
    pub should_quit: bool,
    /// Ticks since start, for the spinner.
    pub frames: u64,
    /// Height of the content pane, so PageUp/PageDown can move a screenful.
    pub page_height: u16,

    root: PathBuf,
    ws: Option<Workspace>,
    engine: Option<Arc<SyncEngine>>,
    client: Option<Arc<dyn ConfluenceClient>>,
    runtime: Option<tokio::runtime::Handle>,
}

impl App {
    pub fn new(
        ws: Workspace,
        engine: Option<Arc<SyncEngine>>,
        client: Option<Arc<dyn ConfluenceClient>>,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Result<Self> {
        let space = ws.space_key().unwrap_or_else(|_| "?".to_string());
        let root = ws.root().to_path_buf();
        let mut app = Self {
            space,
            tree: Tree::default(),
            cursor: 0,
            filter: String::new(),
            mode: Mode::Browse,
            view: View::Preview,
            diff_side: DiffSide::Base,
            content: Vec::new(),
            scroll: 0,
            status: String::new(),
            plan: None,
            resolver: None,
            job: None,
            should_quit: false,
            frames: 0,
            page_height: 20,
            root,
            ws: Some(ws),
            engine,
            client,
            runtime,
        };
        app.reload()?;
        if app.engine.is_none() {
            app.status =
                "Offline: no credentials for this workspace, so fetch/pull/push are disabled."
                    .into();
        }
        Ok(app)
    }

    #[cfg(test)]
    pub fn workspace(&self) -> Option<&Workspace> {
        self.ws.as_ref()
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Rescan the working tree and rebuild the page tree, keeping the selection.
    pub fn reload(&mut self) -> Result<()> {
        let Some(ws) = &self.ws else { return Ok(()) };
        let scan = worktree::scan(ws)?;
        let base = ws.state().all_pages()?;
        let remote = ws.state().all_remote()?;

        let selected_path = self.selected().map(|row| self.tree.nodes[row.node].path.clone());
        let mut tree = Tree::build(&scan, &base, &remote);
        tree.carry_over(&self.tree);
        self.tree = tree;

        if let Some(path) = selected_path {
            let rows = self.rows();
            if let Some(index) = rows.iter().position(|r| self.tree.nodes[r.node].path == path) {
                self.cursor = index;
            }
        }
        self.clamp_cursor();
        self.refresh_content();
        Ok(())
    }

    pub fn rows(&self) -> Vec<Row> {
        self.tree.rows(&self.filter)
    }

    pub fn selected(&self) -> Option<Row> {
        self.rows().get(self.cursor).copied()
    }

    pub fn selected_node(&self) -> Option<&crate::tree::Node> {
        self.selected().map(|row| &self.tree.nodes[row.node])
    }

    fn clamp_cursor(&mut self) {
        let len = self.rows().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// Counts for the header: how much is waiting to move in each direction.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut outgoing = 0;
        let mut incoming = 0;
        let mut conflicted = 0;
        for node in &self.tree.nodes {
            if node.state == PageState::Conflicted {
                conflicted += 1;
            }
            if node.state.has_local_work() {
                outgoing += 1;
            }
            if node.state.has_remote_work() {
                incoming += 1;
            }
        }
        (outgoing, incoming, conflicted)
    }

    pub fn refresh_content(&mut self) {
        self.scroll = 0;
        let (Some(ws), Some(node)) = (self.ws.as_ref(), self.selected_node()) else {
            self.content = Vec::new();
            return;
        };
        let (path, page_id) = (node.path.clone(), node.page_id.clone());
        self.content = match (self.view, page_id) {
            (View::Preview, _) => pane::preview(ws, &path),
            (View::Diff, Some(id)) => pane::diff(ws, &id, &path, self.diff_side),
            (View::Diff, None) => vec![PaneLine::new(
                LineKind::Warn,
                "This page has never been pushed, so there is nothing to diff against.",
            )],
        };
    }

    // ---- keys -------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        // Ctrl-C always wins, whatever mode we are in.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Browse,
            Mode::Filter => self.filter_key(key),
            Mode::PushPlan => self.plan_key(key),
            Mode::Conflict => self.conflict_key(key),
            Mode::Browse => self.browse_key(key),
        }
    }

    fn browse_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                if self.busy() {
                    self.cancel_job();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.clamp_cursor();
                    self.refresh_content();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
            KeyCode::Home => self.set_cursor(0),
            KeyCode::End => self.set_cursor(isize::MAX),
            KeyCode::Char('g') => self.set_cursor(0),
            KeyCode::Char('G') => self.set_cursor(isize::MAX),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_expand(),
            KeyCode::Right | KeyCode::Char('l') => self.set_expanded(true),
            KeyCode::Left | KeyCode::Char('h') => self.set_expanded(false),
            KeyCode::Char('E') => self.expand_all(true),
            KeyCode::Char('C') => self.expand_all(false),
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.filter.clear();
            }
            KeyCode::Char('d') => {
                self.view = if self.view == View::Diff { View::Preview } else { View::Diff };
                self.refresh_content();
            }
            KeyCode::Char('r') => {
                self.diff_side = if self.diff_side == DiffSide::Base {
                    DiffSide::Remote
                } else {
                    DiffSide::Base
                };
                self.view = View::Diff;
                self.refresh_content();
            }
            KeyCode::PageDown => self.scroll_content(self.page_height as i32),
            KeyCode::PageUp => self.scroll_content(-(self.page_height as i32)),
            KeyCode::Char('n') => self.jump_hunk(true),
            KeyCode::Char('N') => self.jump_hunk(false),
            KeyCode::Char('R') => self.status = self.reload_status(),
            KeyCode::Char('f') => self.start_fetch(),
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('P') => self.open_push_plan(),
            KeyCode::Char('c') => self.open_resolver(),
            KeyCode::Char('o') => self.open_in_browser(),
            _ => {}
        }
    }

    fn filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Browse;
                self.clamp_cursor();
                self.refresh_content();
            }
            KeyCode::Enter => {
                self.mode = Mode::Browse;
                self.refresh_content();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.cursor = 0;
                self.refresh_content();
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.cursor = 0;
                self.refresh_content();
            }
            _ => {}
        }
    }

    fn plan_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => self.confirm_push(),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                self.plan = None;
                self.mode = Mode::Browse;
                self.status = "Push cancelled.".into();
            }
            _ => {}
        }
    }

    fn conflict_key(&mut self, key: KeyEvent) {
        let Some(resolver) = self.resolver.as_mut() else {
            self.mode = Mode::Browse;
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.resolver = None;
                self.mode = Mode::Browse;
                self.status = "Left the resolver; nothing was written.".into();
            }
            KeyCode::Char('u') | KeyCode::Char('1') => resolver.choose(Choice::Ours),
            KeyCode::Char('t') | KeyCode::Char('2') => resolver.choose(Choice::Theirs),
            KeyCode::Char('U') => resolver.choose_all(Choice::Ours),
            KeyCode::Char('T') => resolver.choose_all(Choice::Theirs),
            KeyCode::Char('n') | KeyCode::Tab => resolver.next(),
            KeyCode::Char('N') | KeyCode::BackTab => resolver.previous(),
            KeyCode::Char('j') | KeyCode::Down => {
                resolver.scroll = resolver.scroll.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up => resolver.scroll = resolver.scroll.saturating_sub(1),
            KeyCode::Char('w') | KeyCode::Enter => self.commit_resolution(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        self.set_cursor(self.cursor as isize + delta);
    }

    fn set_cursor(&mut self, target: isize) {
        let len = self.rows().len() as isize;
        if len == 0 {
            return;
        }
        self.cursor = target.clamp(0, len - 1) as usize;
        self.refresh_content();
    }

    fn scroll_content(&mut self, delta: i32) {
        let max = self.content.len().saturating_sub(1) as i32;
        self.scroll = (self.scroll as i32 + delta).clamp(0, max.max(0)) as u16;
    }

    /// Scroll to the next `@@` header, wrapping at the end of the diff.
    fn jump_hunk(&mut self, forward: bool) {
        let headers: Vec<u16> = self
            .content
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind == LineKind::Hunk)
            .map(|(index, _)| index as u16)
            .collect();
        let (Some(&first), Some(&last)) = (headers.first(), headers.last()) else {
            self.status = "No hunks here — press d for the diff.".into();
            return;
        };
        self.scroll = if forward {
            headers.iter().copied().find(|&i| i > self.scroll).unwrap_or(first)
        } else {
            headers.iter().rev().copied().find(|&i| i < self.scroll).unwrap_or(last)
        };
    }

    fn toggle_expand(&mut self) {
        if let Some(row) = self.selected() {
            let expanded = self.tree.nodes[row.node].expanded;
            self.tree.set_expanded(row.node, !expanded);
            self.clamp_cursor();
        }
    }

    fn expand_all(&mut self, expanded: bool) {
        self.tree.set_all_expanded(expanded);
        self.clamp_cursor();
    }

    fn set_expanded(&mut self, expanded: bool) {
        if let Some(row) = self.selected() {
            self.tree.set_expanded(row.node, expanded);
            self.clamp_cursor();
        }
    }

    fn reload_status(&mut self) -> String {
        match self.reload() {
            Ok(()) => "Refreshed.".to_string(),
            Err(e) => format!("Refresh failed: {e}"),
        }
    }

    // ---- actions ----------------------------------------------------------

    fn open_in_browser(&mut self) {
        let Some(node) = self.selected_node() else { return };
        let Some(page_id) = node.page_id.clone() else {
            self.status = "That page has no id yet — push it first.".into();
            return;
        };
        let url = match (&self.client, self.ws.as_ref()) {
            (Some(client), _) => client.page_url(&confed_api::PageId::new(&page_id), &self.space),
            // Without credentials confed still knows where the site lives.
            (None, Some(ws)) => {
                let base = ws.base_url().ok().flatten().unwrap_or_default();
                format!("{base}/spaces/{}/pages/{page_id}", self.space)
            }
            (None, None) => return,
        };
        self.status = match open_url(&url) {
            Ok(()) => format!("Opened {url}"),
            Err(e) => format!("{e}"),
        };
    }

    fn open_resolver(&mut self) {
        let Some(node) = self.selected_node() else { return };
        if node.state != PageState::Conflicted {
            self.status = "Only a conflicted page can be resolved (badge C).".into();
            return;
        }
        let (path, page_id) = (node.path.clone(), node.page_id.clone().unwrap_or_default());
        let Some(ws) = &self.ws else { return };
        let content = match std::fs::read_to_string(ws.absolute(&path)) {
            Ok(content) => content,
            Err(e) => {
                self.status = format!("{path}: {e}");
                return;
            }
        };
        match Resolver::new(&page_id, &path, &content) {
            Some(resolver) => {
                self.status = format!("{} conflict(s) in {path}", resolver.hunk_count());
                self.resolver = Some(resolver);
                self.mode = Mode::Conflict;
            }
            None => {
                self.status =
                    "No conflict markers left in this file — run `confed resolve` to clear it."
                        .into();
            }
        }
    }

    fn commit_resolution(&mut self) {
        let Some(resolver) = &self.resolver else { return };
        if resolver.unresolved() > 0 {
            self.status = format!(
                "{} hunk(s) still unresolved — press u or t on each.",
                resolver.unresolved()
            );
            return;
        }
        let resolved = resolver.render();
        let (page_id, path, original) =
            (resolver.page_id.clone(), resolver.path.clone(), resolver.original.clone());

        let Some(ws) = self.ws.as_mut() else { return };
        let record = match ws.state().get_page(&page_id) {
            Ok(Some(record)) => record,
            Ok(None) => {
                self.status = format!("no state record for {path}");
                return;
            }
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };

        match confed_cli::commands::resolve::finish_resolution(ws, &record, &original, &resolved) {
            Ok(markers) if markers.is_empty() => {
                self.resolver = None;
                self.mode = Mode::Browse;
                self.status = format!("Resolved {path}. Press P to push it.");
                let _ = self.reload();
            }
            Ok(markers) => {
                self.status = format!("Still conflicted at line(s) {markers:?}");
            }
            Err(e) => self.status = format!("Could not resolve {path}: {e}"),
        }
    }

    fn open_push_plan(&mut self) {
        let Some(engine) = self.engine.clone() else {
            self.status = "Not connected — `confed init` stores credentials.".into();
            return;
        };
        let Some(ws) = &self.ws else {
            self.status = "Busy — wait for the current operation.".into();
            return;
        };
        let options = self.push_options();
        match engine.plan_push(ws, &options) {
            Ok(plan) if plan.is_empty() => self.status = "Nothing to push.".into(),
            Ok(plan) => {
                self.plan = Some(plan);
                self.mode = Mode::PushPlan;
            }
            Err(e) => self.status = format!("Could not plan the push: {e}"),
        }
    }

    /// Push is scoped to the selected page when one is highlighted; that keeps
    /// an accidental keypress from uploading the whole space.
    fn push_options(&self) -> PushOptions {
        let scope = self
            .selected_node()
            .filter(|node| !node.path.is_empty() && node.state != PageState::RemoteNew)
            .map(|node| vec![node.path.clone()])
            .unwrap_or_default();
        PushOptions { scope, with_attachments: true, with_comments: true, ..Default::default() }
    }

    fn confirm_push(&mut self) {
        self.plan = None;
        self.mode = Mode::Browse;
        let options = self.push_options();
        self.spawn("push", Work::Push(Box::new(options)));
    }

    fn start_fetch(&mut self) {
        self.spawn("fetch", Work::Fetch);
    }

    fn start_pull(&mut self) {
        let scope = self
            .selected_node()
            .and_then(|node| node.page_id.clone())
            .map(|id| vec![id])
            .unwrap_or_default();
        let options = PullOptions { scope, ..PullOptions::everything() };
        self.spawn("pull", Work::Pull(Box::new(options)));
    }

    // ---- the async side ---------------------------------------------------

    fn spawn(&mut self, label: &'static str, work: Work) {
        if self.busy() {
            self.status = format!("Already running {}.", self.job.as_ref().unwrap().label);
            return;
        }
        let (Some(engine), Some(runtime)) = (self.engine.clone(), self.runtime.clone()) else {
            self.status = "Not connected — `confed init` stores credentials.".into();
            return;
        };
        let Some(mut ws) = self.ws.take() else {
            self.status = "The workspace is busy.".into();
            return;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let spawned =
            std::thread::Builder::new().name(format!("confed-tui-{label}")).spawn(move || {
                let outcome = runtime.block_on(async {
                    tokio::select! {
                        result = run_work(&engine, &mut ws, work) => {
                            result.map_err(|e| e.to_string())
                        }
                        _ = cancel_rx => Ok(format!("Cancelled {label}.")),
                    }
                });
                let _ = tx.send(Finished { ws, outcome });
            });

        if let Err(e) = spawned {
            self.status = format!("Could not start {label}: {e}");
            // The workspace never left, so put it back where it came from.
            self.ws = Workspace::open(&self.root).ok();
            return;
        }
        self.status = format!("Running {label}…");
        self.job = Some(Job {
            label,
            started: Instant::now(),
            cancelled: false,
            rx,
            cancel: Some(cancel_tx),
        });
    }

    /// Ask the operation to stop at its next await point. Whatever it already
    /// wrote stays written — the engine's own steps are individually atomic.
    fn cancel_job(&mut self) {
        if let Some(job) = &mut self.job {
            if let Some(cancel) = job.cancel.take() {
                let _ = cancel.send(());
                job.cancelled = true;
                self.status = format!("Cancelling {}…", job.label);
            }
        }
    }

    /// Called once per event-loop tick: collect a finished job, if any.
    pub fn poll_jobs(&mut self) {
        self.frames += 1;
        let Some(job) = &self.job else { return };
        let label = job.label;

        match job.rx.try_recv() {
            Ok(Finished { ws, outcome }) => {
                self.ws = Some(ws);
                self.job = None;
                self.status = match outcome {
                    Ok(message) => message,
                    Err(e) => format!("{label} failed: {e}"),
                };
                // A pull or a push may have moved the rules page, as it would
                // have from the command line.
                if let Some(ws) = &self.ws {
                    match confed_cli::commands::agent_docs::sync_rules(ws) {
                        Ok(sync) if sync.written.is_empty() => {}
                        Ok(sync) => self.status.push_str(&format!(
                            " The rules in {} were updated.",
                            sync.written.join(" and ")
                        )),
                        Err(e) => self.status.push_str(&format!(" (rules not refreshed: {e})")),
                    }
                }
                if let Err(e) = self.reload() {
                    self.status = format!("{} (refresh failed: {e})", self.status);
                }
            }
            Err(TryRecvError::Empty) => {}
            // The worker died without reporting: re-open the workspace so the
            // TUI stays usable instead of freezing with no state.
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                self.status = format!("{label} stopped unexpectedly.");
                self.ws = Workspace::open(&self.root).ok();
                let _ = self.reload();
            }
        }
    }

    /// Stop a running job and wait briefly for the workspace to come back, so
    /// the runtime is not torn down underneath a live SQLite transaction.
    pub fn shutdown(&mut self) {
        if !self.busy() {
            return;
        }
        self.cancel_job();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.busy() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            self.poll_jobs();
        }
    }

    /// How long the current job has been running, for the progress line.
    pub fn job_elapsed(&self) -> Duration {
        self.job.as_ref().map(|j| j.started.elapsed()).unwrap_or_default()
    }
}

#[cfg(test)]
impl App {
    /// An app with no workspace behind it, for tests that only render.
    pub fn for_render(space: &str, tree: Tree, content: Vec<PaneLine>) -> Self {
        Self {
            space: space.to_string(),
            tree,
            cursor: 0,
            filter: String::new(),
            mode: Mode::Browse,
            view: View::Preview,
            diff_side: DiffSide::Base,
            content,
            scroll: 0,
            status: String::new(),
            plan: None,
            resolver: None,
            job: None,
            should_quit: false,
            frames: 0,
            page_height: 20,
            root: PathBuf::new(),
            ws: None,
            engine: None,
            client: None,
            runtime: None,
        }
    }
}

async fn run_work(engine: &SyncEngine, ws: &mut Workspace, work: Work) -> Result<String> {
    let _lock = ws.lock()?;
    match work {
        Work::Fetch => {
            let outcome = engine.fetch(ws, &FetchOptions::default()).await?;
            Ok(format!("Fetched {} page(s), {} unchanged.", outcome.fetched, outcome.unchanged))
        }
        Work::Pull(options) => {
            let outcome = engine.pull(ws, &options).await?;
            let warnings = match outcome.warnings.len() {
                0 => String::new(),
                n => format!(" {n} warning(s): `confed pull` shows them."),
            };
            Ok(format!(
                "Pulled: {} created, {} updated, {} merged, {} conflicted.{warnings}",
                outcome.created.len(),
                outcome.updated.len(),
                outcome.merged.len(),
                outcome.conflicted.len()
            ))
        }
        Work::Push(options) => {
            let outcome = engine.push(ws, &options).await?;
            Ok(format!(
                "Pushed: {} updated, {} created, {} deleted, {} skipped.",
                outcome.pushed.len(),
                outcome.created.len(),
                outcome.deleted.len(),
                outcome.skipped.len()
            ))
        }
    }
}

fn open_url(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ConfedError::io(format!("launching {opener}"), e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::LineKind;
    use confed_api::{Flavor, MockClient, SpaceId};
    use confed_core::worktree::PageState;
    use std::time::Duration;

    /// A real workspace against the stateful mock server, driven only by key
    /// presses — the same path a person takes.
    struct Harness {
        app: App,
        mock: Arc<MockClient>,
        runtime: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    impl Harness {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let mut ws = Workspace::create(dir.path()).expect("workspace");
            ws.state().set_meta("space_key", "DOCS").unwrap();
            ws.state().set_meta("base_url", "https://mock.test").unwrap();
            ws.state().set_meta("flavor", Flavor::Cloud.as_str()).unwrap();

            let mock = Arc::new(MockClient::new(Flavor::Cloud));
            mock.seed_page("1001", "Runbook", None, "<p>Original text.</p>");
            mock.seed_page("1002", "Failover", Some("1001"), "<p>Steps.</p>");

            let client: Arc<dyn ConfluenceClient> = mock.clone();
            let engine = Arc::new(SyncEngine::new(
                Arc::clone(&client),
                SpaceId { key: "DOCS".into(), numeric: Some("1001".into()) },
                2,
            ));

            let runtime =
                tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
            runtime
                .block_on(engine.pull(&mut ws, &PullOptions::everything()))
                .expect("initial pull");

            let app = App::new(ws, Some(engine), Some(client), Some(runtime.handle().clone()))
                .expect("app");
            Self { app, mock, runtime, _dir: dir }
        }

        fn press(&mut self, code: KeyCode) {
            self.app.on_key(key(code));
        }

        /// Wait for the running job, the way the event loop would.
        fn settle(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while self.app.busy() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
                self.app.poll_jobs();
            }
            assert!(!self.app.busy(), "the job never finished: {}", self.app.status);
        }

        fn select(&mut self, path: &str) {
            let rows = self.app.rows();
            let index = rows
                .iter()
                .position(|row| self.app.tree.nodes[row.node].path == path)
                .unwrap_or_else(|| panic!("no row for {path}"));
            self.app.cursor = index;
            self.app.refresh_content();
        }

        fn state_of(&self, path: &str) -> PageState {
            self.app
                .tree
                .nodes
                .iter()
                .find(|node| node.path == path)
                .unwrap_or_else(|| panic!("no node for {path}"))
                .state
        }

        fn path(&self, relative: &str) -> std::path::PathBuf {
            self.app.workspace().expect("workspace").absolute(relative)
        }

        fn edit(&mut self, relative: &str, addition: &str) {
            let file = self.path(relative);
            let mut content = std::fs::read_to_string(&file).expect("read");
            content.push_str(addition);
            std::fs::write(&file, content).expect("write");
            self.app.reload().expect("reload");
        }
    }

    #[test]
    fn the_tree_shows_the_pulled_hierarchy() {
        let harness = Harness::new();
        let labels: Vec<&str> =
            harness.app.rows().iter().map(|row| harness.app.tree.nodes[row.node].label()).collect();
        assert_eq!(labels, ["Runbook", "Failover"]);
        assert_eq!(harness.state_of("Runbook.md"), PageState::Unchanged);
    }

    #[test]
    fn a_local_edit_shows_up_in_the_tree_and_the_diff_pane() {
        let mut harness = Harness::new();
        harness.edit("Runbook.md", "\nA new paragraph.\n");
        harness.select("Runbook.md");
        assert_eq!(harness.state_of("Runbook.md"), PageState::Modified);

        harness.press(KeyCode::Char('d'));
        assert_eq!(harness.app.view, View::Diff);
        let added: Vec<&str> = harness
            .app
            .content
            .iter()
            .filter(|line| line.kind == LineKind::Add)
            .map(|line| line.text.as_str())
            .collect();
        assert!(added.contains(&"+A new paragraph."), "{:?}", harness.app.content);
    }

    #[test]
    fn hunk_navigation_walks_the_diff_and_wraps() {
        let mut harness = Harness::new();
        // Two edits far enough apart to produce two hunks.
        let file = harness.path("Runbook/Failover.md");
        let content = std::fs::read_to_string(&file).unwrap();
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, format!("{content}{body}")).unwrap();
        harness.app.reload().unwrap();
        harness.select("Runbook/Failover.md");
        harness.press(KeyCode::Char('d'));

        let headers: Vec<usize> = harness
            .app
            .content
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind == LineKind::Hunk)
            .map(|(index, _)| index)
            .collect();
        assert!(!headers.is_empty(), "{:?}", harness.app.content);

        harness.press(KeyCode::Char('n'));
        assert_eq!(harness.app.scroll as usize, headers[0]);
        harness.press(KeyCode::Char('N'));
        assert_eq!(harness.app.scroll as usize, *headers.last().unwrap(), "wraps backwards");
    }

    /// The acceptance test for the whole action path: plan, confirm, upload.
    #[test]
    fn pushing_from_the_tui_reaches_the_server_and_leaves_the_page_clean() {
        let mut harness = Harness::new();
        harness.edit("Runbook.md", "\nA new paragraph.\n");
        harness.select("Runbook.md");

        // Push always shows the plan first; nothing has been uploaded yet.
        harness.press(KeyCode::Char('P'));
        assert_eq!(harness.app.mode, Mode::PushPlan);
        let plan = harness.app.plan.as_ref().expect("a plan");
        assert_eq!(plan.ops.len(), 1);
        assert_eq!(plan.ops[0].path, "Runbook.md");
        assert_eq!(harness.mock.page_version("1001"), Some(1), "no upload before confirming");

        harness.press(KeyCode::Enter);
        harness.settle();

        assert!(harness.app.status.starts_with("Pushed: 1 updated"), "{}", harness.app.status);
        let body = harness.mock.page_body("1001").expect("page");
        assert!(body.contains("A new paragraph"), "the edit reached the server: {body}");
        assert!(body.contains("Original text"), "untouched content survives: {body}");
        assert_eq!(harness.mock.page_version("1001"), Some(2));
        assert_eq!(harness.state_of("Runbook.md"), PageState::Unchanged);
    }

    #[test]
    fn cancelling_the_plan_uploads_nothing() {
        let mut harness = Harness::new();
        harness.edit("Runbook.md", "\nA new paragraph.\n");
        harness.select("Runbook.md");
        harness.press(KeyCode::Char('P'));
        harness.press(KeyCode::Esc);

        assert_eq!(harness.app.mode, Mode::Browse);
        assert!(harness.app.plan.is_none());
        assert_eq!(harness.app.status, "Push cancelled.");
        assert!(harness.mock.mutating_calls().is_empty(), "{:?}", harness.mock.mutating_calls());
    }

    #[test]
    fn fetch_then_pull_brings_in_a_page_created_by_somebody_else() {
        let mut harness = Harness::new();
        harness.mock.seed_page("1003", "Escalation", Some("1001"), "<p>Who to call.</p>");

        harness.press(KeyCode::Char('f'));
        harness.settle();
        assert!(harness.app.status.starts_with("Fetched"), "{}", harness.app.status);
        assert_eq!(
            harness.state_of("Runbook/Escalation.md"),
            PageState::RemoteNew,
            "known, under the path pull will give it, but not written yet"
        );
        assert!(!harness.path("Runbook/Escalation.md").exists());

        // Pull is scoped to the selection, so select the page that is missing.
        let index = harness
            .app
            .rows()
            .iter()
            .position(|row| harness.app.tree.nodes[row.node].page_id.as_deref() == Some("1003"))
            .expect("a row for the new page");
        harness.app.cursor = index;
        harness.press(KeyCode::Char('p'));
        harness.settle();

        assert!(harness.path("Runbook/Escalation.md").exists(), "{}", harness.app.status);
        assert_eq!(harness.state_of("Runbook/Escalation.md"), PageState::Unchanged);
    }

    /// A diverged page merges into conflict markers; the resolver clears them
    /// and the page ends up clean, exactly like `confed resolve`.
    #[test]
    fn a_conflicted_page_is_resolved_from_the_three_pane_view() {
        let mut harness = Harness::new();
        harness.edit("Runbook.md", "\nOur addition.\n");
        harness.mock.remote_edit("1001", "<p>Original text.</p><p>Their addition.</p>");

        harness.select("Runbook.md");
        harness.press(KeyCode::Char('p'));
        harness.settle();
        assert_eq!(harness.state_of("Runbook.md"), PageState::Conflicted, "{}", harness.app.status);

        harness.select("Runbook.md");
        harness.press(KeyCode::Char('c'));
        assert_eq!(harness.app.mode, Mode::Conflict);
        let resolver = harness.app.resolver.as_ref().expect("a resolver");
        assert!(resolver.hunk_count() >= 1);
        assert!(!resolver.hunk().ours.is_empty() || !resolver.hunk().theirs.is_empty());

        // Writing with hunks still open is refused.
        harness.press(KeyCode::Char('w'));
        assert_eq!(harness.app.mode, Mode::Conflict);
        assert!(harness.app.status.contains("unresolved"), "{}", harness.app.status);

        harness.press(KeyCode::Char('U')); // take ours everywhere
        harness.press(KeyCode::Char('w'));

        assert_eq!(harness.app.mode, Mode::Browse);
        assert_eq!(harness.state_of("Runbook.md"), PageState::Modified, "{}", harness.app.status);
        let file = std::fs::read_to_string(harness.path("Runbook.md")).unwrap();
        assert!(!confed_core::merge::has_conflict_markers(&file));
        assert!(file.contains("Our addition."));

        // And the resolved page pushes like any other.
        harness.select("Runbook.md");
        harness.press(KeyCode::Char('P'));
        assert_eq!(harness.app.mode, Mode::PushPlan);
        harness.press(KeyCode::Enter);
        harness.settle();
        assert!(harness.mock.page_body("1001").unwrap().contains("Our addition"));
    }

    /// A pull from the TUI moves the rules page like one from the command
    /// line, so the copy in the agent files follows it here too.
    #[test]
    fn pulling_a_new_version_of_the_rules_page_refreshes_the_agent_files() {
        let mut harness = Harness::new();
        let ws = harness.app.workspace().expect("workspace");
        ws.state().set_meta("rules_page_id", "1001").unwrap();
        confed_cli::commands::agent_docs::sync_rules(ws).expect("the first copy");
        let agents = harness.path("AGENTS.md");
        assert!(std::fs::read_to_string(&agents).unwrap().contains("Original text."));

        harness.mock.remote_edit("1001", "<p>Original text.</p><p>Ask before deleting.</p>");
        harness.select("Runbook.md");
        harness.press(KeyCode::Char('p'));
        harness.settle();

        assert!(
            harness.app.status.ends_with("The rules in CLAUDE.md and AGENTS.md were updated."),
            "{}",
            harness.app.status
        );
        assert!(std::fs::read_to_string(&agents).unwrap().contains("Ask before deleting."));
        assert_eq!(harness.state_of("Runbook.md"), PageState::Unchanged, "they are not pages");
    }

    #[test]
    fn a_second_action_is_refused_while_one_is_running() {
        let mut harness = Harness::new();
        harness.press(KeyCode::Char('f'));
        if harness.app.busy() {
            harness.press(KeyCode::Char('f'));
            assert!(harness.app.status.starts_with("Already running"), "{}", harness.app.status);
        }
        harness.settle();
    }

    #[test]
    fn quitting_shuts_a_running_job_down() {
        let mut harness = Harness::new();
        harness.press(KeyCode::Char('f'));
        harness.app.shutdown();
        assert!(!harness.app.busy());
        assert!(harness.app.workspace().is_some(), "the workspace always comes back");
        drop(harness.runtime);
    }

    #[test]
    fn without_a_client_the_network_keys_explain_themselves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = Workspace::create(dir.path()).expect("workspace");
        ws.state().set_meta("space_key", "DOCS").unwrap();
        let mut app = App::new(ws, None, None, None).expect("app");
        assert!(app.status.starts_with("Offline"));

        app.on_key(key(KeyCode::Char('f')));
        assert!(app.status.contains("Not connected"), "{}", app.status);
        app.on_key(key(KeyCode::Char('P')));
        assert!(app.status.contains("Not connected"), "{}", app.status);
    }

    #[test]
    fn keys_move_the_cursor_filter_the_tree_and_toggle_help() {
        let mut harness = Harness::new();
        assert_eq!(harness.app.cursor, 0);
        harness.press(KeyCode::Char('j'));
        assert_eq!(harness.app.cursor, 1);
        harness.press(KeyCode::Char('k'));
        assert_eq!(harness.app.cursor, 0);

        harness.press(KeyCode::Char('h')); // collapse Runbook
        assert_eq!(harness.app.rows().len(), 1);
        harness.press(KeyCode::Char('l'));
        assert_eq!(harness.app.rows().len(), 2);

        harness.press(KeyCode::Char('/'));
        assert_eq!(harness.app.mode, Mode::Filter);
        for c in "fail".chars() {
            harness.press(KeyCode::Char(c));
        }
        let labels: Vec<&str> =
            harness.app.rows().iter().map(|row| harness.app.tree.nodes[row.node].label()).collect();
        assert_eq!(labels, ["Runbook", "Failover"], "the parent stays for context");
        harness.press(KeyCode::Esc);
        assert_eq!(harness.app.mode, Mode::Browse);
        assert!(harness.app.filter.is_empty());

        harness.press(KeyCode::Char('?'));
        assert_eq!(harness.app.mode, Mode::Help);
        harness.press(KeyCode::Char('x'));
        assert_eq!(harness.app.mode, Mode::Browse);

        harness.press(KeyCode::Char('q'));
        assert!(harness.app.should_quit);
    }
}
