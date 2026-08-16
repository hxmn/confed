//! Terminal setup and teardown.
//!
//! Two things must hold no matter how the TUI ends: raw mode is disabled and the
//! alternate screen is left. A guard covers the normal and error paths, and a
//! panic hook covers the rest — a stack trace printed into a raw-mode terminal
//! leaves a shell the user has to `reset`.

use confed_core::error::{ConfedError, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Restores the terminal on drop, whichever way the TUI exits.
pub struct TerminalGuard {
    restored: bool,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen, and arm the panic hook.
    pub fn enter() -> Result<(Self, Tui)> {
        install_panic_hook(restore_terminal);

        enable_raw_mode().map_err(|e| ConfedError::io("switching the terminal to raw mode", e))?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)
            .map_err(|e| ConfedError::io("entering the alternate screen", e))?;

        let terminal = Terminal::new(CrosstermBackend::new(stdout()))
            .map_err(|e| ConfedError::io("initializing the terminal", e))?;
        Ok((Self { restored: false }, terminal))
    }

    /// Put the terminal back. Safe to call more than once.
    pub fn restore(&mut self) {
        if !self.restored {
            self.restored = true;
            restore_terminal();
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Every step is best-effort: during a panic there is nobody left to tell.
fn restore_terminal() {
    // Raw mode first — if anything below fails, the shell is at least usable.
    let _ = disable_raw_mode();
    let _ = stdout().execute(DisableMouseCapture);
    let _ = stdout().execute(LeaveAlternateScreen);
    let _ = stdout().execute(crossterm::cursor::Show);
}

/// Chain a restore step in front of the current panic hook, so the panic message
/// lands on a terminal that can display it.
pub fn install_panic_hook(restore: impl Fn() + Send + Sync + 'static) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// The hook is process-global, so this test installs a silent one, induces a
    /// panic, and puts the previous hook back.
    #[test]
    fn a_panic_restores_the_terminal_before_it_unwinds() {
        let restored = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&restored);

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // keep the test output clean
        install_panic_hook(move || flag.store(true, Ordering::SeqCst));

        let outcome = std::panic::catch_unwind(|| panic!("simulated TUI panic"));

        std::panic::set_hook(previous);
        assert!(outcome.is_err(), "the panic must still propagate");
        assert!(restored.load(Ordering::SeqCst), "the terminal must be restored first");
    }

    #[test]
    fn the_guard_restores_only_once() {
        let mut guard = TerminalGuard { restored: false };
        guard.restore();
        assert!(guard.restored);
        guard.restore(); // must not panic or double-restore
    }
}
