//! Confluence Cloud client (REST API v2, with v1 fallbacks where v2 has no endpoint).
//!
//! `base_url` is the site root *including* the context path, e.g.
//! `https://acme.atlassian.net/wiki`. Every path below is relative to it.
//!
//! v2 covers pages, spaces, labels (read), attachments (read), comments, versions
//! and search-by-id. It has no endpoint for: the current user, label writes,
//! attachment uploads, CQL search, page moves, or fetching a historical body — those
//! fall back to `rest/api/*` (v1), which Cloud still serves.

/// Serde DTOs for the Cloud REST v2 wire format.
pub mod v2;

use async_trait::async_trait;
use confed_api::client::ConfluenceClient;
use confed_api::error::{ApiError, ApiResult};
use confed_api::http::{Auth, Http};
use confed_api::paginate::{collect_cursor, collect_offset, CursorPage};
use confed_api::types::*;
use confed_api::wire::{resolve_under_base, v1};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// v2 caps `limit` at 250 for page and space collections.
const PAGE_SIZE: usize = 250;

/// v2 endpoints address spaces by numeric id, but users (and confed's config) speak
/// keys. Resolving a key costs a request, so remember both directions.
#[derive(Default)]
struct SpaceCache {
    by_key: HashMap<String, String>,
    by_numeric: HashMap<String, String>,
}

pub struct CloudClient {
    http: Http,
    base_url: String,
    capabilities: Capabilities,
    spaces: Mutex<SpaceCache>,
}

impl CloudClient {
    /// `base_url` is the site root, e.g. `https://acme.atlassian.net/wiki`.
    pub fn new(base_url: &str, auth: Auth, concurrency: usize) -> ApiResult<Self> {
        let mut caps = Capabilities::cloud();
        if concurrency > 0 {
            caps.max_request_concurrency = concurrency;
        }
        Ok(Self {
            http: Http::new(base_url, auth, caps.max_request_concurrency)?,
            base_url: base_url.trim_end_matches('/').to_string(),
            capabilities: caps,
            spaces: Mutex::new(SpaceCache::default()),
        })
    }

    pub fn http(&self) -> &Http {
        &self.http
    }

    pub fn with_http(mut self, http: Http) -> Self {
        self.http = http;
        self
    }

    fn remember_space(&self, key: &str, numeric: &str) {
        if key.is_empty() || numeric.is_empty() {
            return;
        }
        let mut cache = self.spaces.lock().expect("space cache poisoned");
        cache.by_key.insert(key.to_string(), numeric.to_string());
        cache.by_numeric.insert(numeric.to_string(), key.to_string());
    }

    fn cached_numeric(&self, key: &str) -> Option<String> {
        self.spaces.lock().expect("space cache poisoned").by_key.get(key).cloned()
    }

    fn cached_key(&self, numeric: &str) -> Option<String> {
        self.spaces.lock().expect("space cache poisoned").by_numeric.get(numeric).cloned()
    }

    async fn fetch_space(&self, key: &str) -> ApiResult<Space> {
        let page: CursorPage<v2::Space> = self
            .http
            .get_json("api/v2/spaces", &[("keys", key.to_string()), ("limit", "1".to_string())])
            .await?;
        let space = page
            .results
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::NotFound(format!("space {key}")))?
            .into_domain();
        if let Some(numeric) = &space.id.numeric {
            self.remember_space(&space.id.key, numeric);
        }
        Ok(space)
    }

    /// The numeric id v2 needs, from whatever the caller happens to know.
    async fn numeric_space_id(&self, space: &SpaceId) -> ApiResult<String> {
        if let Some(numeric) = space.numeric.as_ref().filter(|n| !n.is_empty()) {
            self.remember_space(&space.key, numeric);
            return Ok(numeric.clone());
        }
        if let Some(numeric) = self.cached_numeric(&space.key) {
            return Ok(numeric);
        }
        if space.key.is_empty() {
            return Err(ApiError::NotFound("space: neither key nor numeric id given".into()));
        }
        self.fetch_space(&space.key)
            .await?
            .id
            .numeric
            .ok_or_else(|| ApiError::NotFound(format!("space {} has no numeric id", space.key)))
    }

    /// v2 page payloads carry `spaceId`, never the key. Best-effort reverse lookup —
    /// a page is still usable without its space key, so failures degrade to `""`.
    async fn space_key_for(&self, numeric: Option<String>) -> String {
        let Some(numeric) = numeric.filter(|n| !n.is_empty()) else { return String::new() };
        if let Some(key) = self.cached_key(&numeric) {
            return key;
        }
        let fetched: Option<v2::Space> =
            self.http.get_json(&format!("api/v2/spaces/{numeric}"), &[]).await.ok();
        match fetched {
            Some(space) => {
                let space = space.into_domain();
                self.remember_space(&space.id.key, &numeric);
                space.id.key
            }
            None => String::new(),
        }
    }

    async fn fetch_page(&self, id: &PageId) -> ApiResult<v2::Page> {
        self.http
            .get_json(
                &format!("api/v2/pages/{id}"),
                &[
                    ("body-format", "storage".to_string()),
                    ("include-labels", "true".to_string()),
                    ("include-version", "true".to_string()),
                ],
            )
            .await
    }
}

/// A stale `version.number` comes back as 409 on most Cloud tenants, but some
/// reject it as a 400 whose message merely mentions the version. Both are conflicts.
fn comment_collection(kind: CommentKind) -> &'static str {
    match kind {
        CommentKind::Footer => "footer-comments",
        CommentKind::Inline => "inline-comments",
    }
}

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

fn encode(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

fn storage_body(value: &str) -> serde_json::Value {
    json!({ "representation": "storage", "value": value })
}

#[async_trait]
impl ConfluenceClient for CloudClient {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn page_url(&self, page: &PageId, space_key: &str) -> String {
        format!("{}/spaces/{}/pages/{}", self.base_url, space_key, page)
    }

    async fn whoami(&self) -> ApiResult<User> {
        // v2 has no current-user endpoint; v1 is the only way to identify ourselves.
        let user: v2::CurrentUser = self.http.get_json("rest/api/user/current", &[]).await?;
        Ok(user.into_domain())
    }

    async fn lookup_user(&self, reference: &UserReference) -> ApiResult<User> {
        // Cloud identifies everybody by account id; the other forms only appear
        // in content migrated from Server, which Cloud resolves the same way.
        let query = match reference {
            UserReference::AccountId(id) => ("accountId", id.clone()),
            UserReference::UserKey(key) => ("key", key.clone()),
            UserReference::Username(name) => ("username", name.clone()),
        };
        let user: v2::CurrentUser =
            self.http.get_json("rest/api/user", &[(query.0, query.1)]).await?;
        Ok(user.into_domain())
    }

    fn user_profile_url(&self, user: &User) -> String {
        match &user.account_id {
            Some(id) => format!("{}/people/{}", self.base_url, encode(id)),
            None => format!("{}/people", self.base_url),
        }
    }

    async fn get_space(&self, key: &str) -> ApiResult<Space> {
        self.fetch_space(key).await
    }

    async fn list_spaces(&self, limit: Option<usize>) -> ApiResult<Vec<Space>> {
        let per_page = limit.unwrap_or(PAGE_SIZE).clamp(1, PAGE_SIZE);
        let raw: Vec<v2::Space> =
            collect_cursor(&self.http, "api/v2/spaces", &[("limit", per_page.to_string())], limit)
                .await?;
        let spaces: Vec<Space> = raw.into_iter().map(|s| s.into_domain()).collect();
        for space in &spaces {
            if let Some(numeric) = &space.id.numeric {
                self.remember_space(&space.id.key, numeric);
            }
        }
        Ok(spaces)
    }

    async fn list_pages(&self, space: &SpaceId) -> ApiResult<Vec<PageSummary>> {
        let numeric = self.numeric_space_id(space).await?;
        let key = if space.key.is_empty() {
            self.space_key_for(Some(numeric.clone())).await
        } else {
            space.key.clone()
        };
        let raw: Vec<v2::Page> = collect_cursor(
            &self.http,
            &format!("api/v2/spaces/{numeric}/pages"),
            &[("limit", PAGE_SIZE.to_string()), ("status", "current".to_string())],
            None,
        )
        .await?;
        Ok(raw.iter().map(|p| p.summary(&key)).collect())
    }

    async fn get_page(&self, id: &PageId, _body: BodyFormat) -> ApiResult<Page> {
        let page = self.fetch_page(id).await?;
        let key = self.space_key_for(page.space_numeric()).await;
        Ok(page.into_domain(&key))
    }

    async fn create_page(&self, new: &NewPage) -> ApiResult<Page> {
        let numeric = self.numeric_space_id(&new.space).await?;
        let mut payload = json!({
            "spaceId": numeric,
            "status": "current",
            "title": new.title,
            "body": storage_body(&new.body_storage),
        });
        if let Some(parent) = &new.parent_id {
            payload["parentId"] = json!(parent.as_str());
        }
        let created: v2::Page = self.http.post_json("api/v2/pages", &payload).await?;

        let key = if new.space.key.is_empty() {
            self.space_key_for(created.space_numeric()).await
        } else {
            new.space.key.clone()
        };
        let mut page = created.into_domain(&key);
        if page.body_storage.is_empty() {
            page.body_storage = new.body_storage.clone();
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
        let status = update.status.unwrap_or(PageStatus::Current);
        let mut payload = json!({
            "id": id.as_str(),
            "status": status.as_str(),
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
            payload["parentId"] = json!(parent.as_str());
        }
        let updated: v2::Page = self
            .http
            .put_json(&format!("api/v2/pages/{id}"), &payload)
            .await
            .map_err(as_conflict)?;

        let key = self.space_key_for(updated.space_numeric()).await;
        let mut page = updated.into_domain(&key);
        if page.body_storage.is_empty() {
            if let Some(body) = &update.body_storage {
                page.body_storage = body.clone();
            }
        }
        Ok(page)
    }

    async fn delete_page(&self, id: &PageId) -> ApiResult<()> {
        self.http.delete(&format!("api/v2/pages/{id}")).await
    }

    async fn move_page(&self, id: &PageId, parent: &PageId, pos: Position) -> ApiResult<()> {
        // v2 has no reorder/move endpoint; the v1 move API is still served on Cloud.
        let (position, target) = match pos {
            Position::Append => ("append", parent.to_string()),
            Position::Before(sibling) => ("before", sibling.to_string()),
            Position::After(sibling) => ("after", sibling.to_string()),
        };
        let _: serde_json::Value = self
            .http
            .put_json(&format!("rest/api/content/{id}/move/{position}/{target}"), &json!({}))
            .await?;
        Ok(())
    }

    async fn get_labels(&self, id: &PageId) -> ApiResult<Vec<String>> {
        let raw: Vec<v2::Label> = collect_cursor(
            &self.http,
            &format!("api/v2/pages/{id}/labels"),
            &[("limit", PAGE_SIZE.to_string())],
            None,
        )
        .await?;
        Ok(raw.into_iter().map(|l| l.name).collect())
    }

    async fn add_label(&self, id: &PageId, label: &str) -> ApiResult<()> {
        // v2 exposes no label write API.
        let _: serde_json::Value = self
            .http
            .post_json(&format!("rest/api/content/{id}/label"), &json!([{ "name": label }]))
            .await?;
        Ok(())
    }

    async fn remove_label(&self, id: &PageId, label: &str) -> ApiResult<()> {
        self.http.delete(&format!("rest/api/content/{id}/label?name={}", encode(label))).await
    }

    async fn list_attachments(&self, id: &PageId) -> ApiResult<Vec<Attachment>> {
        let raw: Vec<v2::Attachment> = collect_cursor(
            &self.http,
            &format!("api/v2/pages/{id}/attachments"),
            &[("limit", PAGE_SIZE.to_string())],
            None,
        )
        .await?;
        Ok(raw.into_iter().map(|a| a.into_domain(id)).collect())
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
        // v1 multipart: v2 is read-only for attachments.
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
        self.http.delete(&format!("api/v2/attachments/{id}")).await
    }

    async fn list_comments(&self, page: &PageId) -> ApiResult<Vec<Comment>> {
        let query = [("body-format", "storage".to_string()), ("limit", PAGE_SIZE.to_string())];
        let footer: Vec<v2::Comment> = collect_cursor(
            &self.http,
            &format!("api/v2/pages/{page}/footer-comments"),
            &query,
            None,
        )
        .await?;
        let inline: Vec<v2::Comment> = collect_cursor(
            &self.http,
            &format!("api/v2/pages/{page}/inline-comments"),
            &query,
            None,
        )
        .await?;

        let mut out: Vec<Comment> =
            footer.into_iter().map(|c| c.into_domain(CommentKind::Footer, page)).collect();
        out.extend(inline.into_iter().map(|c| c.into_domain(CommentKind::Inline, page)));
        Ok(out)
    }

    async fn add_footer_comment(
        &self,
        page: &PageId,
        body: &str,
        reply_to: Option<&CommentId>,
    ) -> ApiResult<Comment> {
        let mut payload = json!({ "pageId": page.as_str(), "body": storage_body(body) });
        if let Some(parent) = reply_to {
            payload["parentCommentId"] = json!(parent.as_str());
        }
        let created: v2::Comment = self.http.post_json("api/v2/footer-comments", &payload).await?;
        let mut comment = created.into_domain(CommentKind::Footer, page);
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
        let payload = json!({
            "pageId": page.as_str(),
            "body": storage_body(body),
            "inlineCommentProperties": {
                "textSelection": anchor.text,
                "textSelectionMatchCount": anchor.match_count.unwrap_or(1),
                "textSelectionMatchIndex": anchor.match_index.unwrap_or(0),
            },
        });
        let created: v2::Comment = self.http.post_json("api/v2/inline-comments", &payload).await?;
        let mut comment = created.into_domain(CommentKind::Inline, page);
        if comment.body_storage.is_empty() {
            comment.body_storage = body.to_string();
        }
        if comment.anchor.as_ref().is_none_or(|a| a.text.is_empty()) {
            comment.anchor = Some(anchor.clone());
        }
        Ok(comment)
    }

    async fn resolve_comment(&self, id: &CommentId) -> ApiResult<()> {
        let path = format!("api/v2/inline-comments/{id}");
        match self.http.put_json::<_, serde_json::Value>(&path, &json!({ "resolved": true })).await
        {
            Ok(_) => Ok(()),
            Err(e) if matches!(e, ApiError::Auth(_) | ApiError::NotFound(_)) => Err(e),
            Err(_) => {
                // Some tenants insist on the full update payload (version + body).
                let current: v2::Comment =
                    self.http.get_json(&path, &[("body-format", "storage".to_string())]).await?;
                let version = current.version.as_ref().and_then(|v| v.number).unwrap_or(1);
                let body = current.body.as_ref().map(|b| b.storage_value()).unwrap_or_default();
                let payload = json!({
                    "version": { "number": version + 1, "message": "resolved by confed" },
                    "body": storage_body(&body),
                    "resolved": true,
                });
                let _: serde_json::Value = self.http.put_json(&path, &payload).await?;
                Ok(())
            }
        }
    }

    async fn update_comment(&self, id: &CommentId, kind: CommentKind, body: &str) -> ApiResult<()> {
        let path = format!("api/v2/{}/{id}", comment_collection(kind));
        let current: v2::Comment = self.http.get_json(&path, &[]).await?;
        let version = current.version.as_ref().and_then(|v| v.number).unwrap_or(1);
        let payload = json!({
            "version": { "number": version + 1, "message": "edited with confed" },
            "body": storage_body(body),
        });
        let _: serde_json::Value =
            self.http.put_json(&path, &payload).await.map_err(as_conflict)?;
        Ok(())
    }

    async fn delete_comment(&self, id: &CommentId, kind: CommentKind) -> ApiResult<()> {
        self.http.delete(&format!("api/v2/{}/{id}", comment_collection(kind))).await
    }

    async fn search_users(&self, query: &str, limit: usize) -> ApiResult<Vec<User>> {
        v1::search_users(&self.http, query, limit).await
    }

    async fn search_cql(&self, cql: &str, limit: usize) -> ApiResult<Vec<SearchResult>> {
        // CQL search never made it to v2.
        let limit = limit.max(1);
        let raw: Vec<v1::SearchResult> = collect_offset(
            &self.http,
            "rest/api/search",
            &[("cql", cql.to_string()), ("expand", v1::SEARCH_EXPAND.to_string())],
            limit.min(100),
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
        let raw: Vec<v2::PageVersion> = collect_cursor(
            &self.http,
            &format!("api/v2/pages/{id}/versions"),
            &[("limit", limit.min(PAGE_SIZE).to_string())],
            Some(limit),
        )
        .await?;
        Ok(raw.into_iter().map(|v| v.into_domain()).collect())
    }

    async fn get_page_at_version(&self, id: &PageId, version: u32) -> ApiResult<Page> {
        // v2 can list versions but cannot return a historical body.
        let content: v1::Content = self
            .http
            .get_json(
                &format!("rest/api/content/{id}"),
                &[
                    ("version", version.to_string()),
                    ("expand", "body.storage,version,space,ancestors".to_string()),
                ],
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

    fn client() -> CloudClient {
        CloudClient::new("https://acme.atlassian.net/wiki", Auth::None, 2).unwrap()
    }

    #[test]
    fn browser_urls_use_the_v2_space_path() {
        let c = client();
        assert_eq!(
            c.page_url(&PageId::new("1001"), "DOCS"),
            "https://acme.atlassian.net/wiki/spaces/DOCS/pages/1001"
        );
        assert_eq!(c.base_url(), "https://acme.atlassian.net/wiki");
    }

    #[test]
    fn the_space_cache_answers_in_both_directions() {
        let c = client();
        assert!(c.cached_numeric("DOCS").is_none());
        c.remember_space("DOCS", "500");
        assert_eq!(c.cached_numeric("DOCS").as_deref(), Some("500"));
        assert_eq!(c.cached_key("500").as_deref(), Some("DOCS"));
        // Empty halves are never cached.
        c.remember_space("", "600");
        assert!(c.cached_key("600").is_none());
    }

    #[tokio::test]
    async fn a_numeric_id_on_the_space_needs_no_request() {
        let c = client();
        let id = SpaceId { key: "DOCS".into(), numeric: Some("500".into()) };
        assert_eq!(c.numeric_space_id(&id).await.unwrap(), "500");
        // …and it seeded the cache for the key-only form.
        assert_eq!(c.numeric_space_id(&SpaceId::from_key("DOCS")).await.unwrap(), "500");
    }

    #[test]
    fn version_rejections_become_conflicts() {
        let e = as_conflict(ApiError::Server {
            status: 400,
            body: "PUT api/v2/pages/1: Version must be incremented".into(),
        });
        assert!(matches!(e, ApiError::Conflict(_)), "got {e:?}");

        let e = as_conflict(ApiError::Server { status: 409, body: "conflict".into() });
        assert!(matches!(e, ApiError::Conflict(_)), "got {e:?}");

        let e = as_conflict(ApiError::Server { status: 500, body: "boom".into() });
        assert!(matches!(e, ApiError::Server { .. }), "got {e:?}");
    }

    #[test]
    fn label_names_are_percent_encoded_for_the_delete_query() {
        assert_eq!(encode("needs review/urgent"), "needs%20review%2Furgent");
    }
}
