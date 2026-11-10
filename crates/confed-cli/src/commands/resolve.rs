//! `confed resolve` — finish a merge and clear the conflicted state.

use crate::cli::ResolveArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::resolve::{finish_resolution, take_side};
use confed_core::worktree::{self, PageState};
use serde_json::json;
use std::fmt::Write;

pub fn run(ctx: &mut Context, args: &ResolveArgs) -> Result<Output> {
    let ws = ctx.workspace()?;
    let scan = worktree::scan(ws)?;
    let conflicted: Vec<_> =
        scan.pages.iter().filter(|p| p.state == PageState::Conflicted).collect();

    if args.list {
        let mut human = String::new();
        for page in &conflicted {
            let _ = writeln!(human, "  {}", page.path);
        }
        if conflicted.is_empty() {
            human.push_str("No unresolved conflicts.\n");
        }
        return Ok(Output::new(
            json!({ "conflicted": conflicted.iter().map(|p| json!({
                "page_id": p.page_id, "path": p.path
            })).collect::<Vec<_>>() }),
            human,
        ));
    }

    if args.paths.is_empty() && conflicted.is_empty() {
        return Ok(Output::new(json!({ "resolved": [] }), "No unresolved conflicts.\n"));
    }

    let targets: Vec<String> = if args.paths.is_empty() {
        conflicted.iter().map(|p| p.path.clone()).collect()
    } else {
        args.paths.clone()
    };

    let mut resolved = Vec::new();
    let mut remaining = Vec::new();
    let mut human = String::new();

    for target in &targets {
        let page_id = ctx.resolve_page(target)?;
        let ws = ctx.workspace()?;
        let record = ws
            .state()
            .get_page(&page_id)?
            .ok_or_else(|| ConfedError::NotFound(format!("no page {target}")))?;
        let path = ws.absolute(&record.local_path);

        let content = std::fs::read_to_string(&path)
            .map_err(|e| ConfedError::io(format!("reading {}", record.local_path), e))?;

        let cleaned =
            if args.ours || args.theirs { take_side(&content, args.ours) } else { content.clone() };

        let ws = ctx.workspace_mut()?;
        let markers = finish_resolution(ws, &record, &content, &cleaned)?;
        if !markers.is_empty() {
            remaining.push(json!({
                "page_id": page_id, "path": record.local_path,
                "marker_lines": markers,
            }));
            let _ = writeln!(
                human,
                "  {} {} — still has conflict markers on line(s) {}",
                ctx.style.red("unresolved"),
                record.local_path,
                markers.iter().map(usize::to_string).collect::<Vec<_>>().join(", ")
            );
            continue;
        }

        resolved.push(json!({ "page_id": page_id, "path": record.local_path }));
        let _ = writeln!(human, "  {} {}", ctx.style.green("resolved"), record.local_path);
    }

    if !resolved.is_empty() {
        let _ = writeln!(
            human,
            "\n{}",
            ctx.style.dim("Run `confed push` to upload the resolved pages.")
        );
    }

    let mut output = Output::new(json!({ "resolved": resolved, "remaining": remaining }), human);
    if !remaining.is_empty() {
        output.exit = confed_core::ExitCode::State;
    }
    Ok(output)
}
