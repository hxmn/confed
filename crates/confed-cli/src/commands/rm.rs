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
    let mut removed = Vec::new();
    let mut human = String::new();
    for reference in &args.paths {
        let page_id = ctx.resolve_page(reference)?;
        let ws = ctx.workspace()?;
        let record = if args.dry_run {
            local::removable(ws, &page_id)?
        } else {
            local::remove_page(ws, &page_id, args.keep_local)?
        };
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
            "page_id": page_id,
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
    let (mut removed, mut human) = remove_locally(ctx, args)?;
    let mut output_json = json!({ "removed": removed, "dry_run": false });

    // Deleting on the server is not undoable from confed's side, so ask
    // unless the user has already said yes.
    if !ctx.global.yes && ctx.is_interactive() {
        let question = format!(
            "Delete {} on the server?",
            crate::output::plural(removed.len(), "page", "pages")
        );
        if !crate::prompt::confirm(&question, false)? {
            return Ok(Output::new(
                json!({ "removed": removed, "cancelled": true }),
                format!("{human}Cancelled: nothing was deleted on the server.\n"),
            ));
        }
    }
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
    Ok(Output::new(output_json, human))
}
