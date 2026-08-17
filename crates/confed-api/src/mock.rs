//! A stateful in-memory Confluence, used by sync-engine and command tests.
//!
//! It behaves like a server rather than a fixture: versions increment, stale
//! updates are rejected with [`ApiError::Conflict`], and capability gaps follow
//! the configured flavor. The same scenario test can therefore run against both
//! flavors by flipping [`MockClient::new`]'s argument.

use crate::client::ConfluenceClient;
use crate::error::{ApiError, ApiResult};
use crate::types::*;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
struct MockPage {
    summary: PageSummary,
    body: String,
    history: Vec<VersionInfo>,
    bodies: HashMap<u32, String>,
    deleted: bool,
}

#[derive(Default)]
struct MockState {
    /// People resolvable by the id a mention would carry.
    users: HashMap<String, User>,
    pages: HashMap<String, MockPage>,
    attachments: HashMap<String, (Attachment, Vec<u8>)>,
    comments: Vec<Comment>,
    spaces: HashMap<String, Space>,
    /// Every mutating call recorded, so tests can assert "--dry-run wrote nothing".
    calls: Vec<String>,
}

pub struct MockClient {
    state: Arc<Mutex<MockState>>,
    capabilities: Capabilities,
    base_url: String,
    next_id: Arc<AtomicU64>,
    user: User,
}

impl MockClient {
    pub fn new(flavor: Flavor) -> Self {
        let capabilities = match flavor {
            Flavor::Cloud => Capabilities::cloud(),
            Flavor::DataCenter => Capabilities::data_center(),
        };
        let mut state = MockState::default();
        state.spaces.insert(
            "DOCS".to_string(),
            Space {
                id: SpaceId { key: "DOCS".into(), numeric: Some("1001".into()) },
                name: "Documentation".into(),
                kind: Some("global".into()),
                homepage_id: None,
            },
        );
        Self {
            state: Arc::new(Mutex::new(state)),
            capabilities,
            base_url: match flavor {
                Flavor::Cloud => "https://mock.atlassian.net/wiki".into(),
                Flavor::DataCenter => "https://wiki.mock.test".into(),
            },
            next_id: Arc::new(AtomicU64::new(1000)),
            user: User {
                account_id: Some("acc-1".into()),
                username: Some("tester".into()),
                display_name: "Test User".into(),
                email: Some("tester@example.com".into()),
            },
        }
    }

    fn fresh_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::SeqCst).to_string()
    }

    fn record(&self, call: impl Into<String>) {
        self.state.lock().expect("mock poisoned").calls.push(call.into());
    }

    /// Every mutating call made so far, e.g. `update_page:1001`.
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().expect("mock poisoned").calls.clone()
    }

    pub fn mutating_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| {
                c.starts_with("create")
                    || c.starts_with("update")
                    || c.starts_with("delete")
                    || c.starts_with("move")
                    || c.starts_with("add")
                    || c.starts_with("remove")
                    || c.starts_with("upload")
                    || c.starts_with("resolve")
            })
            .collect()
    }

    /// Seed a page directly, bypassing the API (test setup).
    pub fn seed_page(
        &self,
        id: &str,
        title: &str,
        parent: Option<&str>,
        body_storage: &str,
    ) -> PageId {
        let summary = PageSummary {
            id: PageId::new(id),
            title: title.to_string(),
            space_key: "DOCS".into(),
            parent_id: parent.map(PageId::new),
            position: None,
            version: 1,
            status: PageStatus::Current,
            labels: vec![],
            author: Some("Test User".into()),
            created_at: Some("2026-01-01T00:00:00Z".into()),
            updated_at: Some("2026-01-01T00:00:00Z".into()),
        };
        let mut bodies = HashMap::new();
        bodies.insert(1, body_storage.to_string());
        let page = MockPage {
            summary,
            body: body_storage.to_string(),
            history: vec![VersionInfo {
                number: 1,
                author: Some("Test User".into()),
                when: Some("2026-01-01T00:00:00Z".into()),
                message: Some("created".into()),
            }],
            bodies,
            deleted: false,
        };
        self.state.lock().expect("mock poisoned").pages.insert(id.to_string(), page);
        PageId::new(id)
    }

    /// Simulate somebody else editing the page on the server.
    pub fn remote_edit(&self, id: &str, new_body: &str) -> u32 {
        let mut state = self.state.lock().expect("mock poisoned");
        let page = state.pages.get_mut(id).expect("page seeded");
        page.summary.version += 1;
        page.body = new_body.to_string();
        page.bodies.insert(page.summary.version, new_body.to_string());
        page.summary.updated_at = Some("2026-08-14T12:00:00Z".into());
        page.history.push(VersionInfo {
            number: page.summary.version,
            author: Some("Alice Ng".into()),
            when: Some("2026-08-14T12:00:00Z".into()),
            message: Some("remote edit".into()),
        });
        page.summary.version
    }

    /// Make somebody resolvable, keyed by the id a mention carries.
    pub fn seed_user(&self, id: &str, username: &str, display_name: &str) {
        self.state.lock().expect("mock poisoned").users.insert(
            id.to_string(),
            User {
                account_id: Some(id.to_string()),
                username: Some(username.to_string()),
                display_name: display_name.to_string(),
                email: None,
            },
        );
    }

    pub fn seed_comment(&self, page_id: &str, body_storage: &str, kind: CommentKind) -> CommentId {
        let id = CommentId::new(self.fresh_id());
        self.state.lock().expect("mock poisoned").comments.push(Comment {
            id: id.clone(),
            page_id: PageId::new(page_id),
            parent_comment_id: None,
            kind,
            author: Some("Alice Ng".into()),
            created_at: Some("2026-07-30T10:02:00Z".into()),
            body_storage: body_storage.to_string(),
            resolved: false,
            anchor: (kind == CommentKind::Inline).then(|| InlineAnchor {
                text: "first week checklist".into(),
                context_before: "during your ".into(),
                context_after: " and then".into(),
                marker_ref: Some("marker-1".into()),
                orphaned: false,
            }),
        });
        id
    }

    pub fn page_body(&self, id: &str) -> Option<String> {
        self.state.lock().expect("mock poisoned").pages.get(id).map(|p| p.body.clone())
    }

    pub fn page_version(&self, id: &str) -> Option<u32> {
        self.state.lock().expect("mock poisoned").pages.get(id).map(|p| p.summary.version)
    }

    pub fn page_title(&self, id: &str) -> Option<String> {
        self.state.lock().expect("mock poisoned").pages.get(id).map(|p| p.summary.title.clone())
    }

    /// Rename a page server-side without going through the API.
    pub fn rename_page(&self, id: &str, title: &str) -> u32 {
        let mut state = self.state.lock().expect("mock poisoned");
        let page = state.pages.get_mut(id).expect("page seeded");
        page.summary.version += 1;
        page.summary.title = title.to_string();
        page.history.push(VersionInfo {
            number: page.summary.version,
            author: Some("Alice Ng".into()),
            when: Some("2026-08-14T12:00:00Z".into()),
            message: Some("renamed".into()),
        });
        page.summary.version
    }

    /// Delete a page server-side without going through the API, to simulate
    /// somebody else removing it.
    pub fn delete_page_directly(&self, id: &str) {
        let mut state = self.state.lock().expect("mock poisoned");
        if let Some(page) = state.pages.get_mut(id) {
            page.deleted = true;
            page.summary.status = PageStatus::Trashed;
        }
    }

    pub fn page_exists(&self, id: &str) -> bool {
        self.state.lock().expect("mock poisoned").pages.get(id).is_some_and(|p| !p.deleted)
    }
}

#[async_trait]
impl ConfluenceClient for MockClient {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn page_url(&self, page: &PageId, space_key: &str) -> String {
        match self.capabilities.flavor {
            Flavor::Cloud => format!("{}/spaces/{}/pages/{}", self.base_url, space_key, page),
            Flavor::DataCenter => {
                format!("{}/pages/viewpage.action?pageId={}", self.base_url, page)
            }
        }
    }

    async fn whoami(&self) -> ApiResult<User> {
        Ok(self.user.clone())
    }

    async fn lookup_user(&self, reference: &UserReference) -> ApiResult<User> {
        self.state
            .lock()
            .expect("mock poisoned")
            .users
            .get(reference.value())
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("user {}", reference.value())))
    }

    fn user_profile_url(&self, user: &User) -> String {
        match self.capabilities.flavor {
            Flavor::Cloud => {
                format!("{}/people/{}", self.base_url, user.account_id.clone().unwrap_or_default())
            }
            Flavor::DataCenter => {
                format!("{}/display/~{}", self.base_url, user.username.clone().unwrap_or_default())
            }
        }
    }

    async fn get_space(&self, key: &str) -> ApiResult<Space> {
        self.state
            .lock()
            .expect("mock poisoned")
            .spaces
            .get(key)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("space {key}")))
    }

    async fn list_spaces(&self, _limit: Option<usize>) -> ApiResult<Vec<Space>> {
        Ok(self.state.lock().expect("mock poisoned").spaces.values().cloned().collect())
    }

    async fn list_pages(&self, _space: &SpaceId) -> ApiResult<Vec<PageSummary>> {
        let state = self.state.lock().expect("mock poisoned");
        let mut pages: Vec<_> =
            state.pages.values().filter(|p| !p.deleted).map(|p| p.summary.clone()).collect();
        pages.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        Ok(pages)
    }

    async fn get_page(&self, id: &PageId, _body: BodyFormat) -> ApiResult<Page> {
        let state = self.state.lock().expect("mock poisoned");
        let page = state
            .pages
            .get(id.as_str())
            .filter(|p| !p.deleted)
            .ok_or_else(|| ApiError::NotFound(format!("page {id}")))?;
        Ok(Page { summary: page.summary.clone(), body_storage: page.body.clone() })
    }

    async fn create_page(&self, new: &NewPage) -> ApiResult<Page> {
        let id = self.fresh_id();
        self.record(format!("create_page:{}", new.title));
        let summary = PageSummary {
            id: PageId::new(&id),
            title: new.title.clone(),
            space_key: new.space.key.clone(),
            parent_id: new.parent_id.clone(),
            position: None,
            version: 1,
            status: PageStatus::Current,
            labels: new.labels.clone(),
            author: Some(self.user.display_name.clone()),
            created_at: Some("2026-08-16T00:00:00Z".into()),
            updated_at: Some("2026-08-16T00:00:00Z".into()),
        };
        let mut bodies = HashMap::new();
        bodies.insert(1, new.body_storage.clone());
        let page = MockPage {
            summary: summary.clone(),
            body: new.body_storage.clone(),
            history: vec![VersionInfo {
                number: 1,
                author: Some(self.user.display_name.clone()),
                when: Some("2026-08-16T00:00:00Z".into()),
                message: None,
            }],
            bodies,
            deleted: false,
        };
        self.state.lock().expect("mock poisoned").pages.insert(id, page);
        Ok(Page { summary, body_storage: new.body_storage.clone() })
    }

    async fn update_page(&self, id: &PageId, update: &PageUpdate) -> ApiResult<Page> {
        self.record(format!("update_page:{id}"));
        let mut state = self.state.lock().expect("mock poisoned");
        let page = state
            .pages
            .get_mut(id.as_str())
            .filter(|p| !p.deleted)
            .ok_or_else(|| ApiError::NotFound(format!("page {id}")))?;

        if update.version != page.summary.version + 1 {
            return Err(ApiError::Conflict(format!(
                "page {id}: expected version {}, got {}",
                page.summary.version + 1,
                update.version
            )));
        }
        page.summary.version = update.version;
        page.summary.title = update.title.clone();
        if let Some(body) = &update.body_storage {
            page.body = body.clone();
        }
        if let Some(parent) = &update.parent_id {
            page.summary.parent_id = Some(parent.clone());
        }
        if let Some(status) = update.status {
            page.summary.status = status;
        }
        page.bodies.insert(page.summary.version, page.body.clone());
        page.summary.updated_at = Some("2026-08-16T00:00:00Z".into());
        page.history.push(VersionInfo {
            number: page.summary.version,
            author: Some(self.user.display_name.clone()),
            when: Some("2026-08-16T00:00:00Z".into()),
            message: update.message.clone(),
        });
        Ok(Page { summary: page.summary.clone(), body_storage: page.body.clone() })
    }

    async fn delete_page(&self, id: &PageId) -> ApiResult<()> {
        self.record(format!("delete_page:{id}"));
        let mut state = self.state.lock().expect("mock poisoned");
        let page = state
            .pages
            .get_mut(id.as_str())
            .ok_or_else(|| ApiError::NotFound(format!("page {id}")))?;
        page.deleted = true;
        page.summary.status = PageStatus::Trashed;
        Ok(())
    }

    async fn move_page(&self, id: &PageId, new_parent: &PageId, _pos: Position) -> ApiResult<()> {
        self.record(format!("move_page:{id}"));
        let mut state = self.state.lock().expect("mock poisoned");
        let page = state
            .pages
            .get_mut(id.as_str())
            .ok_or_else(|| ApiError::NotFound(format!("page {id}")))?;
        page.summary.parent_id = Some(new_parent.clone());
        Ok(())
    }

    async fn get_labels(&self, id: &PageId) -> ApiResult<Vec<String>> {
        let state = self.state.lock().expect("mock poisoned");
        Ok(state.pages.get(id.as_str()).map(|p| p.summary.labels.clone()).unwrap_or_default())
    }

    async fn add_label(&self, id: &PageId, label: &str) -> ApiResult<()> {
        self.record(format!("add_label:{id}:{label}"));
        let mut state = self.state.lock().expect("mock poisoned");
        if let Some(page) = state.pages.get_mut(id.as_str()) {
            if !page.summary.labels.iter().any(|l| l == label) {
                page.summary.labels.push(label.to_string());
            }
        }
        Ok(())
    }

    async fn remove_label(&self, id: &PageId, label: &str) -> ApiResult<()> {
        self.record(format!("remove_label:{id}:{label}"));
        let mut state = self.state.lock().expect("mock poisoned");
        if let Some(page) = state.pages.get_mut(id.as_str()) {
            page.summary.labels.retain(|l| l != label);
        }
        Ok(())
    }

    async fn list_attachments(&self, id: &PageId) -> ApiResult<Vec<Attachment>> {
        let state = self.state.lock().expect("mock poisoned");
        let mut out: Vec<_> = state
            .attachments
            .values()
            .filter(|(a, _)| a.page_id == *id)
            .map(|(a, _)| a.clone())
            .collect();
        out.sort_by(|a, b| a.filename.cmp(&b.filename));
        Ok(out)
    }

    async fn download_attachment(&self, attachment: &Attachment, dest: &Path) -> ApiResult<u64> {
        let bytes = {
            let state = self.state.lock().expect("mock poisoned");
            state
                .attachments
                .get(attachment.id.as_str())
                .map(|(_, b)| b.clone())
                .ok_or_else(|| ApiError::NotFound(format!("attachment {}", attachment.id)))?
        };
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(dest, &bytes).await?;
        Ok(bytes.len() as u64)
    }

    async fn upload_attachment(
        &self,
        page: &PageId,
        file: &Path,
        existing: Option<&AttachmentId>,
    ) -> ApiResult<Attachment> {
        let bytes = tokio::fs::read(file).await?;
        let filename =
            file.file_name().and_then(|n| n.to_str()).unwrap_or("attachment").to_string();
        self.record(format!("upload_attachment:{page}:{filename}"));
        let id = existing.cloned().unwrap_or_else(|| AttachmentId::new(self.fresh_id()));
        let mut state = self.state.lock().expect("mock poisoned");
        let version = state.attachments.get(id.as_str()).map(|(a, _)| a.version + 1).unwrap_or(1);
        let attachment = Attachment {
            id: id.clone(),
            page_id: page.clone(),
            filename: filename.clone(),
            media_type: Some("application/octet-stream".into()),
            file_size: Some(bytes.len() as u64),
            version,
            download_url: format!("/download/attachments/{page}/{filename}"),
        };
        state.attachments.insert(id.0.clone(), (attachment.clone(), bytes));
        Ok(attachment)
    }

    async fn delete_attachment(&self, id: &AttachmentId) -> ApiResult<()> {
        self.record(format!("delete_attachment:{id}"));
        self.state.lock().expect("mock poisoned").attachments.remove(id.as_str());
        Ok(())
    }

    async fn list_comments(&self, page: &PageId) -> ApiResult<Vec<Comment>> {
        let state = self.state.lock().expect("mock poisoned");
        Ok(state.comments.iter().filter(|c| c.page_id == *page).cloned().collect())
    }

    async fn add_footer_comment(
        &self,
        page: &PageId,
        body_storage: &str,
        reply_to: Option<&CommentId>,
    ) -> ApiResult<Comment> {
        self.record(format!("add_footer_comment:{page}"));
        let comment = Comment {
            id: CommentId::new(self.fresh_id()),
            page_id: page.clone(),
            parent_comment_id: reply_to.cloned(),
            kind: CommentKind::Footer,
            author: Some(self.user.display_name.clone()),
            created_at: Some("2026-08-16T00:00:00Z".into()),
            body_storage: body_storage.to_string(),
            resolved: false,
            anchor: None,
        };
        self.state.lock().expect("mock poisoned").comments.push(comment.clone());
        Ok(comment)
    }

    async fn add_inline_comment(
        &self,
        page: &PageId,
        anchor: &InlineAnchor,
        body_storage: &str,
    ) -> ApiResult<Comment> {
        if !self.capabilities.inline_comment_create {
            return Err(ApiError::unsupported(self.capabilities.flavor, "create inline comment"));
        }
        self.record(format!("add_inline_comment:{page}"));
        let comment = Comment {
            id: CommentId::new(self.fresh_id()),
            page_id: page.clone(),
            parent_comment_id: None,
            kind: CommentKind::Inline,
            author: Some(self.user.display_name.clone()),
            created_at: Some("2026-08-16T00:00:00Z".into()),
            body_storage: body_storage.to_string(),
            resolved: false,
            anchor: Some(anchor.clone()),
        };
        self.state.lock().expect("mock poisoned").comments.push(comment.clone());
        Ok(comment)
    }

    async fn resolve_comment(&self, id: &CommentId) -> ApiResult<()> {
        if !self.capabilities.comment_resolve {
            return Err(ApiError::unsupported(self.capabilities.flavor, "resolve comment"));
        }
        self.record(format!("resolve_comment:{id}"));
        let mut state = self.state.lock().expect("mock poisoned");
        if let Some(c) = state.comments.iter_mut().find(|c| c.id == *id) {
            c.resolved = true;
        }
        Ok(())
    }

    async fn search_cql(&self, cql: &str, limit: usize) -> ApiResult<Vec<SearchResult>> {
        let needle = cql.to_ascii_lowercase();
        let state = self.state.lock().expect("mock poisoned");
        Ok(state
            .pages
            .values()
            .filter(|p| !p.deleted)
            .filter(|p| needle.is_empty() || needle.contains(&p.summary.title.to_ascii_lowercase()))
            .take(limit)
            .map(|p| SearchResult {
                page_id: p.summary.id.clone(),
                title: p.summary.title.clone(),
                space_key: Some(p.summary.space_key.clone()),
                url: format!("{}/pages/{}", self.base_url, p.summary.id),
                excerpt: None,
            })
            .collect())
    }

    async fn get_page_versions(&self, id: &PageId, limit: usize) -> ApiResult<Vec<VersionInfo>> {
        let state = self.state.lock().expect("mock poisoned");
        let page =
            state.pages.get(id.as_str()).ok_or_else(|| ApiError::NotFound(format!("page {id}")))?;
        Ok(page.history.iter().rev().take(limit).cloned().collect())
    }

    async fn get_page_at_version(&self, id: &PageId, version: u32) -> ApiResult<Page> {
        let state = self.state.lock().expect("mock poisoned");
        let page =
            state.pages.get(id.as_str()).ok_or_else(|| ApiError::NotFound(format!("page {id}")))?;
        let body = page
            .bodies
            .get(&version)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(format!("page {id} version {version}")))?;
        let mut summary = page.summary.clone();
        summary.version = version;
        Ok(Page { summary, body_storage: body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_updates_are_rejected() {
        let mock = MockClient::new(Flavor::Cloud);
        let id = mock.seed_page("1001", "Page", None, "<p>hi</p>");
        mock.remote_edit("1001", "<p>changed</p>"); // now at version 2

        let stale = PageUpdate {
            title: "Page".into(),
            body_storage: Some("<p>mine</p>".into()),
            version: 2, // based on version 1
            parent_id: None,
            status: None,
            message: None,
        };
        let err = mock.update_page(&id, &stale).await.unwrap_err();
        assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");

        let fresh = PageUpdate { version: 3, ..stale };
        assert_eq!(mock.update_page(&id, &fresh).await.unwrap().summary.version, 3);
    }

    #[tokio::test]
    async fn data_center_refuses_inline_comment_creation() {
        let mock = MockClient::new(Flavor::DataCenter);
        let id = mock.seed_page("1001", "Page", None, "<p>hi</p>");
        let err = mock
            .add_inline_comment(&id, &InlineAnchor::default(), "<p>note</p>")
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Unsupported { .. }));
    }
}
