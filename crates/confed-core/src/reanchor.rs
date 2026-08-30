//! Re-anchoring inline comments after the page text has been edited.
//!
//! confed stores inline comments in a sidecar rather than as invisible markers
//! in the body (see docs/design/03-conversion-and-conflicts.md §4), which means
//! an anchor has to be *found* again each time the body changes. The rules, in
//! order, are deliberately conservative: it is far better to mark a comment
//! orphaned than to attach it to the wrong sentence.
//!
//! 1. The anchor text with its original surrounding context, matched exactly.
//! 2. The anchor text alone, if it occurs exactly once.
//! 3. The anchor text occurring several times — pick the occurrence whose
//!    context matches best, and only if one clearly wins.
//! 4. A fuzzy window match above a high similarity threshold.
//! 5. Otherwise: orphaned. Never guessed at, and never deleted server-side.

use confed_api::InlineAnchor;

/// How much of the surrounding text is kept as context.
const CONTEXT_CHARS: usize = 32;

/// Minimum similarity for a fuzzy match. High on purpose: a wrong anchor is
/// worse than an orphaned one.
const FUZZY_THRESHOLD: f64 = 0.75;

/// A fuzzy match only wins if it beats the runner-up by this margin, so
/// repeated boilerplate does not get anchored arbitrarily.
const AMBIGUITY_MARGIN: f64 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    /// Text and context both matched exactly.
    Exact,
    /// The text occurs exactly once.
    Unique,
    /// Several occurrences; context picked the winner.
    ByContext,
    /// Approximate match above the threshold.
    Fuzzy,
    /// Not found; the comment is kept but flagged.
    Orphaned,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reanchored {
    pub anchor: InlineAnchor,
    pub kind: MatchKind,
    /// Byte offset of the anchor text in the body, when it was found.
    pub offset: Option<usize>,
}

impl Reanchored {
    pub fn is_orphaned(&self) -> bool {
        self.kind == MatchKind::Orphaned
    }
}

/// Locate `anchor` in `body` and return it with refreshed context.
pub fn reanchor(anchor: &InlineAnchor, body: &str) -> Reanchored {
    if anchor.text.is_empty() {
        return orphan(anchor);
    }

    // 1. Text plus its recorded context, exactly as it was.
    let with_context = format!("{}{}{}", anchor.context_before, anchor.text, anchor.context_after);
    if !anchor.context_before.is_empty() || !anchor.context_after.is_empty() {
        if let Some(start) = body.find(&with_context) {
            let offset = start + anchor.context_before.len();
            return found(anchor, body, offset, MatchKind::Exact);
        }
    }

    let occurrences: Vec<usize> = body.match_indices(&anchor.text).map(|(i, _)| i).collect();

    // 2. Exactly one occurrence: unambiguous.
    if occurrences.len() == 1 {
        return found(anchor, body, occurrences[0], MatchKind::Unique);
    }

    // 3. Several occurrences: let the surrounding text decide, but only if one
    //    candidate is clearly better than the rest.
    if occurrences.len() > 1 {
        let mut scored: Vec<(usize, f64)> = occurrences
            .iter()
            .map(|&offset| (offset, context_similarity(anchor, body, offset)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));

        let (best_offset, best_score) = scored[0];
        let runner_up = scored[1].1;
        if best_score > runner_up + AMBIGUITY_MARGIN {
            return found(anchor, body, best_offset, MatchKind::ByContext);
        }
        // Equally plausible everywhere — refuse to guess.
        return orphan(anchor);
    }

    // 4. The text itself changed: look for something close enough.
    if let Some((offset, score)) = best_fuzzy_window(&anchor.text, body) {
        if score >= FUZZY_THRESHOLD {
            return found(anchor, body, offset, MatchKind::Fuzzy);
        }
    }

    orphan(anchor)
}

/// The anchor for the span `body[start..end]`, with fresh context — what a
/// mark found in the file says about where its comment sits.
pub fn anchor_at(body: &str, start: usize, end: usize, marker_ref: Option<String>) -> InlineAnchor {
    InlineAnchor {
        text: body[start..end].to_string(),
        context_before: take_before(body, start),
        context_after: take_after(body, end),
        marker_ref,
        orphaned: false,
        ..Default::default()
    }
}

/// Re-anchor a batch, reporting how many ended up orphaned.
pub fn reanchor_all(anchors: &[InlineAnchor], body: &str) -> Vec<Reanchored> {
    anchors.iter().map(|a| reanchor(a, body)).collect()
}

fn found(anchor: &InlineAnchor, body: &str, offset: usize, kind: MatchKind) -> Reanchored {
    let end = offset + anchor.text.len();
    Reanchored {
        anchor: InlineAnchor {
            text: anchor.text.clone(),
            context_before: take_before(body, offset),
            context_after: take_after(body, end),
            marker_ref: anchor.marker_ref.clone(),
            orphaned: false,
            ..Default::default()
        },
        kind,
        offset: Some(offset),
    }
}

fn orphan(anchor: &InlineAnchor) -> Reanchored {
    Reanchored {
        anchor: InlineAnchor { orphaned: true, ..anchor.clone() },
        kind: MatchKind::Orphaned,
        offset: None,
    }
}

/// How well the text around `offset` matches the anchor's recorded context.
fn context_similarity(anchor: &InlineAnchor, body: &str, offset: usize) -> f64 {
    let before = take_before(body, offset);
    let after = take_after(body, offset + anchor.text.len());
    (similarity(&anchor.context_before, &before) + similarity(&anchor.context_after, &after)) / 2.0
}

/// Best approximate match for `needle`, scanning windows of the body.
///
/// Several window widths are tried, because the common edit is a few words
/// inserted into or removed from the anchored span: a window fixed at the
/// original length can never score well against "first week *onboarding*
/// checklist". Windows start at character boundaries and are stepped coarsely —
/// this is a best-effort fallback, not a search engine.
fn best_fuzzy_window(needle: &str, body: &str) -> Option<(usize, f64)> {
    if body.is_empty() {
        return None;
    }
    let needle_len = needle.chars().count();
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    if chars.is_empty() || chars.len() * 2 < needle_len {
        return None;
    }

    let widths: Vec<usize> = [needle_len * 3 / 4, needle_len, needle_len * 3 / 2, needle_len * 2]
        .into_iter()
        .map(|w| w.max(1))
        .collect();

    // Candidate starts: a coarse sweep, plus every place the anchor's opening
    // words still appear. Alignment matters more than density — a window that
    // begins a few characters early carries junk that drags the score below the
    // threshold even when the rest matches well.
    let step = (needle_len / 4).max(1);
    let mut starts: Vec<usize> = (0..chars.len()).step_by(step).collect();
    for lead in leading_words(needle) {
        starts.extend(body.match_indices(&lead).map(|(i, _)| char_index(&chars, i)));
    }
    starts.sort_unstable();
    starts.dedup();

    let mut best: Option<(usize, f64)> = None;
    for start in starts {
        if start >= chars.len() {
            continue;
        }
        let byte_start = chars[start].0;
        for &width in &widths {
            let end_index = (start + width).min(chars.len());
            let byte_end = if end_index < chars.len() { chars[end_index].0 } else { body.len() };

            let score = similarity(needle, &body[byte_start..byte_end]);
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((byte_start, score));
            }
        }
    }
    best
}

/// The first word and first two words of the anchor, when they are long enough
/// to be worth searching for.
fn leading_words(needle: &str) -> Vec<String> {
    let words: Vec<&str> = needle.split_whitespace().collect();
    let mut out = Vec::new();
    if let Some(first) = words.first().filter(|w| w.chars().count() >= 3) {
        out.push((*first).to_string());
        if words.len() > 1 {
            out.push(format!("{first} {}", words[1]));
        }
    }
    out
}

fn char_index(chars: &[(usize, char)], byte_offset: usize) -> usize {
    chars.partition_point(|(i, _)| *i < byte_offset)
}

/// Character-level similarity in `[0, 1]`.
fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    similar::TextDiff::from_chars(a, b).ratio() as f64
}

fn take_before(body: &str, offset: usize) -> String {
    let start = floor_boundary(body, offset.saturating_sub(CONTEXT_CHARS * 4));
    let slice = &body[start..offset.min(body.len())];
    slice.chars().rev().take(CONTEXT_CHARS).collect::<Vec<_>>().into_iter().rev().collect()
}

fn take_after(body: &str, offset: usize) -> String {
    if offset >= body.len() {
        return String::new();
    }
    let start = ceil_boundary(body, offset);
    body[start..].chars().take(CONTEXT_CHARS).collect()
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(text: &str, before: &str, after: &str) -> InlineAnchor {
        InlineAnchor {
            text: text.into(),
            context_before: before.into(),
            context_after: after.into(),
            marker_ref: Some("m1".into()),
            orphaned: false,
            ..Default::default()
        }
    }

    #[test]
    fn an_untouched_body_matches_exactly() {
        let body = "During your first week checklist you will meet the team.";
        let result = reanchor(&anchor("first week checklist", "During your ", " you will"), body);

        assert_eq!(result.kind, MatchKind::Exact);
        assert!(!result.is_orphaned());
        assert_eq!(result.offset, Some(body.find("first week").unwrap()));
        assert_eq!(result.anchor.marker_ref.as_deref(), Some("m1"));
    }

    #[test]
    fn a_moved_paragraph_still_matches_on_unique_text() {
        let body =
            "A new opening paragraph.\n\nCompletely different lead-in: first week checklist.";
        let result = reanchor(&anchor("first week checklist", "During your ", " you will"), body);

        assert_eq!(result.kind, MatchKind::Unique);
        assert!(!result.is_orphaned());
        // The context is refreshed so the next edit has better information.
        assert!(result.anchor.context_before.contains("lead-in"));
    }

    #[test]
    fn deleted_anchor_text_is_orphaned_not_guessed() {
        let body = "The whole section about onboarding is gone now.";
        let result = reanchor(&anchor("quarterly revenue projections", "See the ", " below"), body);

        assert_eq!(result.kind, MatchKind::Orphaned);
        assert!(result.anchor.orphaned);
        assert_eq!(result.offset, None);
        // The original anchor text is preserved so a human can still find it.
        assert_eq!(result.anchor.text, "quarterly revenue projections");
    }

    #[test]
    fn repeated_text_with_intact_context_matches_exactly() {
        let body = "Step one: click save.\n\nLater on, in the admin panel: click save.";
        let result = reanchor(&anchor("click save", "in the admin panel: ", ""), body);

        assert_eq!(result.kind, MatchKind::Exact);
        assert!(result.offset.unwrap() > body.find("Step one").unwrap(), "the second one");
    }

    #[test]
    fn repeated_text_is_disambiguated_by_similar_context() {
        // The context drifted (a comma became a colon), so the exact path fails
        // and the nearby wording has to decide which occurrence is meant.
        let body = "Step one: click save.\n\nLater on, in the admin panel: click save.";
        let result = reanchor(&anchor("click save", "in the admin panel, ", ""), body);

        assert_eq!(result.kind, MatchKind::ByContext);
        assert!(result.offset.unwrap() > body.find("Step one").unwrap());
        assert!(result.anchor.context_before.contains("admin panel"));
    }

    #[test]
    fn repeated_text_with_no_distinguishing_context_is_orphaned() {
        let body = "click save\nclick save\nclick save";
        let result = reanchor(&anchor("click save", "", ""), body);

        assert_eq!(result.kind, MatchKind::Orphaned, "an arbitrary pick would be worse");
    }

    #[test]
    fn an_edit_inside_the_anchor_still_matches_fuzzily() {
        let body = "Please review the first week onboarding checklist before Monday.";
        let result = reanchor(&anchor("first week checklist", "review the ", " before"), body);

        assert_eq!(result.kind, MatchKind::Fuzzy, "close enough to re-attach");
        assert!(!result.is_orphaned());
    }

    #[test]
    fn a_wholly_different_body_does_not_produce_a_fuzzy_match() {
        let body = "Quarterly financial results and the associated board commentary.";
        let result = reanchor(&anchor("first week checklist", "During your ", ""), body);
        assert_eq!(result.kind, MatchKind::Orphaned);
    }

    #[test]
    fn unicode_bodies_never_slice_on_a_bad_boundary() {
        let body = "Añadir la lista de verificación de la primera semana — y después revisarla.";
        let result = reanchor(&anchor("lista de verificación", "Añadir la ", " de la"), body);

        assert!(!result.is_orphaned());
        assert!(body.is_char_boundary(result.offset.unwrap()));
        // Refreshed context must itself be valid UTF-8 slices of the body.
        assert!(!result.anchor.context_before.is_empty());
    }

    #[test]
    fn an_empty_anchor_is_orphaned_rather_than_matching_everything() {
        let result = reanchor(&anchor("", "", ""), "any body at all");
        assert_eq!(result.kind, MatchKind::Orphaned);
    }

    #[test]
    fn an_empty_body_orphans_every_anchor() {
        let results = reanchor_all(&[anchor("something", "", "")], "");
        assert!(results[0].is_orphaned());
    }

    #[test]
    fn context_is_capped_so_the_sidecar_stays_readable() {
        let body = format!("{} TARGET {}", "x".repeat(500), "y".repeat(500));
        let result = reanchor(&anchor("TARGET", "", ""), &body);

        assert!(result.anchor.context_before.chars().count() <= CONTEXT_CHARS);
        assert!(result.anchor.context_after.chars().count() <= CONTEXT_CHARS);
    }

    #[test]
    fn re_anchoring_twice_is_stable() {
        let body = "During your first week checklist you will meet the team.";
        let once = reanchor(&anchor("first week checklist", "During your ", " you will"), body);
        let twice = reanchor(&once.anchor, body);

        assert_eq!(once.anchor, twice.anchor, "a settled anchor must not drift");
        assert_eq!(twice.kind, MatchKind::Exact);
    }
}
