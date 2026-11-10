//! Comment work in a push: drafts posted, replies, resolves and edits, and
//! the selection an inline comment is anchored to.

use super::*;

/// Inline-comment markers in a page's base copy that no comment confed knows
/// claims, as `(ref, text)` — usually left by deleted comments, which Data
/// Center does not unwrap.
pub fn orphan_markers(ws: &Workspace, page_id: &str) -> Result<Vec<(String, String)>> {
    let Some(page) = ws.state().get_page(page_id)? else { return Ok(Vec::new()) };
    let known: Vec<String> = ws
        .state()
        .page_comments(page_id)?
        .iter()
        .filter_map(|c| c.anchor.as_deref())
        .filter_map(|a| serde_json::from_str::<confed_api::InlineAnchor>(a).ok())
        .filter_map(|a| a.marker_ref)
        .collect();
    Ok(confed_converter::selection::marker_refs(&page.storage_body)?
        .into_iter()
        .filter(|(r, _)| !known.contains(r))
        .collect())
}

/// The root of the thread `comment` is in.
pub(super) fn thread_root(
    ws: &Workspace,
    page_id: &str,
    comment: &confed_api::CommentId,
) -> Result<confed_api::CommentId> {
    let all = ws.state().page_comments(page_id)?;
    let mut current = comment.0.clone();
    for _ in 0..all.len() {
        match all.iter().find(|c| c.comment_id == current).and_then(|c| c.parent_comment_id.clone())
        {
            Some(parent) => current = parent,
            None => break,
        }
    }
    Ok(confed_api::CommentId::new(current))
}

/// A comment confed knows, and the page it is on.
pub(super) fn find_comment(
    ws: &Workspace,
    comment_id: &str,
) -> Result<(PageRecord, CommentRecord)> {
    for page in ws.state().all_pages()? {
        if let Some(c) = ws
            .state()
            .page_comments(&page.page_id)?
            .into_iter()
            .find(|c| c.comment_id == comment_id)
        {
            return Ok((page, c));
        }
    }
    Err(ConfedError::NotFound(format!(
        "no comment {comment_id}; `confed pull` refreshes comments, `confed comment list <page>` shows them"
    )))
}

pub(super) fn comment_kind(record: &CommentRecord) -> confed_api::CommentKind {
    if record.kind == "inline" {
        confed_api::CommentKind::Inline
    } else {
        confed_api::CommentKind::Footer
    }
}

/// Comment work a page has not sent yet: `new` marks in its file, drafts and
/// resolve requests in its sidecar.
pub fn pending_comment_work(ws: &Workspace, page_path: &str) -> usize {
    let body = std::fs::read_to_string(ws.absolute(page_path))
        .ok()
        .and_then(|c| crate::frontmatter::parse(&c, page_path).ok())
        .map_or(0, |f| f.drafts().count());
    let sidecar = std::fs::read_to_string(
        ws.absolute(&paths::sidecar_for(page_path)).join(comments::COMMENTS_FILENAME),
    )
    .ok()
    .and_then(|t| comments::parse(&t).ok())
    .map_or(0, |s| s.drafts().count() + s.resolve_requests.len());
    body + sidecar
}

/// Whether a page's sidecar already shows the comments in the state. What a
/// dry run goes by: the answer [`SyncEngine::write_comments_sidecar`] would
/// give, without writing anything.
pub(super) fn sidecar_is_current(ws: &Workspace, page_id: &str, page_path: &str) -> Result<bool> {
    let records = ws.state().page_comments(page_id)?;
    let existing = std::fs::read_to_string(
        ws.absolute(&paths::sidecar_for(page_path)).join(comments::COMMENTS_FILENAME),
    )
    .ok()
    .and_then(|text| comments::parse(&text).ok())
    .unwrap_or_default();
    Ok(comments::is_current(&existing, &records))
}

/// The page file's stripped body, if it can be read.
pub(super) fn local_body(ws: &Workspace, path: &str) -> Option<String> {
    let content = std::fs::read_to_string(ws.absolute(path)).ok()?;
    crate::frontmatter::parse(&content, path).ok().map(|f| f.body)
}

/// How many times `text` occurs in the server's page text.
pub(super) fn server_occurrences(storage: &str, text: &str) -> usize {
    match confed_converter::selection::select(storage, text, Some(usize::MAX)) {
        Ok(Err(confed_converter::selection::SelectionError::OutOfRange { count })) => count,
        Ok(Ok(sel)) => sel.match_count,
        _ => 0,
    }
}

/// Where `text` sits in the server's copy of a page, as a create request must
/// state it — or why it cannot be commented on.
///
/// `local` is the page file's body: text that is in the file but not on the
/// server is an unpushed edit, which is the user's to push, not a typo.
pub fn server_selection(
    storage: &str,
    text: &str,
    occurrence: Option<usize>,
    local: Option<&str>,
    path: &str,
) -> Result<confed_api::InlineAnchor> {
    use confed_converter::selection::{select, SelectionError};
    match select(storage, text, occurrence)? {
        Ok(sel) => Ok(confed_api::InlineAnchor {
            text: sel.text,
            match_index: Some(sel.match_index),
            match_count: Some(sel.match_count),
            ..Default::default()
        }),
        Err(SelectionError::NotFound { .. })
            if local.is_some_and(|body| marks::plain_text(body).contains(text.trim())) =>
        {
            Err(ConfedError::state_with_hint(
                format!("\"{text}\" is in {path} but not in the page on the server"),
                "push the page first (`confed push`); Confluence checks the selection \
                 against its own copy of the page",
            ))
        }
        Err(e) => Err(selection_error(e, text, path)),
    }
}

/// The exit a selection problem deserves: 6 for text that is not there, 2 for
/// a request that has to say more.
pub fn selection_error(
    e: confed_converter::selection::SelectionError,
    text: &str,
    path: &str,
) -> ConfedError {
    use confed_converter::selection::SelectionError;
    match e {
        SelectionError::NotFound { in_macro: true } => ConfedError::NotFound(format!(
            "\"{text}\" in {path} is only inside a macro or code block, where Confluence \
             cannot anchor a comment"
        )),
        SelectionError::NotFound { in_macro: false } => {
            ConfedError::NotFound(format!("the text \"{text}\" does not appear in {path}"))
        }
        SelectionError::Ambiguous { contexts } => ConfedError::usage_with_hint(
            format!(
                "the text \"{text}\" appears {} times in {path}:\n  {}",
                contexts.len(),
                contexts
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{}: {c}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n  ")
            ),
            "pick one with --occurrence N, or include more surrounding words",
        ),
        SelectionError::OutOfRange { count } => ConfedError::usage(format!(
            "--occurrence is out of range: \"{text}\" appears {count} time(s) in {path}"
        )),
        SelectionError::Invalid => ConfedError::usage(
            "an inline comment's text must be non-empty and within one paragraph",
        ),
    }
}

impl SyncEngine {
    pub(super) async fn push_comments(
        &self,
        ws: &mut Workspace,
        opts: &PushOptions,
    ) -> Result<CommentWork> {
        let mut work = CommentWork::default();
        for record in ws.state().all_pages()? {
            if !opts.covers(&record.page_id, &record.local_path) {
                continue;
            }
            let path = ws
                .absolute(&paths::sidecar_for(&record.local_path))
                .join(comments::COMMENTS_FILENAME);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let mut sidecar = comments::parse(&text)?;
            if sidecar.drafts().next().is_none() && sidecar.resolve_requests.is_empty() {
                continue;
            }
            let page_id = PageId::new(&record.page_id);
            if let Some(failure) = self.page_gone(&record).await? {
                work.failed.push(failure);
                continue;
            }

            while let Some(index) = sidecar.comments.iter().position(|c| c.is_draft()) {
                let draft = sidecar.comments[index].clone();
                let storage = confed_converter::markdown_to_storage(
                    &draft.body,
                    &comment_convert_options(ws, &record.local_path)?,
                )?;
                let posted = match &draft.anchor {
                    Some(anchor) if draft.kind == comments::SidecarKind::Inline => {
                        self.require_inline_create()?;
                        let occurrence = anchor.match_index.map(|i| i + 1);
                        let page = self.client.get_page(&page_id, BodyFormat::Storage).await?;
                        let local = local_body(ws, &record.local_path);
                        let selection = server_selection(
                            &page.body_storage,
                            &anchor.text,
                            occurrence,
                            local.as_deref(),
                            &record.local_path,
                        )?;
                        let posted =
                            self.client.add_inline_comment(&page_id, &selection, &storage).await?;
                        ws.state().upsert_comment(&comment_record(&record.page_id, &posted))?;
                        self.adopt_comment_version(ws, &record.page_id, &posted).await?;
                        posted
                    }
                    _ => {
                        let parent = draft.reply_to.as_deref().map(confed_api::CommentId::new);
                        let parent_is_inline = parent.as_ref().is_some_and(|p| {
                            ws.state().page_comments(&record.page_id).is_ok_and(|all| {
                                all.iter().any(|c| c.comment_id == p.0 && c.kind == "inline")
                            })
                        });
                        let posted = match &parent {
                            // Inline threads are one level deep: a reply to a
                            // reply goes to the thread, as the web UI does it.
                            Some(parent) if parent_is_inline => {
                                let root = thread_root(ws, &record.page_id, parent)?;
                                self.client.add_inline_reply(&page_id, &root, &storage).await?
                            }
                            _ => {
                                self.client
                                    .add_footer_comment(&page_id, &storage, parent.as_ref())
                                    .await?
                            }
                        };
                        ws.state().upsert_comment(&comment_record(&record.page_id, &posted))?;
                        posted
                    }
                };
                if posted.parent_comment_id.is_some() {
                    work.replies.push(posted.id.0.clone());
                } else {
                    work.added.push(posted.id.0.clone());
                }
                // Posted: out of the sidecar now, so a failure later in this
                // push cannot post it twice on the retry.
                sidecar.comments.remove(index);
                self.rewrite_sidecar(ws, &record, &path, &sidecar)?;
            }

            while let Some(id) = sidecar.resolve_requests.first().cloned() {
                if !self.client.capabilities().comment_resolve {
                    return Err(ConfedError::Unsupported(format!(
                        "resolving comments is not available on Confluence {}",
                        self.client.flavor()
                    )));
                }
                self.client.resolve_comment(&confed_api::CommentId::new(&id)).await?;
                if let Some(mut c) = ws
                    .state()
                    .page_comments(&record.page_id)?
                    .into_iter()
                    .find(|c| c.comment_id == id)
                {
                    c.resolved = true;
                    ws.state().upsert_comment(&c)?;
                }
                work.resolved.push(id.clone());
                sidecar.resolve_requests.remove(0);
                self.rewrite_sidecar(ws, &record, &path, &sidecar)?;
            }
            // The resolved threads' marks leave the body.
            sync_marks(ws, &record.page_id, &record.local_path)?;
        }

        // Page files confed does not track (a page removed with `confed rm`
        // and the file put back, say) are not posted to — but their comment
        // work is reported, not dropped: the dry run lists it too.
        let (files, _) = worktree::read_working_files(ws)?;
        for local in &files {
            let Some(page_id) = local.file.frontmatter.page_id() else { continue };
            if ws.state().get_page(page_id)?.is_some() || !opts.covers(page_id, &local.path) {
                continue;
            }
            let sidecar = std::fs::read_to_string(
                ws.absolute(&paths::sidecar_for(&local.path)).join(comments::COMMENTS_FILENAME),
            )
            .ok()
            .and_then(|t| comments::parse(&t).ok());
            let has_work = local.file.drafts().next().is_some()
                || sidecar
                    .is_some_and(|s| s.drafts().next().is_some() || !s.resolve_requests.is_empty());
            if !has_work {
                continue;
            }
            let exists =
                match self.client.get_page(&PageId::new(page_id), BodyFormat::Storage).await {
                    Ok(_) => true,
                    Err(confed_api::ApiError::NotFound(_)) => false,
                    Err(e) => return Err(e.into()),
                };
            work.failed.push(FailedPage {
                page_id: page_id.to_string(),
                title: local.file.frontmatter.title.clone(),
                error: if exists {
                    format!(
                        "{}: this file is not tracked by the workspace, so its comment drafts \
                         were not posted; `confed pull` picks the page up again",
                        local.path
                    )
                } else {
                    format!(
                        "{}: the page no longer exists on the server (deleted?); its comment \
                         drafts are kept",
                        local.path
                    )
                },
            });
        }

        // Drafts written into page bodies as `new` marks.
        for record in ws.state().all_pages()? {
            if work.failed.iter().any(|f| f.page_id == record.page_id)
                || !opts.covers(&record.page_id, &record.local_path)
            {
                continue;
            }
            match self.push_body_drafts(ws, &record).await {
                Ok(added) => work.added.extend(added),
                Err(ConfedError::NotFound(_)) if self.page_gone(&record).await?.is_some() => {
                    work.failed.extend(self.page_gone(&record).await?);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(work)
    }

    /// What `--show-storage` reports: the storage each planned page body and
    /// each queued comment would be sent as, built exactly as push builds it.
    pub(super) fn storage_previews(
        &self,
        ws: &Workspace,
        opts: &PushOptions,
        plan: &PushPlan,
    ) -> Result<Vec<StoragePreview>> {
        let mut out = Vec::new();
        for op in &plan.ops {
            let body_changes = op.kind == PushKind::Create || op.ops.iter().any(|o| o == "body");
            if op.kind == PushKind::Delete || !body_changes {
                continue;
            }
            let content = std::fs::read_to_string(ws.absolute(&op.path))
                .map_err(|e| ConfedError::io(format!("reading {}", op.path), e))?;
            let file = crate::frontmatter::parse(&content, &op.path)?;
            let convert_opts = page_convert_options(ws, &op.path)?;
            let base =
                op.page_id.as_deref().map(|id| ws.state().get_page(id)).transpose()?.flatten();
            let storage = match base {
                Some(base) => self.build_storage(&base, &file.marked_body(), &convert_opts)?,
                None => confed_converter::markdown_to_storage(&file.body, &convert_opts)?,
            };
            out.push(StoragePreview {
                page_id: op.page_id.clone().unwrap_or_default(),
                path: op.path.clone(),
                what: "page".into(),
                storage,
            });
        }
        if !opts.with_comments {
            return Ok(out);
        }
        for record in ws.state().all_pages()? {
            if !opts.covers(&record.page_id, &record.local_path)
                || plan.comment_failures.iter().any(|f| f.page_id == record.page_id)
            {
                continue;
            }
            let convert_opts = comment_convert_options(ws, &record.local_path)?;
            let mut preview = |what: String, markdown: &str| -> Result<()> {
                out.push(StoragePreview {
                    page_id: record.page_id.clone(),
                    path: record.local_path.clone(),
                    what,
                    storage: confed_converter::markdown_to_storage(markdown, &convert_opts)?,
                });
                Ok(())
            };
            let sidecar = std::fs::read_to_string(
                ws.absolute(&paths::sidecar_for(&record.local_path))
                    .join(comments::COMMENTS_FILENAME),
            )
            .ok()
            .and_then(|t| comments::parse(&t).ok());
            for draft in sidecar.iter().flat_map(|s| s.drafts()) {
                let what = match (&draft.anchor, &draft.reply_to) {
                    (Some(a), _) => format!("inline comment on \"{}\"", a.text),
                    (_, Some(parent)) => format!("reply to {parent}"),
                    _ => "comment".into(),
                };
                preview(what, &draft.body)?;
            }
            if let Ok(content) = std::fs::read_to_string(ws.absolute(&record.local_path)) {
                if let Ok(file) = crate::frontmatter::parse(&content, &record.local_path) {
                    for draft in file.drafts() {
                        let Some((start, end)) = draft_span(&file.body, draft) else { continue };
                        let what = format!(
                            "inline comment on \"{}\"",
                            marks::plain_text(&file.body[start..end]).trim()
                        );
                        preview(what, draft.draft_body().unwrap_or_default())?;
                    }
                }
            }
        }
        Ok(out)
    }

    /// A page that has comment work but no longer exists on the server: a
    /// failure that says so, rather than whatever the comment endpoint answers.
    pub(super) async fn page_gone(&self, record: &PageRecord) -> Result<Option<FailedPage>> {
        match self.client.get_page(&PageId::new(&record.page_id), BodyFormat::Storage).await {
            Ok(_) => Ok(None),
            Err(confed_api::ApiError::NotFound(_)) => Ok(Some(FailedPage {
                page_id: record.page_id.clone(),
                title: record.title.clone(),
                error: deleted_page_message(&record.local_path),
            })),
            Err(e) => Err(e.into()),
        }
    }

    /// Replace a posted comment's body on the server, then here.
    pub async fn edit_comment(
        &self,
        ws: &mut Workspace,
        comment_id: &str,
        body_markdown: &str,
    ) -> Result<()> {
        let (page, mut record) = find_comment(ws, comment_id)?;
        let storage = confed_converter::markdown_to_storage(
            body_markdown,
            &comment_convert_options(ws, &page.local_path)?,
        )?;
        self.client
            .update_comment(
                &confed_api::CommentId::new(comment_id),
                comment_kind(&record),
                &storage,
            )
            .await?;
        record.body_storage = Some(storage);
        record.body_markdown = body_markdown.trim().to_string();
        ws.state().upsert_comment(&record)?;
        self.refresh_comment_files(ws, &page)
    }

    /// Delete a posted comment, with its replies, on the server, then here.
    /// An inline comment's marker may stay in the page on Data Center; `comment
    /// list` reports such markers as `orphan_markers`.
    /// Returns the ids of the replies that went with it.
    pub async fn delete_comment(
        &self,
        ws: &mut Workspace,
        comment_id: &str,
    ) -> Result<Vec<String>> {
        let (page, record) = find_comment(ws, comment_id)?;
        self.client
            .delete_comment(&confed_api::CommentId::new(comment_id), comment_kind(&record))
            .await?;
        // The whole thread below it, replies to replies included.
        let all = ws.state().page_comments(&page.page_id)?;
        let mut gone = vec![comment_id.to_string()];
        let mut replies = Vec::new();
        while let Some(reply) = all.iter().find(|c| {
            c.parent_comment_id.as_ref().is_some_and(|p| gone.contains(p))
                && !gone.contains(&c.comment_id)
        }) {
            gone.push(reply.comment_id.clone());
            replies.push(reply.comment_id.clone());
        }
        for id in &gone {
            ws.state().delete_comment(id)?;
        }
        self.refresh_comment_files(ws, &page)?;
        Ok(replies)
    }

    /// Rewrite a page's sidecar and mark layer from the database, keeping
    /// unpushed drafts.
    pub(super) fn refresh_comment_files(
        &self,
        ws: &mut Workspace,
        page: &PageRecord,
    ) -> Result<()> {
        let path =
            ws.absolute(&paths::sidecar_for(&page.local_path)).join(comments::COMMENTS_FILENAME);
        let sidecar = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| comments::parse(&t).ok())
            .unwrap_or_default();
        self.rewrite_sidecar(ws, page, &path, &sidecar)?;
        sync_marks(ws, &page.page_id, &page.local_path)
    }

    pub(super) fn require_inline_create(&self) -> Result<()> {
        if self.client.capabilities().inline_comment_create {
            Ok(())
        } else {
            Err(ConfedError::Unsupported(format!(
                "creating inline comments is not available on Confluence {}",
                self.client.flavor()
            )))
        }
    }

    /// Write the sidecar back from the database plus the drafts and resolve
    /// requests still waiting.
    pub(super) fn rewrite_sidecar(
        &self,
        ws: &Workspace,
        record: &PageRecord,
        path: &Path,
        sidecar: &comments::Sidecar,
    ) -> Result<()> {
        let records = ws.state().page_comments(&record.page_id)?;
        let drafts: Vec<comments::SidecarComment> = sidecar.drafts().cloned().collect();
        let text = comments::render_with_requests(
            &record.page_id,
            &record.title,
            &records,
            &drafts,
            &sidecar.resolve_requests,
        );
        write_atomic(path, &text)
    }

    /// Post every `new` mark in a page file as an inline comment, rewriting
    /// each mark with its id as soon as it is posted so a failure part-way is
    /// safe to retry.
    pub(super) async fn push_body_drafts(
        &self,
        ws: &mut Workspace,
        record: &PageRecord,
    ) -> Result<Vec<String>> {
        let path = record.local_path.clone();
        let abs = ws.absolute(&path);
        let Ok(content) = std::fs::read_to_string(&abs) else { return Ok(Vec::new()) };
        let Ok(file) = crate::frontmatter::parse(&content, &path) else {
            return Ok(Vec::new());
        };
        if file.drafts().next().is_none() {
            return Ok(Vec::new());
        }
        validate_drafts(&file, &path)?;
        self.require_inline_create()?;

        let mut added = Vec::new();
        loop {
            // Re-read each round: adopting a new page version may re-render
            // the file, which moves every later draft.
            let content = std::fs::read_to_string(&abs)
                .map_err(|e| ConfedError::io(format!("reading {path}"), e))?;
            let mut file = crate::frontmatter::parse(&content, &path)?;
            let Some(draft) = file.drafts().find(|d| d.end.is_some()).cloned() else { break };
            let Some((start, end)) = draft_span(&file.body, &draft) else { break };
            let base = ws.state().get_page(&record.page_id)?.unwrap_or_else(|| record.clone());
            refuse_unpushed_block(ws, &base, &file.body, start, end, &path)?;

            let local = draft_anchor(&file.body, start, end);
            let page =
                self.client.get_page(&PageId::new(&record.page_id), BodyFormat::Storage).await?;
            let anchor = server_selection(
                &page.body_storage,
                &local.text,
                Some(local.match_index.unwrap_or(0) + 1),
                Some(&file.body),
                &path,
            )
            .and_then(|sel| {
                let loose = local.match_count.unwrap_or(1);
                let on_server = server_occurrences(&page.body_storage, &local.text);
                if on_server != loose {
                    return Err(ConfedError::state_with_hint(
                        format!(
                            "{path}: line {}: \"{}\" occurs {loose} time(s) in the file but \
                             {on_server} on the server, so which one the mark means is unclear",
                            draft.line, local.text
                        ),
                        "push the page first, or comment from comments.md with \
                         `confed comment add --sidecar --occurrence N`",
                    ));
                }
                Ok(confed_api::InlineAnchor {
                    context_before: local.context_before.clone(),
                    context_after: local.context_after.clone(),
                    ..sel
                })
            })?;

            let body = draft.draft_body().unwrap_or_default();
            let storage =
                confed_converter::markdown_to_storage(body, &comment_convert_options(ws, &path)?)?;
            let posted = self
                .client
                .add_inline_comment(&PageId::new(&record.page_id), &anchor, &storage)
                .await?;
            let new_record = comment_record(&record.page_id, &posted);
            ws.state().upsert_comment(&new_record)?;

            if let Some(m) = file
                .marks
                .iter_mut()
                .find(|m| m.id.is_new() && m.start == draft.start && m.end == draft.end)
            {
                m.id = MarkId::Comment(posted.id.0.clone());
                m.note = mark_preview(&new_record, &[], marks_mode(ws));
            }
            rewrite_body(&abs, &content, &file.marked_body())?;
            added.push(posted.id.0.clone());
            self.adopt_comment_version(ws, &record.page_id, &posted).await?;
        }
        Ok(added)
    }

    /// Bring the base up to date with the marker a new inline comment put
    /// into the server's body.
    ///
    /// Cloud adds the marker without a version bump; Data Center saves a new
    /// page version. Either way the base must carry the marker, or the next
    /// push of an unrelated edit would copy stale bytes for the untouched block
    /// and the server would orphan the thread. A new version is adopted only
    /// when, with the new comment's marker taken out, it reads the same as the
    /// base — anything else is somebody else's edit, which `pull` merges.
    pub(super) async fn adopt_comment_version(
        &self,
        ws: &mut Workspace,
        page_id: &str,
        posted: &Comment,
    ) -> Result<()> {
        let page = self.client.get_page(&PageId::new(page_id), BodyFormat::Storage).await?;
        let Some(base) = ws.state().get_page(page_id)? else { return Ok(()) };
        if page.body_storage == base.storage_body && page.summary.version == base.version {
            return Ok(());
        }
        let path = base.local_path.clone();
        let opts = page_convert_options(ws, &path)?;
        if page.summary.version != base.version {
            let marker = posted.anchor.as_ref().and_then(|a| a.marker_ref.as_deref());
            let unmarked = match marker {
                Some(r) => confed_converter::selection::strip_marker(&page.body_storage, r)?,
                None => page.body_storage.clone(),
            };
            let same = unmarked == base.storage_body
                || comparable_markdown(&unmarked, &opts)?
                    == comparable_markdown(&base.storage_body, &opts)?;
            if !same || page.summary.version != base.version + 1 {
                tracing::warn!(
                    target: "confed::sync",
                    page = %page_id, base = base.version, remote = page.summary.version,
                    "the page changed on the server besides the new comment; run `confed pull`"
                );
                return Ok(());
            }
        }

        let converted = confed_converter::storage_to_markdown(&page.body_storage, &opts)?;
        let mut updated = base.clone();
        updated.storage_body = page.body_storage.clone();
        updated.storage_hash = hash_str(&page.body_storage);
        updated.version = page.summary.version;
        updated.updated_at = page.summary.updated_at.clone().or(updated.updated_at);
        updated.block_map = serde_json::to_string(&converted.block_map).ok();
        updated.render_key = render_key(&page.body_storage, &opts.users);

        // The file follows: its frontmatter names the new version, and when it
        // carries no local edits its body is re-rendered from the new storage
        // so it and the base agree exactly.
        let abs = ws.absolute(&path);
        if let Ok(content) = std::fs::read_to_string(&abs) {
            if let Ok(mut file) = crate::frontmatter::parse(&content, &path) {
                let untouched = file.content_hash() == base.markdown_hash;
                if let Some(managed) = file.frontmatter.managed.as_mut() {
                    managed.version = page.summary.version;
                    managed.updated = page.summary.updated_at.clone().or(managed.updated.take());
                }
                if untouched {
                    // Comment marks come from the render; drafts still
                    // waiting to be posted are carried over.
                    file.marks.retain(|m| m.id.is_new());
                    file.set_body(converted.markdown);
                    updated.markdown_hash = file.content_hash();
                }
                write_atomic(&abs, &file.render()?)?;
            }
        }
        ws.state().upsert_page(&updated)?;
        if let Some(mut remote) = ws.state().get_remote(page_id)? {
            remote.version = page.summary.version;
            remote.storage_body = Some(page.body_storage.clone());
            remote.storage_hash = Some(hash_str(&page.body_storage));
            ws.state().upsert_remote(&remote)?;
        }
        if let Ok(cache) = ws.page_store() {
            cache.put_body(page_id, page.summary.version, &page.body_storage)?;
        }
        self.write_storage_copy(ws, &path, &page.body_storage)
    }
}
