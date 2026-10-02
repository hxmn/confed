//! Confluence Data Center client (REST API v1).
//!
//! `base_url` is the Confluence context root, e.g. `https://wiki.corp/confluence`.
//! Everything lives under `rest/api/*`, spaces are addressed by key, and collections
//! page with `start`/`limit`.
//!
//! Inline comments are the exception: REST v1 cannot create, reply to or resolve
//! them, so those go through the private plugin API the page view itself uses,
//! `rest/inlinecomments/1.0`. It is undocumented and may change in any release;
//! the request shapes come from captures of DC 9.5.4 (`tests/fixtures/dc-inline`),
//! majors outside [`TESTED_MAJORS`] get a warning, and its failures name the step
//! that failed.

use crate::client::ConfluenceClient;
use crate::error::{ApiError, ApiResult};
use crate::http::{Auth, Http};
use crate::paginate::{collect_offset, OffsetPage};
use crate::types::*;
use crate::wire::{inline_dc, resolve_under_base, v1};
use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::json;
use std::path::Path;

/// v1 collections are happiest at 100 per request; 200+ times out on older instances.
const PAGE_SIZE: usize = 100;

/// The expansions a page needs to fill a [`PageSummary`] without extra round trips.
const PAGE_EXPAND: &str = "version,ancestors,metadata.labels,space";
const FULL_PAGE_EXPAND: &str = "body.storage,version,ancestors,metadata.labels,space";
const COMMENT_EXPAND: &str =
    "body.storage,extensions.inlineProperties,extensions.resolution,ancestors,history,container";

/// The private inline-comment API.
const INLINE_API: &str = "rest/inlinecomments/1.0/comments";

/// Data Center majors the inline-comment API was captured on.
pub const TESTED_MAJORS: &[u32] = &[9];

/// What the page view sends for highlight positions. The real value records
/// DOM positions confed cannot reproduce; the server accepts an empty list.
const SERIALIZED_HIGHLIGHTS: &str = "[]";

pub struct DcClient {
    http: Http,
    base_url: String,
    capabilities: Capabilities,
    /// The product version, fetched once on first use of the private API.
    version: tokio::sync::OnceCell<Option<String>>,
}

impl DcClient {
    /// `base_url` is the Confluence context root, e.g. `https://wiki.corp/confluence`.
    pub fn new(base_url: &str, auth: Auth, concurrency: usize) -> ApiResult<Self> {
        let mut caps = Capabilities::data_center();
        if concurrency > 0 {
            caps.max_request_concurrency = concurrency;
        }
        Ok(Self {
            http: Http::new(base_url, auth, caps.max_request_concurrency)?,
            base_url: base_url.trim_end_matches('/').to_string(),
            capabilities: caps,
            version: tokio::sync::OnceCell::new(),
        })
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    pub fn with_http(mut self, http: Http) -> Self {
        self.http = http;
        self
    }

    async fn fetch_content(&self, id: &str, query: &[(&str, String)]) -> ApiResult<v1::Content> {
        self.http.get_json(&format!("rest/api/content/{id}"), query).await
    }

    async fn cached_version(&self) -> Option<String> {
        self.version
            .get_or_init(|| async {
                let manifest = self.http.get_text("rest/applinks/1.0/manifest", &[]).await.ok()?;
                parse_manifest_version(&manifest)
            })
            .await
            .clone()
    }

    /// Before the first call to the private API: warn when this server's major
    /// is one the API was not captured on.
    async fn check_inline_api(&self) {
        let version = self.cached_version().await;
        static WARNED: std::sync::Once = std::sync::Once::new();
        let major = version.as_deref().and_then(|v| v.split('.').next()?.parse::<u32>().ok());
        if major.is_none_or(|m| !TESTED_MAJORS.contains(&m)) {
            WARNED.call_once(|| {
                tracing::warn!(
                    "Confluence Data Center {} has not been tested with confed's inline \
                     comments, which use an undocumented API (tested: {}.x)",
                    version.as_deref().unwrap_or("of unknown version"),
                    TESTED_MAJORS.iter().map(u32::to_string).collect::<Vec<_>>().join(".x, ")
                );
            });
        }
    }

    /// Name the step that failed and what the status means for this API.
    async fn inline_error(&self, step: &str, err: ApiError) -> ApiError {
        let on = match self.cached_version().await {
            Some(v) => format!("Data Center {v}"),
            None => "this Data Center".to_string(),
        };
        match err {
            ApiError::NotFound(body) | ApiError::Server { status: 405, body } => {
                ApiError::Unsupported {
                    flavor: Flavor::DataCenter.as_str(),
                    operation: format!(
                        "{step} ({on} does not answer `{INLINE_API}`: {body}; \
                         add the comment in the browser)"
                    ),
                }
            }
            ApiError::Server { status, body }
                if status == 412 || body.to_ascii_lowercase().contains("selection") =>
            {
                ApiError::Rejected(format!("{step}: {on} refused it (HTTP {status}): {body}"))
            }
            ApiError::Server { status, body } => {
                ApiError::Server { status, body: format!("{step}: {body}") }
            }
            other => other,
        }
    }
}

/// `<version>9.5.4</version>` from the applinks manifest (XML by default,
/// JSON when the server prefers it).
fn parse_manifest_version(manifest: &str) -> Option<String> {
    let value = if let Some(start) = manifest.find("<version>") {
        let rest = &manifest[start + "<version>".len()..];
        rest[..rest.find('<')?].to_string()
    } else {
        let json: serde_json::Value = serde_json::from_str(manifest).ok()?;
        json.get("version")?.as_str()?.to_string()
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn now_millis() -> String {
    chrono::Utc::now().timestamp_millis().to_string()
}

/// Data Center answers a stale `version.number` with 409, and some versions with a
/// 400 whose message only mentions the version.
fn as_conflict(err: ApiError) -> ApiError {
    match err {
        ApiError::Server { status, body }
            if status == 409
                || (status == 400 && body.to_ascii_lowercase().contains("version")) =>
        {
            ApiError::Conflict(body)
        }
        other => other,
    }
}

/// A username is a path segment, so `/` and `?` must not survive, but the dots
/// and hyphens usernames are full of should stay readable.
fn encode_path(value: &str) -> String {
    utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
        .replace("%2E", ".")
        .replace("%2D", "-")
        .replace("%5F", "_")
}

fn encode(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

fn storage_body(value: &str) -> serde_json::Value {
    json!({ "storage": { "value": value, "representation": "storage" } })
}

fn ancestors(parent: Option<&PageId>) -> serde_json::Value {
    match parent {
        Some(id) => json!([{ "id": id.as_str() }]),
        None => json!([]),
    }
}

#[async_trait]
impl ConfluenceClient for DcClient {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn page_url(&self, page: &PageId, _space_key: &str) -> String {
        format!("{}/pages/viewpage.action?pageId={}", self.base_url, page)
    }

    async fn whoami(&self) -> ApiResult<User> {
        let user: v1::User = self.http.get_json("rest/api/user/current", &[]).await?;
        Ok(user.into_domain())
    }

    async fn lookup_user(&self, reference: &UserReference) -> ApiResult<User> {
        // Data Center takes exactly one of these, and userkey is what mentions
        // in page content carry.
        let query = match reference {
            UserReference::UserKey(key) => ("key", key.clone()),
            UserReference::Username(name) => ("username", name.clone()),
            UserReference::AccountId(id) => ("accountId", id.clone()),
        };
        let user: v1::User = self.http.get_json("rest/api/user", &[(query.0, query.1)]).await?;
        Ok(user.into_domain())
    }

    fn user_profile_url(&self, user: &User) -> String {
        // Data Center profiles live at a tilde-prefixed username, not at the
        // opaque key the markup uses.
        match &user.username {
            Some(username) => format!("{}/display/~{}", self.base_url, encode_path(username)),
            None => format!("{}/display", self.base_url),
        }
    }

    async fn get_space(&self, key: &str) -> ApiResult<Space> {
        let page: OffsetPage<v1::Space> = self
            .http
            .get_json(
                "rest/api/space",
                &[
                    ("spaceKey", key.to_string()),
                    ("expand", "homepage".to_string()),
                    ("limit", "1".to_string()),
                ],
            )
            .await?;
        page.results
            .into_iter()
            .next()
            .map(|s| s.into_domain())
            .ok_or_else(|| ApiError::NotFound(format!("space {key}")))
    }

    async fn list_spaces(&self, limit: Option<usize>) -> ApiResult<Vec<Space>> {
        let raw: Vec<v1::Space> = collect_offset(
            &self.http,
            "rest/api/space",
            &[("expand", "homepage".to_string())],
            limit.unwrap_or(PAGE_SIZE).clamp(1, PAGE_SIZE),
            limit,
        )
        .await?;
        Ok(raw.into_iter().map(|s| s.into_domain()).collect())
    }

    async fn list_pages(&self, space: &SpaceId) -> ApiResult<Vec<PageSummary>> {
        let raw: Vec<v1::Content> = collect_offset(
            &self.http,
            "rest/api/content",
            &[
                ("spaceKey", space.key.clone()),
                ("type", "page".to_string()),
                ("expand", PAGE_EXPAND.to_string()),
            ],
            PAGE_SIZE,
            None,
        )
        .await?;
        Ok(raw.iter().map(|c| c.summary(&space.key)).collect())
    }

    async fn get_page(&self, id: &PageId, _body: BodyFormat) -> ApiResult<Page> {
        let content =
            self.fetch_content(id.as_str(), &[("expand", FULL_PAGE_EXPAND.to_string())]).await?;
        Ok(content.into_page(""))
    }

    async fn create_page(&self, new: &NewPage) -> ApiResult<Page> {
        let payload = json!({
            "type": "page",
            "title": new.title,
            "space": { "key": new.space.key },
            "ancestors": ancestors(new.parent_id.as_ref()),
            "body": storage_body(&new.body_storage),
        });
        let created: v1::Content = self.http.post_json("rest/api/content", &payload).await?;
        let mut page = created.into_page(&new.space.key);
        if page.body_storage.is_empty() {
            page.body_storage = new.body_storage.clone();
        }
        if page.summary.parent_id.is_none() {
            page.summary.parent_id = new.parent_id.clone();
        }
        for label in &new.labels {
            self.add_label(&page.summary.id, label).await?;
        }
        if page.summary.labels.is_empty() {
            page.summary.labels = new.labels.clone();
        }
        Ok(page)
    }

    async fn update_page(&self, id: &PageId, update: &PageUpdate) -> ApiResult<Page> {
        let mut payload = json!({
            "id": id.as_str(),
            "type": "page",
            "title": update.title,
            "version": {
                "number": update.version,
                "message": update.message.clone().unwrap_or_default(),
            },
        });
        if let Some(body) = &update.body_storage {
            payload["body"] = storage_body(body);
        }
        if let Some(parent) = &update.parent_id {
            payload["ancestors"] = ancestors(Some(parent));
        }
        if let Some(status) = update.status {
            payload["status"] = json!(status.as_str());
        }
        let updated: v1::Content = self
            .http
            .put_json(&format!("rest/api/content/{id}"), &payload)
            .await
            .map_err(as_conflict)?;

        let mut page = updated.into_page("");
        if page.body_storage.is_empty() {
            if let Some(body) = &update.body_storage {
                page.body_storage = body.clone();
            }
        }
        Ok(page)
    }

    async fn delete_page(&self, id: &PageId) -> ApiResult<()> {
        self.http.delete(&format!("rest/api/content/{id}")).await
    }

    async fn move_page(&self, id: &PageId, parent: &PageId, pos: Position) -> ApiResult<()> {
        match pos {
            // Re-parenting is a normal content update with new ancestors, which means
            // it needs a version bump like any other edit.
            Position::Append => {
                let current =
                    self.fetch_content(id.as_str(), &[("expand", "version".to_string())]).await?;
                let next_version = current.version.as_ref().and_then(|v| v.number).unwrap_or(1) + 1;
                let payload = json!({
                    "id": id.as_str(),
                    "type": "page",
                    "title": current.title.clone().unwrap_or_default(),
                    "ancestors": ancestors(Some(parent)),
                    "version": { "number": next_version, "message": "moved by confed" },
                });
                let _: serde_json::Value = self
                    .http
                    .put_json(&format!("rest/api/content/{id}"), &payload)
                    .await
                    .map_err(as_conflict)?;
                Ok(())
            }
            // Sibling ordering only exists on the dedicated move endpoint.
            Position::Before(sibling) | Position::After(sibling) => {
                let position = if matches!(pos, Position::Before(_)) { "before" } else { "after" };
                let _: serde_json::Value = self
                    .http
                    .put_json(
                        &format!("rest/api/content/{id}/move/{position}/{sibling}"),
                        &json!({}),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    async fn get_labels(&self, id: &PageId) -> ApiResult<Vec<String>> {
        let raw: Vec<v1::Label> = collect_offset(
            &self.http,
            &format!("rest/api/content/{id}/label"),
            &[],
            PAGE_SIZE,
            None,
        )
        .await?;
        Ok(raw.into_iter().filter_map(|l| l.name).collect())
    }

    async fn add_label(&self, id: &PageId, label: &str) -> ApiResult<()> {
        let _: serde_json::Value = self
            .http
            .post_json(
                &format!("rest/api/content/{id}/label"),
                &json!([{ "prefix": "global", "name": label }]),
            )
            .await?;
        Ok(())
    }

    async fn remove_label(&self, id: &PageId, label: &str) -> ApiResult<()> {
        self.http.delete(&format!("rest/api/content/{id}/label?name={}", encode(label))).await
    }

    async fn list_attachments(&self, id: &PageId) -> ApiResult<Vec<Attachment>> {
        let raw: Vec<v1::Content> = collect_offset(
            &self.http,
            &format!("rest/api/content/{id}/child/attachment"),
            &[("expand", "version,container".to_string())],
            PAGE_SIZE,
            None,
        )
        .await?;
        Ok(raw.into_iter().map(|c| c.into_attachment(id)).collect())
    }

    async fn download_attachment(&self, a: &Attachment, dest: &Path) -> ApiResult<u64> {
        if a.download_url.is_empty() {
            return Err(ApiError::NotFound(format!("attachment {} has no download link", a.id)));
        }
        let url = resolve_under_base(self.http.base_url(), &a.download_url)?;
        self.http.download_to(url.as_str(), dest).await
    }

    async fn upload_attachment(
        &self,
        page: &PageId,
        file: &Path,
        existing: Option<&AttachmentId>,
    ) -> ApiResult<Attachment> {
        let path = match existing {
            Some(id) => format!("rest/api/content/{page}/child/attachment/{id}/data"),
            None => format!("rest/api/content/{page}/child/attachment"),
        };
        let response: serde_json::Value = self
            .http
            .upload_multipart(&path, file, "file", &[("minorEdit", "true".to_string())])
            .await?;
        v1::attachment_from_response(response, page)
    }

    async fn delete_attachment(&self, id: &AttachmentId) -> ApiResult<()> {
        // Attachments are content, so they are deleted through the content endpoint.
        self.http.delete(&format!("rest/api/content/{id}")).await
    }

    async fn list_comments(&self, page: &PageId) -> ApiResult<Vec<Comment>> {
        let raw: Vec<v1::Content> = collect_offset(
            &self.http,
            &format!("rest/api/content/{page}/child/comment"),
            &[("expand", COMMENT_EXPAND.to_string()), ("depth", "all".to_string())],
            PAGE_SIZE,
            None,
        )
        .await?;
        Ok(raw.into_iter().map(|c| c.into_comment(page)).collect())
    }

    async fn add_footer_comment(
        &self,
        page: &PageId,
        body: &str,
        reply_to: Option<&CommentId>,
    ) -> ApiResult<Comment> {
        let mut payload = json!({
            "type": "comment",
            "container": { "id": page.as_str(), "type": "page" },
            "body": storage_body(body),
        });
        if let Some(parent) = reply_to {
            payload["ancestors"] = json!([{ "id": parent.as_str() }]);
        }
        let created: v1::Content = self.http.post_json("rest/api/content", &payload).await?;
        let mut comment = created.into_comment(page);
        // A freshly created comment has no `extensions`, so it always parses as a
        // footer comment — which is exactly what this endpoint creates.
        if comment.body_storage.is_empty() {
            comment.body_storage = body.to_string();
        }
        if comment.parent_comment_id.is_none() {
            comment.parent_comment_id = reply_to.cloned();
        }
        Ok(comment)
    }

    async fn add_inline_comment(
        &self,
        page: &PageId,
        anchor: &InlineAnchor,
        body: &str,
    ) -> ApiResult<Comment> {
        self.check_inline_api().await;
        let version = self
            .fetch_content(page.as_str(), &[("expand", "version".to_string())])
            .await?
            .summary("")
            .version;
        // The page view's own request (fixtures/dc-inline/create.request.json),
        // less the author fields the server takes from the credentials.
        let payload = json!({
            "originalSelection": anchor.text,
            "body": body,
            "matchIndex": anchor.match_index.unwrap_or(0),
            "numMatches": anchor.match_count.unwrap_or(1),
            "serializedHighlights": SERIALIZED_HIGHLIGHTS,
            "containerId": page.as_str(),
            "containerVersion": version.to_string(),
            "parentCommentId": "0",
            "lastFetchTime": now_millis(),
            "hasDeletePermission": true,
            "hasEditPermission": true,
            "hasResolvePermission": true,
            "resolveProperties": { "resolved": false, "resolvedTime": 0 },
            "deleted": false,
        });
        let created: inline_dc::InlineComment =
            match self.http.post_json(INLINE_API, &payload).await {
                Ok(c) => c,
                Err(e) => return Err(self.inline_error("create inline comment", e).await),
            };
        let mut comment = created.into_comment(page);
        if comment.body_storage.is_empty() {
            comment.body_storage = body.to_string();
        }
        if let Some(a) = comment.anchor.as_mut() {
            if a.text.is_empty() {
                a.text = anchor.text.clone();
            }
            a.match_index = anchor.match_index;
            a.match_count = anchor.match_count;
        }
        Ok(comment)
    }

    async fn add_inline_reply(
        &self,
        page: &PageId,
        parent: &CommentId,
        body: &str,
    ) -> ApiResult<Comment> {
        self.check_inline_api().await;
        let parent_id: serde_json::Value = parent
            .as_str()
            .parse::<u64>()
            .map(Into::into)
            .unwrap_or_else(|_| parent.as_str().into());
        let payload = json!({ "body": body, "commentId": parent_id });
        let path = format!(
            "{INLINE_API}/{}/replies?containerId={}",
            encode(parent.as_str()),
            encode(page.as_str())
        );
        let created: inline_dc::InlineComment = match self.http.post_json(&path, &payload).await {
            Ok(c) => c,
            Err(e) => return Err(self.inline_error("reply to inline comment", e).await),
        };
        let mut comment = created.into_comment(page);
        if comment.body_storage.is_empty() {
            comment.body_storage = body.to_string();
        }
        if comment.parent_comment_id.is_none() {
            comment.parent_comment_id = Some(parent.clone());
        }
        comment.anchor = None;
        Ok(comment)
    }

    async fn resolve_comment(&self, id: &CommentId) -> ApiResult<()> {
        // The page view sends the whole comment back; rebuild it from REST v1.
        let content =
            self.fetch_content(id.as_str(), &[("expand", COMMENT_EXPAND.to_string())]).await?;
        let comment = content.into_comment(&PageId::new(""));
        let Some(anchor) = comment.anchor.filter(|_| comment.kind == CommentKind::Inline) else {
            return Err(ApiError::unsupported(
                Flavor::DataCenter,
                "resolve a page (footer) comment — Data Center only resolves inline threads",
            ));
        };
        self.check_inline_api().await;
        let numeric: serde_json::Value =
            id.as_str().parse::<u64>().map(Into::into).unwrap_or_else(|_| id.as_str().into());
        let payload = json!({
            "id": numeric,
            "originalSelection": anchor.text,
            "body": comment.body_storage,
            "matchIndex": 0,
            "numMatches": 1,
            "serializedHighlights": SERIALIZED_HIGHLIGHTS,
            "containerId": comment.page_id.as_str(),
            "parentCommentId": "0",
            "markerRef": anchor.marker_ref,
            "lastFetchTime": now_millis(),
            "hasDeletePermission": true,
            "hasEditPermission": true,
            "hasResolvePermission": true,
            "resolveProperties": { "resolved": false, "resolvedTime": 0, "resolvedByDangling": false },
            "deleted": false,
        });
        let path = format!("{INLINE_API}/{}/resolve/true/dangling/false", encode(id.as_str()));
        let answer: inline_dc::ResolveResponse = match self.http.put_json(&path, &payload).await {
            Ok(r) => r,
            Err(e) => return Err(self.inline_error("resolve inline comment", e).await),
        };
        if answer.resolve_properties.is_some_and(|r| !r.resolved) {
            return Err(ApiError::Rejected(format!(
                "resolve inline comment: the server answered but left {id} open"
            )));
        }
        Ok(())
    }

    async fn server_version(&self) -> ApiResult<Option<String>> {
        Ok(self.cached_version().await)
    }

    async fn search_cql(&self, cql: &str, limit: usize) -> ApiResult<Vec<SearchResult>> {
        let limit = limit.max(1);
        let raw: Vec<v1::SearchResult> = collect_offset(
            &self.http,
            "rest/api/search",
            &[("cql", cql.to_string()), ("expand", v1::SEARCH_EXPAND.to_string())],
            limit.min(PAGE_SIZE),
            Some(limit),
        )
        .await?;
        let base = self.http.base_url().clone();
        Ok(raw
            .into_iter()
            .filter_map(|r| {
                r.into_domain(|u| {
                    resolve_under_base(&base, u).map(String::from).unwrap_or_else(|_| u.to_string())
                })
            })
            .collect())
    }

    async fn get_page_versions(&self, id: &PageId, limit: usize) -> ApiResult<Vec<VersionInfo>> {
        let limit = limit.max(1);
        let raw: Vec<v1::Version> = collect_offset(
            &self.http,
            &format!("rest/api/content/{id}/version"),
            &[("expand", "content.version".to_string())],
            limit.min(PAGE_SIZE),
            Some(limit),
        )
        .await?;
        Ok(raw.into_iter().map(|v| v.into_domain()).collect())
    }

    async fn get_page_at_version(&self, id: &PageId, version: u32) -> ApiResult<Page> {
        let content = self
            .fetch_content(
                id.as_str(),
                &[("version", version.to_string()), ("expand", FULL_PAGE_EXPAND.to_string())],
            )
            .await?;
        let mut page = content.into_page("");
        page.summary.version = version;
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> DcClient {
        DcClient::new("https://wiki.corp/confluence", Auth::None, 2).unwrap()
    }

    #[test]
    fn browser_urls_use_the_page_id_action() {
        let c = client();
        assert_eq!(
            c.page_url(&PageId::new("1001"), "DOCS"),
            "https://wiki.corp/confluence/pages/viewpage.action?pageId=1001"
        );
    }

    #[test]
    fn capabilities_report_the_data_center_gaps() {
        let c = client();
        assert_eq!(c.flavor(), Flavor::DataCenter);
        assert!(c.capabilities().inline_comment_create, "through the inline-comment plugin API");
        assert!(c.capabilities().comment_resolve);
        assert!(!c.capabilities().adf);
    }

    #[test]
    fn version_rejections_become_conflicts() {
        let e = as_conflict(ApiError::Server {
            status: 409,
            body: "Version must be incremented".into(),
        });
        assert!(matches!(e, ApiError::Conflict(_)), "got {e:?}");
        let e = as_conflict(ApiError::Server { status: 500, body: "boom".into() });
        assert!(matches!(e, ApiError::Server { .. }), "got {e:?}");
    }

    #[test]
    fn ancestors_are_omitted_for_root_pages() {
        assert_eq!(ancestors(None).to_string(), "[]");
        assert_eq!(ancestors(Some(&PageId::new("7"))).to_string(), r#"[{"id":"7"}]"#);
    }
}
