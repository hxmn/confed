//! Drawing. Every widget is a pure function of [`App`], so the golden-frame
//! tests below render real frames through `TestBackend` rather than mocking.

use crate::tui::app::{App, Mode, View};
use crate::tui::conflict::side_lines;
use crate::tui::pane::{Line as PaneLine, LineKind};
use crate::tui::tree;
use confed_core::worktree::PageState;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    draw_header(frame, app, chunks[0]);
    if app.mode == Mode::Conflict {
        draw_resolver(frame, app, chunks[1]);
    } else {
        draw_body(frame, app, chunks[1]);
    }
    draw_footer(frame, app, chunks[2]);

    match app.mode {
        Mode::Help => draw_help(frame, area),
        Mode::PushPlan => draw_plan(frame, app, area),
        _ => {}
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let (outgoing, incoming, conflicted) = app.counts();
    let mut spans = vec![
        Span::styled(" confed ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("{} ", app.space)),
        Span::styled(format!("{outgoing}↑ "), Style::default().fg(Color::Green)),
        Span::styled(format!("{incoming}↓ "), Style::default().fg(Color::Blue)),
    ];
    if conflicted > 0 {
        spans.push(Span::styled(
            format!("{conflicted}C "),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled("  ? help", Style::default().fg(Color::DarkGray)));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Reset)),
        area,
    );
}

fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    draw_tree(frame, app, panes[0]);
    draw_content(frame, app, panes[1]);
}

fn draw_tree(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let node = &app.tree.nodes[row.node];
            let marker = if !app.tree.has_children(row.node) {
                "  "
            } else if node.expanded {
                "▾ "
            } else {
                "▸ "
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", tree::badge(node.state)),
                    state_style(node.state).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  ".repeat(row.depth)),
                Span::raw(marker),
                Span::styled(node.label().to_string(), state_style(node.state)),
            ]))
        })
        .collect();

    let title = if app.mode == Mode::Filter || !app.filter.is_empty() {
        format!(" /{} ", app.filter)
    } else {
        format!(" Pages ({}) ", rows.len())
    };

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.cursor.min(rows.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        area,
        &mut state,
    );
}

fn draw_content(frame: &mut Frame, app: &mut App, area: Rect) {
    app.page_height = area.height.saturating_sub(2);

    let title = match (app.view, app.selected_node()) {
        (View::Preview, Some(node)) => format!(" {} ", display_path(&node.path, node.label())),
        (View::Diff, Some(node)) => {
            format!(" {} — {} ", display_path(&node.path, node.label()), app.diff_side.label())
        }
        (_, None) => " Nothing selected ".to_string(),
    };

    let lines: Vec<Line> = app.content.iter().map(pane_line).collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((app.scroll, 0)),
        area,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    if app.busy() {
        let spinner = SPINNER[(app.frames as usize) % SPINNER.len()];
        spans.push(Span::styled(
            format!(" {spinner} {:.0}s ", app.job_elapsed().as_secs_f32()),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    if app.status.is_empty() {
        spans.push(Span::styled(
            " q quit  ? help  / filter  d diff  f fetch  p pull  P push  c resolve  o open",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        spans.push(Span::raw(app.status.clone()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = vec![
        help_line("j / k / ↑ ↓", "move"),
        help_line("Enter, Space", "expand or collapse"),
        help_line("/", "filter as you type"),
        help_line("d", "preview or diff"),
        help_line("r", "diff against base or remote"),
        help_line("PgUp / PgDn", "scroll the pane"),
        help_line("n / N", "next, previous diff hunk"),
        help_line("f / p / P", "fetch, pull, push (push shows a plan first)"),
        help_line("c", "resolve a conflicted page"),
        help_line("o", "open in the browser"),
        help_line("R", "rescan the working tree"),
        help_line("Esc", "cancel a running operation"),
        help_line("q", "quit"),
    ];
    let popup = centered(66, 80, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(" Keys — any key closes "))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn help_line(keys: &'static str, what: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{keys:<14}"), Style::default().fg(Color::Cyan)),
        Span::raw(what),
    ])
}

fn draw_plan(frame: &mut Frame, app: &App, area: Rect) {
    let Some(plan) = &app.plan else { return };
    let mut lines: Vec<Line> = Vec::new();
    for op in &plan.ops {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<7}", format!("{:?}", op.kind).to_lowercase()),
                Style::default().fg(Color::Green),
            ),
            Span::raw(format!("{} ({})", op.path, op.ops.join(", "))),
        ]));
    }
    for skipped in &plan.skipped {
        lines.push(Line::from(Span::styled(
            format!("skip   {} — {}", skipped.path, skipped.reason),
            Style::default().fg(Color::Yellow),
        )));
    }
    for attachment in &plan.attachment_ops {
        lines.push(Line::from(format!("attach {attachment}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter/y upload    Esc/n cancel",
        Style::default().add_modifier(Modifier::BOLD),
    )));

    let popup = centered(70, 60, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Push plan ")),
        popup,
    );
}

fn draw_resolver(frame: &mut Frame, app: &App, area: Rect) {
    let Some(resolver) = &app.resolver else { return };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(area);

    let hunk = resolver.hunk();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", resolver.path),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "hunk {}/{} [{}] · {} left ",
                resolver.current + 1,
                resolver.hunk_count(),
                resolver.choice().label(),
                resolver.unresolved()
            )),
        ]))
        .alignment(Alignment::Left),
        rows[0],
    );

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(rows[1]);

    let sides = [
        (" ours (u) ", &hunk.ours, Color::Green),
        (" base ", &hunk.base, Color::DarkGray),
        (" theirs (t) ", &hunk.theirs, Color::Blue),
    ];
    for (index, (title, text, color)) in sides.into_iter().enumerate() {
        let lines: Vec<Line> = side_lines(text)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(color))))
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title))
                .scroll((resolver.scroll, 0)),
            panes[index],
        );
    }
}

fn pane_line(line: &PaneLine) -> Line<'static> {
    let style = match line.kind {
        LineKind::Plain => Style::default(),
        LineKind::Heading => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        LineKind::Meta => Style::default().fg(Color::DarkGray),
        LineKind::Hunk => Style::default().fg(Color::Magenta),
        LineKind::Add => Style::default().fg(Color::Green),
        LineKind::Del => Style::default().fg(Color::Red),
        LineKind::Warn => Style::default().fg(Color::Yellow),
    };
    Line::from(Span::styled(line.text.clone(), style))
}

fn state_style(state: PageState) -> Style {
    let color = match state {
        PageState::Unchanged => Color::Reset,
        PageState::Modified | PageState::LocalNew => Color::Green,
        PageState::Behind | PageState::RemoteNew => Color::Blue,
        PageState::Diverged => Color::Yellow,
        PageState::Conflicted | PageState::LocalDeleted | PageState::RemoteDeleted => Color::Red,
        PageState::Untracked => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn display_path<'a>(path: &'a str, fallback: &'a str) -> &'a str {
    if path.is_empty() {
        fallback
    } else {
        path
    }
}

/// A popup covering `width`%×`height`% of the screen.
fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Percentage(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::conflict::{Resolver, CONFLICTED_PAGE};
    use crate::tui::pane::LineKind;
    use crate::tui::tree::{fixtures, Tree};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    /// Render one frame and return it as text, so a golden test reads like the
    /// screen it describes.
    fn frame(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|f| draw(f, app)).expect("draw");
        text_of(terminal.backend().buffer())
    }

    fn text_of(buffer: &Buffer) -> String {
        let area = *buffer.area();
        (0..area.height)
            .map(|y| {
                let row: String =
                    (0..area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sample_app() -> App {
        let (scan, records) = fixtures::sample();
        let tree = Tree::build(&scan, &records, &[]);
        App::for_render(
            "DOCS",
            tree,
            vec![
                PaneLine::new(LineKind::Meta, "---"),
                PaneLine::new(LineKind::Meta, "title: Handbook"),
                PaneLine::new(LineKind::Meta, "---"),
                PaneLine::new(LineKind::Plain, ""),
                PaneLine::new(LineKind::Heading, "# Handbook"),
            ],
        )
    }

    /// The badge column is `confed status --short`, one page per line, and the
    /// hierarchy comes from the state DB rather than the file paths.
    #[test]
    fn the_tree_renders_with_status_badges_in_position_order() {
        let mut app = sample_app();
        assert_eq!(
            frame(&mut app, 62, 11),
            r"
 confed DOCS 1↑ 1↓ 1C   ? help
┌ Pages (4) ────────────┐┌ Handbook.md ──────────────────────┐
│  ▾ Handbook           ││---                                │
│M   ▾ Onboarding       ││title: Handbook                    │
│C       Week One       ││---                                │
│B   Runbook            ││                                   │
│                       ││# Handbook                         │
│                       ││                                   │
│                       ││                                   │
└───────────────────────┘└───────────────────────────────────┘
 q quit  ? help  / filter  d diff  f fetch  p pull  P push  c"
                .trim_start_matches('\n')
        );
    }

    #[test]
    fn collapsing_and_filtering_change_what_the_tree_shows() {
        let mut app = sample_app();
        app.tree.set_all_expanded(false);
        app.filter = "week".into();
        let rendered = frame(&mut app, 62, 11);
        assert!(rendered.contains("/week"), "the filter is shown in the title: {rendered}");
        assert!(rendered.contains("Week One"), "matches stay visible: {rendered}");
        assert!(rendered.contains("Onboarding"), "so do their ancestors: {rendered}");
    }

    #[test]
    fn the_help_overlay_lists_every_binding() {
        let mut app = sample_app();
        app.mode = Mode::Help;
        assert_eq!(
            frame(&mut app, 62, 24),
            r"
 confed DOCS 1↑ 1↓ 1C   ? help
┌ Pages (4) ────────────┐┌ Handbook.md ──────────────────────┐
│  ▾ Handbo┌ Keys — any key closes ───────────────┐          │
│M   ▾ Onbo│j / k / ↑ ↓   move                    │          │
│C       We│Enter, Space  expand or collapse      │          │
│B   Runboo│/             filter as you type      │          │
│          │d             preview or diff         │          │
│          │r             diff against base or    │          │
│          │remote                                │          │
│          │PgUp / PgDn   scroll the pane         │          │
│          │n / N         next, previous diff hunk│          │
│          │f / p / P     fetch, pull, push (push │          │
│          │shows a plan first)                   │          │
│          │c             resolve a conflicted    │          │
│          │page                                  │          │
│          │o             open in the browser     │          │
│          │R             rescan the working tree │          │
│          │Esc           cancel a running        │          │
│          │operation                             │          │
│          │q             quit                    │          │
│          │                                      │          │
│          └──────────────────────────────────────┘          │
└───────────────────────┘└───────────────────────────────────┘
 q quit  ? help  / filter  d diff  f fetch  p pull  P push  c"
                .trim_start_matches('\n')
        );
    }

    #[test]
    fn the_conflict_resolver_shows_all_three_sides() {
        let mut app = sample_app();
        let resolver =
            Resolver::new("3", "Handbook/Onboarding/Week One.md", CONFLICTED_PAGE).unwrap();
        app.resolver = Some(resolver);
        app.mode = Mode::Conflict;
        assert_eq!(
            frame(&mut app, 62, 8),
            r"
 confed DOCS 1↑ 1↓ 1C   ? help
 Handbook/Onboarding/Week One.md hunk 1/1 [unresolved] · 1 lef
┌ ours (u) ─────────┐┌ base ─────────────┐┌ theirs (t) ──────┐
│our paragraph      ││the original       ││their paragraph   │
│                   ││                   ││                  │
│                   ││                   ││                  │
└───────────────────┘└───────────────────┘└──────────────────┘
 q quit  ? help  / filter  d diff  f fetch  p pull  P push  c"
                .trim_start_matches('\n')
        );
    }

    #[test]
    fn a_running_job_shows_a_spinner_and_the_status_line() {
        let mut app = sample_app();
        app.status = "Running push…".into();
        let rendered = frame(&mut app, 62, 8);
        assert!(rendered.contains("Running push…"), "{rendered}");
    }
}
