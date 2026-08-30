//! `confed status` — a git-style summary, computed entirely from local state.

use crate::cli::StatusArgs;
use crate::context::Context;
use crate::output::{plural, Output};
use confed_core::error::{ExitCode, Result};
use confed_core::worktree::{self, PageState, PageStatus, Scan};
use serde::Serialize;
use serde_json::json;
use std::fmt::Write;

#[derive(Serialize)]
struct StatusEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
    path: String,
    title: String,
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    moved_from: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tampered_fields: Vec<String>,
    /// Inline comments drafted in the body or the sidecar, waiting for a push.
    #[serde(skip_serializing_if = "is_zero")]
    comment_drafts: usize,
    /// Inline comments whose anchor text can no longer be found in the page.
    #[serde(skip_serializing_if = "is_zero")]
    orphaned_comments: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Comment work that is not page work: drafts to post, anchors that are lost.
fn comment_counts(ws: &confed_core::workspace::Workspace, page: &PageStatus) -> (usize, usize) {
    let mut drafts = page.comment_drafts;
    let mut orphaned = 0;
    let Some(page_id) = &page.page_id else { return (drafts, orphaned) };
    if !page.path.is_empty() {
        let sidecar = ws
            .absolute(&confed_core::paths::sidecar_for(&page.path))
            .join(confed_core::comments::COMMENTS_FILENAME);
        if let Ok(text) = std::fs::read_to_string(sidecar) {
            if let Ok(parsed) = confed_core::comments::parse(&text) {
                drafts += parsed.drafts().count();
            }
        }
    }
    if let Ok(records) = ws.state().page_comments(page_id) {
        orphaned = records
            .iter()
            .filter(|c| !c.resolved)
            .filter_map(|c| c.anchor.as_deref())
            .filter_map(|a| serde_json::from_str::<confed_api::InlineAnchor>(a).ok())
            .filter(|a| a.orphaned)
            .count();
    }
    (drafts, orphaned)
}

pub fn run(ctx: &mut Context, args: &StatusArgs) -> Result<Output> {
    let ws = ctx.workspace()?;
    let scan = worktree::scan(ws)?;
    render(ctx, args, scan)
}

pub async fn run_with_fetch(ctx: &mut Context, args: &StatusArgs) -> Result<Output> {
    if args.fetch {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        engine.fetch(ws, &Default::default()).await?;
    }
    let scan = worktree::scan(ctx.workspace()?)?;
    render(ctx, args, scan)
}

fn render(ctx: &Context, args: &StatusArgs, scan: Scan) -> Result<Output> {
    let style = &ctx.style;
    let ws = ctx.workspace()?;
    let space = ws.space_key().unwrap_or_default();
    let last_fetch = ws.state().get_meta("last_fetch_at")?;

    let interesting: Vec<&PageStatus> =
        scan.pages.iter().filter(|p| p.state != PageState::Unchanged).collect();

    let counts: Vec<(usize, usize)> = scan.pages.iter().map(|p| comment_counts(ws, p)).collect();
    let entries: Vec<StatusEntry> = scan
        .pages
        .iter()
        .zip(&counts)
        .map(|(p, &(comment_drafts, orphaned_comments))| StatusEntry {
            page_id: p.page_id.clone(),
            path: p.path.clone(),
            title: p.title.clone(),
            state: p.state.as_str(),
            base_version: p.base_version,
            remote_version: p.remote_version,
            moved_from: p.moved_from.clone(),
            tampered_fields: p.tampering.iter().map(|t| t.field.to_string()).collect(),
            comment_drafts,
            orphaned_comments,
        })
        .collect();

    let result = json!({
        "space": space,
        "last_fetch_at": last_fetch,
        "clean": interesting.is_empty() && scan.unreadable.is_empty(),
        "pages": entries,
        "untracked_files": scan.untracked_files,
        "unreadable": scan.unreadable.iter().map(|(p, e)| json!({"path": p, "error": e})).collect::<Vec<_>>(),
    });

    let human = if args.short {
        render_short(&scan, &counts)
    } else {
        render_long(ctx, &scan, &counts, &space, last_fetch.as_deref())
    };

    let mut output = Output::new(result, human);

    if let Some(warning) = stale_fetch_warning(last_fetch.as_deref()) {
        output = output.warn(warning);
    }
    for (path, error) in &scan.unreadable {
        output = output.warn(format!("{path}: {error}"));
    }

    let differences = !interesting.is_empty();
    if scan.pages.iter().any(|p| p.state == PageState::Conflicted) {
        output.exit = ExitCode::Conflict;
    } else if args.exit_code && differences {
        output.exit = ExitCode::Differences;
    }
    let _ = style;
    Ok(output)
}

fn render_short(scan: &Scan, counts: &[(usize, usize)]) -> String {
    let mut out = String::new();
    for (page, &(drafts, _)) in scan.pages.iter().zip(counts) {
        if page.state == PageState::Unchanged && drafts == 0 {
            continue;
        }
        let code = if page.state == PageState::Unchanged { 'c' } else { page.state.short_code() };
        let mut line = format!("{code} {}", display_path(page));
        if drafts > 0 {
            line.push_str(&format!("  +{drafts} comment"));
        }
        let _ = writeln!(out, "{line}");
    }
    for path in &scan.untracked_files {
        let _ = writeln!(out, "? {path}");
    }
    out
}

fn render_long(
    ctx: &Context,
    scan: &Scan,
    counts: &[(usize, usize)],
    space: &str,
    last_fetch: Option<&str>,
) -> String {
    let style = &ctx.style;
    let mut out = String::new();

    let _ = writeln!(out, "Space {}", style.bold(space));
    match last_fetch {
        Some(ts) => {
            let _ = writeln!(out, "Last fetch {}", style.dim(ts));
        }
        None => {
            let _ = writeln!(out, "{}", style.dim("Never fetched — run `confed fetch`"));
        }
    }

    /// A status section: which state it lists, its heading, and how to color it.
    type Section = (PageState, &'static str, fn(&crate::output::Style, &str) -> String);

    let sections: [Section; 8] = [
        (PageState::Conflicted, "Conflicted (resolve before pushing)", |s, t| s.red(t)),
        (PageState::Diverged, "Diverged (edited on both sides)", |s, t| s.yellow(t)),
        (PageState::Modified, "Modified locally", |s, t| s.green(t)),
        (PageState::LocalNew, "New locally", |s, t| s.green(t)),
        (PageState::LocalDeleted, "Deleted locally", |s, t| s.red(t)),
        (PageState::Behind, "Behind the server", |s, t| s.blue(t)),
        (PageState::RemoteNew, "New on the server", |s, t| s.blue(t)),
        (PageState::RemoteDeleted, "Deleted on the server", |s, t| s.red(t)),
    ];

    let mut any = false;
    for (state, heading, paint) in sections {
        let pages: Vec<&PageStatus> = scan.pages.iter().filter(|p| p.state == state).collect();
        if pages.is_empty() {
            continue;
        }
        any = true;
        let _ = writeln!(out, "\n{}", paint(style, heading));
        for page in pages {
            let mut line = format!("  {}", display_path(page));
            if let (Some(base), Some(remote)) = (page.base_version, page.remote_version) {
                if remote > base {
                    line.push_str(&style.dim(&format!("  (base v{base}, server v{remote})")));
                }
            }
            if let Some(from) = &page.moved_from {
                line.push_str(&style.dim(&format!("  (was {from})")));
            }
            let _ = writeln!(out, "{line}");
        }
    }

    let tampered: Vec<&PageStatus> =
        scan.pages.iter().filter(|p| !p.tampering.is_empty()).collect();
    if !tampered.is_empty() {
        any = true;
        let _ = writeln!(out, "\n{}", style.red("Tool-managed frontmatter was edited"));
        for page in tampered {
            let fields: Vec<&str> = page.tampering.iter().map(|t| t.field).collect();
            let _ = writeln!(out, "  {}  ({})", page.path, fields.join(", "));
        }
        let _ = writeln!(
            out,
            "{}",
            style.dim("  push refuses these; `confed pull --force <page>` rebuilds the block")
        );
    }

    let with_drafts: Vec<(&PageStatus, usize)> = scan
        .pages
        .iter()
        .zip(counts)
        .filter(|(_, &(drafts, _))| drafts > 0)
        .map(|(p, &(drafts, _))| (p, drafts))
        .collect();
    if !with_drafts.is_empty() {
        any = true;
        let _ = writeln!(out, "\n{}", style.green("Comment drafts (posted on push)"));
        for (page, drafts) in with_drafts {
            let _ = writeln!(out, "  {}  {}", display_path(page), style.dim(&format!("+{drafts}")));
        }
    }
    let with_orphans: Vec<(&PageStatus, usize)> = scan
        .pages
        .iter()
        .zip(counts)
        .filter(|(_, &(_, orphaned))| orphaned > 0)
        .map(|(p, &(_, orphaned))| (p, orphaned))
        .collect();
    if !with_orphans.is_empty() {
        let _ = writeln!(out, "\n{}", style.yellow("Inline comments whose text is gone"));
        for (page, orphaned) in with_orphans {
            let _ = writeln!(
                out,
                "  {}  {}",
                display_path(page),
                style.dim(&format!("{orphaned} orphaned"))
            );
        }
        let _ = writeln!(out, "{}", style.dim("  see `confed comment list <page> --inline`"));
    }

    let untracked = &scan.untracked_files;
    if !untracked.is_empty() {
        let _ = writeln!(out, "\n{}", style.dim("Markdown files that are not confed pages"));
        for path in untracked.iter().take(10) {
            let _ = writeln!(out, "  {path}");
        }
        if untracked.len() > 10 {
            let _ = writeln!(out, "  … and {} more", untracked.len() - 10);
        }
    }

    if !any {
        let _ = writeln!(out, "\n{}", style.green("Everything is in sync."));
    } else {
        let modified = scan.pages.iter().filter(|p| p.state.has_local_work()).count();
        let incoming = scan.pages.iter().filter(|p| p.state.has_remote_work()).count();
        let _ = writeln!(
            out,
            "\n{}",
            style.dim(&format!(
                "{} to push, {} to pull",
                plural(modified, "page", "pages"),
                plural(incoming, "page", "pages")
            ))
        );
    }
    out
}

fn display_path(page: &PageStatus) -> String {
    if page.path.is_empty() {
        format!("{} (not yet written)", page.title)
    } else {
        page.path.clone()
    }
}

/// Nudge when the local view of the server is old enough to mislead.
fn stale_fetch_warning(last_fetch: Option<&str>) -> Option<String> {
    let last = last_fetch?;
    let then = chrono::DateTime::parse_from_rfc3339(last).ok()?;
    let age = chrono::Utc::now().signed_duration_since(then.with_timezone(&chrono::Utc));
    (age.num_days() >= 7).then(|| {
        format!(
            "last fetch was {} days ago; run `confed fetch` for an accurate picture",
            age.num_days()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use confed_core::state::now;

    #[test]
    fn stale_fetches_are_flagged_but_recent_ones_are_not() {
        let recent = now();
        assert!(stale_fetch_warning(Some(&recent)).is_none());

        let old = (chrono::Utc::now() - chrono::Duration::days(9)).to_rfc3339();
        let warning = stale_fetch_warning(Some(&old)).unwrap();
        assert!(warning.contains("9 days ago"));

        assert!(stale_fetch_warning(None).is_none());
        assert!(stale_fetch_warning(Some("not a timestamp")).is_none());
    }
}
