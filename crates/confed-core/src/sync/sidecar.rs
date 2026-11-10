//! What a page's sidecar directory holds of its attachments, and whether a
//! copy there is the one confed wrote.

use super::*;

/// What bringing one page's sidecar in line with its attachments did.
#[derive(Default)]
pub(super) struct AttachmentSync {
    pub(super) downloaded: usize,
    /// Local copies removed because the server no longer has the attachment.
    pub(super) removed: usize,
    /// Whether the page's attachments are not what its file last listed.
    pub(super) changed: bool,
    pub(super) warnings: Vec<String>,
}

/// Does this attachment have to come down?
///
/// It does while the server has a file, or a version of one, that confed has
/// not written here. Once it has, whatever the sidecar shows instead — an
/// edited file, or none, which is how removing an attachment starts — is local
/// work, and only a reset undoes that. A reset still downloads no more than
/// what differs: re-fetching a file that already matches the server is pure
/// waste, and on a page full of images it is the slowest part of the reset.
pub(super) fn needs_download(record: &AttachmentRecord, dest: &Path, reset: bool) -> bool {
    let Some(recorded) = record.sha256.as_deref().filter(|_| record.downloaded) else {
        return true;
    };
    // Compare the bytes on disk, since a reset exists to undo local changes.
    reset && attachments::file_sha256(dest).ok().as_deref() != Some(recorded)
}

/// Whether the file at `dest` is the copy of this attachment confed last
/// wrote there — the one thing in a sidecar a pull may replace unasked.
pub(super) fn is_our_copy(record: &AttachmentRecord, dest: &Path) -> bool {
    record.sha256.is_some()
        && attachments::file_sha256(dest).ok().as_deref() == record.sha256.as_deref()
}

/// Whether a pull would download this attachment: it has to come down, and
/// nothing local is in the way — or the pull was told to discard what is.
pub(super) fn would_download(
    record: &AttachmentRecord,
    dest: &Path,
    reset: bool,
    discard: bool,
) -> bool {
    needs_download(record, dest, reset) && (discard || !dest.exists() || is_our_copy(record, dest))
}

/// The attachments a page file lists in its frontmatter, if it can be read.
pub(super) fn listed_attachments(ws: &Workspace, page_path: &str) -> Option<Vec<AttachmentRef>> {
    let content = std::fs::read_to_string(ws.absolute(page_path)).ok()?;
    let file = crate::frontmatter::parse(&content, page_path).ok()?;
    Some(file.frontmatter.managed?.attachments)
}

/// Whether two frontmatter lists name the same attachments. Sizes and hashes
/// follow the copies in the sidecar and are not what makes a list different.
pub(super) fn same_refs(a: &[AttachmentRef], b: &[AttachmentRef]) -> bool {
    let key = |refs: &[AttachmentRef]| {
        let mut keys: Vec<_> = refs.iter().map(|r| (r.id.clone(), r.file.clone())).collect();
        keys.sort();
        keys
    };
    key(a) == key(b)
}

/// Make the `attachments` a page file lists the ones in the state, touching
/// nothing but its frontmatter, and only when the list is not already that.
/// Returns whether the file was naming other attachments than the state has.
pub(super) fn write_attachment_refs(
    ws: &Workspace,
    page_id: &str,
    page_path: &str,
) -> Result<bool> {
    let abs = ws.absolute(page_path);
    let Ok(content) = std::fs::read_to_string(&abs) else { return Ok(false) };
    let Ok(mut file) = crate::frontmatter::parse(&content, page_path) else { return Ok(false) };
    let Some(managed) = file.frontmatter.managed.as_mut() else { return Ok(false) };
    let refs = attachments::to_refs(&ws.state().page_attachments(page_id)?);
    if managed.attachments == refs {
        return Ok(false);
    }
    let relisted = !same_refs(&managed.attachments, &refs);
    managed.attachments = refs;

    let Some((_, body)) = crate::frontmatter::split(&content) else { return Ok(false) };
    write_atomic(&abs, &format!("{}\n{body}", file.render_frontmatter()?))?;
    Ok(relisted)
}

/// Warnings for the comments of a page that show or link a file the page does
/// not have — deleted on the server since, or never listed.
pub(super) fn missing_comment_attachments(
    ws: &Workspace,
    page: &PageRecord,
) -> Result<Vec<String>> {
    let comments = ws.state().page_comments(&page.page_id)?;
    if comments.is_empty() {
        return Ok(Vec::new());
    }
    let attached = ws.state().page_attachments(&page.page_id)?;
    let mut out = Vec::new();
    for comment in &comments {
        let Some(storage) = comment.body_storage.as_deref() else { continue };
        for file in confed_converter::attachment_references(storage) {
            if !attached.iter().any(|a| a.filename == file) {
                out.push(format!(
                    "{}: comment {} references {file}, which is not among the page's \
                     attachments",
                    page.local_path, comment.comment_id
                ));
            }
        }
    }
    Ok(out)
}
