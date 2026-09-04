//! Attachments live in the page's hidden sidecar directory and are referenced
//! from the body with a relative link: `![alt](.Onboarding/diagram.png)`.

use crate::error::{ConfedError, Result};
use crate::frontmatter::AttachmentRef;
use crate::state::AttachmentRecord;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Suffix on confed's own half-finished downloads. A pull that dies mid-stream
/// can leave one behind in the sidecar directory, so every reader of that
/// directory has to know it is scratch, not content.
pub use confed_api::PARTIAL_SUFFIX;

/// Is this one of confed's own scratch files rather than a page attachment?
pub fn is_partial(name: &str) -> bool {
    name.ends_with(PARTIAL_SUFFIX)
}

/// Delete stale partial downloads left in a sidecar directory by an interrupted
/// pull. Returns how many went. Unreadable entries are left alone.
pub fn remove_stale_partials(sidecar_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(sidecar_dir) else { return 0 };
    let mut removed = 0;
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if is_partial(name) && path.is_file() && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Content hash of a file on disk, used to decide whether to re-upload.
pub fn file_sha256(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|e| ConfedError::io(format!("reading {}", path.display()), e))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// What push should do with one attachment.
#[derive(Clone, Debug, PartialEq)]
pub enum AttachmentAction {
    /// New file in the sidecar directory.
    Upload {
        filename: String,
    },
    /// Existing attachment whose bytes changed.
    Reupload {
        attachment_id: String,
        filename: String,
    },
    /// Recorded on the server but gone locally.
    Delete {
        attachment_id: String,
        filename: String,
    },
    Unchanged {
        filename: String,
    },
}

impl AttachmentAction {
    pub fn filename(&self) -> &str {
        match self {
            AttachmentAction::Upload { filename }
            | AttachmentAction::Reupload { filename, .. }
            | AttachmentAction::Delete { filename, .. }
            | AttachmentAction::Unchanged { filename } => filename,
        }
    }

    pub fn is_change(&self) -> bool {
        !matches!(self, AttachmentAction::Unchanged { .. })
    }
}

/// Compare the sidecar directory against what the server has.
///
/// Files whose hash matches the recorded one are left alone; changed files are
/// re-uploaded as a new version; recorded attachments with no local file are
/// deletions. Files confed cannot read are skipped rather than fatal.
pub fn diff_attachments(
    sidecar_dir: &Path,
    recorded: &[AttachmentRecord],
) -> Result<Vec<AttachmentAction>> {
    let mut actions = Vec::new();
    let mut local: Vec<(String, std::path::PathBuf)> = Vec::new();

    if sidecar_dir.is_dir() {
        let entries = std::fs::read_dir(sidecar_dir)
            .map_err(|e| ConfedError::io(format!("reading {}", sidecar_dir.display()), e))?;
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            // confed's own sidecar files are never attachments. Partial
            // downloads are filtered here rather than in the uploader so that
            // `push --dry-run` does not offer them either.
            if name == crate::comments::COMMENTS_FILENAME
                || name == crate::paths::STORAGE_FILENAME
                || name.starts_with('.')
                || is_partial(name)
            {
                continue;
            }
            local.push((name.to_string(), path));
        }
    }
    local.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in &local {
        match recorded.iter().find(|r| &r.filename == name) {
            Some(record) => {
                let hash = file_sha256(path)?;
                if record.sha256.as_deref() == Some(hash.as_str()) {
                    actions.push(AttachmentAction::Unchanged { filename: name.clone() });
                } else {
                    actions.push(AttachmentAction::Reupload {
                        attachment_id: record.attachment_id.clone(),
                        filename: name.clone(),
                    });
                }
            }
            None => actions.push(AttachmentAction::Upload { filename: name.clone() }),
        }
    }

    for record in recorded {
        if !local.iter().any(|(name, _)| name == &record.filename) {
            actions.push(AttachmentAction::Delete {
                attachment_id: record.attachment_id.clone(),
                filename: record.filename.clone(),
            });
        }
    }

    Ok(actions)
}

/// Frontmatter entries for a page's attachments.
pub fn to_refs(records: &[AttachmentRecord]) -> Vec<AttachmentRef> {
    records
        .iter()
        .map(|r| AttachmentRef {
            id: r.attachment_id.clone(),
            file: r.filename.clone(),
            size: r.file_size,
            sha256: r.sha256.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, filename: &str, sha: &str) -> AttachmentRecord {
        AttachmentRecord {
            attachment_id: id.into(),
            page_id: "1".into(),
            filename: filename.into(),
            media_type: None,
            file_size: Some(3),
            version: 1,
            sha256: Some(sha.into()),
            downloaded: true,
        }
    }

    #[test]
    fn unchanged_files_are_not_reuploaded() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("diagram.png");
        std::fs::write(&file, b"abc").unwrap();
        let hash = file_sha256(&file).unwrap();

        let actions =
            diff_attachments(dir.path(), &[record("att1", "diagram.png", &hash)]).unwrap();
        assert_eq!(actions, vec![AttachmentAction::Unchanged { filename: "diagram.png".into() }]);
        assert!(!actions[0].is_change());
    }

    #[test]
    fn changed_bytes_become_a_new_version_of_the_same_attachment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("diagram.png"), b"new content").unwrap();

        let actions =
            diff_attachments(dir.path(), &[record("att1", "diagram.png", "stale-hash")]).unwrap();
        assert_eq!(
            actions,
            vec![AttachmentAction::Reupload {
                attachment_id: "att1".into(),
                filename: "diagram.png".into()
            }]
        );
    }

    #[test]
    fn new_files_are_uploaded_and_missing_ones_deleted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fresh.png"), b"x").unwrap();

        let actions = diff_attachments(dir.path(), &[record("att1", "gone.png", "h")]).unwrap();
        assert!(actions.contains(&AttachmentAction::Upload { filename: "fresh.png".into() }));
        assert!(actions.contains(&AttachmentAction::Delete {
            attachment_id: "att1".into(),
            filename: "gone.png".into()
        }));
    }

    #[test]
    fn confeds_own_sidecar_files_are_never_treated_as_attachments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::comments::COMMENTS_FILENAME), "# Comments\n")
            .unwrap();
        std::fs::write(dir.path().join(crate::paths::STORAGE_FILENAME), "<p>body</p>").unwrap();
        std::fs::write(dir.path().join(".hidden"), "x").unwrap();

        assert!(
            diff_attachments(dir.path(), &[]).unwrap().is_empty(),
            "uploading these back to Confluence would be nonsense"
        );
    }

    #[test]
    fn a_partial_download_is_never_a_push_candidate() {
        let dir = tempfile::tempdir().unwrap();
        // A pull that died mid-stream, next to the file it was a partial copy of.
        std::fs::write(dir.path().join("video.webm"), b"the whole thing").unwrap();
        std::fs::write(dir.path().join(".video.webm.confed-part"), b"the whole").unwrap();
        // The pre-fix naming, which older workspaces still have on disk.
        std::fs::write(dir.path().join("video.webm.confed-part"), b"the whole").unwrap();

        let actions = diff_attachments(dir.path(), &[]).unwrap();
        assert_eq!(
            actions,
            vec![AttachmentAction::Upload { filename: "video.webm".into() }],
            "only the completed download is content"
        );
    }

    #[test]
    fn stale_partials_are_swept_and_real_attachments_are_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("video.webm"), b"keep me").unwrap();
        std::fs::write(dir.path().join(".video.webm.confed-part"), b"scratch").unwrap();
        std::fs::write(dir.path().join("video.webm.confed-part"), b"scratch").unwrap();

        assert_eq!(remove_stale_partials(dir.path()), 2);
        assert!(dir.path().join("video.webm").exists(), "the attachment survives");
        assert!(!dir.path().join(".video.webm.confed-part").exists());
        assert!(!dir.path().join("video.webm.confed-part").exists());

        // Sweeping a directory that is not there is a no-op, not an error.
        assert_eq!(remove_stale_partials(&dir.path().join("nope")), 0);
    }

    #[test]
    fn a_page_with_no_sidecar_directory_has_no_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(diff_attachments(&missing, &[]).unwrap().is_empty());

        // But recorded attachments still count as deletions.
        let actions = diff_attachments(&missing, &[record("att1", "gone.png", "h")]).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AttachmentAction::Delete { .. }));
    }

    #[test]
    fn hashes_are_stable_and_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"same").unwrap();
        std::fs::write(&b, b"same").unwrap();
        assert_eq!(file_sha256(&a).unwrap(), file_sha256(&b).unwrap());

        std::fs::write(&b, b"different").unwrap();
        assert_ne!(file_sha256(&a).unwrap(), file_sha256(&b).unwrap());
    }
}
