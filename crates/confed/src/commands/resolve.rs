//! `confed resolve` — finish a merge and clear the conflicted state.

use crate::cli::ResolveArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::merge;
use confed_core::state::{PageRecord, SyncState};
use confed_core::sync::write_atomic;
use confed_core::workspace::Workspace;
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

/// One conflict block, as confed writes it:
///
/// ```text
/// <<<<<<< local
/// ours
/// ||||||| base
/// base
/// =======
/// theirs
/// >>>>>>> remote (v9)
/// ```
///
/// Each side keeps its line terminators, so the pieces concatenate back into
/// the file byte for byte.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConflictHunk {
    pub ours: String,
    /// Empty unless the merge used the diff3 style (confed always does).
    pub base: String,
    pub theirs: String,
    /// Text after `>>>>>>> `, e.g. `remote (v9, edited by Ada)`.
    pub label: String,
}

/// A file split into plain text and conflict blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Conflict(ConflictHunk),
}

/// Split a merged file into text and conflict blocks.
///
/// Unterminated blocks (a `<<<<<<<` with no `>>>>>>>`) are still returned as a
/// conflict, so the resolver shows something rather than silently dropping the
/// tail of the file.
pub fn split_conflicts(content: &str) -> Vec<Segment> {
    #[derive(PartialEq)]
    enum Section {
        Outside,
        Ours,
        Base,
        Theirs,
    }

    let mut section = Section::Outside;
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut hunk = ConflictHunk::default();

    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.starts_with("<<<<<<<") {
            if !text.is_empty() {
                segments.push(Segment::Text(std::mem::take(&mut text)));
            }
            hunk = ConflictHunk::default();
            section = Section::Ours;
            continue;
        }
        if trimmed.starts_with("|||||||") && section != Section::Outside {
            section = Section::Base;
            continue;
        }
        if trimmed == "=======" && section != Section::Outside {
            section = Section::Theirs;
            continue;
        }
        if trimmed.starts_with(">>>>>>>") && section != Section::Outside {
            hunk.label = trimmed.trim_start_matches('>').trim().to_string();
            segments.push(Segment::Conflict(std::mem::take(&mut hunk)));
            section = Section::Outside;
            continue;
        }
        match section {
            Section::Outside => text.push_str(line),
            Section::Ours => hunk.ours.push_str(line),
            Section::Base => hunk.base.push_str(line),
            Section::Theirs => hunk.theirs.push_str(line),
        }
    }

    if section != Section::Outside {
        segments.push(Segment::Conflict(hunk));
    }
    if !text.is_empty() {
        segments.push(Segment::Text(text));
    }
    segments
}

/// Keep one side of every conflict block and drop the markers.
fn take_side(content: &str, ours: bool) -> String {
    let mut out = String::with_capacity(content.len());
    for segment in split_conflicts(content) {
        match segment {
            Segment::Text(text) => out.push_str(&text),
            Segment::Conflict(hunk) => out.push_str(if ours { &hunk.ours } else { &hunk.theirs }),
        }
    }
    out
}

/// Write a resolved page and clear its conflicted state.
///
/// Returns the lines that still carry conflict markers; when that list is not
/// empty nothing is written and the page stays conflicted. The TUI resolver and
/// `confed resolve` both go through here so they agree on what "resolved" means.
pub fn finish_resolution(
    ws: &mut Workspace,
    record: &PageRecord,
    original: &str,
    resolved: &str,
) -> Result<Vec<usize>> {
    let markers = merge::conflict_marker_lines(resolved);
    if !markers.is_empty() {
        return Ok(markers);
    }

    if resolved != original {
        write_atomic(&ws.absolute(&record.local_path), resolved)?;
    }

    // Parse to be sure the resolution is still a valid page, but leave
    // `markdown_hash` pointing at what the last sync wrote: the resolved text is
    // a local edit on top of it, so `status` says "modified" and `push` has
    // something to send. Adopting the resolved hash here would mark the page
    // clean and silently strand the merge on disk.
    confed_core::frontmatter::parse(resolved, &record.local_path)?;
    let mut updated = record.clone();
    updated.sync_state = SyncState::Clean;

    ws.state().upsert_page(&updated)?;
    ws.state().log("resolve", Some(&record.page_id), None, Some(updated.version), "ok", None)?;
    Ok(Vec::new())
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
        assert_eq!(split_conflicts(plain), [Segment::Text(plain.into())]);
    }

    /// A page file with one conflict in its body, as `pull` leaves it.
    fn merged_page() -> String {
        use confed_core::frontmatter::{Frontmatter, Managed, MarkdownFile};
        let mut managed = Managed::new("1", "DOCS", 2);
        managed.status = "current".into();
        MarkdownFile::new(
            Frontmatter {
                title: "Runbook".into(),
                labels: Vec::new(),
                parent_id: None,
                managed: Some(managed),
                extra: Default::default(),
            },
            "<<<<<<< local\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> remote (v2)\n",
        )
        .render()
        .expect("render")
    }

    fn conflicted_workspace() -> (tempfile::TempDir, Workspace, PageRecord, String) {
        let dir = tempfile::tempdir().expect("temp dir");
        let ws = Workspace::create(dir.path()).expect("workspace");
        let page = merged_page();
        std::fs::write(dir.path().join("Runbook.md"), &page).expect("write");

        let merged = confed_core::frontmatter::parse(&page, "Runbook.md").expect("parse");
        let record = PageRecord {
            page_id: "1".into(),
            title: "Runbook".into(),
            slug: "Runbook".into(),
            local_path: "Runbook.md".into(),
            parent_id: None,
            position: None,
            version: 2,
            status: "current".into(),
            labels: Vec::new(),
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: "<p>theirs</p>".into(),
            storage_hash: "sh".into(),
            markdown_hash: merged.content_hash(),
            block_map: None,
            sync_state: SyncState::Conflicted,
            synced_at: confed_core::state::now(),
            render_key: String::new(),
        };
        ws.state().upsert_page(&record).expect("upsert");
        (dir, ws, record, page)
    }

    /// Resolving clears the conflict, but the merge result is a local edit that
    /// still has to reach the server — the page must not come back "clean".
    #[test]
    fn a_resolved_page_stays_pushable() {
        let (_dir, mut ws, record, page) = conflicted_workspace();
        let resolved = take_side(&page, true);

        let markers = finish_resolution(&mut ws, &record, &page, &resolved).unwrap();
        assert!(markers.is_empty());

        let stored = ws.state().get_page("1").unwrap().unwrap();
        assert_eq!(stored.sync_state, SyncState::Clean);
        assert_eq!(stored.markdown_hash, record.markdown_hash, "the base is what pull wrote");

        let scan = worktree::scan(&ws).unwrap();
        assert_eq!(scan.pages[0].state, PageState::Modified);
        assert!(!merge::has_conflict_markers(
            &std::fs::read_to_string(ws.absolute("Runbook.md")).unwrap()
        ));
    }

    #[test]
    fn a_file_that_still_has_markers_is_reported_and_left_alone() {
        let (_dir, mut ws, record, page) = conflicted_workspace();
        let markers = finish_resolution(&mut ws, &record, &page, &page).unwrap();
        assert_eq!(markers.len(), 3, "one `<<<`, one `|||`, one `>>>`: {markers:?}");
        assert_eq!(
            ws.state().get_page("1").unwrap().unwrap().sync_state,
            SyncState::Conflicted,
            "nothing is cleared while the file still has markers"
        );
    }

    #[test]
    fn the_splitter_understands_the_markers_confed_emits() {
        let segments = split_conflicts(CONFLICTED);
        assert_eq!(
            segments,
            [
                Segment::Text("intro\n".into()),
                Segment::Conflict(ConflictHunk {
                    ours: "our line\n".into(),
                    base: "original\n".into(),
                    theirs: "their line\n".into(),
                    label: "remote (v9)".into(),
                }),
                Segment::Text("outro\n".into()),
            ]
        );
    }

    #[test]
    fn several_conflicts_are_split_independently() {
        let text = "a\n<<<<<<< local\nx\n||||||| base\nb\n=======\ny\n>>>>>>> remote\n\
                    mid\n<<<<<<< local\np\n||||||| base\nq\n=======\nr\n>>>>>>> remote (v2)\nend\n";
        let hunks: Vec<_> = split_conflicts(text)
            .into_iter()
            .filter_map(|s| match s {
                Segment::Conflict(h) => Some(h),
                Segment::Text(_) => None,
            })
            .collect();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].ours, "x\n");
        assert_eq!(hunks[1].theirs, "r\n");
        assert_eq!(hunks[1].label, "remote (v2)");
    }

    /// An empty side is common: one party deleted the lines the other edited.
    #[test]
    fn empty_sides_survive_the_round_trip() {
        let text = "<<<<<<< local\n||||||| base\nold\n=======\nnew\n>>>>>>> remote (v3)\n";
        let segments = split_conflicts(text);
        let Segment::Conflict(hunk) = &segments[0] else { panic!("expected a conflict") };
        assert!(hunk.ours.is_empty());
        assert_eq!(hunk.theirs, "new\n");
        assert_eq!(take_side(text, true), "");
        assert_eq!(take_side(text, false), "new\n");
    }
}
