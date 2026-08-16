//! Application state and key handling.
//!
//! Everything here is synchronous and testable: keys go in, state changes come
//! out. The only asynchronous part is [`Job`], which hands the workspace to the
//! tokio runtime for the duration of a fetch/pull/push and takes it back when the
//! operation finishes — so a slow server never blocks the event loop.

use crate::tui::conflict::{Choice, Resolver};
use crate::tui::pane::{self, DiffSide, Line as PaneLine, LineKind};
use crate::tui::tree::{Row, Tree};
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
pub struct Job {
    pub label: &'static str,
    pub started: Instant,
    cancelled: bool,
    rx: Receiver<Finished>,
    handle: tokio::task::JoinHandle<()>,
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

    pub fn selected_node(&self) -> Option<&crate::tui::tree::Node> {
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
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.filter.clear();
            }
            KeyCode::Char('d') => {
                self.view = if self.view == View::Diff { View::Preview } else { View::Diff };
                self.refresh_content();
            }
            KeyCode::Char('r') => {
                self.diff_side =
                    if self.diff_side == DiffSide::Base { DiffSide::Remote } else { DiffSide::Base };
                self.view = View::Diff;
                self.refresh_content();
            }
            KeyCode::PageDown => self.scroll_content(self.page_height as i32),
            KeyCode::PageUp => self.scroll_content(-(self.page_height as i32)),
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
            KeyCode::Char('j') | KeyCode::Down => resolver.scroll = resolver.scroll.saturating_add(1),
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

    fn toggle_expand(&mut self) {
        if let Some(row) = self.selected() {
            let expanded = self.tree.nodes[row.node].expanded;
            self.tree.set_expanded(row.node, !expanded);
            self.clamp_cursor();
        }
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

        match crate::commands::resolve::finish_resolution(ws, &record, &original, &resolved) {
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
            .filter(|node| !node.path.is_empty())
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
        let handle = runtime.spawn(async move {
            let outcome = run_work(&engine, &mut ws, work).await.map_err(|e| e.to_string());
            let _ = tx.send(Finished { ws, outcome });
        });
        self.status = format!("Running {label}…");
        self.job = Some(Job { label, started: Instant::now(), cancelled: false, rx, handle });
    }

    /// True cancellation would have to reach into the sync engine; what the TUI
    /// can do is stop waiting, drop the task, and re-open the workspace.
    fn cancel_job(&mut self) {
        if let Some(job) = &mut self.job {
            if !job.cancelled {
                job.handle.abort();
                job.cancelled = true;
                self.status = format!("Cancelling {}…", job.label);
            }
        }
    }

    /// Called once per event-loop tick: collect a finished job, if any.
    pub fn poll_jobs(&mut self) {
        self.frames += 1;
        let Some(job) = &self.job else { return };

        match job.rx.try_recv() {
            Ok(Finished { ws, outcome }) => {
                let label = job.label;
                self.ws = Some(ws);
                self.job = None;
                self.status = match outcome {
                    Ok(message) => message,
                    Err(e) => format!("{label} failed: {e}"),
                };
                if let Err(e) = self.reload() {
                    self.status = format!("{} (refresh failed: {e})", self.status);
                }
            }
            Err(TryRecvError::Empty) => {
                // An aborted task drops the workspace instead of sending it back,
                // so re-open it from disk once the task is really gone.
                if job.cancelled && job.handle.is_finished() {
                    let label = job.label;
                    self.job = None;
                    self.status = match Workspace::open(&self.root) {
                        Ok(ws) => {
                            self.ws = Some(ws);
                            let _ = self.reload();
                            format!("Cancelled {label}.")
                        }
                        Err(e) => format!("Cancelled {label}, but the workspace is gone: {e}"),
                    };
                }
            }
            Err(TryRecvError::Disconnected) => {
                let label = job.label;
                self.job = None;
                if self.ws.is_none() {
                    match Workspace::open(&self.root) {
                        Ok(ws) => self.ws = Some(ws),
                        Err(e) => self.status = format!("{label}: {e}"),
                    }
                }
                let _ = self.reload();
            }
        }
    }

    /// How long the current job has been running, for the progress line.
    pub fn job_elapsed(&self) -> Duration {
        self.job.as_ref().map(|j| j.started.elapsed()).unwrap_or_default()
    }
}

async fn run_work(engine: &SyncEngine, ws: &mut Workspace, work: Work) -> Result<String> {
    let _lock = ws.lock()?;
    match work {
        Work::Fetch => {
            let outcome = engine.fetch(ws, &FetchOptions::default()).await?;
            Ok(format!(
                "Fetched {} page(s), {} unchanged.",
                outcome.fetched, outcome.unchanged
            ))
        }
        Work::Pull(options) => {
            let outcome = engine.pull(ws, &options).await?;
            Ok(format!(
                "Pulled: {} created, {} updated, {} merged, {} conflicted.",
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
