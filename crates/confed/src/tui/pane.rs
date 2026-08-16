//! Content for the right-hand pane: a Markdown preview, or a diff.
//!
//! The diff is built exactly the way `confed diff` builds it — base storage
//! rendered back to Markdown, then a line diff against the working file — so the
//! TUI and the CLI can never disagree about what changed.

use confed_convert::ConvertOptions;
use confed_core::error::Result;
use confed_core::workspace::Workspace;
use similar::{ChangeTag, TextDiff};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Plain,
    Heading,
    /// Frontmatter and other tool-managed text.
    Meta,
    Hunk,
    Add,
    Del,
    Warn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
}

impl Line {
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }
}

/// Which side the diff pane is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    /// What `push` would send: base ⇄ working file.
    Base,
    /// What `pull` would bring in: last fetched remote ⇄ working file.
    Remote,
}

impl DiffSide {
    pub fn label(self) -> &'static str {
        match self {
            DiffSide::Base => "base ⇄ local",
            DiffSide::Remote => "remote ⇄ local",
        }
    }
}

/// The working file, with its frontmatter dimmed and headings highlighted.
pub fn preview(ws: &Workspace, path: &str) -> Vec<Line> {
    if path.is_empty() {
        return vec![Line::new(
            LineKind::Warn,
            "This page exists only on the server. Press p to pull it.",
        )];
    }
    let absolute = ws.absolute(path);
    let content = match std::fs::read_to_string(&absolute) {
        Ok(content) => content,
        Err(e) => return vec![Line::new(LineKind::Warn, format!("{path}: {e}"))],
    };
    render_markdown(&content)
}

pub fn render_markdown(content: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    // Frontmatter runs from a leading `---` to the next one.
    let mut in_frontmatter = content.starts_with("---");
    for (number, text) in content.lines().enumerate() {
        if in_frontmatter {
            lines.push(Line::new(LineKind::Meta, text));
            if number > 0 && text == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        let kind = if text.starts_with('#') {
            LineKind::Heading
        } else if text.starts_with("<<<<<<<")
            || text.starts_with("=======")
            || text.starts_with(">>>>>>>")
            || text.starts_with("|||||||")
        {
            LineKind::Warn
        } else {
            LineKind::Plain
        };
        lines.push(Line::new(kind, text));
    }
    lines
}

/// A unified diff of the selected page, or an explanation of why there is none.
pub fn diff(ws: &Workspace, page_id: &str, path: &str, side: DiffSide) -> Vec<Line> {
    match build_diff(ws, page_id, path, side) {
        Ok(lines) => lines,
        Err(e) => vec![Line::new(LineKind::Warn, e.to_string())],
    }
}

fn build_diff(ws: &Workspace, page_id: &str, path: &str, side: DiffSide) -> Result<Vec<Line>> {
    let Some(record) = ws.state().get_page(page_id)? else {
        return Ok(vec![Line::new(LineKind::Warn, "No base version yet — run `confed fetch`.")]);
    };

    let options = ConvertOptions {
        attachment_dir: confed_core::paths::sidecar_ref(path),
        base_url: ws.base_url()?.unwrap_or_default(),
        space_key: ws.space_key().unwrap_or_default(),
        ..Default::default()
    };

    let storage = match side {
        DiffSide::Base => Some(record.storage_body.clone()),
        DiffSide::Remote => ws.state().get_remote(page_id)?.and_then(|r| r.storage_body),
    };
    let Some(storage) = storage else {
        return Ok(vec![Line::new(
            LineKind::Warn,
            "No fetched remote body for this page — press f to fetch.",
        )]);
    };

    let left = confed_convert::storage_to_markdown(&storage, &options)?.markdown;
    let content = std::fs::read_to_string(ws.absolute(path)).unwrap_or_default();
    let right =
        confed_core::frontmatter::parse(&content, path).map(|file| file.body).unwrap_or(content);

    Ok(unified(&left, &right))
}

/// A unified diff with three lines of context, in the same shape as `confed diff`.
pub fn unified(left: &str, right: &str) -> Vec<Line> {
    let diff = TextDiff::from_lines(left, right);
    let mut lines = Vec::new();

    for group in diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
        let old = first.old_range().start..last.old_range().end;
        let new = first.new_range().start..last.new_range().end;
        lines.push(Line::new(
            LineKind::Hunk,
            format!("@@ -{},{} +{},{} @@", old.start + 1, old.len(), new.start + 1, new.len()),
        ));
        for op in &group {
            for change in diff.iter_changes(op) {
                let (kind, sign) = match change.tag() {
                    ChangeTag::Insert => (LineKind::Add, '+'),
                    ChangeTag::Delete => (LineKind::Del, '-'),
                    ChangeTag::Equal => (LineKind::Plain, ' '),
                };
                lines.push(Line::new(
                    kind,
                    format!("{sign}{}", change.value().trim_end_matches('\n')),
                ));
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::new(LineKind::Plain, "No differences."));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unified_diff_marks_both_sides() {
        let lines = unified("a\nb\nc\n", "a\nB\nc\n");
        let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(text, ["@@ -1,3 +1,3 @@", " a", "-b", "+B", " c"]);
        assert_eq!(lines[2].kind, LineKind::Del);
        assert_eq!(lines[3].kind, LineKind::Add);
    }

    #[test]
    fn identical_text_says_so_instead_of_rendering_nothing() {
        let lines = unified("same\n", "same\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "No differences.");
    }

    #[test]
    fn frontmatter_and_headings_are_classified() {
        let lines = render_markdown("---\ntitle: X\n---\n\n# Heading\n\nbody\n");
        assert_eq!(lines[0].kind, LineKind::Meta);
        assert_eq!(lines[1].kind, LineKind::Meta);
        assert_eq!(lines[2].kind, LineKind::Meta);
        assert_eq!(lines[4].kind, LineKind::Heading);
        assert_eq!(lines[6].kind, LineKind::Plain);
    }

    #[test]
    fn conflict_markers_stand_out_in_the_preview() {
        let lines = render_markdown("ok\n<<<<<<< local\nmine\n");
        assert_eq!(lines[1].kind, LineKind::Warn);
    }

    /// A 10k-line page must render in well under a frame.
    #[test]
    fn large_pages_render_quickly() {
        let big: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        let started = std::time::Instant::now();
        let lines = render_markdown(&big);
        assert_eq!(lines.len(), 10_000);
        assert!(started.elapsed().as_millis() < 500, "took {:?}", started.elapsed());
    }
}
