//! Pretty-printing for storage format.
//!
//! Confluence hands out a page body as one enormous line. That is unreadable in
//! a `storage.xml` sidecar and worse inside a ```` ```confluence ```` fence,
//! where somebody is expected to *edit* it. This module lays it out — and it
//! does so under a rule strict enough that the result is still the same
//! document:
//!
//! > A level is re-spaced only when its parent is a **block container**, every
//! > element on it is **block-level**, and nothing else on it is anything but
//! > whitespace or a comment. Any other level is emitted byte for byte.
//!
//! Both halves matter. The parent test keeps the printer out of `ac:link` and
//! `ac:image`, whose children are not laid out by an XHTML renderer at all. The
//! child test keeps it out of `<td><code>a</code><code>b</code></td>`, where a
//! newline between two inline elements would become a rendered space — the same
//! reason a paragraph, a heading, an `ac:parameter` and a CDATA body are never
//! touched. What is left is whitespace between block boxes, which every renderer
//! throws away.
//!
//! [`minify`] is that rule run the other way — it deletes exactly the whitespace
//! [`format`] is allowed to insert. The pair defines the equivalence class the
//! printer moves within, and `minify(format(x)) == minify(x)` is the invariant
//! the tests hold it to.

use crate::dom::{self, Element, Node};

const INDENT: &str = "  ";

/// Elements that may hold their children on separate lines.
///
/// Names are matched with their prefix: `ac:link` is a container that happens to
/// be spelled like the inline HTML `link`, and confusing the two is exactly the
/// mistake this module cannot afford. Notably absent: `p`, the headings,
/// `ac:link`, `ac:image` and the `ac:adf-*` family — inline content, or content
/// no XHTML renderer lays out.
const BLOCK_CONTAINERS: &[&str] = &[
    // XHTML
    "blockquote",
    "colgroup",
    "dd",
    "details",
    "div",
    "dl",
    "li",
    "ol",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
    // Confluence
    "ac:layout",
    "ac:layout-cell",
    "ac:layout-section",
    "ac:rich-text-body",
    "ac:structured-macro",
    "ac:task",
    "ac:task-list",
];

/// Elements that start a box of their own, so a newline before or after one is
/// whitespace no renderer keeps — plus the parts a macro is assembled from,
/// which Confluence reads structurally rather than laying out.
const BLOCK_LEVEL: &[&str] = &[
    // XHTML
    "address",
    "blockquote",
    "caption",
    "col",
    "colgroup",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "li",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
    // Confluence
    "ac:layout",
    "ac:layout-cell",
    "ac:layout-section",
    "ac:parameter",
    "ac:plain-text-body",
    "ac:rich-text-body",
    "ac:task",
    "ac:task-body",
    "ac:task-id",
    "ac:task-list",
    "ac:task-status",
];

fn is_block_container(name: &str) -> bool {
    BLOCK_CONTAINERS.contains(&name)
}

fn is_block_level(el: &Element) -> bool {
    if el.name == "ac:structured-macro" {
        // A macro is block-level unless it renders inline; `status` is the one
        // Confluence puts in the middle of a sentence.
        return crate::macros::macro_name(el) != Some("status");
    }
    BLOCK_LEVEL.contains(&el.name.as_str())
}

/// Lay out a storage fragment across lines, indented two spaces per level.
///
/// Unparseable input is returned unchanged: a printer is not the place to fail.
pub fn format(src: &str) -> String {
    let Ok(nodes) = dom::parse_fragment(src) else {
        return src.to_string();
    };
    if !breakable(&nodes, true) {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len() + src.len() / 8);
    for (i, node) in nodes.iter().filter(|n| !n.is_blank()).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        write_node(node, src, 0, &mut out);
    }
    out
}

/// Delete the whitespace [`format`] is allowed to add, leaving everything else
/// byte for byte.
///
/// Two fragments with the same `minify` differ only in ways no renderer can
/// see, which is how "the printer did not change the document" is stated as a
/// test rather than as a hope.
pub fn minify(src: &str) -> String {
    let Ok(nodes) = dom::parse_fragment(src) else {
        return src.to_string();
    };
    let mut out = String::with_capacity(src.len());
    minify_level(&nodes, src, true, &mut out);
    out
}

/// Whether the printer may put each of these siblings on its own line.
///
/// `container` is whether the level's parent allows it at all; the rest is about
/// the level itself — a block element or a comment can move, an inline element
/// cannot, a run of text cannot, and character data never can.
fn breakable(nodes: &[Node], container: bool) -> bool {
    container
        && nodes.iter().any(|n| matches!(n, Node::Element(_)))
        && nodes.iter().all(|n| match n {
            Node::Element(el) => is_block_level(el),
            Node::Comment(_) => true,
            Node::Text(_) => n.is_blank(),
            _ => false,
        })
}

fn write_node(node: &Node, src: &str, depth: usize, out: &mut String) {
    match node {
        Node::Element(el) if breakable(&el.children, is_block_container(&el.name)) => {
            write_element(el, src, depth, out)
        }
        // Anything else — inline markup, text, CDATA, a container holding a
        // mix of the two — keeps the bytes it came with.
        other => out.push_str(verbatim(src, other.span())),
    }
}

/// Write an element whose children level is breakable: open tag, one child per
/// line indented one level in, close tag back at `depth`.
fn write_element(el: &Element, src: &str, depth: usize, out: &mut String) {
    let (open, close) = tags(el, src);
    out.push_str(open);
    for child in el.children.iter().filter(|n| !n.is_blank()) {
        out.push('\n');
        indent(depth + 1, out);
        write_node(child, src, depth + 1, out);
    }
    // An unclosed element has no close tag to put on a line of its own.
    if !close.is_empty() {
        out.push('\n');
        indent(depth, out);
        out.push_str(close);
    }
}

fn minify_level(nodes: &[Node], src: &str, container: bool, out: &mut String) {
    if !breakable(nodes, container) {
        // Not a level the printer may touch, so neither may this: copy it whole,
        // gaps between siblings included.
        if let (Some(first), Some(last)) = (nodes.first(), nodes.last()) {
            out.push_str(verbatim(src, (first.span().0, last.span().1)));
        }
        return;
    }
    for node in nodes.iter().filter(|n| !n.is_blank()) {
        match node {
            Node::Element(el) => {
                let (open, close) = tags(el, src);
                out.push_str(open);
                minify_level(&el.children, src, is_block_container(&el.name), out);
                out.push_str(close);
            }
            other => out.push_str(verbatim(src, other.span())),
        }
    }
}

/// An element's own bytes, split around its children: `("<p …>", "</p>")`.
/// A childless element is all open tag.
fn tags<'s>(el: &Element, src: &'s str) -> (&'s str, &'s str) {
    match (el.children.first(), el.children.last()) {
        (Some(first), Some(last)) => {
            (verbatim(src, (el.span.0, first.span().0)), verbatim(src, (last.span().1, el.span.1)))
        }
        _ => (verbatim(src, el.span), ""),
    }
}

/// Byte range of a node, clamped so a span the parser recovered past the end of
/// a truncated fragment cannot panic the printer.
fn verbatim(src: &str, (start, end): (usize, usize)) -> &str {
    src.get(start..end).unwrap_or("")
}

fn indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_body_gets_one_block_per_line() {
        assert_eq!(format("<p>a</p><p>b</p>"), "<p>a</p>\n<p>b</p>");
    }

    #[test]
    fn nested_containers_are_indented() {
        assert_eq!(
            format("<table><tbody><tr><td><p>1</p></td></tr></tbody></table>"),
            concat!(
                "<table>\n",
                "  <tbody>\n",
                "    <tr>\n",
                "      <td>\n",
                "        <p>1</p>\n",
                "      </td>\n",
                "    </tr>\n",
                "  </tbody>\n",
                "</table>"
            )
        );
    }

    #[test]
    fn a_macro_and_its_body_lay_out() {
        assert_eq!(
            format(concat!(
                r#"<ac:structured-macro ac:name="info" ac:schema-version="1">"#,
                r#"<ac:parameter ac:name="title">Heads up</ac:parameter>"#,
                "<ac:rich-text-body><p>Read this.</p></ac:rich-text-body>",
                "</ac:structured-macro>"
            )),
            concat!(
                "<ac:structured-macro ac:name=\"info\" ac:schema-version=\"1\">\n",
                "  <ac:parameter ac:name=\"title\">Heads up</ac:parameter>\n",
                "  <ac:rich-text-body>\n",
                "    <p>Read this.</p>\n",
                "  </ac:rich-text-body>\n",
                "</ac:structured-macro>"
            )
        );
    }

    /// The whole safety argument in one test: a newline between two inline
    /// elements is a rendered space, so the printer stays out of every level
    /// that holds one — including inside a `td` or `li`, which may hold inline
    /// content directly even though it is a block container.
    #[test]
    fn inline_content_is_never_re_spaced() {
        for src in [
            "<p><strong>a</strong><em>b</em></p>",
            "<p>text <em>and</em> more</p>",
            "<li>an item <code>x</code></li>",
            "<td><code>a</code><code>b</code></td>",
            "<li><strong>a</strong><em>b</em></li>",
            "<div><em>a</em><em>b</em></div>",
            r#"<p><ac:structured-macro ac:name="status"><ac:parameter ac:name="title">Done</ac:parameter></ac:structured-macro></p>"#,
            r#"<ac:link><ri:page ri:content-title="Runbook" /><ac:plain-text-link-body><![CDATA[the runbook]]></ac:plain-text-link-body></ac:link>"#,
            r#"<ac:image ac:width="300"><ri:attachment ri:filename="c.png"/></ac:image>"#,
        ] {
            assert_eq!(format(src), src, "{src}");
        }
    }

    #[test]
    fn character_data_is_untouched() {
        let src = concat!(
            r#"<ac:structured-macro ac:name="code"><ac:plain-text-body>"#,
            "<![CDATA[fn a() {\n    b()\n}]]>",
            "</ac:plain-text-body></ac:structured-macro>"
        );
        assert_eq!(
            format(src),
            concat!(
                "<ac:structured-macro ac:name=\"code\">\n",
                "  <ac:plain-text-body><![CDATA[fn a() {\n    b()\n}]]></ac:plain-text-body>\n",
                "</ac:structured-macro>"
            )
        );
    }

    #[test]
    fn existing_layout_is_normalised_not_compounded() {
        let src = "<ac:layout>\n<ac:layout-section>\n<ac:layout-cell><p>x</p></ac:layout-cell>\n</ac:layout-section>\n</ac:layout>";
        let once = format(src);
        assert_eq!(
            once,
            concat!(
                "<ac:layout>\n",
                "  <ac:layout-section>\n",
                "    <ac:layout-cell>\n",
                "      <p>x</p>\n",
                "    </ac:layout-cell>\n",
                "  </ac:layout-section>\n",
                "</ac:layout>"
            )
        );
        assert_eq!(format(&once), once, "formatting is a fixed point");
    }

    #[test]
    fn comments_keep_their_own_line() {
        assert_eq!(
            format("<div><!-- why --><p>a</p></div>"),
            "<div>\n  <!-- why -->\n  <p>a</p>\n</div>"
        );
    }

    #[test]
    fn minify_undoes_exactly_what_format_added() {
        for src in [
            "<p>a</p><p>b</p>",
            "<table><tbody><tr><td><p>1</p></td></tr></tbody></table>",
            "<ac:layout>\n<ac:layout-section>\n<ac:layout-cell><p>x</p></ac:layout-cell>\n</ac:layout-section>\n</ac:layout>",
            "<p>text <em>and</em> more</p>",
            "<ul><li>a<ul><li>b</li></ul></li></ul>",
            "<!-- lead --><p>a</p>",
            "",
            "just text",
            "<p>unclosed",
        ] {
            assert_eq!(minify(&format(src)), minify(src), "{src:?}");
        }
    }

    #[test]
    fn minify_leaves_significant_whitespace_alone() {
        assert_eq!(minify("<p>a <em>b</em> c</p>"), "<p>a <em>b</em> c</p>");
        assert_eq!(minify("<p>  padded  </p>"), "<p>  padded  </p>");
    }

    #[test]
    fn degenerate_input_survives() {
        assert_eq!(format(""), "");
        assert_eq!(format("plain text"), "plain text");
        assert_eq!(format("<hr/>"), "<hr/>");
        assert_eq!(format("<p>unclosed"), "<p>unclosed");
        assert_eq!(format("</stray>"), "</stray>");
    }
}
