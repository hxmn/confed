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

    /// The archived spaces, which [`Self::list_spaces`] leaves out.
    async fn list_archived_spaces(&self) -> ApiResult<Vec<Space>> {
        Ok(Vec::new())
    }

    /// The space with this key, whatever its status — current, archived or
    /// personal — or `None` when there is none this account can see.
    async fn find_space(&self, key: &str) -> ApiResult<Option<Space>> {
        match self.get_space(key).await {
            Ok(space) => Ok(Some(space)),
            Err(crate::error::ApiError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Create a space, with its home page, and return it as the server now
    /// has it. A refusal confed can name is
    /// [`crate::error::ApiError::SpaceRefused`].
    async fn create_space(&self, new: &NewSpace) -> ApiResult<Space> {
        let _ = new;
        Err(crate::error::ApiError::unsupported(self.flavor(), "create a space"))
    }

    /// Whether this account may create spaces, asked without creating one:
    /// [`Probe::Available`] yes, [`Probe::Unavailable`] no, [`Probe::Unknown`]
    /// when the server would not say.
    async fn probe_create_space(&self) -> ApiResult<Probe> {
        Ok(Probe::Unknown)
    }

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

    /// Move a page, and with it every page under it, relative to `target`,
    /// which may be in another space: the page then belongs to the target's
    /// space. Ids, versions, history, comments, attachments and labels stay
    /// with the pages. This is the dedicated move endpoint
    /// (`PUT rest/api/content/{id}/move/{position}/{target}`), never a copy.
    async fn move_page_to(
        &self,
        id: &PageId,
        position: MovePosition,
        target: &PageId,
    ) -> ApiResult<()> {
        let _ = (id, position, target);
        Err(crate::error::ApiError::unsupported(self.flavor(), "move a page to another space"))
    }

    /// Whether the server can do [`Self::move_page_to`], asked without moving
    /// anything and without naming a real page in a write request. `page` is
    /// a real page that may be *viewed*, which is how a Data Center web
    /// session is opened.
    async fn probe_move(&self, page: &PageId) -> ApiResult<Probe> {
        let _ = page;
        Ok(Probe::Unknown)
    }

    /// The page titled exactly `title` in `space_key`, if there is one.
    async fn find_page_by_title(
        &self,
        space_key: &str,
        title: &str,
    ) -> ApiResult<Option<PageSummary>> {
        let _ = (space_key, title);
        Err(crate::error::ApiError::unsupported(self.flavor(), "look a page up by title"))
    }

    /// The operations (`read`, `update`) a page restricts to named people or
    /// groups. Empty when it has no restrictions of its own.
    async fn page_restrictions(&self, id: &PageId) -> ApiResult<Vec<String>> {
        let _ = id;
        Ok(Vec::new())
    }

    /// What the signed-in user may do with a page (`read`, `update`, `delete`,
    /// `create`…), or `None` when the server does not say.
    async fn page_operations(&self, id: &PageId) -> ApiResult<Option<Vec<String>>> {
        let _ = id;
        Ok(None)
    }

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
    /// Where in `space` an attachment was added, or given a new version, in
    /// the last `minutes` minutes.
    ///
    /// An attachment has a version of its own, like a comment: attaching a
    /// file to a page — or uploading one while writing a comment on it —
    /// leaves the page's version alone. This is how `fetch` finds the
    /// unchanged pages whose attachments it has to list again. A deleted
    /// attachment is in no search result, so a deletion is not found this way.
    async fn recent_attachment_activity(
        &self,
        space: &SpaceId,
        minutes: u64,
    ) -> ApiResult<ContentActivity> {
        let _ = (space, minutes);
        Err(crate::error::ApiError::unsupported(self.flavor(), "find recently changed attachments"))
    }

    async fn list_comments(&self, page: &PageId) -> ApiResult<Vec<Comment>>;
    /// Where in `space` a comment was added or edited in the last `minutes`
    /// minutes.
    ///
    /// A comment has a version of its own: adding or editing one leaves its
    /// page's version alone, so a page listing cannot show it. This is how
    /// `fetch` finds the unchanged pages whose comments it has to read again.
    /// A deleted comment is in no search result, so a deletion is not found
    /// this way.
    async fn recent_comment_activity(
        &self,
        space: &SpaceId,
        minutes: u64,
    ) -> ApiResult<CommentActivity> {
        let _ = (space, minutes);
        Err(crate::error::ApiError::unsupported(self.flavor(), "find recently changed comments"))
    }
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

    /// People whose name matches `query`, for writing mentions.
    async fn search_users(&self, query: &str, limit: usize) -> ApiResult<Vec<User>>;

    /// The server's product version (`9.5.4`), when it says. Used to warn before
    /// relying on an undocumented API on a release confed was not tested with.
    async fn server_version(&self) -> ApiResult<Option<String>> {
        Ok(None)
    }

    async fn search_cql(&self, cql: &str, limit: usize) -> ApiResult<Vec<SearchResult>>;

    async fn get_page_versions(&self, id: &PageId, limit: usize) -> ApiResult<Vec<VersionInfo>>;
    async fn get_page_at_version(&self, id: &PageId, version: u32) -> ApiResult<Page>;

    // ------------------------------------------------------ administration ---
    //
    // Users are named by username on Data Center and by account id on Cloud;
    // groups by name. Every one needs an administrator's credential.

    /// Create a user in the server's own directory.
    async fn create_user(&self, user: &NewUser) -> ApiResult<User> {
        let _ = user;
        Err(crate::error::ApiError::unsupported(self.flavor(), "create a user"))
    }

    async fn update_user(&self, username: &str, update: &UserUpdate) -> ApiResult<()> {
        let _ = (username, update);
        Err(crate::error::ApiError::unsupported(self.flavor(), "change a user's profile"))
    }

    /// Disable a user (they keep their content and can no longer sign in), or
    /// enable them again.
    async fn set_user_enabled(&self, username: &str, enabled: bool) -> ApiResult<()> {
        let _ = (username, enabled);
        Err(crate::error::ApiError::unsupported(self.flavor(), "disable or enable a user"))
    }

    async fn delete_user(&self, username: &str) -> ApiResult<()> {
        let _ = username;
        Err(crate::error::ApiError::unsupported(self.flavor(), "delete a user"))
    }

    /// The groups `user` belongs to.
    async fn user_groups(&self, user: &str) -> ApiResult<Vec<Group>> {
        let _ = user;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list a user's groups"))
    }

    async fn list_groups(&self, limit: Option<usize>) -> ApiResult<Vec<Group>> {
        let _ = limit;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list groups"))
    }

    async fn create_group(&self, name: &str) -> ApiResult<Group> {
        let _ = name;
        Err(crate::error::ApiError::unsupported(self.flavor(), "create a group"))
    }

    async fn delete_group(&self, name: &str) -> ApiResult<()> {
        let _ = name;
        Err(crate::error::ApiError::unsupported(self.flavor(), "delete a group"))
    }

    async fn group_members(&self, group: &str, limit: Option<usize>) -> ApiResult<Vec<User>> {
        let _ = (group, limit);
        Err(crate::error::ApiError::unsupported(self.flavor(), "list a group's members"))
    }

    async fn add_group_member(&self, group: &str, user: &str) -> ApiResult<()> {
        let _ = (group, user);
        Err(crate::error::ApiError::unsupported(self.flavor(), "add a user to a group"))
    }

    async fn remove_group_member(&self, group: &str, user: &str) -> ApiResult<()> {
        let _ = (group, user);
        Err(crate::error::ApiError::unsupported(self.flavor(), "remove a user from a group"))
    }

    /// Whether the site grants space access as permissions, as roles, or both.
    async fn space_role_mode(&self) -> ApiResult<RoleMode> {
        Ok(RoleMode::Permissions)
    }

    /// Every permission granted in the space.
    async fn space_permissions(&self, space_key: &str) -> ApiResult<Vec<SpacePermission>> {
        let _ = space_key;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list space permissions"))
    }

    /// Grant `operations` in the space, leaving the subject's others alone.
    async fn grant_space_permissions(
        &self,
        space_key: &str,
        subject: &Subject,
        operations: &[SpaceOperation],
    ) -> ApiResult<()> {
        let _ = (space_key, subject, operations);
        Err(crate::error::ApiError::unsupported(self.flavor(), "grant space permissions"))
    }

    /// Revoke `operations` in the space, leaving the subject's others alone.
    async fn revoke_space_permissions(
        &self,
        space_key: &str,
        subject: &Subject,
        operations: &[SpaceOperation],
    ) -> ApiResult<()> {
        let _ = (space_key, subject, operations);
        Err(crate::error::ApiError::unsupported(self.flavor(), "revoke space permissions"))
    }

    /// The roles a space can assign.
    async fn space_roles(&self, space_key: &str) -> ApiResult<Vec<SpaceRole>> {
        let _ = space_key;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list space roles"))
    }

    async fn space_role_assignments(&self, space_key: &str) -> ApiResult<Vec<RoleAssignment>> {
        let _ = space_key;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list space role assignments"))
    }

    /// Assign `role_id` to the subject in the space, or with `None` take the
    /// subject's role away.
    async fn set_space_role(
        &self,
        space_key: &str,
        subject: &Subject,
        role_id: Option<&str>,
    ) -> ApiResult<()> {
        let _ = (space_key, subject, role_id);
        Err(crate::error::ApiError::unsupported(self.flavor(), "assign a space role"))
    }
}
