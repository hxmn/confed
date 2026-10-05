//! The three-pane conflict resolver.
//!
//! The marker parsing lives in `commands::resolve`, shared with the headless
//! `confed resolve`; this module only tracks which side the user picked for each
//! hunk and renders the file back out.

use confed_cli::commands::resolve::{ConflictHunk, Segment};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Still showing conflict markers.
    Unresolved,
    Ours,
    Theirs,
}

impl Choice {
    pub fn label(self) -> &'static str {
        match self {
            Choice::Unresolved => "unresolved",
            Choice::Ours => "ours",
            Choice::Theirs => "theirs",
        }
    }
}

/// One page being resolved.
pub struct Resolver {
    pub page_id: String,
    pub path: String,
    /// The file as it was read, so an unchanged resolution writes nothing.
    pub original: String,
    segments: Vec<Segment>,
    /// Indices into `segments` that are conflicts, and the choice for each.
    hunks: Vec<usize>,
    choices: Vec<Choice>,
    pub current: usize,
    pub scroll: u16,
}

impl Resolver {
    /// `None` when the file has no conflict markers left.
    pub fn new(page_id: &str, path: &str, content: &str) -> Option<Self> {
        let segments = confed_cli::commands::resolve::split_conflicts(content);
        let hunks: Vec<usize> = segments
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Segment::Conflict(_)))
            .map(|(i, _)| i)
            .collect();
        if hunks.is_empty() {
            return None;
        }
        let choices = vec![Choice::Unresolved; hunks.len()];
        Some(Self {
            page_id: page_id.to_string(),
            path: path.to_string(),
            original: content.to_string(),
            segments,
            hunks,
            choices,
            current: 0,
            scroll: 0,
        })
    }

    /// Never zero: a resolver only exists when there is a conflict.
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    pub fn hunk(&self) -> &ConflictHunk {
        match &self.segments[self.hunks[self.current]] {
            Segment::Conflict(hunk) => hunk,
            Segment::Text(_) => unreachable!("hunks only ever index conflicts"),
        }
    }

    pub fn choice(&self) -> Choice {
        self.choices[self.current]
    }

    pub fn unresolved(&self) -> usize {
        self.choices.iter().filter(|c| **c == Choice::Unresolved).count()
    }

    /// Pick a side and step to the next unresolved hunk, so holding `u`/`t`
    /// walks the whole file.
    pub fn choose(&mut self, choice: Choice) {
        self.choices[self.current] = choice;
        self.scroll = 0;
        if let Some(next) = self.next_unresolved() {
            self.current = next;
        }
    }

    fn next_unresolved(&self) -> Option<usize> {
        (self.current + 1..self.hunks.len())
            .chain(0..self.current)
            .find(|i| self.choices[*i] == Choice::Unresolved)
    }

    pub fn next(&mut self) {
        if self.current + 1 < self.hunks.len() {
            self.current += 1;
            self.scroll = 0;
        }
    }

    pub fn previous(&mut self) {
        if self.current > 0 {
            self.current -= 1;
            self.scroll = 0;
        }
    }

    pub fn choose_all(&mut self, choice: Choice) {
        self.choices.fill(choice);
    }

    /// The file with every resolved hunk collapsed to the chosen side.
    ///
    /// Unresolved hunks keep their markers, so writing the file out and running
    /// `confed resolve` afterwards reports exactly the same thing.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.original.len());
        let mut hunk_number = 0;
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => out.push_str(text),
                Segment::Conflict(hunk) => {
                    match self.choices[hunk_number] {
                        Choice::Ours => out.push_str(&hunk.ours),
                        Choice::Theirs => out.push_str(&hunk.theirs),
                        Choice::Unresolved => out.push_str(&markers(hunk)),
                    }
                    hunk_number += 1;
                }
            }
        }
        out
    }
}

/// Re-emit a conflict block in confed's marker style.
fn markers(hunk: &ConflictHunk) -> String {
    let label = if hunk.label.is_empty() { "remote" } else { &hunk.label };
    format!(
        "<<<<<<< local\n{}||||||| base\n{}=======\n{}>>>>>>> {label}\n",
        hunk.ours, hunk.base, hunk.theirs
    )
}

/// Split a side into display lines, keeping empty sides visible.
pub fn side_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec!["(empty)".to_string()];
    }
    text.lines().map(str::to_string).collect()
}

#[cfg(test)]
pub(crate) const CONFLICTED_PAGE: &str = "---\ntitle: Runbook\n---\n\nIntro.\n\
     <<<<<<< local\nour paragraph\n||||||| base\nthe original\n=======\ntheir paragraph\n\
     >>>>>>> remote (v9)\nOutro.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_file_has_nothing_to_resolve() {
        assert!(Resolver::new("1", "P.md", "no conflicts here\n").is_none());
    }

    #[test]
    fn the_three_sides_are_pulled_out_of_the_markers() {
        let resolver = Resolver::new("1", "Runbook.md", CONFLICTED_PAGE).unwrap();
        assert_eq!(resolver.hunk_count(), 1);
        let hunk = resolver.hunk();
        assert_eq!(hunk.ours, "our paragraph\n");
        assert_eq!(hunk.base, "the original\n");
        assert_eq!(hunk.theirs, "their paragraph\n");
        assert_eq!(hunk.label, "remote (v9)");
    }

    #[test]
    fn an_untouched_resolver_round_trips_the_file_byte_for_byte() {
        let resolver = Resolver::new("1", "Runbook.md", CONFLICTED_PAGE).unwrap();
        assert_eq!(resolver.render(), CONFLICTED_PAGE);
        assert_eq!(resolver.unresolved(), 1);
    }

    #[test]
    fn taking_a_side_removes_the_markers() {
        let mut ours = Resolver::new("1", "Runbook.md", CONFLICTED_PAGE).unwrap();
        ours.choose(Choice::Ours);
        assert_eq!(ours.unresolved(), 0);
        let text = ours.render();
        assert!(text.contains("our paragraph"));
        assert!(!text.contains("their paragraph"));
        assert!(!text.contains("the original"));
        assert!(!confed_core::merge::has_conflict_markers(&text));

        let mut theirs = Resolver::new("1", "Runbook.md", CONFLICTED_PAGE).unwrap();
        theirs.choose(Choice::Theirs);
        assert!(theirs.render().contains("their paragraph"));
    }

    #[test]
    fn choosing_advances_to_the_next_unresolved_hunk() {
        let two = "a\n<<<<<<< local\nx\n||||||| base\no\n=======\ny\n>>>>>>> remote\n\
                   b\n<<<<<<< local\np\n||||||| base\nq\n=======\nr\n>>>>>>> remote\nc\n";
        let mut resolver = Resolver::new("1", "P.md", two).unwrap();
        assert_eq!(resolver.hunk_count(), 2);
        assert_eq!(resolver.current, 0);
        resolver.choose(Choice::Ours);
        assert_eq!(resolver.current, 1, "the cursor moves to the hunk still open");
        resolver.choose(Choice::Theirs);
        assert_eq!(resolver.unresolved(), 0);
        assert_eq!(resolver.render(), "a\nx\nb\nr\nc\n");
    }

    #[test]
    fn a_partly_resolved_file_keeps_the_remaining_markers() {
        let two = "<<<<<<< local\nx\n||||||| base\no\n=======\ny\n>>>>>>> remote (v2)\n\
                   <<<<<<< local\np\n||||||| base\nq\n=======\nr\n>>>>>>> remote (v2)\n";
        let mut resolver = Resolver::new("1", "P.md", two).unwrap();
        resolver.choices[0] = Choice::Ours;
        let text = resolver.render();
        assert!(text.starts_with("x\n"));
        assert!(confed_core::merge::has_conflict_markers(&text));
        assert_eq!(text, "x\n<<<<<<< local\np\n||||||| base\nq\n=======\nr\n>>>>>>> remote (v2)\n");
    }

    #[test]
    fn take_all_resolves_every_hunk_at_once() {
        let mut resolver = Resolver::new("1", "Runbook.md", CONFLICTED_PAGE).unwrap();
        resolver.choose_all(Choice::Theirs);
        assert_eq!(resolver.unresolved(), 0);
    }

    #[test]
    fn empty_sides_are_still_shown() {
        assert_eq!(side_lines(""), ["(empty)"]);
        assert_eq!(side_lines("a\nb\n"), ["a", "b"]);
    }
}
