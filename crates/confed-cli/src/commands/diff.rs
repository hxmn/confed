//! `confed diff` — base vs local by default, `--remote` against the server,
//! `--storage` for the XHTML that push would upload.

use crate::cli::DiffArgs;
use crate::context::Context;
use crate::output::Output;
use confed_converter::ConvertOptions;
use confed_core::error::{ExitCode, Result};
use confed_core::state::PageRecord;
use confed_core::sync::path_matches;
use confed_core::worktree;
use serde::Serialize;
use serde_json::json;
use similar::{ChangeTag, TextDiff};
use std::fmt::Write;

#[derive(Serialize)]
struct PageDiff {
    page_id: String,
    path: String,
    title: String,
    change: &'static str,
    additions: usize,
    deletions: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hunks: Vec<Hunk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frontmatter_changes: Option<serde_json::Value>,
    /// What the server changed since the same base. Only filled by `--base`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    remote_hunks: Vec<Hunk>,
}

#[derive(Serialize)]
struct Hunk {
    header: String,
    lines: Vec<DiffLine>,
}

#[derive(Serialize)]
struct DiffLine {
    tag: &'static str,
    text: String,
}

pub fn run(ctx: &mut Context, args: &DiffArgs) -> Result<Output> {
    let diffs = collect(ctx, args, false)?;
    render(ctx, args, diffs)
}

pub async fn run_remote(ctx: &mut Context, args: &DiffArgs) -> Result<Output> {
    if args.remote {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        engine.fetch(ws, &Default::default()).await?;
    }
    let diffs = collect(ctx, args, args.remote)?;
    render(ctx, args, diffs)
}

fn collect(ctx: &Context, args: &DiffArgs, against_remote: bool) -> Result<Vec<PageDiff>> {
    let ws = ctx.workspace()?;
    let (files, _) = worktree::read_working_files(ws)?;
    let base = ws.state().all_pages()?;
    let remote = ws.state().all_remote()?;

    let scope = ctx.workspace_paths(&args.paths);
    let mut out = Vec::new();
    for local in &files {
        let Some(page_id) = local.file.frontmatter.page_id() else { continue };
        if !in_scope(&scope, &local.path, page_id) {
            continue;
        }
        let Some(base_record) = base.iter().find(|b| b.page_id == page_id) else { continue };

        // The same context the file on disk was rendered with. Anything less —
        // no resolved people, no page links — re-renders the base differently
        // and reports changes the user never made.
        let convert_opts = confed_core::sync::page_convert_options(ws, &local.path);

        let (left_label, left_text) = if against_remote {
            let Some(remote_page) = remote.iter().find(|r| r.page_id == page_id) else { continue };
            let storage = remote_page.storage_body.clone().unwrap_or_default();
            ("remote", render_side(&storage, &convert_opts, args)?)
        } else {
            ("base", render_side(&base_record.storage_body, &convert_opts, args)?)
        };

        // --base adds what the *server* changed since the same base, so both
        // sides of a divergence are visible next to each other.
        let remote_hunks = if args.base {
            match remote.iter().find(|r| r.page_id == page_id) {
                Some(remote_page) => {
                    let storage = remote_page.storage_body.clone().unwrap_or_default();
                    let remote_text = render_side(&storage, &convert_opts, args)?;
                    let base_text = render_side(&base_record.storage_body, &convert_opts, args)?;
                    build_hunks(&TextDiff::from_lines(&base_text, &remote_text))
                }
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let right_text = if args.storage {
            // What push would actually upload.
            build_push_storage(base_record, &local.file.marked_body(), &convert_opts)?
        } else {
            local.file.body.clone()
        };

        let diff = TextDiff::from_lines(&left_text, &right_text);
        let (additions, deletions) = count_changes(&diff);
        let frontmatter_changes = frontmatter_diff(base_record, local);

        if additions == 0 && deletions == 0 && frontmatter_changes.is_none() {
            continue;
        }

        out.push(PageDiff {
            page_id: page_id.to_string(),
            path: local.path.clone(),
            title: local.file.frontmatter.title.clone(),
            change: if against_remote { "differs_from_remote" } else { "modified" },
            additions,
            deletions,
            hunks: if args.name_only || args.stat { Vec::new() } else { build_hunks(&diff) },
            frontmatter_changes,
            remote_hunks,
        });
        let _ = left_label;
    }
    Ok(out)
}

fn render_side(storage: &str, opts: &ConvertOptions, args: &DiffArgs) -> Result<String> {
    if args.storage {
        return Ok(storage.to_string());
    }
    confed_core::sync::comparable_markdown(storage, opts)
}

fn build_push_storage(base: &PageRecord, new_body: &str, opts: &ConvertOptions) -> Result<String> {
    let base_md = confed_converter::storage_to_markdown(&base.storage_body, opts)?;
    let block_map = base
        .block_map
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_else(|| base_md.block_map.clone());

    Ok(confed_converter::markdown_to_storage_patched(
        &base.storage_body,
        &block_map,
        &base_md.markdown,
        new_body,
        opts,
    )
    .unwrap_or_else(|_| confed_converter::markdown_to_storage(new_body, opts).unwrap_or_default()))
}

fn in_scope(patterns: &[String], path: &str, page_id: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|p| p == page_id || p == path || path_matches(p, path))
}

fn count_changes<'a>(diff: &TextDiff<'a, 'a, '_, str>) -> (usize, usize) {
    let mut additions = 0;
    let mut deletions = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, deletions)
}

fn build_hunks<'a>(diff: &TextDiff<'a, 'a, '_, str>) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
        let old = first.old_range().start..last.old_range().end;
        let new = first.new_range().start..last.new_range().end;

        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                lines.push(DiffLine {
                    tag: match change.tag() {
                        ChangeTag::Insert => "+",
                        ChangeTag::Delete => "-",
                        ChangeTag::Equal => " ",
                    },
                    text: change.value().trim_end_matches('\n').to_string(),
                });
            }
        }
        hunks.push(Hunk {
            header: format!(
                "@@ -{},{} +{},{} @@",
                old.start + 1,
                old.len(),
                new.start + 1,
                new.len()
            ),
            lines,
        });
    }
    hunks
}

fn write_hunks(human: &mut String, style: &crate::output::Style, hunks: &[Hunk]) {
    for hunk in hunks {
        let _ = writeln!(human, "{}", style.blue(&hunk.header));
        for line in &hunk.lines {
            let text = format!("{}{}", line.tag, line.text);
            let _ = writeln!(
                human,
                "{}",
                match line.tag {
                    "+" => style.green(&text),
                    "-" => style.red(&text),
                    _ => text,
                }
            );
        }
    }
}

fn frontmatter_diff(base: &PageRecord, local: &worktree::LocalFile) -> Option<serde_json::Value> {
    let fm = &local.file.frontmatter;
    let mut changes = serde_json::Map::new();

    if fm.title != base.title {
        changes.insert("title".into(), json!({"from": base.title, "to": fm.title}));
    }
    if fm.parent_id != base.parent_id {
        changes.insert("parent_id".into(), json!({"from": base.parent_id, "to": fm.parent_id}));
    }

    let added: Vec<&String> = fm.labels.iter().filter(|l| !base.labels.contains(l)).collect();
    let removed: Vec<&String> = base.labels.iter().filter(|l| !fm.labels.contains(l)).collect();
    if !added.is_empty() || !removed.is_empty() {
        changes.insert("labels".into(), json!({"added": added, "removed": removed}));
    }

    (!changes.is_empty()).then_some(serde_json::Value::Object(changes))
}

fn render(ctx: &Context, args: &DiffArgs, diffs: Vec<PageDiff>) -> Result<Output> {
    let style = &ctx.style;
    let mut human = String::new();

    if diffs.is_empty() {
        human.push_str(&style.dim("No differences.\n"));
    } else if args.name_only {
        for diff in &diffs {
            let _ = writeln!(human, "{}", diff.path);
        }
    } else if args.stat {
        for diff in &diffs {
            let _ = writeln!(
                human,
                "{:<50} {} {}",
                diff.path,
                style.green(&format!("+{}", diff.additions)),
                style.red(&format!("-{}", diff.deletions))
            );
        }
        let total_add: usize = diffs.iter().map(|d| d.additions).sum();
        let total_del: usize = diffs.iter().map(|d| d.deletions).sum();
        let _ = writeln!(
            human,
            "{} changed, {} insertions(+), {} deletions(-)",
            crate::output::plural(diffs.len(), "page", "pages"),
            total_add,
            total_del
        );
    } else {
        for diff in &diffs {
            let _ =
                writeln!(human, "\n{}", style.bold(&format!("--- {} ({})", diff.path, diff.title)));
            if let Some(changes) = &diff.frontmatter_changes {
                let _ = writeln!(human, "{}", style.yellow(&format!("frontmatter: {changes}")));
            }
            if args.base {
                let _ = writeln!(human, "{}", style.dim("what you changed since the base:"));
            }
            write_hunks(&mut human, style, &diff.hunks);

            if args.base {
                let _ = writeln!(
                    human,
                    "{}",
                    style.dim("what the server changed since the same base:")
                );
                if diff.remote_hunks.is_empty() {
                    let _ =
                        writeln!(human, "  {}", style.dim("(nothing — the server has not moved)"));
                }
                write_hunks(&mut human, style, &diff.remote_hunks);
            }
        }
    }

    let differences = !diffs.is_empty();
    let mut output = Output::new(json!({ "pages": diffs }), human);
    if args.exit_code && differences {
        output.exit = ExitCode::Differences;
    }
    Ok(output)
}
