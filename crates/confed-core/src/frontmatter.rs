//! The Markdown file format: YAML frontmatter + body.
//!
//! Frontmatter has two halves. The top-level keys `title`, `labels` and
//! `parent_id` are the user's to edit and are synced to the server. Everything
//! under `confed:` is managed by the tool — hand edits are detected and refused
//! rather than silently overwritten. Unknown top-level keys are preserved, so
//! teams can keep their own metadata alongside.

use crate::error::{ConfedError, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

/// Bumped when the frontmatter contract changes in a way tools must notice.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: String,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// The `confed:` block. Written by confed, read-only for everyone else.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Managed {
    pub schema: u32,
    pub page_id: String,
    pub space_key: String,
    /// Base version: the server version this file was last synced with.
    pub version: u32,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
}

impl Managed {
    pub fn new(page_id: impl Into<String>, space_key: impl Into<String>, version: u32) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            page_id: page_id.into(),
            space_key: space_key.into(),
            version,
            status: "current".into(),
            position: None,
            created: None,
            updated: None,
            author: None,
            attachments: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frontmatter {
    // Writable by the user.
    pub title: String,
    pub labels: Vec<String>,
    pub parent_id: Option<String>,
    /// Absent for a page that does not exist on the server yet.
    pub managed: Option<Managed>,
    /// Any other top-level keys, preserved verbatim in their original order.
    pub extra: Mapping,
}

impl Frontmatter {
    pub fn new_local(title: impl Into<String>) -> Self {
        Self { title: title.into(), ..Default::default() }
    }

    pub fn page_id(&self) -> Option<&str> {
        self.managed.as_ref().map(|m| m.page_id.as_str())
    }

    pub fn version(&self) -> Option<u32> {
        self.managed.as_ref().map(|m| m.version)
    }

    /// A page confed has never seen on the server.
    pub fn is_new(&self) -> bool {
        self.managed.is_none()
    }
}

/// A parsed `.md` file.
#[derive(Clone, Debug, PartialEq)]
pub struct MarkdownFile {
    pub frontmatter: Frontmatter,
    pub body: String,
}

impl MarkdownFile {
    pub fn new(frontmatter: Frontmatter, body: impl Into<String>) -> Self {
        Self { frontmatter, body: body.into() }
    }

    /// Hash of everything the user controls: writable frontmatter plus body.
    ///
    /// Deliberately excludes the `confed:` block so that a version bump written
    /// by push does not make the file look locally modified, and so YAML
    /// formatting noise (quoting, key order, label order) does not either.
    pub fn content_hash(&self) -> String {
        let fm = &self.frontmatter;
        let mut labels = fm.labels.clone();
        labels.sort();
        labels.dedup();

        let mut hasher = Sha256::new();
        hasher.update(b"title\0");
        hasher.update(fm.title.trim().as_bytes());
        hasher.update(b"\0labels\0");
        hasher.update(labels.join("\u{1}").as_bytes());
        hasher.update(b"\0parent\0");
        hasher.update(fm.parent_id.clone().unwrap_or_default().as_bytes());
        hasher.update(b"\0extra\0");
        hasher.update(
            serde_yaml::to_string(&Value::Mapping(fm.extra.clone()))
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update(b"\0body\0");
        // Normalize line endings and the trailing newline: neither is content.
        hasher.update(self.body.replace("\r\n", "\n").trim_end().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Serialize to the on-disk form, with a stable key order.
    pub fn render(&self) -> Result<String> {
        let fm = &self.frontmatter;
        let mut map = Mapping::new();
        map.insert(Value::from("title"), Value::from(fm.title.clone()));
        map.insert(
            Value::from("labels"),
            Value::Sequence(fm.labels.iter().map(|l| Value::from(l.clone())).collect()),
        );
        if let Some(parent) = &fm.parent_id {
            map.insert(Value::from("parent_id"), Value::from(parent.clone()));
        }
        for (k, v) in &fm.extra {
            map.insert(k.clone(), v.clone());
        }
        if let Some(managed) = &fm.managed {
            map.insert(Value::from("confed"), serde_yaml::to_value(managed)?);
        }

        let yaml = serde_yaml::to_string(&Value::Mapping(map))?;
        let body = self.body.trim_start_matches('\n');
        let mut out = String::with_capacity(yaml.len() + body.len() + 16);
        out.push_str("---\n");
        out.push_str(&yaml);
        if !yaml.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("---\n\n");
        out.push_str(body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}

/// Split a file into its raw YAML frontmatter and body.
///
/// Returns `Ok(None)` when the file has no frontmatter at all — such files are
/// not confed pages and are skipped by the scanner rather than treated as errors.
pub fn split(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let rest = content.strip_prefix("---\r\n").or_else(|| content.strip_prefix("---\n"))?;

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" || trimmed == "..." {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((yaml, body.strip_prefix('\n').unwrap_or(body)));
        }
        offset += line.len();
    }
    None
}

/// Parse a `.md` file. `path` is only used for error messages.
pub fn parse(content: &str, path: &str) -> Result<MarkdownFile> {
    let (yaml, body) = split(content).ok_or_else(|| {
        ConfedError::state_with_hint(
            format!("{path}: no YAML frontmatter found"),
            "confed pages start with a `---` frontmatter block; use `confed new` to scaffold one",
        )
    })?;

    let value: Value = if yaml.trim().is_empty() {
        Value::Mapping(Mapping::new())
    } else {
        serde_yaml::from_str(yaml)?
    };
    let mut map = match value {
        Value::Mapping(m) => m,
        _ => {
            return Err(ConfedError::state(format!(
                "{path}: frontmatter must be a YAML mapping"
            )))
        }
    };

    let title = match map.remove(Value::from("title")) {
        Some(Value::String(s)) => s,
        Some(other) => {
            return Err(ConfedError::state(format!(
                "{path}: `title` must be a string, found {other:?}"
            )))
        }
        None => {
            return Err(ConfedError::state_with_hint(
                format!("{path}: frontmatter is missing `title`"),
                "add a `title:` line, or run `confed pull --force` on this page to restore it",
            ))
        }
    };

    let labels = match map.remove(Value::from("labels")) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Sequence(seq)) => seq
            .into_iter()
            .map(|v| match v {
                Value::String(s) => Ok(s),
                other => Err(ConfedError::state(format!(
                    "{path}: labels must be strings, found {other:?}"
                ))),
            })
            .collect::<Result<Vec<_>>>()?,
        Some(other) => {
            return Err(ConfedError::state(format!(
                "{path}: `labels` must be a list, found {other:?}"
            )))
        }
    };

    let parent_id = match map.remove(Value::from("parent_id")) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(other) => {
            return Err(ConfedError::state(format!(
                "{path}: `parent_id` must be a string, found {other:?}"
            )))
        }
    };

    let managed = match map.remove(Value::from("confed")) {
        None | Some(Value::Null) => None,
        Some(value) => Some(serde_yaml::from_value::<Managed>(value).map_err(|e| {
            ConfedError::state_with_hint(
                format!("{path}: the `confed:` block is malformed: {e}"),
                "the `confed:` block is tool-managed; `confed pull --force <page>` will rebuild it",
            )
        })?),
    };

    Ok(MarkdownFile {
        frontmatter: Frontmatter { title, labels, parent_id, managed, extra: map },
        body: body.to_string(),
    })
}

/// Differences between a file's managed block and what confed recorded at the
/// last sync. Any of these means somebody hand-edited tool-managed state.
#[derive(Clone, Debug, PartialEq)]
pub struct Tampering {
    pub field: &'static str,
    pub expected: String,
    pub found: String,
}

/// Detect hand edits to the managed block. `expected` comes from `.state.db`.
pub fn detect_tampering(file: &Managed, expected: &Managed) -> Vec<Tampering> {
    let mut out = Vec::new();
    let mut check = |field: &'static str, expected: String, found: String| {
        if expected != found {
            out.push(Tampering { field, expected, found });
        }
    };
    check("page_id", expected.page_id.clone(), file.page_id.clone());
    check("space_key", expected.space_key.clone(), file.space_key.clone());
    check("version", expected.version.to_string(), file.version.to_string());
    check("schema", expected.schema.to_string(), file.schema.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\ntitle: Onboarding\nlabels:\n  - hr\n  - onboarding\nparent_id: '163841'\nteam: platform\nconfed:\n  schema: 1\n  page_id: '163842'\n  space_key: DOCS\n  version: 7\n  status: current\n  attachments:\n    - id: att901\n      file: diagram.png\n      size: 48213\n---\n\n# Onboarding\n\nFirst week checklist.\n";

    #[test]
    fn parses_the_documented_shape() {
        let file = parse(SAMPLE, "Onboarding.md").unwrap();
        let fm = &file.frontmatter;
        assert_eq!(fm.title, "Onboarding");
        assert_eq!(fm.labels, ["hr", "onboarding"]);
        assert_eq!(fm.parent_id.as_deref(), Some("163841"));
        let managed = fm.managed.as_ref().unwrap();
        assert_eq!(managed.page_id, "163842");
        assert_eq!(managed.version, 7);
        assert_eq!(managed.attachments[0].file, "diagram.png");
        assert!(file.body.starts_with("# Onboarding"));
    }

    #[test]
    fn unknown_top_level_keys_survive_a_round_trip() {
        let file = parse(SAMPLE, "x.md").unwrap();
        assert!(file.frontmatter.extra.contains_key(Value::from("team")));
        let rendered = file.render().unwrap();
        assert!(rendered.contains("team: platform"));
        assert_eq!(parse(&rendered, "x.md").unwrap(), file);
    }

    #[test]
    fn render_then_parse_is_stable() {
        let file = parse(SAMPLE, "x.md").unwrap();
        let once = file.render().unwrap();
        let twice = parse(&once, "x.md").unwrap().render().unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn content_hash_ignores_managed_fields_and_yaml_noise() {
        let mut file = parse(SAMPLE, "x.md").unwrap();
        let before = file.content_hash();

        // A version bump from push must not look like a local edit.
        file.frontmatter.managed.as_mut().unwrap().version = 99;
        assert_eq!(file.content_hash(), before);

        // Neither does reordering labels or a trailing newline.
        file.frontmatter.labels.reverse();
        file.body.push_str("\n\n");
        assert_eq!(file.content_hash(), before);

        // Real edits do change it.
        file.body.push_str("new sentence\n");
        assert_ne!(file.content_hash(), before);
    }

    #[test]
    fn content_hash_tracks_writable_frontmatter() {
        let base = parse(SAMPLE, "x.md").unwrap();
        let mut renamed = base.clone();
        renamed.frontmatter.title = "Onboarding v2".into();
        assert_ne!(renamed.content_hash(), base.content_hash());

        let mut relabeled = base.clone();
        relabeled.frontmatter.labels.push("new".into());
        assert_ne!(relabeled.content_hash(), base.content_hash());

        let mut moved = base.clone();
        moved.frontmatter.parent_id = Some("999".into());
        assert_ne!(moved.content_hash(), base.content_hash());
    }

    #[test]
    fn files_without_a_managed_block_are_new_pages() {
        let content = "---\ntitle: Draft\n---\n\nbody\n";
        let file = parse(content, "Draft.md").unwrap();
        assert!(file.frontmatter.is_new());
        assert_eq!(file.frontmatter.page_id(), None);
    }

    #[test]
    fn missing_title_explains_how_to_fix_it() {
        let err = parse("---\nlabels: []\n---\nbody\n", "x.md").unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::State);
        assert!(err.hint().unwrap().contains("confed"));
    }

    #[test]
    fn missing_frontmatter_is_a_state_error_not_a_panic() {
        let err = parse("# Just markdown\n", "x.md").unwrap_err();
        assert!(err.to_string().contains("no YAML frontmatter"));
    }

    #[test]
    fn crlf_and_bom_are_tolerated() {
        let content = "\u{feff}---\r\ntitle: Windows\r\n---\r\n\r\nbody\r\n";
        let file = parse(content, "x.md").unwrap();
        assert_eq!(file.frontmatter.title, "Windows");
        assert!(file.body.contains("body"));
    }

    #[test]
    fn tampering_with_managed_fields_is_detected() {
        let expected = Managed::new("163842", "DOCS", 7);
        let mut edited = expected.clone();
        edited.page_id = "999".into();
        edited.version = 42;

        let found = detect_tampering(&edited, &expected);
        let fields: Vec<_> = found.iter().map(|t| t.field).collect();
        assert!(fields.contains(&"page_id"));
        assert!(fields.contains(&"version"));
        assert!(detect_tampering(&expected, &expected).is_empty());
    }

    #[test]
    fn body_only_documents_split_correctly() {
        assert_eq!(split("---\ntitle: x\n---\nbody\n"), Some(("title: x\n", "body\n")));
        assert_eq!(split("no frontmatter"), None);
        // An unterminated block is not frontmatter.
        assert_eq!(split("---\ntitle: x\n"), None);
    }
}
