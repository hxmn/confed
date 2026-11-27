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

/// Does a scope argument select the page file at `path`?
///
/// A pattern with `*`, `?`, `[` or `{` is a glob: `*` within a segment, `**`
/// across them. Anything else names a page or a folder, and selects it with
/// everything beneath: `Handbook`, `Handbook/` and `Handbook.md` all select
/// `Handbook.md` and `Handbook/…`. `.` (or an empty argument, which is what
/// `.` becomes at the workspace root) selects every page.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }
    if pattern.contains(['*', '?', '[', '{']) {
        return globset::Glob::new(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(path));
    }
    let name = pattern.trim_end_matches('/');
    if name.is_empty() || name == "." {
        return true;
    }
    let stem = name.strip_suffix(".md").unwrap_or(name);
    path == format!("{stem}.md") || path.starts_with(&format!("{stem}/"))
}

/// The scope arguments that select none of `pages` (page id, file path):
/// a typo, or a folder with no page in it. Each command that takes a scope
/// refuses them rather than doing nothing without a word.
pub fn unmatched_scope<'a, 'p>(
    scope: &'a [String],
    pages: impl IntoIterator<Item = (&'p str, &'p str)> + Clone,
) -> Vec<&'a str> {
    scope
        .iter()
        .filter(|pattern| {
            !pages.clone().into_iter().any(|(id, path)| {
                id == pattern.as_str() || (!path.is_empty() && path_matches(pattern, path))
            })
        })
        .map(String::as_str)
        .collect()
}

/// The error for scope arguments that matched nothing: exit 6, naming them.
pub fn no_page_matches(unmatched: &[&str]) -> ConfedError {
    let names: Vec<String> = unmatched.iter().map(|s| format!("\"{s}\"")).collect();
    ConfedError::refused(
        "no_page_matches",
        crate::error::ExitCode::NotFound,
        format!("no page matches {}", names.join(", ")),
        Some(
            "name a page file, a folder of pages, a page id or a glob; `confed status` lists them"
                .into(),
        ),
    )
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
    // Written and synced before it takes the file's place: a crash after the
    // rename must find the new content, not an empty file the rename made
    // visible before its data reached the disk.
    let written = (|| {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfedError::io(format!("writing {}", tmp.display()), e));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfedError::io(format!("renaming into {}", path.display()), e));
    }
    sync_dir(path.parent());
    Ok(())
}

/// Make a rename into `dir` durable. Best effort: Windows cannot open a
/// directory to sync it, and a filesystem that does not support it has
/// nothing to sync.
pub fn sync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(dir) = dir.filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_atomic_write_replaces_the_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Team Handbook.md");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "Фраза 1.").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Фраза 1.");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp file left");
    }

    /// A rename that fails — here the target is a directory — leaves no
    /// temp file beside it for a scan to find.
    #[test]
    fn a_failed_write_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Onboarding.md");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), "x").unwrap();
        assert!(write_atomic(&path, "content").is_err());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Onboarding.md"], "only what was there before");
    }
}
