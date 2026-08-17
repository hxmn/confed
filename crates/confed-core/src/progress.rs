//! Progress reporting for the long-running commands.
//!
//! The sync engine reports what it is doing through [`Progress`]; deciding
//! whether any of it is *shown* belongs to the caller. That keeps the engine
//! usable from the CLI, the TUI and tests without any of them inheriting a
//! terminal assumption.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Receives progress events from a long-running operation.
///
/// Implementations must be cheap: `item` is called once per page, and the
/// engine does not check whether anyone is listening.
pub trait Progress: Send + Sync {
    /// A new stage began. `total` is the number of items when it is known up
    /// front, which it is not for the listing stage of a fetch.
    fn stage(&self, name: &str, total: Option<usize>);

    /// One item finished. `detail` is what the user would recognize it by —
    /// a page path or title.
    fn item(&self, detail: &str);

    /// The operation finished; clear any transient display.
    fn finish(&self);
}

/// Discards everything. The default, and what non-interactive runs use.
pub struct NoProgress;

impl Progress for NoProgress {
    fn stage(&self, _name: &str, _total: Option<usize>) {}
    fn item(&self, _detail: &str) {}
    fn finish(&self) {}
}

/// A shared handle, so the engine can hold one without caring which kind it is.
pub type ProgressRef = Arc<dyn Progress>;

pub fn none() -> ProgressRef {
    Arc::new(NoProgress)
}

/// Records every event, for tests.
#[derive(Default)]
pub struct RecordingProgress {
    stages: Mutex<Vec<(String, Option<usize>)>>,
    items: Mutex<Vec<String>>,
    finishes: AtomicUsize,
}

impl RecordingProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stages(&self) -> Vec<(String, Option<usize>)> {
        self.stages.lock().expect("progress poisoned").clone()
    }

    pub fn stage_names(&self) -> Vec<String> {
        self.stages().into_iter().map(|(name, _)| name).collect()
    }

    pub fn items(&self) -> Vec<String> {
        self.items.lock().expect("progress poisoned").clone()
    }

    pub fn finish_count(&self) -> usize {
        self.finishes.load(Ordering::SeqCst)
    }
}

impl Progress for RecordingProgress {
    fn stage(&self, name: &str, total: Option<usize>) {
        self.stages.lock().expect("progress poisoned").push((name.to_string(), total));
    }

    fn item(&self, detail: &str) {
        self.items.lock().expect("progress poisoned").push(detail.to_string());
    }

    fn finish(&self) {
        self.finishes.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_null_reporter_accepts_everything_and_keeps_nothing() {
        let progress = NoProgress;
        progress.stage("Fetching", Some(3));
        progress.item("a page");
        progress.finish();
    }

    #[test]
    fn the_recorder_keeps_events_in_order() {
        let progress = RecordingProgress::new();
        progress.stage("Fetching", Some(2));
        progress.item("One.md");
        progress.item("Two.md");
        progress.stage("Writing", None);
        progress.finish();

        assert_eq!(
            progress.stages(),
            [("Fetching".to_string(), Some(2)), ("Writing".to_string(), None)]
        );
        assert_eq!(progress.items(), ["One.md", "Two.md"]);
        assert_eq!(progress.finish_count(), 1);
    }

    #[test]
    fn a_reporter_can_be_shared_across_threads() {
        let recorder = Arc::new(RecordingProgress::new());

        std::thread::scope(|scope| {
            for i in 0..4 {
                let progress: ProgressRef = Arc::clone(&recorder) as ProgressRef;
                scope.spawn(move || progress.item(&format!("page {i}")));
            }
        });

        let mut items = recorder.items();
        items.sort();
        assert_eq!(items, ["page 0", "page 1", "page 2", "page 3"]);
    }
}
