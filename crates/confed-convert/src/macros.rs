//! What confed knows how to say in Markdown, and what it refuses to.
//!
//! The rule that keeps the converter honest: if an element (or any of its
//! descendants, or any of its macro parameters) is not in the tables below, the
//! whole top-level block falls back to a verbatim `confluence` fence. It is
//! better to show a user raw storage format than to silently drop a macro
//! parameter they were relying on.

use crate::dom::{Element, Node};

/// The `ac:name` of a structured macro, if this element is one.
pub fn macro_name(el: &Element) -> Option<&str> {
    if el.name == "ac:structured-macro" || el.local() == "structured-macro" {
        el.attr_local("name")
    } else {
        None
    }
}

/// Collect `<ac:parameter ac:name="…">value</ac:parameter>` children.
pub fn macro_params(el: &Element) -> Vec<(String, String)> {
    el.child_elements()
        .filter(|c| c.local() == "parameter")
        .map(|c| (c.attr_local("name").unwrap_or_default().to_string(), c.text()))
        .collect()
}

pub fn macro_param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// A parameter we can ignore without losing information: either it is empty or
/// it carries a Confluence default that changes nothing about the content.
pub fn param_is_inert(value: &str) -> bool {
    let v = value.trim();
    v.is_empty() || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("none")
}

pub fn plain_text_body(el: &Element) -> Option<String> {
    el.child_elements()
        .find(|c| c.local() == "plain-text-body")
        .map(|c| c.text())
}

pub fn rich_text_body(el: &Element) -> Option<&Element> {
    el.child_elements().find(|c| c.local() == "rich-text-body")
}

/// Confluence admonition macro → GitHub alert keyword.
///
/// Confluence has five severities and GitHub has five keywords, but they do not
/// line up: `info` and `note` both read as "here is some context", so both map
/// to `NOTE`. `panel` is a generic coloured box with no severity at all, so it
/// also becomes `NOTE`. This is intentionally lossy in one direction and is why
/// `alert_to_admonition` cannot be a perfect inverse.
pub fn admonition_to_alert(name: &str) -> Option<&'static str> {
    match name {
        "info" | "note" => Some("NOTE"),
        "tip" => Some("TIP"),
        "warning" => Some("WARNING"),
        // A panel is a box, not a severity.
        "panel" => Some("NOTE"),
        _ => None,
    }
}

/// GitHub alert keyword → Confluence macro name. `NOTE` resolves to `info`
/// (by far the most common admonition in real spaces) and `IMPORTANT` /
/// `CAUTION` have no Confluence equivalent, so they borrow the nearest one.
pub fn alert_to_admonition(alert: &str) -> &'static str {
    match alert.to_ascii_uppercase().as_str() {
        "TIP" => "tip",
        "WARNING" | "CAUTION" => "warning",
        "IMPORTANT" => "note",
        _ => "info",
    }
}

pub fn is_admonition(name: &str) -> bool {
    matches!(name, "info" | "note" | "warning" | "tip" | "panel")
}

/// Marker left in the Markdown where a `toc` macro was, so the macro can be put
/// back on push instead of quietly disappearing.
pub const TOC_MARKER: &str = "<!-- confed:toc -->";

/// `ac:emoticon` names → the character a human would have typed.
const EMOTICONS: &[(&str, &str)] = &[
    ("smile", "\u{1f642}"),
    ("sad", "\u{1f641}"),
    ("cheeky", "\u{1f61c}"),
    ("laugh", "\u{1f604}"),
    ("wink", "\u{1f609}"),
    ("thumbs-up", "\u{1f44d}"),
    ("thumbs-down", "\u{1f44e}"),
    ("information", "\u{2139}\u{fe0f}"),
    ("tick", "\u{2705}"),
    ("cross", "\u{274c}"),
    ("warning", "\u{26a0}\u{fe0f}"),
    ("plus", "\u{2795}"),
    ("minus", "\u{2796}"),
    ("question", "\u{2753}"),
    ("light-on", "\u{1f4a1}"),
    ("light-off", "\u{1f50c}"),
    ("yellow-star", "\u{2b50}"),
    ("red-star", "\u{2764}\u{fe0f}"),
    ("green-star", "\u{1f49a}"),
    ("blue-star", "\u{1f499}"),
    ("heart", "\u{2764}\u{fe0f}"),
    ("broken-heart", "\u{1f494}"),
];

pub fn emoticon(name: &str) -> Option<&'static str> {
    EMOTICONS.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

/// Attributes on `<ac:image>` that Markdown can express. Anything else (width,
/// height, border, alignment, thumbnail…) sends the block to a preserved fence,
/// as design 03 §2 requires.
fn image_attrs_are_expressible(el: &Element) -> bool {
    el.attrs.iter().all(|(k, v)| {
        let local = k.split_once(':').map(|(_, l)| l).unwrap_or(k);
        matches!(local, "alt" | "title") || param_is_inert(v)
    })
}

/// Plain XHTML elements confed renders directly.
fn is_known_html(local: &str) -> bool {
    matches!(
        local,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "strong"
            | "b"
            | "em"
            | "i"
            | "del"
            | "s"
            | "strike"
            | "u"
            | "ins"
            | "sub"
            | "sup"
            | "code"
            | "tt"
            | "br"
            | "hr"
            | "span"
            | "a"
            | "ul"
            | "ol"
            | "li"
            | "blockquote"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "th"
            | "td"
            | "colgroup"
            | "col"
            | "img"
            | "time"
    )
}

/// `ac:`/`ri:` elements confed renders directly (macros are checked separately).
fn is_known_ac(local: &str) -> bool {
    matches!(
        local,
        "image"
            | "link"
            | "link-body"
            | "plain-text-link-body"
            | "emoticon"
            | "task-list"
            | "task"
            | "task-id"
            | "task-uuid"
            | "task-status"
            | "task-body"
            | "inline-comment-marker"
            | "attachment"
            | "url"
            | "page"
            | "user"
            | "parameter"
            | "plain-text-body"
            | "rich-text-body"
    )
}

/// Can this macro be expressed in Markdown *without losing a parameter*?
pub fn macro_is_renderable(el: &Element, name: &str) -> bool {
    let params = macro_params(el);
    match name {
        // A code fence carries a language and nothing else. `collapse`,
        // `linenumbers`, `theme`… have no Markdown spelling, so a macro that
        // sets one goes to a preserved block rather than losing it.
        "code" | "noformat" => params
            .iter()
            .all(|(k, v)| k == "language" || param_is_inert(v)),
        // Admonitions carry an optional title, rendered as a bold first line.
        n if is_admonition(n) => params
            .iter()
            .all(|(k, v)| k == "title" || param_is_inert(v)),
        "expand" => params
            .iter()
            .all(|(k, v)| k == "title" || param_is_inert(v)),
        "status" => params
            .iter()
            .all(|(k, v)| k == "title" || k == "colour" || k == "color" || param_is_inert(v)),
        // A table of contents is regenerated by the server; the marker comment
        // is enough to put the macro back.
        "toc" => params.iter().all(|(_, v)| param_is_inert(v)),
        _ => false,
    }
}

/// Block-level tags — the things a GFM table cell cannot contain.
fn is_block_tag(el: &Element) -> bool {
    if let Some(name) = macro_name(el) {
        // A status lozenge is inline; every other macro we render is a block.
        return name != "status";
    }
    let prefixed = el.name.contains(':');
    matches!(
        (prefixed, el.local()),
        (
            false,
            "ul" | "ol"
                | "li"
                | "table"
                | "blockquote"
                | "hr"
                | "div"
                | "pre"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
        ) | (true, "task-list")
    )
}

/// A GFM table can express exactly one thing: a rectangle of inline content.
///
/// Anything else — a merged cell, a list inside a cell, two paragraphs in one
/// cell — has no Markdown spelling, so the table goes to a preserved fence
/// whole rather than being silently flattened.
pub fn table_is_simple(table: &Element) -> bool {
    let mut rows = 0usize;
    let mut ok = true;
    walk_table(table, &mut rows, &mut ok);
    ok && rows > 0
}

fn walk_table(el: &Element, rows: &mut usize, ok: &mut bool) {
    for child in el.child_elements() {
        match child.local() {
            "thead" | "tbody" | "tfoot" => walk_table(child, rows, ok),
            "tr" => {
                *rows += 1;
                for cell in child.child_elements() {
                    if !matches!(cell.local(), "td" | "th") {
                        continue;
                    }
                    for attr in ["colspan", "rowspan"] {
                        if let Some(v) = cell.attr_local(attr) {
                            if v.trim() != "1" && !v.trim().is_empty() {
                                *ok = false;
                            }
                        }
                    }
                    if !cell_is_inline(cell) {
                        *ok = false;
                    }
                }
            }
            // A nested table, or anything else structural, is not simple.
            "table" => *ok = false,
            _ => {}
        }
    }
}

/// Confluence wraps cell content in a single `<p>`; that is fine. Two
/// paragraphs, or any other block, is not.
fn cell_is_inline(cell: &Element) -> bool {
    let mut paragraphs = 0;
    for child in cell.child_elements() {
        if child.local() == "p" && !child.name.contains(':') {
            paragraphs += 1;
            if paragraphs > 1 || contains_block(child) {
                return false;
            }
        } else if is_block_tag(child) || contains_block(child) {
            return false;
        }
    }
    true
}

fn contains_block(el: &Element) -> bool {
    el.child_elements()
        .any(|c| is_block_tag(c) || contains_block(c))
}

/// Whether a subtree can be rendered to Markdown with no loss of *structure*.
///
/// Inline styling (a `<span style="color:…">`) is deliberately treated as
/// renderable: design 03 §3 accepts losing it, and only inside blocks the user
/// actually edited. Unknown *macros* are not, because those carry content.
pub fn renderable(el: &Element) -> bool {
    let local = el.local();
    let known = match el.name.split_once(':').map(|(p, _)| p) {
        Some("ac") | Some("ri") => match macro_name(el) {
            Some(name) => macro_is_renderable(el, name),
            None => match local {
                // An emoticon we have no character for would silently vanish.
                "emoticon" => el.attr_local("name").and_then(emoticon).is_some(),
                "image" => image_attrs_are_expressible(el),
                other => is_known_ac(other),
            },
        },
        Some(_) => false,
        None if local == "table" => return table_is_simple(el),
        None => is_known_html(local),
    };
    if !known {
        return false;
    }
    // A code macro's body is CDATA; nothing below it needs checking.
    if matches!(macro_name(el), Some("code") | Some("noformat")) {
        return true;
    }
    el.children.iter().all(|c| match c {
        Node::Element(child) => renderable(child),
        // A stray processing instruction or doctype inside a block is odd
        // enough that preserving the block verbatim is the safe answer.
        Node::Raw(_) => false,
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::parse_fragment;

    fn el(src: &str) -> Element {
        parse_fragment(src).unwrap()[0].as_element().unwrap().clone()
    }

    #[test]
    fn code_macro_language_is_renderable() {
        let e = el(r#"<ac:structured-macro ac:name="code"><ac:parameter ac:name="language">rust</ac:parameter><ac:plain-text-body><![CDATA[fn main(){}]]></ac:plain-text-body></ac:structured-macro>"#);
        assert!(renderable(&e));
        assert_eq!(macro_param(&macro_params(&e), "language"), Some("rust"));
        assert_eq!(plain_text_body(&e).as_deref(), Some("fn main(){}"));
    }

    #[test]
    fn code_macro_with_unmapped_param_is_preserved() {
        let e = el(r#"<ac:structured-macro ac:name="code"><ac:parameter ac:name="collapse">true</ac:parameter><ac:plain-text-body><![CDATA[x]]></ac:plain-text-body></ac:structured-macro>"#);
        assert!(!renderable(&e));
    }

    #[test]
    fn inert_params_do_not_force_preservation() {
        let e = el(r#"<ac:structured-macro ac:name="code"><ac:parameter ac:name="collapse">false</ac:parameter><ac:parameter ac:name="theme"></ac:parameter><ac:plain-text-body><![CDATA[x]]></ac:plain-text-body></ac:structured-macro>"#);
        assert!(renderable(&e));
    }

    #[test]
    fn unknown_macro_is_never_renderable() {
        let e = el(r#"<ac:structured-macro ac:name="jira"><ac:parameter ac:name="key">PROJ-1</ac:parameter></ac:structured-macro>"#);
        assert!(!renderable(&e));
    }

    #[test]
    fn unknown_macro_nested_in_a_paragraph_taints_the_block() {
        let e = el(r#"<p>see <ac:structured-macro ac:name="jira"><ac:parameter ac:name="key">P-1</ac:parameter></ac:structured-macro> please</p>"#);
        assert!(!renderable(&e));
    }

    #[test]
    fn inline_styling_is_accepted_lossiness() {
        let e = el(r#"<p><span style="color: rgb(255,0,0);">red</span></p>"#);
        assert!(renderable(&e));
    }

    #[test]
    fn alert_mapping() {
        assert_eq!(admonition_to_alert("info"), Some("NOTE"));
        assert_eq!(admonition_to_alert("note"), Some("NOTE"));
        assert_eq!(admonition_to_alert("tip"), Some("TIP"));
        assert_eq!(admonition_to_alert("warning"), Some("WARNING"));
        assert_eq!(admonition_to_alert("panel"), Some("NOTE"));
        assert_eq!(admonition_to_alert("jira"), None);

        assert_eq!(alert_to_admonition("NOTE"), "info");
        assert_eq!(alert_to_admonition("TIP"), "tip");
        assert_eq!(alert_to_admonition("WARNING"), "warning");
        assert_eq!(alert_to_admonition("CAUTION"), "warning");
        assert_eq!(alert_to_admonition("IMPORTANT"), "note");
    }

    #[test]
    fn simple_tables_are_recognised() {
        assert!(table_is_simple(&el(
            "<table><tbody><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></tbody></table>"
        )));
        // Confluence wraps every cell in a paragraph; that is still simple.
        assert!(table_is_simple(&el(
            "<table><tbody><tr><td><p>one</p></td><td><p><strong>two</strong></p></td></tr></tbody></table>"
        )));
        // Inline macros, images and links inside a cell are fine.
        assert!(table_is_simple(&el(
            r#"<table><tbody><tr><td><p><a href="x">l</a> <ac:emoticon ac:name="tick"/></p></td></tr></tbody></table>"#
        )));
        // A `colspan="1"` is a no-op, not a merge.
        assert!(table_is_simple(&el(
            r#"<table><tbody><tr><td colspan="1">x</td></tr></tbody></table>"#
        )));
        // thead/tbody wrappers do not change anything.
        assert!(table_is_simple(&el(
            "<table><thead><tr><th>A</th></tr></thead><tbody><tr><td>1</td></tr></tbody></table>"
        )));
    }

    #[test]
    fn complex_tables_are_rejected() {
        for src in [
            // merged cells
            r#"<table><tbody><tr><td colspan="2">wide</td></tr></tbody></table>"#,
            r#"<table><tbody><tr><td rowspan="3">tall</td></tr></tbody></table>"#,
            // a list in a cell
            "<table><tbody><tr><td><ul><li>a</li></ul></td></tr></tbody></table>",
            // a list nested inside the cell's paragraph
            "<table><tbody><tr><td><p><ul><li>a</li></ul></p></td></tr></tbody></table>",
            // two paragraphs in one cell
            "<table><tbody><tr><td><p>one</p><p>two</p></td></tr></tbody></table>",
            // a block macro in a cell
            r#"<table><tbody><tr><td><ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[x]]></ac:plain-text-body></ac:structured-macro></td></tr></tbody></table>"#,
            // a nested table
            "<table><tbody><tr><td><table><tbody><tr><td>x</td></tr></tbody></table></td></tr></tbody></table>",
            // no rows at all
            "<table><tbody></tbody></table>",
        ] {
            assert!(!table_is_simple(&el(src)), "should be complex: {src}");
        }
    }

    #[test]
    fn a_status_lozenge_in_a_cell_keeps_the_table_simple() {
        assert!(table_is_simple(&el(
            r#"<table><tbody><tr><td><p><ac:structured-macro ac:name="status"><ac:parameter ac:name="title">OK</ac:parameter></ac:structured-macro></p></td></tr></tbody></table>"#
        )));
    }

    #[test]
    fn emoticons_map_to_unicode() {
        assert_eq!(emoticon("smile"), Some("\u{1f642}"));
        assert_eq!(emoticon("thumbs-up"), Some("\u{1f44d}"));
        assert_eq!(emoticon("no-such-emoticon"), None);
    }
}
