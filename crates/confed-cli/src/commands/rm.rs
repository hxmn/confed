//! `confed rm` — delete a page locally; the server follows at the next push.

use crate::cli::RmArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::local;
use confed_core::sync::PushOptions;
use serde_json::{json, Value};
use std::fmt::Write;

/// The pages removed on disk (or, for a dry run, that would be), as the
/// result lists them, and the human report so far. No credential needed.
fn remove_locally(ctx: &mut Context, args: &RmArgs) -> Result<(Vec<Value>, String)> {
    if args.paths.is_empty() {
        return Err(ConfedError::usage("name at least one page to remove"));
    }
    // Every name resolved and every page checked before anything is removed:
    // one that cannot go stops the lot.
    let mut page_ids = Vec::with_capacity(args.paths.len());
    for reference in &args.paths {
        let id = ctx.resolve_page(reference)?;
        if !page_ids.contains(&id) {
            page_ids.push(id);
        }
    }
    let ws = ctx.workspace()?;
    let records = if args.dry_run {
        local::removable_pages(ws, &page_ids)?
    } else {
        local::remove_pages(ws, &page_ids, args.keep_local)?
    };
    let mut removed = Vec::new();
    let mut human = String::new();
    for record in records {
        let _ = if args.dry_run {
            writeln!(
                human,
                "  would remove {}{}",
                record.local_path,
                if args.push { " and delete it on the server" } else { "" }
            )
        } else {
            writeln!(human, "  removed {}", record.local_path)
        };
        removed.push(json!({
            "page_id": record.page_id,
            "path": record.local_path,
            "server_deleted": false,
        }));
    }
    if args.dry_run {
        let _ = writeln!(human, "{}", ctx.style.dim("Dry run — nothing was removed or deleted."));
    }
    Ok((removed, human))
}

/// `confed rm` without `--push` (or with `--dry-run`): on disk only.
pub fn run(ctx: &mut Context, args: &RmArgs) -> Result<Output> {
    let (removed, mut human) = remove_locally(ctx, args)?;
    if args.dry_run {
        return Ok(Output::new(
            json!({ "removed": removed, "dry_run": true, "would_delete_on_server": args.push }),
            human,
        ));
    }
    let _ = writeln!(
        human,
        "{}",
        ctx.style.dim("Run `confed push --allow-delete` to delete them on the server.")
    );
    Ok(Output::new(json!({ "removed": removed, "dry_run": false }), human))
}

/// `confed rm --push`: removed on disk, then deleted on the server.
pub async fn run_and_push(ctx: &mut Context, args: &RmArgs) -> Result<Output> {
    if args.dry_run {
        return run(ctx, args);
    }
    // Deleting on the server is not undoable from confed's side: asked
    // before anything is removed, here or there. Unattended, only `--yes`
    // says yes — as for every destructive command.
    if !ctx.global.yes {
        let pages = crate::output::plural(args.paths.len(), "page", "pages");
        if !ctx.is_interactive() {
            return Err(ConfedError::usage_with_hint(
                format!("`rm --push` deletes {pages} on the server, and nothing confirmed it"),
                "add --yes to delete without asking, or drop --push to remove the files only",
            ));
        }
        if !crate::prompt::confirm(&format!("Delete {pages} on the server?"), false)? {
            return Ok(Output::new(
                json!({ "removed": [], "cancelled": true }),
                "Cancelled: nothing was removed.\n".to_string(),
            ));
        }
    }
    let (mut removed, mut human) = remove_locally(ctx, args)?;
    let mut output_json = json!({ "removed": removed, "dry_run": false });
    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    // Only the pages named: an unscoped push would also upload every other
    // edit and delete any other page whose file is missing.
    let scope: Vec<String> =
        removed.iter().filter_map(|r| r["page_id"].as_str().map(str::to_string)).collect();
    let outcome =
        engine.push(ws, &PushOptions { scope, allow_delete: true, ..Default::default() }).await?;
    for entry in &mut removed {
        let id = entry["page_id"].as_str().unwrap_or_default().to_string();
        entry["server_deleted"] = json!(outcome.deleted.iter().any(|d| d.page_id == id));
    }
    let _ = writeln!(human, "Deleted {} page(s) on the server.", outcome.deleted.len());
    output_json["removed"] = json!(removed);
    output_json["push"] = serde_json::to_value(&outcome)?;
    Ok(Output::new(output_json, human).with_push_outcome(&outcome))
}
