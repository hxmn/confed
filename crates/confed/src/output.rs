//! Output: a stable JSON envelope for agents, readable text for people.
//!
//! Every command produces the same envelope shape under `--json`, versioned by
//! `confed.schema`, so scripts can rely on it across releases.

use confed_core::{ConfedError, ExitCode};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::Write;

/// Bumped only when the envelope or a command's `result` shape changes
/// incompatibly. Additive fields are not breaking.
pub const JSON_SCHEMA_VERSION: u32 = 1;

/// What a command produced.
#[derive(Debug)]
pub struct Output {
    pub result: Value,
    /// Human-readable rendering, already formatted.
    pub human: String,
    pub exit: ExitCode,
    pub warnings: Vec<String>,
}

impl Output {
    pub fn new(result: Value, human: impl Into<String>) -> Self {
        Self { result, human: human.into(), exit: ExitCode::Ok, warnings: Vec::new() }
    }

    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self::new(json!({}), String::new())
    }

    #[allow(dead_code)]
    pub fn with_exit(mut self, exit: ExitCode) -> Self {
        self.exit = exit;
        self
    }

    pub fn warn(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    pub fn warn_all(mut self, warnings: impl IntoIterator<Item = String>) -> Self {
        self.warnings.extend(warnings);
        self
    }

    /// Serialize a command result struct into the envelope's `result`.
    pub fn from_data<T: Serialize>(data: &T, human: impl Into<String>) -> Self {
        Self::new(serde_json::to_value(data).unwrap_or_else(|_| json!({})), human)
    }
}

#[derive(Serialize)]
struct ErrorEntry {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

/// Print a successful command's output in the requested format.
pub fn emit(command: &str, output: &Output, json_mode: bool, duration_ms: u128) {
    if json_mode {
        let envelope = json!({
            "confed": {
                "schema": JSON_SCHEMA_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
                "command": command,
                "ok": output.exit == ExitCode::Ok || output.exit == ExitCode::Differences,
                "exit_code": output.exit.as_i32(),
                "duration_ms": duration_ms,
            },
            "result": output.result,
            "errors": Vec::<ErrorEntry>::new(),
            "warnings": output.warnings,
        });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap_or_default());
        return;
    }

    if !output.human.is_empty() {
        println!("{}", output.human.trim_end());
    }
    let mut stderr = std::io::stderr();
    for warning in &output.warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
}

/// Print a failed command in the requested format.
pub fn emit_error(command: &str, error: &ConfedError, json_mode: bool, duration_ms: u128) {
    let entry = ErrorEntry {
        code: error.code(),
        message: error.to_string(),
        hint: error.hint().map(str::to_string),
    };

    if json_mode {
        let envelope = json!({
            "confed": {
                "schema": JSON_SCHEMA_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
                "command": command,
                "ok": false,
                "exit_code": error.exit_code().as_i32(),
                "duration_ms": duration_ms,
            },
            "result": Value::Null,
            "errors": [entry],
            "warnings": Vec::<String>::new(),
        });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap_or_default());
        return;
    }

    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "error: {}", entry.message);
    if let Some(hint) = entry.hint {
        let _ = writeln!(stderr, "hint: {hint}");
    }
}

/// ANSI colors, suppressed when not writing to a terminal or when NO_COLOR is set.
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn detect(json_mode: bool) -> Self {
        use std::io::IsTerminal;
        Self {
            enabled: !json_mode
                && std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    #[allow(dead_code)]
    pub fn plain() -> Self {
        Self { enabled: false }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn green(&self, text: &str) -> String {
        self.paint("32", text)
    }
    pub fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }
    pub fn yellow(&self, text: &str) -> String {
        self.paint("33", text)
    }
    pub fn blue(&self, text: &str) -> String {
        self.paint("34", text)
    }
    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }
}

/// "1 page", "3 pages" — small thing, but error messages read badly without it.
pub fn plural(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_are_inert_when_disabled() {
        let style = Style::plain();
        assert_eq!(style.green("ok"), "ok");
        assert_eq!(style.red("bad"), "bad");
    }

    #[test]
    fn plurals_read_naturally() {
        assert_eq!(plural(1, "page", "pages"), "1 page");
        assert_eq!(plural(0, "page", "pages"), "0 pages");
        assert_eq!(plural(3, "page", "pages"), "3 pages");
    }

    #[test]
    fn output_carries_warnings_and_exit_codes() {
        let out =
            Output::new(json!({"a": 1}), "human").with_exit(ExitCode::Differences).warn("careful");
        assert_eq!(out.exit, ExitCode::Differences);
        assert_eq!(out.warnings, ["careful"]);
        assert_eq!(out.result["a"], 1);
    }
}
