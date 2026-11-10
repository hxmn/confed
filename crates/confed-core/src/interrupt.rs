//! Ctrl-C, asked for and honoured.
//!
//! A process killed mid-write can leave a page file and the base it was
//! written from out of step — a file newer than what `.state.db` says it came
//! from, which reads as a local edit nobody made. So the binary catches the
//! first Ctrl-C and only raises this flag, and the engine looks at it between
//! pages: the page in hand is finished and recorded, nothing after it is
//! started, and the outcome — what was done — comes back marked
//! `interrupted`, for the binary to exit 130 with.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ask whatever is running to stop at the next page boundary.
pub fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Whether a stop has been asked for.
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Forget an earlier request: for a long-lived process (the TUI) that has
/// honoured one and goes on.
pub fn clear() {
    REQUESTED.store(false, Ordering::SeqCst);
}
