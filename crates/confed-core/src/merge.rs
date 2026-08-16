//! Three-way merge for diverged pages.
//!
//! All three inputs are Markdown — base is regenerated from the storage body
//! confed kept, ours is the working file, theirs is the freshly fetched remote
//! version rendered through the same converter — so a line-based merge is
//! meaningful rather than a comparison of two different representations.

use std::collections::BTreeSet;

/// Result of merging one page.
#[derive(Clone, Debug, PartialEq)]
pub enum MergeOutcome {
    /// Merged without conflicts.
    Clean(String),
    /// Merged with conflict markers left in the text.
    Conflicted(String),
}

impl MergeOutcome {
    pub fn text(&self) -> &str {
        match self {
            MergeOutcome::Clean(t) | MergeOutcome::Conflicted(t) => t,
        }
    }

    pub fn is_conflicted(&self) -> bool {
        matches!(self, MergeOutcome::Conflicted(_))
    }
}

/// How the remote side is described in conflict markers.
#[derive(Clone, Debug, Default)]
pub struct RemoteLabel {
    pub version: Option<u32>,
    pub author: Option<String>,
    pub when: Option<String>,
}

impl RemoteLabel {
    fn render(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.version {
            parts.push(format!("v{v}"));
        }
        if let Some(a) = &self.author {
            parts.push(format!("edited by {a}"));
        }
        if let Some(w) = &self.when {
            parts.push(w.clone());
        }
        if parts.is_empty() {
            "remote".to_string()
        } else {
            format!("remote ({})", parts.join(", "))
        }
    }
}

/// Merge page bodies. Conflict markers use confed's labels rather than diffy's
/// defaults, so the file explains which side is which.
pub fn merge_bodies(base: &str, ours: &str, theirs: &str, remote: &RemoteLabel) -> MergeOutcome {
    if ours == theirs {
        return MergeOutcome::Clean(ours.to_string());
    }
    if base == ours {
        // Only the remote changed: fast-forward.
        return MergeOutcome::Clean(theirs.to_string());
    }
    if base == theirs {
        // Only we changed.
        return MergeOutcome::Clean(ours.to_string());
    }

    // Diff3 keeps the base section between the markers, so a human resolving the
    // conflict can see what the text said before either side touched it.
    let mut options = diffy::MergeOptions::new();
    options.set_conflict_style(diffy::ConflictStyle::Diff3);
    match options.merge(base, ours, theirs) {
        Ok(merged) => MergeOutcome::Clean(relabel(&merged, remote)),
        Err(conflicted) => MergeOutcome::Conflicted(relabel(&conflicted, remote)),
    }
}

/// Rewrite diffy's `ours`/`original`/`theirs` marker labels into confed's.
fn relabel(text: &str, remote: &RemoteLabel) -> String {
    let remote_label = remote.render();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        let replacement = if trimmed.starts_with("<<<<<<<") {
            Some("<<<<<<< local".to_string())
        } else if trimmed.starts_with("|||||||") {
            Some("||||||| base".to_string())
        } else if trimmed.starts_with(">>>>>>>") {
            Some(format!(">>>>>>> {remote_label}"))
        } else {
            None
        };
        match replacement {
            Some(text) => {
                out.push_str(&text);
                out.push('\n');
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Does this text still contain unresolved conflict markers?
pub fn has_conflict_markers(text: &str) -> bool {
    text.lines()
        .any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>") || l.starts_with("|||||||"))
}

/// Lines that still carry conflict markers, for error messages.
pub fn conflict_marker_lines(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>") || l.starts_with("|||||||")
        })
        .map(|(i, _)| i + 1)
        .collect()
}

/// Three-way set merge for labels: additions from both sides are kept,
/// deletions from either side are honored.
pub fn merge_labels(base: &[String], ours: &[String], theirs: &[String]) -> Vec<String> {
    let base_set: BTreeSet<&String> = base.iter().collect();
    let ours_set: BTreeSet<&String> = ours.iter().collect();
    let theirs_set: BTreeSet<&String> = theirs.iter().collect();

    let mut result: BTreeSet<String> = base_set.iter().map(|s| (*s).clone()).collect();
    for added in ours_set.difference(&base_set).chain(theirs_set.difference(&base_set)) {
        result.insert((*added).clone());
    }
    for removed in base_set.difference(&ours_set).chain(base_set.difference(&theirs_set)) {
        result.remove(*removed);
    }
    result.into_iter().collect()
}

/// Merging a single value (title, parent): whichever side changed wins; if both
/// changed to different values it is a conflict.
#[derive(Clone, Debug, PartialEq)]
pub enum ScalarMerge<T> {
    Value(T),
    Conflict { ours: T, theirs: T },
}

pub fn merge_scalar<T: Clone + PartialEq>(base: &T, ours: &T, theirs: &T) -> ScalarMerge<T> {
    if ours == theirs {
        return ScalarMerge::Value(ours.clone());
    }
    if base == ours {
        return ScalarMerge::Value(theirs.clone());
    }
    if base == theirs {
        return ScalarMerge::Value(ours.clone());
    }
    ScalarMerge::Conflict { ours: ours.clone(), theirs: theirs.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label() -> RemoteLabel {
        RemoteLabel {
            version: Some(9),
            author: Some("Alice Ng".into()),
            when: Some("2026-08-14".into()),
        }
    }

    #[test]
    fn disjoint_edits_merge_cleanly() {
        let base = "line one\nline two\nline three\n";
        let ours = "line one EDITED\nline two\nline three\n";
        let theirs = "line one\nline two\nline three CHANGED\n";

        let merged = merge_bodies(base, ours, theirs, &label());
        assert!(!merged.is_conflicted(), "{}", merged.text());
        assert!(merged.text().contains("line one EDITED"));
        assert!(merged.text().contains("line three CHANGED"));
    }

    #[test]
    fn same_line_edits_conflict_with_confed_labels() {
        let base = "shared line\n";
        let ours = "our version\n";
        let theirs = "their version\n";

        let merged = merge_bodies(base, ours, theirs, &label());
        assert!(merged.is_conflicted());
        let text = merged.text();
        assert!(text.contains("<<<<<<< local"), "{text}");
        assert!(text.contains("||||||| base"), "the base section explains what changed: {text}");
        assert!(text.contains(">>>>>>> remote (v9, edited by Alice Ng, 2026-08-14)"), "{text}");
        assert!(text.contains("our version"));
        assert!(text.contains("shared line"), "base text is shown");
        assert!(text.contains("their version"));
        assert!(has_conflict_markers(text));
        assert_eq!(conflict_marker_lines(text).len(), 3, "open, base, close");
    }

    #[test]
    fn one_sided_changes_fast_forward() {
        let base = "a\n";
        assert_eq!(merge_bodies(base, base, "b\n", &label()), MergeOutcome::Clean("b\n".into()));
        assert_eq!(merge_bodies(base, "c\n", base, &label()), MergeOutcome::Clean("c\n".into()));
        // Both sides made the same edit.
        assert_eq!(merge_bodies(base, "d\n", "d\n", &label()), MergeOutcome::Clean("d\n".into()));
    }

    #[test]
    fn clean_text_has_no_markers() {
        assert!(!has_conflict_markers("normal text\nwith lines\n"));
        assert!(conflict_marker_lines("normal\n").is_empty());
        // A line that merely mentions arrows is not a marker.
        assert!(!has_conflict_markers("a --> b\n"));
    }

    #[test]
    fn labels_merge_as_a_three_way_set() {
        let base = vec!["hr".to_string(), "draft".to_string()];
        let ours = vec!["hr".to_string(), "onboarding".to_string()]; // -draft +onboarding
        let theirs = vec!["hr".to_string(), "draft".to_string(), "reviewed".to_string()]; // +reviewed

        let merged = merge_labels(&base, &ours, &theirs);
        assert_eq!(merged, vec!["hr", "onboarding", "reviewed"]);
    }

    #[test]
    fn a_label_deleted_on_either_side_stays_deleted() {
        let base = vec!["a".to_string(), "b".to_string()];
        assert_eq!(merge_labels(&base, &["a".to_string()], &base), vec!["a"]);
        assert_eq!(merge_labels(&base, &base, &["b".to_string()]), vec!["b"]);
        // Both removed the same label.
        assert_eq!(merge_labels(&base, &["a".into()], &["a".into()]), vec!["a"]);
    }

    #[test]
    fn scalars_take_the_side_that_changed() {
        let base = "Old Title".to_string();
        assert_eq!(
            merge_scalar(&base, &base, &"Server Title".to_string()),
            ScalarMerge::Value("Server Title".to_string())
        );
        assert_eq!(
            merge_scalar(&base, &"My Title".to_string(), &base),
            ScalarMerge::Value("My Title".to_string())
        );
        assert_eq!(
            merge_scalar(&base, &"Mine".to_string(), &"Theirs".to_string()),
            ScalarMerge::Conflict { ours: "Mine".into(), theirs: "Theirs".into() }
        );
    }

    #[test]
    fn merging_preserves_a_trailing_newline() {
        let merged = merge_bodies("a\nb\n", "a\nb2\n", "a2\nb\n", &label());
        assert!(merged.text().ends_with('\n'));
    }
}
