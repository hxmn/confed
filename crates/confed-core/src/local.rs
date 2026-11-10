//! Changes to a workspace that need no server: moving, renaming and removing
//! a page's file. Each takes the workspace lock and records what it did in
//! `.state.db`, so a push later sends it — `confed mv` and `confed rm` without
//! `--push` are these and nothing more.

use crate::error::{ConfedError, Result};
use crate::paths;
use crate::slug::slugify;
use crate::state::PageRecord;
use crate::sync::{prune_empty_dirs, write_atomic};
use crate::workspace::Workspace;

/// What [`move_page`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalMove {
    pub page_id: String,
    pub from: String,
    pub to: String,
    pub title_changed: bool,
    pub parent_changed: bool,
}

/// Move or rename a page's file to `destination`, a workspace-relative path
/// (`.md` optional). Its sidecar and its children's folder follow it, and
/// its frontmatter takes the new title and parent. The file name is the new
/// title unless `rename_title` is false and the name is the old title's.
pub fn move_page(
    ws: &Workspace,
    page_id: &str,
    destination: &str,
    rename_title: bool,
) -> Result<LocalMove> {
    let destination = paths::checked_relative(destination)?;
    let _lock = ws.lock()?;
    let record = page_record(ws, page_id)?;
    let old_path = record.local_path.clone();

    let raw = destination.trim_end_matches(".md");
    let (dir, name) = match raw.rsplit_once('/') {
        Some((dir, name)) => (Some(dir.to_string()), name.to_string()),
        None => (None, raw.to_string()),
    };
    let new_path = match &dir {
        Some(dir) => format!("{dir}/{}.md", slugify(&name)),
        None => format!("{}.md", slugify(&name)),
    };
    let old_dir = old_path.rsplit_once('/').map(|(d, _)| d.to_string());
    let parent_changed = dir != old_dir;
    // Renaming the file renames the page unless told otherwise.
    let title_changed = rename_title || name != record.title;

    let absolute_new = ws.absolute(&new_path);
    if absolute_new.exists() && new_path != old_path {
        return Err(ConfedError::state(format!("{new_path} already exists")));
    }

    if new_path != old_path {
        if let Some(parent) = absolute_new.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
        }
        // The page, then its sidecar and children directory. Should one of
        // them fail to follow, what already moved goes back: a page apart
        // from its attachments and children is worse than a move refused.
        let mut moved: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
        let steps = [
            (old_path.clone(), new_path.clone(), true),
            (paths::sidecar_for(&old_path), paths::sidecar_for(&new_path), false),
            (paths::children_dir(&old_path), paths::children_dir(&new_path), false),
        ];
        for (from, to, required) in steps {
            let (from_abs, to_abs) = (ws.absolute(&from), ws.absolute(&to));
            if !required && !from_abs.is_dir() {
                continue;
            }
            if let Err(e) = std::fs::rename(&from_abs, &to_abs) {
                for (back_from, back_to) in moved.iter().rev() {
                    let _ = std::fs::rename(back_to, back_from);
                }
                return Err(ConfedError::io(format!("moving {from} to {to}"), e));
            }
            moved.push((from_abs, to_abs));
        }
    }

    // The file's own frontmatter follows its new home.
    let content = std::fs::read_to_string(&absolute_new)
        .map_err(|e| ConfedError::io(format!("reading {new_path}"), e))?;
    let mut file = crate::frontmatter::parse(&content, &new_path)?;
    if title_changed {
        file.frontmatter.title = name;
    }
    if parent_changed {
        file.frontmatter.parent_id = match &dir {
            Some(dir) => ws.state().get_page_by_path(&format!("{dir}.md"))?.map(|p| p.page_id),
            None => None,
        };
    }
    write_atomic(&absolute_new, &file.render()?)?;

    let mut updated = record;
    updated.local_path = new_path.clone();
    updated.slug = new_path
        .rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".md"))
        .unwrap_or(&new_path)
        .to_string();
    ws.state().upsert_page(&updated)?;

    Ok(LocalMove {
        page_id: page_id.to_string(),
        from: old_path,
        to: new_path,
        title_changed,
        parent_changed,
    })
}

/// The page, if it may be removed: one with child pages may not, since
/// deleting it would orphan them on the server.
pub fn removable(ws: &Workspace, page_id: &str) -> Result<PageRecord> {
    Ok(removable_pages(ws, &[page_id.to_string()])?.remove(0))
}

/// The pages, if every one of them may be removed. A parent may go with its
/// children when they all go together; a child left behind refuses it.
pub fn removable_pages(ws: &Workspace, page_ids: &[String]) -> Result<Vec<PageRecord>> {
    let index = ws.state().page_index()?;
    let mut records = Vec::with_capacity(page_ids.len());
    for page_id in page_ids {
        let record = page_record(ws, page_id)?;
        let children: Vec<&str> = index
            .iter()
            .filter(|p| p.parent_id.as_deref() == Some(page_id.as_str()))
            .filter(|p| !page_ids.contains(&p.page_id))
            .map(|p| p.local_path.as_str())
            .collect();
        if !children.is_empty() {
            return Err(ConfedError::state_with_hint(
                format!("{} has {} child page(s)", record.local_path, children.len()),
                format!("remove or move them first: {}", children.join(", ")),
            ));
        }
        records.push(record);
    }
    Ok(records)
}

/// Remove a page's file and sidecar — or, with `keep_local`, nothing on disk
/// — leaving its base record, which is what tells the next push there is a
/// deletion to send.
pub fn remove_page(ws: &Workspace, page_id: &str, keep_local: bool) -> Result<PageRecord> {
    Ok(remove_pages(ws, &[page_id.to_string()], keep_local)?.remove(0))
}

/// [`remove_page`] for several, all checked before any is removed: one that
/// may not go stops the lot, rather than leaving the ones before it removed.
pub fn remove_pages(
    ws: &Workspace,
    page_ids: &[String],
    keep_local: bool,
) -> Result<Vec<PageRecord>> {
    let _lock = ws.lock()?;
    let records = removable_pages(ws, page_ids)?;
    if keep_local {
        return Ok(records);
    }
    for record in &records {
        let path = ws.absolute(&record.local_path);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| ConfedError::io(format!("removing {}", record.local_path), e))?;
        }
        let sidecar = ws.absolute(&paths::sidecar_for(&record.local_path));
        if sidecar.is_dir() {
            std::fs::remove_dir_all(&sidecar).map_err(|e| {
                ConfedError::io(format!("removing {}", paths::sidecar_for(&record.local_path)), e)
            })?;
        }
        // A folder left empty — the parent's children folder, say — goes too.
        if let Some(parent) = path.parent() {
            prune_empty_dirs(ws.root(), parent);
        }
    }
    Ok(records)
}

fn page_record(ws: &Workspace, page_id: &str) -> Result<PageRecord> {
    ws.state().get_page(page_id)?.ok_or_else(|| ConfedError::NotFound(format!("no page {page_id}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{hash_str, now, SyncState};

    fn workspace_with(pages: &[(&str, &str, Option<&str>)]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::create(dir.path()).unwrap();
        for (id, path, parent) in pages {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            let title = path.rsplit('/').next().unwrap().trim_end_matches(".md");
            std::fs::write(&full, format!("---\ntitle: {title}\nlabels: []\n---\n\nФраза 1.\n"))
                .unwrap();
            ws.state()
                .upsert_page(&PageRecord {
                    page_id: (*id).into(),
                    title: title.into(),
                    slug: title.into(),
                    local_path: (*path).into(),
                    parent_id: parent.map(str::to_string),
                    position: None,
                    version: 1,
                    status: "current".into(),
                    labels: Vec::new(),
                    author: None,
                    created_at: None,
                    updated_at: None,
                    storage_body: "<p>x</p>".into(),
                    storage_hash: hash_str("<p>x</p>"),
                    markdown_hash: String::new(),
                    block_map: None,
                    sync_state: SyncState::Clean,
                    synced_at: now(),
                    render_key: String::new(),
                })
                .unwrap();
        }
        (dir, ws)
    }

    /// `rm A B` where B may not go removes neither.
    #[test]
    fn removing_several_checks_them_all_first() {
        let (dir, ws) = workspace_with(&[
            ("1001", "Team Handbook.md", None),
            ("1002", "Onboarding.md", None),
            ("1003", "Onboarding/Week One.md", Some("1002")),
        ]);
        let ids = ["1001".to_string(), "1002".to_string()];
        let err = remove_pages(&ws, &ids, false).unwrap_err();
        assert!(err.to_string().contains("child page"), "{err}");
        assert!(dir.path().join("Team Handbook.md").exists(), "nothing removed");

        // With its child, the parent may go.
        let ids = ["1002".to_string(), "1003".to_string()];
        remove_pages(&ws, &ids, false).unwrap();
        assert!(!dir.path().join("Onboarding.md").exists());
        assert!(!dir.path().join("Onboarding/Week One.md").exists());
    }

    /// A sidecar that cannot follow its page puts the page back.
    #[test]
    fn a_move_whose_sidecar_cannot_follow_is_undone() {
        let (dir, ws) = workspace_with(&[("1001", "Team Handbook.md", None)]);
        let sidecar = dir.path().join(paths::sidecar_for("Team Handbook.md"));
        std::fs::create_dir_all(&sidecar).unwrap();
        // The sidecar's new name is taken by a directory that is not empty.
        let blocked = dir.path().join(paths::sidecar_for("Handbook.md"));
        std::fs::create_dir_all(blocked.join("inside")).unwrap();
        let err = move_page(&ws, "1001", "Handbook.md", false).unwrap_err();
        assert!(err.to_string().contains("moving"), "{err}");
        assert!(dir.path().join("Team Handbook.md").exists(), "the page went back");
        assert!(!dir.path().join("Handbook.md").exists());
        assert!(sidecar.is_dir());
        assert_eq!(ws.state().get_page("1001").unwrap().unwrap().local_path, "Team Handbook.md");
    }
}
