//! `confed tui` — the interactive layer.
//!
//! Nothing here implements sync behaviour: the TUI drives the same
//! [`SyncEngine`](confed_core::sync::SyncEngine) calls and the same resolution
//! helper the headless commands use, and renders what they return. Anything the
//! TUI can do, a script can do without it.

mod app;
mod conflict;
mod pane;
mod term;
mod tree;
mod ui;

use app::App;
use confed_cli::context::Context;
use confed_cli::output::Output;
use confed_core::error::{ConfedError, Result};
use crossterm::event::{self, Event, KeyEventKind};
use serde_json::json;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

/// How long the loop waits for a key before redrawing. Fast enough for a
/// spinner, slow enough to stay at zero CPU while nothing happens.
const TICK: Duration = Duration::from_millis(100);

pub fn run(mut ctx: Context) -> Result<Output> {
    require_terminal()?;
    ctx.workspace()?; // fail early with "run confed init" rather than on a blank screen

    // Credentials are resolved out here: the OS keyring blocks, and the TUI must
    // not stall its own event loop on it. A workspace with no session still
    // opens — browsing, diffing and resolving are entirely offline.
    let (client, engine) = match connect(&mut ctx) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!(target: "confed::tui", error = %e, "starting offline");
            (None, None)
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ConfedError::io("starting the async runtime", e))?;

    let workspace = ctx.take_workspace()?;
    let mut app = App::new(workspace, engine, client, Some(runtime.handle().clone()))?;

    let (mut guard, mut terminal) = term::TerminalGuard::enter()?;
    let outcome = event_loop(&mut terminal, &mut app);
    guard.restore();
    // Let an in-flight operation stop cleanly before the runtime goes away.
    app.shutdown();
    runtime.shutdown_timeout(Duration::from_millis(500));
    outcome?;

    Ok(Output::new(json!({ "space": app.space }), String::new()))
}

type Connection =
    (Option<Arc<dyn confed_api::ConfluenceClient>>, Option<Arc<confed_core::sync::SyncEngine>>);

/// Build a client only when this workspace has a stored session, so the TUI
/// never opens with a token prompt.
fn connect(ctx: &mut Context) -> Result<Connection> {
    if ctx.session()?.is_none() {
        return Ok((None, None));
    }
    ctx.preload_credentials()?;
    let client = ctx.build_client()?;
    let engine = ctx.engine(Arc::clone(&client))?;
    Ok((Some(client), Some(Arc::new(engine))))
}

fn event_loop(terminal: &mut term::Tui, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(|e| ConfedError::io("drawing the screen", e))?;

        if event::poll(TICK).map_err(|e| ConfedError::io("polling for input", e))? {
            match event::read().map_err(|e| ConfedError::io("reading input", e))? {
                // Windows reports both press and release; only act on press.
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                _ => {}
            }
        }
        app.poll_jobs();
    }
    Ok(())
}

/// The TUI is the one command that cannot fall back to plain output.
fn require_terminal() -> Result<()> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return Ok(());
    }
    Err(ConfedError::usage_with_hint(
        "`confed tui` needs an interactive terminal",
        "run it from a terminal; every action it offers is also a command \
         (status, diff, pull, push, resolve)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use confed_core::ExitCode;

    #[test]
    fn without_a_terminal_the_tui_exits_with_the_usage_code() {
        // The test harness never has a TTY on stdin, so this is the real path.
        let error = require_terminal().expect_err("no terminal in a test process");
        assert_eq!(error.exit_code(), ExitCode::Usage);
        assert!(error.to_string().contains("interactive terminal"));
        assert!(error.hint().unwrap().contains("resolve"));
    }
}
