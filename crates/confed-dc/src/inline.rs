//! DTOs for Data Center's private inline-comment plugin API,
//! `rest/inlinecomments/1.0`. Undocumented: shapes are taken from the page
//! view's own requests (see `tests/fixtures/dc-inline`), so every field is
//! optional and unknown ones are ignored.

use confed_api::types::{Comment, CommentId, CommentKind, InlineAnchor, PageId};
use confed_api::wire::Id;
use serde::Deserialize;

/// A comment as the plugin returns it from create and reply.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineComment {
    pub id: Option<Id>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub author_display_name: Option<String>,
    /// Epoch milliseconds in responses; the page view echoes it back as text.
    #[serde(default)]
    pub last_modification_date: Option<serde_json::Value>,
    #[serde(default)]
    pub marker_ref: Option<String>,
    #[serde(default)]
    pub original_selection: Option<String>,
    /// On a reply: the thread's root comment.
    #[serde(default)]
    pub comment_id: Option<Id>,
    #[serde(default)]
    pub resolve_properties: Option<ResolveProperties>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveProperties {
    #[serde(default)]
    pub resolved: bool,
}

/// The body of a resolve response.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveResponse {
    #[serde(default)]
    pub resolve_properties: Option<ResolveProperties>,
}

impl InlineComment {
    pub fn into_comment(self, page: &PageId) -> Comment {
        let created_at = self.last_modification_date.as_ref().and_then(|v| match v {
            serde_json::Value::Number(n) => n
                .as_i64()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        });
        let parent = self.comment_id.map(|i| CommentId::new(i.as_string()));
        let anchor = (parent.is_none()).then(|| InlineAnchor {
            text: self.original_selection.clone().unwrap_or_default(),
            marker_ref: self.marker_ref.clone(),
            ..Default::default()
        });
        Comment {
            id: CommentId::new(self.id.map(|i| i.as_string()).unwrap_or_default()),
            page_id: page.clone(),
            parent_comment_id: parent,
            kind: CommentKind::Inline,
            author: self.author_display_name,
            created_at,
            body_storage: self.body.unwrap_or_default(),
            resolved: self.resolve_properties.is_some_and(|r| r.resolved),
            anchor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREATED: &str = include_str!("../tests/fixtures/dc-inline/create.response.json");
    const REPLIED: &str = include_str!("../tests/fixtures/dc-inline/reply.response.json");
    const RESOLVED: &str = include_str!("../tests/fixtures/dc-inline/resolve.response.json");

    #[test]
    fn the_captured_create_response_is_an_open_inline_root() {
        let c: InlineComment = serde_json::from_str(CREATED).unwrap();
        let c = c.into_comment(&PageId::new("900000099218"));
        assert_eq!(c.id, CommentId::new("900000099220"));
        assert_eq!(c.kind, CommentKind::Inline);
        assert!(c.parent_comment_id.is_none());
        assert!(!c.resolved);
        let anchor = c.anchor.unwrap();
        assert_eq!(anchor.text, "Фраза 3.");
        assert_eq!(anchor.marker_ref.as_deref(), Some("0f8c1a52-4e7b-4c3d-9a6e-2b5d7e9f1c30"));
        assert_eq!(c.created_at.as_deref(), Some("2026-10-02T07:12:28Z"));
        assert_eq!(c.author.as_deref(), Some("Alice Bergmann"));
    }

    #[test]
    fn the_captured_reply_response_points_at_its_root() {
        let c: InlineComment = serde_json::from_str(REPLIED).unwrap();
        let c = c.into_comment(&PageId::new("900000099218"));
        assert_eq!(c.id, CommentId::new("900000099224"));
        assert_eq!(c.parent_comment_id, Some(CommentId::new("900000099222")));
        assert!(c.anchor.is_none());
        assert_eq!(c.body_storage, "<p>replya</p>");
    }

    #[test]
    fn the_captured_resolve_response_says_resolved() {
        let r: ResolveResponse = serde_json::from_str(RESOLVED).unwrap();
        assert!(r.resolve_properties.unwrap().resolved);
    }
}
