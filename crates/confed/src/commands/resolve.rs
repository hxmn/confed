//! `confed resolve` — finish a merge and clear the conflicted state.

use crate::cli::ResolveArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::merge;
use confed_core::state::SyncState;
use confed_core::sync::write_atomic;
use confed_core::worktree::{self, PageState};
use serde_json::json;
use std::fmt::Write;

pub fn run(ctx: &mut Context, args: &ResolveArgs) -> Result<Output> {
    let ws = ctx.workspace()?;
    let scan = worktree::scan(ws)?;
    let conflicted: Vec<_> = scan
        .pages
        .iter()
        .filter(|p| p.state == PageState::Conflicted)
        .collect();

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

        let cleaned = if args.ours || args.theirs {
            take_side(&content, args.ours)
        } else {
            content.clone()
        };

        let markers = merge::conflict_marker_lines(&cleaned);
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

        if cleaned != content {
            write_atomic(&path, &cleaned)?;
        }

        // Recompute the hash so the file's new content becomes the base.
        let file = confed_core::frontmatter::parse(&cleaned, &record.local_path)?;
        let mut updated = record.clone();
        updated.sync_state = SyncState::Clean;
        updated.markdown_hash = file.content_hash();

        let ws = ctx.workspace_mut()?;
        ws.state().upsert_page(&updated)?;
        ws.state().log("resolve", Some(&page_id), None, Some(updated.version), "ok", None)?;

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

    let mut output = Output::new(
        json!({ "resolved": resolved, "remaining": remaining }),
        human,
    );
    if !remaining.is_empty() {
        output.exit = confed_core::ExitCode::State;
    }
    Ok(output)
}

/// Keep one side of every conflict block and drop the markers.
fn take_side(content: &str, ours: bool) -> String {
    #[derive(PartialEq)]
    enum Section {
        Outside,
        Ours,
        Base,
        Theirs,
    }
    let mut section = Section::Outside;
    let mut out = String::with_capacity(content.len());

    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.starts_with("<<<<<<<") {
            section = Section::Ours;
            continue;
        }
        if trimmed.starts_with("|||||||") {
            section = Section::Base;
            continue;
        }
        if trimmed == "=======" && section != Section::Outside {
            section = Section::Theirs;
            continue;
        }
        if trimmed.starts_with(">>>>>>>") {
            section = Section::Outside;
            continue;
        }
        let keep = match section {
            Section::Outside => true,
            Section::Ours => ours,
            Section::Base => false,
            Section::Theirs => !ours,
        };
        if keep {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFLICTED: &str = "intro\n\
        <<<<<<< local\nour line\n||||||| base\noriginal\n=======\ntheir line\n>>>>>>> remote (v9)\n\
        outro\n";

    #[test]
    fn taking_ours_keeps_only_the_local_side() {
        let resolved = take_side(CONFLICTED, true);
        assert_eq!(resolved, "intro\nour line\noutro\n");
        assert!(!merge::has_conflict_markers(&resolved));
    }

    #[test]
    fn taking_theirs_keeps_only_the_remote_side() {
        let resolved = take_side(CONFLICTED, false);
        assert_eq!(resolved, "intro\ntheir line\noutro\n");
    }

    #[test]
    fn the_base_section_is_always_dropped() {
        for ours in [true, false] {
            assert!(!take_side(CONFLICTED, ours).contains("original"));
        }
    }

    #[test]
    fn text_without_conflicts_is_unchanged() {
        let plain = "just\nsome\nlines\n";
        assert_eq!(take_side(plain, true), plain);
    }
}
