//! Interactive prompts. Only ever reached when stdin is a TTY and neither
//! `--json` nor `--non-interactive` was given; everything else fails fast.

use confed_core::config::{Prompter, ValueSpec};
use confed_core::error::{ConfedError, Result};
use std::io::{BufRead, Write};

pub struct TtyPrompter;

impl Prompter for TtyPrompter {
    fn prompt(&self, spec: &ValueSpec) -> Result<Option<String>> {
        if spec.secret {
            return read_secret(&format!("{} ({}): ", spec.name, spec.env)).map(Some);
        }
        read_line(&format!("{}: ", spec.name)).map(Some)
    }
}

/// Prompt on stderr so stdout stays clean for piping.
pub fn read_line(prompt: &str) -> Result<String> {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| ConfedError::io("reading from stdin", e))?;
    Ok(line.trim().to_string())
}

/// Read without echoing. Falls back to an echoing read if the terminal will not
/// go into raw mode, so confed still works over odd transports.
pub fn read_secret(prompt: &str) -> Result<String> {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();

    if crossterm::terminal::enable_raw_mode().is_err() {
        let _ = writeln!(stderr);
        return read_line("");
    }

    let mut secret = String::new();
    loop {
        match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(key)) => {
                use crossterm::event::{KeyCode, KeyModifiers};
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => break,
                    KeyCode::Backspace => {
                        secret.pop();
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = crossterm::terminal::disable_raw_mode();
                        let _ = writeln!(stderr);
                        return Err(ConfedError::usage("cancelled"));
                    }
                    KeyCode::Char(c) => secret.push(c),
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(e) => {
                let _ = crossterm::terminal::disable_raw_mode();
                return Err(ConfedError::io("reading from the terminal", e));
            }
        }
    }
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = writeln!(stderr);
    Ok(secret)
}

/// Yes/no confirmation. `default_yes` is what an empty answer means.
pub fn confirm(question: &str, default_yes: bool) -> Result<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    let answer = read_line(&format!("{question} {suffix} "))?;
    Ok(match answer.to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}

/// Pick one item from a list, by number.
pub fn select(prompt: &str, options: &[String]) -> Result<usize> {
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "{prompt}");
    for (i, option) in options.iter().enumerate() {
        let _ = writeln!(stderr, "  {:>3}. {option}", i + 1);
    }
    loop {
        let answer = read_line("Choose a number: ")?;
        match answer.parse::<usize>() {
            Ok(n) if n >= 1 && n <= options.len() => return Ok(n - 1),
            _ => {
                let _ = writeln!(stderr, "Enter a number between 1 and {}.", options.len());
            }
        }
    }
}
