//! Where an inline comment's text sits, the way Confluence counts it.
//!
//! Creating an inline comment names a selection by its text plus which
//! occurrence is meant: `textSelection`/`textSelectionMatchIndex` on Cloud,
//! `originalSelection`/`matchIndex`/`numMatches` on Data Center. The server
//! checks those against its own text extraction of the page and refuses a
//! mismatch (412, "The text selection is wrong"), so the count must come from
//! the page's storage, not from the Markdown, where escaping, mentions and
//! macros read differently.
//!
//! The extraction keeps what a reader can select: text nodes, entities
//! decoded. Macro parameters, code bodies and other CDATA are excluded —
//! Confluence cannot put a marker inside them. Block boundaries separate the
//! text so a match never spans two paragraphs.

use crate::dom::{self, Element, Node};
use crate::error::ConvertResult;

/// Elements whose text is not page text a selection can cover.
const OPAQUE: &[&str] = &[
    "parameter",
    "plain-text-body",
    "plain-text-link-body",
    "placeholder",
    "emoticon",
    "image",
    "task-id",
    "task-status",
    "adf-attribute",
];

/// Elements that end a run of text.
const BLOCKS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "td",
    "th",
    "tr",
    "pre",
    "blockquote",
    "div",
    "table",
    "ul",
    "ol",
    "br",
    "hr",
    "rich-text-body",
    "structured-macro",
    "task",
    "task-body",
    "layout-cell",
    "layout-section",
];

/// Character that separates blocks in the extracted text. It cannot occur in a
/// selection, so no match crosses it.
const SEPARATOR: char = '\n';

/// The selectable text of a page, plus whether it has any text inside macro
/// parameters or code bodies (for a better "not found" message).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageText {
    pub text: String,
    pub hidden: String,
}

pub fn page_text(storage: &str) -> ConvertResult<PageText> {
    let nodes = dom::parse_fragment(storage)?;
    let mut out = PageText::default();
    walk(&nodes, false, &mut out);
    Ok(out)
}

fn walk(nodes: &[Node], hidden: bool, out: &mut PageText) {
    for node in nodes {
        match node {
            Node::Text(c) => {
                // The converter reads `&nbsp;` as a plain space; the server's text
                // keeps U+00A0, and the selection has to match it.
                let text = dom::unescape(&c.raw.replace("&nbsp;", "\u{a0}"));
                if hidden {
                    out.hidden.push_str(&text);
                } else {
                    // Newlines in storage are source formatting, not text.
                    out.text.extend(text.chars().map(|ch| if ch == '\n' { ' ' } else { ch }));
                }
            }
            Node::CData(c) => {
                out.hidden.push_str(&c.raw);
                out.hidden.push(SEPARATOR);
            }
            Node::Element(el) => element(el, hidden, out),
            _ => {}
        }
    }
}

fn element(el: &Element, hidden: bool, out: &mut PageText) {
    let local = el.local();
    let block = BLOCKS.contains(&local);
    if block {
        out.text.push(SEPARATOR);
    }
    walk(&el.children, hidden || OPAQUE.contains(&local), out);
    if block {
        out.text.push(SEPARATOR);
    }
}

/// A selection as the server wants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// The page's own text for the chosen occurrence — with any non-breaking
    /// spaces the page has, even if the caller typed plain spaces.
    pub text: String,
    /// 0-based index among occurrences of exactly `text`.
    pub match_index: usize,
    /// How many times exactly `text` occurs in the page text.
    pub match_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionError {
    /// The text is not page text. `in_macro` when it occurs only inside macro
    /// parameters or code, where Confluence cannot anchor a comment.
    NotFound { in_macro: bool },
    /// More than one occurrence and no pick; carries a short context for each.
    Ambiguous { contexts: Vec<String> },
    /// `--occurrence` outside 1..=count.
    OutOfRange { count: usize },
    /// The text spans a block boundary or is empty.
    Invalid,
}

/// Find `needle` in a page's storage and describe it as a selection.
///
/// `occurrence` is 1-based, counted over the page's text with spaces and
/// non-breaking spaces treated alike.
pub fn select(
    storage: &str,
    needle: &str,
    occurrence: Option<usize>,
) -> ConvertResult<Result<Selection, SelectionError>> {
    let page = page_text(storage)?;
    Ok(select_in(&page, needle, occurrence))
}

pub fn select_in(
    page: &PageText,
    needle: &str,
    occurrence: Option<usize>,
) -> Result<Selection, SelectionError> {
    let needle = needle.trim();
    if needle.is_empty() || needle.contains(SEPARATOR) {
        return Err(SelectionError::Invalid);
    }
    let found = find_loose(&page.text, needle);
    if found.is_empty() {
        let in_macro = !find_loose(&page.hidden, needle).is_empty();
        return Err(SelectionError::NotFound { in_macro });
    }
    let pick = match occurrence {
        Some(n) if n >= 1 && n <= found.len() => n - 1,
        Some(_) => return Err(SelectionError::OutOfRange { count: found.len() }),
        None if found.len() == 1 => 0,
        None => {
            let contexts = found.iter().map(|&(s, e)| context(&page.text, s, e)).collect();
            return Err(SelectionError::Ambiguous { contexts });
        }
    };
    let (start, end) = found[pick];
    let text = page.text[start..end].to_string();
    // The server counts its own string, so count exactly what is sent.
    let exact: Vec<usize> = page.text.match_indices(&text).map(|(i, _)| i).collect();
    let match_index = exact.iter().position(|&i| i == start).unwrap_or(0);
    Ok(Selection { text, match_index, match_count: exact.len() })
}

/// Non-overlapping byte ranges of `needle` in `hay`, with any space matching
/// a space or a non-breaking space.
fn find_loose(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let fold = |c: char| if c == '\u{a0}' { ' ' } else { c };
    let needle: Vec<char> = needle.chars().map(fold).collect();
    let chars: Vec<(usize, char)> = hay.char_indices().map(|(i, c)| (i, fold(c))).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()].iter().map(|&(_, c)| c).eq(needle.iter().copied()) {
            let start = chars[i].0;
            let end = chars.get(i + needle.len()).map_or(hay.len(), |&(b, _)| b);
            out.push((start, end));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

fn context(text: &str, start: usize, end: usize) -> String {
    let before: String =
        text[..start].chars().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
    let after: String = text[end..].chars().take(20).collect();
    let clean = |s: &str| s.replace(SEPARATOR, " ").trim().to_string();
    format!("…{} [{}] {}…", clean(&before), &text[start..end], clean(&after))
}

/// Every inline-comment marker in storage, as `(ref, text)`, fragments of
/// one ref joined, in order of first appearance.
pub fn marker_refs(storage: &str) -> ConvertResult<Vec<(String, String)>> {
    let nodes = dom::parse_fragment(storage)?;
    let mut out: Vec<(String, String)> = Vec::new();
    fn visit(nodes: &[Node], out: &mut Vec<(String, String)>) {
        for node in nodes {
            let Node::Element(el) = node else { continue };
            if el.local() == "inline-comment-marker" {
                if let Some(r) = el.attr_local("ref") {
                    let text = el.text();
                    match out.iter_mut().find(|(known, _)| known == r) {
                        Some((_, joined)) => joined.push_str(&text),
                        None => out.push((r.to_string(), text)),
                    }
                }
            }
            visit(&el.children, out);
        }
    }
    visit(&nodes, &mut out);
    Ok(out)
}

/// Remove one comment's `<ac:inline-comment-marker>` wrappers from storage,
/// keeping what they enclose. Used to check that the only change the server
/// made when a comment was created was wrapping its selection.
pub fn strip_marker(storage: &str, marker_ref: &str) -> ConvertResult<String> {
    const CLOSE: &str = "</ac:inline-comment-marker>";
    let nodes = dom::parse_fragment(storage)?;
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    fn visit(nodes: &[Node], src: &str, marker_ref: &str, cuts: &mut Vec<(usize, usize)>) {
        for node in nodes {
            let Node::Element(el) = node else { continue };
            if el.local() == "inline-comment-marker" && el.attr_local("ref") == Some(marker_ref) {
                let (start, end) = el.span;
                let open_end = src[start..end].find('>').map_or(end, |i| start + i + 1);
                if src[start..open_end].ends_with("/>") {
                    cuts.push((start, end));
                } else {
                    cuts.push((start, open_end));
                    if src[..end].ends_with(CLOSE) {
                        cuts.push((end - CLOSE.len(), end));
                    }
                }
            }
            visit(&el.children, src, marker_ref, cuts);
        }
    }
    visit(&nodes, storage, marker_ref, &mut cuts);
    cuts.sort_unstable();
    let mut out = String::with_capacity(storage.len());
    let mut cursor = 0;
    for (start, end) in cuts {
        out.push_str(&storage[cursor..start]);
        cursor = end;
    }
    out.push_str(&storage[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(storage: &str, needle: &str, occ: Option<usize>) -> Result<Selection, SelectionError> {
        select(storage, needle, occ).unwrap()
    }

    #[test]
    fn a_unique_phrase_is_index_zero_of_one() {
        let s = sel("<p>Фраза 1.</p><p>Фраза 2.</p>", "Фраза 2.", None).unwrap();
        assert_eq!(s, Selection { text: "Фраза 2.".into(), match_index: 0, match_count: 1 });
    }

    #[test]
    fn a_non_breaking_space_in_the_page_is_matched_by_a_space_and_sent_as_is() {
        let s = sel("<ol><li><p>1.&nbsp;Фраза 2.</p></li></ol>", "1. Фраза", None).unwrap();
        assert_eq!(s.text, "1.\u{a0}Фраза");
        assert_eq!((s.match_index, s.match_count), (0, 1));
        let s = sel("<p>a\u{a0}b</p>", "a b", None).unwrap();
        assert_eq!(s.text, "a\u{a0}b");
    }

    #[test]
    fn the_count_is_over_the_exact_text_sent() {
        // One plain-space and two nbsp occurrences: picking an nbsp one counts
        // only nbsp ones, as the server would.
        let storage = "<p>a b</p><p>a&nbsp;b</p><p>a&nbsp;b</p>";
        let s = sel(storage, "a b", Some(3)).unwrap();
        assert_eq!(s, Selection { text: "a\u{a0}b".into(), match_index: 1, match_count: 2 });
        let s = sel(storage, "a b", Some(1)).unwrap();
        assert_eq!(s, Selection { text: "a b".into(), match_index: 0, match_count: 1 });
    }

    #[test]
    fn text_across_inline_formatting_is_one_run() {
        let s = sel("<p>see <strong>bold</strong> text</p>", "see bold text", None).unwrap();
        assert_eq!(s.match_count, 1);
        let s = sel("<p>run <code>confed</code> now</p>", "run confed", None).unwrap();
        assert_eq!(s.text, "run confed");
    }

    #[test]
    fn existing_markers_do_not_split_the_text() {
        let storage =
            "<p>a <ac:inline-comment-marker ac:ref=\"x\">b c</ac:inline-comment-marker> d</p>";
        assert!(sel(storage, "a b c d", None).is_ok());
    }

    #[test]
    fn table_cells_are_text_but_never_joined() {
        let storage = "<table><tbody><tr><td>left</td><td>right</td></tr></tbody></table>";
        assert!(sel(storage, "left", None).is_ok());
        assert_eq!(
            sel(storage, "leftright", None),
            Err(SelectionError::NotFound { in_macro: false })
        );
    }

    #[test]
    fn paragraphs_are_never_joined() {
        assert_eq!(
            sel("<p>one</p><p>two</p>", "one two", None),
            Err(SelectionError::NotFound { in_macro: false })
        );
        assert_eq!(sel("<p>one</p><p>two</p>", "one\ntwo", None), Err(SelectionError::Invalid));
    }

    #[test]
    fn macro_parameters_and_code_are_not_selectable() {
        let storage = "<ac:structured-macro ac:name=\"code\"><ac:parameter ac:name=\"title\">Setup</ac:parameter><ac:plain-text-body><![CDATA[make all]]></ac:plain-text-body></ac:structured-macro>";
        assert_eq!(sel(storage, "Setup", None), Err(SelectionError::NotFound { in_macro: true }));
        assert_eq!(
            sel(storage, "make all", None),
            Err(SelectionError::NotFound { in_macro: true })
        );
    }

    #[test]
    fn rich_text_inside_a_macro_is_page_text() {
        let storage = "<ac:structured-macro ac:name=\"info\"><ac:rich-text-body><p>Read this.</p></ac:rich-text-body></ac:structured-macro>";
        assert!(sel(storage, "Read this.", None).is_ok());
    }

    #[test]
    fn repeated_text_needs_an_occurrence() {
        let storage = "<p>team one</p><p>team two</p>";
        match sel(storage, "team", None) {
            Err(SelectionError::Ambiguous { contexts }) => {
                assert_eq!(contexts.len(), 2);
                assert!(contexts[1].contains("[team] two"), "{contexts:?}");
            }
            other => panic!("{other:?}"),
        }
        let s = sel(storage, "team", Some(2)).unwrap();
        assert_eq!((s.match_index, s.match_count), (1, 2));
        assert_eq!(sel(storage, "team", Some(3)), Err(SelectionError::OutOfRange { count: 2 }));
    }

    #[test]
    fn entities_are_decoded() {
        let s = sel("<p>R&amp;D &mdash; ok</p>", "R&D — ok", None).unwrap();
        assert_eq!(s.text, "R&D — ok");
    }

    #[test]
    fn markers_are_listed_with_their_text() {
        let storage = "<p><ac:inline-comment-marker ac:ref=\"a\">Фраза</ac:inline-comment-marker> 3.<strong><ac:inline-comment-marker ac:ref=\"a\">!</ac:inline-comment-marker></strong> <ac:inline-comment-marker ac:ref=\"b\">x</ac:inline-comment-marker></p>";
        assert_eq!(
            marker_refs(storage).unwrap(),
            vec![("a".to_string(), "Фраза!".to_string()), ("b".to_string(), "x".to_string())]
        );
    }

    #[test]
    fn stripping_a_marker_restores_the_storage() {
        let before = "<p>a <strong>b</strong> c</p>";
        let after = "<p>a <ac:inline-comment-marker ac:ref=\"r1\">(</ac:inline-comment-marker><strong><ac:inline-comment-marker ac:ref=\"r1\">b</ac:inline-comment-marker></strong> c</p>";
        assert_eq!(strip_marker(after, "r1").unwrap(), before.replace("a <", "a (<"));
        let other = "<p><ac:inline-comment-marker ac:ref=\"keep\">x</ac:inline-comment-marker></p>";
        assert_eq!(strip_marker(other, "r1").unwrap(), other);
        assert_eq!(
            strip_marker("<p>x<ac:inline-comment-marker ac:ref=\"r1\"/>y</p>", "r1").unwrap(),
            "<p>xy</p>"
        );
    }
}
