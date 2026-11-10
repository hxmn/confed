//! Terminal setup and teardown.
//!
//! Two things must hold no matter how the TUI ends: raw mode is disabled and the
//! alternate screen is left. A guard covers the normal and error paths, and a
//! panic hook covers the rest — a stack trace printed into a raw-mode terminal
//! leaves a shell the user has to `reset`.

use confed_core::error::{ConfedError, Result};
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
        // The mouse is deliberately not captured: the terminal's own selection
        // and copy keep working, which matters more here than click handling.
        execute!(stdout(), EnterAlternateScreen)
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
    let _ = stdout().execute(LeaveAlternateScreen);
    let _ = stdout().execute(crossterm::cursor::Show);
}

/// Chain a restore step in front of the current panic hook, so the panic message
/// lands on a terminal that can display it.
///
/// Only for a panic on the thread that installed it — the UI's. A worker
/// that panics is reported by the UI, which is still drawing: restoring the
/// terminal under it, or printing a backtrace over it, would leave a live UI
/// on a terminal in cooked mode. The worker's message is kept instead, for
/// [`take_worker_panic`].
pub fn install_panic_hook(restore: impl Fn() + Send + Sync + 'static) {
    let ui = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui {
            restore();
            previous(info);
        } else {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic".to_string());
            *WORKER_PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
        }
    }));
}

static WORKER_PANIC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// What the last worker thread that panicked said, once.
pub fn take_worker_panic() -> Option<String> {
    WORKER_PANIC.lock().unwrap_or_else(|e| e.into_inner()).take()
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
        assert!(outcome.is_err(), "the panic must still propagate");
        assert!(restored.load(Ordering::SeqCst), "the terminal must be restored first");

        // A worker's panic leaves the terminal to the UI, which is still on
        // it, and is kept for the UI to report. In the same test: the hook is
        // process-wide.
        restored.store(false, Ordering::SeqCst);
        let worker = std::thread::spawn(|| panic!("simulated worker panic")).join();
        std::panic::set_hook(previous);
        assert!(worker.is_err());
        assert!(!restored.load(Ordering::SeqCst), "the UI's terminal is left alone");
        assert_eq!(take_worker_panic().as_deref(), Some("simulated worker panic"));
        assert_eq!(take_worker_panic(), None, "reported once");
    }

    #[test]
    fn the_guard_restores_only_once() {
        let mut guard = TerminalGuard { restored: false };
        guard.restore();
        assert!(guard.restored);
        guard.restore(); // must not panic or double-restore
    }
}
