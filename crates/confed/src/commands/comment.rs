//! `confed comment` — a structured accessor over the `comments.md` sidecar.
//!
//! Every write goes through the sidecar file, which stays the source of truth,
//! so the same edits can be made with a text editor or by an agent.

use crate::cli::CommentCommand;
use crate::context::Context;
use crate::output::Output;
use confed_convert::MarkId;
use confed_core::comments::{self, SidecarComment, SidecarKind};
use confed_core::error::{ConfedError, Result};
use confed_core::frontmatter::MarkdownFile;
use confed_core::paths;
use confed_core::sync::{write_atomic, PushOptions};
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, command: &CommentCommand) -> Result<Output> {
    match command {
        CommentCommand::List { page, unresolved, inline } => list(ctx, page, *unresolved, *inline),
        CommentCommand::Add { page, body, anchor, occurrence, sidecar, push } => {
            let placement = AnchorPlacement { occurrence: *occurrence, sidecar: *sidecar };
            add(ctx, page, body.as_deref(), anchor.as_deref(), placement, None, *push).await
        }
        CommentCommand::Reply { comment_id, body, push } => {
            let page = page_of_comment(ctx, comment_id)?;
            add(
                ctx,
                &page,
                Some(body),
                None,
                AnchorPlacement::default(),
                Some(comment_id.clone()),
                *push,
            )
            .await
        }
        CommentCommand::Resolve { comment_id, push } => resolve(ctx, comment_id, *push).await,
    }
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

fn list(ctx: &Context, page: &str, unresolved: bool, inline_only: bool) -> Result<Output> {
    let page_id = ctx.resolve_page(page)?;
    let records = ctx.workspace()?.state().page_comments(&page_id)?;
    let marks = page_file(ctx, &page_id).map(|(_, _, f)| f.marks).unwrap_or_default();

    let mut human = String::new();
    let mut entries = Vec::new();
    for record in &records {
        if unresolved && record.resolved {
            continue;
        }
        if inline_only && record.kind != "inline" {
            continue;
        }
        let anchor: Option<confed_api::InlineAnchor> =
            record.anchor.as_deref().and_then(|a| serde_json::from_str(a).ok());

        let indent = if record.parent_comment_id.is_some() { "    " } else { "" };
        let _ = writeln!(
            human,
            "{indent}{} {} {}",
            ctx.style.bold(&record.author.clone().unwrap_or_else(|| "unknown".into())),
            ctx.style.dim(&record.created_at.clone().unwrap_or_default()),
            if record.resolved { ctx.style.green("(resolved)") } else { String::new() }
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
            "resolved": record.resolved,
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

    Ok(Output::new(json!({ "page_id": page_id, "comments": entries }), human))
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

    // Inline anchoring is a Cloud-only API; fail before writing anything.
    if let Some(anchor_text) = anchor {
        let client = ctx.build_client()?;
        if !client.capabilities().inline_comment_create {
            return Err(ConfedError::Unsupported(format!(
                "creating inline comments is not available on Confluence {}",
                client.flavor()
            )));
        }
        if placement.sidecar {
            verify_anchor_is_unique(ctx, &page_id, anchor_text)?;
        } else {
            // The draft goes into the page body as a mark, at the chosen
            // occurrence, so repeated text is no obstacle.
            let (path, _, mut file) = page_file(ctx, &page_id)?;
            let (start, end) = locate_anchor(&file, anchor_text, placement.occurrence)?;
            let line = file.body[..start].matches('\n').count() + 1;
            file.marks.push(confed_convert::Mark {
                id: MarkId::New,
                start,
                end: Some(end),
                text: anchor_text.to_string(),
                note: body.clone(),
                line,
            });
            write_atomic(&path, &file.render()?)?;
            let mut human =
                format!("Added a draft inline comment at line {line} of {}\n", path.display());
            let mut result = json!({ "page_id": page_id, "draft": true, "body": body, "anchor": anchor_text, "line": line });
            if push {
                let engine = ctx.engine(client)?;
                let ws = ctx.workspace_mut()?;
                let _lock = ws.lock()?;
                let outcome = engine
                    .push(ws, &PushOptions { with_comments: true, ..Default::default() })
                    .await?;
                human = format!("Posted {} comment(s).\n", outcome.comments_added.len());
                result["posted"] = json!(outcome.comments_added);
                result["draft"] = json!(false);
            } else {
                human.push_str(&ctx.style.dim("Run `confed push` to post it.\n"));
            }
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
        anchor: anchor
            .map(|text| confed_api::InlineAnchor { text: text.to_string(), ..Default::default() }),
        body: body.clone(),
    });

    write_sidecar(ctx, &page_id, &sidecar)?;

    let mut human = format!("Added a draft comment to {}\n", path.display());
    let mut result = json!({ "page_id": page_id, "draft": true, "body": body });

    if push {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome =
            engine.push(ws, &PushOptions { with_comments: true, ..Default::default() }).await?;
        human = format!("Posted {} comment(s).\n", outcome.comments_added.len());
        result["posted"] = json!(outcome.comments_added);
        result["draft"] = json!(false);
    } else {
        human.push_str(&ctx.style.dim("Run `confed push` to post it.\n"));
    }

    Ok(Output::new(result, human))
}

async fn resolve(ctx: &mut Context, comment_id: &str, push: bool) -> Result<Output> {
    let client = ctx.build_client()?;
    if !client.capabilities().comment_resolve {
        return Err(ConfedError::Unsupported(format!(
            "resolving comments is not available on Confluence {}",
            client.flavor()
        )));
    }

    let page_id = page_of_comment(ctx, comment_id)?;
    let path = sidecar_path(ctx, &page_id)?;
    let mut content = std::fs::read_to_string(&path)
        .map_err(|e| ConfedError::io(format!("reading {}", path.display()), e))?;
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("\n<!-- confed:resolve id={comment_id} -->\n"));
    write_atomic(&path, &content)?;

    let mut human = format!("Marked {comment_id} resolved in the sidecar.\n");
    if push {
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        engine.push(ws, &PushOptions { with_comments: true, ..Default::default() }).await?;
        human = format!("Resolved {comment_id}.\n");
    } else {
        human.push_str(&ctx.style.dim("Run `confed push` to apply it.\n"));
    }
    Ok(Output::new(json!({ "comment_id": comment_id, "page_id": page_id }), human))
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
        return Err(ConfedError::state_with_hint(
            format!("the text \"{text}\" does not appear in the page"),
            "copy the exact wording from the page body",
        ));
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
            return Err(ConfedError::state_with_hint(
                format!("the text \"{text}\" appears {} times ({})", found.len(), lines.join(", ")),
                "pick one with --occurrence N, or include more surrounding words",
            ));
        }
    };
    Ok((found[index], found[index] + text.len()))
}

/// An inline anchor has to identify exactly one place in the page.
fn verify_anchor_is_unique(ctx: &Context, page_id: &str, anchor: &str) -> Result<()> {
    let ws = ctx.workspace()?;
    let record = ws
        .state()
        .get_page(page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("no page {page_id}")))?;
    let content = std::fs::read_to_string(ws.absolute(&record.local_path))
        .map_err(|e| ConfedError::io(format!("reading {}", record.local_path), e))?;

    match content.matches(anchor).count() {
        1 => Ok(()),
        0 => Err(ConfedError::state_with_hint(
            format!("the text \"{anchor}\" does not appear in {}", record.local_path),
            "copy the exact wording from the page body",
        )),
        n => Err(ConfedError::state_with_hint(
            format!("the text \"{anchor}\" appears {n} times in {}", record.local_path),
            "include more surrounding words so the anchor is unambiguous",
        )),
    }
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
