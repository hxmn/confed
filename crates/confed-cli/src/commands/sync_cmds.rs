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
        let opts = FetchOptions {
            pages,
            since: args.since.clone(),
            allow_mass_delete: args.allow_mass_delete,
        };
        let outcome = engine.fetch(ws, &opts).await?;

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
        if outcome.attachments_refreshed > 0 {
            let _ = writeln!(
                human,
                "Listed the attachments of {} again; {} changed.",
                plural(outcome.attachments_refreshed, "unchanged page", "unchanged pages"),
                outcome.attachments_changed.len()
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
        if outcome.fetched > 0
            || !outcome.attachments_changed.is_empty()
            || !outcome.comments_changed.is_empty()
        {
            let _ =
                writeln!(human, "{}", style.dim("Run `confed pull` to write the changes to disk."));
        }

        let failures = outcome.failed.len();
        let mut output = Output::from_data(&outcome, human);
        for failure in &outcome.failed {
            output = output.warn(format!("{}: {}", failure.page_id, failure.error));
        }
        output = output.warn_all(outcome.check_warnings());
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
        let mut truncated = Vec::new();
        if let Some(label) = &args.label {
            let space = ctx.workspace()?.space_key()?;
            let cql = format!("space = \"{space}\" and label = \"{label}\"");
            let (ids, cut) = search_ids(&client, &cql).await?;
            scope.extend(ids);
            truncated.extend(cut.then(|| format!("--label {label}")));
        }
        if let Some(cql) = &args.cql {
            let (ids, cut) = search_ids(&client, cql).await?;
            scope.extend(ids);
            truncated.extend(cut.then(|| "--cql".to_string()));
        }
        // An empty scope means every page, so a filter that matched nothing
        // must stop here: `--label typo --reset` would otherwise discard the
        // local edits of the whole space.
        if scope.is_empty() && (args.label.is_some() || args.cql.is_some()) {
            let outcome =
                confed_core::sync::PullOutcome { dry_run: args.dry_run, ..Default::default() };
            let human = format!("{}\n", ctx.style.yellow("No page matches; nothing was pulled."));
            return Ok(Output::from_data(&outcome, human)
                .warn("no page matches the --label or --cql filter; nothing was pulled"));
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
            allow_mass_delete: args.allow_mass_delete,
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
                // A page whose attachments or comments changed and nothing else.
                let what = if page.ops.is_empty() {
                    String::new()
                } else {
                    style.dim(&format!("  ({})", page.ops.join(", ")))
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
                match (outcome.attachments_downloaded, outcome.attachments_removed) {
                    (0, 0) => String::new(),
                    (downloaded, 0) => format!(", {downloaded} attachments"),
                    (downloaded, removed) =>
                        format!(", {downloaded} attachments ({removed} removed)"),
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
            .warn_all(outcome.warnings.iter().cloned())
            .warn_all(truncated.iter().map(|filter| {
                format!(
                    "{filter} matched more than {SEARCH_LIMIT} pages; only the first \
                     {SEARCH_LIMIT} were pulled — narrow the filter, or pull without one"
                )
            }));
        if !args.dry_run {
            output = output.warn_all(agent_docs::sync_rules_after(ctx.workspace()?));
        }
        if conflicts > 0 {
            output.exit = ExitCode::Conflict;
        }
        Ok(output)
    }

    /// How many matches a `--label` or `--cql` filter pulls at most.
    const SEARCH_LIMIT: usize = 10_000;

    /// The ids a filter matches, and whether there were more than the limit:
    /// one more is asked for, so a cut is known rather than silent.
    async fn search_ids(
        client: &std::sync::Arc<dyn confed_api::ConfluenceClient>,
        cql: &str,
    ) -> Result<(Vec<String>, bool)> {
        let mut ids: Vec<String> = client
            .search_cql(cql, SEARCH_LIMIT + 1)
            .await?
            .into_iter()
            .map(|r| r.page_id.0)
            .collect();
        let cut = ids.len() > SEARCH_LIMIT;
        ids.truncate(SEARCH_LIMIT);
        Ok((ids, cut))
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
            allow_empty: args.allow_empty,
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

        let mut output = Output::from_data(&outcome, human).warn_all(outcome.warnings.clone());
        if !args.dry_run {
            output = output.warn_all(agent_docs::sync_rules_after(ctx.workspace()?));
        }
        if !outcome.cross_space.is_empty() {
            // A move between spaces is not a push: refused, with the way to do it.
            output = output.with_errors(
                outcome
                    .cross_space
                    .iter()
                    .map(|blocked| crate::output::ErrorEntry {
                        code: ExitCode::State.name(),
                        kind: None,
                        message: format!("{}: {}", blocked.path, blocked.reason),
                        hint: Some(
                            "pages move between spaces only with `confed instance mv`, which \
                             keeps both workspaces in step; restore the page's parent_id"
                                .into(),
                        ),
                    })
                    .collect(),
            );
            output.exit = ExitCode::State;
        } else if !outcome.failed.is_empty() {
            output.exit = ExitCode::Partial;
        } else if outcome.skipped.iter().any(|s| s.kind.is_conflict()) {
            output.exit = ExitCode::Conflict;
        }
        Ok(output)
    }
}
