//! `fetch`, `pull` and `push` — thin wrappers over the sync engine that turn
//! its outcomes into human text and JSON.

use crate::cli::{FetchArgs, PullArgs, PushArgs};
use crate::commands::agent_docs;
use crate::context::Context;
use crate::output::{plural, Output};
use confed_core::error::{ConfedError, ExitCode, Result};
use confed_core::sync::{FetchOptions, PullOptions, PushOptions};
use serde_json::json;
use std::fmt::Write;

pub mod fetch {
    use super::*;

    pub async fn run(ctx: &mut Context, args: &FetchArgs) -> Result<Output> {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;

        let pages = args.pages.iter().map(|p| ctx.resolve_page(p)).collect::<Result<Vec<_>>>()?;

        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome = engine.fetch(ws, &FetchOptions { pages, since: args.since.clone() }).await?;

        let style = &ctx.style;
        let mut human = String::new();
        let _ = writeln!(
            human,
            "Fetched {}, {} unchanged.",
            plural(outcome.fetched, "page", "pages"),
            outcome.unchanged
        );
        if outcome.from_cache > 0 {
            let _ = writeln!(
                human,
                "{}",
                style.dim(&format!(
                    "({} restored from the local cache without a request)",
                    plural(outcome.from_cache, "page", "pages")
                ))
            );
        }
        if outcome.resumed {
            let _ = writeln!(human, "{}", style.dim("(continued an interrupted fetch)"));
        }
        if !outcome.deleted_on_remote.is_empty() {
            let _ = writeln!(
                human,
                "{}",
                style.yellow(&format!(
                    "{} deleted on the server",
                    plural(outcome.deleted_on_remote.len(), "page", "pages")
                ))
            );
        }
        if outcome.comments_refreshed > 0 {
            let _ = writeln!(
                human,
                "Read the comments of {} again; {} changed.",
                plural(outcome.comments_refreshed, "unchanged page", "unchanged pages"),
                outcome.comments_changed.len()
            );
        }
        if outcome.fetched > 0 || !outcome.comments_changed.is_empty() {
            let _ =
                writeln!(human, "{}", style.dim("Run `confed pull` to write the changes to disk."));
        }

        let failures = outcome.failed.len();
        let mut output = Output::from_data(&outcome, human);
        for failure in &outcome.failed {
            output = output.warn(format!("{}: {}", failure.page_id, failure.error));
        }
        if let Some(warning) = outcome.comment_check_warning() {
            output = output.warn(warning);
        }
        if failures > 0 {
            output.exit = ExitCode::Partial;
        }
        Ok(output)
    }
}

pub mod pull {
    use super::*;

    pub async fn run(ctx: &mut Context, args: &PullArgs) -> Result<Output> {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client.clone())?;

        let mut scope = ctx.workspace_paths(&args.paths);
        scope.extend(ctx.workspace_paths(&args.pages));

        // Label and CQL filters resolve to page ids on the server.
        if let Some(label) = &args.label {
            let space = ctx.workspace()?.space_key()?;
            let cql = format!("space = \"{space}\" and label = \"{label}\"");
            scope.extend(search_ids(&client, &cql).await?);
        }
        if let Some(cql) = &args.cql {
            scope.extend(search_ids(&client, cql).await?);
        }

        let opts = PullOptions {
            scope,
            no_fetch: args.no_fetch,
            force: args.force,
            no_merge: args.no_merge,
            reset: args.reset,
            dry_run: args.dry_run,
            with_attachments: !args.no_attachments,
            with_comments: !args.no_comments,
        };

        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome = engine.pull(ws, &opts).await?;

        let style = &ctx.style;
        let mut human = String::new();
        if args.dry_run {
            let _ = writeln!(human, "{}", style.dim("Dry run — nothing was written."));
        }

        for (label, pages) in [
            ("created", &outcome.created),
            ("updated", &outcome.updated),
            ("merged", &outcome.merged),
            ("deleted", &outcome.deleted),
        ] {
            for page in pages {
                // A page whose comments changed and nothing else.
                let what = if page.ops == ["comments"] {
                    style.dim("  (comments)")
                } else {
                    String::new()
                };
                let _ = writeln!(human, "  {:<8} {}{}", label, page.path, what);
            }
        }
        for moved in &outcome.moved {
            let _ = writeln!(human, "  {:<8} {} -> {}", "moved", moved.from, moved.to);
        }
        for page in &outcome.conflicted {
            let _ = writeln!(human, "  {} {}", style.red("conflict"), page.path);
        }
        for page in &outcome.discarded {
            let _ = writeln!(human, "  {} {}", style.yellow("discarded"), page.path);
        }

        if outcome.is_empty() {
            let _ = writeln!(human, "{}", style.green("Already up to date."));
        } else {
            let _ = writeln!(
                human,
                "\n{} created, {} updated, {} merged, {} deleted{}",
                outcome.created.len(),
                outcome.updated.len(),
                outcome.merged.len(),
                outcome.deleted.len(),
                if outcome.attachments_downloaded > 0 {
                    format!(", {} attachments", outcome.attachments_downloaded)
                } else {
                    String::new()
                }
            );
        }

        let conflicts = outcome.conflicted.len();
        if conflicts > 0 {
            let _ = writeln!(
                human,
                "\n{}",
                style.red(&format!(
                    "{} left conflict markers. Edit them, then run `confed resolve <page>`.",
                    plural(conflicts, "page", "pages")
                ))
            );
        }

        let skipped: Vec<String> =
            outcome.skipped_dirty.iter().map(|b| format!("{}: {}", b.path, b.reason)).collect();
        let mut output = Output::from_data(&outcome, human)
            .warn_all(skipped)
            .warn_all(outcome.warnings.iter().cloned());
        if !args.dry_run {
            output = output.warn_all(agent_docs::sync_rules_after(ctx.workspace()?));
        }
        if conflicts > 0 {
            output.exit = ExitCode::Conflict;
        }
        Ok(output)
    }

    async fn search_ids(
        client: &std::sync::Arc<dyn confed_api::ConfluenceClient>,
        cql: &str,
    ) -> Result<Vec<String>> {
        Ok(client.search_cql(cql, 500).await?.into_iter().map(|r| r.page_id.0).collect())
    }
}

pub mod push {
    use super::*;

    pub async fn run(ctx: &mut Context, args: &PushArgs) -> Result<Output> {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;

        let opts = PushOptions {
            scope: ctx.workspace_paths(&args.paths),
            dry_run: args.dry_run,
            allow_delete: args.allow_delete,
            allow_attachment_delete: args.allow_delete,
            message: args.message.clone(),
            with_attachments: !args.no_attachments,
            comments_only: false,
            show_storage: args.show_storage,
            with_comments: !args.no_comments,
        };

        // Show the plan before doing anything irreversible.
        if args.interactive && !args.dry_run {
            if !ctx.is_interactive() {
                return Err(ConfedError::usage_with_hint(
                    "--interactive needs a terminal",
                    "drop --interactive, or use --dry-run to preview instead",
                ));
            }
            let plan = engine.plan_push(ctx.workspace()?, &opts)?;
            if plan.is_empty() {
                return Ok(Output::new(
                    json!({"pushed": [], "created": [], "deleted": []}),
                    "Nothing to push.\n",
                ));
            }
            for op in &plan.ops {
                if ctx.global.yes {
                    continue;
                }
                let question = format!("{:?} {} ({})?", op.kind, op.path, op.ops.join(", "));
                if !crate::prompt::confirm(&question, true)? {
                    return Ok(Output::new(json!({"cancelled": true}), "Cancelled.\n"));
                }
            }
        }

        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome = engine.push(ws, &opts).await?;

        let style = &ctx.style;
        let mut human = String::new();
        if args.dry_run {
            let _ = writeln!(human, "{}", style.dim("Dry run — nothing was uploaded."));
        }
        for page in &outcome.created {
            let _ = writeln!(human, "  {:<8} {}", "created", page.path);
        }
        for page in &outcome.pushed {
            let versions = match (page.from_version, page.to_version) {
                (Some(from), Some(to)) => style.dim(&format!("  (v{from} -> v{to})")),
                _ => String::new(),
            };
            let _ = writeln!(human, "  {:<8} {}{}", "updated", page.path, versions);
        }
        for page in &outcome.deleted {
            let _ = writeln!(human, "  {:<8} {}", "deleted", page.path);
        }
        for file in &outcome.attachments_uploaded {
            let _ = writeln!(human, "  {:<8} {}", "uploaded", file);
        }
        for file in &outcome.attachments_deleted {
            let _ = writeln!(human, "  {:<8} {}", "unlinked", file);
        }
        for skipped in &outcome.skipped {
            let _ = writeln!(
                human,
                "  {:<8} {} — {}",
                style.yellow("skipped"),
                skipped.path,
                skipped.reason
            );
        }
        for failure in &outcome.failed {
            let _ = writeln!(
                human,
                "  {:<8} {} — {}",
                style.red("failed"),
                failure.page_id,
                failure.error
            );
        }

        for (label, ids) in [
            ("comment", &outcome.comments_added),
            ("reply", &outcome.replies_added),
            ("resolved", &outcome.comments_resolved),
        ] {
            for id in ids {
                let _ = writeln!(human, "  {label:<8} {id}");
            }
        }
        for op in &outcome.comments_pending {
            let _ = writeln!(human, "  {:<8} {op}", "would");
        }
        for preview in &outcome.storage {
            let _ = writeln!(
                human,
                "\n{}",
                style
                    .bold(&format!("--- {} ({}): {}", preview.path, preview.page_id, preview.what))
            );
            let _ = writeln!(human, "{}", preview.storage);
        }

        let total = outcome.created.len() + outcome.pushed.len() + outcome.deleted.len();
        let files = outcome.attachments_uploaded.len() + outcome.attachments_deleted.len();
        let comment_work = outcome.comments_added.len()
            + outcome.replies_added.len()
            + outcome.comments_resolved.len()
            + outcome.comments_pending.len();
        if total == 0 && files == 0 && comment_work == 0 && outcome.skipped.is_empty() {
            let _ = writeln!(human, "{}", style.green("Nothing to push."));
        } else if !args.dry_run {
            let attachments = if files > 0 {
                format!(", {}", plural(files, "attachment", "attachments"))
            } else {
                String::new()
            };
            let _ = writeln!(human, "\n{} pushed{}.", plural(total, "page", "pages"), attachments);
        }

        let mut output = Output::from_data(&outcome, human);
        if !args.dry_run {
            output = output.warn_all(agent_docs::sync_rules_after(ctx.workspace()?));
        }
        if !outcome.failed.is_empty() {
            output.exit = ExitCode::Partial;
        } else if outcome
            .skipped
            .iter()
            .any(|s| s.reason.contains("conflict") || s.reason.contains("version"))
        {
            output.exit = ExitCode::Conflict;
        }
        Ok(output)
    }
}
