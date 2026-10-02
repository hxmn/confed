//! `confed log` — version history, or confed's own sync log.
//!
//! With a page it answers for that page; without one it widens to the whole
//! space this directory is bound to, which is the view you want when someone
//! else has been editing and you do not yet know which page moved.

use crate::cli::LogArgs;
use crate::context::Context;
use crate::output::Output;
use confed_api::PageId;
use confed_core::error::{ConfedError, Result};
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, args: &LogArgs) -> Result<Output> {
    if args.local {
        return run_local(ctx, args);
    }
    match &args.page {
        Some(page) => {
            let page_id = ctx.page_id_arg(page)?;
            page_history(ctx, args, &page_id).await
        }
        None => space_history(ctx, args).await,
    }
}

/// The offline half, so `--local` never wakes the OS keyring.
pub fn run_local(ctx: &mut Context, args: &LogArgs) -> Result<Output> {
    let page_id = args.page.as_deref().map(|p| ctx.page_id_arg(p)).transpose()?;
    let ws = ctx.workspace()?;
    let entries = ws.state().recent_log(args.limit, page_id.as_deref())?;
    let paths = local_paths(ctx)?;

    let mut human = String::new();
    for entry in &entries {
        let page = entry.page_id.as_deref().and_then(|id| paths.get(id));
        let mut line = format!(
            "{}  {:<14} {} {}",
            ctx.style.dim(&entry.ts),
            entry.op,
            entry.result,
            entry.detail.clone().unwrap_or_default()
        );
        // Which page an entry is about only matters when the log spans the space.
        if page_id.is_none() {
            if let Some(page) = page.or(entry.page_id.as_ref()) {
                line = format!("{line}  {}", ctx.style.dim(page));
            }
        }
        let _ = writeln!(human, "{}", line.trim_end());
    }

    let mut result = json!({
        "entries": entries.iter().map(|e| json!({
            "ts": e.ts, "op": e.op, "result": e.result,
            "from_version": e.from_version, "to_version": e.to_version,
            "detail": e.detail,
            "page_id": e.page_id,
            "page": e.page_id.as_deref().and_then(|id| paths.get(id)),
        })).collect::<Vec<_>>()
    });
    match &page_id {
        Some(id) => result["page_id"] = json!(id),
        None => result["space"] = json!(ctx.workspace()?.space_key()?),
    }
    Ok(Output::new(result, human))
}

/// The server's version history for one page.
async fn page_history(ctx: &mut Context, args: &LogArgs, page_id: &str) -> Result<Output> {
    let base_version = ctx.workspace()?.state().get_page(page_id)?.map(|p| p.version);
    let client = ctx.build_client()?;
    let versions = match client.get_page_versions(&PageId::new(page_id), args.limit).await {
        Ok(v) => v,
        Err(confed_api::ApiError::NotFound(_)) => {
            return Err(ConfedError::NotFound(format!(
                "page {page_id} does not exist on the server (deleted, or never there)"
            )))
        }
        Err(e) => return Err(e.into()),
    };

    let mut human = String::new();
    for version in &versions {
        let marker = if Some(version.number) == base_version { "*" } else { " " };
        let _ = writeln!(
            human,
            "{marker} v{:<4} {:<20} {} {}",
            version.number,
            version.author.clone().unwrap_or_default(),
            ctx.style.dim(&version.when.clone().unwrap_or_default()),
            version.message.clone().unwrap_or_default()
        );
    }
    if base_version.is_some() {
        let _ = writeln!(human, "\n{}", ctx.style.dim("* the version your local file is based on"));
    }

    Ok(Output::new(
        json!({
            "page_id": page_id,
            "base_version": base_version,
            "versions": versions.iter().map(|v| json!({
                "number": v.number, "author": v.author,
                "when": v.when, "message": v.message,
            })).collect::<Vec<_>>()
        }),
        human,
    ))
}

/// What has changed lately anywhere in the space this directory is bound to.
///
/// CQL, not a full page listing: the server does the ordering and sends back
/// only the pages asked for, so this stays one request on a space of any size.
async fn space_history(ctx: &mut Context, args: &LogArgs) -> Result<Output> {
    let space = ctx.workspace()?.space_key()?;
    let cql = space_cql(&space);
    let paths = local_paths(ctx)?;

    let client = ctx.build_client()?;
    let results = client.search_cql(&cql, args.limit).await?;

    let mut human = String::new();
    for result in &results {
        let version = result.version.map(|n| format!("v{n}")).unwrap_or_default();
        let where_ =
            paths.get(result.page_id.as_str()).cloned().unwrap_or_else(|| result.url.clone());
        let line = format!(
            "{}  {:<5} {:<20} {:<40} {}",
            ctx.style.dim(result.when.as_deref().unwrap_or("")),
            version,
            result.author.clone().unwrap_or_default(),
            result.title,
            ctx.style.dim(&where_),
        );
        let _ = writeln!(human, "{}", line.trim_end());
    }
    if results.is_empty() {
        let _ = writeln!(human, "No pages in {space}");
    }

    Ok(Output::new(
        json!({
            "space": space,
            "cql": cql,
            "pages": results.iter().map(|r| json!({
                "page_id": r.page_id.0,
                "title": r.title,
                "version": r.version,
                "author": r.author,
                "when": r.when,
                "url": r.url,
                "local_path": paths.get(r.page_id.as_str()),
            })).collect::<Vec<_>>()
        }),
        human,
    ))
}

/// Newest first — the server sorts, so `--limit` really is the N most recent.
fn space_cql(space: &str) -> String {
    let escaped = space.replace('"', "\\\"");
    format!("space = \"{escaped}\" and type = page order by lastmodified desc")
}

/// page id → workspace-relative path, for the pages that have been pulled.
fn local_paths(ctx: &Context) -> Result<HashMap<String, String>> {
    Ok(ctx
        .workspace()?
        .state()
        .all_pages()?
        .into_iter()
        .map(|p| (p.page_id, p.local_path))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_space_query_asks_for_pages_newest_first() {
        assert_eq!(
            space_cql("DOCS"),
            "space = \"DOCS\" and type = page order by lastmodified desc"
        );
    }
}
