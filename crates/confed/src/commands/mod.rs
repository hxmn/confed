//! One module per command. Each returns an [`Output`](crate::output::Output)
//! carrying both the human rendering and the JSON `result` payload.

pub mod agent_docs;
pub mod attach;
pub mod clone;
pub mod comment;
pub mod completion;
pub mod config;
pub mod diff;
pub mod doctor;
pub mod export;
pub mod init;
pub mod log;
pub mod mkdocs;
pub mod mv;
pub mod new;
pub mod open;
pub mod resolve;
pub mod rm;
pub mod search;
pub mod spaces;
pub mod status;
pub mod sync_cmds;
pub mod version;
pub mod whoami;

pub use sync_cmds::{fetch, pull, push};
