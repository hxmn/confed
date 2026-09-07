//! Confluence REST v1 DTOs.
//!
//! Data Center speaks v1 for everything; Cloud still falls back to it for label
//! writes, attachment upload, CQL search and page-at-version. One set of structs
//! serves both.

use super::{space_key_from_display_url, strip_highlight_markers, Id};
use crate::error::{ApiError, ApiResult};
use crate::types::*;
use serde::Deserialize;

/// Attachment uploads answer with `{ "results": [ … ] }` when creating and with a
/// bare attachment object when replacing the data of an existing one. Accept both.
pub fn attachment_from_response(
    value: serde_json::Value,
    page: &PageId,
) -> ApiResult<crate::types::Attachment> {
    let object = match value {
        serde_json::Value::Object(mut map) => match map.remove("results") {
            Some(serde_json::Value::Array(mut items)) if !items.is_empty() => items.remove(0),
            Some(other) => {
                map.insert("results".to_string(), other);
                serde_json::Value::Object(map)
            }
            None => serde_json::Value::Object(map),
        },
        other => other,
    };
    let content: Content = serde_json::from_value(object)
        .map_err(|e| ApiError::Decode { context: "attachment upload".to_string(), source: e })?;
    Ok(content.into_attachment(page))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub user_key: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub public_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

impl User {
    pub fn into_domain(self) -> crate::types::User {
        let display_name = self
            .display_name
            .or_else(|| self.public_name.clone())
            .or_else(|| self.username.clone())
            .or_else(|| self.account_id.clone())
            .unwrap_or_else(|| "unknown".to_string());
        crate::types::User {
            account_id: self.account_id,
            username: self.username.or(self.user_key),
            display_name,
            email: self.email,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub by: Option<User>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl Version {
    pub fn into_domain(self) -> VersionInfo {
        VersionInfo {
            number: self.number.unwrap_or(1),
            author: self.by.and_then(|u| u.display_name.or(u.username)),
            when: self.when,
            message: self.message.filter(|m| !m.is_empty()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BodyValue {
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Body {
    #[serde(default)]
    pub storage: Option<BodyValue>,
}

#[derive(Debug, Deserialize)]
pub struct Label {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LabelPage {
    #[serde(default)]
    pub results: Vec<Label>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub labels: Option<LabelPage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineProperties {
    #[serde(default)]
    pub original_selection: Option<String>,
    #[serde(default)]
    pub marker_ref: Option<String>,
    #[serde(default)]
    pub original_text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Resolution {
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extensions {
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    /// Only present on inline comments — this is how DC identifies them.
    #[serde(default)]
    pub inline_properties: Option<InlineProperties>,
    #[serde(default)]
    pub resolution: Option<Resolution>,
    #[serde(default)]
    pub location: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    #[serde(default)]
    pub created_by: Option<User>,
    #[serde(default)]
    pub created_date: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Links {
    #[serde(default)]
    pub download: Option<String>,
    #[serde(default)]
    pub webui: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Ref {
    #[serde(default)]
    pub id: Option<Id>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    #[serde(default)]
    pub id: Option<Id>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub homepage: Option<Box<Content>>,
}

impl Space {
    pub fn into_domain(self) -> crate::types::Space {
        let key = self.key.unwrap_or_default();
        crate::types::Space {
            id: SpaceId { key: key.clone(), numeric: self.id.map(|i| i.as_string()) },
            name: self.name.unwrap_or_else(|| key.clone()),
            kind: self.kind,
            homepage_id: self.homepage.map(|h| PageId::new(h.id.as_string())),
        }
    }
}

/// Anything addressable through `/rest/api/content`: pages, comments, attachments.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    pub id: Id,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub space: Option<Space>,
    #[serde(default)]
    pub version: Option<Version>,
    #[serde(default)]
    pub ancestors: Option<Vec<Ref>>,
    #[serde(default)]
    pub body: Option<Body>,
    #[serde(default)]
    pub metadata: Option<Metadata>,
    #[serde(default)]
    pub extensions: Option<Extensions>,
    #[serde(default)]
    pub history: Option<History>,
    #[serde(default)]
    pub container: Option<Ref>,
    #[serde(rename = "_links", default)]
    pub links: Option<Links>,
}

impl Content {
    fn body_storage(&self) -> String {
        self.body
            .as_ref()
            .and_then(|b| b.storage.as_ref())
            .and_then(|s| s.value.clone())
            .unwrap_or_default()
    }

    fn labels(&self) -> Vec<String> {
        self.metadata
            .as_ref()
            .and_then(|m| m.labels.as_ref())
            .map(|l| l.results.iter().filter_map(|x| x.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The deepest ancestor is the direct parent; v1 lists them root-first.
    fn parent_id(&self) -> Option<PageId> {
        self.ancestors
            .as_ref()?
            .iter()
            .rfind(|a| a.kind.as_deref().is_none_or(|k| k != "comment"))?
            .id
            .as_ref()
            .map(|i| PageId::new(i.as_string()))
    }

    fn parent_comment_id(&self) -> Option<CommentId> {
        self.ancestors
            .as_ref()?
            .iter()
            .rfind(|a| a.kind.as_deref() != Some("page"))?
            .id
            .as_ref()
            .map(|i| CommentId::new(i.as_string()))
    }

    fn author(&self) -> Option<String> {
        self.version
            .as_ref()
            .and_then(|v| v.by.as_ref())
            .and_then(|u| u.display_name.clone().or_else(|| u.username.clone()))
            .or_else(|| {
                self.history
                    .as_ref()
                    .and_then(|h| h.created_by.as_ref())
                    .and_then(|u| u.display_name.clone().or_else(|| u.username.clone()))
            })
    }

    pub fn space_key(&self) -> Option<String> {
        self.space.as_ref().and_then(|s| s.key.clone())
    }

    pub fn summary(&self, fallback_space: &str) -> PageSummary {
        PageSummary {
            id: PageId::new(self.id.as_string()),
            title: self.title.clone().unwrap_or_default(),
            space_key: self.space_key().unwrap_or_else(|| fallback_space.to_string()),
            parent_id: self.parent_id(),
            position: None,
            version: self.version.as_ref().and_then(|v| v.number).unwrap_or(1),
            status: self.status.as_deref().map(PageStatus::parse).unwrap_or(PageStatus::Current),
            labels: self.labels(),
            author: self.author(),
            created_at: self.history.as_ref().and_then(|h| h.created_date.clone()),
            updated_at: self.version.as_ref().and_then(|v| v.when.clone()),
        }
    }

    pub fn into_page(self, fallback_space: &str) -> crate::types::Page {
        let summary = self.summary(fallback_space);
        crate::types::Page { summary, body_storage: self.body_storage() }
    }

    pub fn into_attachment(self, fallback_page: &PageId) -> crate::types::Attachment {
        let ext = self.extensions.unwrap_or_default();
        let page_id = self
            .container
            .and_then(|c| c.id)
            .map(|i| PageId::new(i.as_string()))
            .unwrap_or_else(|| fallback_page.clone());
        crate::types::Attachment {
            id: AttachmentId::new(self.id.as_string()),
            page_id,
            filename: self.title.unwrap_or_default(),
            media_type: ext.media_type,
            file_size: ext.file_size,
            version: self.version.and_then(|v| v.number).unwrap_or(1),
            download_url: self.links.and_then(|l| l.download).unwrap_or_default(),
        }
    }

    /// A v1 comment is *inline* exactly when `extensions.inlineProperties` is present;
    /// there is no other discriminator on Data Center.
    pub fn into_comment(self, fallback_page: &PageId) -> crate::types::Comment {
        let author = self.author();
        let created_at = self
            .history
            .as_ref()
            .and_then(|h| h.created_date.clone())
            .or_else(|| self.version.as_ref().and_then(|v| v.when.clone()));
        let body_storage = self.body_storage();
        let parent_comment_id = self.parent_comment_id();
        let page_id = self
            .container
            .as_ref()
            .and_then(|c| c.id.as_ref())
            .map(|i| PageId::new(i.as_string()))
            .unwrap_or_else(|| fallback_page.clone());

        let ext = self.extensions.unwrap_or_default();
        let resolved = ext
            .resolution
            .as_ref()
            .and_then(|r| r.status.as_deref())
            .is_some_and(|s| s.eq_ignore_ascii_case("resolved"));
        let (kind, anchor) = match ext.inline_properties {
            Some(props) => {
                let text = props.original_selection.or(props.original_text).unwrap_or_default();
                let orphaned = text.is_empty()
                    || ext
                        .resolution
                        .as_ref()
                        .and_then(|r| r.status.as_deref())
                        .is_some_and(|s| s.eq_ignore_ascii_case("dangling"));
                (
                    CommentKind::Inline,
                    Some(InlineAnchor {
                        text,
                        context_before: String::new(),
                        context_after: String::new(),
                        marker_ref: props.marker_ref,
                        orphaned,
                        ..Default::default()
                    }),
                )
            }
            None => (CommentKind::Footer, None),
        };

        crate::types::Comment {
            id: CommentId::new(self.id.as_string()),
            page_id,
            parent_comment_id,
            kind,
            author,
            created_at,
            body_storage,
            resolved,
            anchor,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchContainer {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub display_url: Option<String>,
}

/// What `rest/api/search` must expand for a result to carry its version,
/// author and space — search returns a bare content stub otherwise.
pub const SEARCH_EXPAND: &str = "content.version,content.space";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub excerpt: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Reported by search itself, so recency survives even unexpanded content.
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub result_global_container: Option<SearchContainer>,
}

impl SearchResult {
    /// `resolve_url` turns the server-relative `url` into something a browser can open.
    pub fn into_domain(
        self,
        resolve_url: impl Fn(&str) -> String,
    ) -> Option<crate::types::SearchResult> {
        let content = self.content?;
        let space_key = content.space_key().or_else(|| {
            self.result_global_container
                .as_ref()
                .and_then(|c| c.display_url.as_deref())
                .and_then(space_key_from_display_url)
        });
        let title = content
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| self.title.as_deref().map(strip_highlight_markers))
            .unwrap_or_default();
        Some(crate::types::SearchResult {
            page_id: PageId::new(content.id.as_string()),
            title,
            space_key,
            url: self.url.as_deref().map(&resolve_url).unwrap_or_default(),
            excerpt: self
                .excerpt
                .as_deref()
                .map(strip_highlight_markers)
                .filter(|e| !e.trim().is_empty()),
            version: content.version.as_ref().and_then(|v| v.number),
            author: content.author(),
            when: content.version.as_ref().and_then(|v| v.when.clone()).or(self.last_modified),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_json() -> serde_json::Value {
        serde_json::json!({
            "id": "1001",
            "type": "page",
            "status": "current",
            "title": "Onboarding",
            "space": { "id": 500, "key": "DOCS", "name": "Documentation" },
            "version": { "number": 4, "when": "2026-02-02T00:00:00Z", "by": { "displayName": "Alice Ng" } },
            "ancestors": [ { "id": "1", "type": "page" }, { "id": "900", "type": "page" } ],
            "body": { "storage": { "value": "<p>hi</p>", "representation": "storage" } },
            "metadata": { "labels": { "results": [ { "name": "guide", "prefix": "global" } ] } },
            "history": { "createdDate": "2026-01-01T00:00:00Z", "createdBy": { "displayName": "Bob" } }
        })
    }

    #[test]
    fn the_deepest_ancestor_becomes_the_parent() {
        let c: Content = serde_json::from_value(page_json()).unwrap();
        let page = c.into_page("");
        assert_eq!(page.summary.parent_id, Some(PageId::new("900")));
        assert_eq!(page.summary.space_key, "DOCS");
        assert_eq!(page.summary.version, 4);
        assert_eq!(page.summary.author.as_deref(), Some("Alice Ng"));
        assert_eq!(page.summary.labels, vec!["guide"]);
        assert_eq!(page.body_storage, "<p>hi</p>");
        assert_eq!(page.summary.created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn a_page_with_no_ancestors_has_no_parent() {
        let mut json = page_json();
        json["ancestors"] = serde_json::json!([]);
        let c: Content = serde_json::from_value(json).unwrap();
        assert!(c.into_page("").summary.parent_id.is_none());
    }

    #[test]
    fn the_space_key_falls_back_when_the_expansion_is_missing() {
        let mut json = page_json();
        json.as_object_mut().unwrap().remove("space");
        let c: Content = serde_json::from_value(json).unwrap();
        assert_eq!(c.into_page("OPS").summary.space_key, "OPS");
    }

    #[test]
    fn inline_properties_identify_an_inline_comment() {
        let c: Content = serde_json::from_value(serde_json::json!({
            "id": "77",
            "type": "comment",
            "container": { "id": "1001", "type": "page" },
            "body": { "storage": { "value": "<p>note</p>" } },
            "extensions": {
                "inlineProperties": { "originalSelection": "first week checklist", "markerRef": "m-1" },
                "resolution": { "status": "open" }
            },
            "history": { "createdDate": "2026-07-30T10:02:00Z", "createdBy": { "displayName": "Alice Ng" } }
        }))
        .unwrap();
        let comment = c.into_comment(&PageId::new("0"));
        assert_eq!(comment.kind, CommentKind::Inline);
        assert_eq!(comment.page_id, PageId::new("1001"));
        assert!(!comment.resolved);
        let anchor = comment.anchor.unwrap();
        assert_eq!(anchor.text, "first week checklist");
        assert_eq!(anchor.marker_ref.as_deref(), Some("m-1"));
        assert!(!anchor.orphaned);
    }

    #[test]
    fn a_comment_without_inline_properties_is_a_footer_comment_and_keeps_its_parent() {
        let c: Content = serde_json::from_value(serde_json::json!({
            "id": "78",
            "type": "comment",
            "container": { "id": "1001", "type": "page" },
            "ancestors": [ { "id": "1001", "type": "page" }, { "id": "77", "type": "comment" } ],
            "body": { "storage": { "value": "<p>reply</p>" } },
            "extensions": { "resolution": { "status": "resolved" } }
        }))
        .unwrap();
        let comment = c.into_comment(&PageId::new("0"));
        assert_eq!(comment.kind, CommentKind::Footer);
        assert_eq!(comment.parent_comment_id, Some(CommentId::new("77")));
        assert!(comment.resolved);
    }

    #[test]
    fn attachments_read_size_type_and_download_link() {
        let c: Content = serde_json::from_value(serde_json::json!({
            "id": "att123",
            "type": "attachment",
            "title": "diagram.png",
            "container": { "id": "1001", "type": "page" },
            "version": { "number": 3 },
            "extensions": { "mediaType": "image/png", "fileSize": 2048 },
            "_links": { "download": "/download/attachments/1001/diagram.png?version=3" }
        }))
        .unwrap();
        let a = c.into_attachment(&PageId::new("0"));
        assert_eq!(a.filename, "diagram.png");
        assert_eq!(a.media_type.as_deref(), Some("image/png"));
        assert_eq!(a.file_size, Some(2048));
        assert_eq!(a.version, 3);
        assert_eq!(a.page_id, PageId::new("1001"));
        assert!(a.download_url.starts_with("/download/attachments/"));
    }

    #[test]
    fn upload_responses_parse_whether_or_not_they_are_wrapped_in_results() {
        let one = serde_json::json!({
            "id": "att1", "title": "a.txt", "version": { "number": 2 },
            "extensions": { "fileSize": 3 }
        });
        let wrapped = serde_json::json!({ "results": [one.clone()], "size": 1 });

        let page = PageId::new("1001");
        let a = attachment_from_response(wrapped, &page).unwrap();
        assert_eq!(a.id, AttachmentId::new("att1"));
        assert_eq!(a.page_id, page);
        assert_eq!(a.version, 2);

        let b = attachment_from_response(one, &page).unwrap();
        assert_eq!(b.id, AttachmentId::new("att1"));
        assert_eq!(b.filename, "a.txt");
    }

    #[test]
    fn an_unparseable_upload_response_is_a_decode_error() {
        let err = attachment_from_response(serde_json::json!({ "results": [] }), &PageId::new("1"))
            .unwrap_err();
        assert!(matches!(err, ApiError::Decode { .. }), "got {err:?}");
    }

    #[test]
    fn search_results_strip_highlights_and_recover_the_space_key() {
        let r: SearchResult = serde_json::from_value(serde_json::json!({
            "content": { "id": "1001", "type": "page", "title": "Onboarding" },
            "title": "@@@hl@@@Onboarding@@@endhl@@@",
            "excerpt": "your @@@hl@@@first week@@@endhl@@@ checklist",
            "url": "/spaces/DOCS/pages/1001/Onboarding",
            "resultGlobalContainer": { "title": "Documentation", "displayUrl": "/spaces/DOCS" }
        }))
        .unwrap();
        let d = r.into_domain(|u| format!("https://acme.atlassian.net/wiki{u}")).unwrap();
        assert_eq!(d.title, "Onboarding");
        assert_eq!(d.space_key.as_deref(), Some("DOCS"));
        assert_eq!(d.excerpt.as_deref(), Some("your first week checklist"));
        assert_eq!(d.url, "https://acme.atlassian.net/wiki/spaces/DOCS/pages/1001/Onboarding");
    }

    #[test]
    fn search_results_without_content_are_dropped() {
        let r: SearchResult = serde_json::from_value(serde_json::json!({
            "title": "a user", "url": "/people/1"
        }))
        .unwrap();
        assert!(r.into_domain(|u| u.to_string()).is_none());
    }
}
