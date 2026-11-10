//! Sending local changes: plan what differs from the base, apply it page by
//! page, and advance the base to what the server confirmed.

use super::*;

/// Prefix of the `meta` key that records a page create in flight, by path.
pub(super) const PENDING_CREATE: &str = "pending_create:";

/// What a push recorded before asking the server to create a page.
#[derive(Serialize, serde::Deserialize)]
pub(super) struct SentCreate {
    pub(super) title: String,
    pub(super) parent_id: Option<String>,
    /// [`MarkdownFile::content_hash`] of the file as it was sent.
    pub(super) content_hash: String,
    /// Hash of the storage body sent, laid out by `pretty::minify` so a
    /// server that re-spaces it still matches. Absent in records written by
    /// confed 0.18 and 0.19.
    #[serde(default)]
    pub(super) storage_hash: Option<String>,
    /// When the create was sent.
    #[serde(default)]
    pub(super) sent_at: Option<String>,
}

/// How far a server's clock may trail confed's when telling a page created
/// by the lost request from one that was there before it.
const CLOCK_SKEW_MINUTES: i64 = 10;

/// Whether `page`, found by title and parent where a lost create would
/// have put it, is that create's page and not one somebody else made. Its
/// body says so, or — for a server that rewrites bodies on save — it is
/// untouched since it was made, by this user, no earlier than the request.
fn is_the_lost_create(page: &Page, sent: &SentCreate, me: Option<&str>) -> bool {
    if sent.storage_hash.as_deref()
        == Some(hash_str(&confed_converter::pretty::minify(&page.body_storage)).as_str())
    {
        return true;
    }
    let summary = &page.summary;
    let made_after = match (&sent.sent_at, &summary.created_at) {
        (Some(sent), Some(created)) => {
            match (
                chrono::DateTime::parse_from_rfc3339(sent),
                chrono::DateTime::parse_from_rfc3339(created),
            ) {
                (Ok(sent), Ok(created)) => {
                    created >= sent - chrono::Duration::minutes(CLOCK_SKEW_MINUTES)
                }
                _ => false,
            }
        }
        _ => false,
    };
    summary.version == 1 && made_after && me.is_some() && summary.author.as_deref() == me
}

#[derive(Clone, Debug, Default)]
pub struct PushOptions {
    pub scope: Vec<String>,
    pub dry_run: bool,
    /// Deletions are skipped unless this is set: an accidental `rm -rf` must not
    /// silently delete a Confluence subtree.
    pub allow_delete: bool,
    /// Allow attachment deletions without allowing page deletions, so
    /// `confed attach --rm --push` cannot take the page down with the file.
    pub allow_attachment_delete: bool,
    pub message: Option<String>,
    pub with_attachments: bool,
    pub with_comments: bool,
    /// Only comment work — no page bodies, no attachments — and, with a
    /// scope, only for those pages. What the `comment … --push` shortcuts use.
    pub comments_only: bool,
    /// With `dry_run`: report the storage each page body and comment would be
    /// sent as, so a conversion can be checked before it reaches the server.
    pub show_storage: bool,
}

impl PushOptions {
    pub(super) fn deletes_attachments(&self) -> bool {
        self.allow_delete || self.allow_attachment_delete
    }

    /// Whether a page is in this push's scope (an empty scope is everything).
    pub(super) fn covers(&self, page_id: &str, path: &str) -> bool {
        self.scope.is_empty() || self.scope.iter().any(|s| s == page_id || path_matches(s, path))
    }
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

/// One attachment change a push would make.
#[derive(Clone, Debug, Serialize)]
pub struct AttachmentOp {
    pub page_id: String,
    /// Path of the page that owns it.
    pub path: String,
    /// Sidecar-relative path of the file, as reported by push.
    pub file: String,
    pub filename: String,
    /// The server's id, for everything but a first upload.
    pub attachment_id: Option<String>,
    pub kind: AttachmentOpKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentOpKind {
    Upload,
    Reupload,
    Delete,
}

/// Why an attachment deletion did not happen — said out loud, because a silent
/// no-op reads as a success.
pub(super) fn blocked_attachment(op: &AttachmentOp) -> BlockedPage {
    BlockedPage {
        page_id: op.page_id.clone(),
        path: op.file.clone(),
        reason: "attachment is gone locally; pass --allow-delete to remove it on the server".into(),
        kind: Blocked::NeedsAllowDelete,
    }
}

impl std::fmt::Display for AttachmentOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let verb = match self.kind {
            AttachmentOpKind::Upload => "upload",
            AttachmentOpKind::Reupload => "reupload",
            AttachmentOpKind::Delete => "delete",
        };
        write!(f, "{verb:<8} {}", self.file)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PushPlan {
    pub ops: Vec<PushOp>,
    pub skipped: Vec<BlockedPage>,
    pub attachment_ops: Vec<AttachmentOp>,
    pub comment_ops: Vec<String>,
    /// Comment work that cannot be sent: its page is gone from the server, as
    /// of the last fetch. The push would report the same under `failed`.
    pub comment_failures: Vec<FailedPage>,
}

impl PushPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.attachment_ops.is_empty() && self.comment_ops.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PushOutcome {
    /// Stopped by Ctrl-C: what is listed was done, and the rest is left for
    /// the next run. Only in the JSON when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    pub pushed: Vec<PageChange>,
    pub created: Vec<PageChange>,
    pub deleted: Vec<PageChange>,
    pub attachments_uploaded: Vec<String>,
    pub attachments_deleted: Vec<String>,
    /// New top-level comments, page and inline.
    pub comments_added: Vec<String>,
    /// Replies posted, to either kind of thread.
    #[serde(default)]
    pub replies_added: Vec<String>,
    /// Threads resolved.
    #[serde(default)]
    pub comments_resolved: Vec<String>,
    /// With `--dry-run --show-storage`: what each page body and comment would be
    /// sent as.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<StoragePreview>,
    /// Comment work a dry run found; a real push reports ids in `comments_added`,
    /// `replies_added` and `comments_resolved`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments_pending: Vec<String>,
    pub skipped: Vec<BlockedPage>,
    /// The pages among `skipped` whose `parent_id` names a page in another
    /// space: a move between spaces, which only `confed instance mv` makes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cross_space: Vec<BlockedPage>,
    pub failed: Vec<FailedPage>,
    pub dry_run: bool,
}

/// The storage a push would send for one page body or comment.
#[derive(Clone, Debug, Serialize)]
pub struct StoragePreview {
    pub page_id: String,
    pub path: String,
    /// `page`, `comment`, `inline comment on "…"` or `reply to <id>`.
    pub what: String,
    pub storage: String,
}

/// What a push did with comments.
#[derive(Default)]
pub(super) struct CommentWork {
    pub(super) added: Vec<String>,
    pub(super) replies: Vec<String>,
    pub(super) resolved: Vec<String>,
    pub(super) failed: Vec<FailedPage>,
}

// ------------------------------------------------------------ the engine ----

/// Refuse to post a body draft whose paragraph has unpushed edits: Confluence
/// checks the selection against its own copy, which does not have them.
pub(super) fn refuse_unpushed_block(
    ws: &Workspace,
    base: &PageRecord,
    body: &str,
    start: usize,
    end: usize,
    path: &str,
) -> Result<()> {
    let block_start = body[..start].rfind("\n\n").map_or(0, |i| i + 2);
    let block_end = body[end..].find("\n\n").map_or(body.len(), |i| end + i);
    let block = body[block_start..block_end].trim();
    let base_md = comparable_markdown(&base.storage_body, &page_convert_options(ws, path)?)?;
    if block.is_empty() || base_md.contains(block) {
        return Ok(());
    }
    let line = body[..start].matches('\n').count() + 1;
    Err(ConfedError::state_with_hint(
        format!("{path}: line {line}: the paragraph with the new comment has unpushed edits"),
        "push the page first (`confed push`), then the comment; Confluence checks the \
         selection against its own copy of the page",
    ))
}

impl SyncEngine {
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
                    kind: Blocked::Conflicted,
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
                    kind: Blocked::LocalWork,
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
                            kind: Blocked::Stale,
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
                            kind: Blocked::NeedsAllowDelete,
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
                // A page fetched but never written has no sidecar to scan.
                if status.path.is_empty() {
                    continue;
                }
                if !opts.scope.is_empty()
                    && !opts.scope.iter().any(|s| s == page_id || path_matches(s, &status.path))
                {
                    continue;
                }
                let sidecar_rel = paths::sidecar_for(&status.path);
                let sidecar = ws.absolute(&sidecar_rel);
                let recorded = ws.state().page_attachments(page_id)?;
                let removed = ws.state().removed_attachments(page_id)?;
                for action in attachments::diff_attachments(&sidecar, &recorded)? {
                    let (kind, attachment_id) = match &action {
                        attachments::AttachmentAction::Unchanged { .. } => continue,
                        // Not a new file: the copy of an attachment deleted on
                        // the server, which a pull has yet to take away.
                        // Uploading it would quietly undo the deletion.
                        attachments::AttachmentAction::Upload { filename }
                            if removed.iter().any(|r| &r.filename == filename) =>
                        {
                            plan.skipped.push(BlockedPage {
                                page_id: page_id.clone(),
                                path: format!("{sidecar_rel}/{filename}"),
                                reason: "the attachment was deleted on the server; `confed pull` \
                                         removes this copy, or keeps it if it has changed"
                                    .into(),
                                kind: Blocked::Other,
                            });
                            continue;
                        }
                        attachments::AttachmentAction::Upload { .. } => {
                            (AttachmentOpKind::Upload, None)
                        }
                        attachments::AttachmentAction::Reupload { attachment_id, .. } => {
                            (AttachmentOpKind::Reupload, Some(attachment_id.clone()))
                        }
                        attachments::AttachmentAction::Delete { attachment_id, .. } => {
                            (AttachmentOpKind::Delete, Some(attachment_id.clone()))
                        }
                    };
                    let filename = action.filename().to_string();
                    plan.attachment_ops.push(AttachmentOp {
                        page_id: page_id.clone(),
                        path: status.path.clone(),
                        file: format!("{sidecar_rel}/{filename}"),
                        filename,
                        attachment_id,
                        kind,
                    });
                }
            }
        }
        if opts.comments_only {
            plan.ops.clear();
            plan.attachment_ops.clear();
            plan.skipped.clear();
        }
        if opts.with_comments {
            let gone: Vec<&str> =
                remote.iter().filter(|r| r.deleted).map(|r| r.page_id.as_str()).collect();
            for status in &statuses {
                let Some(page_id) = &status.page_id else { continue };
                if !opts.covers(page_id, &status.path) {
                    continue;
                }
                if gone.contains(&page_id.as_str()) || status.state == PageState::RemoteDeleted {
                    if pending_comment_work(ws, &status.path) > 0 {
                        plan.comment_failures.push(FailedPage {
                            page_id: page_id.clone(),
                            title: status.title.clone(),
                            error: deleted_page_message(&status.path),
                        });
                    }
                    continue;
                }
                let path = ws
                    .absolute(&paths::sidecar_for(&status.path))
                    .join(comments::COMMENTS_FILENAME);
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let sidecar = comments::parse(&text)?;
                for draft in sidecar.drafts() {
                    let chars = draft.body.trim().chars().count();
                    plan.comment_ops.push(match (&draft.anchor, &draft.reply_to) {
                        (Some(anchor), _) if draft.kind == comments::SidecarKind::Inline => {
                            format!(
                                "{page_id}: add inline comment on \"{}\"{} (comments.md)",
                                anchor.text,
                                anchor
                                    .match_index
                                    .map(|i| format!(", occurrence {}", i + 1))
                                    .unwrap_or_default()
                            )
                        }
                        (_, Some(parent)) => {
                            format!("{page_id}: reply to {parent} ({chars} chars)")
                        }
                        _ => format!("{page_id}: add comment ({chars} chars)"),
                    });
                }
                for id in &sidecar.resolve_requests {
                    plan.comment_ops.push(format!("{page_id}: resolve {id}"));
                }
            }
            for local in &files {
                let Some(page_id) = local.file.frontmatter.page_id() else { continue };
                if !opts.covers(page_id, &local.path)
                    || plan.comment_failures.iter().any(|f| f.page_id == page_id)
                {
                    continue;
                }
                if local.file.drafts().next().is_some() {
                    validate_drafts(&local.file, &local.path)?;
                }
                for draft in local.file.drafts() {
                    let Some((start, end)) = draft_span(&local.file.body, draft) else { continue };
                    plan.comment_ops.push(format!(
                        "{page_id}: add inline comment on \"{}\" (line {})",
                        marks::plain_text(&local.file.body[start..end]).trim(),
                        draft.line
                    ));
                }
            }
        }

        Ok(plan)
    }

    /// Upload local changes.
    pub async fn push(&self, ws: &mut Workspace, opts: &PushOptions) -> Result<PushOutcome> {
        let mut plan = self.plan_push(ws, opts)?;
        let cross_space = self.refuse_other_space_parents(ws, &mut plan).await?;
        let mut outcome = PushOutcome {
            dry_run: opts.dry_run,
            skipped: plan.skipped.clone(),
            cross_space,
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
            // A dry run that hides attachment and comment work is worse than no
            // dry run: it reads as a promise that nothing else will happen.
            for op in &plan.attachment_ops {
                match op.kind {
                    AttachmentOpKind::Upload | AttachmentOpKind::Reupload => {
                        outcome.attachments_uploaded.push(op.file.clone());
                    }
                    AttachmentOpKind::Delete if opts.deletes_attachments() => {
                        outcome.attachments_deleted.push(op.file.clone());
                    }
                    AttachmentOpKind::Delete => outcome.skipped.push(blocked_attachment(op)),
                }
            }
            outcome.comments_pending = plan.comment_ops.clone();
            outcome.failed.extend(plan.comment_failures.clone());
            if opts.show_storage {
                outcome.storage = self.storage_previews(ws, opts, &plan)?;
            }
            return Ok(outcome);
        }

        // New pages created during this run, so children can find their parent.
        let mut created_ids: HashMap<String, String> = HashMap::new();
        self.progress.stage("Pushing", Some(plan.ops.len()));

        for op in &plan.ops {
            if crate::interrupt::requested() {
                outcome.interrupted = true;
                break;
            }
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

        if outcome.interrupted {
            // Pages already pushed are recorded; attachments and comments wait
            // for the next push, which plans them afresh.
            self.progress.finish();
            return Ok(outcome);
        }

        // Attachments after page bodies: a body that references a new file is
        // uploaded first, so the reference is never dangling for long.
        if opts.with_attachments {
            let uploaded =
                self.push_attachments(ws, opts, &plan.attachment_ops, &mut outcome).await?;
            self.relist_attachments(ws, &plan, &outcome, &uploaded).await?;
        }
        if opts.with_comments {
            let work = self.push_comments(ws, opts).await?;
            outcome.comments_added = work.added;
            outcome.replies_added = work.replies;
            outcome.comments_resolved = work.resolved;
            outcome.failed.extend(work.failed);
        }

        self.progress.finish();
        Ok(outcome)
    }

    /// Take out of the plan every page whose `parent_id` names a page in
    /// another space, and return them.
    ///
    /// The server would either refuse such a parent or move the page — and
    /// everything under it — out of this space behind confed's back, leaving
    /// this workspace tracking pages it no longer has and the other one
    /// unaware of them. Moving between spaces is `confed instance mv`'s job. A
    /// parent this workspace knows is in this space by definition, so only an
    /// unknown one costs a request.
    pub(super) async fn refuse_other_space_parents(
        &self,
        ws: &Workspace,
        plan: &mut PushPlan,
    ) -> Result<Vec<BlockedPage>> {
        let mut refused = Vec::new();
        let mut kept = Vec::new();
        for op in std::mem::take(&mut plan.ops) {
            let wants_parent = match op.kind {
                PushKind::Update => op.ops.iter().any(|o| o == "parent"),
                PushKind::Create => true,
                PushKind::Delete => false,
            };
            let parent = if wants_parent {
                std::fs::read_to_string(ws.absolute(&op.path))
                    .ok()
                    .and_then(|text| crate::frontmatter::parse(&text, &op.path).ok())
                    .and_then(|file| file.frontmatter.parent_id)
            } else {
                None
            };
            let Some(parent) = parent else {
                kept.push(op);
                continue;
            };
            let known = ws.state().get_page(&parent)?.is_some()
                || ws.state().get_remote(&parent)?.is_some_and(|r| !r.deleted);
            if known {
                kept.push(op);
                continue;
            }
            let elsewhere =
                match self.client.get_page(&PageId::new(&parent), BodyFormat::Storage).await {
                    Ok(page)
                        if !page.summary.space_key.is_empty()
                            && page.summary.space_key != self.space.key =>
                    {
                        Some(page.summary.space_key)
                    }
                    _ => None,
                };
            match elsewhere {
                Some(space) => {
                    let blocked = BlockedPage {
                        page_id: op.page_id.clone().unwrap_or_default(),
                        path: op.path.clone(),
                        reason: format!(
                            "parent_id {parent} is a page in space {space}, not {own}; a page \
                             moves to another space only with `confed instance mv \"{own}:{path}\" \
                             {parent}` from the instance root (a page id names the new parent)",
                            own = self.space.key,
                            path = op.path
                        ),
                        kind: Blocked::CrossSpace,
                    };
                    plan.skipped.push(blocked.clone());
                    refused.push(blocked);
                }
                None => kept.push(op),
            }
        }
        plan.ops = kept;
        Ok(refused)
    }

    /// Apply the attachment half of a plan: upload new and changed files, and
    /// delete the ones removed locally.
    ///
    /// This runs from the same plan `--dry-run` prints, so what a dry run
    /// promises and what a push does cannot drift apart. Returns the
    /// attachments as the server answered each upload.
    pub(super) async fn push_attachments(
        &self,
        ws: &mut Workspace,
        opts: &PushOptions,
        ops: &[AttachmentOp],
        outcome: &mut PushOutcome,
    ) -> Result<Vec<confed_api::Attachment>> {
        let mut uploaded = Vec::new();
        for op in ops {
            let page_id = PageId::new(&op.page_id);
            let sidecar = ws.absolute(&paths::sidecar_for(&op.path));
            let file = sidecar.join(&op.filename);

            match op.kind {
                AttachmentOpKind::Upload | AttachmentOpKind::Reupload => {
                    let existing = op.attachment_id.as_deref().map(confed_api::AttachmentId::new);
                    let attachment =
                        self.client.upload_attachment(&page_id, &file, existing.as_ref()).await?;

                    ws.state().upsert_attachment(&AttachmentRecord {
                        attachment_id: attachment.id.0.clone(),
                        page_id: op.page_id.clone(),
                        filename: attachment.filename.clone(),
                        media_type: attachment.media_type.clone(),
                        file_size: attachments::file_size(&file),
                        version: attachment.version,
                        sha256: attachments::file_sha256(&file).ok(),
                        downloaded: true,
                    })?;
                    outcome.attachments_uploaded.push(op.file.clone());
                    uploaded.push(attachment);
                }
                // Deleting an attachment removes content from the server, so it
                // follows the same explicit opt-in as deleting a page — and is
                // reported as skipped rather than dropped on the floor.
                AttachmentOpKind::Delete => {
                    let Some(attachment_id) = &op.attachment_id else { continue };
                    if !opts.deletes_attachments() {
                        outcome.skipped.push(blocked_attachment(op));
                        continue;
                    }
                    self.client
                        .delete_attachment(&confed_api::AttachmentId::new(attachment_id))
                        .await?;
                    ws.state().delete_attachment(attachment_id)?;
                    outcome.attachments_deleted.push(op.file.clone());
                }
            }
        }
        Ok(uploaded)
    }

    /// List the attachments of every page this push wrote to, and bring what
    /// each page file lists in line.
    ///
    /// A push is the one visit confed pays a page it believes is current: a
    /// file attached there by somebody else, or removed, changes nothing a
    /// later fetch would notice by the page's version — least of all on Data
    /// Center, where the push itself has just taken that version. New files
    /// are downloaded by the next pull; nothing in the sidecar is touched here.
    ///
    /// `uploaded` is what this push attached itself. A list that does not have
    /// one of those yet is behind, not saying it is gone.
    pub(super) async fn relist_attachments(
        &self,
        ws: &mut Workspace,
        plan: &PushPlan,
        outcome: &PushOutcome,
        uploaded: &[confed_api::Attachment],
    ) -> Result<()> {
        let mut pages: Vec<(&str, &str)> = Vec::new();
        let written = outcome.pushed.iter().chain(&outcome.created);
        let attached = plan
            .attachment_ops
            .iter()
            .filter(|op| {
                outcome.attachments_uploaded.contains(&op.file)
                    || outcome.attachments_deleted.contains(&op.file)
            })
            .map(|op| (op.page_id.as_str(), op.path.as_str()));
        for page in written.map(|c| (c.page_id.as_str(), c.path.as_str())).chain(attached) {
            if !pages.contains(&page) {
                pages.push(page);
            }
        }
        for (page_id, path) in pages {
            match self.client.list_attachments(&PageId::new(page_id)).await {
                Ok(mut listed) => {
                    for own in uploaded.iter().filter(|a| a.page_id.0 == page_id) {
                        if !listed.iter().any(|a| a.id == own.id) {
                            listed.push(own.clone());
                        }
                    }
                    self.refresh_attachments(ws, page_id, &listed)?;
                }
                // Not worth failing a push that went through: the list is
                // owed, and the next fetch reads it.
                Err(e) => {
                    tracing::debug!(
                        target: "confed::sync",
                        page = %page_id, error = %e,
                        "could not list the page's attachments after the push"
                    );
                    ws.state().enqueue_fetch_also(page_id, &[Extra::Attachments.name()])?;
                }
            }
            write_attachment_refs(ws, page_id, path)?;
        }
        Ok(())
    }

    pub(super) async fn apply_push_op(
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
                ws.state().atomically(|state| {
                    state.delete_page(page_id)?;
                    state.delete_remote(page_id)?;
                    state.log("push-delete", Some(page_id), op.base_version, None, "ok", None)
                })?;
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
                let journal = format!("{PENDING_CREATE}{path}");

                if let Some(found) =
                    self.find_lost_create(ws, &journal, parent_id.as_deref()).await?
                {
                    let (page, sent) = found;
                    created_ids.insert(path.clone(), page.summary.id.0.clone());
                    self.commit_page(ws, &page, &path, &file)?;
                    // The base is what was sent. Should the file have changed
                    // since, that change is still owed, and shows as one.
                    if let Some(mut record) = ws.state().get_page(&page.summary.id.0)? {
                        record.markdown_hash = sent.content_hash;
                        ws.state().upsert_page(&record)?;
                    }
                    ws.state().delete_meta(&journal)?;
                    ws.state().log(
                        "push-create",
                        Some(&page.summary.id.0),
                        None,
                        Some(page.summary.version),
                        "ok",
                        Some("found the page an interrupted push had created"),
                    )?;
                    return Ok(Some(PageChange {
                        page_id: page.summary.id.0.clone(),
                        path,
                        title: page.summary.title.clone(),
                        from_version: None,
                        to_version: Some(page.summary.version),
                        ops: op.ops.clone(),
                    }));
                }

                let convert_opts = page_convert_options(ws, &path)?;
                let storage = confed_converter::markdown_to_storage(&file.body, &convert_opts)?;

                // Written before the request goes out, removed once its id is
                // in the file: whatever happens between — a lost answer, a
                // kill — the next push knows a page may already exist.
                let sent = SentCreate {
                    title: file.frontmatter.title.clone(),
                    parent_id: parent_id.clone(),
                    content_hash: file.content_hash(),
                    storage_hash: Some(hash_str(&confed_converter::pretty::minify(&storage))),
                    sent_at: Some(now()),
                };
                let record = serde_json::to_string(&sent).map_err(|e| {
                    ConfedError::state(format!("recording the create of {path}: {e}"))
                })?;
                ws.state().set_meta(&journal, &record)?;

                let created = self
                    .client
                    .create_page(&NewPage {
                        space: self.space.clone(),
                        title: file.frontmatter.title.clone(),
                        parent_id: parent_id.as_deref().map(PageId::new),
                        body_storage: storage,
                        labels: file.frontmatter.labels.clone(),
                    })
                    .await;
                let created = match created {
                    Ok(created) => created,
                    // No answer: the page may exist. The record stays for the
                    // next push to look for it.
                    Err(
                        e @ (confed_api::ApiError::Network { .. } | confed_api::ApiError::Io(_)),
                    ) => return Err(e.into()),
                    Err(e) => {
                        ws.state().delete_meta(&journal)?;
                        return Err(e.into());
                    }
                };

                created_ids.insert(path.clone(), created.summary.id.0.clone());
                self.commit_page(ws, &created, &path, &file)?;
                ws.state().delete_meta(&journal)?;
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

                let convert_opts = page_convert_options(ws, &path)?;
                // The marked body: a regenerated block keeps its inline comment
                // markers, so an edit next to a thread does not orphan it.
                let body_storage = if op.ops.iter().any(|o| o == "body") {
                    Some(self.build_storage(&base_record, &file.marked_body(), &convert_opts)?)
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
    pub(super) fn build_storage(
        &self,
        base: &PageRecord,
        new_body: &str,
        opts: &ConvertOptions,
    ) -> Result<String> {
        let base_md = confed_converter::storage_to_markdown(&base.storage_body, opts)?;
        let block_map: BlockMap = base
            .block_map
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_else(|| base_md.block_map.clone());

        match confed_converter::markdown_to_storage_patched(
            &base.storage_body,
            &block_map,
            &base_md.markdown,
            new_body,
            opts,
        ) {
            Ok(storage) => Ok(storage),
            Err(confed_converter::ConvertError::StaleBlockMap(reason)) => {
                // Falling back means the whole document is regenerated, so blocks
                // the user never touched may be rewritten. Say so rather than
                // silently reformatting the page.
                tracing::warn!(
                    target: "confed::sync",
                    page = %base.page_id, %reason,
                    "block map unusable; regenerating the whole body"
                );
                Ok(confed_converter::markdown_to_storage(new_body, opts)?)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn resolve_parent(
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

    pub(super) async fn sync_labels(
        &self,
        page_id: &str,
        base: &[String],
        desired: &[String],
    ) -> Result<()> {
        let id = PageId::new(page_id);
        for label in desired.iter().filter(|l| !base.contains(l)) {
            self.client.add_label(&id, label).await?;
        }
        for label in base.iter().filter(|l| !desired.contains(l)) {
            self.client.remove_label(&id, label).await?;
        }
        Ok(())
    }

    /// The page a push created without learning so — its answer lost, or
    /// confed stopped before writing the id down — when `journal` says one
    /// was sent. Found by title where the push put it, and taken only when
    /// it is that page ([`is_the_lost_create`]): a page somebody else made
    /// with the same title is left alone, and the create is sent.
    pub(super) async fn find_lost_create(
        &self,
        ws: &Workspace,
        journal: &str,
        parent_id: Option<&str>,
    ) -> Result<Option<(Page, SentCreate)>> {
        let Some(record) = ws.state().get_meta(journal)? else { return Ok(None) };
        let Ok(sent) = serde_json::from_str::<SentCreate>(&record) else {
            ws.state().delete_meta(journal)?;
            return Ok(None);
        };
        let found = match self.client.find_page_by_title(&self.space.key, &sent.title).await {
            Ok(found) => found,
            // Nowhere to look: creating is all that is left, as before.
            Err(confed_api::ApiError::Unsupported { .. }) => None,
            Err(e) => return Err(e.into()),
        };
        let parent = parent_id.or(sent.parent_id.as_deref());
        let Some(summary) =
            found.filter(|p| p.parent_id.as_ref().map(|id| id.0.as_str()) == parent)
        else {
            ws.state().delete_meta(journal)?;
            return Ok(None);
        };
        let page = self.client.get_page(&summary.id, BodyFormat::Storage).await?;
        let me = self.client.whoami().await.ok().map(|u| u.display_name);
        if !is_the_lost_create(&page, &sent, me.as_deref()) {
            tracing::info!(
                target: "confed::sync",
                page = %summary.id, title = %summary.title,
                "a page with this title is there, but not the one the earlier push sent"
            );
            ws.state().delete_meta(journal)?;
            return Ok(None);
        }
        tracing::info!(
            target: "confed::sync",
            page = %summary.id, title = %summary.title,
            "an earlier push created this page without recording it; taking it up"
        );
        Ok(Some((page, sent)))
    }

    /// After a successful upload, advance the base and rewrite managed frontmatter.
    pub(super) fn commit_page(
        &self,
        ws: &mut Workspace,
        page: &Page,
        path: &str,
        local: &MarkdownFile,
    ) -> Result<()> {
        let summary = &page.summary;
        let convert_opts = page_convert_options(ws, path)?;

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

        // The id goes into the file first. Everything after it can fail and
        // be redone; a page the file does not name would be created again.
        write_atomic(&ws.absolute(path), &file.render()?)?;
        // The server's response is the new truth, including for the copy on disk.
        self.write_storage_copy(ws, path, &page.body_storage)?;

        // Only the block map comes of this; without one, the next push of the
        // page regenerates its body rather than patching it.
        let block_map =
            match confed_converter::storage_to_markdown(&page.body_storage, &convert_opts) {
                Ok(converted) => serde_json::to_string(&converted.block_map).ok(),
                Err(e) => {
                    tracing::warn!(
                        target: "confed::sync",
                        page = %summary.id, error = %e,
                        "could not read back the body the server stored"
                    );
                    None
                }
            };

        let slug =
            path.rsplit('/').next().and_then(|f| f.strip_suffix(".md")).unwrap_or(path).to_string();

        // The base and the remote copy advance together.
        ws.state().atomically(|state| {
            state.upsert_page(&PageRecord {
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
                block_map,
                sync_state: SyncState::Clean,
                synced_at: now(),
                render_key: render_key(&page.body_storage, &convert_opts.users),
            })?;

            state.upsert_remote(&RemotePage {
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
        })
    }
}

#[cfg(test)]
mod lost_create_tests {
    use super::*;
    use confed_api::{PageStatus, PageSummary};

    fn page(body: &str, version: u32, author: &str, created: &str) -> Page {
        Page {
            summary: PageSummary {
                id: PageId::new("1001"),
                title: "Onboarding".into(),
                space_key: "DOCS".into(),
                parent_id: None,
                position: None,
                version,
                status: PageStatus::Current,
                labels: Vec::new(),
                author: Some(author.into()),
                created_at: Some(created.into()),
                updated_at: Some(created.into()),
            },
            body_storage: body.into(),
        }
    }

    fn sent(body: &str, at: &str) -> SentCreate {
        SentCreate {
            title: "Onboarding".into(),
            parent_id: None,
            content_hash: String::new(),
            storage_hash: Some(hash_str(&confed_converter::pretty::minify(body))),
            sent_at: Some(at.into()),
        }
    }

    #[test]
    fn the_body_it_was_sent_with_identifies_it() {
        let sent = sent("<p>Фраза 1.</p>", "2026-11-10T10:00:00Z");
        assert!(is_the_lost_create(
            &page("<p>Фраза 1.</p>", 4, "Bob Lee", "2020-01-01T00:00:00Z"),
            &sent,
            Some("Alice Ng")
        ));
    }

    /// A server that rewrites the body on save: a first version by this user,
    /// made no earlier than the request, is the one.
    #[test]
    fn otherwise_a_fresh_untouched_page_of_ones_own_is_it() {
        let sent = sent("<p>Фраза 1.</p>", "2026-11-10T10:00:00Z");
        let rewritten = "<p local-id=\"x\">Фраза 1.</p>";
        let me = Some("Alice Ng");
        assert!(is_the_lost_create(
            &page(rewritten, 1, "Alice Ng", "2026-11-10T10:00:02Z"),
            &sent,
            me
        ));
        assert!(
            is_the_lost_create(&page(rewritten, 1, "Alice Ng", "2026-11-10T09:55:00Z"), &sent, me),
            "within the clock skew"
        );
        assert!(
            !is_the_lost_create(&page(rewritten, 2, "Alice Ng", "2026-11-10T10:00:02Z"), &sent, me),
            "edited since"
        );
        assert!(
            !is_the_lost_create(&page(rewritten, 1, "Bob Lee", "2026-11-10T10:00:02Z"), &sent, me),
            "someone else's"
        );
        assert!(
            !is_the_lost_create(&page(rewritten, 1, "Alice Ng", "2026-11-09T10:00:00Z"), &sent, me),
            "there before"
        );
        assert!(
            !is_the_lost_create(
                &page(rewritten, 1, "Alice Ng", "2026-11-10T10:00:02Z"),
                &sent,
                None
            ),
            "who am I unknown"
        );
    }
}
