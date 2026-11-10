//! Bringing working files in line with what fetch saw: decide, place, write,
//! merge, and keep each page's sidecar in step.

use super::*;

#[derive(Clone, Debug, Default)]
pub struct PullOptions {
    /// Path globs / page ids limiting what is materialized.
    pub scope: Vec<String>,
    pub no_fetch: bool,
    /// Overwrite local changes instead of stopping.
    pub force: bool,
    /// Make every tracked page match the server again, discarding local edits,
    /// merges, conflicts and comment drafts, and re-downloading attachments.
    ///
    /// Modelled on `git reset --hard`: it discards local changes to pages confed
    /// tracks, and leaves files that exist only locally alone. Implies `force`.
    pub reset: bool,
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
    /// Stopped by Ctrl-C: what is listed was done, and the rest is left for
    /// the next run. Only in the JSON when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
    pub created: Vec<PageChange>,
    pub updated: Vec<PageChange>,
    pub merged: Vec<PageChange>,
    pub conflicted: Vec<PageChange>,
    pub deleted: Vec<PageChange>,
    pub moved: Vec<MovedPage>,
    /// Pages left untouched because writing would have destroyed local work.
    pub skipped_dirty: Vec<BlockedPage>,
    /// Pages whose local changes `--reset` or `--force` deliberately discarded.
    pub discarded: Vec<PageChange>,
    pub attachments_downloaded: usize,
    /// Local copies removed because the server no longer has the attachment.
    pub attachments_removed: usize,
    pub dry_run: bool,
    /// What the user should know besides the changes: comments that could not
    /// be re-read, say. Reported as warnings, not as part of the result.
    #[serde(skip)]
    pub warnings: Vec<String>,
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
    /// `delete`. Empty for pull-side changes, which always rewrite the file —
    /// except `attachments` and `comments`, for a pull that found only those
    /// changed (the page's text and its version are as they were).
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
    /// Why, for a person.
    pub reason: String,
    /// Why, for code: what decides the exit code and what to suggest. Not in
    /// the JSON, whose `reason` is the documented field.
    #[serde(skip)]
    pub kind: Blocked,
}

/// What stops a page from going up or coming down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Blocked {
    /// The file still has merge conflict markers.
    Conflicted,
    /// The server has a newer version than the file was based on.
    Stale,
    /// Local work that the operation would overwrite or drop.
    LocalWork,
    /// Something only `--allow-delete` may do.
    NeedsAllowDelete,
    /// Its parent is in another space: `confed instance mv`'s work.
    CrossSpace,
    #[default]
    Other,
}

impl Blocked {
    /// Whether this is a version conflict: what `pull` and then `push` fix,
    /// and what exit 4 means.
    pub fn is_conflict(self) -> bool {
        matches!(self, Blocked::Conflicted | Blocked::Stale)
    }
}

// ----------------------------------------------------------------- push ----

/// What to say about comment work whose page is gone from the server.
pub(super) fn deleted_page_message(path: &str) -> String {
    format!(
        "{path}: the page no longer exists on the server (deleted?), so its comment drafts \
         cannot be posted; they are kept — copy what you need, then `confed pull --force` \
         removes the page and its drafts"
    )
}

pub(super) fn status_of<'a>(statuses: &'a [PageStatus], page_id: &str) -> Option<&'a PageStatus> {
    statuses.iter().find(|s| s.page_id.as_deref() == Some(page_id))
}

/// What a pull reads once and every pass of it consults: the server's
/// pages, the base, each page's status and placement, and how to render.
pub(super) struct PullView<'a> {
    pub(super) remote: &'a [RemotePage],
    pub(super) base: &'a [PageRecord],
    pub(super) statuses: &'a [PageStatus],
    pub(super) placements: &'a HashMap<String, Placement>,
    pub(super) render: &'a Rendering,
    pub(super) files_by_id: HashMap<&'a str, &'a LocalFile>,
    pub(super) base_by_id: HashMap<&'a str, &'a PageRecord>,
    pub(super) claimed: Vec<&'a str>,
    pub(super) claimed_sidecars: Vec<&'a str>,
    /// Whether attachments and comments are read for every page in scope,
    /// not only for those the checks point at.
    pub(super) insist: bool,
}

/// What `pull` should do with one page.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum PullAction {
    Nothing,
    Create,
    /// Base is behind and there is nothing local to lose.
    Overwrite,
    Merge,
    Delete,
    /// Writing would destroy local work.
    Blocked(String),
}

pub(super) fn decide_pull(
    remote: &RemotePage,
    status: Option<&PageStatus>,
    base: Option<&PageRecord>,
    opts: &PullOptions,
    stale_rendering: bool,
) -> PullAction {
    if remote.deleted {
        // Never pulled: there is no file to delete, and nothing to report.
        if base.is_none() && status.is_none() {
            return PullAction::Nothing;
        }
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

    // `--reset` answers one question for every tracked page: what does the
    // server have? Local edits, merges and conflicts are all discarded.
    if opts.reset && remote.storage_body.is_some() {
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
            if opts.force || opts.reset {
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
            if opts.force || opts.reset {
                PullAction::Overwrite
            } else if opts.no_merge {
                PullAction::Blocked("local edits and remote edits (merging disabled)".into())
            } else {
                PullAction::Merge
            }
        }
        PageState::Modified => PullAction::Nothing,
        PageState::LocalDeleted => {
            if opts.force || opts.reset {
                PullAction::Create
            } else {
                PullAction::Nothing
            }
        }
        // Unchanged, RemoteDeleted (handled above), and LocalNew: nothing to write.
        _ => PullAction::Nothing,
    }
}

pub(super) fn link_map(placements: &HashMap<String, Placement>) -> HashMap<String, String> {
    placements.iter().map(|(id, p)| (id.clone(), p.path.clone())).collect()
}

/// Move a page's sidecar to follow a rename.
///
/// The destination may already exist, because pull writes the new page's
/// comment sidecar before it gets here; anything not already at the destination
/// is carried over, then the old directory is removed.
pub(super) fn move_sidecar(old: &Path, new: &Path) {
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

impl SyncEngine {
    /// Materialize the fetched state into working files.
    pub async fn pull(&self, ws: &mut Workspace, opts: &PullOptions) -> Result<PullOutcome> {
        let (warnings, read, interrupted) = self.fetch_before_pull(ws, opts).await?;
        let mut outcome =
            PullOutcome { dry_run: opts.dry_run, warnings, interrupted, ..Default::default() };
        if interrupted {
            return Ok(outcome);
        }

        let (files, _) = worktree::read_working_files(ws)?;
        self.reconcile_paths(ws, &files)?;
        let base = ws.state().all_pages()?;
        let remote = ws.state().all_remote()?;
        let statuses = worktree::compute_status(&files, &base, &remote);
        let placements = self.plan_placements(&remote, &base);
        let render = Rendering { links: link_map(&placements), ctx: ConvertContext::load(ws)? };
        let view = PullView {
            remote: &remote,
            base: &base,
            statuses: &statuses,
            placements: &placements,
            render: &render,
            files_by_id: files
                .iter()
                .filter_map(|f| f.file.frontmatter.page_id().map(|id| (id, f)))
                .collect(),
            base_by_id: base.iter().map(|p| (p.page_id.as_str(), p)).collect(),
            // What every page in the space lays claim to. A rename leaves a
            // file and a sidecar behind, and they are removed — but never one
            // that another page is taking over: when two pages swap titles,
            // one page's old path is the other's new one.
            claimed: placements.values().map(|p| p.path.as_str()).collect(),
            claimed_sidecars: placements.values().map(|p| p.sidecar.as_str()).collect(),
            // A page named in the scope, and every page under `--force` or
            // `--reset`, has its attachments and comments read whatever its
            // version says.
            insist: !opts.scope.is_empty() || opts.force || opts.reset,
        };

        // Pass one: decide, and refuse the whole operation if anything would
        // be clobbered. Nothing is written until the plan is known to be safe.
        let plan = self.plan_pull(ws, opts, &view, &mut outcome)?;
        if !outcome.skipped_dirty.is_empty() && !opts.force && !opts.reset {
            return Err(ConfedError::state_with_hint(
                format!(
                    "{} page(s) have local changes that `pull` would overwrite",
                    outcome.skipped_dirty.len()
                ),
                "review with `confed diff`, then re-run with --force to discard local edits \
                 (or push your changes first)",
            ));
        }

        if view.insist {
            self.reread_extras(ws, opts, &view, &read, &mut outcome).await?;
            if outcome.interrupted {
                self.progress.finish();
                return Ok(outcome);
            }
        }

        // Pass two: apply.
        self.progress.stage(if opts.dry_run { "Checking" } else { "Writing" }, Some(plan.len()));
        let mut handled: Vec<String> = Vec::new();
        for (remote_page, action) in plan {
            if crate::interrupt::requested() {
                outcome.interrupted = true;
                break;
            }
            handled.push(remote_page.page_id.clone());
            self.pull_one(ws, opts, &view, remote_page, action, &mut outcome).await?;
        }

        // Each page written is whole; the ones after it wait for the next pull.
        if !outcome.interrupted {
            self.tend_unpulled_sidecars(ws, opts, &view, &handled, &mut outcome).await?;
        }
        if !outcome.interrupted {
            self.warn_missing_comment_attachments(ws, opts, &view, &mut outcome)?;
        }

        self.progress.finish();
        Ok(outcome)
    }

    /// Fetch first, unless told not to: what it could not read becomes the
    /// pull's first warnings, and what it did read is not asked for twice.
    async fn fetch_before_pull(
        &self,
        ws: &mut Workspace,
        opts: &PullOptions,
    ) -> Result<(Vec<String>, ExtrasRead, bool)> {
        let mut warnings: Vec<String> = Vec::new();
        // What this run has already read from the server, beside page bodies.
        let mut read = ExtrasRead::default();
        let mut interrupted = false;
        if !opts.no_fetch {
            let (fetch, fetched) = self.fetch_tracking(ws, &FetchOptions::default()).await?;
            read = fetched;
            interrupted = fetch.interrupted;
            // A page that could not be fetched is pulled as it was last seen;
            // say which, rather than only that there were some.
            warnings.extend(fetch.failed.iter().map(|f| {
                let name = if f.title.is_empty() { &f.page_id } else { &f.title };
                format!("{name} ({}): {}", f.page_id, f.error)
            }));
            warnings.extend(fetch.check_warnings());
        }
        Ok((warnings, read, interrupted))
    }

    /// Pass one: what to do with each page in scope. A page whose local work
    /// the plan would overwrite is listed in `skipped_dirty`.
    fn plan_pull(
        &self,
        ws: &Workspace,
        opts: &PullOptions,
        view: &PullView<'_>,
        outcome: &mut PullOutcome,
    ) -> Result<Vec<(RemotePage, PullAction)>> {
        let known_users = &view.render.ctx.users;
        // Pages the listing has moved on whose body could not be fetched yet.
        // What is stored for them is the previous text, or none: writing it
        // would stamp old content with the new version, and nothing would ever
        // notice. They stay as they are until a fetch brings the body.
        let owed: Vec<String> = ws
            .state()
            .pending_fetches()?
            .into_iter()
            .filter(|(_, needs)| needs.iter().any(|n| n == "body"))
            .map(|(id, _)| id)
            .collect();
        let mut plan: Vec<(RemotePage, PullAction)> = Vec::new();
        for remote_page in view.remote {
            if !self.in_scope(&opts.scope, view.placements, &remote_page.page_id) {
                continue;
            }
            if owed.contains(&remote_page.page_id) && !remote_page.deleted {
                // With a fetch in this run, its failure has been reported.
                if opts.no_fetch {
                    outcome.warnings.push(format!(
                        "{} ({}): its content has not been fetched yet, so it is left as it \
                         is; `confed pull` fetches it",
                        remote_page.title, remote_page.page_id
                    ));
                }
                continue;
            }
            let status =
                view.statuses.iter().find(|s| s.page_id.as_deref() == Some(&remote_page.page_id));
            let base_record = view.base_by_id.get(remote_page.page_id.as_str()).copied();
            // Was this file rendered from the same inputs confed would use now?
            let stale_rendering = match (base_record, &remote_page.storage_body) {
                (Some(base), Some(body)) => base.render_key != render_key(body, known_users),
                _ => false,
            };
            let mut action = decide_pull(remote_page, status, base_record, opts, stale_rendering);
            // Unsent comment work is local work too: a page deleted on the server
            // is not removed with its drafts unless told to.
            if action == PullAction::Delete && !opts.force && !opts.reset {
                let pending = base_record.map_or(0, |b| pending_comment_work(ws, &b.local_path));
                if pending > 0 {
                    action = PullAction::Blocked(format!(
                        "deleted on the server, with {pending} unsent comment draft(s) that can \
                         no longer be posted; copy what you need, then `confed pull --force` \
                         removes the page and its drafts"
                    ));
                }
            }
            if let PullAction::Blocked(reason) = &action {
                outcome.skipped_dirty.push(BlockedPage {
                    page_id: remote_page.page_id.clone(),
                    path: status.map(|s| s.path.clone()).unwrap_or_default(),
                    reason: reason.clone(),
                    kind: Blocked::LocalWork,
                });
            }
            if !matches!(action, PullAction::Nothing) {
                plan.push((remote_page.clone(), action));
            }
        }

        Ok(plan)
    }

    /// The check fetch makes finds what was added or edited, through a
    /// search; this is the request that cannot miss, and the one that sees a
    /// deletion: the attachments and comments of every page in scope, read
    /// again unless this run already has.
    async fn reread_extras(
        &self,
        ws: &mut Workspace,
        opts: &PullOptions,
        view: &PullView<'_>,
        read: &ExtrasRead,
        outcome: &mut PullOutcome,
    ) -> Result<()> {
        if !opts.no_fetch {
            let work: Vec<FetchWork> = view
                .remote
                .iter()
                .filter(|r| !r.deleted && r.storage_body.is_some())
                .filter(|r| self.in_scope(&opts.scope, view.placements, &r.page_id))
                .map(|r| FetchWork {
                    page_id: r.page_id.clone(),
                    body: false,
                    attachments: opts.with_attachments && !read.attachments.contains(&r.page_id),
                    comments: opts.with_comments && !read.comments.contains(&r.page_id),
                })
                .filter(|w| w.attachments || w.comments)
                .collect();
            let titles: HashMap<String, String> =
                view.remote.iter().map(|r| (r.page_id.clone(), r.title.clone())).collect();
            self.progress.stage("Reading attachments and comments", Some(work.len()));
            let tally = self.run_fetches(ws, work, &titles).await?;
            outcome.interrupted = tally.interrupted;
            outcome.warnings.extend(tally.failed.iter().map(|f| {
                let path = view.placements.get(&f.page_id).map_or(f.page_id.as_str(), |p| &p.path);
                format!("{path}: {}", f.error)
            }));
        }
        Ok(())
    }

    /// Pass two, for one page: delete it, or write or merge it at its
    /// placement — following a server-side rename, and bringing its sidecar
    /// along — and then its attachments and comments.
    async fn pull_one(
        &self,
        ws: &mut Workspace,
        opts: &PullOptions,
        view: &PullView<'_>,
        remote_page: RemotePage,
        action: PullAction,
        outcome: &mut PullOutcome,
    ) -> Result<()> {
        let local = view.files_by_id.get(remote_page.page_id.as_str()).copied();
        let base_record = view.base_by_id.get(remote_page.page_id.as_str()).copied();

        // Deletions are handled first: a page that is gone on the server has
        // no placement, because placements only cover pages that still exist.
        if action == PullAction::Delete {
            let path = base_record.map(|b| b.local_path.clone()).unwrap_or_default();
            // Say what --force threw away with it.
            let dirty =
                status_of(view.statuses, &remote_page.page_id).is_some_and(|s| s.local_dirty);
            if (opts.force || opts.reset) && (dirty || pending_comment_work(ws, &path) > 0) {
                outcome.discarded.push(PageChange {
                    page_id: remote_page.page_id.clone(),
                    path: path.clone(),
                    title: remote_page.title.clone(),
                    from_version: base_record.map(|b| b.version),
                    to_version: None,
                    ops: Vec::new(),
                });
            }
            if !opts.dry_run {
                self.delete_local(ws, base_record)?;
            }
            self.progress.item(&path);
            outcome.deleted.push(PageChange {
                page_id: remote_page.page_id.clone(),
                path,
                title: remote_page.title.clone(),
                from_version: base_record.map(|b| b.version),
                to_version: None,
                ops: Vec::new(),
            });
            return Ok(());
        }

        let Some(placement) = view.placements.get(&remote_page.page_id) else { return Ok(()) };
        let writes = match &action {
            PullAction::Nothing | PullAction::Delete => false,
            PullAction::Blocked(_) => opts.force || opts.reset,
            _ => true,
        };
        if !writes {
            return Ok(());
        }

        // A page renamed on the server moves its file, and its sidecar goes
        // ahead of it: the attachments and unsent comment work in it are
        // then where everything below looks for them, and a pull cut short
        // right here is finished by the next one. Unless the sidecar cannot
        // simply follow — pages that swap titles, or take over one
        // another's, each want a directory another still has. Whatever such
        // a page finds at its new path is not its own, so nothing there is
        // taken for its attachments: they are all fetched again.
        let moved_from = base_record
            .map(|b| b.local_path.as_str())
            .filter(|old| *old != placement.path && !opts.dry_run);
        let mut displaced = false;
        if let Some(old) = moved_from {
            let page = remote_page.page_id.as_str();
            let old_sidecar = paths::sidecar_for(old);
            let new_sidecar = paths::sidecar_for(&placement.path);
            displaced =
                view.placements.iter().any(|(id, p)| id != page && p.sidecar == old_sidecar)
                    || view.base.iter().any(|b| {
                        b.page_id != page && paths::sidecar_for(&b.local_path) == new_sidecar
                    });
            if displaced {
                for mut record in ws.state().page_attachments(page)? {
                    record.downloaded = false;
                    ws.state().upsert_attachment(&record)?;
                }
            } else {
                move_sidecar(&ws.absolute(&old_sidecar), &ws.absolute(&new_sidecar));
            }
        }

        match action {
            PullAction::Nothing | PullAction::Delete => return Ok(()),
            PullAction::Create | PullAction::Overwrite | PullAction::Blocked(_) => {
                let change =
                    self.write_page(ws, &remote_page, placement, local, view.render, opts, None)?;
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
                    view.render,
                    opts,
                )?;
                if conflicted {
                    outcome.conflicted.push(change);
                } else {
                    outcome.merged.push(change);
                }
            }
        }

        // The page is at its new path, so the file at the old one goes
        // now: two files naming one page is not a state to leave behind,
        // should the rest of this pull not happen.
        if let Some(old) = moved_from {
            outcome.moved.push(MovedPage {
                page_id: remote_page.page_id.clone(),
                from: old.to_string(),
                to: placement.path.clone(),
            });
            let left = ws.absolute(old);
            if !view.claimed.contains(&old) && left.exists() {
                let _ = std::fs::remove_file(&left);
            }
        }

        // Say what was thrown away. A destructive flag is not a licence to
        // be quiet about it.
        if (opts.reset || opts.force)
            && status_of(view.statuses, &remote_page.page_id).is_some_and(|s| s.local_dirty)
        {
            outcome.discarded.push(PageChange {
                page_id: remote_page.page_id.clone(),
                path: placement.path.clone(),
                title: remote_page.title.clone(),
                from_version: base_record.map(|b| b.version),
                to_version: Some(remote_page.version),
                ops: Vec::new(),
            });
        }

        self.progress.item(&placement.path);

        if opts.with_attachments && !opts.dry_run {
            let synced = self
                .sync_attachments(ws, &remote_page.page_id, &placement.path, opts, displaced)
                .await?;
            outcome.attachments_downloaded += synced.downloaded;
            outcome.attachments_removed += synced.removed;
            outcome.warnings.extend(synced.warnings);
        }
        if opts.with_comments && !opts.dry_run {
            self.write_comments_sidecar(
                ws,
                &remote_page.page_id,
                &remote_page.title,
                &placement.path,
                !opts.reset,
            )?;
        }

        // A sidecar that could not go ahead of its page is folded into the
        // new one now, where nobody else is taking it over: what the page
        // just wrote and downloaded stays, the rest comes along.
        if let (Some(old), true) = (moved_from, displaced) {
            let old_sidecar = paths::sidecar_for(old);
            if !view.claimed_sidecars.contains(&old_sidecar.as_str()) {
                move_sidecar(
                    &ws.absolute(&old_sidecar),
                    &ws.absolute(&paths::sidecar_for(&placement.path)),
                );
            }
        }
        Ok(())
    }

    /// Pages pull had nothing to write still need their sidecar looked after.
    async fn tend_unpulled_sidecars(
        &self,
        ws: &mut Workspace,
        opts: &PullOptions,
        view: &PullView<'_>,
        handled: &[String],
        outcome: &mut PullOutcome,
    ) -> Result<()> {
        // Pages pull had nothing to write still need their sidecar looked after:
        // the markup copy may be missing (a workspace pulled by an older confed,
        // or a deleted file), a page edited only locally is exactly when an
        // inline comment anchor moves — and files are attached and removed, and
        // comments added, edited and deleted, on the server without the page
        // itself changing.
        for record in ws.state().all_pages()? {
            if crate::interrupt::requested() {
                outcome.interrupted = true;
                break;
            }
            if handled.contains(&record.page_id)
                || !self.in_scope(&opts.scope, view.placements, &record.page_id)
            {
                continue;
            }
            let path = record.local_path.clone();
            let mut ops: Vec<String> = Vec::new();
            // A page whose file was deleted here keeps what it has until the
            // deletion is pushed or undone.
            if opts.with_attachments && ws.absolute(&path).is_file() {
                let changed = if opts.dry_run {
                    self.attachments_differ(ws, &record.page_id, &path, opts)?
                } else {
                    let synced =
                        self.sync_attachments(ws, &record.page_id, &path, opts, false).await?;
                    outcome.attachments_downloaded += synced.downloaded;
                    outcome.attachments_removed += synced.removed;
                    outcome.warnings.extend(synced.warnings);
                    synced.changed
                };
                if changed {
                    ops.push(Extra::Attachments.name().to_string());
                }
            }
            let comments_changed = if opts.dry_run {
                opts.with_comments && !sidecar_is_current(ws, &record.page_id, &path)?
            } else {
                self.ensure_storage_copy(ws, &path, &record.storage_body)?;
                opts.with_comments
                    && self.write_comments_sidecar(
                        ws,
                        &record.page_id,
                        &record.title,
                        &path,
                        !opts.reset,
                    )?
            };
            if comments_changed {
                ops.push(Extra::Comments.name().to_string());
            }
            if !ops.is_empty() {
                outcome.updated.push(PageChange {
                    page_id: record.page_id.clone(),
                    path,
                    title: record.title.clone(),
                    from_version: Some(record.version),
                    to_version: Some(record.version),
                    ops,
                });
            }
        }
        Ok(())
    }

    /// Warn about comments that show or link a file their page lacks.
    fn warn_missing_comment_attachments(
        &self,
        ws: &Workspace,
        opts: &PullOptions,
        view: &PullView<'_>,
        outcome: &mut PullOutcome,
    ) -> Result<()> {
        // A comment that shows or links a file the page does not have cannot
        // be read in full here. Said for the pages this pull touched or was
        // pointed at, not for the whole space on every run.
        let touched: Vec<&str> = [&outcome.created, &outcome.updated, &outcome.merged]
            .into_iter()
            .flatten()
            .chain(&outcome.conflicted)
            .map(|change| change.page_id.as_str())
            .collect();
        if opts.with_attachments && opts.with_comments && (view.insist || !touched.is_empty()) {
            for record in ws.state().all_pages()? {
                let wanted = touched.contains(&record.page_id.as_str())
                    || (view.insist
                        && self.in_scope(&opts.scope, view.placements, &record.page_id));
                if wanted {
                    outcome.warnings.extend(missing_comment_attachments(ws, &record)?);
                }
            }
        }
        Ok(())
    }

    /// Learn where files actually are.
    ///
    /// A page is identified by `page_id`, so a user is free to rename or move
    /// its file. Recording that choice here is what lets a later server-side
    /// rename leave their filename alone while still updating the title.
    pub(super) fn reconcile_paths(&self, ws: &mut Workspace, files: &[LocalFile]) -> Result<()> {
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

    pub(super) fn plan_placements(
        &self,
        remote: &[RemotePage],
        base: &[PageRecord],
    ) -> HashMap<String, Placement> {
        paths::plan_from_records(remote, base)
    }

    pub(super) fn in_scope(
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
    pub(super) fn write_page(
        &self,
        ws: &mut Workspace,
        remote: &RemotePage,
        placement: &Placement,
        local: Option<&LocalFile>,
        render: &Rendering,
        opts: &PullOptions,
        body_override: Option<String>,
    ) -> Result<PageChange> {
        let storage = remote.storage_body.clone().unwrap_or_default();
        let mut convert_opts = render.options(&placement.path);
        if opts.with_comments {
            convert_opts.inline_marks = inline_marks_for(ws, &remote.page_id)?;
        }
        let converted = confed_converter::storage_to_markdown(&storage, &convert_opts)?;

        let body = body_override.unwrap_or(converted.markdown);
        let file = self.build_file(ws, remote, placement, local, body)?;
        let rendered = file.render()?;
        // Read before the base moves: afterwards it is the new version.
        let from_version = ws.state().get_page(&remote.page_id)?.map(|p| p.version);

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
                &render.ctx.users,
            )?;
        }

        Ok(PageChange {
            page_id: remote.page_id.clone(),
            path: placement.path.clone(),
            title: remote.title.clone(),
            from_version,
            to_version: Some(remote.version),
            ops: Vec::new(),
        })
    }

    pub(super) fn build_file(
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

    /// Keep the page's Confluence markup next to its Markdown. It is what the
    /// Markdown was rendered from and what a push patches, so it is the thing to
    /// read when a conversion looks wrong.
    pub(super) fn write_storage_copy(
        &self,
        ws: &Workspace,
        page_path: &str,
        storage: &str,
    ) -> Result<()> {
        write_atomic(&ws.absolute(&paths::storage_file_for(page_path)), &storage_file_text(storage))
    }

    /// Write the markup copy only when it is missing or out of date.
    ///
    /// This is what backfills a workspace that was pulled before confed kept the
    /// copy, and what restores one somebody deleted — without rewriting every
    /// page's file, and its mtime, on every pull.
    pub(super) fn ensure_storage_copy(
        &self,
        ws: &Workspace,
        page_path: &str,
        storage: &str,
    ) -> Result<()> {
        let path = ws.absolute(&paths::storage_file_for(page_path));
        let text = storage_file_text(storage);
        if std::fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
            return Ok(());
        }
        write_atomic(&path, &text)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_base(
        &self,
        ws: &mut Workspace,
        remote: &RemotePage,
        placement: &Placement,
        file: &MarkdownFile,
        storage: &str,
        block_map: &BlockMap,
        sync_state: SyncState,
        users: &HashMap<String, confed_converter::UserLink>,
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
            render_key: render_key(storage, users),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn merge_page(
        &self,
        ws: &mut Workspace,
        remote: &RemotePage,
        placement: &Placement,
        local: Option<&LocalFile>,
        base_record: Option<&PageRecord>,
        render: &Rendering,
        opts: &PullOptions,
    ) -> Result<(PageChange, bool)> {
        let (Some(local), Some(base_record)) = (local, base_record) else {
            let change = self.write_page(ws, remote, placement, local, render, opts, None)?;
            return Ok((change, false));
        };

        let convert_opts = render.options(&placement.path);
        let base_md =
            confed_converter::storage_to_markdown(&base_record.storage_body, &convert_opts)?;
        let remote_storage = remote.storage_body.clone().unwrap_or_default();
        let remote_md = confed_converter::storage_to_markdown(&remote_storage, &convert_opts)?;

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
        // The layer is re-placed after the merge. A conflicted file keeps only
        // the user's drafts: with two candidate texts for a span, placing a
        // comment would be a guess.
        file.marks = if conflicted {
            local.file.drafts().cloned().collect()
        } else {
            local.file.marks.clone()
        };
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
                &render.ctx.users,
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

    pub(super) fn delete_local(&self, ws: &mut Workspace, base: Option<&PageRecord>) -> Result<()> {
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
        if let Some(parent) = path.parent() {
            prune_empty_dirs(ws.root(), parent);
        }
        ws.state().delete_page(&base.page_id)?;
        Ok(())
    }

    /// Bring a page's sidecar, and the `attachments` its file lists, in line
    /// with the attachments in the state: download what is new or has a new
    /// version, and remove the copy of what the server no longer has.
    ///
    /// A file in the sidecar that is not the copy confed last wrote there is
    /// local work. It is neither overwritten nor removed — the page's warning
    /// says so — unless the pull was told to discard local changes.
    ///
    /// `displaced` is for a page this pull moved to a path where its sidecar
    /// could not follow: what is there belongs to whichever page had the path
    /// before, so it is replaced rather than taken for this page's local work.
    pub(super) async fn sync_attachments(
        &self,
        ws: &mut Workspace,
        page_id: &str,
        page_path: &str,
        opts: &PullOptions,
        displaced: bool,
    ) -> Result<AttachmentSync> {
        let mut sync = AttachmentSync::default();
        let discard = opts.force || opts.reset || displaced;
        let restore = opts.reset;
        let sidecar = paths::sidecar_for(page_path);
        let dir = ws.absolute(&sidecar);
        // Scratch from a download this or an earlier pull gave up on. Sweeping
        // it here keeps the sidecar directory nothing but page content.
        let swept = attachments::remove_stale_partials(&dir);
        if swept > 0 {
            tracing::debug!(
                target: "confed::sync",
                page = %page_id, swept,
                "removed partial downloads left by an interrupted pull"
            );
        }

        // The state does not keep download links, so the page is asked for
        // them — only when there is something to download. What it answers is
        // the newest word on its attachments, and the state follows it first:
        // a file deleted since the fetch is then not waited for forever.
        let wants = |ws: &Workspace| -> Result<bool> {
            Ok(ws
                .state()
                .page_attachments(page_id)?
                .iter()
                .any(|r| would_download(r, &dir.join(&r.filename), restore, discard)))
        };
        let mut listed = Vec::new();
        if wants(ws)? {
            match self.client.list_attachments(&PageId::new(page_id)).await {
                Ok(now) => {
                    self.refresh_attachments(ws, page_id, &now)?;
                    listed = now;
                }
                Err(e) => sync.warnings.push(format!(
                    "{page_path}: its attachments could not be downloaded: {}",
                    ConfedError::from(e)
                )),
            }
        }

        for gone in ws.state().removed_attachments(page_id)? {
            let dest = dir.join(&gone.filename);
            // Only a copy confed wrote is confed's to remove: untouched, or —
            // when told to discard local changes — edited since. A file it
            // never wrote merely shares the name, and stays without a word.
            let ours = gone.sha256.as_deref().filter(|_| attachments::is_storable(&gone.filename));
            if let (Some(written), true) = (ours, dest.is_file()) {
                let untouched = attachments::file_sha256(&dest).ok().as_deref() == Some(written);
                if untouched || discard {
                    std::fs::remove_file(&dest)
                        .map_err(|e| ConfedError::io(format!("removing {}", dest.display()), e))?;
                    sync.removed += 1;
                } else {
                    sync.warnings.push(format!(
                        "{sidecar}/{}: the attachment was deleted on the server, but the file \
                         here has changed since confed downloaded it, so it is kept — delete \
                         it, or `confed push` attaches it again",
                        gone.filename
                    ));
                }
            }
            ws.state().forget_removed_attachment(page_id, &gone.filename)?;
        }

        for record in ws.state().page_attachments(page_id)? {
            let dest = dir.join(&record.filename);
            if !needs_download(&record, &dest, restore) {
                continue;
            }
            if !would_download(&record, &dest, restore, discard) {
                sync.warnings.push(format!(
                    "{sidecar}/{}: the server's version has not been downloaded here, and the \
                     file here is not a copy confed downloaded, so it is kept — `confed pull \
                     --force` on the page takes the server's, `confed push` uploads this one \
                     over it",
                    record.filename
                ));
                continue;
            }
            let Some(attachment) = listed.iter().find(|a| a.id.0 == record.attachment_id) else {
                continue;
            };
            std::fs::create_dir_all(&dir)
                .map_err(|e| ConfedError::io(format!("creating {}", dir.display()), e))?;
            // One file that will not come down is no reason to stop: the rest
            // of the pull stands, and the next one asks for this file again.
            if let Err(e) = self.client.download_attachment(attachment, &dest).await {
                sync.warnings.push(format!(
                    "{sidecar}/{}: could not be downloaded: {}",
                    record.filename,
                    ConfedError::from(e)
                ));
                continue;
            }
            let mut updated = record.clone();
            updated.version = attachment.version;
            updated.downloaded = true;
            updated.sha256 = attachments::file_sha256(&dest).ok();
            updated.file_size = attachments::file_size(&dest);
            ws.state().upsert_attachment(&updated)?;
            sync.downloaded += 1;
        }

        let relisted = write_attachment_refs(ws, page_id, page_path)?;
        sync.changed = relisted || sync.downloaded > 0 || sync.removed > 0;
        Ok(sync)
    }

    /// Whether [`Self::sync_attachments`] would find something to do: what a
    /// dry run reports, without downloading or removing anything.
    pub(super) fn attachments_differ(
        &self,
        ws: &Workspace,
        page_id: &str,
        page_path: &str,
        opts: &PullOptions,
    ) -> Result<bool> {
        let dir = ws.absolute(&paths::sidecar_for(page_path));
        let records = ws.state().page_attachments(page_id)?;
        let restore = opts.reset;
        let discard = opts.force || opts.reset;
        let removable = |gone: &crate::state::RemovedAttachment| {
            gone.sha256.is_some() && dir.join(&gone.filename).is_file()
        };
        Ok(ws.state().removed_attachments(page_id)?.iter().any(removable)
            || records.iter().any(|r| would_download(r, &dir.join(&r.filename), restore, discard))
            || listed_attachments(ws, page_path)
                .is_some_and(|listed| !same_refs(&listed, &attachments::to_refs(&records))))
    }

    /// Bring a page's comment files in line with the comments in the state:
    /// the marks in the page body, then the sidecar.
    ///
    /// Returns whether the sidecar was showing something else than the server's
    /// comments — which is how a pull learns that a page's comments changed
    /// while the page did not. The file is written only when its text differs.
    pub(super) fn write_comments_sidecar(
        &self,
        ws: &mut Workspace,
        page_id: &str,
        title: &str,
        page_path: &str,
        keep_drafts: bool,
    ) -> Result<bool> {
        sync_marks(ws, page_id, page_path)?;
        self.reanchor_inline_comments(ws, page_id, page_path)?;
        let records = ws.state().page_comments(page_id)?;
        let path = ws.absolute(&paths::sidecar_for(page_path)).join(comments::COMMENTS_FILENAME);
        let on_disk = std::fs::read_to_string(&path).ok();
        let existing =
            on_disk.as_deref().and_then(|text| comments::parse(text).ok()).unwrap_or_default();
        let changed = !comments::is_current(&existing, &records);

        // Unsent work survives a refresh — except under `--reset`, where it is
        // a local modification like any other. A resolve request for a thread
        // that is resolved by now, or gone, has nothing left to do.
        let unsent = existing.drafts().count() + existing.resolve_requests.len();
        let (drafts, requests): (Vec<comments::SidecarComment>, Vec<String>) = if keep_drafts {
            let open = |id: &String| records.iter().any(|r| &r.comment_id == id && !r.resolved);
            (
                existing.drafts().cloned().collect(),
                existing.resolve_requests.iter().filter(|id| open(id)).cloned().collect(),
            )
        } else {
            (Vec::new(), Vec::new())
        };

        // No comments, nothing unsent, and no file that says otherwise.
        let nothing_to_show = records.is_empty() && drafts.is_empty() && requests.is_empty();
        if nothing_to_show && !changed && unsent == 0 {
            return Ok(false);
        }
        let text = comments::render_with_requests(page_id, title, &records, &drafts, &requests);
        if on_disk.as_deref() != Some(text.as_str()) {
            write_atomic(&path, &text)?;
        }
        Ok(changed)
    }

    /// Re-locate every inline comment's anchor in the page's current text.
    ///
    /// Anchors are stored rather than embedded in the body, so an edit can move
    /// or destroy the text a comment points at. Anything that cannot be located
    /// confidently is flagged `orphaned` instead of being attached to the wrong
    /// sentence — and never deleted, on either side.
    pub(super) fn reanchor_inline_comments(
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
        let body = &marks::strip(body).body;

        let mut orphaned = 0;
        for record in
            records.into_iter().filter(|c| c.kind == "inline" && c.parent_comment_id.is_none())
        {
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
}
