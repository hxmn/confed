//! An instance root: one directory holding a workspace per space of a single
//! Confluence instance.
//!
//! `.confed-instance.db` marks the root, the way `.state.db` marks a
//! workspace. It records which instance the root is for, which spaces are
//! registered and in which directory each lives, and a journal of the
//! operations that span two workspaces — a page moved from one space to
//! another is written to both, and the journal is what lets an interrupted
//! move be finished or rolled back (`confed instance resume` / `abort`).
//!
//! Every member is an ordinary workspace and keeps working on its own. What
//! membership adds is a shared credential (`.confed-instance-session.db`, see
//! [`crate::session`]) found by walking up from a member that has none.

use crate::error::{ConfedError, Result};
use crate::workspace::Workspace;
use confed_api::Flavor;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const INSTANCE_DB_FILENAME: &str = ".confed-instance.db";
pub const SCHEMA_VERSION: u32 = 1;

/// What the root's `.gitignore` keeps out of git: the instance's own
/// databases and every member's local state.
pub const IGNORED_PATTERNS: &[&str] = &[
    INSTANCE_DB_FILENAME,
    crate::session::INSTANCE_SESSION_DB_FILENAME,
    "*/.state.db",
    "*/.session.db",
    "*/.pages.db",
    "*/.confed.lock",
];

const GITIGNORE_HEADER: &str =
    "# confed: instance state, credentials, and each space's local state";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- One row per registered space. `dir` is relative to the root.
CREATE TABLE IF NOT EXISTS spaces (
  space_key TEXT PRIMARY KEY,
  space_id  TEXT,
  dir       TEXT NOT NULL UNIQUE,
  added_at  TEXT NOT NULL,
  enabled   INTEGER NOT NULL DEFAULT 1
);

-- Operations that write to more than one workspace. `state` is the last step
-- that finished; `plan` is everything needed to finish or undo the rest.
CREATE TABLE IF NOT EXISTS journal (
  id         TEXT PRIMARY KEY,
  op         TEXT NOT NULL,
  state      TEXT NOT NULL,
  plan       TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  error      TEXT
);
"#;

/// A registered space.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Member {
    pub space_key: String,
    pub space_id: Option<String>,
    /// Relative to the root, with `/` separators.
    pub dir: String,
    pub added_at: String,
    pub enabled: bool,
}

/// The steps of a cross-space operation, in order. Each is recorded before
/// the next one starts, so the journal always names the last one that
/// finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Planned,
    ServerMoved,
    LocalDstWritten,
    LocalSrcRemoved,
    LinksRewritten,
    Done,
    /// Rolled back with `confed instance abort`, before the server was touched.
    Aborted,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Planned => "planned",
            Step::ServerMoved => "server_moved",
            Step::LocalDstWritten => "local_dst_written",
            Step::LocalSrcRemoved => "local_src_removed",
            Step::LinksRewritten => "links_rewritten",
            Step::Done => "done",
            Step::Aborted => "aborted",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "planned" => Step::Planned,
            "server_moved" => Step::ServerMoved,
            "local_dst_written" => Step::LocalDstWritten,
            "local_src_removed" => Step::LocalSrcRemoved,
            "links_rewritten" => Step::LinksRewritten,
            "done" => Step::Done,
            "aborted" => Step::Aborted,
            other => return Err(ConfedError::Other(format!("unknown journal step `{other}`"))),
        })
    }

    /// Nothing is left to do.
    pub fn is_final(self) -> bool {
        matches!(self, Step::Done | Step::Aborted)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct JournalEntry {
    pub id: String,
    pub op: String,
    pub state: Step,
    pub plan: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct Instance {
    root: PathBuf,
    conn: Connection,
}

impl Instance {
    /// Make `root` an instance root for `base_url`. Refuses a directory that
    /// is itself a workspace, or a root for another instance.
    pub fn create(root: &Path, base_url: &str, flavor: Flavor) -> Result<Self> {
        if root.join(crate::state::STATE_DB_FILENAME).exists() {
            return Err(ConfedError::state_with_hint(
                format!("{} is a workspace, so it cannot also be an instance root", root.display()),
                "make the instance root the directory above it, and run `confed instance adopt` there",
            ));
        }
        std::fs::create_dir_all(root)
            .map_err(|e| ConfedError::io(format!("creating {}", root.display()), e))?;
        let existed = root.join(INSTANCE_DB_FILENAME).exists();
        let instance = Self::open(root)?;
        if existed {
            if let Some(bound) = instance.base_url()? {
                if !same_instance(&bound, base_url) {
                    return Err(ConfedError::state_with_hint(
                        format!("{} is already the instance root for {bound}", root.display()),
                        "use another directory for that instance",
                    ));
                }
                return Ok(instance);
            }
        }
        instance.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        instance.set_meta("base_url", base_url.trim_end_matches('/'))?;
        instance.set_meta("flavor", flavor.as_str())?;
        instance.set_meta("created_at", &crate::state::now())?;
        Ok(instance)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join(INSTANCE_DB_FILENAME);
        let conn = Connection::open(&path)
            .map_err(|e| ConfedError::Other(format!("could not open {}: {e}", path.display())))?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.execute_batch(SCHEMA)?;
        let instance = Self { root: root.to_path_buf(), conn };
        if let Some(version) = instance.get_meta("schema_version")? {
            if version.parse::<u32>().unwrap_or(0) > SCHEMA_VERSION {
                return Err(ConfedError::state_with_hint(
                    format!(
                        "{} was written by a newer confed (schema {version}, this build \
                         supports {SCHEMA_VERSION})",
                        path.display()
                    ),
                    "upgrade confed",
                ));
            }
        }
        Ok(instance)
    }

    /// Walk up from `start` looking for `.confed-instance.db`, like git looks
    /// for `.git`. `None` when no directory above is a root.
    pub fn discover(start: &Path) -> Result<Option<Self>> {
        Ok(match find_root(start) {
            Some(root) => Some(Self::open(&root)?),
            None => None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn base_url(&self) -> Result<Option<String>> {
        self.get_meta("base_url")
    }

    pub fn flavor(&self) -> Result<Option<Flavor>> {
        Ok(self.get_meta("flavor")?.and_then(|f| f.parse().ok()))
    }

    // ---------------------------------------------------------- members ---

    /// Every registered space, by key.
    pub fn members(&self) -> Result<Vec<Member>> {
        let mut stmt = self.conn.prepare(
            "SELECT space_key, space_id, dir, added_at, enabled FROM spaces ORDER BY space_key",
        )?;
        let rows = stmt.query_map([], member_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// The registered spaces that fan-out commands visit.
    pub fn enabled_members(&self) -> Result<Vec<Member>> {
        Ok(self.members()?.into_iter().filter(|m| m.enabled).collect())
    }

    pub fn member(&self, space_key: &str) -> Result<Option<Member>> {
        Ok(self
            .conn
            .query_row(
                "SELECT space_key, space_id, dir, added_at, enabled FROM spaces
                 WHERE space_key = ?1 COLLATE NOCASE",
                [space_key],
                member_from_row,
            )
            .optional()?)
    }

    /// Register a space in `dir` (relative to the root), or update where it is.
    pub fn add_member(&self, space_key: &str, space_id: Option<&str>, dir: &str) -> Result<Member> {
        if let Some(other) = self.members()?.into_iter().find(|m| m.dir == dir) {
            if other.space_key != space_key {
                return Err(ConfedError::state_with_hint(
                    format!("{dir} is already registered for space {}", other.space_key),
                    format!(
                        "choose another directory, or `confed instance remove {}`",
                        other.space_key
                    ),
                ));
            }
        }
        self.conn.execute(
            "INSERT INTO spaces (space_key, space_id, dir, added_at, enabled)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(space_key) DO UPDATE SET
               space_id = COALESCE(excluded.space_id, spaces.space_id), dir = excluded.dir",
            params![space_key, space_id, dir, crate::state::now()],
        )?;
        self.member(space_key)?.ok_or_else(|| ConfedError::Other("registration vanished".into()))
    }

    pub fn remove_member(&self, space_key: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM spaces WHERE space_key = ?1 COLLATE NOCASE", [space_key])?
            > 0)
    }

    pub fn set_enabled(&self, space_key: &str, enabled: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE spaces SET enabled = ?2 WHERE space_key = ?1 COLLATE NOCASE",
            params![space_key, enabled],
        )?;
        Ok(())
    }

    /// Absolute directory of a member.
    pub fn member_dir(&self, member: &Member) -> PathBuf {
        self.root.join(&member.dir)
    }

    pub fn open_workspace(&self, member: &Member) -> Result<Workspace> {
        let dir = self.member_dir(member);
        if !dir.join(crate::state::STATE_DB_FILENAME).exists() {
            return Err(ConfedError::state_with_hint(
                format!(
                    "space {} is registered in {}, which is not a workspace",
                    member.space_key, member.dir
                ),
                format!(
                    "`confed instance clone {key} --force` makes it one again, or \
                     `confed instance remove {key}` forgets it",
                    key = member.space_key
                ),
            ));
        }
        Workspace::open(&dir)
    }

    /// The member whose directory holds `path`, if any.
    pub fn member_for_path(&self, path: &Path) -> Result<Option<Member>> {
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let Ok(relative) = path.strip_prefix(&root) else { return Ok(None) };
        let relative = relative.to_string_lossy().replace('\\', "/");
        Ok(self
            .members()?
            .into_iter()
            .filter(|m| relative == m.dir || relative.starts_with(&format!("{}/", m.dir)))
            .max_by_key(|m| m.dir.len()))
    }

    // ---------------------------------------------------------- journal ---

    /// Record a new operation at [`Step::Planned`]. Returns its id.
    pub fn journal_begin(&self, op: &str, plan: &serde_json::Value) -> Result<String> {
        let id = format!("{op}-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let now = crate::state::now();
        self.conn.execute(
            "INSERT INTO journal (id, op, state, plan, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, op, Step::Planned.as_str(), plan.to_string(), now],
        )?;
        Ok(id)
    }

    /// Record that `step` finished, with the plan as it now stands.
    pub fn journal_advance(&self, id: &str, step: Step, plan: &serde_json::Value) -> Result<()> {
        self.conn.execute(
            "UPDATE journal SET state = ?2, plan = ?3, updated_at = ?4, error = NULL WHERE id = ?1",
            params![id, step.as_str(), plan.to_string(), crate::state::now()],
        )?;
        Ok(())
    }

    /// Note why the operation stopped, leaving its step as it was.
    pub fn journal_fail(&self, id: &str, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE journal SET error = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, error, crate::state::now()],
        )?;
        Ok(())
    }

    /// [`Self::journal_fail`], also saving the plan as it now stands.
    pub fn journal_fail_with_plan(
        &self,
        id: &str,
        error: &str,
        plan: &serde_json::Value,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE journal SET error = ?2, plan = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, error, plan.to_string(), crate::state::now()],
        )?;
        Ok(())
    }

    pub fn journal_entry(&self, id: &str) -> Result<Option<JournalEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, op, state, plan, created_at, updated_at, error FROM journal WHERE id = ?1",
        )?;
        let entry = stmt.query_row([id], journal_from_row).optional()?;
        entry.map(finish_entry).transpose()
    }

    /// Entries that have neither finished nor been aborted, oldest first.
    pub fn journal_unfinished(&self) -> Result<Vec<JournalEntry>> {
        Ok(self.journal_all()?.into_iter().filter(|e| !e.state.is_final()).collect())
    }

    pub fn journal_all(&self) -> Result<Vec<JournalEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, op, state, plan, created_at, updated_at, error FROM journal
             ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], journal_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?.into_iter().map(finish_entry).collect()
    }
}

type RawEntry = (String, String, String, String, String, String, Option<String>);

fn journal_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
}

fn finish_entry(raw: RawEntry) -> Result<JournalEntry> {
    let (id, op, state, plan, created_at, updated_at, error) = raw;
    Ok(JournalEntry {
        id,
        op,
        state: Step::parse(&state)?,
        plan: serde_json::from_str(&plan)?,
        created_at,
        updated_at,
        error,
    })
}

fn member_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Member> {
    Ok(Member {
        space_key: r.get(0)?,
        space_id: r.get(1)?,
        dir: r.get(2)?,
        added_at: r.get(3)?,
        enabled: r.get::<_, i64>(4)? != 0,
    })
}

/// The nearest directory at or above `start` holding `.confed-instance.db`.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if dir.join(INSTANCE_DB_FILENAME).exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Where a space's workspace goes by default: a directory named for its key.
///
/// A personal space's key starts with `~`, which a shell expands to a home
/// directory, so `~alice` goes to `@alice` instead — a name no space key can
/// have, and that never needs quoting.
pub fn default_dir(space_key: &str) -> String {
    match space_key.strip_prefix('~') {
        Some(user) => format!("@{user}"),
        None => space_key.to_string(),
    }
}

/// Whether two base URLs name the same Confluence: scheme, host, port and
/// path, ignoring case in the host and a trailing slash.
pub fn same_instance(a: &str, b: &str) -> bool {
    fn normalize(raw: &str) -> String {
        let raw = raw.trim().trim_end_matches('/');
        match url::Url::parse(raw) {
            Ok(url) => format!(
                "{}://{}:{}{}",
                url.scheme(),
                url.host_str().unwrap_or_default(),
                url.port_or_known_default().unwrap_or_default(),
                url.path().trim_end_matches('/')
            ),
            Err(_) => raw.to_ascii_lowercase(),
        }
    }
    normalize(a) == normalize(b)
}

/// Create or extend the root's `.gitignore`. Returns the lines added.
pub fn ensure_gitignore(root: &Path) -> Result<Vec<String>> {
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let already: Vec<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<String> = IGNORED_PATTERNS
        .iter()
        .filter(|p| !already.iter().any(|line| line == *p || line.trim_start_matches('/') == **p))
        .map(|p| p.to_string())
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

    fn root() -> (tempfile::TempDir, Instance) {
        let dir = tempfile::tempdir().unwrap();
        let instance =
            Instance::create(dir.path(), "https://wiki.example.test/", Flavor::DataCenter).unwrap();
        (dir, instance)
    }

    #[test]
    fn a_root_records_its_instance_and_is_found_from_below() {
        let (dir, instance) = root();
        assert_eq!(instance.base_url().unwrap().as_deref(), Some("https://wiki.example.test"));
        assert_eq!(instance.flavor().unwrap(), Some(Flavor::DataCenter));
        assert!(instance.get_meta("created_at").unwrap().is_some());

        let nested = dir.path().join("DOCS/Team Handbook");
        std::fs::create_dir_all(&nested).unwrap();
        let found = Instance::discover(&nested).unwrap().unwrap();
        assert_eq!(found.root(), dir.path().canonicalize().unwrap());

        let elsewhere = tempfile::tempdir().unwrap();
        assert!(Instance::discover(elsewhere.path()).unwrap().is_none());
    }

    #[test]
    fn a_root_for_one_instance_is_not_taken_over_by_another() {
        let (dir, _) = root();
        assert!(
            Instance::create(dir.path(), "https://wiki.example.test", Flavor::DataCenter).is_ok()
        );
        let err =
            Instance::create(dir.path(), "https://wiki.corp", Flavor::DataCenter).unwrap_err();
        assert_eq!(err.exit_code(), crate::ExitCode::State);
    }

    #[test]
    fn a_workspace_cannot_be_a_root() {
        let dir = tempfile::tempdir().unwrap();
        Workspace::create(dir.path()).unwrap();
        let err = Instance::create(dir.path(), "https://wiki.example.test", Flavor::DataCenter)
            .unwrap_err();
        assert!(err.to_string().contains("is a workspace"), "{err}");
    }

    #[test]
    fn members_are_registered_once_per_directory() {
        let (dir, instance) = root();
        instance.add_member("DOCS", Some("500"), "DOCS").unwrap();
        instance.add_member("ENG", None, "ENG").unwrap();
        // Registering again moves it, and keeps the id it already had.
        let docs = instance.add_member("DOCS", None, "Docs").unwrap();
        assert_eq!(docs.space_id.as_deref(), Some("500"));
        assert_eq!(docs.dir, "Docs");

        let err = instance.add_member("OPS", None, "ENG").unwrap_err();
        assert!(err.to_string().contains("already registered for space ENG"), "{err}");

        assert_eq!(instance.member("docs").unwrap().unwrap().space_key, "DOCS");
        instance.set_enabled("ENG", false).unwrap();
        let keys: Vec<String> =
            instance.enabled_members().unwrap().into_iter().map(|m| m.space_key).collect();
        assert_eq!(keys, ["DOCS"]);

        std::fs::create_dir_all(dir.path().join("Docs/Team Handbook")).unwrap();
        let found = instance.member_for_path(&dir.path().join("Docs/Team Handbook")).unwrap();
        assert_eq!(found.unwrap().space_key, "DOCS");
        assert!(instance.member_for_path(dir.path()).unwrap().is_none());

        assert!(instance.remove_member("ENG").unwrap());
        assert!(!instance.remove_member("ENG").unwrap());
    }

    #[test]
    fn the_journal_names_the_last_finished_step() {
        let (_dir, instance) = root();
        let plan = serde_json::json!({ "pages": ["1001"] });
        let id = instance.journal_begin("mv", &plan).unwrap();
        assert!(id.starts_with("mv-"));
        assert_eq!(instance.journal_unfinished().unwrap().len(), 1);

        instance.journal_advance(&id, Step::ServerMoved, &plan).unwrap();
        instance.journal_fail(&id, "network down").unwrap();
        let entry = instance.journal_entry(&id).unwrap().unwrap();
        assert_eq!(entry.state, Step::ServerMoved);
        assert_eq!(entry.error.as_deref(), Some("network down"));

        instance.journal_advance(&id, Step::Done, &plan).unwrap();
        assert!(instance.journal_unfinished().unwrap().is_empty());
        assert!(instance.journal_entry(&id).unwrap().unwrap().error.is_none());
        assert!(Step::Planned < Step::ServerMoved && Step::LinksRewritten < Step::Done);
    }

    #[test]
    fn personal_spaces_get_a_directory_a_shell_leaves_alone() {
        assert_eq!(default_dir("DOCS"), "DOCS");
        assert_eq!(default_dir("~alice"), "@alice");
    }

    #[test]
    fn the_root_gitignore_covers_every_members_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "site/\n").unwrap();
        let added = ensure_gitignore(dir.path()).unwrap();
        assert_eq!(added.len(), IGNORED_PATTERNS.len());
        let text = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(text.starts_with("site/\n"));
        assert!(text.contains("*/.session.db"));
        assert!(ensure_gitignore(dir.path()).unwrap().is_empty());
    }
}
