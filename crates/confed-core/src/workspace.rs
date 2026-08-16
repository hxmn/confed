//! A confed workspace: a directory bound to one Confluence space.

use crate::error::{ConfedError, Result};
use crate::lock::WorkspaceLock;
use crate::session::SESSION_DB_FILENAME;
use crate::session::{Session, SessionStore};
use crate::state::{StateDb, STATE_DB_FILENAME};
use confed_api::Flavor;
use std::path::{Path, PathBuf};

/// Files confed manages that must never be committed to git.
pub const IGNORED_FILES: &[&str] = &[STATE_DB_FILENAME, SESSION_DB_FILENAME, ".confed.lock"];

const GITIGNORE_HEADER: &str = "# confed: local sync state and credentials";

#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    state: StateDb,
}

impl Workspace {
    /// Open the workspace rooted at `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        let state = StateDb::open(dir)?;
        Ok(Self { root: dir.to_path_buf(), state })
    }

    /// Walk up from `start` looking for a `.state.db`, like git looks for `.git`.
    pub fn discover(start: &Path) -> Result<Self> {
        let mut dir = start
            .canonicalize()
            .map_err(|e| ConfedError::io(format!("resolving {}", start.display()), e))?;
        loop {
            if dir.join(STATE_DB_FILENAME).exists() {
                return Self::open(&dir);
            }
            if !dir.pop() {
                return Err(ConfedError::not_initialized());
            }
        }
    }

    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|e| ConfedError::io(format!("creating {}", dir.display()), e))?;
        let state = StateDb::create(dir)?;
        Ok(Self { root: dir.to_path_buf(), state })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state(&self) -> &StateDb {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut StateDb {
        &mut self.state
    }

    pub fn lock(&self) -> Result<WorkspaceLock> {
        WorkspaceLock::acquire(&self.root)
    }

    pub fn session_store(&self) -> Result<SessionStore> {
        SessionStore::open(&self.root)
    }

    pub fn session(&self) -> Result<Option<Session>> {
        self.session_store()?.load()
    }

    /// The space this directory is bound to.
    pub fn space_key(&self) -> Result<String> {
        self.state.get_meta("space_key")?.ok_or_else(|| {
            ConfedError::state_with_hint(
                "this workspace is not bound to a space",
                "run `confed init --space <KEY>`",
            )
        })
    }

    pub fn space_numeric_id(&self) -> Result<Option<String>> {
        self.state.get_meta("space_id")
    }

    pub fn base_url(&self) -> Result<Option<String>> {
        self.state.get_meta("base_url")
    }

    pub fn flavor(&self) -> Result<Option<Flavor>> {
        Ok(self.state.get_meta("flavor")?.and_then(|f| f.parse().ok()))
    }

    /// Absolute path of a workspace-relative page path.
    pub fn absolute(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Workspace-relative form of an absolute path, if it is inside.
    pub fn relative(&self, path: &Path) -> Option<String> {
        let canonical_root = self.root.canonicalize().ok()?;
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        canonical.strip_prefix(&canonical_root).ok().map(|p| p.to_string_lossy().replace('\\', "/"))
    }

    /// Stored settings that feed the config resolver's third precedence step.
    pub fn stored_config(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut out = std::collections::BTreeMap::new();
        for (key, value) in self.state.all_meta()? {
            match key.as_str() {
                "space_key" => {
                    out.insert("space".to_string(), value);
                }
                "base_url" | "flavor" | "concurrency" | "editor" => {
                    out.insert(key, value);
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Create or extend `.gitignore` so local state and credentials stay out of git.
///
/// `.state.db` is ignored deliberately: it holds full page bodies, is rebuilt by
/// `confed init` + `confed fetch`, and would conflict on every sync if committed.
/// Returns the lines that were added.
pub fn ensure_gitignore(root: &Path) -> Result<Vec<String>> {
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    let already: Vec<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<String> = IGNORED_FILES
        .iter()
        .filter(|f| !already.iter().any(|line| line == *f || line.trim_start_matches('/') == **f))
        .map(|f| f.to_string())
        .collect();

    if missing.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = existing.clone();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.contains(GITIGNORE_HEADER) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(GITIGNORE_HEADER);
        out.push('\n');
    }
    for line in &missing {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(&path, out)
        .map_err(|e| ConfedError::io(format!("writing {}", path.display()), e))?;
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_walks_up_like_git() {
        let dir = tempfile::tempdir().unwrap();
        Workspace::create(dir.path()).unwrap();
        let nested = dir.path().join("Handbook/Onboarding");
        std::fs::create_dir_all(&nested).unwrap();

        let ws = Workspace::discover(&nested).unwrap();
        assert_eq!(ws.root().canonicalize().unwrap(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn discovery_outside_a_workspace_explains_init() {
        let dir = tempfile::tempdir().unwrap();
        let err = Workspace::discover(dir.path()).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::State);
        assert!(err.hint().unwrap().contains("confed init"));
    }

    #[test]
    fn gitignore_is_created_with_every_managed_file() {
        let dir = tempfile::tempdir().unwrap();
        let added = ensure_gitignore(dir.path()).unwrap();
        assert_eq!(added.len(), IGNORED_FILES.len());

        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        for f in IGNORED_FILES {
            assert!(content.contains(f), "missing {f} in {content}");
        }
    }

    #[test]
    fn gitignore_is_extended_not_clobbered_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n*.tmp\n").unwrap();

        ensure_gitignore(dir.path()).unwrap();
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains("target/"), "existing entries must survive");
        assert!(content.contains(STATE_DB_FILENAME));

        // Running again adds nothing.
        assert!(ensure_gitignore(dir.path()).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(), content);
    }

    #[test]
    fn gitignore_respects_entries_already_written_with_a_leading_slash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/.session.db\n").unwrap();
        let added = ensure_gitignore(dir.path()).unwrap();
        assert!(!added.iter().any(|l| l == SESSION_DB_FILENAME));
    }

    #[test]
    fn relative_paths_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::create(dir.path()).unwrap();
        let file = dir.path().join("Handbook/Onboarding.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();

        assert_eq!(ws.relative(&file).as_deref(), Some("Handbook/Onboarding.md"));
        assert_eq!(ws.absolute("Handbook/Onboarding.md"), file);
    }

    #[test]
    fn space_binding_is_required_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::create(dir.path()).unwrap();
        assert!(ws.space_key().is_err());

        ws.state().set_meta("space_key", "DOCS").unwrap();
        assert_eq!(ws.space_key().unwrap(), "DOCS");
        assert_eq!(ws.stored_config().unwrap().get("space").map(String::as_str), Some("DOCS"));
    }
}
