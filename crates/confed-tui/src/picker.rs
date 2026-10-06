//! Choosing one page of the space.
//!
//! The picker behind `confed config --set rules_page_id`: the page tree, a
//! search box that narrows it by title as you type, and a preview of the page
//! under the cursor. Like the rest of the TUI it reads the workspace and nothing
//! else, so the pages on offer are the ones the last fetch saw.

use crate::pane::{self, Line as PaneLine, LineKind};
use crate::tree::{Row, Tree};
use crate::ui::pane_line;
use confed_core::error::Result;
use confed_core::frontmatter;
use confed_core::workspace::Workspace;
use confed_core::worktree;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

/// How the picker ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The id of the page that was chosen.
    Chosen(String),
    Cancelled,
}

pub struct Picker<'a> {
    pub space: String,
    pub tree: Tree,
    /// What has been typed into the search box.
    pub query: String,
    /// Index into the rows currently listed.
    pub cursor: usize,
    /// The page the setting names now, marked in the list.
    pub current: Option<String>,
    pub content: Vec<PaneLine>,
    pub status: String,
    /// Set once the user has chosen or backed out, which ends the loop.
    pub outcome: Option<Outcome>,
    /// Height of the list, so PageUp/PageDown can move a screenful.
    pub list_height: u16,

    ws: Option<&'a Workspace>,
}

impl<'a> Picker<'a> {
    /// A picker over every page the workspace knows, starting on `current`.
    pub fn new(ws: &'a Workspace, current: Option<&str>) -> Result<Self> {
        let scan = worktree::scan(ws)?;
        let tree = Tree::build(&scan, &ws.state().all_pages()?, &ws.state().all_remote()?);
        let mut picker = Self {
            space: ws.space_key().unwrap_or_else(|_| "?".to_string()),
            tree,
            query: String::new(),
            cursor: 0,
            current: current.map(str::to_string),
            content: Vec::new(),
            status: String::new(),
            outcome: None,
            list_height: 20,
            ws: Some(ws),
        };
        let on_current = picker.rows().iter().position(|row| picker.is_current(row.node));
        picker.cursor = on_current.unwrap_or(0);
        picker.refresh_preview();
        Ok(picker)
    }

    /// The whole tree, or — once something is typed — the pages whose title
    /// matches it.
    pub fn rows(&self) -> Vec<Row> {
        if self.searching() {
            self.tree.search_titles(&self.query)
        } else {
            self.tree.rows("")
        }
    }

    pub fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    pub fn selected(&self) -> Option<Row> {
        self.rows().get(self.cursor).copied()
    }

    fn is_current(&self, node: usize) -> bool {
        self.current.is_some() && self.tree.nodes[node].page_id == self.current
    }

    // ---- keys -------------------------------------------------------------

    /// Every printable key goes into the search box, so the list is moved with
    /// the arrow keys alone.
    pub fn on_key(&mut self, key: KeyEvent) {
        self.status.clear();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.list_height.max(1) as isize;
        match key.code {
            KeyCode::Char('c') if ctrl => self.outcome = Some(Outcome::Cancelled),
            KeyCode::Char('u') if ctrl => self.set_query(String::new()),
            KeyCode::Esc if self.query.is_empty() => self.outcome = Some(Outcome::Cancelled),
            KeyCode::Esc => self.set_query(String::new()),
            KeyCode::Enter => self.choose(),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::Home => self.set_cursor(0),
            KeyCode::End => self.set_cursor(isize::MAX),
            KeyCode::Backspace => {
                let mut query = self.query.clone();
                query.pop();
                self.set_query(query);
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                let mut query = self.query.clone();
                query.push(c);
                self.set_query(query);
            }
            _ => {}
        }
    }

    fn set_query(&mut self, query: String) {
        let narrowing = query.len() > self.query.len();
        let selected = self.selected().map(|row| row.node);
        self.query = query;

        // Typing goes to the first answer; taking it back stays on the page the
        // cursor was on, now shown where it sits in the tree.
        let rows = self.rows();
        self.cursor = match selected {
            Some(node) if !narrowing => rows.iter().position(|row| row.node == node).unwrap_or(0),
            _ => 0,
        };
        self.refresh_preview();
    }

    fn move_cursor(&mut self, delta: isize) {
        self.set_cursor((self.cursor as isize).saturating_add(delta));
    }

    fn set_cursor(&mut self, target: isize) {
        let len = self.rows().len() as isize;
        if len == 0 {
            return;
        }
        self.cursor = target.clamp(0, len - 1) as usize;
        self.refresh_preview();
    }

    fn choose(&mut self) {
        let Some(row) = self.selected() else {
            self.status = "No page has that title — Backspace widens the search.".into();
            return;
        };
        match &self.tree.nodes[row.node].page_id {
            Some(page_id) => self.outcome = Some(Outcome::Chosen(page_id.clone())),
            None => self.status = "That page has no id yet — push it first.".into(),
        }
    }

    fn refresh_preview(&mut self) {
        // Without a workspace there is nothing to read: a render test supplies
        // the content itself.
        let Some(ws) = self.ws else { return };
        let Some(row) = self.selected() else {
            self.content = Vec::new();
            return;
        };
        let path = &self.tree.nodes[row.node].path;
        if path.is_empty() || !ws.absolute(path).exists() {
            self.content = vec![PaneLine::new(
                LineKind::Warn,
                "Not pulled yet. It can be chosen: `confed pull` then copies its rules.",
            )];
            return;
        }
        // The body alone: it is what the page says that is being chosen, and
        // the frontmatter would fill the pane before it.
        self.content = match std::fs::read_to_string(ws.absolute(path)) {
            Ok(content) => {
                let body = frontmatter::split(&content).map_or(content.as_str(), |(_, body)| body);
                pane::render_markdown(body)
            }
            Err(e) => vec![PaneLine::new(LineKind::Warn, format!("{path}: {e}"))],
        };
    }
}

#[cfg(test)]
impl Picker<'static> {
    /// A picker with no workspace behind it, for tests that only render.
    pub fn for_render(space: &str, tree: Tree, content: Vec<PaneLine>) -> Self {
        Self {
            space: space.to_string(),
            tree,
            query: String::new(),
            cursor: 0,
            current: None,
            content,
            status: String::new(),
            outcome: None,
            list_height: 20,
            ws: None,
        }
    }
}

// ---- drawing ----------------------------------------------------------------

pub fn draw(frame: &mut Frame, picker: &mut Picker) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
        .split(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" confed ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{} ", picker.space)),
            Span::styled(
                "choose the page that holds this space's rules for agents",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        chunks[0],
    );

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(chunks[1]);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(panes[0]);

    draw_search(frame, picker, left[0]);
    draw_list(frame, picker, left[1]);
    draw_preview(frame, picker, panes[1]);

    let footer = if !picker.status.is_empty() {
        Span::raw(format!(" {}", picker.status))
    } else {
        let esc = if picker.query.is_empty() { "cancel" } else { "clear the search" };
        Span::styled(
            format!(" type to search by title  ↑ ↓ move  Enter choose  Esc {esc}"),
            Style::default().fg(Color::DarkGray),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(footer)), chunks[2]);
}

fn draw_search(frame: &mut Frame, picker: &Picker, area: Rect) {
    let query = Line::from(picker.query.as_str());
    // The terminal's own cursor marks where the next character goes.
    let column = area.x.saturating_add(1).saturating_add(query.width() as u16);
    frame.set_cursor_position(Position::new(
        column.min(area.right().saturating_sub(2)),
        area.y.saturating_add(1),
    ));
    frame.render_widget(
        Paragraph::new(query)
            .block(Block::default().borders(Borders::ALL).title(" Search by title ")),
        area,
    );
}

fn draw_list(frame: &mut Frame, picker: &mut Picker, area: Rect) {
    picker.list_height = area.height.saturating_sub(2);

    let rows = picker.rows();
    let searching = picker.searching();
    let dim = Style::default().fg(Color::DarkGray);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let node = &picker.tree.nodes[row.node];
            // A page with no id cannot be named by a setting.
            let style = if node.page_id.is_some() { Style::default() } else { dim };
            let mut spans = vec![
                Span::raw("  ".repeat(row.depth)),
                Span::styled(node.label().to_string(), style),
            ];
            if picker.is_current(row.node) {
                spans.push(Span::styled("  current", Style::default().fg(Color::Cyan)));
            }
            // A match has lost its place in the tree, so say where it lives.
            if let (true, Some((parent, _))) = (searching, node.path.rsplit_once('/')) {
                spans.push(Span::styled(format!("  {parent}"), dim));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = if searching {
        format!(" {} of {} ", rows.len(), picker.tree.nodes.len())
    } else {
        format!(" Pages ({}) ", rows.len())
    };

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(picker.cursor.min(rows.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn draw_preview(frame: &mut Frame, picker: &Picker, area: Rect) {
    let title = match picker.selected() {
        Some(row) => {
            let node = &picker.tree.nodes[row.node];
            format!(" {} ", if node.path.is_empty() { node.label() } else { &node.path })
        }
        None => " Nothing selected ".to_string(),
    };
    let lines: Vec<Line> = picker.content.iter().map(pane_line).collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::fixtures;
    use confed_api::{ConfluenceClient, Flavor, MockClient, SpaceId};
    use confed_core::sync::{FetchOptions, PullOptions, SyncEngine};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::sync::Arc;

    /// A pulled three-page space: Runbook, its child Failover, and Team rules.
    struct Space {
        ws: Workspace,
        mock: Arc<MockClient>,
        engine: SyncEngine,
        runtime: tokio::runtime::Runtime,
        _dir: tempfile::TempDir,
    }

    impl Space {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let mut ws = Workspace::create(dir.path()).expect("workspace");
            ws.state().set_meta("space_key", "DOCS").unwrap();
            ws.state().set_meta("base_url", "https://mock.test").unwrap();
            ws.state().set_meta("flavor", Flavor::Cloud.as_str()).unwrap();

            let mock = Arc::new(MockClient::new(Flavor::Cloud));
            mock.seed_page("1001", "Runbook", None, "<p>Original text.</p>");
            mock.seed_page("1002", "Failover", Some("1001"), "<p>Steps.</p>");
            mock.seed_page("1003", "Team rules", None, "<p>Write in plain English.</p>");

            let client: Arc<dyn ConfluenceClient> = mock.clone();
            let engine = SyncEngine::new(
                client,
                SpaceId { key: "DOCS".into(), numeric: Some("1001".into()) },
                2,
            );
            let runtime =
                tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
            runtime
                .block_on(engine.pull(&mut ws, &PullOptions::everything()))
                .expect("initial pull");
            Self { ws, mock, engine, runtime, _dir: dir }
        }
    }

    fn press(picker: &mut Picker, code: KeyCode) {
        picker.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(picker: &mut Picker, text: &str) {
        for c in text.chars() {
            press(picker, KeyCode::Char(c));
        }
    }

    fn titles(picker: &Picker) -> Vec<String> {
        picker.rows().iter().map(|row| picker.tree.nodes[row.node].label().to_string()).collect()
    }

    #[test]
    fn typing_narrows_the_list_by_title_and_enter_chooses() {
        let space = Space::new();
        let mut picker = Picker::new(&space.ws, None).expect("picker");
        assert_eq!(titles(&picker), ["Runbook", "Failover", "Team rules"]);

        type_text(&mut picker, "RULES");
        assert_eq!(titles(&picker), ["Team rules"], "case does not matter");
        assert_eq!(
            picker.content.first().map(|line| line.text.as_str()),
            Some("Write in plain English."),
            "the preview follows the cursor and starts at the body: {:?}",
            picker.content
        );

        press(&mut picker, KeyCode::Enter);
        assert_eq!(picker.outcome, Some(Outcome::Chosen("1003".into())));
    }

    #[test]
    fn letters_that_move_the_main_tui_are_just_letters_here() {
        let space = Space::new();
        let mut picker = Picker::new(&space.ws, None).expect("picker");
        type_text(&mut picker, "jq");
        assert_eq!(picker.query, "jq");
        assert_eq!(picker.outcome, None, "q types a q; it does not quit");
        assert!(titles(&picker).is_empty());

        press(&mut picker, KeyCode::Enter);
        assert_eq!(picker.outcome, None);
        assert!(picker.status.contains("No page has that title"), "{}", picker.status);
    }

    #[test]
    fn escape_clears_the_search_first_and_cancels_second() {
        let space = Space::new();
        let mut picker = Picker::new(&space.ws, None).expect("picker");
        type_text(&mut picker, "fail");
        assert_eq!(titles(&picker), ["Failover"]);

        press(&mut picker, KeyCode::Esc);
        assert_eq!(picker.outcome, None);
        assert!(picker.query.is_empty());
        let selected = picker.selected().expect("a row");
        assert_eq!(
            picker.tree.nodes[selected.node].label(),
            "Failover",
            "the cursor stays on the page it found"
        );

        press(&mut picker, KeyCode::Esc);
        assert_eq!(picker.outcome, Some(Outcome::Cancelled));
    }

    #[test]
    fn the_cursor_starts_on_the_page_the_setting_names() {
        let space = Space::new();
        let mut picker = Picker::new(&space.ws, Some("1003")).expect("picker");
        assert_eq!(picker.cursor, 2);
        press(&mut picker, KeyCode::Up);
        press(&mut picker, KeyCode::Enter);
        assert_eq!(picker.outcome, Some(Outcome::Chosen("1002".into())));
    }

    /// "All pages in the space" includes one a fetch has seen and no pull has
    /// written yet.
    #[test]
    fn a_page_that_was_fetched_but_not_pulled_can_be_chosen() {
        let mut space = Space::new();
        space.mock.seed_page("1004", "House style", None, "<p>Short sentences.</p>");
        space
            .runtime
            .block_on(space.engine.fetch(&mut space.ws, &FetchOptions::default()))
            .expect("fetch");

        let mut picker = Picker::new(&space.ws, None).expect("picker");
        type_text(&mut picker, "house");
        assert_eq!(titles(&picker), ["House style"]);
        assert!(picker.content[0].text.starts_with("Not pulled yet"), "{:?}", picker.content);
        press(&mut picker, KeyCode::Enter);
        assert_eq!(picker.outcome, Some(Outcome::Chosen("1004".into())));
    }

    fn frame(picker: &mut Picker, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|f| draw(f, picker)).expect("draw");
        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        (0..area.height)
            .map(|y| {
                let row: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sample_picker() -> Picker<'static> {
        let (scan, records) = fixtures::sample();
        let mut picker = Picker::for_render(
            "DOCS",
            Tree::build(&scan, &records, &[]),
            vec![PaneLine::new(LineKind::Heading, "# Week One")],
        );
        picker.current = Some("4".into());
        picker
    }

    #[test]
    fn the_picker_shows_the_tree_and_marks_the_current_page() {
        let mut picker = sample_picker();
        assert_eq!(
            frame(&mut picker, 62, 11),
            r"
 confed DOCS choose the page that holds this space's rules for
┌ Search by title ──────┐┌ Handbook.md ──────────────────────┐
│                       ││# Week One                         │
└───────────────────────┘│                                   │
┌ Pages (4) ────────────┐│                                   │
│Handbook               ││                                   │
│  Onboarding           ││                                   │
│    Week One           ││                                   │
│Runbook  current       ││                                   │
└───────────────────────┘└───────────────────────────────────┘
 type to search by title  ↑ ↓ move  Enter choose  Esc cancel"
                .trim_start_matches('\n')
        );
    }

    #[test]
    fn a_search_shows_the_matches_and_where_each_one_lives() {
        let mut picker = sample_picker();
        type_text(&mut picker, "o");
        assert_eq!(
            frame(&mut picker, 62, 11),
            r"
 confed DOCS choose the page that holds this space's rules for
┌ Search by title ──────┐┌ Handbook.md ──────────────────────┐
│o                      ││# Week One                         │
└───────────────────────┘│                                   │
┌ 4 of 4 ───────────────┐│                                   │
│Handbook               ││                                   │
│Onboarding  Handbook   ││                                   │
│Week One  Handbook/Onbo││                                   │
│Runbook  current       ││                                   │
└───────────────────────┘└───────────────────────────────────┘
 type to search by title  ↑ ↓ move  Enter choose  Esc clear th"
                .trim_start_matches('\n')
        );
    }
}
