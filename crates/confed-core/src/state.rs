//! `.state.db` — the last state confed observed on the server.
//!
//! Two tables carry the sync model: `pages` is the **base** (the version a
//! working file was materialized from, and the merge base for 3-way merges) and
//! `remote_pages` is what `fetch` last saw (the "remote-tracking" copy). Base is
//! only ever advanced to a state the server confirmed.

use crate::error::{ConfedError, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const STATE_DB_FILENAME: &str = ".state.db";
pub const SCHEMA_VERSION: u32 = 2;

/// Adds the resolved-people cache and a fingerprint of everything a page's
/// rendering depended on, so pull can re-render when any of it changes.
const SCHEMA_V2: &str = r#"
CREATE TABLE users (
  id            TEXT PRIMARY KEY,
  id_attr       TEXT NOT NULL,
  username      TEXT,
  display_name  TEXT NOT NULL,
  profile_url   TEXT NOT NULL,
  fetched_at    TEXT NOT NULL
);

ALTER TABLE pages ADD COLUMN render_key TEXT NOT NULL DEFAULT '';
"#;

/// Tables added since, in a form any build can live with: created when
/// missing, and never looked at by a confed that does not know them. That is
/// what lets them arrive without a schema version of their own.
const SCHEMA_ADDITIONS: &str = r#"
-- Attachments the server no longer lists, whose local copies a pull has yet to
-- remove. `sha256` is the copy confed last wrote, so a file changed since is
-- not taken for it.
CREATE TABLE IF NOT EXISTS removed_attachments (
  page_id  TEXT NOT NULL,
  filename TEXT NOT NULL,
  sha256   TEXT,
  PRIMARY KEY (page_id, filename)
);
"#;

const SCHEMA_V1: &str = r#"
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE pages (
  page_id       TEXT PRIMARY KEY,
  title         TEXT NOT NULL,
  slug          TEXT NOT NULL,
  local_path    TEXT NOT NULL,
  parent_id     TEXT,
  position      INTEGER,
  version       INTEGER NOT NULL,
  status        TEXT NOT NULL,
  labels        TEXT NOT NULL DEFAULT '[]',
  author        TEXT,
  created_at    TEXT,
  updated_at    TEXT,
  storage_body  BLOB NOT NULL,
  storage_hash  TEXT NOT NULL,
  markdown_hash TEXT NOT NULL,
  block_map     TEXT,
  sync_state    TEXT NOT NULL DEFAULT 'clean',
  synced_at     TEXT NOT NULL
);
-- Deliberately not UNIQUE: while a pull is rewriting a set of pages, two of
-- them can transiently claim the same path (two pages swapping titles is the
-- clearest case). Uniqueness is enforced by the filesystem, not by this index.
CREATE INDEX pages_local_path ON pages(local_path);

CREATE TABLE remote_pages (
  page_id      TEXT PRIMARY KEY,
  title        TEXT NOT NULL,
  parent_id    TEXT,
  position     INTEGER,
  version      INTEGER NOT NULL,
  status       TEXT NOT NULL,
  labels       TEXT NOT NULL DEFAULT '[]',
  author       TEXT,
  created_at   TEXT,
  updated_at   TEXT,
  storage_body BLOB,
  storage_hash TEXT,
  fetched_at   TEXT NOT NULL,
  deleted      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE attachments (
  attachment_id TEXT PRIMARY KEY,
  page_id       TEXT NOT NULL,
  filename      TEXT NOT NULL,
  media_type    TEXT,
  file_size     INTEGER,
  version       INTEGER NOT NULL,
  sha256        TEXT,
  downloaded    INTEGER NOT NULL DEFAULT 0,
  UNIQUE (page_id, filename)
);

CREATE TABLE comments (
  comment_id        TEXT PRIMARY KEY,
  page_id           TEXT NOT NULL,
  parent_comment_id TEXT,
  kind              TEXT NOT NULL,
  author            TEXT,
  created_at        TEXT,
  body_storage      BLOB,
  body_markdown     TEXT NOT NULL,
  resolved          INTEGER NOT NULL DEFAULT 0,
  anchor            TEXT,
  synced_at         TEXT
);
CREATE INDEX comments_page ON comments(page_id);

CREATE TABLE fetch_queue (
  page_id TEXT PRIMARY KEY,
  needs   TEXT NOT NULL,
  done    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sync_log (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  ts           TEXT NOT NULL,
  op           TEXT NOT NULL,
  page_id      TEXT,
  from_version INTEGER,
  to_version   INTEGER,
  result       TEXT NOT NULL,
  detail       TEXT
);
"#;

/// The base record for a page: what the working file was materialized from.
#[derive(Clone, Debug, PartialEq)]
pub struct PageRecord {
    pub page_id: String,
    pub title: String,
    pub slug: String,
    pub local_path: String,
    pub parent_id: Option<String>,
    pub position: Option<i64>,
    pub version: u32,
    pub status: String,
    pub labels: Vec<String>,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Storage-format body of the base version, uncompressed in memory.
    pub storage_body: String,
    pub storage_hash: String,
    pub markdown_hash: String,
    pub block_map: Option<String>,
    pub sync_state: SyncState,
    pub synced_at: String,
    /// Fingerprint of everything the Markdown on disk was rendered from: the
    /// converter version and the people the page mentions. When it no longer
    /// matches, the file is out of date even though neither side changed, and
    /// pull re-renders it.
    pub render_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    Clean,
    Conflicted,
}

impl SyncState {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncState::Clean => "clean",
            SyncState::Conflicted => "conflicted",
        }
    }
    fn parse(s: &str) -> Self {
        if s == "conflicted" {
            SyncState::Conflicted
        } else {
            SyncState::Clean
        }
    }
}

/// What `fetch` last saw on the server.
#[derive(Clone, Debug, PartialEq)]
pub struct RemotePage {
    pub page_id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub position: Option<i64>,
    pub version: u32,
    pub status: String,
    pub labels: Vec<String>,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub storage_body: Option<String>,
    pub storage_hash: Option<String>,
    pub fetched_at: String,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttachmentRecord {
    pub attachment_id: String,
    pub page_id: String,
    pub filename: String,
    pub media_type: Option<String>,
    pub file_size: Option<u64>,
    /// The server's version of the attachment, as last listed.
    pub version: u32,
    /// Hash of the local copy as confed last downloaded or uploaded it: what
    /// tells a file edited since from one that is merely out of date.
    pub sha256: Option<String>,
    /// Whether that copy is the server's current version.
    pub downloaded: bool,
}

/// An attachment that is gone from the server while its local copy may still
/// be in the sidecar.
#[derive(Clone, Debug, PartialEq)]
pub struct RemovedAttachment {
    pub page_id: String,
    pub filename: String,
    /// Hash of the copy confed last wrote, if it ever wrote one.
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommentRecord {
    pub comment_id: String,
    pub page_id: String,
    pub parent_comment_id: Option<String>,
    pub kind: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub body_storage: Option<String>,
    pub body_markdown: String,
    pub resolved: bool,
    /// JSON-encoded `InlineAnchor` for inline comments.
    pub anchor: Option<String>,
    pub synced_at: Option<String>,
}

/// Somebody confed resolved once and does not need to look up again.
#[derive(Clone, Debug, PartialEq)]
pub struct UserRecord {
    /// The id as it appears in page markup.
    pub id: String,
    /// Which `ri:user` attribute carried it.
    pub id_attr: String,
    pub username: Option<String>,
    pub display_name: String,
    pub profile_url: String,
    pub fetched_at: String,
}

#[derive(Clone, Debug)]
pub struct SyncLogEntry {
    pub id: i64,
    pub ts: String,
    pub op: String,
    pub page_id: Option<String>,
    pub from_version: Option<u32>,
    pub to_version: Option<u32>,
    pub result: String,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct StateDb {
    conn: Connection,
    path: PathBuf,
}

impl StateDb {
    /// Open an existing state DB, or fail with the "not initialized" error.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(STATE_DB_FILENAME);
        if !path.exists() {
            return Err(ConfedError::not_initialized());
        }
        let conn = Connection::open(&path)?;
        let db = Self::configure(conn, path)?;
        db.migrate()?;
        Ok(db)
    }

    /// Create the state DB, or open it if it already exists.
    pub fn create(dir: &Path) -> Result<Self> {
        let path = dir.join(STATE_DB_FILENAME);
        let conn = Connection::open(&path)?;
        let db = Self::configure(conn, path)?;
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self::configure(conn, PathBuf::from(":memory:"))?;
        db.migrate()?;
        Ok(db)
    }

    fn configure(conn: Connection, path: PathBuf) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        Ok(self.conn.transaction()?)
    }

    fn migrate(&self) -> Result<()> {
        let current: u32 = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .unwrap_or(None)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        if current > SCHEMA_VERSION {
            return Err(ConfedError::state_with_hint(
                format!(
                    "this .state.db was written by a newer confed (schema {current}, this build supports {SCHEMA_VERSION})"
                ),
                "upgrade confed, or delete .state.db and re-run `confed init` + `confed fetch`",
            ));
        }
        if current == 0 {
            self.conn.execute_batch(SCHEMA_V1)?;
        }
        if current < 2 {
            self.conn.execute_batch(SCHEMA_V2)?;
        }
        if current != SCHEMA_VERSION {
            self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        }
        self.conn.execute_batch(SCHEMA_ADDITIONS)?;
        Ok(())
    }

    /// Cheap corruption check, used by `confed doctor`.
    pub fn integrity_check(&self) -> Result<String> {
        Ok(self.conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?)
    }

    // ---- meta -------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
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

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn all_meta(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM meta ORDER BY key")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- base pages -------------------------------------------------------

    pub fn upsert_page(&self, page: &PageRecord) -> Result<()> {
        upsert_page_on(&self.conn, page)
    }

    pub fn get_page(&self, page_id: &str) -> Result<Option<PageRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT page_id, title, slug, local_path, parent_id, position, version, status,
                        labels, author, created_at, updated_at, storage_body, storage_hash,
                        markdown_hash, block_map, sync_state, synced_at, render_key
                 FROM pages WHERE page_id = ?1",
                params![page_id],
                page_from_row,
            )
            .optional()?)
    }

    pub fn get_page_by_path(&self, local_path: &str) -> Result<Option<PageRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT page_id, title, slug, local_path, parent_id, position, version, status,
                        labels, author, created_at, updated_at, storage_body, storage_hash,
                        markdown_hash, block_map, sync_state, synced_at, render_key
                 FROM pages WHERE local_path = ?1",
                params![local_path],
                page_from_row,
            )
            .optional()?)
    }

    pub fn all_pages(&self) -> Result<Vec<PageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT page_id, title, slug, local_path, parent_id, position, version, status,
                    labels, author, created_at, updated_at, storage_body, storage_hash,
                    markdown_hash, block_map, sync_state, synced_at, render_key
             FROM pages ORDER BY local_path",
        )?;
        let rows = stmt.query_map([], page_from_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_page(&self, page_id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM pages WHERE page_id = ?1", params![page_id])?;
        self.conn.execute("DELETE FROM attachments WHERE page_id = ?1", params![page_id])?;
        self.conn
            .execute("DELETE FROM removed_attachments WHERE page_id = ?1", params![page_id])?;
        self.conn.execute("DELETE FROM comments WHERE page_id = ?1", params![page_id])?;
        Ok(())
    }

    pub fn set_sync_state(&self, page_id: &str, state: SyncState) -> Result<()> {
        self.conn.execute(
            "UPDATE pages SET sync_state = ?2 WHERE page_id = ?1",
            params![page_id, state.as_str()],
        )?;
        Ok(())
    }

    // ---- remote pages -----------------------------------------------------

    pub fn upsert_remote(&self, page: &RemotePage) -> Result<()> {
        upsert_remote_on(&self.conn, page)
    }

    pub fn get_remote(&self, page_id: &str) -> Result<Option<RemotePage>> {
        Ok(self
            .conn
            .query_row(
                "SELECT page_id, title, parent_id, position, version, status, labels, author,
                        created_at, updated_at, storage_body, storage_hash, fetched_at, deleted
                 FROM remote_pages WHERE page_id = ?1",
                params![page_id],
                remote_from_row,
            )
            .optional()?)
    }

    pub fn all_remote(&self) -> Result<Vec<RemotePage>> {
        let mut stmt = self.conn.prepare(
            "SELECT page_id, title, parent_id, position, version, status, labels, author,
                    created_at, updated_at, storage_body, storage_hash, fetched_at, deleted
             FROM remote_pages ORDER BY page_id",
        )?;
        let rows = stmt.query_map([], remote_from_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn mark_remote_deleted(&self, page_id: &str) -> Result<()> {
        self.conn
            .execute("UPDATE remote_pages SET deleted = 1 WHERE page_id = ?1", params![page_id])?;
        Ok(())
    }

    pub fn delete_remote(&self, page_id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM remote_pages WHERE page_id = ?1", params![page_id])?;
        Ok(())
    }

    // ---- attachments ------------------------------------------------------

    pub fn upsert_attachment(&self, a: &AttachmentRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO attachments
                (attachment_id, page_id, filename, media_type, file_size, version, sha256, downloaded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(attachment_id) DO UPDATE SET
                page_id = excluded.page_id, filename = excluded.filename,
                media_type = excluded.media_type, file_size = excluded.file_size,
                version = excluded.version, sha256 = excluded.sha256,
                downloaded = excluded.downloaded",
            params![
                a.attachment_id,
                a.page_id,
                a.filename,
                a.media_type,
                a.file_size.map(|v| v as i64),
                a.version,
                a.sha256,
                a.downloaded as i32,
            ],
        )?;
        Ok(())
    }

    pub fn page_attachments(&self, page_id: &str) -> Result<Vec<AttachmentRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT attachment_id, page_id, filename, media_type, file_size, version, sha256, downloaded
             FROM attachments WHERE page_id = ?1 ORDER BY filename",
        )?;
        let rows = stmt.query_map(params![page_id], |r| {
            Ok(AttachmentRecord {
                attachment_id: r.get(0)?,
                page_id: r.get(1)?,
                filename: r.get(2)?,
                media_type: r.get(3)?,
                file_size: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                version: r.get(5)?,
                sha256: r.get(6)?,
                downloaded: r.get::<_, i32>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_attachment(&self, attachment_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM attachments WHERE attachment_id = ?1", params![attachment_id])?;
        Ok(())
    }

    pub fn clear_page_attachments(&self, page_id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM attachments WHERE page_id = ?1", params![page_id])?;
        Ok(())
    }

    /// Remember that the server no longer has this attachment, so the next
    /// pull removes the copy in the sidecar and a push does not upload it back.
    pub fn note_removed_attachment(
        &self,
        page_id: &str,
        filename: &str,
        sha256: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO removed_attachments (page_id, filename, sha256) VALUES (?1, ?2, ?3)
             ON CONFLICT(page_id, filename) DO UPDATE SET sha256 = excluded.sha256",
            params![page_id, filename, sha256],
        )?;
        Ok(())
    }

    pub fn removed_attachments(&self, page_id: &str) -> Result<Vec<RemovedAttachment>> {
        let mut stmt = self.conn.prepare(
            "SELECT page_id, filename, sha256 FROM removed_attachments
             WHERE page_id = ?1 ORDER BY filename",
        )?;
        let rows = stmt.query_map(params![page_id], |r| {
            Ok(RemovedAttachment { page_id: r.get(0)?, filename: r.get(1)?, sha256: r.get(2)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The removal was dealt with, or the name is an attachment again.
    pub fn forget_removed_attachment(&self, page_id: &str, filename: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM removed_attachments WHERE page_id = ?1 AND filename = ?2",
            params![page_id, filename],
        )?;
        Ok(())
    }

    // ---- comments ---------------------------------------------------------

    pub fn upsert_comment(&self, c: &CommentRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO comments
                (comment_id, page_id, parent_comment_id, kind, author, created_at,
                 body_storage, body_markdown, resolved, anchor, synced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(comment_id) DO UPDATE SET
                page_id = excluded.page_id, parent_comment_id = excluded.parent_comment_id,
                kind = excluded.kind, author = excluded.author, created_at = excluded.created_at,
                body_storage = excluded.body_storage, body_markdown = excluded.body_markdown,
                resolved = excluded.resolved, anchor = excluded.anchor,
                synced_at = excluded.synced_at",
            params![
                c.comment_id,
                c.page_id,
                c.parent_comment_id,
                c.kind,
                c.author,
                c.created_at,
                c.body_storage.as_ref().map(|b| compress(b)).transpose()?,
                c.body_markdown,
                c.resolved as i32,
                c.anchor,
                c.synced_at,
            ],
        )?;
        Ok(())
    }

    pub fn page_comments(&self, page_id: &str) -> Result<Vec<CommentRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT comment_id, page_id, parent_comment_id, kind, author, created_at,
                    body_storage, body_markdown, resolved, anchor, synced_at
             FROM comments WHERE page_id = ?1 ORDER BY created_at, comment_id",
        )?;
        let rows = stmt.query_map(params![page_id], |r| {
            let body: Option<Vec<u8>> = r.get(6)?;
            Ok(CommentRecord {
                comment_id: r.get(0)?,
                page_id: r.get(1)?,
                parent_comment_id: r.get(2)?,
                kind: r.get(3)?,
                author: r.get(4)?,
                created_at: r.get(5)?,
                body_storage: body.and_then(|b| decompress(&b).ok()),
                body_markdown: r.get(7)?,
                resolved: r.get::<_, i32>(8)? != 0,
                anchor: r.get(9)?,
                synced_at: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn delete_comment(&self, comment_id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM comments WHERE comment_id = ?1", params![comment_id])?;
        Ok(())
    }

    pub fn clear_page_comments(&self, page_id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM comments WHERE page_id = ?1", params![page_id])?;
        Ok(())
    }

    // ---- resolved people --------------------------------------------------

    /// Remember a person so their mentions render without another lookup.
    pub fn upsert_user(&self, user: &UserRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO users (id, id_attr, username, display_name, profile_url, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                id_attr = excluded.id_attr, username = excluded.username,
                display_name = excluded.display_name, profile_url = excluded.profile_url,
                fetched_at = excluded.fetched_at",
            params![
                user.id,
                user.id_attr,
                user.username,
                user.display_name,
                user.profile_url,
                user.fetched_at
            ],
        )?;
        Ok(())
    }

    pub fn all_users(&self) -> Result<Vec<UserRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, id_attr, username, display_name, profile_url, fetched_at FROM users",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(UserRecord {
                id: r.get(0)?,
                id_attr: r.get(1)?,
                username: r.get(2)?,
                display_name: r.get(3)?,
                profile_url: r.get(4)?,
                fetched_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get_user(&self, id: &str) -> Result<Option<UserRecord>> {
        Ok(self.all_users()?.into_iter().find(|u| u.id == id))
    }

    // ---- fetch queue ------------------------------------------------------

    pub fn enqueue_fetch(&self, page_id: &str, needs: &[&str]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO fetch_queue (page_id, needs, done) VALUES (?1, ?2, 0)
             ON CONFLICT(page_id) DO UPDATE SET needs = excluded.needs, done = 0",
            params![page_id, serde_json::to_string(needs)?],
        )?;
        Ok(())
    }

    /// Ask for more of a page without dropping what is already owed for it.
    pub fn enqueue_fetch_also(&self, page_id: &str, needs: &[&str]) -> Result<()> {
        let owed = self.pending_fetch(page_id)?.unwrap_or_default();
        let mut all: Vec<&str> = owed.iter().map(String::as_str).collect();
        all.extend(needs.iter().filter(|n| !owed.iter().any(|o| o == *n)));
        self.enqueue_fetch(page_id, &all)
    }

    pub fn pending_fetches(&self) -> Result<Vec<(String, Vec<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT page_id, needs FROM fetch_queue WHERE done = 0 ORDER BY page_id")?;
        let rows = stmt.query_map([], |r| {
            let needs: String = r.get(1)?;
            Ok((r.get::<_, String>(0)?, needs))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, needs) = row?;
            out.push((id, serde_json::from_str(&needs).unwrap_or_default()));
        }
        Ok(out)
    }

    pub fn mark_fetch_done(&self, page_id: &str) -> Result<()> {
        self.conn
            .execute("UPDATE fetch_queue SET done = 1 WHERE page_id = ?1", params![page_id])?;
        Ok(())
    }

    /// What is still owed for one page, if anything is.
    pub fn pending_fetch(&self, page_id: &str) -> Result<Option<Vec<String>>> {
        let needs: Option<String> = self
            .conn
            .query_row(
                "SELECT needs FROM fetch_queue WHERE page_id = ?1 AND done = 0",
                params![page_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(needs.map(|needs| serde_json::from_str(&needs).unwrap_or_default()))
    }

    /// Part of what a page's entry asked for was read — its `attachments`, its
    /// `comments` — without the page itself: take that off the entry, and
    /// settle it once nothing is left. An entry still waiting for the body
    /// stays whole, or the page would never be fetched.
    pub fn mark_fetched(&self, page_id: &str, read: &[&str]) -> Result<()> {
        let Some(needs) = self.pending_fetch(page_id)? else { return Ok(()) };
        if needs.iter().any(|n| n == "body") {
            return Ok(());
        }
        let left: Vec<&str> =
            needs.iter().map(String::as_str).filter(|n| !read.contains(n)).collect();
        if left.is_empty() {
            self.mark_fetch_done(page_id)
        } else {
            self.enqueue_fetch(page_id, &left)
        }
    }

    /// [`Self::mark_fetched`] for a page's comments.
    pub fn mark_comments_fetched(&self, page_id: &str) -> Result<()> {
        self.mark_fetched(page_id, &["comments"])
    }

    pub fn clear_fetch_queue(&self) -> Result<()> {
        self.conn.execute("DELETE FROM fetch_queue", [])?;
        Ok(())
    }

    pub fn fetch_queue_len(&self) -> Result<(usize, usize)> {
        let total: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM fetch_queue", [], |r| r.get(0))?;
        let done: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM fetch_queue WHERE done = 1", [], |r| r.get(0))?;
        Ok((done as usize, total as usize))
    }

    // ---- audit log --------------------------------------------------------

    pub fn log(
        &self,
        op: &str,
        page_id: Option<&str>,
        from_version: Option<u32>,
        to_version: Option<u32>,
        result: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_log (ts, op, page_id, from_version, to_version, result, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![now(), op, page_id, from_version, to_version, result, detail],
        )?;
        Ok(())
    }

    pub fn recent_log(&self, limit: usize, page_id: Option<&str>) -> Result<Vec<SyncLogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, op, page_id, from_version, to_version, result, detail
             FROM sync_log
             WHERE (?1 IS NULL OR page_id = ?1)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![page_id, limit as i64], |r| {
            Ok(SyncLogEntry {
                id: r.get(0)?,
                ts: r.get(1)?,
                op: r.get(2)?,
                page_id: r.get(3)?,
                from_version: r.get(4)?,
                to_version: r.get(5)?,
                result: r.get(6)?,
                detail: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

/// Upsert usable inside a transaction as well as on the connection.
pub fn upsert_page_on(conn: &Connection, page: &PageRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO pages
            (page_id, title, slug, local_path, parent_id, position, version, status, labels,
             author, created_at, updated_at, storage_body, storage_hash, markdown_hash,
             block_map, sync_state, synced_at, render_key)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
         ON CONFLICT(page_id) DO UPDATE SET
            title = excluded.title, slug = excluded.slug, local_path = excluded.local_path,
            parent_id = excluded.parent_id, position = excluded.position,
            version = excluded.version, status = excluded.status, labels = excluded.labels,
            author = excluded.author, created_at = excluded.created_at,
            updated_at = excluded.updated_at, storage_body = excluded.storage_body,
            storage_hash = excluded.storage_hash, markdown_hash = excluded.markdown_hash,
            block_map = excluded.block_map, sync_state = excluded.sync_state,
            synced_at = excluded.synced_at, render_key = excluded.render_key",
        params![
            page.page_id,
            page.title,
            page.slug,
            page.local_path,
            page.parent_id,
            page.position,
            page.version,
            page.status,
            serde_json::to_string(&page.labels)?,
            page.author,
            page.created_at,
            page.updated_at,
            compress(&page.storage_body)?,
            page.storage_hash,
            page.markdown_hash,
            page.block_map,
            page.sync_state.as_str(),
            page.synced_at,
            page.render_key,
        ],
    )?;
    Ok(())
}

pub fn upsert_remote_on(conn: &Connection, page: &RemotePage) -> Result<()> {
    conn.execute(
        "INSERT INTO remote_pages
            (page_id, title, parent_id, position, version, status, labels, author,
             created_at, updated_at, storage_body, storage_hash, fetched_at, deleted)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
         ON CONFLICT(page_id) DO UPDATE SET
            title = excluded.title, parent_id = excluded.parent_id,
            position = excluded.position, version = excluded.version,
            status = excluded.status, labels = excluded.labels, author = excluded.author,
            created_at = excluded.created_at, updated_at = excluded.updated_at,
            storage_body = COALESCE(excluded.storage_body, remote_pages.storage_body),
            storage_hash = COALESCE(excluded.storage_hash, remote_pages.storage_hash),
            fetched_at = excluded.fetched_at, deleted = excluded.deleted",
        params![
            page.page_id,
            page.title,
            page.parent_id,
            page.position,
            page.version,
            page.status,
            serde_json::to_string(&page.labels)?,
            page.author,
            page.created_at,
            page.updated_at,
            page.storage_body.as_ref().map(|b| compress(b)).transpose()?,
            page.storage_hash,
            page.fetched_at,
            page.deleted as i32,
        ],
    )?;
    Ok(())
}

fn page_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PageRecord> {
    let labels: String = r.get(8)?;
    let body: Vec<u8> = r.get(12)?;
    Ok(PageRecord {
        page_id: r.get(0)?,
        title: r.get(1)?,
        slug: r.get(2)?,
        local_path: r.get(3)?,
        parent_id: r.get(4)?,
        position: r.get(5)?,
        version: r.get(6)?,
        status: r.get(7)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        author: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        storage_body: decompress(&body).unwrap_or_default(),
        storage_hash: r.get(13)?,
        markdown_hash: r.get(14)?,
        block_map: r.get(15)?,
        sync_state: SyncState::parse(&r.get::<_, String>(16)?),
        synced_at: r.get(17)?,
        render_key: r.get(18)?,
    })
}

fn remote_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RemotePage> {
    let labels: String = r.get(6)?;
    let body: Option<Vec<u8>> = r.get(10)?;
    Ok(RemotePage {
        page_id: r.get(0)?,
        title: r.get(1)?,
        parent_id: r.get(2)?,
        position: r.get(3)?,
        version: r.get(4)?,
        status: r.get(5)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        author: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
        storage_body: body.and_then(|b| decompress(&b).ok()),
        storage_hash: r.get(11)?,
        fetched_at: r.get(12)?,
        deleted: r.get::<_, i32>(13)? != 0,
    })
}

/// Bodies are stored compressed: a space of a few thousand pages is mostly XHTML.
fn compress(text: &str) -> Result<Vec<u8>> {
    zstd::encode_all(text.as_bytes(), 3).map_err(|e| ConfedError::io("compressing page body", e))
}

fn decompress(bytes: &[u8]) -> Result<String> {
    let raw = zstd::decode_all(bytes).map_err(|e| ConfedError::io("decompressing page body", e))?;
    String::from_utf8(raw).map_err(|e| ConfedError::Other(format!("stored body is not UTF-8: {e}")))
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn hash_str(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_page(id: &str) -> PageRecord {
        PageRecord {
            page_id: id.into(),
            title: "Onboarding".into(),
            slug: "Onboarding".into(),
            local_path: format!("Handbook/{id}.md"),
            parent_id: Some("100".into()),
            position: Some(2),
            version: 7,
            status: "current".into(),
            labels: vec!["hr".into(), "onboarding".into()],
            author: Some("jdoe".into()),
            created_at: Some("2025-11-02T09:14:00Z".into()),
            updated_at: Some("2026-08-01T16:40:00Z".into()),
            storage_body: "<p>hello</p>".into(),
            storage_hash: hash_str("<p>hello</p>"),
            markdown_hash: hash_str("hello"),
            block_map: Some("{\"blocks\":[]}".into()),
            sync_state: SyncState::Clean,
            synced_at: now(),
            render_key: String::new(),
        }
    }

    #[test]
    fn schema_is_created_and_reported() {
        let db = StateDb::open_in_memory().unwrap();
        assert_eq!(
            db.get_meta("schema_version").unwrap().as_deref(),
            Some(SCHEMA_VERSION.to_string().as_str())
        );
        assert_eq!(db.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn pages_round_trip_including_compressed_bodies() {
        let db = StateDb::open_in_memory().unwrap();
        let page = sample_page("163842");
        db.upsert_page(&page).unwrap();
        assert_eq!(db.get_page("163842").unwrap().unwrap(), page);
        assert_eq!(db.get_page_by_path("Handbook/163842.md").unwrap().unwrap(), page);

        // Upsert is idempotent and updates in place.
        let mut updated = page.clone();
        updated.version = 8;
        updated.storage_body = "<p>changed</p>".into();
        db.upsert_page(&updated).unwrap();
        assert_eq!(db.all_pages().unwrap().len(), 1);
        assert_eq!(db.get_page("163842").unwrap().unwrap().storage_body, "<p>changed</p>");
    }

    #[test]
    fn deleting_a_page_takes_its_attachments_and_comments() {
        let db = StateDb::open_in_memory().unwrap();
        db.upsert_page(&sample_page("1")).unwrap();
        db.upsert_attachment(&AttachmentRecord {
            attachment_id: "att1".into(),
            page_id: "1".into(),
            filename: "d.png".into(),
            media_type: None,
            file_size: Some(10),
            version: 1,
            sha256: Some("abc".into()),
            downloaded: true,
        })
        .unwrap();
        db.upsert_comment(&CommentRecord {
            comment_id: "c1".into(),
            page_id: "1".into(),
            parent_comment_id: None,
            kind: "footer".into(),
            author: Some("Alice".into()),
            created_at: Some(now()),
            body_storage: Some("<p>note</p>".into()),
            body_markdown: "note".into(),
            resolved: false,
            anchor: None,
            synced_at: Some(now()),
        })
        .unwrap();

        db.note_removed_attachment("1", "old.png", Some("def")).unwrap();

        db.delete_page("1").unwrap();
        assert!(db.page_attachments("1").unwrap().is_empty());
        assert!(db.removed_attachments("1").unwrap().is_empty());
        assert!(db.page_comments("1").unwrap().is_empty());
    }

    #[test]
    fn a_removed_attachment_is_remembered_until_it_is_dealt_with() {
        let db = StateDb::open_in_memory().unwrap();
        db.note_removed_attachment("1", "old.png", Some("abc")).unwrap();
        db.note_removed_attachment("1", "never-downloaded.png", None).unwrap();
        db.note_removed_attachment("2", "other.png", None).unwrap();
        // Noted twice, it is still one removal, with the later hash.
        db.note_removed_attachment("1", "old.png", Some("def")).unwrap();

        let removed = db.removed_attachments("1").unwrap();
        let names: Vec<&str> = removed.iter().map(|r| r.filename.as_str()).collect();
        assert_eq!(names, ["never-downloaded.png", "old.png"]);
        assert_eq!(removed[1].sha256.as_deref(), Some("def"));

        db.forget_removed_attachment("1", "old.png").unwrap();
        assert_eq!(db.removed_attachments("1").unwrap().len(), 1);
        assert_eq!(db.removed_attachments("2").unwrap().len(), 1, "another page's is untouched");
    }

    #[test]
    fn a_state_from_before_removals_were_tracked_gains_the_table_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = StateDb::create(dir.path()).unwrap();
            db.conn().execute("DROP TABLE removed_attachments", []).unwrap();
        }
        let db = StateDb::open(dir.path()).unwrap();
        assert!(db.removed_attachments("1").unwrap().is_empty());
        assert_eq!(
            db.get_meta("schema_version").unwrap().as_deref(),
            Some(SCHEMA_VERSION.to_string().as_str()),
            "an older confed can still open it"
        );
    }

    #[test]
    fn comment_bodies_survive_compression() {
        let db = StateDb::open_in_memory().unwrap();
        let comment = CommentRecord {
            comment_id: "c1".into(),
            page_id: "1".into(),
            parent_comment_id: Some("c0".into()),
            kind: "inline".into(),
            author: Some("Bob".into()),
            created_at: Some(now()),
            body_storage: Some("<p>Link the checklist?</p>".into()),
            body_markdown: "Link the checklist?".into(),
            resolved: true,
            anchor: Some(r#"{"text":"first week"}"#.into()),
            synced_at: Some(now()),
        };
        db.upsert_comment(&comment).unwrap();
        assert_eq!(db.page_comments("1").unwrap()[0], comment);
    }

    #[test]
    fn remote_upsert_keeps_a_body_when_only_metadata_is_refreshed() {
        let db = StateDb::open_in_memory().unwrap();
        let mut remote = RemotePage {
            page_id: "1".into(),
            title: "T".into(),
            parent_id: None,
            position: None,
            version: 3,
            status: "current".into(),
            labels: vec![],
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: Some("<p>body</p>".into()),
            storage_hash: Some(hash_str("<p>body</p>")),
            fetched_at: now(),
            deleted: false,
        };
        db.upsert_remote(&remote).unwrap();

        // A listing pass has no body; it must not wipe the one we already have.
        remote.storage_body = None;
        remote.storage_hash = None;
        remote.version = 4;
        db.upsert_remote(&remote).unwrap();

        let stored = db.get_remote("1").unwrap().unwrap();
        assert_eq!(stored.version, 4);
        assert_eq!(stored.storage_body.as_deref(), Some("<p>body</p>"));
    }

    #[test]
    fn fetch_queue_tracks_progress_for_resume() {
        let db = StateDb::open_in_memory().unwrap();
        db.enqueue_fetch("1", &["body", "comments"]).unwrap();
        db.enqueue_fetch("2", &["body"]).unwrap();
        assert_eq!(db.pending_fetches().unwrap().len(), 2);

        db.mark_fetch_done("1").unwrap();
        let pending = db.pending_fetches().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "2");
        assert_eq!(db.fetch_queue_len().unwrap(), (1, 2));

        // Re-enqueueing a done page resets it (its version changed again).
        db.enqueue_fetch("1", &["body"]).unwrap();
        assert_eq!(db.pending_fetches().unwrap().len(), 2);

        // Reading a page's comments settles only an entry that wanted no more.
        db.enqueue_fetch("3", &["comments"]).unwrap();
        db.mark_comments_fetched("1").unwrap();
        db.mark_comments_fetched("3").unwrap();
        let pending: Vec<String> =
            db.pending_fetches().unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(pending, ["1", "2"], "the body of page 1 is still owed");

        // Half of what was asked for leaves the other half owed.
        db.enqueue_fetch("4", &["attachments", "comments"]).unwrap();
        db.mark_fetched("4", &["attachments"]).unwrap();
        assert_eq!(db.pending_fetch("4").unwrap(), Some(vec!["comments".to_string()]));
        db.mark_fetched("4", &["comments"]).unwrap();
        assert_eq!(db.pending_fetch("4").unwrap(), None);
        db.mark_fetched("1", &["attachments", "comments"]).unwrap();
        assert_eq!(db.pending_fetch("1").unwrap(), Some(vec!["body".to_string()]));
        db.mark_fetched("nobody", &["comments"]).unwrap();

        // Asking for more of a page keeps what it was already owed.
        db.enqueue_fetch("5", &["comments"]).unwrap();
        db.enqueue_fetch_also("5", &["attachments", "comments"]).unwrap();
        db.enqueue_fetch_also("6", &["attachments"]).unwrap();
        db.enqueue_fetch_also("1", &["attachments"]).unwrap();
        let needs = |id: &str| db.pending_fetch(id).unwrap().unwrap();
        assert_eq!(needs("5"), ["comments", "attachments"]);
        assert_eq!(needs("6"), ["attachments"]);
        assert_eq!(needs("1"), ["body", "attachments"]);
    }

    #[test]
    fn sync_log_returns_newest_first() {
        let db = StateDb::open_in_memory().unwrap();
        db.log("pull", Some("1"), Some(1), Some(2), "ok", None).unwrap();
        db.log("push", Some("1"), Some(2), Some(3), "ok", Some("body")).unwrap();
        let entries = db.recent_log(10, Some("1")).unwrap();
        assert_eq!(entries[0].op, "push");
        assert_eq!(entries[1].op, "pull");
        assert!(db.recent_log(10, Some("nope")).unwrap().is_empty());
    }

    #[test]
    fn conflicted_pages_are_remembered_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = StateDb::create(dir.path()).unwrap();
            db.upsert_page(&sample_page("1")).unwrap();
            db.set_sync_state("1", SyncState::Conflicted).unwrap();
        }
        let db = StateDb::open(dir.path()).unwrap();
        assert_eq!(db.get_page("1").unwrap().unwrap().sync_state, SyncState::Conflicted);
    }

    #[test]
    fn opening_a_missing_workspace_says_how_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();
        let err = StateDb::open(dir.path()).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::State);
        assert!(err.hint().unwrap().contains("confed init"));
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_corrupted() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = StateDb::create(dir.path()).unwrap();
            db.set_meta("schema_version", "99").unwrap();
        }
        let err = StateDb::open(dir.path()).unwrap_err();
        assert!(err.to_string().contains("newer confed"));
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A database written by an older confed is migrated in place, keeping the
    /// pages it already knows about.
    #[test]
    fn a_v1_database_gains_the_v2_tables_without_losing_pages() {
        let dir = tempfile::tempdir().unwrap();
        {
            // Build the schema as version 1 left it.
            let conn = Connection::open(dir.path().join(STATE_DB_FILENAME)).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute("INSERT INTO meta (key, value) VALUES ('schema_version', '1')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO pages (page_id, title, slug, local_path, version, status,
                                    storage_body, storage_hash, markdown_hash, synced_at)
                 VALUES ('1', 'Kept', 'Kept', 'Kept.md', 3, 'current', X'00', 'h', 'h', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }

        let db = StateDb::open(dir.path()).unwrap();
        assert_eq!(
            db.get_meta("schema_version").unwrap().as_deref(),
            Some(SCHEMA_VERSION.to_string().as_str())
        );

        let page = db.get_page("1").unwrap().expect("the page survived the migration");
        assert_eq!(page.title, "Kept");
        assert_eq!(page.version, 3);
        assert_eq!(page.render_key, "", "an unknown fingerprint means 'render it again'");

        // The people cache exists and works.
        db.upsert_user(&UserRecord {
            id: "key-1".into(),
            id_attr: "userkey".into(),
            username: Some("alice.ng".into()),
            display_name: "Alice Ng".into(),
            profile_url: "https://wiki/display/~alice.ng".into(),
            fetched_at: now(),
        })
        .unwrap();
        assert_eq!(db.get_user("key-1").unwrap().unwrap().display_name, "Alice Ng");
    }

    #[test]
    fn migrating_twice_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        StateDb::create(dir.path()).unwrap();
        let db = StateDb::open(dir.path()).unwrap();
        assert_eq!(db.integrity_check().unwrap(), "ok");
    }
}
