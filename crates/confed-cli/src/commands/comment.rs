//! `confed comment` — a structured accessor over the `comments.md` sidecar.
//!
//! Every write goes through the sidecar file, which stays the source of truth,
//! so the same edits can be made with a text editor or by an agent.

use crate::cli::CommentCommand;
use crate::context::Context;
use crate::output::Output;
use confed_converter::MarkId;
use confed_core::comments::{self, SidecarComment, SidecarKind};
use confed_core::error::{ConfedError, Result};
use confed_core::frontmatter::MarkdownFile;
use confed_core::paths;
use confed_core::sync::{write_atomic, PushOptions};
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, command: &CommentCommand) -> Result<Output> {
    match command {
        CommentCommand::List { page, unresolved, inline } => match ctx.resolve_page(page) {
            Ok(_) => list(ctx, page, *unresolved, *inline),
            // Not in the workspace — removed with `confed rm`, or never
            // pulled: an id can still be asked of the server.
            Err(_) if ctx.page_id_arg(page).is_ok() => {
                let id = ctx.page_id_arg(page)?;
                list_live(ctx, &id, *unresolved, *inline).await
            }
            Err(e) => Err(e),
        },
        CommentCommand::Add { page, body, anchor, occurrence, sidecar, push } => {
            let placement = AnchorPlacement { occurrence: *occurrence, sidecar: *sidecar };
            add(ctx, page, body.as_deref(), anchor.as_deref(), placement, None, *push).await
        }
        CommentCommand::Reply { comment_ids, body, push } => {
            reply(ctx, comment_ids, body, *push).await
        }
        CommentCommand::Resolve { comment_ids, all, push } => {
            resolve(ctx, comment_ids, all.as_deref(), *push).await
        }
        CommentCommand::Edit { comment_id, body } => edit_comment(ctx, comment_id, body).await,
        CommentCommand::Rm { comment_ids } => remove(ctx, comment_ids).await,
    }
}

/// Push the comment work of `pages` now — nothing else: no page edits, no
/// attachments, no other page's drafts.
async fn push_now(ctx: &mut Context, pages: Vec<String>) -> Result<confed_core::sync::PushOutcome> {
    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    engine.push(ws, &comment_push(pages)).await
}

fn comment_push(pages: Vec<String>) -> PushOptions {
    PushOptions { scope: pages, with_comments: true, comments_only: true, ..Default::default() }
}

/// A page whose comment work failed — deleted on the server, say — makes the
/// command a partial success (exit 8) that names it.
fn with_failures(mut output: Output, failed: &[confed_core::sync::FailedPage]) -> Output {
    for failure in failed {
        output = output.warn(failure.error.clone());
    }
    if !failed.is_empty() {
        output.exit = confed_core::error::ExitCode::Partial;
    }
    output
}

fn comment_results(outcome: &confed_core::sync::PushOutcome) -> serde_json::Value {
    json!({
        "comments_added": outcome.comments_added,
        "replies_added": outcome.replies_added,
        "comments_resolved": outcome.comments_resolved,
    })
}

/// Queue the same reply on each thread, then push them together.
async fn reply(ctx: &mut Context, ids: &[String], body: &str, push: bool) -> Result<Output> {
    if body.trim().is_empty() {
        return Err(ConfedError::usage("the comment body is empty"));
    }
    let mut queued = Vec::new();
    for id in ids {
        let page_id = page_of_comment(ctx, id)?;
        let path = sidecar_path(ctx, &page_id)?;
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut sidecar = if existing.is_empty() {
            comments::Sidecar { page_id: page_id.clone(), ..Default::default() }
        } else {
            comments::parse(&existing)?
        };
        sidecar.comments.push(SidecarComment {
            id: None,
            kind: SidecarKind::Footer,
            reply_to: Some(id.clone()),
            author: None,
            date: None,
            resolved: false,
            anchor: None,
            body: body.to_string(),
        });
        write_sidecar(ctx, &page_id, &sidecar)?;
        queued.push(json!({ "reply_to": id, "page_id": page_id }));
    }
    let mut result = json!({ "queued": queued, "draft": !push });
    let mut failed = Vec::new();
    let human = if push {
        let pages: Vec<String> =
            queued.iter().filter_map(|q| q["page_id"].as_str().map(str::to_string)).collect();
        let outcome = push_now(ctx, pages).await?;
        failed = outcome.failed.clone();
        result["result"] = comment_results(&outcome);
        result["draft"] = json!(!outcome.failed.is_empty());
        // What was posted, under the parent the server keeps: an inline thread
        // is one level deep, so a reply to a reply sits under the thread root.
        let mut asked: Vec<(String, String)> = queued
            .iter()
            .filter_map(|q| {
                Some((q["page_id"].as_str()?.to_string(), q["reply_to"].as_str()?.to_string()))
            })
            .collect();
        let mut posted = Vec::new();
        let mut human = String::new();
        for reply in &outcome.replies_added {
            let Some(page_id) = asked.first().map(|(p, _)| p.clone()) else { break };
            let records = ctx.workspace()?.state().page_comments(&page_id)?;
            let Some(r) = records.iter().find(|r| &r.comment_id == reply) else { continue };
            let parent = r.parent_comment_id.clone().unwrap_or_default();
            let root_of = |id: &str| {
                let mut current = id.to_string();
                while let Some(p) = records
                    .iter()
                    .find(|c| c.comment_id == current)
                    .and_then(|c| c.parent_comment_id.clone())
                {
                    current = p;
                }
                current
            };
            let index =
                asked.iter().position(|(_, a)| *a == parent || root_of(a) == parent).unwrap_or(0);
            let (_, requested) = asked.remove(index);
            let _ = write!(human, "  reply    {} on {parent}", r.comment_id);
            if requested != parent {
                let _ =
                    write!(human, " (asked for {requested}; an inline thread is one level deep)");
            }
            human.push('\n');
            posted.push(json!({
                "id": r.comment_id,
                "reply_to": requested,
                "parent": parent,
                "kind": r.kind,
            }));
        }
        result["posted"] = json!(outcome.replies_added);
        result["replies"] = json!(posted);
        let _ = writeln!(
            human,
            "Posted {}.",
            crate::output::plural(outcome.replies_added.len(), "reply", "replies")
        );
        human
    } else {
        format!(
            "Queued {}. {}",
            crate::output::plural(ids.len(), "reply", "replies"),
            ctx.style.dim("Run `confed push` to post.\n")
        )
    };
    Ok(with_failures(Output::new(result, human), &failed))
}

/// Edit a posted comment on the server right away.
async fn edit_comment(ctx: &mut Context, id: &str, body: &str) -> Result<Output> {
    if body.trim().is_empty() {
        return Err(ConfedError::usage("the comment body is empty"));
    }
    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    engine.edit_comment(ws, id, body).await?;
    Ok(Output::new(json!({ "comment_id": id, "edited": true }), format!("Edited {id}.\n")))
}

/// Delete posted comments on the server right away.
async fn remove(ctx: &mut Context, ids: &[String]) -> Result<Output> {
    for id in ids {
        page_of_comment(ctx, id)?;
    }
    if !ctx.global.yes && ctx.is_interactive() {
        let question = format!(
            "Delete {} and their replies on the server?",
            crate::output::plural(ids.len(), "comment", "comments")
        );
        if !crate::prompt::confirm(&question, false)? {
            return Ok(Output::new(
                json!({ "deleted": [], "cancelled": true }),
                "Cancelled: nothing was deleted.\n".to_string(),
            ));
        }
    }
    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    let mut deleted = Vec::new();
    let mut replies = Vec::new();
    for id in ids {
        replies.extend(engine.delete_comment(ws, id).await?);
        deleted.push(id.clone());
    }
    let mut human =
        format!("Deleted {}", crate::output::plural(deleted.len(), "comment", "comments"));
    if !replies.is_empty() {
        human.push_str(&format!(
            " and {}",
            crate::output::plural(replies.len(), "reply", "replies")
        ));
    }
    human.push_str(".\n");
    Ok(Output::new(json!({ "deleted": deleted, "replies_deleted": replies }), human))
}

fn sidecar_path(ctx: &Context, page_id: &str) -> Result<std::path::PathBuf> {
    let ws = ctx.workspace()?;
    let record = ws
        .state()
        .get_page(page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("no page {page_id}")))?;
    Ok(ws.absolute(&paths::sidecar_for(&record.local_path)).join(comments::COMMENTS_FILENAME))
}

#[derive(Clone, Copy, Debug, Default)]
struct AnchorPlacement {
    occurrence: Option<usize>,
    sidecar: bool,
}

/// The page file, parsed — so marks and their lines are known.
fn page_file(ctx: &Context, page_id: &str) -> Result<(std::path::PathBuf, String, MarkdownFile)> {
    let ws = ctx.workspace()?;
    let record = ws
        .state()
        .get_page(page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("no page {page_id}")))?;
    let path = ws.absolute(&record.local_path);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| ConfedError::io(format!("reading {}", record.local_path), e))?;
    let file = confed_core::frontmatter::parse(&content, &record.local_path)?;
    Ok((path, content, file))
}

/// Comments of a page the workspace does not track, read from the server.
async fn list_live(
    ctx: &Context,
    page_id: &str,
    unresolved: bool,
    inline_only: bool,
) -> Result<Output> {
    let client = ctx.build_client()?;
    let comments = match client.list_comments(&confed_api::PageId::new(page_id)).await {
        Ok(c) => c,
        Err(confed_api::ApiError::NotFound(_)) => {
            return Err(ConfedError::NotFound(format!(
                "page {page_id} is not in this workspace and does not exist on the server"
            )))
        }
        Err(e) => return Err(e.into()),
    };
    let root_resolved = |c: &confed_api::Comment| {
        let mut current = c;
        while let Some(parent) = current.parent_comment_id.as_ref() {
            match comments.iter().find(|p| &p.id == parent) {
                Some(p) => current = p,
                None => break,
            }
        }
        current.resolved
    };
    let mut entries = Vec::new();
    let mut human = String::new();
    for c in &comments {
        let resolved = root_resolved(c);
        let inline = c.kind == confed_api::CommentKind::Inline;
        if (unresolved && resolved) || (inline_only && !inline) {
            continue;
        }
        let body = confed_converter::storage_fragment_to_markdown(&c.body_storage)
            .unwrap_or_else(|_| c.body_storage.clone());
        let indent = if c.parent_comment_id.is_some() { "    " } else { "" };
        let _ = writeln!(
            human,
            "{indent}{} {} {}",
            ctx.style.bold(c.author.as_deref().unwrap_or("unknown")),
            ctx.style.dim(c.created_at.as_deref().unwrap_or_default()),
            if resolved { ctx.style.green("(resolved)") } else { String::new() }
        );
        for line in body.trim().lines() {
            let _ = writeln!(human, "{indent}  {line}");
        }
        human.push('\n');
        let anchor = c.anchor.as_ref().filter(|_| c.parent_comment_id.is_none());
        entries.push(json!({
            "id": c.id.0,
            "kind": if inline { "inline" } else { "footer" },
            "author": c.author,
            "created": c.created_at,
            "resolved": resolved,
            "thread_resolved": resolved,
            "reply_to": c.parent_comment_id.as_ref().map(|p| p.0.clone()),
            "anchor": anchor.map(|a| json!({ "text": a.text, "orphaned": a.orphaned })),
            "body_markdown": body.trim(),
        }));
    }
    if entries.is_empty() {
        human.push_str("No comments.\n");
    }
    Ok(Output::new(json!({ "page_id": page_id, "source": "server", "comments": entries }), human)
        .warn(format!(
            "page {page_id} is not in this workspace; these comments were read from the server"
        )))
}

fn list(ctx: &Context, page: &str, unresolved: bool, inline_only: bool) -> Result<Output> {
    let page_id = ctx.resolve_page(page)?;
    let records = ctx.workspace()?.state().page_comments(&page_id)?;
    let marks = page_file(ctx, &page_id).map(|(_, _, f)| f.marks).unwrap_or_default();

    // A reply has no status of its own: it is as resolved as its thread.
    let thread_resolved = |record: &confed_core::state::CommentRecord| {
        let mut current = record;
        for _ in 0..records.len() {
            let Some(parent) = current.parent_comment_id.as_deref() else { break };
            match records.iter().find(|r| r.comment_id == parent) {
                Some(p) => current = p,
                None => break,
            }
        }
        current.resolved
    };

    let mut human = String::new();
    let mut entries = Vec::new();
    for record in &records {
        let resolved = thread_resolved(record);
        if unresolved && resolved {
            continue;
        }
        if inline_only && record.kind != "inline" {
            continue;
        }
        // Only a thread's root is anchored; a reply sits on its thread.
        let anchor: Option<confed_api::InlineAnchor> = record
            .anchor
            .as_deref()
            .filter(|_| record.parent_comment_id.is_none())
            .and_then(|a| serde_json::from_str(a).ok());

        let indent = if record.parent_comment_id.is_some() { "    " } else { "" };
        let _ = writeln!(
            human,
            "{indent}{} {} {}",
            ctx.style.bold(&record.author.clone().unwrap_or_else(|| "unknown".into())),
            ctx.style.dim(&record.created_at.clone().unwrap_or_default()),
            if resolved { ctx.style.green("(resolved)") } else { String::new() }
        );
        let mark = marks
            .iter()
            .find(|m| m.id == MarkId::Comment(record.comment_id.clone()) && m.end.is_some());
        if let Some(anchor) = &anchor {
            let _ = writeln!(
                human,
                "{indent}  {} {}{}",
                ctx.style.dim("on:"),
                if anchor.orphaned {
                    ctx.style.yellow(&format!("\"{}\" (anchor text is gone)", anchor.text))
                } else {
                    format!("\"{}\"", anchor.text)
                },
                mark.map(|m| ctx.style.dim(&format!("  L{}", m.line))).unwrap_or_default()
            );
        }
        for line in record.body_markdown.lines() {
            let _ = writeln!(human, "{indent}  {line}");
        }
        let _ = writeln!(human);

        entries.push(json!({
            "id": record.comment_id,
            "kind": record.kind,
            "author": record.author,
            "created": record.created_at,
            "resolved": resolved,
            "thread_resolved": resolved,
            "reply_to": record.parent_comment_id,
            "anchor": anchor.map(|a| json!({
                "text": a.text,
                "orphaned": a.orphaned,
                "placed": mark.is_some(),
                "line": mark.map(|m| m.line),
            })),
            "body_markdown": record.body_markdown,
        }));
    }
    if entries.is_empty() {
        human.push_str("No comments.\n");
    }

    let orphans = confed_core::sync::orphan_markers(ctx.workspace()?, &page_id)?;
    if !orphans.is_empty() {
        let _ = writeln!(
            human,
            "{}",
            ctx.style.yellow(&format!(
                "{} in the page belong to no comment (left by deleted comments?):",
                crate::output::plural(orphans.len(), "inline marker", "inline markers")
            ))
        );
        for (r, text) in &orphans {
            let _ = writeln!(human, "  {} on \"{text}\"", ctx.style.dim(r));
        }
    }
    let orphan_markers: Vec<serde_json::Value> =
        orphans.iter().map(|(r, text)| json!({ "ref": r, "text": text })).collect();

    let output = Output::new(
        json!({ "page_id": page_id, "comments": entries, "orphan_markers": orphan_markers }),
        human,
    );
    // Comments are read from the local copy; say so when the page itself is
    // gone from the server.
    let gone = ctx.workspace()?.state().get_remote(&page_id)?.is_some_and(|r| r.deleted);
    Ok(if gone {
        output.warn(format!(
            "page {page_id} was deleted on the server (as of the last fetch); these are the \
             comments confed last saw, not live ones — `confed pull` removes the page locally"
        ))
    } else {
        output
    })
}

async fn add(
    ctx: &mut Context,
    page: &str,
    body: Option<&str>,
    anchor: Option<&str>,
    placement: AnchorPlacement,
    reply_to: Option<String>,
    push: bool,
) -> Result<Output> {
    let page_id = ctx.resolve_page(page)?;

    let body = match body {
        Some(text) => text.to_string(),
        None if ctx.is_interactive() => crate::prompt::read_line("Comment: ")?,
        None => {
            return Err(ConfedError::usage_with_hint("no comment body", "pass -m \"your comment\""))
        }
    };
    if body.trim().is_empty() {
        return Err(ConfedError::usage("the comment body is empty"));
    }

    let mut sidecar_reason: Option<&str> = None;
    // An inline anchor must name one place in the page's text, as Confluence
    // extracts it. Checked against the base copy, so a missing or ambiguous
    // anchor fails before anything is written or sent.
    let mut selection = None;
    if let Some(anchor_text) = anchor {
        selection = Some(check_anchor(ctx, &page_id, anchor_text, placement.occurrence)?);
        let client = ctx.build_client()?;
        if !client.capabilities().inline_comment_create {
            return Err(ConfedError::Unsupported(format!(
                "creating inline comments is not available on Confluence {}",
                client.flavor()
            )));
        }
        let body_draft = if placement.sidecar {
            None
        } else {
            body_draft(ctx, &page_id, anchor_text, placement.occurrence, &body)?
        };
        if body_draft.is_none() && !placement.sidecar {
            sidecar_reason = Some(
                "the text cannot carry a mark in the page body (it is inside a ```confluence \
                 block, reads differently in the Markdown, or the comment is more than one line)",
            );
        }
        if let Some((path, line)) = body_draft {
            let mut human =
                format!("Added a draft inline comment at line {line} of {}\n", path.display());
            let mut result = json!({ "page_id": page_id, "draft": true, "body": body, "anchor": anchor_text, "line": line, "written_to": "body" });
            if push {
                let engine = ctx.engine(client)?;
                let ws = ctx.workspace_mut()?;
                let _lock = ws.lock()?;
                let outcome = engine.push(ws, &comment_push(vec![page_id.clone()])).await?;
                human = format!("Posted {} comment(s).\n", outcome.comments_added.len());
                result["posted"] = json!(outcome.comments_added);
                result["comments"] = posted_details(ctx, &page_id, &outcome.comments_added)?;
                result["draft"] = json!(!outcome.failed.is_empty());
                return Ok(with_failures(Output::new(result, human), &outcome.failed));
            }
            human.push_str(&ctx.style.dim("Run `confed push` to post it.\n"));
            return Ok(Output::new(result, human));
        }
    }

    let path = sidecar_path(ctx, &page_id)?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut sidecar = if existing.is_empty() {
        comments::Sidecar { page_id: page_id.clone(), ..Default::default() }
    } else {
        comments::parse(&existing)?
    };

    sidecar.comments.push(SidecarComment {
        id: None,
        kind: if anchor.is_some() { SidecarKind::Inline } else { SidecarKind::Footer },
        reply_to,
        author: None,
        date: None,
        resolved: false,
        // The occurrence is kept so push posts the one that was picked.
        anchor: anchor.map(|text| confed_api::InlineAnchor {
            text: text.to_string(),
            match_index: placement.occurrence.map(|n| n - 1),
            match_count: selection.as_ref().and_then(|s| s.match_count),
            ..Default::default()
        }),
        body: body.clone(),
    });

    write_sidecar(ctx, &page_id, &sidecar)?;

    let mut human = format!("Added a draft comment to {}\n", path.display());
    if let Some(reason) = sidecar_reason {
        let _ = writeln!(human, "  in comments.md, not the page body: {reason}");
    }
    let mut result = json!({
        "page_id": page_id,
        "draft": true,
        "body": body,
        "anchor": anchor,
        "written_to": "sidecar",
        "sidecar_reason": sidecar_reason,
    });

    if push {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome = engine.push(ws, &comment_push(vec![page_id.clone()])).await?;
        human = format!("Posted {} comment(s).\n", outcome.comments_added.len());
        result["posted"] = json!(outcome.comments_added);
        result["comments"] = posted_details(ctx, &page_id, &outcome.comments_added)?;
        result["draft"] = json!(!outcome.failed.is_empty());
        return Ok(with_failures(Output::new(result, human), &outcome.failed));
    } else {
        human.push_str(&ctx.style.dim("Run `confed push` to post it.\n"));
    }

    Ok(Output::new(result, human))
}

/// Queue resolves — the ids given, or every open thread on a page — then
/// push them together.
async fn resolve(
    ctx: &mut Context,
    ids: &[String],
    all: Option<&str>,
    push: bool,
) -> Result<Output> {
    let client = ctx.build_client()?;
    if !client.capabilities().comment_resolve {
        return Err(ConfedError::Unsupported(format!(
            "resolving comments is not available on Confluence {}",
            client.flavor()
        )));
    }
    let data_center = client.flavor() == confed_api::Flavor::DataCenter;

    let mut targets: Vec<(String, String)> = Vec::new(); // (page, id)
    let mut skipped: Vec<serde_json::Value> = Vec::new();
    match all {
        Some(page) => {
            let page_id = ctx.resolve_page(page)?;
            for c in ctx.workspace()?.state().page_comments(&page_id)? {
                if c.parent_comment_id.is_some() || c.resolved {
                    continue;
                }
                if data_center && c.kind != "inline" {
                    skipped.push(json!({ "id": c.comment_id, "reason": "page comments cannot be resolved on Data Center" }));
                    continue;
                }
                targets.push((page_id.clone(), c.comment_id));
            }
        }
        None => {
            for id in ids {
                targets.push((page_of_comment(ctx, id)?, id.clone()));
            }
        }
    }

    for (page_id, id) in &targets {
        let path = sidecar_path(ctx, page_id)?;
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        if content.contains(&format!("confed:resolve id={id} ")) {
            continue;
        }
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!("\n<!-- confed:resolve id={id} -->\n"));
        write_atomic(&path, &content)?;
    }

    let ids: Vec<&String> = targets.iter().map(|(_, id)| id).collect();
    let mut result = json!({ "queued": ids, "skipped": skipped, "draft": !push });
    let mut human = String::new();
    for s in &skipped {
        let _ = writeln!(
            human,
            "  skipped  {} — {}",
            s["id"].as_str().unwrap_or(""),
            s["reason"].as_str().unwrap_or("")
        );
    }
    let mut failed = Vec::new();
    if push && !targets.is_empty() {
        let pages: Vec<String> = targets.iter().map(|(page, _)| page.clone()).collect();
        let outcome = push_now(ctx, pages).await?;
        failed = outcome.failed.clone();
        result["result"] = comment_results(&outcome);
        for id in &outcome.comments_resolved {
            let _ = writeln!(human, "  resolved {id}");
        }
        let _ = writeln!(
            human,
            "Resolved {}.",
            crate::output::plural(outcome.comments_resolved.len(), "thread", "threads")
        );
    } else if targets.is_empty() {
        human.push_str("No open threads to resolve.\n");
    } else {
        let _ = writeln!(
            human,
            "Queued {} in the sidecar. {}",
            crate::output::plural(targets.len(), "resolve", "resolves"),
            ctx.style.dim("Run `confed push` to apply.")
        );
    }
    Ok(with_failures(Output::new(result, human), &failed))
}

fn write_sidecar(ctx: &Context, page_id: &str, sidecar: &comments::Sidecar) -> Result<()> {
    let ws = ctx.workspace()?;
    let record = ws
        .state()
        .get_page(page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("no page {page_id}")))?;
    let records = ws.state().page_comments(page_id)?;
    let drafts: Vec<SidecarComment> =
        sidecar.comments.iter().filter(|c| c.is_draft()).cloned().collect();

    let path = sidecar_path(ctx, page_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
    }
    write_atomic(&path, &comments::render(page_id, &record.title, &records, &drafts))
}

/// Write an inline draft into the page body as a `<!--c new …-->` mark, and
/// check it is really there. `None` when it cannot be: a multi-line or `--`
/// comment (a mark holds one line), text the Markdown reads differently, or a
/// span inside a ```confluence block, where a mark would be content and is
/// never written. The caller then uses the sidecar — a draft is never lost.
fn body_draft(
    ctx: &Context,
    page_id: &str,
    anchor: &str,
    occurrence: Option<usize>,
    body: &str,
) -> Result<Option<(std::path::PathBuf, usize)>> {
    if body.contains('\n') || body.contains("--") {
        return Ok(None);
    }
    let (path, original, mut file) = page_file(ctx, page_id)?;
    let Ok((start, end)) = locate_anchor(&file, anchor, occurrence) else { return Ok(None) };
    let before = file.drafts().count();
    let line = file.body[..start].matches('\n').count() + 1;
    file.marks.push(confed_converter::Mark {
        id: MarkId::New,
        start,
        end: Some(end),
        text: anchor.to_string(),
        note: body.to_string(),
        line,
    });
    write_atomic(&path, &file.render()?)?;

    let written = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| confed_core::frontmatter::parse(&c, &path.to_string_lossy()).ok())
        .is_some_and(|f| f.drafts().count() == before + 1);
    if !written {
        write_atomic(&path, &original)?;
        return Ok(None);
    }
    Ok(Some((path, line)))
}

/// Where a body draft goes: the only occurrence of the text, or the one the
/// user picked. Several occurrences with no pick is an error that lists them.
fn locate_anchor(
    file: &MarkdownFile,
    text: &str,
    occurrence: Option<usize>,
) -> Result<(usize, usize)> {
    let body = &file.body;
    let found: Vec<usize> = body.match_indices(text).map(|(i, _)| i).collect();
    if found.is_empty() {
        return Err(ConfedError::NotFound(format!(
            "the text \"{text}\" does not appear in the page file; copy the exact wording \
             from the page body"
        )));
    }
    let index = match occurrence {
        Some(n) if n >= 1 && n <= found.len() => n - 1,
        Some(n) => {
            return Err(ConfedError::usage(format!(
                "--occurrence {n} is out of range: the text appears {} time(s)",
                found.len()
            )))
        }
        None if found.len() == 1 => 0,
        None => {
            let lines: Vec<String> = found
                .iter()
                .enumerate()
                .map(|(i, &at)| format!("{}: line {}", i + 1, body[..at].matches('\n').count() + 1))
                .collect();
            return Err(ConfedError::usage_with_hint(
                format!("the text \"{text}\" appears {} times ({})", found.len(), lines.join(", ")),
                "pick one with --occurrence N, or include more surrounding words",
            ));
        }
    };
    Ok((found[index], found[index] + text.len()))
}

/// Where an inline anchor sits in the page's base copy: exit 6 when it is not
/// there, 2 when it is there more than once and no occurrence was picked.
fn check_anchor(
    ctx: &Context,
    page_id: &str,
    anchor: &str,
    occurrence: Option<usize>,
) -> Result<confed_api::InlineAnchor> {
    let ws = ctx.workspace()?;
    let record = ws
        .state()
        .get_page(page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("no page {page_id}")))?;
    let local = std::fs::read_to_string(ws.absolute(&record.local_path))
        .ok()
        .and_then(|c| confed_core::frontmatter::parse(&c, &record.local_path).ok())
        .map(|f| f.body);
    confed_core::sync::server_selection(
        &record.storage_body,
        anchor,
        occurrence,
        local.as_deref(),
        &record.local_path,
    )
}

/// What a push created, as the server reported it: id, kind, and for an
/// inline comment the text it is anchored to and Confluence's marker ref.
fn posted_details(ctx: &Context, page_id: &str, ids: &[String]) -> Result<serde_json::Value> {
    // The comment may sit on another page than the one asked about; look
    // everywhere rather than report it with no kind.
    let ws = ctx.workspace()?;
    let mut records = ws.state().page_comments(page_id)?;
    for page in ws.state().all_pages()? {
        if page.page_id != page_id {
            records.extend(ws.state().page_comments(&page.page_id)?);
        }
    }
    Ok(ids
        .iter()
        .map(|id| {
            let record = records.iter().find(|r| &r.comment_id == id);
            let anchor: Option<confed_api::InlineAnchor> =
                record.and_then(|r| r.anchor.as_deref()).and_then(|a| serde_json::from_str(a).ok());
            json!({
                "id": id,
                "page_id": record.map(|r| r.page_id.as_str()),
                "kind": record.map(|r| r.kind.as_str()),
                "reply_to": record.and_then(|r| r.parent_comment_id.as_deref()),
                "anchor": anchor.as_ref().map(|a| a.text.as_str()),
                "marker_ref": anchor.as_ref().and_then(|a| a.marker_ref.as_deref()),
            })
        })
        .collect())
}

fn page_of_comment(ctx: &Context, comment_id: &str) -> Result<String> {
    let ws = ctx.workspace()?;
    for record in ws.state().all_pages()? {
        if ws.state().page_comments(&record.page_id)?.iter().any(|c| c.comment_id == comment_id) {
            return Ok(record.page_id);
        }
    }
    Err(ConfedError::NotFound(format!("no comment {comment_id}")))
}
