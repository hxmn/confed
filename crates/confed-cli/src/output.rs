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
    /// What was refused, when confed names it (`key_taken`, `no_permission`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ErrorEntry {
    pub fn from_error(error: &ConfedError) -> Self {
        Self {
            code: error.code(),
            kind: error.kind(),
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
/// indexes an array; `*` takes every element of an array, or every entry of
/// an object, and applies the rest of the path to each: an array stays an
/// array and an object keeps its keys. `[field=value]` after a key (or after
/// `*`, or alone) does the same for the elements that match only:
/// `permissions[subject.name=writers].operation`. Several paths separated by
/// commas give an object keyed by path. What is not there is `null`.
pub fn select(envelope: &Value, path: &str) -> Value {
    let one = |path: &str| {
        let Ok(segments) = parse_path(path) else { return Value::Null };
        let top = segments.first().map(|s| s.key).unwrap_or_default();
        let root = if ["confed", "result", "errors", "warnings"].contains(&top) {
            envelope
        } else {
            &envelope["result"]
        };
        walk(root, &segments)
    };
    let parts = split_top(path, ',');
    if parts.len() > 1 {
        let mut out = serde_json::Map::new();
        for part in parts.iter().filter(|p| !p.trim().is_empty()) {
            out.insert(part.trim().to_string(), one(part));
        }
        Value::Object(out)
    } else {
        one(path)
    }
}

/// Whether `--select` can read `path`, said before the command runs: a
/// filter that is never closed or compares nothing is a mistake to report,
/// not a `null` to print after the work is done.
pub fn check_select(path: &str) -> Result<(), String> {
    for part in split_top(path, ',') {
        parse_path(part)?;
    }
    Ok(())
}

/// One step of a path: a key (or `*`, or nothing), then the filters on what
/// it names.
struct Segment<'a> {
    key: &'a str,
    filters: Vec<Filter<'a>>,
}

/// `[field=value]` or `[field!=value]`: `field` is a dot path into each
/// element, `value` is compared as text.
struct Filter<'a> {
    field: Vec<&'a str>,
    value: &'a str,
    equal: bool,
}

impl Filter<'_> {
    fn matches(&self, element: &Value) -> bool {
        let mut found = Some(element);
        for key in &self.field {
            found = found.and_then(|v| step(v, key));
        }
        let is = |v: &Value| match v {
            Value::String(text) => text == self.value,
            Value::Null => self.value == "null",
            Value::Bool(flag) => self.value.parse() == Ok(*flag),
            Value::Number(number) => number.to_string() == self.value,
            Value::Array(_) | Value::Object(_) => false,
        };
        // A list matches by any of its elements: `[operations=space:read]`.
        let equal = match found {
            Some(Value::Array(items)) => items.iter().any(is),
            Some(value) => is(value),
            None => false,
        };
        equal == self.equal
    }
}

/// `text` cut at each `separator` that is outside `[…]` and outside quotes.
fn split_top(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0usize, None, 0usize);
    for (at, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') if depth > 0 => quote = Some(c),
            (None, '[') => depth += 1,
            (None, ']') => depth = depth.saturating_sub(1),
            (None, _) if c == separator && depth == 0 => {
                parts.push(&text[start..at]);
                start = at + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

fn parse_path(path: &str) -> Result<Vec<Segment<'_>>, String> {
    split_top(path.trim(), '.').into_iter().filter(|s| !s.is_empty()).map(parse_segment).collect()
}

fn parse_segment(text: &str) -> Result<Segment<'_>, String> {
    let Some(open) = text.find('[') else {
        return match text.contains(']') {
            true => Err(format!("`{text}`: a `]` with no `[` before it")),
            false => Ok(Segment { key: text, filters: Vec::new() }),
        };
    };
    let (key, mut rest) = text.split_at(open);
    let mut filters = Vec::new();
    while !rest.is_empty() {
        let unclosed = || format!("`{text}`: a filter is written [field=value]");
        let inner = rest.strip_prefix('[').ok_or_else(unclosed)?;
        // The first `]` that is not inside a quoted value.
        let mut quote = None;
        let close = inner.char_indices().find_map(|(at, c)| {
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (None, '"' | '\'') => quote = Some(c),
                (None, ']') => return Some(at),
                _ => {}
            }
            None
        });
        let close = close.ok_or_else(unclosed)?;
        let condition = &inner[..close];
        rest = &inner[close + 1..];
        let (field, value, equal) = match (condition.split_once("!="), condition.split_once('=')) {
            (Some((field, value)), Some((first, _))) if first.len() > field.len() => {
                (field, value, false)
            }
            (_, Some((field, value))) => (field, value, true),
            _ => return Err(unclosed()),
        };
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
            .unwrap_or(value);
        let field: Vec<&str> = field.trim().split('.').filter(|k| !k.is_empty()).collect();
        if field.is_empty() {
            return Err(unclosed());
        }
        filters.push(Filter { field, value, equal });
    }
    Ok(Segment { key, filters })
}

/// One key into an object, or one index into an array.
fn step<'v>(value: &'v Value, key: &str) -> Option<&'v Value> {
    match value {
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        Value::Object(map) => map.get(key),
        _ => None,
    }
}

fn walk(value: &Value, segments: &[Segment<'_>]) -> Value {
    let Some((first, rest)) = segments.split_first() else { return value.clone() };
    let inner = match first.key {
        "*" | "" => Some(value),
        key => step(value, key),
    };
    let Some(inner) = inner else { return Value::Null };
    if first.key != "*" && first.filters.is_empty() {
        return walk(inner, rest);
    }
    let keep = |element: &Value| first.filters.iter().all(|f| f.matches(element));
    match inner {
        Value::Array(items) => {
            Value::Array(items.iter().filter(|v| keep(v)).map(|v| walk(v, rest)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter().filter(|(_, v)| keep(v)).map(|(k, v)| (k.clone(), walk(v, rest))).collect(),
        ),
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
    fn select_keeps_the_keys_of_an_object() {
        let envelope = json!({
            "confed": { "exit_code": 0, "ok": true },
            "result": { "by_space": { "DOCS": { "pages": 4 }, "ENG": { "pages": 2 } } },
        });
        assert_eq!(select(&envelope, "by_space.*.pages"), json!({ "DOCS": 4, "ENG": 2 }));
        assert_eq!(select(&envelope, "confed.*"), json!({ "exit_code": 0, "ok": true }));
        assert_eq!(select(&envelope, "*"), envelope["result"]);
        assert_eq!(select(&envelope, "by_space[pages=4]"), json!({ "DOCS": { "pages": 4 } }));
    }

    #[test]
    fn select_filters_a_list_by_a_field_of_its_elements() {
        let envelope = json!({
            "result": { "permissions": [
                { "subject": { "type": "group", "name": "docs-team" }, "operation": "space:read" },
                { "subject": { "type": "group", "name": "docs-team" }, "operation": "page:create" },
                { "subject": { "type": "user", "name": "Alice Ng" }, "operation": "space:read" },
                { "subject": { "type": "anonymous", "name": null }, "operation": "space:read" },
            ], "subjects": [
                { "subject": "group:a.b", "operations": ["space:read", "page:create"], "count": 2 },
                { "subject": "user:x,y", "operations": ["space:read"], "count": 1 },
            ] },
        });
        assert_eq!(
            select(&envelope, "permissions[subject.name=docs-team].operation"),
            json!(["space:read", "page:create"])
        );
        assert_eq!(
            select(&envelope, "permissions[subject.type=group][operation!=space:read].operation"),
            json!(["page:create"])
        );
        assert_eq!(
            select(&envelope, "permissions[subject.name=null].subject.type"),
            json!(["anonymous"])
        );
        assert_eq!(select(&envelope, "permissions[subject.name=nobody]"), json!([]));
        // A value with a dot, a comma or a space in it: no quotes needed for
        // the first two, and quotes for anything.
        assert_eq!(select(&envelope, "subjects[subject=group:a.b].count"), json!([2]));
        assert_eq!(select(&envelope, "subjects[subject=user:x,y].count"), json!([1]));
        assert_eq!(
            select(&envelope, "permissions[subject.name=\"Alice Ng\"].operation"),
            json!(["space:read"])
        );
        // A number is compared as written; a list matches by any element.
        assert_eq!(select(&envelope, "subjects[count=2].subject"), json!(["group:a.b"]));
        assert_eq!(
            select(&envelope, "subjects[operations=page:create].subject"),
            json!(["group:a.b"])
        );
        assert_eq!(select(&envelope, "subjects.*[count!=2].subject"), json!(["user:x,y"]));
        assert_eq!(
            select(&envelope, "subjects[count=1].subject,permissions.0.operation"),
            json!({ "subjects[count=1].subject": ["user:x,y"], "permissions.0.operation": "space:read" })
        );
    }

    #[test]
    fn a_select_that_cannot_be_read_is_said_before_the_command_runs() {
        for good in ["spaces.*.result.clean", "a[b=c].d,e", "a[b.c!=d][e=f]", "*", "a[b=]"] {
            assert!(check_select(good).is_ok(), "{good}");
        }
        for bad in ["a[b=c", "a[b]", "a]b", "a[=c]", "a[b=c]d"] {
            assert!(check_select(bad).is_err(), "{bad}");
        }
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
