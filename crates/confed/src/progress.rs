//! Terminal progress display.
//!
//! Progress goes to stderr as a single line rewritten in place, so stdout stays
//! parseable and a redirected run leaves no half-drawn bars behind. It is shown
//! only when someone is watching: an interactive stderr, no `--silent`, no
//! `--quiet`, and no `--json`.

use confed_core::progress::Progress;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Redraw at most this often, so a fast local sync does not spend its time
/// writing escape codes.
const REDRAW_INTERVAL: Duration = Duration::from_millis(80);

pub struct TerminalProgress {
    stage: Mutex<String>,
    /// What the most recent item was, shown after the counter.
    last_detail: Mutex<String>,
    total: AtomicUsize,
    done: AtomicUsize,
    /// Zero means "no total known", which the counter renders as a bare count.
    has_total: AtomicUsize,
    last_draw: Mutex<Instant>,
    width: usize,
}

impl TerminalProgress {
    pub fn new() -> Self {
        Self {
            stage: Mutex::new(String::new()),
            last_detail: Mutex::new(String::new()),
            total: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            has_total: AtomicUsize::new(0),
            last_draw: Mutex::new(Instant::now() - REDRAW_INTERVAL),
            width: terminal_width(),
        }
    }

    /// Whether a progress display makes sense for this run.
    pub fn should_show(silent: bool, quiet: bool, json: bool) -> bool {
        !silent && !quiet && !json && std::io::stderr().is_terminal()
    }

    fn draw(&self, force: bool) {
        {
            let mut last = self.last_draw.lock().expect("progress poisoned");
            if !force && last.elapsed() < REDRAW_INTERVAL {
                return;
            }
            *last = Instant::now();
        }

        write_line(&render_line(
            &self.stage.lock().expect("progress poisoned").clone(),
            self.done.load(Ordering::Relaxed),
            (self.has_total.load(Ordering::Relaxed) == 1)
                .then(|| self.total.load(Ordering::Relaxed)),
            &self.last_detail.lock().expect("progress poisoned").clone(),
            self.width,
        ));
    }
}

/// The visible line, as a pure function of what is being reported.
fn render_line(
    stage: &str,
    done: usize,
    total: Option<usize>,
    detail: &str,
    width: usize,
) -> String {
    let counter = match total {
        Some(total) => format!("{done}/{total}"),
        None => done.to_string(),
    };
    let mut line = format!("{stage} {counter}");
    if !detail.is_empty() {
        line.push_str("  ");
        line.push_str(detail);
    }
    truncate(&line, width)
}

fn write_line(line: &str) {
    let mut stderr = std::io::stderr();
    // \r returns to the start, \x1b[K clears what the previous, longer line left.
    let _ = write!(stderr, "\r\x1b[K{line}");
    let _ = stderr.flush();
}

fn clear_line() {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "\r\x1b[K");
    let _ = stderr.flush();
}

/// Cut to the terminal width on a character boundary, with an ellipsis.
fn truncate(line: &str, width: usize) -> String {
    if width == 0 || line.chars().count() <= width {
        return line.to_string();
    }
    let keep = width.saturating_sub(1);
    line.chars().take(keep).collect::<String>() + "…"
}

fn terminal_width() -> usize {
    crossterm::terminal::size().map(|(cols, _)| cols as usize).unwrap_or(80)
}

impl Progress for TerminalProgress {
    fn stage(&self, name: &str, total: Option<usize>) {
        *self.stage.lock().expect("progress poisoned") = name.to_string();
        self.last_detail.lock().expect("progress poisoned").clear();
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total.unwrap_or(0), Ordering::Relaxed);
        self.has_total.store(usize::from(total.is_some()), Ordering::Relaxed);
        self.draw(true);
    }

    fn item(&self, detail: &str) {
        self.done.fetch_add(1, Ordering::Relaxed);
        *self.last_detail.lock().expect("progress poisoned") = detail.to_string();
        self.draw(false);
    }

    fn finish(&self) {
        clear_line();
    }
}

impl Default for TerminalProgress {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_shown_only_when_someone_is_watching() {
        // stderr is not a terminal under `cargo test`, so every case is false —
        // which is itself the property that matters for CI logs.
        assert!(!TerminalProgress::should_show(true, false, false), "--silent");
        assert!(!TerminalProgress::should_show(false, true, false), "--quiet");
        assert!(!TerminalProgress::should_show(false, false, true), "--json");
        assert!(!TerminalProgress::should_show(false, false, false), "redirected stderr");
    }

    #[test]
    fn long_lines_are_cut_to_the_terminal_width() {
        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("exactly ten", 11), "exactly ten");
        assert_eq!(truncate("a much longer line than fits", 10), "a much lo…");
        assert_eq!(truncate("anything", 0), "anything", "an unknown width does not truncate");
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        let line = "Añadir la lista de verificación de la primera semana";
        let cut = truncate(line, 12);
        assert_eq!(cut.chars().count(), 12);
        // The real property: the result is still valid UTF-8 we can index.
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_rendered_line_shows_the_stage_the_count_and_the_current_item() {
        assert_eq!(
            render_line("Writing", 12, Some(47), "Team Handbook/Onboarding.md", 80),
            "Writing 12/47  Team Handbook/Onboarding.md"
        );
    }

    #[test]
    fn a_stage_with_no_known_total_shows_a_bare_count() {
        assert_eq!(render_line("Listing pages", 0, None, "", 80), "Listing pages 0");
        assert_eq!(render_line("Listing pages", 5, None, "", 80), "Listing pages 5");
    }

    #[test]
    fn the_rendered_line_never_exceeds_the_terminal_width() {
        let line = render_line("Writing", 3, Some(9), &"deep/path/".repeat(20), 40);
        assert_eq!(line.chars().count(), 40);
    }

    #[test]
    fn counters_render_with_and_without_a_known_total() {
        let progress = TerminalProgress::new();

        progress.stage("Fetching", Some(3));
        assert_eq!(progress.total.load(Ordering::Relaxed), 3);
        assert_eq!(progress.has_total.load(Ordering::Relaxed), 1);

        progress.item("One.md");
        progress.item("Two.md");
        assert_eq!(progress.done.load(Ordering::Relaxed), 2);

        // A new stage restarts the count.
        progress.stage("Listing pages", None);
        assert_eq!(progress.done.load(Ordering::Relaxed), 0);
        assert_eq!(progress.has_total.load(Ordering::Relaxed), 0);
    }
}
