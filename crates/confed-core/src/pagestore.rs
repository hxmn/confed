//! `.pages.db` — content confed has already downloaded, keyed by version.
//!
//! A page body at a given version never changes, so it only ever needs to be
//! downloaded once. Keeping it here rather than in `.state.db` means the cache
//! survives everything that rebuilds sync state — a fresh clone of a repository
//! that tracks the Markdown but not the state, a `confed init` over an existing
//! tree, a version that goes back to one seen before — and `fetch` degenerates
//! to asking which versions exist and which attachments and comments changed.
//!
//! Nothing here is authoritative. Deleting the file costs bandwidth, not
//! correctness.

use crate::error::{ConfedError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PAGES_DB_FILENAME: &str = ".pages.db";

/// Versions kept per page. Enough that flipping between a couple of recent
/// versions stays free, without growing without bound.
const KEEP_VERSIONS_PER_PAGE: usize = 5;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS page_versions (
  page_id      TEXT    NOT NULL,
  version      INTEGER NOT NULL,
  storage_body BLOB    NOT NULL,
  storage_hash TEXT    NOT NULL,
  fetched_at   TEXT    NOT NULL,
  PRIMARY KEY (page_id, version)
);

-- Attachments and comments are not versioned by the page's version, so they are
-- cached as "what confed last saw", not as immutable content. Restoring them
-- after a state rebuild returns confed to what it knew; it does not claim to be
-- more current than that.
CREATE TABLE IF NOT EXISTS page_extras (
  page_id     TEXT PRIMARY KEY,
  version     INTEGER NOT NULL,
  attachments TEXT NOT NULL,
  comments    TEXT NOT NULL,
  fetched_at  TEXT NOT NULL
);

-- What the cache knows about itself. `comments_checked_at` and
-- `attachments_checked_at` are when the comments and the attachments in
-- `page_extras` were last known to be current, so a state rebuilt from the
-- cache can ask the server for what changed since, instead of for everything.
CREATE TABLE IF NOT EXISTS cache_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

const COMMENTS_CHECKED_AT: &str = "comments_checked_at";
const ATTACHMENTS_CHECKED_AT: &str = "attachments_checked_at";

/// Attachment and comment snapshots for one page, as JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageExtras {
    pub attachments: String,
    pub comments: String,
}

#[derive(Debug)]
pub struct PageStore {
    conn: Connection,
    path: PathBuf,
}

impl PageStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(PAGES_DB_FILENAME);
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, path })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, path: PathBuf::from(":memory:") })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The body of a page at a version, if it has ever been downloaded.
    pub fn body(&self, page_id: &str, version: u32) -> Result<Option<String>> {
        let stored: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT storage_body FROM page_versions WHERE page_id = ?1 AND version = ?2",
                params![page_id, version],
                |r| r.get(0),
            )
            .optional()?;
        stored.map(|bytes| decompress(&bytes)).transpose()
    }

    pub fn put_body(&self, page_id: &str, version: u32, storage: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO page_versions (page_id, version, storage_body, storage_hash, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(page_id, version) DO UPDATE SET
                storage_body = excluded.storage_body,
                storage_hash = excluded.storage_hash,
                fetched_at = excluded.fetched_at",
            params![
                page_id,
                version,
                compress(storage)?,
                crate::state::hash_str(storage),
                crate::state::now()
            ],
        )?;
        self.prune(page_id)?;
        Ok(())
    }

    /// Drop all but the newest few versions of one page.
    fn prune(&self, page_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM page_versions
             WHERE page_id = ?1 AND version NOT IN (
                 SELECT version FROM page_versions WHERE page_id = ?1
                 ORDER BY version DESC LIMIT ?2
             )",
            params![page_id, KEEP_VERSIONS_PER_PAGE as i64],
        )?;
        Ok(())
    }

    /// Attachment and comment snapshots, if they were captured at this version.
    pub fn extras(&self, page_id: &str, version: u32) -> Result<Option<PageExtras>> {
        Ok(self
            .conn
            .query_row(
                "SELECT attachments, comments FROM page_extras
                 WHERE page_id = ?1 AND version = ?2",
                params![page_id, version],
                |r| Ok(PageExtras { attachments: r.get(0)?, comments: r.get(1)? }),
            )
            .optional()?)
    }

    pub fn put_extras(&self, page_id: &str, version: u32, extras: &PageExtras) -> Result<()> {
        self.conn.execute(
            "INSERT INTO page_extras (page_id, version, attachments, comments, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(page_id) DO UPDATE SET
                version = excluded.version, attachments = excluded.attachments,
                comments = excluded.comments, fetched_at = excluded.fetched_at",
            params![page_id, version, extras.attachments, extras.comments, crate::state::now()],
        )?;
        Ok(())
    }

    /// Replace the comment snapshot taken at this version, leaving the
    /// attachments beside it alone. Without a snapshot at this version there is
    /// nothing to bring up to date, and nothing is written: half a snapshot
    /// would restore as "this page has no attachments".
    pub fn put_comments(&self, page_id: &str, version: u32, comments: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE page_extras SET comments = ?3, fetched_at = ?4
             WHERE page_id = ?1 AND version = ?2",
            params![page_id, version, comments, crate::state::now()],
        )?;
        Ok(())
    }

    /// Record the attachments seen at this version when the comments beside
    /// them could not be read: the comments already cached are kept, as the
    /// ones in the state are, until [`Self::put_comments`] replaces them.
    pub fn put_attachments(&self, page_id: &str, version: u32, attachments: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO page_extras (page_id, version, attachments, comments, fetched_at)
             VALUES (?1, ?2, ?3, '[]', ?4)
             ON CONFLICT(page_id) DO UPDATE SET
                version = excluded.version, attachments = excluded.attachments,
                fetched_at = excluded.fetched_at",
            params![page_id, version, attachments, crate::state::now()],
        )?;
        Ok(())
    }

    /// Replace the attachment snapshot taken at this version, leaving the
    /// comments beside it alone — [`Self::put_comments`] the other way round,
    /// and as careful not to write half a snapshot.
    pub fn refresh_attachments(
        &self,
        page_id: &str,
        version: u32,
        attachments: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE page_extras SET attachments = ?3, fetched_at = ?4
             WHERE page_id = ?1 AND version = ?2",
            params![page_id, version, attachments, crate::state::now()],
        )?;
        Ok(())
    }

    /// When the cached comments were last known to be current, if ever.
    pub fn comments_checked_at(&self) -> Result<Option<String>> {
        self.meta(COMMENTS_CHECKED_AT)
    }

    pub fn set_comments_checked_at(&self, when: &str) -> Result<()> {
        self.set_meta(COMMENTS_CHECKED_AT, when)
    }

    /// When the cached attachment lists were last known to be current, if ever.
    pub fn attachments_checked_at(&self) -> Result<Option<String>> {
        self.meta(ATTACHMENTS_CHECKED_AT)
    }

    pub fn set_attachments_checked_at(&self, when: &str) -> Result<()> {
        self.set_meta(ATTACHMENTS_CHECKED_AT, when)
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM cache_meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cache_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// How many page versions are cached, for `doctor` and tests.
    pub fn len(&self) -> Result<usize> {
        let count: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM page_versions", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Forget everything. The next fetch downloads it again.
    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM page_versions", [])?;
        self.conn.execute("DELETE FROM page_extras", [])?;
        self.conn.execute("DELETE FROM cache_meta", [])?;
        Ok(())
    }
}

fn compress(text: &str) -> Result<Vec<u8>> {
    zstd::encode_all(text.as_bytes(), 3).map_err(|e| ConfedError::io("compressing a page body", e))
}

fn decompress(bytes: &[u8]) -> Result<String> {
    let raw = zstd::decode_all(bytes).map_err(|e| ConfedError::io("reading a cached body", e))?;
    String::from_utf8(raw)
        .map_err(|e| ConfedError::Other(format!("a cached body is not UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_is_returned_for_the_version_it_was_stored_at() {
        let store = PageStore::open_in_memory().unwrap();
        store.put_body("1", 7, "<p>seven</p>").unwrap();

        assert_eq!(store.body("1", 7).unwrap().as_deref(), Some("<p>seven</p>"));
        assert_eq!(store.body("1", 8).unwrap(), None, "a version never seen is a miss");
        assert_eq!(store.body("2", 7).unwrap(), None, "another page is a miss");
    }

    #[test]
    fn several_versions_of_one_page_coexist() {
        let store = PageStore::open_in_memory().unwrap();
        store.put_body("1", 7, "<p>seven</p>").unwrap();
        store.put_body("1", 8, "<p>eight</p>").unwrap();

        // A page reverted on the server costs nothing to pull again.
        assert_eq!(store.body("1", 7).unwrap().as_deref(), Some("<p>seven</p>"));
        assert_eq!(store.body("1", 8).unwrap().as_deref(), Some("<p>eight</p>"));
    }

    #[test]
    fn old_versions_are_pruned_so_the_cache_does_not_grow_forever() {
        let store = PageStore::open_in_memory().unwrap();
        for version in 1..=12 {
            store.put_body("1", version, &format!("<p>v{version}</p>")).unwrap();
        }

        assert_eq!(store.len().unwrap(), KEEP_VERSIONS_PER_PAGE);
        assert!(store.body("1", 12).unwrap().is_some(), "the newest is kept");
        assert!(store.body("1", 1).unwrap().is_none(), "the oldest is dropped");
    }

    #[test]
    fn extras_are_only_returned_for_the_version_they_were_captured_at() {
        let store = PageStore::open_in_memory().unwrap();
        let extras = PageExtras { attachments: "[]".into(), comments: "[1]".into() };
        store.put_extras("1", 7, &extras).unwrap();

        assert!(store.extras("1", 7).unwrap().is_some());
        assert!(
            store.extras("1", 8).unwrap().is_none(),
            "a newer page version means they must be looked up again"
        );
    }

    #[test]
    fn comments_are_refreshed_only_beside_the_attachments_of_the_same_version() {
        let store = PageStore::open_in_memory().unwrap();
        let extras = PageExtras { attachments: "[\"a\"]".into(), comments: "[1]".into() };
        store.put_extras("1", 7, &extras).unwrap();

        store.put_comments("1", 7, "[1,2]").unwrap();
        let stored = store.extras("1", 7).unwrap().unwrap();
        assert_eq!(stored.comments, "[1,2]");
        assert_eq!(stored.attachments, "[\"a\"]", "the attachments are left alone");

        // Attachments seen at a new version, the comments unread: those stay.
        store.put_attachments("1", 8, "[\"a\",\"b\"]").unwrap();
        let stored = store.extras("1", 8).unwrap().unwrap();
        assert_eq!(
            (stored.attachments.as_str(), stored.comments.as_str()),
            ("[\"a\",\"b\"]", "[1,2]")
        );
        store.put_extras("1", 7, &extras).unwrap();
        store.put_comments("1", 7, "[1,2]").unwrap();

        // No snapshot at that version, or none at all: nothing to update.
        store.put_comments("1", 8, "[9]").unwrap();
        store.put_comments("2", 1, "[9]").unwrap();
        assert_eq!(store.extras("1", 7).unwrap().unwrap().comments, "[1,2]");
        assert!(store.extras("2", 1).unwrap().is_none());

        // And the same the other way round, for attachments listed again.
        store.refresh_attachments("1", 7, "[\"a\",\"c\"]").unwrap();
        store.refresh_attachments("1", 8, "[]").unwrap();
        store.refresh_attachments("2", 1, "[\"z\"]").unwrap();
        let stored = store.extras("1", 7).unwrap().unwrap();
        assert_eq!(
            (stored.attachments.as_str(), stored.comments.as_str()),
            ("[\"a\",\"c\"]", "[1,2]")
        );
        assert!(store.extras("2", 1).unwrap().is_none());
    }

    #[test]
    fn the_cache_remembers_when_its_comments_were_current() {
        let store = PageStore::open_in_memory().unwrap();
        assert_eq!(store.comments_checked_at().unwrap(), None);
        store.set_comments_checked_at("2026-01-01T00:00:00Z").unwrap();
        store.set_comments_checked_at("2026-02-02T00:00:00Z").unwrap();
        assert_eq!(store.comments_checked_at().unwrap().as_deref(), Some("2026-02-02T00:00:00Z"));

        // The attachments have a mark of their own.
        assert_eq!(store.attachments_checked_at().unwrap(), None);
        store.set_attachments_checked_at("2026-03-03T00:00:00Z").unwrap();
        assert_eq!(
            store.attachments_checked_at().unwrap().as_deref(),
            Some("2026-03-03T00:00:00Z")
        );
        assert_eq!(store.comments_checked_at().unwrap().as_deref(), Some("2026-02-02T00:00:00Z"));

        store.clear().unwrap();
        assert_eq!(store.comments_checked_at().unwrap(), None, "an empty cache knows nothing");
        assert_eq!(store.attachments_checked_at().unwrap(), None);
    }

    #[test]
    fn the_cache_survives_reopening_and_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = PageStore::open(dir.path()).unwrap();
            store.put_body("1", 7, "<p>seven</p>").unwrap();
        }

        let store = PageStore::open(dir.path()).unwrap();
        assert_eq!(store.body("1", 7).unwrap().as_deref(), Some("<p>seven</p>"));

        store.clear().unwrap();
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn bodies_round_trip_through_compression() {
        let store = PageStore::open_in_memory().unwrap();
        let body = "<p>Ünicode — and a “quote”</p>".repeat(50);
        store.put_body("1", 1, &body).unwrap();
        assert_eq!(store.body("1", 1).unwrap().unwrap(), body);
    }
}
