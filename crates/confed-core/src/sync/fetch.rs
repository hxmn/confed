//! Refreshing `.state.db` from the server: the listing, page bodies, and the
//! attachments and comments read with them.

use super::*;

/// Workspace setting holding when the stored comments were last known to be
/// current: the start of the last fetch that asked the server which comments
/// had changed. The next such fetch asks from here.
pub const COMMENTS_CHECKED_AT_KEY: &str = "comments_checked_at";

/// The same for the stored attachment lists. It is a mark of its own because
/// a workspace can have checked one and never the other.
pub const ATTACHMENTS_CHECKED_AT_KEY: &str = "attachments_checked_at";

/// How far past the last check a search for changes reaches back. It covers
/// the server's search index catching up with an edit, and costs only
/// re-reading the pages touched in that window.
pub(super) const CHECK_OVERLAP_MINUTES: u64 = 15;

/// What a page carries beside its body. Each has versions of its own, so the
/// page's version says nothing about either: a file attached to a page, or a
/// comment edited on it, leaves the page where it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Extra {
    Attachments,
    Comments,
}

impl Extra {
    /// What the fetch queue calls it, and what messages do.
    pub(super) fn name(self) -> &'static str {
        match self {
            Extra::Attachments => "attachments",
            Extra::Comments => "comments",
        }
    }

    pub(super) fn mark_key(self) -> &'static str {
        match self {
            Extra::Attachments => ATTACHMENTS_CHECKED_AT_KEY,
            Extra::Comments => COMMENTS_CHECKED_AT_KEY,
        }
    }
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
    /// Pages whose content came from `.pages.db` instead of the network.
    pub from_cache: usize,
    pub deleted_on_remote: Vec<String>,
    pub failed: Vec<FailedPage>,
    /// True when an interrupted fetch was continued rather than restarted.
    pub resumed: bool,
    /// Pages at an unchanged version whose comments were read again: a comment
    /// has a version of its own, and editing one leaves the page's alone.
    pub comments_refreshed: usize,
    /// Of those, the pages whose comments turned out to have changed.
    pub comments_changed: Vec<String>,
    /// Why the server could not be asked which comments changed, when it could
    /// not. Comments of unchanged pages may then be out of date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_check_failed: Option<String>,
    /// Pages at an unchanged version whose attachments were listed again: a
    /// file attached to a page — or uploaded into a comment on it — leaves the
    /// page's version alone, as a comment does.
    pub attachments_refreshed: usize,
    /// Of those, the pages whose attachments turned out to have changed.
    pub attachments_changed: Vec<String>,
    /// Why the server could not be asked which attachments changed, when it
    /// could not. Attachments of unchanged pages may then be out of date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_check_failed: Option<String>,
}

impl FetchOutcome {
    /// What to tell the user when the comment check failed.
    pub fn comment_check_warning(&self) -> Option<String> {
        let reason = self.comment_check_failed.as_deref()?;
        Some(format!(
            "could not ask the server which comments changed ({reason}); comments of pages \
             that did not change themselves may be out of date — `confed pull <page>` or \
             `confed comment list <page> --refresh` re-reads a page's"
        ))
    }

    /// What to tell the user when the attachment check failed.
    pub fn attachment_check_warning(&self) -> Option<String> {
        let reason = self.attachment_check_failed.as_deref()?;
        Some(format!(
            "could not ask the server which attachments changed ({reason}); attachments of \
             pages that did not change themselves may be out of date — `confed pull <page>` \
             lists a page's again"
        ))
    }

    /// Both of the above, for whoever reports a fetch.
    pub fn check_warnings(&self) -> Vec<String> {
        self.attachment_check_warning().into_iter().chain(self.comment_check_warning()).collect()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FailedPage {
    pub page_id: String,
    pub title: String,
    pub error: String,
}

/// A list read from the server, or the error when it could not be read. What
/// is already stored is then kept: a failed request is not "this page has
/// none".
pub(super) type Listed<T> = std::result::Result<Vec<T>, String>;

/// Everything fetched for one page, before it is written to the DB.
pub(super) struct FetchedPage {
    pub(super) page: Page,
    pub(super) attachments: Listed<confed_api::Attachment>,
    pub(super) comments: Listed<Comment>,
}

/// What one item of fetch work brought back.
pub(super) enum Fetched {
    Page(Box<FetchedPage>),
    /// What was asked for of a page whose body is already current; `None` for
    /// the part that was not.
    Extras {
        attachments: Option<Listed<confed_api::Attachment>>,
        comments: Option<Listed<Comment>>,
    },
}

/// One item of fetch work: a page, and how much of it is wanted.
pub(super) struct FetchWork {
    pub(super) page_id: String,
    /// The page itself, which brings everything on it along.
    pub(super) body: bool,
    pub(super) attachments: bool,
    pub(super) comments: bool,
}

impl FetchWork {
    /// The work a fetch queue entry stands for.
    pub(super) fn owed(page_id: String, needs: &[String]) -> Self {
        let has = |need: &str| needs.iter().any(|n| n == need);
        Self {
            body: has("body"),
            attachments: has(Extra::Attachments.name()),
            comments: has(Extra::Comments.name()),
            page_id,
        }
    }
}

/// The pages whose attachments, and whose comments, a run has read from the
/// server — so that nothing is asked for twice.
#[derive(Default)]
pub(super) struct ExtrasRead {
    pub(super) attachments: Vec<String>,
    pub(super) comments: Vec<String>,
}

/// What a round of fetch work did, on top of what it stored.
#[derive(Default)]
pub(super) struct FetchTally {
    pub(super) fetched: usize,
    /// Pages whose attachments and comments are now as the server has them.
    pub(super) read: ExtrasRead,
    /// Pages whose attachments were listed without the page.
    pub(super) attachments_refreshed: usize,
    /// Of those, the ones whose attachments had changed.
    pub(super) attachments_changed: Vec<String>,
    /// Pages whose comments were read without the page.
    pub(super) comments_refreshed: usize,
    /// Of those, the ones whose comments had changed.
    pub(super) comments_changed: Vec<String>,
    pub(super) failed: Vec<FailedPage>,
}

/// Who can say how current the stored attachments and comments of a page are,
/// when its body needs no request.
pub(super) enum Voucher {
    /// The state has had them all along: its marks.
    State,
    /// They were just put back from the page cache, where the state had none
    /// of its own: the cache's marks. What the state did have was left in
    /// place, and only its own mark can vouch for that.
    Cache { own_attachments: bool, own_comments: bool },
    /// Nobody: the page is read.
    Nobody,
}

/// One fetch's reckoning of which pages need an [`Extra`] read again.
pub(super) struct ExtraCheck {
    pub(super) extra: Extra,
    /// When the state's copies were last known to be current, if it says.
    pub(super) state_mark: Option<chrono::DateTime<chrono::Utc>>,
    /// The same for the copies in the page cache.
    pub(super) cache_mark: Option<chrono::DateTime<chrono::Utc>>,
    /// Pages a mark vouches for: what changed on these since is searched for.
    pub(super) covered: Vec<String>,
    /// Pages no mark vouches for: these are read, not reasoned about.
    pub(super) uncovered: Vec<String>,
    /// The oldest mark vouching for a covered page, which is where the search
    /// has to start.
    pub(super) oldest_mark: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether this run can say the whole space is current as of its start.
    pub(super) checked: bool,
    /// Why the server could not be asked, when it could not.
    pub(super) failed: Option<String>,
}

impl ExtraCheck {
    pub(super) fn new(
        extra: Extra,
        state: &crate::state::StateDb,
        cache: Option<&crate::pagestore::PageStore>,
    ) -> Result<Self> {
        let cached = cache.and_then(|c| match extra {
            Extra::Attachments => c.attachments_checked_at().ok().flatten(),
            Extra::Comments => c.comments_checked_at().ok().flatten(),
        });
        Ok(Self {
            extra,
            state_mark: state.get_meta(extra.mark_key())?.and_then(|m| trusted_mark(&m)),
            cache_mark: cached.and_then(|m| trusted_mark(&m)),
            covered: Vec::new(),
            uncovered: Vec::new(),
            oldest_mark: None,
            checked: false,
            failed: None,
        })
    }

    /// File a page whose body is current under the mark that vouches for it,
    /// or under none.
    pub(super) fn place(&mut self, page_id: &str, voucher: &Voucher) {
        let mark = match voucher {
            Voucher::State => self.state_mark,
            Voucher::Cache { own_attachments, own_comments } => {
                let own = match self.extra {
                    Extra::Attachments => *own_attachments,
                    Extra::Comments => *own_comments,
                };
                match (self.cache_mark, self.state_mark) {
                    (Some(c), Some(s)) => Some(c.min(s)),
                    (Some(c), None) if !own => Some(c),
                    _ => None,
                }
            }
            Voucher::Nobody => None,
        };
        match mark {
            Some(mark) => {
                self.oldest_mark = Some(self.oldest_mark.map_or(mark, |o| o.min(mark)));
                self.covered.push(page_id.to_string());
            }
            None => self.uncovered.push(page_id.to_string()),
        }
    }
}

// ----------------------------------------------------------------- pull ----

/// One API comment as confed stores it.
pub(super) fn comment_record(page_id: &str, comment: &Comment) -> CommentRecord {
    CommentRecord {
        comment_id: comment.id.0.clone(),
        page_id: page_id.to_string(),
        parent_comment_id: comment.parent_comment_id.as_ref().map(|c| c.0.clone()),
        kind: match comment.kind {
            CommentKind::Footer => "footer".into(),
            CommentKind::Inline => "inline".into(),
        },
        author: comment.author.clone(),
        created_at: comment.created_at.clone(),
        body_storage: Some(comment.body_storage.clone()),
        body_markdown: confed_converter::storage_fragment_to_markdown(&comment.body_storage)
            .unwrap_or_else(|_| comment.body_storage.clone()),
        resolved: comment.resolved,
        anchor: comment.anchor.as_ref().and_then(|a| serde_json::to_string(a).ok()),
        synced_at: Some(now()),
    }
}

/// The Confluence marker an inline comment's anchor names, if it has one.
pub(super) fn marker_ref(record: &CommentRecord) -> Option<String> {
    let anchor: confed_api::InlineAnchor = serde_json::from_str(record.anchor.as_deref()?).ok()?;
    anchor.marker_ref
}

/// Whether two snapshots of a page's comments read the same: the same
/// comments, in the same threads, with the same text and resolved state.
/// Anchors are left out — where a comment sits is worked out locally.
pub(super) fn same_comments(before: &[CommentRecord], after: &[CommentRecord]) -> bool {
    let key = |records: &[CommentRecord]| {
        let mut keys: Vec<_> = records
            .iter()
            .map(|r| {
                (
                    r.comment_id.clone(),
                    r.parent_comment_id.clone(),
                    r.kind.clone(),
                    r.resolved,
                    r.body_storage.clone(),
                )
            })
            .collect();
        keys.sort();
        keys
    };
    key(before) == key(after)
}

/// Whether two snapshots of a page's attachments are the same files at the
/// same versions. Where confed's local copies stand is left out.
pub(super) fn same_attachments(before: &[AttachmentRecord], after: &[AttachmentRecord]) -> bool {
    let key = |records: &[AttachmentRecord]| {
        let mut keys: Vec<_> =
            records.iter().map(|r| (&r.attachment_id, &r.filename, r.version)).collect();
        keys.sort();
        keys.into_iter().map(|(id, file, v)| (id.clone(), file.clone(), v)).collect::<Vec<_>>()
    };
    key(before) == key(after)
}

/// A stored mark, if it can be searched from.
///
/// A mark well in the future was stamped by a clock that has since been set
/// back: how long ago the check really was is unknown, so it vouches for
/// nothing and every page is read again. One ahead by less than the overlap
/// the search adds anyway is ordinary drift, and the overlap covers it.
pub(super) fn trusted_mark(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let mark = chrono::DateTime::parse_from_rfc3339(value).ok()?.with_timezone(&chrono::Utc);
    let drift = chrono::Duration::minutes(CHECK_OVERLAP_MINUTES as i64);
    (mark <= chrono::Utc::now() + drift).then_some(mark)
}

/// Whole minutes from `mark` to now, rounded up; none for a mark slightly
/// ahead of the clock.
pub(super) fn minutes_since(mark: chrono::DateTime<chrono::Utc>) -> u64 {
    let seconds = (chrono::Utc::now() - mark).num_seconds().max(0) as u64;
    seconds.div_ceil(60)
}

/// Put cached attachment and comment snapshots back into the sync state.
pub(super) fn restore_extras(
    state: &crate::state::StateDb,
    page_id: &str,
    extras: &crate::pagestore::PageExtras,
) -> Result<()> {
    if state.page_attachments(page_id)?.is_empty() {
        let attachments: Vec<confed_api::Attachment> =
            serde_json::from_str(&extras.attachments).unwrap_or_default();
        for attachment in attachments {
            state.upsert_attachment(&AttachmentRecord {
                attachment_id: attachment.id.0.clone(),
                page_id: page_id.to_string(),
                filename: attachment.filename.clone(),
                media_type: attachment.media_type.clone(),
                file_size: attachment.file_size,
                version: attachment.version,
                sha256: None,
                downloaded: false,
            })?;
        }
    }

    if state.page_comments(page_id)?.is_empty() {
        let comments: Vec<Comment> = serde_json::from_str(&extras.comments).unwrap_or_default();
        for comment in comments {
            state.upsert_comment(&comment_record(page_id, &comment))?;
        }
    }
    Ok(())
}

impl SyncEngine {
    /// Refresh `.state.db`'s view of the server. Working files are untouched.
    pub async fn fetch(&self, ws: &mut Workspace, opts: &FetchOptions) -> Result<FetchOutcome> {
        Ok(self.fetch_tracking(ws, opts).await?.0)
    }

    /// [`Self::fetch`], also naming the pages whose attachments and comments
    /// this run read, so a pull that must re-read some does not ask twice.
    pub(super) async fn fetch_tracking(
        &self,
        ws: &mut Workspace,
        opts: &FetchOptions,
    ) -> Result<(FetchOutcome, ExtrasRead)> {
        let mut outcome = FetchOutcome::default();
        // Taken before anything is asked, so nothing that changes while this
        // fetch runs falls between it and the next one.
        let started = now();
        // Losing the cache costs bandwidth, not correctness, so a store that
        // will not open is a reason to fetch more, not to fail.
        let cache = ws.page_store().ok();
        let state = ws.state();

        let pending = state.pending_fetches()?;
        outcome.resumed = !pending.is_empty();

        // A page's version says nothing about its attachments or its comments,
        // so each has a mark of its own: when they were last known to be
        // current. The cache carries both too, for what is restored from it.
        let mut checks = [
            ExtraCheck::new(Extra::Attachments, state, cache.as_ref())?,
            ExtraCheck::new(Extra::Comments, state, cache.as_ref())?,
        ];

        self.progress.stage("Listing pages", None);
        let summaries = self.client.list_pages(&self.space).await?;
        let live: Vec<String> = summaries.iter().map(|s| s.id.0.clone()).collect();

        // What the listing says about every page goes in as one write: a
        // first fetch of a large space is thousands of rows, and a fetch
        // stopped halfway leaves the previous listing whole.
        let listed: Result<()> = state.atomically(|state| {
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
                let have_current_body = existing
                    .as_ref()
                    .is_some_and(|r| r.version == summary.version && r.storage_body.is_some());

                // Anything already downloaded at this version is served from the
                // cache, so the listing above is the only request its body costs.
                let cached_body = if have_current_body {
                    None
                } else {
                    cache
                        .as_ref()
                        .and_then(|c| c.body(&summary.id.0, summary.version).ok().flatten())
                };
                let needs_body = !have_current_body && cached_body.is_none();

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
                    storage_hash: cached_body.as_deref().map(hash_str),
                    storage_body: cached_body.clone(),
                    fetched_at: now(),
                    deleted: false,
                })?;

                if needs_body {
                    state.enqueue_fetch(&summary.id.0, &["body", "attachments", "comments"])?;
                    continue;
                }

                let voucher = if cached_body.is_some() {
                    outcome.from_cache += 1;
                    // Restore the snapshots taken alongside that body, so a rebuilt
                    // state database does not have to ask for those either. They are
                    // exactly as current as the cache says they are.
                    let extras = cache
                        .as_ref()
                        .and_then(|c| c.extras(&summary.id.0, summary.version).ok().flatten());
                    match extras {
                        Some(extras) => {
                            let own_attachments =
                                !state.page_attachments(&summary.id.0)?.is_empty();
                            let own_comments = !state.page_comments(&summary.id.0)?.is_empty();
                            restore_extras(state, &summary.id.0, &extras)?;
                            Voucher::Cache { own_attachments, own_comments }
                        }
                        None => Voucher::Nobody,
                    }
                } else if existing.as_ref().is_some_and(|r| r.deleted) {
                    // A page back in the listing after being gone from it — restored,
                    // or a restriction lifted — was out of the search's sight meanwhile.
                    Voucher::Nobody
                } else {
                    Voucher::State
                };
                for check in &mut checks {
                    check.place(&summary.id.0, &voucher);
                }
                outcome.unchanged += 1;
            }

            // Pages that vanished from the listing were deleted or trashed.
            for remote in state.remote_index()? {
                if !remote.deleted && !live.contains(&remote.page_id) {
                    state.mark_remote_deleted(&remote.page_id)?;
                    outcome.deleted_on_remote.push(remote.page_id);
                }
            }
            // Nothing is owed for a page that is gone: asking again would fail on
            // every fetch from here on.
            for (id, _) in state.pending_fetches()? {
                if !live.contains(&id) {
                    state.mark_fetch_done(&id)?;
                }
            }
            Ok(())
        });
        listed?;

        // Attachments and comments of the pages whose body is current. Adding
        // or changing either leaves the page's version alone, so the listing
        // cannot show it: ask the server which pages had one touched since the
        // mark, and read those again — and every page no mark vouches for.
        let whole_space = opts.pages.is_empty() && opts.since.is_none();
        let mut reread: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for check in &mut checks {
            let extra = check.extra;
            let mut again: BTreeSet<&String> = check.uncovered.iter().collect();
            check.checked = whole_space;
            if !opts.pages.is_empty() {
                // Pages asked for by name are read, not reasoned about.
                again.extend(&check.covered);
            } else if let (true, Some(mark)) =
                (whole_space && !check.covered.is_empty(), check.oldest_mark)
            {
                self.progress.stage(&format!("Checking {}", extra.name()), None);
                let minutes = minutes_since(mark) + CHECK_OVERLAP_MINUTES;
                let activity = match extra {
                    Extra::Attachments => {
                        self.client.recent_attachment_activity(&self.space, minutes).await
                    }
                    Extra::Comments => {
                        self.client.recent_comment_activity(&self.space, minutes).await
                    }
                };
                match activity {
                    Ok(confed_api::ContentActivity::Pages(pages)) => {
                        again.extend(
                            check.covered.iter().filter(|id| pages.iter().any(|p| p.0 == **id)),
                        );
                    }
                    Ok(confed_api::ContentActivity::Unbounded) => again.extend(&check.covered),
                    Err(e) => {
                        check.checked = false;
                        check.failed = Some(e.to_string());
                    }
                }
            }
            if !again.is_empty() && check.state_mark.is_none() && whole_space {
                tracing::info!(
                    target: "confed::sync",
                    pages = again.len(),
                    "reading every page's {} once: this workspace has not checked them before",
                    extra.name()
                );
            }
            for id in again {
                reread.entry(id.clone()).or_default().push(extra.name());
            }
        }
        // A page queued for its body is queued for everything on it; one
        // queued for less keeps what it is still owed.
        let queued: HashMap<String, Vec<String>> = state.pending_fetches()?.into_iter().collect();
        for (id, mut needs) in reread {
            let owed = queued.get(&id).map(Vec::as_slice).unwrap_or_default();
            if owed.iter().any(|n| n == "body") {
                continue;
            }
            for need in owed {
                if !needs.contains(&need.as_str()) {
                    needs.push(need);
                }
            }
            state.enqueue_fetch(&id, &needs)?;
        }
        // Everything these checks found is in the queue, which outlives an
        // interrupted run, so the marks can move now.
        for check in checks.iter().filter(|c| c.checked) {
            state.set_meta(check.extra.mark_key(), &started)?;
        }

        let work: Vec<FetchWork> = state
            .pending_fetches()?
            .into_iter()
            .map(|(page_id, needs)| FetchWork::owed(page_id, &needs))
            .collect();
        let titles: HashMap<String, String> =
            summaries.iter().map(|s| (s.id.0.clone(), s.title.clone())).collect();
        self.progress.stage("Fetching", Some(work.len()));
        let tally = self.run_fetches(ws, work, &titles).await?;
        outcome.fetched = tally.fetched;
        outcome.attachments_refreshed = tally.attachments_refreshed;
        outcome.attachments_changed = tally.attachments_changed;
        outcome.comments_refreshed = tally.comments_refreshed;
        outcome.comments_changed = tally.comments_changed;
        outcome.failed = tally.failed;

        // The cache's copies are as current as the state's once nothing is
        // left in the queue.
        if ws.state().pending_fetches()?.is_empty() {
            if let Ok(cache) = ws.page_store() {
                for check in checks.iter().filter(|c| c.checked) {
                    match check.extra {
                        Extra::Attachments => cache.set_attachments_checked_at(&started)?,
                        Extra::Comments => cache.set_comments_checked_at(&started)?,
                    }
                }
            }
        }
        let [attachments, comments] = checks;
        outcome.attachment_check_failed = attachments.failed;
        outcome.comment_check_failed = comments.failed;

        self.resolve_mentioned_users(ws).await?;
        ws.state().set_meta("last_fetch_at", &now())?;
        self.progress.finish();
        Ok((outcome, tally.read))
    }

    /// Fetch `work` concurrently and store each result as it lands.
    ///
    /// Fetch concurrently, write serially: the SQLite connection is not shared
    /// across tasks, and one writer keeps every page's write atomic.
    pub(super) async fn run_fetches(
        &self,
        ws: &mut Workspace,
        work: Vec<FetchWork>,
        titles: &HashMap<String, String>,
    ) -> Result<FetchTally> {
        let mut tally = FetchTally::default();
        let title_of = |id: &str| titles.get(id).cloned().unwrap_or_default();

        let mut inflight = FuturesUnordered::new();
        let mut work = work.into_iter();
        for _ in 0..self.concurrency {
            if let Some(item) = work.next() {
                inflight.push(self.fetch_one(item));
            }
        }
        while let Some(result) = inflight.next().await {
            // Once a stop is asked for, what is in flight is stored and nothing
            // new is started. What was not read stays queued for the next fetch.
            if !crate::interrupt::requested() {
                if let Some(item) = work.next() {
                    inflight.push(self.fetch_one(item));
                }
            }
            match result {
                Ok((id, Fetched::Page(fetched))) => {
                    let title = &fetched.page.summary.title;
                    self.progress.item(title);
                    self.store_fetched(ws, &id, &fetched)?;
                    tally.fetched += 1;
                    match &fetched.attachments {
                        Ok(_) => tally.read.attachments.push(id.clone()),
                        Err(error) => tally.failed.push(FailedPage {
                            page_id: id.clone(),
                            title: title.clone(),
                            error: format!(
                                "the page was fetched, but its attachments could not be listed \
                                 (the ones already here are kept): {error}"
                            ),
                        }),
                    }
                    match &fetched.comments {
                        Ok(_) => tally.read.comments.push(id),
                        Err(error) => tally.failed.push(FailedPage {
                            title: title.clone(),
                            error: format!(
                                "the page was fetched, but its comments could not be read \
                                 (the ones already here are kept): {error}"
                            ),
                            page_id: id,
                        }),
                    }
                }
                Ok((id, Fetched::Extras { attachments, comments })) => {
                    let title = title_of(&id);
                    self.progress.item(&title);
                    // One transaction for the page, as for a page read whole.
                    let began = ws.state().begin()?;
                    let stored: Result<()> = (|| {
                        let mut read: Vec<&str> = Vec::new();
                        let mut failed = |error: String| {
                            tally.failed.push(FailedPage {
                                page_id: id.clone(),
                                title: title.clone(),
                                error,
                            });
                        };
                        match attachments {
                            Some(Ok(listed)) => {
                                if self.refresh_attachments(ws, &id, &listed)? {
                                    tally.attachments_changed.push(id.clone());
                                }
                                tally.attachments_refreshed += 1;
                                tally.read.attachments.push(id.clone());
                                read.push(Extra::Attachments.name());
                            }
                            Some(Err(e)) => {
                                failed(format!("its attachments could not be listed again: {e}"));
                            }
                            None => {}
                        }
                        match comments {
                            Some(Ok(comments)) => {
                                if self.store_comments(ws, &id, &comments, true)? {
                                    tally.comments_changed.push(id.clone());
                                }
                                tally.comments_refreshed += 1;
                                tally.read.comments.push(id.clone());
                                read.push(Extra::Comments.name());
                            }
                            Some(Err(e)) => {
                                failed(format!("its comments could not be read again: {e}"));
                            }
                            None => {}
                        }
                        // Only what was read is settled: whatever failed, and a
                        // body still waited for, stay in the queue.
                        ws.state().mark_fetched(&id, &read)?;
                        Ok(())
                    })();
                    ws.state().finish(began, stored.is_ok())?;
                    stored?;
                }
                Err((item, e)) => {
                    let title = title_of(&item.page_id);
                    self.progress.item(&title);
                    tally.failed.push(FailedPage {
                        page_id: item.page_id,
                        title,
                        error: e.to_string(),
                    });
                }
            }
        }
        crate::interrupt::check(|| format!("{} page(s) fetched", tally.fetched))?;
        tally.attachments_changed.sort();
        tally.comments_changed.sort();
        Ok(tally)
    }

    pub(super) async fn fetch_one(
        &self,
        item: FetchWork,
    ) -> std::result::Result<(String, Fetched), (FetchWork, ConfedError)> {
        let page_id = PageId::new(&item.page_id);
        // A list that cannot be read is reported beside whatever else came
        // through, in the words the user would get for the request alone.
        let said = |e: confed_api::ApiError| ConfedError::from(e).to_string();
        let attachments = async { self.client.list_attachments(&page_id).await.map_err(said) };
        let comments = async { self.client.list_comments(&page_id).await.map_err(said) };

        if !item.body {
            let attachments = if item.attachments { Some(attachments.await) } else { None };
            let comments = if item.comments { Some(comments.await) } else { None };
            return Ok((item.page_id, Fetched::Extras { attachments, comments }));
        }
        match self.client.get_page(&page_id, BodyFormat::Storage).await {
            Ok(page) => {
                let fetched =
                    FetchedPage { page, attachments: attachments.await, comments: comments.await };
                Ok((item.page_id, Fetched::Page(Box::new(fetched))))
            }
            Err(e) => Err((item, e.into())),
        }
    }

    /// Everything read for one page goes into the state as one transaction:
    /// a body never lands without the attachments and comments read with it.
    pub(super) fn store_fetched(
        &self,
        ws: &mut Workspace,
        id: &str,
        fetched: &FetchedPage,
    ) -> Result<()> {
        ws.atomically(|ws| self.store_fetched_now(ws, id, fetched))
    }

    pub(super) fn store_fetched_now(
        &self,
        ws: &mut Workspace,
        id: &str,
        fetched: &FetchedPage,
    ) -> Result<()> {
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
        if let Ok(cache) = ws.page_store() {
            cache.put_body(id, summary.version, &fetched.page.body_storage)?;
        }

        // The body is in. A list that could not be read is still owed, and the
        // queue is what remembers that for the next fetch.
        let mut owed: Vec<&str> = Vec::new();
        match &fetched.attachments {
            Ok(attachments) => {
                self.store_attachments(ws, id, attachments)?;
            }
            Err(_) => owed.push(Extra::Attachments.name()),
        }
        match &fetched.comments {
            // The body moved, so every anchor is placed afresh.
            Ok(comments) => {
                self.store_comments(ws, id, comments, false)?;
            }
            Err(_) => owed.push(Extra::Comments.name()),
        }
        if owed.is_empty() {
            ws.state().mark_fetch_done(id)?;
        } else {
            ws.state().enqueue_fetch(id, &owed)?;
        }

        // The cache takes a snapshot only of what was read: one with a half
        // missing would restore as "this page has none".
        if let Ok(cache) = ws.page_store() {
            match (&fetched.attachments, &fetched.comments) {
                (Ok(attachments), Ok(comments)) => cache.put_extras(
                    id,
                    summary.version,
                    &crate::pagestore::PageExtras {
                        attachments: serde_json::to_string(attachments)?,
                        comments: serde_json::to_string(comments)?,
                    },
                )?,
                // The comments already cached are kept beside these, as the
                // ones in the state are.
                (Ok(attachments), Err(_)) => cache.put_attachments(
                    id,
                    summary.version,
                    &serde_json::to_string(attachments)?,
                )?,
                (Err(_), _) => {}
            }
        }
        Ok(())
    }

    /// Replace a page's stored attachments with what the server lists now, and
    /// say whether the list differs: a file added, replaced, renamed or gone.
    ///
    /// What confed knows about the copies in the sidecar is carried over: the
    /// hash of the one it wrote stays with the name, and that copy counts as
    /// downloaded only while the server still has the version it was. A name
    /// the server no longer lists is noted as removed, for the next pull to
    /// take the local copy away — and for a push in between not to upload it
    /// back.
    pub(super) fn store_attachments(
        &self,
        ws: &mut Workspace,
        id: &str,
        listed: &[confed_api::Attachment],
    ) -> Result<bool> {
        let state = ws.state();
        let before = state.page_attachments(id)?;
        let mut after: Vec<AttachmentRecord> = Vec::with_capacity(listed.len());
        for attachment in listed {
            // The name becomes a path in the sidecar. One that cannot be a
            // file there is not tracked: it could be neither downloaded nor,
            // later, safely removed.
            if !attachments::is_storable(&attachment.filename) {
                tracing::warn!(
                    target: "confed::sync",
                    page = %id, attachment = %attachment.id, name = %attachment.filename,
                    "this attachment's name cannot be a file in the sidecar; it is left out"
                );
                continue;
            }
            // One name, one file: a server that lists a name twice is not
            // followed into storing two.
            if after.iter().any(|a| a.filename == attachment.filename) {
                continue;
            }
            let same = before.iter().find(|b| b.attachment_id == attachment.id.0);
            // A file deleted and attached again is a new attachment under an
            // old name: the copy in the sidecar is still the old one's.
            let known = same.or_else(|| {
                before.iter().find(|b| {
                    b.filename == attachment.filename
                        && !listed.iter().any(|a| a.id.0 == b.attachment_id)
                })
            });
            let current = same.is_some_and(|k| {
                k.downloaded && k.version == attachment.version && k.filename == attachment.filename
            });
            after.push(AttachmentRecord {
                attachment_id: attachment.id.0.clone(),
                page_id: id.to_string(),
                filename: attachment.filename.clone(),
                media_type: attachment.media_type.clone(),
                file_size: attachment.file_size,
                version: attachment.version,
                sha256: known.and_then(|k| k.sha256.clone()),
                downloaded: current,
            });
        }

        // All or nothing: half of this would lose which copies are confed's.
        state.atomically(|state| {
            state.clear_page_attachments(id)?;
            for record in &after {
                state.upsert_attachment(record)?;
                state.forget_removed_attachment(id, &record.filename)?;
            }
            // A name that could never be a file in the sidecar has no copy
            // there to remove: what it points at is something else's.
            for gone in before.iter().filter(|b| {
                attachments::is_storable(&b.filename)
                    && !after.iter().any(|a| a.filename == b.filename)
            }) {
                state.note_removed_attachment(id, &gone.filename, gone.sha256.as_deref())?;
            }
            Ok(())
        })?;
        Ok(!same_attachments(&before, &after))
    }

    /// [`Self::store_attachments`] for a list read without its page: the
    /// snapshot the cache took at the page's version follows.
    pub(super) fn refresh_attachments(
        &self,
        ws: &mut Workspace,
        id: &str,
        listed: &[confed_api::Attachment],
    ) -> Result<bool> {
        let changed = self.store_attachments(ws, id, listed)?;
        if let (Ok(cache), Some(remote)) = (ws.page_store(), ws.state().get_remote(id)?) {
            cache.refresh_attachments(id, remote.version, &serde_json::to_string(listed)?)?;
        }
        Ok(changed)
    }

    /// Replace a page's stored comments with what the server has now, and say
    /// whether a reader would see the difference.
    ///
    /// `same_body` is for comments read without their page: the body has not
    /// moved, so where confed last placed each anchor in it still holds, and is
    /// kept rather than recomputed from the server's bare selection.
    pub(super) fn store_comments(
        &self,
        ws: &mut Workspace,
        id: &str,
        comments: &[Comment],
        same_body: bool,
    ) -> Result<bool> {
        let state = ws.state();
        let before = state.page_comments(id)?;
        let mut after: Vec<CommentRecord> =
            comments.iter().map(|c| comment_record(id, c)).collect();
        if same_body {
            for record in &mut after {
                let kept = before
                    .iter()
                    .find(|b| b.comment_id == record.comment_id)
                    .filter(|b| marker_ref(b).is_some() && marker_ref(b) == marker_ref(record));
                if let Some(kept) = kept {
                    record.anchor = kept.anchor.clone();
                }
            }
        }

        // Replaced whole or not at all: a half-written set would lose threads.
        state.atomically(|state| {
            state.clear_page_comments(id)?;
            for record in &after {
                state.upsert_comment(record)?;
            }
            Ok(())
        })?;

        if same_body {
            if let (Ok(cache), Some(remote)) = (ws.page_store(), ws.state().get_remote(id)?) {
                cache.put_comments(id, remote.version, &serde_json::to_string(comments)?)?;
            }
        }
        Ok(!same_comments(&before, &after))
    }

    /// Read one page's comments from the server again and bring its comment
    /// files — the sidecar and the marks in the page body — in line with them.
    /// Unsent drafts and resolve requests are kept. Returns whether the
    /// comments had changed.
    pub async fn refresh_comments(&self, ws: &mut Workspace, page_id: &str) -> Result<bool> {
        let record = ws
            .state()
            .get_page(page_id)?
            .ok_or_else(|| ConfedError::NotFound(format!("no page {page_id} in this workspace")))?;
        let comments = self.client.list_comments(&PageId::new(page_id)).await?;
        self.store_comments(ws, page_id, &comments, true)?;
        self.write_comments_sidecar(ws, page_id, &record.title, &record.local_path, true)
    }

    /// Look up everyone mentioned in the fetched bodies who is not cached yet.
    ///
    /// Mentions are opaque ids in the markup, so this is what lets them render
    /// as `[@Name](profile)`. A lookup that fails — a deleted account, or a
    /// permission confed does not have — is left unresolved rather than fatal:
    /// those blocks stay as Confluence markup, which is honest and lossless.
    pub(super) async fn resolve_mentioned_users(&self, ws: &mut Workspace) -> Result<()> {
        let mut wanted: Vec<(String, String)> = Vec::new();
        for remote in ws.state().all_remote()? {
            let Some(body) = &remote.storage_body else { continue };
            for (attr, id) in confed_converter::user_references(body) {
                if !wanted.iter().any(|(_, existing)| existing == &id) {
                    wanted.push((attr, id));
                }
            }
        }

        let mut unknown = Vec::new();
        for (attr, id) in wanted {
            if ws.state().get_user(&id)?.is_none() {
                unknown.push((attr, id));
            }
        }
        if !unknown.is_empty() {
            self.progress.stage("Looking up mentioned people", Some(unknown.len()));
        }
        for (attr, id) in unknown {
            self.progress.item(&id);
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
}
