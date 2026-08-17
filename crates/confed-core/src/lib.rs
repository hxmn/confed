//! Core of confed: workspace state, the sync engine, and everything that decides
//! what to write where.
//!
//! The sync model mirrors git: `pages` in `.state.db` is the **base** (what the
//! working file was materialized from), `remote_pages` is what `fetch` last saw,
//! and the `.md` files are the working tree. Every page's status is derived by
//! comparing those three.

pub mod attachments;
pub mod comments;
pub mod config;
pub mod error;
pub mod frontmatter;
pub mod lock;
pub mod merge;
pub mod paths;
pub mod progress;
pub mod reanchor;
pub mod session;
pub mod slug;
pub mod state;
pub mod sync;
pub mod workspace;
pub mod worktree;

pub use error::{ConfedError, ExitCode, Result};
