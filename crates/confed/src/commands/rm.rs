//! `confed rm` — delete a page locally; the server follows at the next push.

use crate::cli::RmArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::paths;
use confed_core::sync::PushOptions;
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, args: &RmArgs) -> Result<Output> {
    if args.paths.is_empty() {
        return Err(ConfedError::usage("name at least one page to remove"));
    }

    let mut removed = Vec::new();
    let mut human = String::new();

    for reference in &args.paths {
        let page_id = ctx.resolve_page(reference)?;
        let ws = ctx.workspace()?;
        let record = ws
            .state()
            .get_page(&page_id)?
            .ok_or_else(|| ConfedError::NotFound(format!("no page {reference}")))?;

        // Deleting a parent would orphan its children on the server.
        let children: Vec<String> = ws
            .state()
            .all_pages()?
            .into_iter()
            .filter(|p| p.parent_id.as_deref() == Some(page_id.as_str()))
            .map(|p| p.local_path)
            .collect();
        if !children.is_empty() {
            return Err(ConfedError::state_with_hint(
                format!("{} has {} child page(s)", record.local_path, children.len()),
                format!("remove or move them first: {}", children.join(", ")),
            ));
        }

        if !args.keep_local {
            let path = ws.absolute(&record.local_path);
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| ConfedError::io(format!("removing {}", record.local_path), e))?;
            }
            let sidecar = ws.absolute(&paths::sidecar_for(&record.local_path));
            if sidecar.is_dir() {
                let _ = std::fs::remove_dir_all(sidecar);
            }
        }

        // The base record stays: it is what tells push there is a deletion to send.
        let _ = writeln!(human, "  removed {}", record.local_path);
        removed.push(json!({
            "page_id": page_id,
            "path": record.local_path,
            "server_deleted": false,
        }));
    }

    let mut output_json = json!({ "removed": removed });

    if args.push {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome = engine
            .push(ws, &PushOptions { allow_delete: true, ..Default::default() })
            .await?;
        let _ = writeln!(human, "Deleted {} page(s) on the server.", outcome.deleted.len());
        output_json["push"] = serde_json::to_value(&outcome)?;
    } else {
        let _ = writeln!(
            human,
            "{}",
            ctx.style.dim("Run `confed push --allow-delete` to delete them on the server.")
        );
    }

    Ok(Output::new(output_json, human))
}
