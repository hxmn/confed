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
    /// Failures that did not stop the command: the spaces an instance
    /// command could not do, each one entry, alongside what it did do.
    pub errors: Vec<ErrorEntry>,
}

impl Output {
    pub fn new(result: Value, human: impl Into<String>) -> Self {
        Self {
            result,
            human: human.into(),
            exit: ExitCode::Ok,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn with_errors(mut self, errors: Vec<ErrorEntry>) -> Self {
        self.errors = errors;
        self
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

#[derive(Clone, Debug, Serialize)]
pub struct ErrorEntry {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ErrorEntry {
    pub fn from_error(error: &ConfedError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
            hint: error.hint().map(str::to_string),
        }
    }

    /// The same failure, said of one space.
    pub fn in_space(mut self, space: &str) -> Self {
        self.message = format!("{space}: {}", self.message);
        self
    }
}

/// Write to stdout, and stop quietly when nobody reads any more: a closed
/// pipe (`confed … | head`) ends the output, not the process with a panic.
fn print(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}").and_then(|_| out.flush());
}

/// Print a successful command's output in the requested format; with
/// `select`, only that part of the envelope (see [`select`]).
pub fn emit(
    command: &str,
    output: &Output,
    json_mode: bool,
    select: Option<&str>,
    duration_ms: u128,
) {
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
            "errors": output.errors,
            "warnings": output.warnings,
        });
        let shown = match select {
            Some(path) => self::select(&envelope, path),
            None => envelope,
        };
        print(&serde_json::to_string_pretty(&shown).unwrap_or_default());
        return;
    }

    if !output.human.is_empty() {
        print(output.human.trim_end());
    }
    let mut stderr = std::io::stderr();
    for warning in &output.warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    for error in &output.errors {
        let _ = writeln!(stderr, "error: {}", error.message);
        if let Some(hint) = &error.hint {
            let _ = writeln!(stderr, "hint: {hint}");
        }
    }
}

/// Print a failed command in the requested format.
pub fn emit_error(command: &str, error: &ConfedError, json_mode: bool, duration_ms: u128) {
    let entry = ErrorEntry::from_error(error);

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
        print(&serde_json::to_string_pretty(&envelope).unwrap_or_default());
        return;
    }

    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "error: {}", entry.message);
    if let Some(hint) = entry.hint {
        let _ = writeln!(stderr, "hint: {hint}");
    }
}

/// The part of an envelope `--select` names.
///
/// A path is dot-separated keys into `result` — or into the envelope when
/// it starts with `confed`, `result`, `errors` or `warnings`. A number
/// indexes an array; `*` takes every element of an array (or value of an
/// object) and applies the rest of the path to each. Several paths separated
/// by commas give an object keyed by path. What is not there is `null`.
pub fn select(envelope: &Value, path: &str) -> Value {
    let one = |path: &str| {
        let path = path.trim();
        let top = path.split('.').next().unwrap_or_default();
        let (root, rest) = if ["confed", "result", "errors", "warnings"].contains(&top) {
            (envelope, path)
        } else {
            (&envelope["result"], path)
        };
        let segments: Vec<&str> = rest.split('.').filter(|s| !s.is_empty()).collect();
        walk(root, &segments)
    };
    if path.contains(',') {
        let mut out = serde_json::Map::new();
        for part in path.split(',').filter(|p| !p.trim().is_empty()) {
            out.insert(part.trim().to_string(), one(part));
        }
        Value::Object(out)
    } else {
        one(path)
    }
}

fn walk(value: &Value, segments: &[&str]) -> Value {
    let Some((first, rest)) = segments.split_first() else { return value.clone() };
    match (*first, value) {
        ("*", Value::Array(items)) => Value::Array(items.iter().map(|v| walk(v, rest)).collect()),
        ("*", Value::Object(map)) => Value::Array(map.values().map(|v| walk(v, rest)).collect()),
        (key, Value::Array(items)) => match key.parse::<usize>() {
            Ok(index) => items.get(index).map_or(Value::Null, |v| walk(v, rest)),
            Err(_) => Value::Null,
        },
        (key, Value::Object(map)) => map.get(key).map_or(Value::Null, |v| walk(v, rest)),
        _ => Value::Null,
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
    fn select_picks_parts_of_the_result_or_the_envelope() {
        let envelope = json!({
            "confed": { "exit_code": 0 },
            "result": { "spaces": [
                { "space_key": "DOCS", "result": { "clean": true } },
                { "space_key": "ENG", "result": { "clean": false } },
            ] },
            "warnings": ["careful"],
        });
        assert_eq!(select(&envelope, "spaces.*.space_key"), json!(["DOCS", "ENG"]));
        assert_eq!(select(&envelope, "spaces.1.result.clean"), json!(false));
        assert_eq!(select(&envelope, "result.spaces.0.space_key"), json!("DOCS"));
        assert_eq!(select(&envelope, "confed.exit_code"), json!(0));
        assert_eq!(
            select(&envelope, "spaces.*.space_key,warnings"),
            json!({ "spaces.*.space_key": ["DOCS", "ENG"], "warnings": ["careful"] })
        );
        assert_eq!(select(&envelope, "nothing.here"), Value::Null);
        assert_eq!(select(&envelope, "spaces.9"), Value::Null);
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
