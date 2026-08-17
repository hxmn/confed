//! The sync engine: `fetch`, `pull`, `push`.
//!
//! Invariants that hold across all three:
//!
//! 1. The base (`pages` in `.state.db`) only ever advances to a state the server
//!    confirmed — a fetch result or a push response, never a local edit.
//! 2. Working files are written atomically (temp file + rename).
//! 3. `pull` never clobbers local edits it cannot merge; `push` never uploads a
//!    page whose base version is stale or whose merge is unresolved.

use crate::attachments;
use crate::comments;
use crate::error::{ConfedError, Result};
use crate::frontmatter::{Frontmatter, Managed, MarkdownFile};
use crate::merge::{self, RemoteLabel, ScalarMerge};
use crate::paths::{self, PagePlacement, Placement};
use crate::progress::{self, ProgressRef};
use crate::state::{
    hash_str, now, AttachmentRecord, CommentRecord, PageRecord, RemotePage, SyncState,
};
use crate::workspace::Workspace;
use crate::worktree::{self, LocalFile, PageState, PageStatus};
use confed_api::{
    BodyFormat, Comment, CommentKind, ConfluenceClient, NewPage, Page, PageId, PageUpdate, SpaceId,
};
use confed_convert::{BlockMap, ConvertOptions};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

pub struct SyncEngine {
    client: Arc<dyn ConfluenceClient>,
    space: SpaceId,
    concurrency: usize,
    progress: ProgressRef,
}

// ---------------------------------------------------------------- fetch ----

#[derive(Clone, Debug, Default)]
pub struct FetchOptions {
    /// Restrict to these page ids; empty means the whole space.
    pub pages: Vec<String>,
    /// Only consider pages modified at or after this timestamp.
    pub since: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FetchOutcome {
    pub fetched: usize,
    pub unchanged: usize,
    pub deleted_on_remote: Vec<String>,
    pub failed: Vec<FailedPage>,
    /// True when an interrupted fetch was continued rather than restarted.
    pub resumed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct FailedPage {
    pub page_id: String,
    pub title: String,
    pub error: String,
}

/// Everything fetched for one page, before it is written to the DB.
struct FetchedPage {
    page: Page,
    attachments: Vec<confed_api::Attachment>,
    comments: Vec<Comment>,
}

// ----------------------------------------------------------------- pull ----

#[derive(Clone, Debug, Default)]
pub struct PullOptions {
    /// Path globs / page ids limiting what is materialized.
    pub scope: Vec<String>,
    pub no_fetch: bool,
    /// Overwrite local changes instead of stopping.
    pub force: bool,
    /// Refuse to merge; treat every diverged page as a clobber.
    pub no_merge: bool,
    pub dry_run: bool,
    pub with_attachments: bool,
    pub with_comments: bool,
}

impl PullOptions {
    pub fn everything() -> Self {
        Self { with_attachments: true, with_comments: true, ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PullOutcome {
    pub created: Vec<PageChange>,
    pub updated: Vec<PageChange>,
    pub merged: Vec<PageChange>,
    pub conflicted: Vec<PageChange>,
    pub deleted: Vec<PageChange>,
    pub moved: Vec<MovedPage>,
    /// Pages left untouched because writing would have destroyed local work.
    pub skipped_dirty: Vec<BlockedPage>,
    pub attachments_downloaded: usize,
    pub dry_run: bool,
}

impl PullOutcome {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty()
            && self.updated.is_empty()
            && self.merged.is_empty()
            && self.conflicted.is_empty()
            && self.deleted.is_empty()
            && self.moved.is_empty()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PageChange {
    pub page_id: String,
    pub path: String,
    pub title: String,
    pub from_version: Option<u32>,
    pub to_version: Option<u32>,
    /// Which aspects changed: any of `body`, `title`, `labels`, `parent`,
    /// `delete`. Empty for pull-side changes, which always rewrite the file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MovedPage {
    pub page_id: String,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BlockedPage {
    pub page_id: String,
    pub path: String,
    pub reason: String,
}

// ----------------------------------------------------------------- push ----

#[derive(Clone, Debug, Default)]
pub struct PushOptions {
    pub scope: Vec<String>,
    pub dry_run: bool,
    /// Deletions are skipped unless this is set: an accidental `rm -rf` must not
    /// silently delete a Confluence subtree.
    pub allow_delete: bool,
    pub message: Option<String>,
    pub with_attachments: bool,
    pub with_comments: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PushOp {
    pub page_id: Option<String>,
    pub path: String,
    pub title: String,
    pub kind: PushKind,
    /// Which aspects change: any of `body`, `title`, `labels`, `parent`.
    pub ops: Vec<String>,
    pub base_version: Option<u32>,
    pub remote_version: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PushKind {
    Create,
    Update,
    Delete,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PushPlan {
    pub ops: Vec<PushOp>,
    pub skipped: Vec<BlockedPage>,
    pub attachment_ops: Vec<String>,
    pub comment_ops: Vec<String>,
}

impl PushPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.attachment_ops.is_empty() && self.comment_ops.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PushOutcome {
    pub pushed: Vec<PageChange>,
    pub created: Vec<PageChange>,
    pub deleted: Vec<PageChange>,
    pub attachments_uploaded: Vec<String>,
    pub comments_added: Vec<String>,
    pub skipped: Vec<BlockedPage>,
    pub failed: Vec<FailedPage>,
    pub dry_run: bool,
}

// ------------------------------------------------------------ the engine ----

impl SyncEngine {
    pub fn new(client: Arc<dyn ConfluenceClient>, space: SpaceId, concurrency: usize) -> Self {
        let concurrency = if concurrency == 0 {
            client.capabilities().max_request_concurrency
        } else {
            concurrency
        };
        Self { client, space, concurrency, progress: progress::none() }
    }

    /// Report what the engine is doing. Without this, progress is discarded.
    pub fn with_progress(mut self, progress: ProgressRef) -> Self {
        self.progress = progress;
        self
    }

    pub fn client(&self) -> &Arc<dyn ConfluenceClient> {
        &self.client
    }

    /// Refresh `.state.db`'s view of the server. Working files are untouched.
    pub async fn fetch(&self, ws: &mut Workspace, opts: &FetchOptions) -> Result<FetchOutcome> {
        let mut outcome = FetchOutcome::default();
        let state = ws.state();

        let pending = state.pending_fetches()?;
        outcome.resumed = !pending.is_empty();

        self.progress.stage("Listing pages", None);
        let summaries = self.client.list_pages(&self.space).await?;
        let live: Vec<String> = summaries.iter().map(|s| s.id.0.clone()).collect();

        for summary in &summaries {
            if !opts.pages.is_empty() && !opts.pages.contains(&summary.id.0) {
                continue;
            }
            if let Some(since) = &opts.since {
                if summary.updated_at.as_deref().is_some_and(|u| u < since.as_str()) {
                    continue;
                }
            }

            let existing = state.get_remote(&summary.id.0)?;
            let needs_body = match &existing {
                Some(r) => r.version != summary.version || r.storage_body.is_none(),
                None => true,
            };

            // Metadata is cheap and always current after a listing.
            state.upsert_remote(&RemotePage {
                page_id: summary.id.0.clone(),
                title: summary.title.clone(),
                parent_id: summary.parent_id.as_ref().map(|p| p.0.clone()),
                position: summary.position,
                version: summary.version,
                status: summary.status.as_str().to_string(),
                labels: summary.labels.clone(),
                author: summary.author.clone(),
                created_at: summary.created_at.clone(),
                updated_at: summary.updated_at.clone(),
                storage_body: None,
                storage_hash: None,
                fetched_at: now(),
                deleted: false,
            })?;

            if needs_body {
                state.enqueue_fetch(&summary.id.0, &["body", "attachments", "comments"])?;
            } else {
                outcome.unchanged += 1;
            }
        }

        // Pages that vanished from the listing were deleted or trashed.
        for remote in state.all_remote()? {
            if !remote.deleted && !live.contains(&remote.page_id) {
                state.mark_remote_deleted(&remote.page_id)?;
                outcome.deleted_on_remote.push(remote.page_id);
            }
        }

        let queue: Vec<String> = state.pending_fetches()?.into_iter().map(|(id, _)| id).collect();
        let titles: HashMap<&str, &str> =
            summaries.iter().map(|s| (s.id.0.as_str(), s.title.as_str())).collect();
        self.progress.stage("Fetching", Some(queue.len()));

        // Fetch concurrently, write serially: the SQLite connection is not shared
        // across tasks, and one writer keeps every page's write atomic.
        let mut inflight = FuturesUnordered::new();
        let mut queue_iter = queue.iter();
        for _ in 0..self.concurrency {
            if let Some(id) = queue_iter.next() {
                inflight.push(self.fetch_one(id.clone()));
            }
        }
        while let Some(result) = inflight.next().await {
            if let Some(id) = queue_iter.next() {
                inflight.push(self.fetch_one(id.clone()));
            }
            match result {
                Ok((id, fetched)) => {
                    self.progress.item(&fetched.page.summary.title);
                    self.store_fetched(ws, &id, &fetched)?;
                    outcome.fetched += 1;
                }
                Err((id, e)) => {
                    let title = titles.get(id.as_str()).copied().unwrap_or("").to_string();
                    self.progress.item(&title);
                    outcome.failed.push(FailedPage { page_id: id, title, error: e.to_string() });
                }
            }
        }

        self.resolve_mentioned_users(ws).await?;
        ws.state().set_meta("last_fetch_at", &now())?;
        self.progress.finish();
        Ok(outcome)
    }

    async fn fetch_one(
        &self,
        id: String,
    ) -> std::result::Result<(String, FetchedPage), (String, ConfedError)> {
        let page_id = PageId::new(&id);
        let result = async {
            let page = self.client.get_page(&page_id, BodyFormat::Storage).await?;
            let attachments = self.client.list_attachments(&page_id).await.unwrap_or_default();
            let comments = self.client.list_comments(&page_id).await.unwrap_or_default();
            Ok::<_, confed_api::ApiError>(FetchedPage { page, attachments, comments })
        }
        .await;

        match result {
            Ok(fetched) => Ok((id, fetched)),
            Err(e) => Err((id, e.into())),
        }
    }

    fn store_fetched(&self, ws: &mut Workspace, id: &str, fetched: &FetchedPage) -> Result<()> {
        let state = ws.state();
        let summary = &fetched.page.summary;
        state.upsert_remote(&RemotePage {
            page_id: id.to_string(),
            title: summary.title.clone(),
            parent_id: summary.parent_id.as_ref().map(|p| p.0.clone()),
            position: summary.position,
            version: summary.version,
            status: summary.status.as_str().to_string(),
            labels: summary.labels.clone(),
            author: summary.author.clone(),
            created_at: summary.created_at.clone(),
            updated_at: summary.updated_at.clone(),
            storage_body: Some(fetched.page.body_storage.clone()),
            storage_hash: Some(hash_str(&fetched.page.body_storage)),
            fetched_at: now(),
            deleted: false,
        })?;

        for attachment in &fetched.attachments {
            let existing = state
                .page_attachments(id)?
                .into_iter()
                .find(|a| a.attachment_id == attachment.id.0);
            state.upsert_attachment(&AttachmentRecord {
                attachment_id: attachment.id.0.clone(),
                page_id: id.to_string(),
                filename: attachment.filename.clone(),
                media_type: attachment.media_type.clone(),
                file_size: attachment.file_size,
                version: attachment.version,
                // A new server version invalidates the hash of what we downloaded.
                sha256: existing.filter(|e| e.version == attachment.version).and_then(|e| e.sha256),
                downloaded: false,
            })?;
        }

        state.clear_page_comments(id)?;
        for comment in &fetched.comments {
            state.upsert_comment(&CommentRecord {
                comment_id: comment.id.0.clone(),
                page_id: id.to_string(),
                parent_comment_id: comment.parent_comment_id.as_ref().map(|c| c.0.clone()),
                kind: match comment.kind {
                    CommentKind::Footer => "footer".into(),
                    CommentKind::Inline => "inline".into(),
                },
                author: comment.author.clone(),
                created_at: comment.created_at.clone(),
                body_storage: Some(comment.body_storage.clone()),
                body_markdown: confed_convert::storage_fragment_to_markdown(&comment.body_storage)
                    .unwrap_or_else(|_| comment.body_storage.clone()),
                resolved: comment.resolved,
                anchor: comment.anchor.as_ref().and_then(|a| serde_json::to_string(a).ok()),
                synced_at: Some(now()),
            })?;
        }

        state.mark_fetch_done(id)?;
        Ok(())
    }

    /// Look up everyone mentioned in the fetched bodies who is not cached yet.
    ///
    /// Mentions are opaque ids in the markup, so this is what lets them render
    /// as `[@Name](profile)`. A lookup that fails — a deleted account, or a
    /// permission confed does not have — is left unresolved rather than fatal:
    /// those blocks stay as Confluence markup, which is honest and lossless.
    async fn resolve_mentioned_users(&self, ws: &mut Workspace) -> Result<()> {
        let mut wanted: Vec<(String, String)> = Vec::new();
        for remote in ws.state().all_remote()? {
            let Some(body) = &remote.storage_body else { continue };
            for (attr, id) in confed_convert::user_references(body) {
                if !wanted.iter().any(|(_, existing)| existing == &id) {
                    wanted.push((attr, id));
                }
            }
        }

        for (attr, id) in wanted {
            if ws.state().get_user(&id)?.is_some() {
                continue;
            }
            let reference = match attr.as_str() {
                "account-id" => confed_api::UserReference::AccountId(id.clone()),
                "username" => confed_api::UserReference::Username(id.clone()),
                _ => confed_api::UserReference::UserKey(id.clone()),
            };
            match self.client.lookup_user(&reference).await {
                Ok(user) => {
                    let profile_url = self.client.user_profile_url(&user);
                    ws.state().upsert_user(&crate::state::UserRecord {
                        id,
                        id_attr: attr,
                        username: user.username.clone(),
                        display_name: user.display_name.clone(),
                        profile_url,
                        fetched_at: now(),
                    })?;
                }
                Err(e) => tracing::debug!(
                    target: "confed::sync",
                    user = %id, error = %e,
                    "could not resolve a mention; its block stays as Confluence markup"
                ),
            }
        }
        Ok(())
    }

    /// Materialize the fetched state into working files.
    pub async fn pull(&self, ws: &mut Workspace, opts: &PullOptions) -> Result<PullOutcome> {
        if !opts.no_fetch {
            let fetch = self.fetch(ws, &FetchOptions::default()).await?;
            if !fetch.failed.is_empty() {
                tracing::warn!(
                    target: "confed::sync",
                    failed = fetch.failed.len(),
                    "some pages could not be fetched"
                );
            }
        }

        let mut outcome = PullOutcome { dry_run: opts.dry_run, ..Default::default() };
        let (files, _) = worktree::read_working_files(ws)?;
        self.reconcile_paths(ws, &files)?;
        let base = ws.state().all_pages()?;
        let remote = ws.state().all_remote()?;
        let statuses = worktree::compute_status(&files, &base, &remote);

        let placements = self.plan_placements(&remote, &base);
        let links = link_map(&placements);
        let files_by_id: HashMap<&str, &LocalFile> =
            files.iter().filter_map(|f| f.file.frontmatter.page_id().map(|id| (id, f))).collect();
        let base_by_id: HashMap<&str, &PageRecord> =
            base.iter().map(|p| (p.page_id.as_str(), p)).collect();

        // Pass one: decide, and refuse the whole operation if anything would be
        // clobbered. Nothing is written until the plan is known to be safe.
        let known_users = convert_options(ws, "", &HashMap::new()).users;
        let mut plan: Vec<(RemotePage, PullAction)> = Vec::new();
        for remote_page in &remote {
            if !self.in_scope(&opts.scope, &placements, &remote_page.page_id) {
                continue;
            }
            let status =
                statuses.iter().find(|s| s.page_id.as_deref() == Some(&remote_page.page_id));
            let base_record = base_by_id.get(remote_page.page_id.as_str()).copied();
            // Was this file rendered from the same inputs confed would use now?
            let stale_rendering = match (base_record, &remote_page.storage_body) {
                (Some(base), Some(body)) => base.render_key != render_key(body, &known_users),
                _ => false,
            };
            let action = decide_pull(remote_page, status, base_record, opts, stale_rendering);
            if let PullAction::Blocked(reason) = &action {
                outcome.skipped_dirty.push(BlockedPage {
                    page_id: remote_page.page_id.clone(),
                    path: status.map(|s| s.path.clone()).unwrap_or_default(),
                    reason: reason.clone(),
                });
            }
            if !matches!(action, PullAction::Nothing) {
                plan.push((remote_page.clone(), action));
            }
        }

        if !outcome.skipped_dirty.is_empty() && !opts.force {
            return Err(ConfedError::state_with_hint(
                format!(
                    "{} page(s) have local changes that `pull` would overwrite",
                    outcome.skipped_dirty.len()
                ),
                "review with `confed diff`, then re-run with --force to discard local edits \
                 (or push your changes first)",
            ));
        }

        // Pass two: apply.
        self.progress.stage(if opts.dry_run { "Checking" } else { "Writing" }, Some(plan.len()));
        let mut handled: Vec<String> = Vec::new();
        // (path a rename left behind, path the page moved to)
        let mut vacated: Vec<(String, String)> = Vec::new();
        for (remote_page, action) in plan {
            handled.push(remote_page.page_id.clone());
            let local = files_by_id.get(remote_page.page_id.as_str()).copied();
            let base_record = base_by_id.get(remote_page.page_id.as_str()).copied();

            // Deletions are handled first: a page that is gone on the server has
            // no placement, because placements only cover pages that still exist.
            if action == PullAction::Delete {
                if !opts.dry_run {
                    self.delete_local(ws, base_record)?;
                }
                let path = base_record.map(|b| b.local_path.clone()).unwrap_or_default();
                self.progress.item(&path);
                outcome.deleted.push(PageChange {
                    page_id: remote_page.page_id.clone(),
                    path,
                    title: remote_page.title.clone(),
                    from_version: base_record.map(|b| b.version),
                    to_version: None,
                    ops: Vec::new(),
                });
                continue;
            }

            let Some(placement) = placements.get(&remote_page.page_id) else { continue };

            match action {
                PullAction::Nothing | PullAction::Blocked(_) if !opts.force => continue,
                PullAction::Nothing | PullAction::Delete => continue,
                PullAction::Create | PullAction::Overwrite | PullAction::Blocked(_) => {
                    let change =
                        self.write_page(ws, &remote_page, placement, local, &links, opts, None)?;
                    if base_record.is_some() {
                        outcome.updated.push(change);
                    } else {
                        outcome.created.push(change);
                    }
                }
                PullAction::Merge => {
                    let (change, conflicted) = self.merge_page(
                        ws,
                        &remote_page,
                        placement,
                        local,
                        base_record,
                        &links,
                        opts,
                    )?;
                    if conflicted {
                        outcome.conflicted.push(change);
                    } else {
                        outcome.merged.push(change);
                    }
                }
            }

            // A page renamed on the server moves its file. The old path is not
            // removed yet: when two pages swap titles, one page's old path is
            // another page's new one.
            if let (Some(base_record), false) = (base_record, opts.dry_run) {
                if base_record.local_path != placement.path {
                    outcome.moved.push(MovedPage {
                        page_id: remote_page.page_id.clone(),
                        from: base_record.local_path.clone(),
                        to: placement.path.clone(),
                    });
                    vacated.push((base_record.local_path.clone(), placement.path.clone()));
                }
            }

            self.progress.item(&placement.path);

            if opts.with_attachments && !opts.dry_run {
                outcome.attachments_downloaded +=
                    self.download_attachments(ws, &remote_page.page_id, placement).await?;
            }
            if opts.with_comments && !opts.dry_run {
                self.write_comments_sidecar(
                    ws,
                    &remote_page.page_id,
                    &remote_page.title,
                    &placement.path,
                )?;
            }
        }

        // Now that every page has been written, remove the files left behind by
        // renames — but never one that another page has just claimed.
        let claimed: Vec<&str> = placements.values().map(|p| p.path.as_str()).collect();
        let claimed_sidecars: Vec<String> =
            placements.values().map(|p| p.sidecar.clone()).collect();

        for (old_path, new_path) in vacated {
            // The page's attachments and comments move with it — unless another
            // page now owns that sidecar, in which case it is not ours to move.
            let old_sidecar_rel = paths::sidecar_for(&old_path);
            if !claimed_sidecars.contains(&old_sidecar_rel) {
                move_sidecar(
                    &ws.absolute(&old_sidecar_rel),
                    &ws.absolute(&paths::sidecar_for(&new_path)),
                );
            }

            if claimed.contains(&old_path.as_str()) {
                continue;
            }
            let old = ws.absolute(&old_path);
            if old.exists() {
                let _ = std::fs::remove_file(&old);
            }
        }

        // Pages pull had nothing to write still need their sidecar looked after:
        // the markup copy may be missing (a workspace pulled by an older confed,
        // or a deleted file), and a page edited only locally is exactly when an
        // inline comment anchor moves.
        if !opts.dry_run {
            for record in ws.state().all_pages()? {
                if handled.contains(&record.page_id)
                    || !self.in_scope(&opts.scope, &placements, &record.page_id)
                {
                    continue;
                }
                let path = record.local_path.clone();
                self.ensure_storage_copy(ws, &path, &record.storage_body)?;
                if opts.with_comments {
                    self.write_comments_sidecar(ws, &record.page_id, &record.title, &path)?;
                }
            }
        }

        self.progress.finish();
        Ok(outcome)
    }

    /// Learn where files actually are.
    ///
    /// A page is identified by `page_id`, so a user is free to rename or move
    /// its file. Recording that choice here is what lets a later server-side
    /// rename leave their filename alone while still updating the title.
    fn reconcile_paths(&self, ws: &mut Workspace, files: &[LocalFile]) -> Result<()> {
        for local in files {
            let Some(page_id) = local.file.frontmatter.page_id() else { continue };
            let Some(record) = ws.state().get_page(page_id)? else { continue };
            if record.local_path == local.path {
                continue;
            }
            let slug = local
                .path
                .rsplit('/')
                .next()
                .and_then(|f| f.strip_suffix(".md"))
                .unwrap_or(&local.path)
                .to_string();

            tracing::debug!(
                target: "confed::sync",
                page = page_id, from = %record.local_path, to = %local.path,
                "page file was moved locally"
            );
            let mut updated = record;
            updated.local_path = local.path.clone();
            updated.slug = slug;
            ws.state().upsert_page(&updated)?;
        }
        Ok(())
    }

    fn plan_placements(
        &self,
        remote: &[RemotePage],
        base: &[PageRecord],
    ) -> HashMap<String, Placement> {
        let pages: Vec<PagePlacement> = remote
            .iter()
            .filter(|r| !r.deleted)
            .map(|r| PagePlacement {
                page_id: r.page_id.clone(),
                title: r.title.clone(),
                parent_id: r.parent_id.clone(),
                position: r.position,
            })
            .collect();
        // Pin a filename only when the user chose it. If the slug on disk is
        // still the one confed derived from the title it last synced, a rename
        // on the server should move the file; if the user renamed it themselves,
        // that choice wins and only the title changes.
        let existing: HashMap<String, String> = base
            .iter()
            .filter(|b| b.slug != crate::slug::slugify(&b.title))
            .map(|b| (b.page_id.clone(), b.slug.clone()))
            .collect();
        paths::plan_paths(&pages, &existing)
    }

    fn in_scope(
        &self,
        scope: &[String],
        placements: &HashMap<String, Placement>,
        page_id: &str,
    ) -> bool {
        if scope.is_empty() {
            return true;
        }
        if scope.iter().any(|s| s == page_id) {
            return true;
        }
        let Some(placement) = placements.get(page_id) else { return false };
        scope.iter().any(|pattern| path_matches(pattern, &placement.path))
    }

    /// Write a page file from the remote body, replacing whatever is there.
    #[allow(clippy::too_many_arguments)]
    fn write_page(
        &self,
        ws: &mut Workspace,
        remote: &RemotePage,
        placement: &Placement,
        local: Option<&LocalFile>,
        links: &HashMap<String, String>,
        opts: &PullOptions,
        body_override: Option<String>,
    ) -> Result<PageChange> {
        let storage = remote.storage_body.clone().unwrap_or_default();
        let convert_opts = convert_options(ws, &placement.path, links);
        let converted = confed_convert::storage_to_markdown(&storage, &convert_opts)?;

        let body = body_override.unwrap_or(converted.markdown);
        let file = self.build_file(ws, remote, placement, local, body)?;
        let rendered = file.render()?;

        if !opts.dry_run {
            write_atomic(&ws.absolute(&placement.path), &rendered)?;
            self.record_base(
                ws,
                remote,
                placement,
                &file,
                &storage,
                &converted.block_map,
                SyncState::Clean,
            )?;
        }

        Ok(PageChange {
            page_id: remote.page_id.clone(),
            path: placement.path.clone(),
            title: remote.title.clone(),
            from_version: ws.state().get_page(&remote.page_id)?.map(|p| p.version),
            to_version: Some(remote.version),
            ops: Vec::new(),
        })
    }

    fn build_file(
        &self,
        ws: &Workspace,
        remote: &RemotePage,
        placement: &Placement,
        local: Option<&LocalFile>,
        body: String,
    ) -> Result<MarkdownFile> {
        let attachments = attachments::to_refs(&ws.state().page_attachments(&remote.page_id)?);
        let managed = Managed {
            schema: crate::frontmatter::SCHEMA_VERSION,
            page_id: remote.page_id.clone(),
            space_key: ws.space_key().unwrap_or_default(),
            version: remote.version,
            status: remote.status.clone(),
            position: remote.position,
            created: remote.created_at.clone(),
            updated: remote.updated_at.clone(),
            author: remote.author.clone(),
            attachments,
        };
        let _ = placement;
        Ok(MarkdownFile::new(
            Frontmatter {
                title: remote.title.clone(),
                labels: remote.labels.clone(),
                parent_id: remote.parent_id.clone(),
                managed: Some(managed),
                // Keep any user-added frontmatter keys the file already had.
                extra: local.map(|l| l.file.frontmatter.extra.clone()).unwrap_or_default(),
            },
            body,
        ))
    }

    /// Keep the page's Confluence markup next to its Markdown, exactly as the
    /// server has it. It is what the Markdown was rendered from and what a push
    /// patches, so it is the thing to read when a conversion looks wrong.
    fn write_storage_copy(&self, ws: &Workspace, page_path: &str, storage: &str) -> Result<()> {
        write_atomic(&ws.absolute(&paths::storage_file_for(page_path)), storage)
    }

    /// Write the markup copy only when it is missing or out of date.
    ///
    /// This is what backfills a workspace that was pulled before confed kept the
    /// copy, and what restores one somebody deleted — without rewriting every
    /// page's file, and its mtime, on every pull.
    fn ensure_storage_copy(&self, ws: &Workspace, page_path: &str, storage: &str) -> Result<()> {
        let path = ws.absolute(&paths::storage_file_for(page_path));
        if std::fs::read_to_string(&path).is_ok_and(|existing| existing == storage) {
            return Ok(());
        }
        self.write_storage_copy(ws, page_path, storage)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_base(
        &self,
        ws: &mut Workspace,
        remote: &RemotePage,
        placement: &Placement,
        file: &MarkdownFile,
        storage: &str,
        block_map: &BlockMap,
        sync_state: SyncState,
    ) -> Result<()> {
        self.write_storage_copy(ws, &placement.path, storage)?;
        ws.state().upsert_page(&PageRecord {
            page_id: remote.page_id.clone(),
            title: remote.title.clone(),
            slug: placement.slug.clone(),
            local_path: placement.path.clone(),
            parent_id: remote.parent_id.clone(),
            position: remote.position,
            version: remote.version,
            status: remote.status.clone(),
            labels: remote.labels.clone(),
            author: remote.author.clone(),
            created_at: remote.created_at.clone(),
            updated_at: remote.updated_at.clone(),
            storage_body: storage.to_string(),
            storage_hash: hash_str(storage),
            markdown_hash: file.content_hash(),
            block_map: serde_json::to_string(block_map).ok(),
            sync_state,
            synced_at: now(),
            render_key: render_key(
                storage,
                &convert_options(ws, &placement.path, &HashMap::new()).users,
            ),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_page(
        &self,
        ws: &mut Workspace,
        remote: &RemotePage,
        placement: &Placement,
        local: Option<&LocalFile>,
        base_record: Option<&PageRecord>,
        links: &HashMap<String, String>,
        opts: &PullOptions,
    ) -> Result<(PageChange, bool)> {
        let (Some(local), Some(base_record)) = (local, base_record) else {
            let change = self.write_page(ws, remote, placement, local, links, opts, None)?;
            return Ok((change, false));
        };

        let convert_opts = convert_options(ws, &placement.path, links);
        let base_md =
            confed_convert::storage_to_markdown(&base_record.storage_body, &convert_opts)?;
        let remote_storage = remote.storage_body.clone().unwrap_or_default();
        let remote_md = confed_convert::storage_to_markdown(&remote_storage, &convert_opts)?;

        let label = RemoteLabel {
            version: Some(remote.version),
            author: remote.author.clone(),
            when: remote.updated_at.clone(),
        };
        let merged =
            merge::merge_bodies(&base_md.markdown, &local.file.body, &remote_md.markdown, &label);
        let conflicted = merged.is_conflicted();

        // Frontmatter merges field by field.
        let labels = merge::merge_labels(
            &base_record.labels,
            &local.file.frontmatter.labels,
            &remote.labels,
        );
        let title = match merge::merge_scalar(
            &base_record.title,
            &local.file.frontmatter.title,
            &remote.title,
        ) {
            ScalarMerge::Value(v) => v,
            ScalarMerge::Conflict { ours, theirs } => {
                tracing::warn!(
                    target: "confed::sync",
                    page = %remote.page_id, %ours, %theirs, "title changed on both sides"
                );
                ours
            }
        };

        let mut file =
            self.build_file(ws, remote, placement, Some(local), merged.text().to_string())?;
        file.frontmatter.title = title;
        file.frontmatter.labels = labels;
        let rendered = file.render()?;

        if !opts.dry_run {
            write_atomic(&ws.absolute(&placement.path), &rendered)?;
            let state = if conflicted { SyncState::Conflicted } else { SyncState::Clean };
            self.record_base(
                ws,
                remote,
                placement,
                &file,
                &remote_storage,
                &remote_md.block_map,
                state,
            )?;
            ws.state().log(
                "pull-merge",
                Some(&remote.page_id),
                Some(base_record.version),
                Some(remote.version),
                if conflicted { "conflict" } else { "merged" },
                None,
            )?;
        }

        Ok((
            PageChange {
                page_id: remote.page_id.clone(),
                path: placement.path.clone(),
                title: remote.title.clone(),
                from_version: Some(base_record.version),
                to_version: Some(remote.version),
                ops: Vec::new(),
            },
            conflicted,
        ))
    }

    fn delete_local(&self, ws: &mut Workspace, base: Option<&PageRecord>) -> Result<()> {
        let Some(base) = base else { return Ok(()) };
        let path = ws.absolute(&base.local_path);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| ConfedError::io(format!("removing {}", path.display()), e))?;
        }
        let sidecar = ws.absolute(&paths::sidecar_for(&base.local_path));
        if sidecar.is_dir() {
            let _ = std::fs::remove_dir_all(&sidecar);
        }
        let children = ws.absolute(&paths::children_dir(&base.local_path));
        if children.is_dir() && std::fs::read_dir(&children).map(|d| d.count()).unwrap_or(1) == 0 {
            let _ = std::fs::remove_dir(&children);
        }
        ws.state().delete_page(&base.page_id)?;
        Ok(())
    }

    async fn download_attachments(
        &self,
        ws: &mut Workspace,
        page_id: &str,
        placement: &Placement,
    ) -> Result<usize> {
        let records = ws.state().page_attachments(page_id)?;
        if records.is_empty() {
            return Ok(0);
        }
        let remote_attachments =
            self.client.list_attachments(&PageId::new(page_id)).await.unwrap_or_default();

        let dir = ws.absolute(&placement.sidecar);
        let mut count = 0;
        for record in records {
            let dest = dir.join(&record.filename);
            if record.downloaded && dest.exists() {
                continue;
            }
            let Some(attachment) =
                remote_attachments.iter().find(|a| a.id.0 == record.attachment_id)
            else {
                continue;
            };
            std::fs::create_dir_all(&dir)
                .map_err(|e| ConfedError::io(format!("creating {}", dir.display()), e))?;
            self.client.download_attachment(attachment, &dest).await?;
            let mut updated = record.clone();
            updated.downloaded = true;
            updated.sha256 = attachments::file_sha256(&dest).ok();
            updated.file_size = attachments::file_size(&dest);
            ws.state().upsert_attachment(&updated)?;
            count += 1;
        }
        Ok(count)
    }

    fn write_comments_sidecar(
        &self,
        ws: &mut Workspace,
        page_id: &str,
        title: &str,
        page_path: &str,
    ) -> Result<()> {
        self.reanchor_inline_comments(ws, page_id, page_path)?;
        let records = ws.state().page_comments(page_id)?;
        let dir = ws.absolute(&paths::sidecar_for(page_path));
        let path = dir.join(comments::COMMENTS_FILENAME);

        // Unpushed drafts survive a refresh.
        let drafts: Vec<comments::SidecarComment> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| comments::parse(&text).ok())
            .map(|s| s.comments.into_iter().filter(|c| c.is_draft()).collect())
            .unwrap_or_default();

        if records.is_empty() && drafts.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| ConfedError::io(format!("creating {}", dir.display()), e))?;
        write_atomic(&path, &comments::render(page_id, title, &records, &drafts))
    }

    /// Re-locate every inline comment's anchor in the page's current text.
    ///
    /// Anchors are stored rather than embedded in the body, so an edit can move
    /// or destroy the text a comment points at. Anything that cannot be located
    /// confidently is flagged `orphaned` instead of being attached to the wrong
    /// sentence — and never deleted, on either side.
    fn reanchor_inline_comments(
        &self,
        ws: &mut Workspace,
        page_id: &str,
        path: &str,
    ) -> Result<usize> {
        let records = ws.state().page_comments(page_id)?;
        if !records.iter().any(|c| c.kind == "inline") {
            return Ok(0);
        }
        let Ok(content) = std::fs::read_to_string(ws.absolute(path)) else { return Ok(0) };
        let body = crate::frontmatter::split(&content).map(|(_, b)| b).unwrap_or(&content);

        let mut orphaned = 0;
        for record in records.into_iter().filter(|c| c.kind == "inline") {
            let Some(stored) = record.anchor.as_deref() else { continue };
            let Ok(anchor) = serde_json::from_str::<confed_api::InlineAnchor>(stored) else {
                continue;
            };

            let result = crate::reanchor::reanchor(&anchor, body);
            if result.is_orphaned() {
                orphaned += 1;
                tracing::debug!(
                    target: "confed::sync",
                    page = page_id, comment = %record.comment_id, text = %anchor.text,
                    "inline comment anchor could not be located"
                );
            }
            if result.anchor != anchor {
                let mut updated = record;
                updated.anchor = serde_json::to_string(&result.anchor).ok();
                ws.state().upsert_comment(&updated)?;
            }
        }
        Ok(orphaned)
    }

    // ------------------------------------------------------------- push ----

    /// Work out what `push` would do, without touching the server.
    pub fn plan_push(&self, ws: &Workspace, opts: &PushOptions) -> Result<PushPlan> {
        let (files, _) = worktree::read_working_files(ws)?;
        let base = ws.state().all_pages()?;
        let remote = ws.state().all_remote()?;
        let statuses = worktree::compute_status(&files, &base, &remote);
        let remote_by_id: HashMap<&str, &RemotePage> =
            remote.iter().map(|r| (r.page_id.as_str(), r)).collect();

        let mut plan = PushPlan::default();
        for status in &statuses {
            if !opts.scope.is_empty()
                && !opts.scope.iter().any(|s| {
                    Some(s.as_str()) == status.page_id.as_deref() || path_matches(s, &status.path)
                })
            {
                continue;
            }
            // Checked before `has_local_work`, which excludes conflicted pages:
            // a conflict must be reported to the user, not silently dropped.
            if status.state == PageState::Conflicted {
                plan.skipped.push(BlockedPage {
                    page_id: status.page_id.clone().unwrap_or_default(),
                    path: status.path.clone(),
                    reason: "unresolved merge conflict; run `confed resolve` first".into(),
                });
                continue;
            }

            if !status.state.has_local_work() {
                continue;
            }
            if !status.tampering.is_empty() {
                let fields: Vec<&str> = status.tampering.iter().map(|t| t.field).collect();
                plan.skipped.push(BlockedPage {
                    page_id: status.page_id.clone().unwrap_or_default(),
                    path: status.path.clone(),
                    reason: format!(
                        "tool-managed frontmatter was edited ({}); run `confed pull --force` on this page",
                        fields.join(", ")
                    ),
                });
                continue;
            }

            // Optimistic concurrency: refuse when the remote moved past our base.
            if let (Some(page_id), Some(base_version)) = (&status.page_id, status.base_version) {
                if let Some(remote) = remote_by_id.get(page_id.as_str()) {
                    if remote.version > base_version && !remote.deleted {
                        plan.skipped.push(BlockedPage {
                            page_id: page_id.clone(),
                            path: status.path.clone(),
                            reason: format!(
                                "remote is at version {} but this file is based on {}; run `confed pull`",
                                remote.version, base_version
                            ),
                        });
                        continue;
                    }
                }
            }

            match status.state {
                PageState::LocalNew => plan.ops.push(PushOp {
                    page_id: None,
                    path: status.path.clone(),
                    title: status.title.clone(),
                    kind: PushKind::Create,
                    ops: vec!["body".into()],
                    base_version: None,
                    remote_version: None,
                }),
                PageState::LocalDeleted => {
                    if opts.allow_delete {
                        plan.ops.push(PushOp {
                            page_id: status.page_id.clone(),
                            path: status.path.clone(),
                            title: status.title.clone(),
                            kind: PushKind::Delete,
                            ops: vec!["delete".into()],
                            base_version: status.base_version,
                            remote_version: status.remote_version,
                        });
                    } else {
                        plan.skipped.push(BlockedPage {
                            page_id: status.page_id.clone().unwrap_or_default(),
                            path: status.path.clone(),
                            reason:
                                "deleted locally; pass --allow-delete to delete it on the server"
                                    .into(),
                        });
                    }
                }
                _ => {
                    let mut ops = Vec::new();
                    if status.local_dirty {
                        ops.push("body".to_string());
                    }
                    if status.field_changes.title {
                        ops.push("title".to_string());
                    }
                    if status.field_changes.labels {
                        ops.push("labels".to_string());
                    }
                    if status.field_changes.parent {
                        ops.push("parent".to_string());
                    }
                    if ops.is_empty() {
                        continue;
                    }
                    plan.ops.push(PushOp {
                        page_id: status.page_id.clone(),
                        path: status.path.clone(),
                        title: status.title.clone(),
                        kind: PushKind::Update,
                        ops,
                        base_version: status.base_version,
                        remote_version: status.remote_version,
                    });
                }
            }
        }

        // Creates run parent-first (shallower paths sort first); deletes run
        // child-first so a parent is never removed out from under its children.
        plan.ops.sort_by_key(|op| {
            let depth = op.path.matches('/').count();
            match op.kind {
                PushKind::Create => (0, depth as i64),
                PushKind::Update => (1, depth as i64),
                PushKind::Delete => (2, -(depth as i64)),
            }
        });

        if opts.with_attachments {
            for status in &statuses {
                let Some(page_id) = &status.page_id else { continue };
                let sidecar = ws.absolute(&paths::sidecar_for(&status.path));
                let recorded = ws.state().page_attachments(page_id)?;
                for action in attachments::diff_attachments(&sidecar, &recorded)? {
                    if action.is_change() {
                        plan.attachment_ops.push(format!("{}: {:?}", status.path, action));
                    }
                }
            }
        }
        if opts.with_comments {
            for status in &statuses {
                let Some(page_id) = &status.page_id else { continue };
                let path = ws
                    .absolute(&paths::sidecar_for(&status.path))
                    .join(comments::COMMENTS_FILENAME);
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let sidecar = comments::parse(&text)?;
                for draft in sidecar.drafts() {
                    plan.comment_ops
                        .push(format!("{page_id}: add comment ({} chars)", draft.body.len()));
                }
                for id in &sidecar.resolve_requests {
                    plan.comment_ops.push(format!("{page_id}: resolve {id}"));
                }
            }
        }

        Ok(plan)
    }

    /// Upload local changes.
    pub async fn push(&self, ws: &mut Workspace, opts: &PushOptions) -> Result<PushOutcome> {
        let plan = self.plan_push(ws, opts)?;
        let mut outcome = PushOutcome {
            dry_run: opts.dry_run,
            skipped: plan.skipped.clone(),
            ..Default::default()
        };

        if opts.dry_run {
            for op in &plan.ops {
                let change = PageChange {
                    page_id: op.page_id.clone().unwrap_or_default(),
                    path: op.path.clone(),
                    title: op.title.clone(),
                    from_version: op.base_version,
                    to_version: op.base_version.map(|v| v + 1),
                    ops: op.ops.clone(),
                };
                match op.kind {
                    PushKind::Create => outcome.created.push(change),
                    PushKind::Update => outcome.pushed.push(change),
                    PushKind::Delete => outcome.deleted.push(change),
                }
            }
            return Ok(outcome);
        }

        // New pages created during this run, so children can find their parent.
        let mut created_ids: HashMap<String, String> = HashMap::new();
        self.progress.stage("Pushing", Some(plan.ops.len()));

        for op in &plan.ops {
            self.progress.item(&op.path);
            let result = self.apply_push_op(ws, op, opts, &mut created_ids).await;
            match result {
                Ok(Some(change)) => match op.kind {
                    PushKind::Create => outcome.created.push(change),
                    PushKind::Update => outcome.pushed.push(change),
                    PushKind::Delete => outcome.deleted.push(change),
                },
                Ok(None) => {}
                Err(e) => outcome.failed.push(FailedPage {
                    page_id: op.page_id.clone().unwrap_or_default(),
                    title: op.title.clone(),
                    error: e.to_string(),
                }),
            }
        }

        // Attachments after page bodies: a body that references a new file is
        // uploaded first, so the reference is never dangling for long.
        if opts.with_attachments {
            outcome.attachments_uploaded = self.push_attachments(ws, opts).await?;
        }
        if opts.with_comments {
            outcome.comments_added = self.push_comments(ws).await?;
        }

        self.progress.finish();
        Ok(outcome)
    }

    /// Upload new and changed attachments, and delete the ones removed locally.
    async fn push_attachments(
        &self,
        ws: &mut Workspace,
        opts: &PushOptions,
    ) -> Result<Vec<String>> {
        let mut uploaded = Vec::new();

        for record in ws.state().all_pages()? {
            if !opts.scope.is_empty()
                && !opts.scope.iter().any(|s| {
                    s == &record.page_id
                        || s == &record.local_path
                        || path_matches(s, &record.local_path)
                })
            {
                continue;
            }

            let sidecar_rel = paths::sidecar_for(&record.local_path);
            let sidecar = ws.absolute(&sidecar_rel);
            let recorded = ws.state().page_attachments(&record.page_id)?;
            let actions = attachments::diff_attachments(&sidecar, &recorded)?;
            let page_id = PageId::new(&record.page_id);

            for action in actions {
                match action {
                    attachments::AttachmentAction::Unchanged { .. } => {}
                    attachments::AttachmentAction::Upload { filename }
                    | attachments::AttachmentAction::Reupload { filename, .. } => {
                        let existing = recorded
                            .iter()
                            .find(|r| r.filename == filename)
                            .map(|r| confed_api::AttachmentId::new(&r.attachment_id));
                        let file = sidecar.join(&filename);
                        let attachment = self
                            .client
                            .upload_attachment(&page_id, &file, existing.as_ref())
                            .await?;

                        ws.state().upsert_attachment(&AttachmentRecord {
                            attachment_id: attachment.id.0.clone(),
                            page_id: record.page_id.clone(),
                            filename: attachment.filename.clone(),
                            media_type: attachment.media_type.clone(),
                            file_size: attachments::file_size(&file),
                            version: attachment.version,
                            sha256: attachments::file_sha256(&file).ok(),
                            downloaded: true,
                        })?;
                        uploaded.push(format!("{}/{}", sidecar_rel, filename));
                    }
                    // Deleting an attachment removes content from the server, so
                    // it follows the same explicit opt-in as deleting a page.
                    attachments::AttachmentAction::Delete { attachment_id, filename } => {
                        if !opts.allow_delete {
                            tracing::debug!(
                                target: "confed::sync",
                                page = %record.page_id, %filename,
                                "attachment is gone locally; pass --allow-delete to remove it"
                            );
                            continue;
                        }
                        self.client
                            .delete_attachment(&confed_api::AttachmentId::new(&attachment_id))
                            .await?;
                        ws.state().delete_attachment(&attachment_id)?;
                    }
                }
            }
        }
        Ok(uploaded)
    }

    async fn apply_push_op(
        &self,
        ws: &mut Workspace,
        op: &PushOp,
        opts: &PushOptions,
        created_ids: &mut HashMap<String, String>,
    ) -> Result<Option<PageChange>> {
        match op.kind {
            PushKind::Delete => {
                let Some(page_id) = &op.page_id else { return Ok(None) };
                self.client.delete_page(&PageId::new(page_id)).await?;
                ws.state().delete_page(page_id)?;
                ws.state().delete_remote(page_id)?;
                ws.state().log("push-delete", Some(page_id), op.base_version, None, "ok", None)?;
                Ok(Some(PageChange {
                    page_id: page_id.clone(),
                    path: op.path.clone(),
                    title: op.title.clone(),
                    from_version: op.base_version,
                    to_version: None,
                    ops: op.ops.clone(),
                }))
            }
            PushKind::Create => {
                let path = op.path.clone();
                let content = std::fs::read_to_string(ws.absolute(&path))
                    .map_err(|e| ConfedError::io(format!("reading {path}"), e))?;
                let file = crate::frontmatter::parse(&content, &path)?;

                let parent_id = self.resolve_parent(ws, &path, &file, created_ids)?;
                let convert_opts = convert_options(ws, &path, &HashMap::new());
                let storage = confed_convert::markdown_to_storage(&file.body, &convert_opts)?;

                let created = self
                    .client
                    .create_page(&NewPage {
                        space: self.space.clone(),
                        title: file.frontmatter.title.clone(),
                        parent_id: parent_id.as_deref().map(PageId::new),
                        body_storage: storage,
                        labels: file.frontmatter.labels.clone(),
                    })
                    .await?;

                created_ids.insert(path.clone(), created.summary.id.0.clone());
                self.commit_page(ws, &created, &path, &file)?;
                ws.state().log(
                    "push-create",
                    Some(&created.summary.id.0),
                    None,
                    Some(created.summary.version),
                    "ok",
                    None,
                )?;
                Ok(Some(PageChange {
                    page_id: created.summary.id.0.clone(),
                    path,
                    title: created.summary.title.clone(),
                    from_version: None,
                    to_version: Some(created.summary.version),
                    ops: op.ops.clone(),
                }))
            }
            PushKind::Update => {
                let Some(page_id) = op.page_id.clone() else { return Ok(None) };
                let path = op.path.clone();
                let content = std::fs::read_to_string(ws.absolute(&path))
                    .map_err(|e| ConfedError::io(format!("reading {path}"), e))?;
                let file = crate::frontmatter::parse(&content, &path)?;
                let base_record = ws
                    .state()
                    .get_page(&page_id)?
                    .ok_or_else(|| ConfedError::state(format!("{path}: no base record")))?;

                let convert_opts = convert_options(ws, &path, &HashMap::new());
                let body_storage = if op.ops.iter().any(|o| o == "body") {
                    Some(self.build_storage(&base_record, &file.body, &convert_opts)?)
                } else {
                    None
                };

                let updated = self
                    .client
                    .update_page(
                        &PageId::new(&page_id),
                        &PageUpdate {
                            title: file.frontmatter.title.clone(),
                            body_storage,
                            version: base_record.version + 1,
                            parent_id: file
                                .frontmatter
                                .parent_id
                                .as_deref()
                                .filter(|_| op.ops.iter().any(|o| o == "parent"))
                                .map(PageId::new),
                            status: None,
                            message: opts.message.clone(),
                        },
                    )
                    .await?;

                if op.ops.iter().any(|o| o == "labels") {
                    self.sync_labels(&page_id, &base_record.labels, &file.frontmatter.labels)
                        .await?;
                }

                self.commit_page(ws, &updated, &path, &file)?;
                ws.state().log(
                    "push-update",
                    Some(&page_id),
                    Some(base_record.version),
                    Some(updated.summary.version),
                    "ok",
                    Some(&op.ops.join(",")),
                )?;
                Ok(Some(PageChange {
                    page_id,
                    path,
                    title: updated.summary.title.clone(),
                    from_version: Some(base_record.version),
                    to_version: Some(updated.summary.version),
                    ops: op.ops.clone(),
                }))
            }
        }
    }

    /// Regenerate storage, patching only the blocks the user changed.
    fn build_storage(
        &self,
        base: &PageRecord,
        new_body: &str,
        opts: &ConvertOptions,
    ) -> Result<String> {
        let base_md = confed_convert::storage_to_markdown(&base.storage_body, opts)?;
        let block_map: BlockMap = base
            .block_map
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_else(|| base_md.block_map.clone());

        match confed_convert::markdown_to_storage_patched(
            &base.storage_body,
            &block_map,
            &base_md.markdown,
            new_body,
            opts,
        ) {
            Ok(storage) => Ok(storage),
            Err(confed_convert::ConvertError::StaleBlockMap(reason)) => {
                // Falling back means the whole document is regenerated, so blocks
                // the user never touched may be rewritten. Say so rather than
                // silently reformatting the page.
                tracing::warn!(
                    target: "confed::sync",
                    page = %base.page_id, %reason,
                    "block map unusable; regenerating the whole body"
                );
                Ok(confed_convert::markdown_to_storage(new_body, opts)?)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn resolve_parent(
        &self,
        ws: &Workspace,
        path: &str,
        file: &MarkdownFile,
        created_ids: &HashMap<String, String>,
    ) -> Result<Option<String>> {
        if let Some(parent) = &file.frontmatter.parent_id {
            return Ok(Some(parent.clone()));
        }
        // A page in `A/B/C.md` is a child of the page in `A/B.md`.
        let Some((dir, _)) = path.rsplit_once('/') else { return Ok(None) };
        let parent_path = format!("{dir}.md");

        if let Some(id) = created_ids.get(&parent_path) {
            return Ok(Some(id.clone()));
        }
        if let Some(record) = ws.state().get_page_by_path(&parent_path)? {
            return Ok(Some(record.page_id));
        }
        let parent_file = ws.absolute(&parent_path);
        if parent_file.exists() {
            let content = std::fs::read_to_string(&parent_file)
                .map_err(|e| ConfedError::io(format!("reading {parent_path}"), e))?;
            if let Ok(parsed) = crate::frontmatter::parse(&content, &parent_path) {
                return Ok(parsed.frontmatter.page_id().map(str::to_string));
            }
        }
        Ok(None)
    }

    async fn sync_labels(&self, page_id: &str, base: &[String], desired: &[String]) -> Result<()> {
        let id = PageId::new(page_id);
        for label in desired.iter().filter(|l| !base.contains(l)) {
            self.client.add_label(&id, label).await?;
        }
        for label in base.iter().filter(|l| !desired.contains(l)) {
            self.client.remove_label(&id, label).await?;
        }
        Ok(())
    }

    /// After a successful upload, advance the base and rewrite managed frontmatter.
    fn commit_page(
        &self,
        ws: &mut Workspace,
        page: &Page,
        path: &str,
        local: &MarkdownFile,
    ) -> Result<()> {
        let summary = &page.summary;
        let convert_opts = convert_options(ws, path, &HashMap::new());
        let converted = confed_convert::storage_to_markdown(&page.body_storage, &convert_opts)?;

        let mut file = local.clone();
        file.frontmatter.managed = Some(Managed {
            schema: crate::frontmatter::SCHEMA_VERSION,
            page_id: summary.id.0.clone(),
            space_key: summary.space_key.clone().max(ws.space_key().unwrap_or_default()),
            version: summary.version,
            status: summary.status.as_str().to_string(),
            position: summary.position,
            created: summary.created_at.clone(),
            updated: summary.updated_at.clone(),
            author: summary.author.clone(),
            attachments: attachments::to_refs(&ws.state().page_attachments(&summary.id.0)?),
        });
        file.frontmatter.title = summary.title.clone();

        write_atomic(&ws.absolute(path), &file.render()?)?;
        // The server's response is the new truth, including for the copy on disk.
        self.write_storage_copy(ws, path, &page.body_storage)?;

        let slug =
            path.rsplit('/').next().and_then(|f| f.strip_suffix(".md")).unwrap_or(path).to_string();

        ws.state().upsert_page(&PageRecord {
            page_id: summary.id.0.clone(),
            title: summary.title.clone(),
            slug,
            local_path: path.to_string(),
            parent_id: summary.parent_id.as_ref().map(|p| p.0.clone()),
            position: summary.position,
            version: summary.version,
            status: summary.status.as_str().to_string(),
            labels: file.frontmatter.labels.clone(),
            author: summary.author.clone(),
            created_at: summary.created_at.clone(),
            updated_at: summary.updated_at.clone(),
            storage_body: page.body_storage.clone(),
            storage_hash: hash_str(&page.body_storage),
            markdown_hash: file.content_hash(),
            block_map: serde_json::to_string(&converted.block_map).ok(),
            sync_state: SyncState::Clean,
            synced_at: now(),
            render_key: render_key(&page.body_storage, &convert_opts.users),
        })?;

        ws.state().upsert_remote(&RemotePage {
            page_id: summary.id.0.clone(),
            title: summary.title.clone(),
            parent_id: summary.parent_id.as_ref().map(|p| p.0.clone()),
            position: summary.position,
            version: summary.version,
            status: summary.status.as_str().to_string(),
            labels: file.frontmatter.labels.clone(),
            author: summary.author.clone(),
            created_at: summary.created_at.clone(),
            updated_at: summary.updated_at.clone(),
            storage_body: Some(page.body_storage.clone()),
            storage_hash: Some(hash_str(&page.body_storage)),
            fetched_at: now(),
            deleted: false,
        })
    }

    async fn push_comments(&self, ws: &mut Workspace) -> Result<Vec<String>> {
        let mut added = Vec::new();
        for record in ws.state().all_pages()? {
            let path = ws
                .absolute(&paths::sidecar_for(&record.local_path))
                .join(comments::COMMENTS_FILENAME);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let sidecar = comments::parse(&text)?;

            for draft in sidecar.comments.iter().filter(|c| c.is_draft()) {
                let storage =
                    confed_convert::markdown_to_storage(&draft.body, &ConvertOptions::default())?;
                let posted = match &draft.anchor {
                    Some(anchor) if draft.kind == comments::SidecarKind::Inline => {
                        if !self.client.capabilities().inline_comment_create {
                            return Err(ConfedError::Unsupported(format!(
                                "creating inline comments is not available on Confluence {}",
                                self.client.flavor()
                            )));
                        }
                        self.client
                            .add_inline_comment(&PageId::new(&record.page_id), anchor, &storage)
                            .await?
                    }
                    _ => {
                        self.client
                            .add_footer_comment(
                                &PageId::new(&record.page_id),
                                &storage,
                                draft.reply_to.as_deref().map(confed_api::CommentId::new).as_ref(),
                            )
                            .await?
                    }
                };
                added.push(posted.id.0.clone());
            }

            for id in &sidecar.resolve_requests {
                if !self.client.capabilities().comment_resolve {
                    return Err(ConfedError::Unsupported(format!(
                        "resolving comments is not available on Confluence {}",
                        self.client.flavor()
                    )));
                }
                self.client.resolve_comment(&confed_api::CommentId::new(id)).await?;
            }
        }
        Ok(added)
    }
}

/// What `pull` should do with one page.
#[derive(Clone, Debug, PartialEq)]
enum PullAction {
    Nothing,
    Create,
    /// Base is behind and there is nothing local to lose.
    Overwrite,
    Merge,
    Delete,
    /// Writing would destroy local work.
    Blocked(String),
}

fn decide_pull(
    remote: &RemotePage,
    status: Option<&PageStatus>,
    base: Option<&PageRecord>,
    opts: &PullOptions,
    stale_rendering: bool,
) -> PullAction {
    if remote.deleted {
        return match status.map(|s| s.local_dirty) {
            Some(true) if !opts.force => {
                PullAction::Blocked("deleted on the server but modified locally".to_string())
            }
            _ => PullAction::Delete,
        };
    }
    let Some(status) = status else {
        return if base.is_none() { PullAction::Create } else { PullAction::Overwrite };
    };

    // What the file was rendered from has changed — better conversion rules, or
    // a mention that can now be resolved — so the Markdown is out of date even
    // though neither side moved. Re-render it, but only when there is nothing
    // local to lose: otherwise it would show up as a change the user did not
    // make. The body is required, since re-rendering from a metadata-only
    // record would write an empty page over a perfectly good file.
    if stale_rendering && status.state == PageState::Unchanged && remote.storage_body.is_some() {
        return PullAction::Overwrite;
    }

    match status.state {
        // No local file exists, so there is nothing to lose.
        PageState::RemoteNew => PullAction::Create,
        // A file exists but confed has no base record for it — a rebuilt or
        // deleted `.state.db`, which is exactly what a fresh git clone looks
        // like, since `.state.db` is git-ignored. There is no base to compare
        // against, so confed cannot tell whether the file holds unpushed work.
        // Refuse rather than overwrite it.
        PageState::Untracked => {
            if opts.force {
                PullAction::Overwrite
            } else {
                PullAction::Blocked(
                    "this file names a page confed has no record of, so its contents cannot be \
                     compared with the server"
                        .into(),
                )
            }
        }
        PageState::Behind => PullAction::Overwrite,
        PageState::Diverged | PageState::Conflicted => {
            if opts.force {
                PullAction::Overwrite
            } else if opts.no_merge {
                PullAction::Blocked("local edits and remote edits (merging disabled)".into())
            } else {
                PullAction::Merge
            }
        }
        PageState::Modified => PullAction::Nothing,
        PageState::LocalDeleted => {
            if opts.force {
                PullAction::Create
            } else {
                PullAction::Nothing
            }
        }
        // Unchanged, RemoteDeleted (handled above), and LocalNew: nothing to write.
        _ => PullAction::Nothing,
    }
}

/// Conversion context for one page file.
fn convert_options(ws: &Workspace, path: &str, links: &HashMap<String, String>) -> ConvertOptions {
    let page_links: HashMap<String, String> =
        links.iter().map(|(id, target)| (id.clone(), paths::relative_link(path, target))).collect();
    let link_targets = page_links.iter().map(|(id, link)| (link.clone(), id.clone())).collect();

    // People this workspace has already resolved. A mention of anyone else keeps
    // its block verbatim rather than linking to the wrong profile.
    let users = ws
        .state()
        .all_users()
        .unwrap_or_default()
        .into_iter()
        .map(|u| {
            (
                u.id.clone(),
                confed_convert::UserLink {
                    display_name: u.display_name,
                    profile_url: u.profile_url,
                    id_attr: u.id_attr,
                    id_value: u.id,
                },
            )
        })
        .collect();

    ConvertOptions {
        attachment_dir: paths::sidecar_ref(path),
        page_links,
        link_targets,
        base_url: ws.base_url().ok().flatten().unwrap_or_default(),
        space_key: ws.space_key().unwrap_or_default(),
        users,
    }
}

fn link_map(placements: &HashMap<String, Placement>) -> HashMap<String, String> {
    placements.iter().map(|(id, p)| (id.clone(), p.path.clone())).collect()
}

/// Fingerprint of everything a page's Markdown was rendered from.
///
/// Two things make an already-synced file out of date without either side
/// changing: the converter's rules improve, or somebody the page mentions
/// becomes resolvable. Both belong in one value, so pull has a single question
/// to ask rather than a growing list of special cases.
pub fn render_key(storage: &str, users: &HashMap<String, confed_convert::UserLink>) -> String {
    let mut parts = vec![format!("converter={}", confed_convert::CONVERTER_VERSION)];
    let mut mentioned: Vec<String> = confed_convert::user_references(storage)
        .into_iter()
        .map(|(_, id)| match users.get(&id) {
            Some(user) => format!("{id}={}|{}", user.display_name, user.profile_url),
            None => format!("{id}=unresolved"),
        })
        .collect();
    mentioned.sort();
    parts.extend(mentioned);
    hash_str(&parts.join("\n"))
}

/// Glob-ish matching for scope arguments: `*` within a segment, `**` across them.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }
    match globset::Glob::new(pattern) {
        Ok(glob) => glob.compile_matcher().is_match(path),
        // A plain prefix like `Handbook` selects the subtree.
        Err(_) => path.starts_with(pattern),
    }
}

/// Move a page's sidecar to follow a rename.
///
/// The destination may already exist, because pull writes the new page's
/// comment sidecar before it gets here; anything not already at the destination
/// is carried over, then the old directory is removed.
fn move_sidecar(old: &Path, new: &Path) {
    if !old.is_dir() || old == new {
        return;
    }
    if !new.exists() && std::fs::rename(old, new).is_ok() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(old) else { return };
    let _ = std::fs::create_dir_all(new);
    for entry in entries.filter_map(std::result::Result::ok) {
        let destination = new.join(entry.file_name());
        if destination.exists() {
            // The destination copy is the fresh one — pull regenerates the
            // comment sidecar and re-downloads attachments under the new name —
            // so the old copy is redundant rather than lost.
            let _ = std::fs::remove_file(entry.path());
        } else {
            let _ = std::fs::rename(entry.path(), &destination);
        }
    }
    // Only removes the directory if it is now empty, so nothing is lost.
    let _ = std::fs::remove_dir(old);
}

/// Write via a temp file + rename so an interrupted write never truncates a page.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
    }
    let tmp = path.with_extension(format!(
        "{}.confed-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, content)
        .map_err(|e| ConfedError::io(format!("writing {}", tmp.display()), e))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| ConfedError::io(format!("renaming into {}", path.display()), e))?;
    Ok(())
}

/// Group a scan's pages by state, for reporting.
pub fn group_by_state(statuses: &[PageStatus]) -> BTreeMap<&'static str, Vec<&PageStatus>> {
    let mut out: BTreeMap<&'static str, Vec<&PageStatus>> = BTreeMap::new();
    for status in statuses {
        out.entry(status.state.as_str()).or_default().push(status);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_patterns_select_paths_and_subtrees() {
        assert!(path_matches("Handbook/Onboarding.md", "Handbook/Onboarding.md"));
        assert!(path_matches("Handbook/**", "Handbook/Onboarding.md"));
        assert!(path_matches("Handbook/**", "Handbook/Deep/Nested.md"));
        assert!(path_matches("*.md", "Root.md"));
        assert!(!path_matches("Other/**", "Handbook/Onboarding.md"));
    }

    #[test]
    fn atomic_writes_leave_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/Page.md");
        write_atomic(&path, "hello\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("confed-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn a_remote_deletion_with_local_edits_is_blocked_unless_forced() {
        let remote = RemotePage {
            page_id: "1".into(),
            title: "T".into(),
            parent_id: None,
            position: None,
            version: 3,
            status: "current".into(),
            labels: vec![],
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: None,
            storage_hash: None,
            fetched_at: now(),
            deleted: true,
        };
        let status = PageStatus {
            page_id: Some("1".into()),
            path: "T.md".into(),
            title: "T".into(),
            state: PageState::RemoteDeleted,
            base_version: Some(3),
            remote_version: Some(3),
            local_dirty: true,
            remote_ahead: false,
            field_changes: Default::default(),
            moved_from: None,
            tampering: vec![],
        };

        let blocked = decide_pull(&remote, Some(&status), None, &PullOptions::default(), false);
        assert!(matches!(blocked, PullAction::Blocked(_)));

        let forced = decide_pull(
            &remote,
            Some(&status),
            None,
            &PullOptions { force: true, ..Default::default() },
            false,
        );
        assert_eq!(forced, PullAction::Delete);
    }

    #[test]
    fn diverged_pages_merge_by_default_and_block_with_no_merge() {
        let remote = RemotePage {
            page_id: "1".into(),
            title: "T".into(),
            parent_id: None,
            position: None,
            version: 9,
            status: "current".into(),
            labels: vec![],
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: Some("<p>x</p>".into()),
            storage_hash: None,
            fetched_at: now(),
            deleted: false,
        };
        let status = PageStatus {
            page_id: Some("1".into()),
            path: "T.md".into(),
            title: "T".into(),
            state: PageState::Diverged,
            base_version: Some(7),
            remote_version: Some(9),
            local_dirty: true,
            remote_ahead: true,
            field_changes: Default::default(),
            moved_from: None,
            tampering: vec![],
        };

        assert_eq!(
            decide_pull(&remote, Some(&status), None, &PullOptions::default(), false),
            PullAction::Merge
        );
        assert!(matches!(
            decide_pull(
                &remote,
                Some(&status),
                None,
                &PullOptions { no_merge: true, ..Default::default() },
                false
            ),
            PullAction::Blocked(_)
        ));
        assert_eq!(
            decide_pull(
                &remote,
                Some(&status),
                None,
                &PullOptions { force: true, ..Default::default() },
                false
            ),
            PullAction::Overwrite
        );
    }

    #[test]
    fn a_locally_modified_page_is_left_alone_by_pull() {
        let remote = RemotePage {
            page_id: "1".into(),
            title: "T".into(),
            parent_id: None,
            position: None,
            version: 7,
            status: "current".into(),
            labels: vec![],
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: Some("<p>x</p>".into()),
            storage_hash: None,
            fetched_at: now(),
            deleted: false,
        };
        let status = PageStatus {
            page_id: Some("1".into()),
            path: "T.md".into(),
            title: "T".into(),
            state: PageState::Modified,
            base_version: Some(7),
            remote_version: Some(7),
            local_dirty: true,
            remote_ahead: false,
            field_changes: Default::default(),
            moved_from: None,
            tampering: vec![],
        };
        assert_eq!(
            decide_pull(&remote, Some(&status), None, &PullOptions::default(), false),
            PullAction::Nothing
        );
    }
}
