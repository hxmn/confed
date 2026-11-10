//! Inline comment marks in page bodies: which threads show, and keeping the
//! marks in a file in step with the comments.

use super::*;

/// How inline comment marks are written into page bodies (design 06 §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarksMode {
    /// `<!--c 77120 Alice Ng: Link the template?-->…<!--/c 77120-->`
    Full,
    /// `<!--c 77120-->…<!--/c 77120-->`
    Ids,
    /// No marks in the body; the sidecar alone.
    Off,
}

impl MarksMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "full" => Some(MarksMode::Full),
            "ids" => Some(MarksMode::Ids),
            "off" => Some(MarksMode::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MarksMode::Full => "full",
            MarksMode::Ids => "ids",
            MarksMode::Off => "off",
        }
    }
}

/// Workspace setting that selects the [`MarksMode`].
pub const MARKS_MODE_KEY: &str = "comments.marks";

pub fn marks_mode(ws: &Workspace) -> MarksMode {
    ws.state()
        .get_meta(MARKS_MODE_KEY)
        .ok()
        .flatten()
        .and_then(|v| MarksMode::parse(&v))
        .unwrap_or(MarksMode::Full)
}

/// A page's open inline comments, keyed by Confluence marker ref, for the
/// converter to place as marks.
pub fn inline_marks_for(ws: &Workspace, page_id: &str) -> Result<HashMap<String, InlineMark>> {
    let mode = marks_mode(ws);
    if mode == MarksMode::Off {
        return Ok(HashMap::new());
    }
    let records = ws.state().page_comments(page_id)?;
    Ok(live_inline(&records)
        .filter_map(|(record, anchor)| {
            let marker_ref = anchor.marker_ref?;
            Some((
                marker_ref,
                InlineMark {
                    id: record.comment_id.clone(),
                    preview: mark_preview(record, &records, mode),
                },
            ))
        })
        .collect())
}

pub(super) fn mark_preview(
    record: &CommentRecord,
    all: &[CommentRecord],
    mode: MarksMode,
) -> String {
    if mode != MarksMode::Full {
        return String::new();
    }
    let replies = all
        .iter()
        .filter(|c| c.parent_comment_id.as_deref() == Some(record.comment_id.as_str()))
        .count();
    marks::preview(record.author.as_deref(), &record.body_markdown, replies)
}

/// Root inline comments that are still open, with their anchors.
pub(super) fn live_inline(
    records: &[CommentRecord],
) -> impl Iterator<Item = (&CommentRecord, confed_api::InlineAnchor)> {
    records
        .iter()
        .filter(|c| c.kind == "inline" && !c.resolved && c.parent_comment_id.is_none())
        .filter_map(|c| {
            let anchor: confed_api::InlineAnchor =
                serde_json::from_str(c.anchor.as_deref()?).ok()?;
            Some((c, anchor))
        })
}

/// The stretch of a stripped body a `new` mark means.
///
/// A mark cannot open a line (a line starting with `<!--` is an HTML block),
/// so a draft on a paragraph's first word is written a character in —
/// `Ф<!--c new …-->раза 2.` — and still means `Фраза 2.`. Everything that
/// reports or posts a draft goes through here, so they cannot disagree.
pub fn draft_span(body: &str, draft: &Mark) -> Option<(usize, usize)> {
    let end = draft.end?;
    Some((marks::intended_start(body, draft.start), end))
}

/// The anchor a body draft creates: the span's plain text plus which
/// occurrence of it on the page is meant, so repeated text is no obstacle.
pub fn draft_anchor(body: &str, start: usize, end: usize) -> confed_api::InlineAnchor {
    let selection = marks::plain_text(&body[start..end]).trim().to_string();
    let before = marks::plain_text(&body[..start]);
    let all = marks::plain_text(body);
    let count = all.matches(&selection).count().max(1);
    let index = before.matches(&selection).count().min(count - 1);
    confed_api::InlineAnchor {
        text: selection,
        context_before: crate::reanchor::anchor_at(body, start, end, None).context_before,
        context_after: crate::reanchor::anchor_at(body, start, end, None).context_after,
        marker_ref: None,
        orphaned: false,
        match_index: Some(index),
        match_count: Some(count),
    }
}

/// Refuse to push a page whose `new` marks cannot be posted as written.
pub fn validate_drafts(file: &MarkdownFile, path: &str) -> Result<()> {
    for issue in &file.mark_issues {
        if !issue.id().is_new() {
            continue;
        }
        let hint = match issue {
            MarkIssue::Unterminated { .. } => "close the span with `<!--/c new-->`",
            MarkIssue::UnmatchedClose { .. } => {
                "open the span with `<!--c new Your comment-->` before the text"
            }
            MarkIssue::InFence { .. } => {
                "Confluence cannot anchor a comment inside a code block; comment on it from the \
                 sidecar with `<!-- confed:new anchor=\"…\" -->` instead"
            }
        };
        return Err(ConfedError::state_with_hint(format!("{path}: {issue}"), hint));
    }
    for draft in file.drafts() {
        if draft.draft_body().is_none() {
            return Err(ConfedError::state_with_hint(
                format!("{path}: line {}: the `new` mark has no comment text", draft.line),
                "write the comment inside the opener: `<!--c new Your comment-->`",
            ));
        }
        if draft.text.trim().is_empty() {
            return Err(ConfedError::state_with_hint(
                format!("{path}: line {}: the `new` mark wraps no text", draft.line),
                "put the opener before the text you are commenting on and the closer after it",
            ));
        }
    }
    Ok(())
}

/// Bring a page file's mark layer in line with its comments (design 06 §5).
///
/// A mark already in the file says where its comment sits now, and that
/// refreshes the stored anchor. A live comment with no mark is placed by text
/// search; a mark whose comment is resolved or gone is removed; drafts stay.
/// Only the body is rewritten, and only when the layer changed. A conflicted
/// file is left alone.
pub fn sync_marks(ws: &mut Workspace, page_id: &str, page_path: &str) -> Result<()> {
    let mode = marks_mode(ws);

    let abs = ws.absolute(page_path);
    let Ok(content) = std::fs::read_to_string(&abs) else { return Ok(()) };
    let Ok(mut file) = crate::frontmatter::parse(&content, page_path) else { return Ok(()) };
    if merge::has_conflict_markers(&file.body) {
        return Ok(());
    }
    let records = ws.state().page_comments(page_id)?;
    let body = file.body.clone();
    let mut placed: Vec<PlacedMark> = Vec::new();

    for (record, anchor) in live_inline(&records) {
        let id = MarkId::Comment(record.comment_id.clone());
        let note = mark_preview(record, &records, mode);
        let in_file: Vec<&Mark> =
            file.marks.iter().filter(|m| m.id == id && m.end.is_some()).collect();

        if let Some(first) = in_file.first() {
            if mode != MarksMode::Off {
                for m in &in_file {
                    placed.push(PlacedMark {
                        id: id.clone(),
                        start: m.start,
                        end: m.end.unwrap_or(m.start),
                        note: note.clone(),
                    });
                }
            }
            // The file is authoritative for where the comment sits. A mark
            // may start a character or two late (it cannot open a line), so
            // the stored text is kept when it still fits around the mark.
            let end = first.end.unwrap_or(first.start);
            let start = (0..=2)
                .map(|k| first.start.saturating_sub(k))
                .find(|&st| body.get(st..end) == Some(anchor.text.as_str()))
                .unwrap_or(first.start);
            let refreshed =
                crate::reanchor::anchor_at(&body, start, end, anchor.marker_ref.clone());
            if refreshed.text != anchor.text
                || refreshed.context_before != anchor.context_before
                || refreshed.context_after != anchor.context_after
                || anchor.orphaned
            {
                let mut updated = record.clone();
                updated.anchor = serde_json::to_string(&refreshed).ok();
                ws.state().upsert_comment(&updated)?;
            }
            continue;
        }

        if mode == MarksMode::Off {
            continue;
        }
        // A mark is written only where the anchor text sits verbatim. A fuzzy
        // hit gives a start but no trustworthy end, and the text it found may
        // be markup in a preserved storage block; such a comment stays in the
        // sidecar. `apply` additionally refuses spans inside code and tags.
        let found = crate::reanchor::reanchor(&anchor, &body);
        if let Some(offset) =
            found.offset.filter(|_| found.kind != crate::reanchor::MatchKind::Fuzzy)
        {
            let end = offset + anchor.text.len();
            if body.get(offset..end) == Some(anchor.text.as_str()) {
                placed.push(PlacedMark { id, start: offset, end, note });
            }
        }
    }

    for draft in file.drafts() {
        if let Some(end) = draft.end {
            placed.push(PlacedMark {
                id: MarkId::New,
                start: draft.start,
                end,
                note: draft.note.clone(),
            });
        }
    }

    file.marks = placed
        .iter()
        .map(|p| Mark {
            id: p.id.clone(),
            start: p.start,
            end: Some(p.end),
            text: body.get(p.start..p.end).unwrap_or("").to_string(),
            note: p.note.clone(),
            line: 0,
        })
        .collect();
    rewrite_body(&abs, &content, &file.marked_body())
}

/// Replace a file's body on disk, leaving its frontmatter bytes untouched.
pub(super) fn rewrite_body(path: &Path, content: &str, new_body: &str) -> Result<()> {
    let Some((_, body)) = crate::frontmatter::split(content) else { return Ok(()) };
    if body == new_body {
        return Ok(());
    }
    let head = &content[..content.len() - body.len()];
    write_atomic(path, &format!("{head}{new_body}"))
}
