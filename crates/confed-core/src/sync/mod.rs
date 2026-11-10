//! The sync engine: `fetch`, `pull`, `push`.
//!
//! Invariants that hold across all three:
//!
//! 1. The base (`pages` in `.state.db`) only ever advances to a state the server
//!    confirmed — a fetch result or a push response, never a local edit.
//! 2. Working files are written atomically (temp file + rename).
//! 3. `pull` never clobbers local edits it cannot merge; `push` never uploads a
//!    page whose base version is stale or whose merge is unresolved.

use crate::attachments;
use crate::comments;
use crate::error::{ConfedError, Result};
use crate::frontmatter::{AttachmentRef, Frontmatter, Managed, MarkdownFile};
use crate::merge::{self, RemoteLabel, ScalarMerge};
use crate::paths::{self, Placement};
use crate::progress::{self, ProgressRef};
use crate::state::{
    hash_str, now, AttachmentRecord, CommentRecord, PageRecord, RemotePage, SyncState,
};
use crate::workspace::Workspace;
use crate::worktree::{self, LocalFile, PageState, PageStatus};
use confed_api::{
    BodyFormat, Comment, CommentKind, ConfluenceClient, NewPage, Page, PageId, PageUpdate, SpaceId,
};
use confed_converter::{
    marks, BlockMap, ConvertOptions, InlineMark, Mark, MarkId, MarkIssue, PlacedMark,
};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

mod comment_push;
mod convert;
mod fetch;
mod files;
mod inline_marks;
mod pull;
mod push;
mod sidecar;

pub use comment_push::*;
pub use convert::*;
pub use fetch::*;
pub use files::*;
pub use inline_marks::*;
pub use pull::*;
pub use push::*;
use sidecar::*;

pub struct SyncEngine {
    client: Arc<dyn ConfluenceClient>,
    space: SpaceId,
    concurrency: usize,
    progress: ProgressRef,
}

/// Group a scan's pages by state, for reporting.
pub fn group_by_state(statuses: &[PageStatus]) -> BTreeMap<&'static str, Vec<&PageStatus>> {
    let mut out: BTreeMap<&'static str, Vec<&PageStatus>> = BTreeMap::new();
    for status in statuses {
        out.entry(status.state.as_str()).or_default().push(status);
    }
    out
}

impl SyncEngine {
    pub fn new(client: Arc<dyn ConfluenceClient>, space: SpaceId, concurrency: usize) -> Self {
        let concurrency = if concurrency == 0 {
            client.capabilities().max_request_concurrency
        } else {
            concurrency
        };
        Self { client, space, concurrency, progress: progress::none() }
    }

    /// Report what the engine is doing. Without this, progress is discarded.
    pub fn with_progress(mut self, progress: ProgressRef) -> Self {
        self.progress = progress;
        self
    }

    pub fn client(&self) -> &Arc<dyn ConfluenceClient> {
        &self.client
    }
}

#[cfg(test)]
mod tests;
