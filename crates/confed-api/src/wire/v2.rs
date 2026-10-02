//! Confluence Cloud REST v2 DTOs and their mapping onto [`crate::types`].

use super::Id;
use crate::paginate::CursorPage;
use crate::types::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub id: Id,
    pub key: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub homepage_id: Option<Id>,
}

impl Space {
    pub fn into_domain(self) -> crate::types::Space {
        crate::types::Space {
            id: SpaceId { key: self.key.clone(), numeric: Some(self.id.as_string()) },
            name: self.name.unwrap_or(self.key),
            kind: self.kind,
            homepage_id: self.homepage_id.map(|h| PageId::new(h.as_string())),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub author_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BodyValue {
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Body {
    #[serde(default)]
    pub storage: Option<BodyValue>,
    /// Present when a caller asked for `body-format=atlas_doc_format`; confed never
    /// does, but the field keeps round-tripping honest if that ever changes.
    #[serde(default)]
    pub atlas_doc_format: Option<BodyValue>,
}

impl Body {
    pub fn storage_value(&self) -> String {
        self.storage.as_ref().and_then(|s| s.value.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub id: Id,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub space_id: Option<Id>,
    #[serde(default)]
    pub parent_id: Option<Id>,
    #[serde(default)]
    pub author_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub position: Option<i64>,
    #[serde(default)]
    pub version: Option<Version>,
    #[serde(default)]
    pub body: Option<Body>,
    #[serde(default)]
    pub labels: Option<CursorPage<Label>>,
}

impl Page {
    pub fn space_numeric(&self) -> Option<String> {
        self.space_id.as_ref().map(|s| s.as_string())
    }

    pub fn summary(&self, space_key: &str) -> PageSummary {
        let version = self.version.as_ref();
        PageSummary {
            id: PageId::new(self.id.as_string()),
            title: self.title.clone().unwrap_or_default(),
            space_key: space_key.to_string(),
            parent_id: self.parent_id.as_ref().map(|p| PageId::new(p.as_string())),
            position: self.position,
            version: version.and_then(|v| v.number).unwrap_or(1),
            status: self.status.as_deref().map(PageStatus::parse).unwrap_or(PageStatus::Current),
            labels: self
                .labels
                .as_ref()
                .map(|l| l.results.iter().map(|x| x.name.clone()).collect())
                .unwrap_or_default(),
            // v2 only exposes account ids on pages; the display name would cost an
            // extra user lookup per page, so callers resolve it lazily if they care.
            author: version.and_then(|v| v.author_id.clone()).or_else(|| self.author_id.clone()),
            created_at: self.created_at.clone(),
            updated_at: version.and_then(|v| v.created_at.clone()),
        }
    }

    pub fn into_domain(self, space_key: &str) -> crate::types::Page {
        let summary = self.summary(space_key);
        let body_storage = self.body.as_ref().map(|b| b.storage_value()).unwrap_or_default();
        crate::types::Page { summary, body_storage }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: Id,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub page_id: Option<Id>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub version: Option<Version>,
    #[serde(default)]
    pub download_link: Option<String>,
}

impl Attachment {
    pub fn into_domain(self, fallback_page: &PageId) -> crate::types::Attachment {
        let page_id = self
            .page_id
            .map(|p| PageId::new(p.as_string()))
            .unwrap_or_else(|| fallback_page.clone());
        crate::types::Attachment {
            id: AttachmentId::new(self.id.as_string()),
            page_id,
            filename: self.title.unwrap_or_default(),
            media_type: self.media_type,
            file_size: self.file_size,
            version: self.version.and_then(|v| v.number).unwrap_or(1),
            download_url: self.download_link.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineProperties {
    #[serde(default)]
    pub inline_marker_ref: Option<String>,
    #[serde(default)]
    pub inline_original_selection: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: Id,
    #[serde(default)]
    pub page_id: Option<Id>,
    #[serde(default)]
    pub parent_comment_id: Option<Id>,
    #[serde(default)]
    pub version: Option<Version>,
    #[serde(default)]
    pub body: Option<Body>,
    /// `open`, `resolved` or `dangling` on inline comments.
    #[serde(default)]
    pub resolution_status: Option<String>,
    #[serde(default)]
    pub properties: Option<InlineProperties>,
}

impl Comment {
    pub fn into_domain(self, kind: CommentKind, fallback_page: &PageId) -> crate::types::Comment {
        let status = self.resolution_status.as_deref().unwrap_or("");
        let anchor = (kind == CommentKind::Inline).then(|| {
            let props = self.properties.unwrap_or_default();
            InlineAnchor {
                text: props.inline_original_selection.unwrap_or_default(),
                context_before: String::new(),
                context_after: String::new(),
                marker_ref: props.inline_marker_ref,
                orphaned: status.eq_ignore_ascii_case("dangling"),
                ..Default::default()
            }
        });
        let version = self.version;
        crate::types::Comment {
            id: CommentId::new(self.id.as_string()),
            page_id: self
                .page_id
                .map(|p| PageId::new(p.as_string()))
                .unwrap_or_else(|| fallback_page.clone()),
            parent_comment_id: self.parent_comment_id.map(|p| CommentId::new(p.as_string())),
            kind,
            author: version.as_ref().and_then(|v| v.author_id.clone()),
            created_at: version.as_ref().and_then(|v| v.created_at.clone()),
            body_storage: self.body.as_ref().map(|b| b.storage_value()).unwrap_or_default(),
            resolved: status.eq_ignore_ascii_case("resolved"),
            anchor,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageVersion {
    #[serde(default)]
    pub number: Option<u32>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub author_id: Option<String>,
}

impl PageVersion {
    pub fn into_domain(self) -> VersionInfo {
        VersionInfo {
            number: self.number.unwrap_or(1),
            author: self.author_id,
            when: self.created_at,
            message: self.message.filter(|m| !m.is_empty()),
        }
    }
}

/// `GET rest/api/user/current` — the one place Cloud still needs v1, since v2 has no
/// current-user endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentUser {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub public_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

impl CurrentUser {
    pub fn into_domain(self) -> User {
        let display_name = self
            .display_name
            .or_else(|| self.public_name.clone())
            .or_else(|| self.username.clone())
            .or_else(|| self.account_id.clone())
            .unwrap_or_else(|| "unknown".to_string());
        User {
            user_key: None,
            account_id: self.account_id,
            username: self.username,
            display_name,
            email: self.email,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_maps_body_version_parent_and_labels() {
        let json = serde_json::json!({
            "id": "1001",
            "status": "current",
            "title": "Onboarding",
            "spaceId": "500",
            "parentId": "900",
            "createdAt": "2026-01-01T00:00:00Z",
            "version": { "number": 7, "createdAt": "2026-02-02T00:00:00Z", "authorId": "acc-1" },
            "body": { "storage": { "value": "<p>hi</p>", "representation": "storage" } },
            "labels": { "results": [{ "name": "guide" }, { "name": "team" }] }
        });
        let page: Page = serde_json::from_value(json).unwrap();
        assert_eq!(page.space_numeric().as_deref(), Some("500"));
        let domain = page.into_domain("DOCS");
        assert_eq!(domain.body_storage, "<p>hi</p>");
        assert_eq!(domain.summary.version, 7);
        assert_eq!(domain.summary.parent_id, Some(PageId::new("900")));
        assert_eq!(domain.summary.space_key, "DOCS");
        assert_eq!(domain.summary.labels, vec!["guide", "team"]);
        assert_eq!(domain.summary.status, PageStatus::Current);
        assert_eq!(domain.summary.updated_at.as_deref(), Some("2026-02-02T00:00:00Z"));
    }

    #[test]
    fn a_page_without_optional_fields_still_parses() {
        let page: Page = serde_json::from_value(serde_json::json!({ "id": 42 })).unwrap();
        let domain = page.into_domain("");
        assert_eq!(domain.summary.id, PageId::new("42"));
        assert_eq!(domain.summary.version, 1);
        assert!(domain.summary.parent_id.is_none());
        assert_eq!(domain.body_storage, "");
    }

    #[test]
    fn inline_comment_resolution_status_drives_resolved_and_orphaned() {
        let mk = |status: &str| -> crate::types::Comment {
            let c: Comment = serde_json::from_value(serde_json::json!({
                "id": "5",
                "pageId": "1001",
                "resolutionStatus": status,
                "properties": { "inlineMarkerRef": "m-1", "inlineOriginalSelection": "checklist" },
                "body": { "storage": { "value": "<p>note</p>" } }
            }))
            .unwrap();
            c.into_domain(CommentKind::Inline, &PageId::new("1001"))
        };
        let open = mk("open");
        assert!(!open.resolved);
        assert_eq!(open.anchor.as_ref().unwrap().text, "checklist");
        assert_eq!(open.anchor.as_ref().unwrap().marker_ref.as_deref(), Some("m-1"));
        assert!(mk("resolved").resolved);
        assert!(mk("dangling").anchor.unwrap().orphaned);
    }

    #[test]
    fn footer_comments_have_no_anchor() {
        let c: Comment = serde_json::from_value(serde_json::json!({
            "id": "6", "pageId": "1001", "body": { "storage": { "value": "<p>x</p>" } }
        }))
        .unwrap();
        let domain = c.into_domain(CommentKind::Footer, &PageId::new("1001"));
        assert!(domain.anchor.is_none());
        assert!(!domain.resolved);
    }

    #[test]
    fn current_user_falls_back_through_the_name_fields() {
        let u: CurrentUser =
            serde_json::from_value(serde_json::json!({ "accountId": "acc-9" })).unwrap();
        assert_eq!(u.into_domain().display_name, "acc-9");
    }
}
