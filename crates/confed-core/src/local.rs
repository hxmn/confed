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
        std::fs::rename(ws.absolute(&old_path), &absolute_new)
            .map_err(|e| ConfedError::io(format!("moving {old_path}"), e))?;
        // The sidecar and the children directory follow the page.
        for (from, to) in [
            (paths::sidecar_for(&old_path), paths::sidecar_for(&new_path)),
            (paths::children_dir(&old_path), paths::children_dir(&new_path)),
        ] {
            let from_abs = ws.absolute(&from);
            if from_abs.is_dir() {
                let _ = std::fs::rename(from_abs, ws.absolute(&to));
            }
        }
    }

    // The file's own frontmatter follows its new home.
    let content = std::fs::read_to_string(&absolute_new)
        .map_err(|e| ConfedError::io(format!("reading {new_path}"), e))?;
    let mut file = crate::frontmatter::parse(&content, &new_path)?;
    if title_changed {
        file.frontmatter.title = name.clone();
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
    let record = page_record(ws, page_id)?;
    let children: Vec<String> = ws
        .state()
        .page_index()?
        .into_iter()
        .filter(|p| p.parent_id.as_deref() == Some(page_id))
        .map(|p| p.local_path)
        .collect();
    if !children.is_empty() {
        return Err(ConfedError::state_with_hint(
            format!("{} has {} child page(s)", record.local_path, children.len()),
            format!("remove or move them first: {}", children.join(", ")),
        ));
    }
    Ok(record)
}

/// Remove a page's file and sidecar — or, with `keep_local`, nothing on disk
/// — leaving its base record, which is what tells the next push there is a
/// deletion to send.
pub fn remove_page(ws: &Workspace, page_id: &str, keep_local: bool) -> Result<PageRecord> {
    let _lock = ws.lock()?;
    let record = removable(ws, page_id)?;
    if keep_local {
        return Ok(record);
    }
    let path = ws.absolute(&record.local_path);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| ConfedError::io(format!("removing {}", record.local_path), e))?;
    }
    let sidecar = ws.absolute(&paths::sidecar_for(&record.local_path));
    if sidecar.is_dir() {
        let _ = std::fs::remove_dir_all(sidecar);
    }
    // A folder left empty — the parent's children folder, say — goes too.
    if let Some(parent) = path.parent() {
        prune_empty_dirs(ws.root(), parent);
    }
    Ok(record)
}

fn page_record(ws: &Workspace, page_id: &str) -> Result<PageRecord> {
    ws.state().get_page(page_id)?.ok_or_else(|| ConfedError::NotFound(format!("no page {page_id}")))
}
