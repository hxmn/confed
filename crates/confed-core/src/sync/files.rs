//! Writing files: atomically, tidily, and only where a scope says.

use super::*;

/// Remove `dir` and each parent above it while they are empty, stopping at the
/// workspace root: a page's children folder goes with its last child.
pub fn prune_empty_dirs(root: &Path, dir: &Path) {
    let mut current = dir.to_path_buf();
    while current.starts_with(root) && current != root {
        let empty = std::fs::read_dir(&current).map(|mut d| d.next().is_none()).unwrap_or(false);
        if !empty || std::fs::remove_dir(&current).is_err() {
            break;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
}

/// Glob-ish matching for scope arguments: `*` within a segment, `**` across them.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }
    match globset::Glob::new(pattern) {
        Ok(glob) => glob.compile_matcher().is_match(path),
        // A plain prefix like `Handbook` selects the subtree.
        Err(_) => path.starts_with(pattern),
    }
}

/// What goes in a `storage.xml` sidecar: the page's markup, laid out across
/// lines and newline-terminated.
///
/// The file exists to be read and diffed, and Confluence ships a page body as a
/// single line thousands of bytes long. Formatting only moves whitespace between
/// block-level siblings — see [`confed_converter::pretty`] — and nothing reads the
/// file back, so the sidecar stays a faithful copy of what the server has.
pub(super) fn storage_file_text(storage: &str) -> String {
    let body = confed_converter::pretty::format(storage);
    if body.is_empty() {
        return body;
    }
    format!("{body}\n")
}

/// Write via a temp file + rename so an interrupted write never truncates a page.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
    }
    let tmp = path.with_extension(format!(
        "{}.confed-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, content)
        .map_err(|e| ConfedError::io(format!("writing {}", tmp.display()), e))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| ConfedError::io(format!("renaming into {}", path.display()), e))?;
    Ok(())
}
