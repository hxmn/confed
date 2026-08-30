//! The comment sidecar: `.<slug>/comments.md`.
//!
//! The sidecar is the primary store, and `confed comment …` is a structured
//! accessor over it. That keeps comments offline-first and editable with plain
//! file tools (including by agents), which a CLI-only design would not.
//!
//! Each entry is introduced by an HTML comment carrying its metadata, so the
//! file renders cleanly anywhere while staying machine-parseable. Users add new
//! comments under a `confed:new` marker and resolve with `confed:resolve`.

use crate::error::Result;
use crate::state::CommentRecord;
use confed_api::InlineAnchor;

pub const COMMENTS_FILENAME: &str = "comments.md";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarKind {
    Footer,
    Inline,
}

impl SidecarKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SidecarKind::Footer => "footer",
            SidecarKind::Inline => "inline",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SidecarComment {
    /// `None` for a draft that has not been pushed yet.
    pub id: Option<String>,
    pub kind: SidecarKind,
    pub reply_to: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
    pub resolved: bool,
    pub anchor: Option<InlineAnchor>,
    pub body: String,
}

impl SidecarComment {
    pub fn is_draft(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sidecar {
    pub page_id: String,
    pub title: String,
    pub comments: Vec<SidecarComment>,
    /// Ids the user asked to resolve, via `<!-- confed:resolve id=… -->`.
    pub resolve_requests: Vec<String>,
}

impl Sidecar {
    pub fn drafts(&self) -> impl Iterator<Item = &SidecarComment> {
        self.comments.iter().filter(|c| c.is_draft())
    }

    pub fn unresolved(&self) -> impl Iterator<Item = &SidecarComment> {
        self.comments.iter().filter(|c| !c.resolved)
    }

    pub fn orphaned(&self) -> impl Iterator<Item = &SidecarComment> {
        self.comments.iter().filter(|c| c.anchor.as_ref().is_some_and(|a| a.orphaned))
    }
}

/// Render the sidecar file for a page.
///
/// `drafts` are unpushed local entries, appended after the server threads so a
/// refresh from `pull` never loses them.
pub fn render(
    page_id: &str,
    title: &str,
    comments: &[CommentRecord],
    drafts: &[SidecarComment],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Comments — {title} (page {page_id})\n"));

    if comments.is_empty() && drafts.is_empty() {
        out.push_str("\nNo comments yet. Add one under a `confed:new` marker, or run\n");
        out.push_str("`confed comment add <page> -m \"…\"`.\n");
        return out;
    }

    // Roots first, each followed by its replies, so threads read top to bottom.
    let roots: Vec<&CommentRecord> =
        comments.iter().filter(|c| c.parent_comment_id.is_none()).collect();
    for root in roots {
        out.push('\n');
        write_record(&mut out, root, 0);
        write_replies(&mut out, comments, &root.comment_id, 1);
    }

    // Records whose parent is missing from this snapshot still need to appear.
    for record in comments {
        let orphaned_reply = record
            .parent_comment_id
            .as_ref()
            .is_some_and(|p| !comments.iter().any(|c| &c.comment_id == p));
        if orphaned_reply {
            out.push('\n');
            write_record(&mut out, record, 0);
        }
    }

    for draft in drafts {
        out.push('\n');
        write_draft(&mut out, draft);
    }
    out
}

fn write_replies(out: &mut String, all: &[CommentRecord], parent: &str, depth: usize) {
    for reply in all.iter().filter(|c| c.parent_comment_id.as_deref() == Some(parent)) {
        out.push('\n');
        write_record(out, reply, depth);
        write_replies(out, all, &reply.comment_id, depth + 1);
    }
}

fn write_record(out: &mut String, record: &CommentRecord, depth: usize) {
    let indent = "  ".repeat(depth);
    let tag = if record.kind == "inline" { "confed:inline" } else { "confed:comment" };

    let mut attrs = vec![format!("id={}", record.comment_id)];
    if let Some(parent) = &record.parent_comment_id {
        attrs.push(format!("reply-to={parent}"));
    }
    if let Some(author) = &record.author {
        attrs.push(format!("author={}", quote(author)));
    }
    if let Some(date) = &record.created_at {
        attrs.push(format!("date={date}"));
    }
    if record.resolved {
        attrs.push("resolved=true".to_string());
    }
    if let Some(anchor) = record.anchor.as_ref().and_then(|a| parse_anchor(a)) {
        attrs.push(format!("anchor={}", quote(&anchor.text)));
        if !anchor.context_before.is_empty() {
            attrs.push(format!("context-before={}", quote(&anchor.context_before)));
        }
        if !anchor.context_after.is_empty() {
            attrs.push(format!("context-after={}", quote(&anchor.context_after)));
        }
        if anchor.orphaned {
            attrs.push("orphaned=true".to_string());
        }
    }

    out.push_str(&format!("{indent}<!-- {tag} {} -->\n", attrs.join(" ")));
    for line in record.body_markdown.trim().lines() {
        out.push_str(indent.as_str());
        out.push_str(line);
        out.push('\n');
    }
}

fn write_draft(out: &mut String, draft: &SidecarComment) {
    let mut attrs = Vec::new();
    if let Some(parent) = &draft.reply_to {
        attrs.push(format!("reply-to={parent}"));
    }
    if let Some(anchor) = &draft.anchor {
        attrs.push(format!("anchor={}", quote(&anchor.text)));
    }
    let suffix = if attrs.is_empty() { String::new() } else { format!(" {}", attrs.join(" ")) };
    out.push_str(&format!("<!-- confed:new{suffix} -->\n"));
    out.push_str(draft.body.trim());
    out.push('\n');
}

fn parse_anchor(json: &str) -> Option<InlineAnchor> {
    serde_json::from_str(json).ok()
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Parse a sidecar file back into structured comments.
pub fn parse(text: &str) -> Result<Sidecar> {
    let mut sidecar = Sidecar::default();

    if let Some(header) = text.lines().find(|l| l.starts_with("# Comments")) {
        if let Some((title, rest)) =
            header.trim_start_matches("# Comments — ").rsplit_once(" (page ")
        {
            sidecar.title = title.to_string();
            sidecar.page_id = rest.trim_end_matches(')').to_string();
        }
    }

    let mut current: Option<SidecarComment> = None;
    let mut body = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(marker) = parse_marker(trimmed) {
            if let Some(mut comment) = current.take() {
                comment.body = body.trim().to_string();
                sidecar.comments.push(comment);
            }
            body.clear();

            match marker {
                Marker::Resolve(id) => sidecar.resolve_requests.push(id),
                Marker::Comment(comment) => current = Some(*comment),
            }
            continue;
        }
        if current.is_some() {
            body.push_str(line.trim_start());
            body.push('\n');
        }
    }
    if let Some(mut comment) = current.take() {
        comment.body = body.trim().to_string();
        sidecar.comments.push(comment);
    }

    // A body-less draft marker is noise, not a comment to post.
    sidecar.comments.retain(|c| !(c.is_draft() && c.body.is_empty()));
    Ok(sidecar)
}

enum Marker {
    Comment(Box<SidecarComment>),
    Resolve(String),
}

fn parse_marker(line: &str) -> Option<Marker> {
    let inner = line.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
    let (tag, rest) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
    let attrs = parse_attrs(rest);

    match tag {
        "confed:resolve" => attrs.get("id").map(|id| Marker::Resolve(id.clone())),
        "confed:comment" | "confed:inline" | "confed:new" => {
            let kind =
                if tag == "confed:inline" { SidecarKind::Inline } else { SidecarKind::Footer };
            let anchor = attrs.get("anchor").map(|text| InlineAnchor {
                text: text.clone(),
                context_before: attrs.get("context-before").cloned().unwrap_or_default(),
                context_after: attrs.get("context-after").cloned().unwrap_or_default(),
                marker_ref: attrs.get("marker-ref").cloned(),
                orphaned: attrs.get("orphaned").map(|v| v == "true").unwrap_or(false),
                ..Default::default()
            });
            Some(Marker::Comment(Box::new(SidecarComment {
                id: attrs.get("id").cloned(),
                kind: if anchor.is_some() && tag == "confed:new" {
                    SidecarKind::Inline
                } else {
                    kind
                },
                reply_to: attrs.get("reply-to").cloned(),
                author: attrs.get("author").cloned(),
                date: attrs.get("date").cloned(),
                resolved: attrs.get("resolved").map(|v| v == "true").unwrap_or(false),
                anchor,
                body: String::new(),
            })))
        }
        _ => None,
    }
}

/// `key=value` / `key="quoted value"` pairs from a marker.
fn parse_attrs(input: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        if key_start == i {
            break;
        }
        let key: String = chars[key_start..i].iter().collect();
        if i >= chars.len() || chars[i] != '=' {
            out.insert(key, String::new());
            continue;
        }
        i += 1; // skip '='

        let value = if i < chars.len() && chars[i] == '"' {
            i += 1;
            let mut value = String::new();
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                value.push(chars[i]);
                i += 1;
            }
            i += 1; // closing quote
            value
        } else {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            chars[start..i].iter().collect()
        };
        out.insert(key, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, parent: Option<&str>, body: &str, kind: &str) -> CommentRecord {
        CommentRecord {
            comment_id: id.into(),
            page_id: "163842".into(),
            parent_comment_id: parent.map(str::to_string),
            kind: kind.into(),
            author: Some("Alice Ng".into()),
            created_at: Some("2026-07-30T10:02:00Z".into()),
            body_storage: None,
            body_markdown: body.into(),
            resolved: false,
            anchor: None,
            synced_at: None,
        }
    }

    #[test]
    fn threads_render_with_replies_indented_under_their_parent() {
        let comments = vec![
            record("98211", None, "Should this mention the VPN setup?", "footer"),
            record("98230", Some("98211"), "Yes — adding it.", "footer"),
        ];
        let text = render("163842", "Onboarding", &comments, &[]);

        assert!(text.starts_with("# Comments — Onboarding (page 163842)"));
        assert!(text.contains("<!-- confed:comment id=98211"));
        assert!(text.contains("  <!-- confed:comment id=98230 reply-to=98211"));
        assert!(text.contains("  Yes — adding it."));
    }

    #[test]
    fn rendering_then_parsing_recovers_the_comments() {
        let comments = vec![
            record("98211", None, "Question?", "footer"),
            record("98230", Some("98211"), "Answer.", "footer"),
        ];
        let parsed = parse(&render("163842", "Onboarding", &comments, &[])).unwrap();

        assert_eq!(parsed.page_id, "163842");
        assert_eq!(parsed.title, "Onboarding");
        assert_eq!(parsed.comments.len(), 2);
        assert_eq!(parsed.comments[0].id.as_deref(), Some("98211"));
        assert_eq!(parsed.comments[0].body, "Question?");
        assert_eq!(parsed.comments[1].reply_to.as_deref(), Some("98211"));
        assert_eq!(parsed.comments[1].body, "Answer.");
    }

    #[test]
    fn inline_anchors_survive_the_round_trip() {
        let mut inline = record("77120", None, "Link the checklist template?", "inline");
        inline.anchor = Some(
            serde_json::to_string(&InlineAnchor {
                text: "first week checklist".into(),
                context_before: "during your ".into(),
                context_after: " and then".into(),
                marker_ref: Some("m1".into()),
                orphaned: false,
                ..Default::default()
            })
            .unwrap(),
        );

        let text = render("163842", "Onboarding", &[inline], &[]);
        assert!(text.contains("<!-- confed:inline id=77120"));
        assert!(text.contains(r#"anchor="first week checklist""#));

        let parsed = parse(&text).unwrap();
        let anchor = parsed.comments[0].anchor.as_ref().unwrap();
        assert_eq!(anchor.text, "first week checklist");
        assert_eq!(anchor.context_before, "during your ");
        assert_eq!(parsed.comments[0].kind, SidecarKind::Inline);
    }

    #[test]
    fn orphaned_anchors_are_flagged_not_dropped() {
        let mut inline = record("77120", None, "Still relevant?", "inline");
        inline.anchor = Some(
            serde_json::to_string(&InlineAnchor {
                text: "deleted text".into(),
                orphaned: true,
                ..Default::default()
            })
            .unwrap(),
        );
        let parsed = parse(&render("1", "P", &[inline], &[])).unwrap();
        assert_eq!(parsed.orphaned().count(), 1);
    }

    #[test]
    fn drafts_are_recognized_and_have_no_id() {
        let text = "# Comments — Onboarding (page 163842)\n\n\
                    <!-- confed:new -->\nReviewed for Q3.\n";
        let parsed = parse(text).unwrap();
        assert_eq!(parsed.comments.len(), 1);
        assert!(parsed.comments[0].is_draft());
        assert_eq!(parsed.comments[0].body, "Reviewed for Q3.");
        assert_eq!(parsed.drafts().count(), 1);
    }

    #[test]
    fn a_draft_reply_records_its_parent() {
        let text = "<!-- confed:new reply-to=98211 -->\nYes, agreed.\n";
        let parsed = parse(text).unwrap();
        assert_eq!(parsed.comments[0].reply_to.as_deref(), Some("98211"));
    }

    #[test]
    fn an_empty_draft_marker_is_ignored() {
        let parsed = parse("<!-- confed:new -->\n\n").unwrap();
        assert!(parsed.comments.is_empty(), "an empty draft must not be posted");
    }

    #[test]
    fn resolve_requests_are_collected() {
        let text = "<!-- confed:comment id=1 author=\"A\" -->\nbody\n\n\
                    <!-- confed:resolve id=1 -->\n";
        let parsed = parse(text).unwrap();
        assert_eq!(parsed.resolve_requests, ["1"]);
        assert_eq!(parsed.comments.len(), 1);
    }

    #[test]
    fn resolved_state_round_trips_as_an_attribute() {
        let mut resolved = record("1", None, "Done.", "footer");
        resolved.resolved = true;
        let parsed = parse(&render("1", "P", &[resolved], &[])).unwrap();
        assert!(parsed.comments[0].resolved);
        assert_eq!(parsed.unresolved().count(), 0);
    }

    #[test]
    fn drafts_are_preserved_when_the_sidecar_is_refreshed() {
        let draft = SidecarComment {
            id: None,
            kind: SidecarKind::Footer,
            reply_to: None,
            author: None,
            date: None,
            resolved: false,
            anchor: None,
            body: "My unpushed note.".into(),
        };
        let text = render("1", "P", &[record("9", None, "Server comment", "footer")], &[draft]);
        let parsed = parse(&text).unwrap();

        assert_eq!(parsed.comments.len(), 2);
        assert_eq!(parsed.drafts().count(), 1);
        assert_eq!(parsed.drafts().next().unwrap().body, "My unpushed note.");
    }

    #[test]
    fn quoted_attribute_values_handle_spaces_and_escapes() {
        let attrs = parse_attrs(r#"id=1 author="Alice \"Al\" Ng" date=2026-01-01 flag=true"#);
        assert_eq!(attrs["id"], "1");
        assert_eq!(attrs["author"], r#"Alice "Al" Ng"#);
        assert_eq!(attrs["date"], "2026-01-01");
        assert_eq!(attrs["flag"], "true");
    }

    #[test]
    fn an_empty_page_gets_guidance_rather_than_a_bare_heading() {
        let text = render("1", "Onboarding", &[], &[]);
        assert!(text.contains("No comments yet"));
        assert!(text.contains("confed:new"));
        assert!(parse(&text).unwrap().comments.is_empty());
    }

    #[test]
    fn ordinary_markdown_around_markers_is_ignored() {
        let text = "# Comments — P (page 1)\n\nSome prose nobody asked for.\n\n\
                    <!-- confed:comment id=1 -->\nreal body\n";
        let parsed = parse(text).unwrap();
        assert_eq!(parsed.comments.len(), 1);
        assert_eq!(parsed.comments[0].body, "real body");
    }
}
