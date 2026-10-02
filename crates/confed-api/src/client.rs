//! The flavor-independent client contract.
//!
//! Deviation from the design doc: list operations return `Vec<T>` with pagination
//! handled internally rather than `BoxStream`. Page summaries for even a 10k-page
//! space are a few MB, and the concrete type keeps mocking and error handling
//! simple. Bodies and attachments — the actually large payloads — are still
//! fetched one page at a time by the worker pool, and downloads stream to disk.

use crate::error::ApiResult;
use crate::types::*;
use async_trait::async_trait;
use std::path::Path;

#[async_trait]
pub trait ConfluenceClient: Send + Sync {
    fn capabilities(&self) -> &Capabilities;

    fn flavor(&self) -> Flavor {
        self.capabilities().flavor
    }

    /// Public base URL of the site, used to build browser links.
    fn base_url(&self) -> &str;

    /// Browser URL for a page (not an API URL).
    fn page_url(&self, page: &PageId, space_key: &str) -> String;

    async fn whoami(&self) -> ApiResult<User>;

    /// Resolve somebody mentioned in page content.
    ///
    /// Confluence writes mentions as an opaque id, so this is what turns one
    /// into a name and a profile link.
    async fn lookup_user(&self, reference: &UserReference) -> ApiResult<User>;

    /// Browser URL for a person's profile page, in whatever form this site uses.
    fn user_profile_url(&self, user: &User) -> String;

    async fn get_space(&self, key: &str) -> ApiResult<Space>;
    async fn list_spaces(&self, limit: Option<usize>) -> ApiResult<Vec<Space>>;

    /// Every page in the space, bodies excluded.
    async fn list_pages(&self, space: &SpaceId) -> ApiResult<Vec<PageSummary>>;

    async fn get_page(&self, id: &PageId, body: BodyFormat) -> ApiResult<Page>;
    async fn create_page(&self, new: &NewPage) -> ApiResult<Page>;
    /// `update.version` must be `base + 1`; a stale value must surface as
    /// [`crate::error::ApiError::Conflict`].
    async fn update_page(&self, id: &PageId, update: &PageUpdate) -> ApiResult<Page>;
    async fn delete_page(&self, id: &PageId) -> ApiResult<()>;
    async fn move_page(
        &self,
        id: &PageId,
        new_parent: &PageId,
        position: Position,
    ) -> ApiResult<()>;

    async fn get_labels(&self, id: &PageId) -> ApiResult<Vec<String>>;
    async fn add_label(&self, id: &PageId, label: &str) -> ApiResult<()>;
    async fn remove_label(&self, id: &PageId, label: &str) -> ApiResult<()>;

    async fn list_attachments(&self, id: &PageId) -> ApiResult<Vec<Attachment>>;
    async fn download_attachment(&self, attachment: &Attachment, dest: &Path) -> ApiResult<u64>;
    async fn upload_attachment(
        &self,
        page: &PageId,
        file: &Path,
        existing: Option<&AttachmentId>,
    ) -> ApiResult<Attachment>;
    async fn delete_attachment(&self, id: &AttachmentId) -> ApiResult<()>;

    async fn list_comments(&self, page: &PageId) -> ApiResult<Vec<Comment>>;
    async fn add_footer_comment(
        &self,
        page: &PageId,
        body_storage: &str,
        reply_to: Option<&CommentId>,
    ) -> ApiResult<Comment>;
    /// Anchor a comment to `anchor.text`, the `match_index`-th of `match_count`
    /// occurrences in the page's text as the server extracts it.
    async fn add_inline_comment(
        &self,
        page: &PageId,
        anchor: &InlineAnchor,
        body_storage: &str,
    ) -> ApiResult<Comment>;
    /// Reply to an inline thread. Data Center keeps inline replies in its
    /// inline-comment API; elsewhere a reply is a reply.
    async fn add_inline_reply(
        &self,
        page: &PageId,
        parent: &CommentId,
        body_storage: &str,
    ) -> ApiResult<Comment> {
        self.add_footer_comment(page, body_storage, Some(parent)).await
    }
    async fn resolve_comment(&self, id: &CommentId) -> ApiResult<()>;
    /// Replace a comment's body.
    async fn update_comment(
        &self,
        id: &CommentId,
        kind: CommentKind,
        body_storage: &str,
    ) -> ApiResult<()>;
    /// Delete a comment (and, as Confluence does it, its replies).
    async fn delete_comment(&self, id: &CommentId, kind: CommentKind) -> ApiResult<()>;

    /// The server's product version (`9.5.4`), when it says. Used to warn before
    /// relying on an undocumented API on a release confed was not tested with.
    async fn server_version(&self) -> ApiResult<Option<String>> {
        Ok(None)
    }

    async fn search_cql(&self, cql: &str, limit: usize) -> ApiResult<Vec<SearchResult>>;

    async fn get_page_versions(&self, id: &PageId, limit: usize) -> ApiResult<Vec<VersionInfo>>;
    async fn get_page_at_version(&self, id: &PageId, version: u32) -> ApiResult<Page>;
}
