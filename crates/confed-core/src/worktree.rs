//! Scanning the working tree and deriving each page's sync status.
//!
//! Status is a pure function of three things confed already has: the hash of the
//! working file, the base record in `.state.db`, and the last fetched remote
//! version. Nothing here touches the network, so `status` and `diff` work
//! offline.

use crate::error::Result;
use crate::frontmatter::{self, MarkdownFile, Tampering};
use crate::state::{PageRecord, RemotePage, SyncState};
use crate::workspace::Workspace;
use std::collections::{BTreeMap, HashMap};
use walkdir::WalkDir;

/// Generated files that live in a workspace but are not pages.
pub const NON_PAGE_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md"];

/// Where a page stands relative to base and remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageState {
    /// Local matches base, base matches remote.
    Unchanged,
    /// Edited locally; the remote has not moved.
    Modified,
    /// The remote moved ahead; no local edits. `pull` fast-forwards.
    Behind,
    /// Both sides moved. Needs a merge.
    Diverged,
    /// A file with no `page_id`: `push` will create it.
    LocalNew,
    /// A page `fetch` found that has no local file yet.
    RemoteNew,
    /// The file is gone but confed has a base record for it.
    LocalDeleted,
    /// The page was deleted or trashed on the server.
    RemoteDeleted,
    /// A merge left conflict markers; `push` refuses until resolved.
    Conflicted,
    /// The file names a page confed has no record of — usually a rebuilt
    /// `.state.db`. `fetch` repairs it.
    Untracked,
}

impl PageState {
    pub fn as_str(self) -> &'static str {
        match self {
            PageState::Unchanged => "unchanged",
            PageState::Modified => "modified",
            PageState::Behind => "behind",
            PageState::Diverged => "diverged",
            PageState::LocalNew => "local_new",
            PageState::RemoteNew => "remote_new",
            PageState::LocalDeleted => "local_deleted",
            PageState::RemoteDeleted => "remote_deleted",
            PageState::Conflicted => "conflicted",
            PageState::Untracked => "untracked",
        }
    }

    /// Single-letter code for `status --short`, in the spirit of `git status`.
    pub fn short_code(self) -> char {
        match self {
            PageState::Unchanged => ' ',
            PageState::Modified => 'M',
            PageState::Behind => 'B',
            PageState::Diverged => 'V',
            PageState::LocalNew => 'A',
            PageState::RemoteNew => 'R',
            PageState::LocalDeleted => 'D',
            PageState::RemoteDeleted => 'X',
            PageState::Conflicted => 'C',
            PageState::Untracked => '?',
        }
    }

    /// True when `push` has something to send.
    pub fn has_local_work(self) -> bool {
        matches!(
            self,
            PageState::Modified
                | PageState::LocalNew
                | PageState::LocalDeleted
                | PageState::Diverged
        )
    }

    /// True when `pull` has something to write.
    pub fn has_remote_work(self) -> bool {
        matches!(
            self,
            PageState::Behind
                | PageState::RemoteNew
                | PageState::RemoteDeleted
                | PageState::Diverged
        )
    }
}

/// Which writable fields differ from the base record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FieldChanges {
    pub title: bool,
    pub labels: bool,
    pub parent: bool,
}

impl FieldChanges {
    pub fn any(self) -> bool {
        self.title || self.labels || self.parent
    }
}

#[derive(Clone, Debug)]
pub struct PageStatus {
    pub page_id: Option<String>,
    /// Workspace-relative path of the Markdown file.
    pub path: String,
    pub title: String,
    pub state: PageState,
    pub base_version: Option<u32>,
    pub remote_version: Option<u32>,
    pub local_dirty: bool,
    pub remote_ahead: bool,
    pub field_changes: FieldChanges,
    /// Set when the file moved since the last sync (base recorded another path).
    pub moved_from: Option<String>,
    /// Hand edits to the tool-managed `confed:` block.
    pub tampering: Vec<Tampering>,
    /// Inline comments drafted in the body as `new` marks, waiting for a push.
    pub comment_drafts: usize,
}

impl PageStatus {
    pub fn is_clean(&self) -> bool {
        self.state == PageState::Unchanged && self.tampering.is_empty()
    }
}

/// Everything a scan found.
#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub pages: Vec<PageStatus>,
    /// `.md` files that are not confed pages (no frontmatter).
    pub untracked_files: Vec<String>,
    /// Files that could not be parsed, with the reason.
    pub unreadable: Vec<(String, String)>,
}

impl Scan {
    pub fn is_clean(&self) -> bool {
        self.pages.iter().all(PageStatus::is_clean) && self.unreadable.is_empty()
    }

    pub fn by_state(&self, state: PageState) -> impl Iterator<Item = &PageStatus> {
        self.pages.iter().filter(move |p| p.state == state)
    }

    pub fn conflicted(&self) -> impl Iterator<Item = &PageStatus> {
        self.by_state(PageState::Conflicted)
    }

    pub fn find(&self, page_id: &str) -> Option<&PageStatus> {
        self.pages.iter().find(|p| p.page_id.as_deref() == Some(page_id))
    }

    pub fn find_path(&self, path: &str) -> Option<&PageStatus> {
        self.pages.iter().find(|p| p.path == path)
    }
}

/// One parsed working file.
#[derive(Clone, Debug)]
pub struct LocalFile {
    pub path: String,
    pub file: MarkdownFile,
    pub content_hash: String,
}

/// Read every page file in the workspace.
pub fn read_working_files(ws: &Workspace) -> Result<(Vec<LocalFile>, Scan)> {
    let mut files = Vec::new();
    let mut scan = Scan::default();

    for entry in WalkDir::new(ws.root())
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden directories: sidecars, .git, and confed's own state.
            let name = e.file_name().to_string_lossy();
            e.depth() == 0 || !name.starts_with('.')
        })
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(relative) = ws.relative(path) else { continue };
        if NON_PAGE_FILES.contains(&relative.as_str()) {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                scan.unreadable.push((relative, e.to_string()));
                continue;
            }
        };
        if frontmatter::split(&content).is_none() {
            scan.untracked_files.push(relative);
            continue;
        }
        match frontmatter::parse(&content, &relative) {
            Ok(file) => {
                let content_hash = file.content_hash();
                files.push(LocalFile { path: relative, file, content_hash });
            }
            Err(e) => scan.unreadable.push((relative, e.to_string())),
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    scan.untracked_files.sort();
    scan.unreadable.sort();
    Ok((files, scan))
}

/// Derive the status of every page from working files, base records, and the
/// last fetched remote state.
pub fn compute_status(
    files: &[LocalFile],
    base: &[PageRecord],
    remote: &[RemotePage],
) -> Vec<PageStatus> {
    let base_by_id: HashMap<&str, &PageRecord> =
        base.iter().map(|p| (p.page_id.as_str(), p)).collect();
    let remote_by_id: HashMap<&str, &RemotePage> =
        remote.iter().map(|p| (p.page_id.as_str(), p)).collect();

    // BTreeMap keeps the output ordered by path without a later sort.
    let mut out: BTreeMap<String, PageStatus> = BTreeMap::new();
    let mut seen_ids: Vec<String> = Vec::new();

    for local in files {
        let Some(page_id) = local.file.frontmatter.page_id() else {
            out.insert(
                local.path.clone(),
                PageStatus {
                    page_id: None,
                    path: local.path.clone(),
                    title: local.file.frontmatter.title.clone(),
                    state: PageState::LocalNew,
                    base_version: None,
                    remote_version: None,
                    local_dirty: true,
                    remote_ahead: false,
                    field_changes: FieldChanges::default(),
                    moved_from: None,
                    tampering: Vec::new(),
                    comment_drafts: local.file.drafts().count(),
                },
            );
            continue;
        };
        seen_ids.push(page_id.to_string());

        let base_record = base_by_id.get(page_id).copied();
        let remote_record = remote_by_id.get(page_id).copied();

        let Some(base_record) = base_record else {
            out.insert(
                local.path.clone(),
                PageStatus {
                    page_id: Some(page_id.to_string()),
                    path: local.path.clone(),
                    title: local.file.frontmatter.title.clone(),
                    state: PageState::Untracked,
                    base_version: None,
                    remote_version: remote_record.map(|r| r.version),
                    local_dirty: true,
                    remote_ahead: remote_record.is_some(),
                    field_changes: FieldChanges::default(),
                    moved_from: None,
                    tampering: Vec::new(),
                    comment_drafts: local.file.drafts().count(),
                },
            );
            continue;
        };

        let local_dirty = local.content_hash != base_record.markdown_hash;
        let remote_ahead =
            remote_record.is_some_and(|r| r.version > base_record.version && !r.deleted);
        let remote_deleted = remote_record.is_some_and(|r| r.deleted);

        let state = if base_record.sync_state == SyncState::Conflicted {
            PageState::Conflicted
        } else if remote_deleted {
            PageState::RemoteDeleted
        } else {
            match (local_dirty, remote_ahead) {
                (false, false) => PageState::Unchanged,
                (true, false) => PageState::Modified,
                (false, true) => PageState::Behind,
                (true, true) => PageState::Diverged,
            }
        };

        let fm = &local.file.frontmatter;
        let mut base_labels = base_record.labels.clone();
        base_labels.sort();
        let mut local_labels = fm.labels.clone();
        local_labels.sort();
        local_labels.dedup();

        let field_changes = FieldChanges {
            title: fm.title != base_record.title,
            labels: local_labels != base_labels,
            parent: fm.parent_id != base_record.parent_id,
        };

        let tampering = match &fm.managed {
            Some(managed) => frontmatter::detect_tampering(
                managed,
                &crate::frontmatter::Managed {
                    schema: managed.schema.min(frontmatter::SCHEMA_VERSION),
                    page_id: base_record.page_id.clone(),
                    space_key: managed.space_key.clone(),
                    version: base_record.version,
                    status: base_record.status.clone(),
                    position: base_record.position,
                    created: base_record.created_at.clone(),
                    updated: base_record.updated_at.clone(),
                    author: base_record.author.clone(),
                    attachments: managed.attachments.clone(),
                },
            ),
            None => Vec::new(),
        };

        let moved_from =
            (base_record.local_path != local.path).then(|| base_record.local_path.clone());

        out.insert(
            local.path.clone(),
            PageStatus {
                page_id: Some(page_id.to_string()),
                path: local.path.clone(),
                title: fm.title.clone(),
                state,
                base_version: Some(base_record.version),
                remote_version: remote_record.map(|r| r.version),
                local_dirty,
                remote_ahead,
                field_changes,
                moved_from,
                tampering,
                comment_drafts: local.file.drafts().count(),
            },
        );
    }

    // Base records with no working file: deleted locally.
    for record in base {
        if seen_ids.iter().any(|id| id == &record.page_id) {
            continue;
        }
        let remote_record = remote_by_id.get(record.page_id.as_str()).copied();
        let state = if remote_record.is_some_and(|r| r.deleted) {
            PageState::RemoteDeleted
        } else {
            PageState::LocalDeleted
        };
        out.insert(
            record.local_path.clone(),
            PageStatus {
                page_id: Some(record.page_id.clone()),
                path: record.local_path.clone(),
                title: record.title.clone(),
                state,
                base_version: Some(record.version),
                remote_version: remote_record.map(|r| r.version),
                local_dirty: true,
                remote_ahead: remote_record.is_some_and(|r| r.version > record.version),
                field_changes: FieldChanges::default(),
                moved_from: None,
                tampering: Vec::new(),
                comment_drafts: 0,
            },
        );
    }

    // Pages fetch found that were never materialized.
    for record in remote {
        if record.deleted || base_by_id.contains_key(record.page_id.as_str()) {
            continue;
        }
        let key = format!("\u{10ffff}remote:{}", record.page_id);
        out.insert(
            key,
            PageStatus {
                page_id: Some(record.page_id.clone()),
                path: String::new(),
                title: record.title.clone(),
                state: PageState::RemoteNew,
                base_version: None,
                remote_version: Some(record.version),
                local_dirty: false,
                remote_ahead: true,
                field_changes: FieldChanges::default(),
                moved_from: None,
                tampering: Vec::new(),
                comment_drafts: 0,
            },
        );
    }

    out.into_values().collect()
}

/// Scan the workspace and compute status for everything in it.
pub fn scan(ws: &Workspace) -> Result<Scan> {
    let (files, mut scan) = read_working_files(ws)?;
    scan.pages = compute_status(&files, &ws.state().all_pages()?, &ws.state().all_remote()?);
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::{Frontmatter, Managed};
    use crate::state::{now, SyncState};

    fn base_record(id: &str, version: u32, markdown_hash: &str) -> PageRecord {
        PageRecord {
            page_id: id.into(),
            title: "Onboarding".into(),
            slug: "Onboarding".into(),
            local_path: "Onboarding.md".into(),
            parent_id: None,
            position: None,
            version,
            status: "current".into(),
            labels: vec!["hr".into()],
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: "<p>base</p>".into(),
            storage_hash: "sh".into(),
            markdown_hash: markdown_hash.into(),
            block_map: None,
            sync_state: SyncState::Clean,
            synced_at: now(),
            render_key: String::new(),
        }
    }

    fn remote_record(id: &str, version: u32, deleted: bool) -> RemotePage {
        RemotePage {
            page_id: id.into(),
            title: "Onboarding".into(),
            parent_id: None,
            position: None,
            version,
            status: "current".into(),
            labels: vec!["hr".into()],
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: None,
            storage_hash: None,
            fetched_at: now(),
            deleted,
        }
    }

    fn local_file(path: &str, page_id: Option<&str>, version: u32, body: &str) -> LocalFile {
        let managed = page_id.map(|id| {
            let mut m = Managed::new(id, "DOCS", version);
            m.status = "current".into();
            m
        });
        let file = MarkdownFile::new(
            Frontmatter {
                title: "Onboarding".into(),
                labels: vec!["hr".into()],
                parent_id: None,
                managed,
                extra: Default::default(),
            },
            body,
        );
        let content_hash = file.content_hash();
        LocalFile { path: path.into(), file, content_hash }
    }

    /// The 2×2 of the sync model, plus the special states.
    #[test]
    fn the_four_core_states_follow_from_dirty_and_ahead() {
        let clean = local_file("Onboarding.md", Some("1"), 7, "body\n");
        let dirty = local_file("Onboarding.md", Some("1"), 7, "edited body\n");

        let cases = [
            (&clean, 7u32, PageState::Unchanged),
            (&dirty, 7, PageState::Modified),
            (&clean, 9, PageState::Behind),
            (&dirty, 9, PageState::Diverged),
        ];
        for (file, remote_version, expected) in cases {
            let base = base_record("1", 7, &clean.content_hash);
            let status = compute_status(
                std::slice::from_ref(file),
                &[base],
                &[remote_record("1", remote_version, false)],
            );
            assert_eq!(status[0].state, expected, "remote v{remote_version}");
        }
    }

    #[test]
    fn a_file_without_a_page_id_is_a_new_page() {
        let file = local_file("Draft.md", None, 0, "body\n");
        let status = compute_status(&[file], &[], &[]);
        assert_eq!(status[0].state, PageState::LocalNew);
        assert_eq!(status[0].page_id, None);
        assert!(status[0].state.has_local_work());
    }

    #[test]
    fn a_missing_file_is_a_local_deletion() {
        let base = base_record("1", 7, "hash");
        let status = compute_status(&[], &[base], &[remote_record("1", 7, false)]);
        assert_eq!(status[0].state, PageState::LocalDeleted);
        assert_eq!(status[0].path, "Onboarding.md");
    }

    #[test]
    fn a_page_only_on_the_server_is_remote_new() {
        let status = compute_status(&[], &[], &[remote_record("1", 3, false)]);
        assert_eq!(status[0].state, PageState::RemoteNew);
        assert!(status[0].state.has_remote_work());
    }

    #[test]
    fn remote_deletion_wins_over_local_edits_so_pull_can_warn() {
        let dirty = local_file("Onboarding.md", Some("1"), 7, "edited\n");
        let clean = local_file("Onboarding.md", Some("1"), 7, "body\n");
        let base = base_record("1", 7, &clean.content_hash);
        let status = compute_status(&[dirty], &[base], &[remote_record("1", 7, true)]);
        assert_eq!(status[0].state, PageState::RemoteDeleted);
    }

    #[test]
    fn a_conflicted_page_stays_conflicted_regardless_of_hashes() {
        let file = local_file("Onboarding.md", Some("1"), 7, "body\n");
        let mut base = base_record("1", 7, &file.content_hash);
        base.sync_state = SyncState::Conflicted;
        let status = compute_status(&[file], &[base], &[remote_record("1", 7, false)]);
        assert_eq!(status[0].state, PageState::Conflicted);
    }

    #[test]
    fn a_page_id_with_no_base_record_is_untracked_not_new() {
        let file = local_file("Onboarding.md", Some("1"), 7, "body\n");
        let status = compute_status(&[file], &[], &[]);
        assert_eq!(status[0].state, PageState::Untracked);
        assert_eq!(status[0].page_id.as_deref(), Some("1"));
    }

    #[test]
    fn frontmatter_field_changes_are_reported_separately_from_the_body() {
        let mut file = local_file("Onboarding.md", Some("1"), 7, "body\n");
        let base = base_record("1", 7, &file.content_hash);

        file.file.frontmatter.title = "Renamed".into();
        file.file.frontmatter.labels = vec!["hr".into(), "new".into()];
        file.file.frontmatter.parent_id = Some("42".into());
        file.content_hash = file.file.content_hash();

        let status = compute_status(&[file], &[base], &[]);
        let changes = status[0].field_changes;
        assert!(changes.title && changes.labels && changes.parent);
        assert!(changes.any());
    }

    #[test]
    fn reordering_labels_is_not_a_change() {
        let mut file = local_file("Onboarding.md", Some("1"), 7, "body\n");
        file.file.frontmatter.labels = vec!["b".into(), "a".into()];
        file.content_hash = file.file.content_hash();

        let mut base = base_record("1", 7, &file.content_hash);
        base.labels = vec!["a".into(), "b".into()];

        let status = compute_status(&[file], &[base], &[]);
        assert!(!status[0].field_changes.labels);
        assert_eq!(status[0].state, PageState::Unchanged);
    }

    #[test]
    fn a_renamed_file_is_tracked_by_page_id_not_path() {
        let file = local_file("Renamed.md", Some("1"), 7, "body\n");
        let base = base_record("1", 7, &file.content_hash);

        let status = compute_status(&[file], &[base], &[remote_record("1", 7, false)]);
        assert_eq!(status.len(), 1, "the page must not appear twice");
        assert_eq!(status[0].path, "Renamed.md");
        assert_eq!(status[0].moved_from.as_deref(), Some("Onboarding.md"));
    }

    #[test]
    fn edits_to_the_managed_block_are_detected() {
        let mut file = local_file("Onboarding.md", Some("1"), 7, "body\n");
        let base = base_record("1", 7, &file.content_hash);
        file.file.frontmatter.managed.as_mut().unwrap().version = 999;

        let status = compute_status(&[file], &[base], &[]);
        assert!(status[0].tampering.iter().any(|t| t.field == "version"));
        assert!(!status[0].is_clean());
    }

    #[test]
    fn scanning_skips_hidden_directories_and_generated_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::create(dir.path()).unwrap();

        std::fs::write(dir.path().join("Page.md"), "---\ntitle: Page\n---\n\nbody\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# agent contract\n").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# agent contract\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not markdown\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".Page")).unwrap();
        std::fs::write(dir.path().join(".Page/comments.md"), "---\ntitle: c\n---\n").unwrap();

        let (files, scan) = read_working_files(&ws).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["Page.md"], "sidecars and generated docs are not pages");
        assert!(scan.untracked_files.is_empty());
    }

    #[test]
    fn markdown_without_frontmatter_is_reported_untracked_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::create(dir.path()).unwrap();
        std::fs::write(dir.path().join("README.md"), "# just a readme\n").unwrap();
        std::fs::write(dir.path().join("Page.md"), "---\ntitle: Page\n---\n\nbody\n").unwrap();

        let (files, scan) = read_working_files(&ws).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(scan.untracked_files, ["README.md"]);
    }

    #[test]
    fn a_malformed_page_does_not_abort_the_whole_scan() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::create(dir.path()).unwrap();
        std::fs::write(dir.path().join("Bad.md"), "---\nlabels: []\n---\nbody\n").unwrap();
        std::fs::write(dir.path().join("Good.md"), "---\ntitle: Good\n---\n\nbody\n").unwrap();

        let (files, scan) = read_working_files(&ws).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(scan.unreadable.len(), 1);
        assert!(scan.unreadable[0].0.contains("Bad.md"));
        assert!(!scan.is_clean());
    }

    #[test]
    fn statuses_come_back_sorted_by_path() {
        let files = vec![
            local_file("z.md", None, 0, "b\n"),
            local_file("a.md", None, 0, "b\n"),
            local_file("m.md", None, 0, "b\n"),
        ];
        let status = compute_status(&files, &[], &[]);
        let paths: Vec<&str> = status.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["a.md", "m.md", "z.md"]);
    }
}
