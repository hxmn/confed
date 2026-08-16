//! Split a storage body into top-level blocks with exact byte spans.
//!
//! "Top-level block" is the unit of the whole system: it is what the block map
//! records, what the push patcher aligns, and what inline comments re-anchor
//! against. A block is one root-level element of the storage fragment (plus any
//! loose text between elements), classified by what it turns into in Markdown.

use crate::blockmap::BlockKind;
use crate::dom::{self, Element, Node};
use crate::error::ConvertResult;
use crate::macros;

/// A parsed storage document: a flat sequence of top-level blocks with the exact
/// byte range each occupies in the source.
#[derive(Clone, Debug, Default)]
pub struct StorageDoc {
    pub blocks: Vec<StorageBlock>,
}

#[derive(Clone, Debug)]
pub struct StorageBlock {
    pub span: (usize, usize),
    pub kind: crate::blockmap::BlockKind,
}

pub fn parse(storage: &str) -> ConvertResult<StorageDoc> {
    let nodes = dom::parse_fragment(storage)?;
    let mut blocks = Vec::new();
    for node in &nodes {
        // Whitespace between blocks belongs to no block. It is recovered by the
        // patcher as the gap between consecutive spans, so it is never lost.
        if node.is_blank() {
            continue;
        }
        let kind = match node {
            Node::Element(el) => classify(el),
            Node::Text(_) | Node::CData(_) => BlockKind::Paragraph,
            // Comments and processing instructions are meaningful to somebody;
            // keep the bytes.
            Node::Comment(_) | Node::Raw(_) => BlockKind::Preserved,
        };
        blocks.push(StorageBlock { span: node.span(), kind });
    }
    Ok(StorageDoc { blocks })
}

/// Decide what a top-level element becomes.
///
/// Two questions, in order: what *shape* is it, and can we render it without
/// losing anything? A shape we recognise but cannot render losslessly still
/// becomes `Preserved` — the fence is always the safe answer.
pub fn classify(el: &Element) -> BlockKind {
    let shape = shape_of(el);
    if shape == BlockKind::Preserved || macros::renderable(el) {
        shape
    } else {
        BlockKind::Preserved
    }
}

fn shape_of(el: &Element) -> BlockKind {
    if let Some(name) = macros::macro_name(el) {
        return match name {
            "code" | "noformat" => BlockKind::Code,
            n if macros::is_admonition(n) => BlockKind::Admonition,
            "expand" => BlockKind::Expand,
            "toc" | "status" => BlockKind::Other,
            _ => BlockKind::Preserved,
        };
    }
    let prefixed = el.name.contains(':');
    match (prefixed, el.local()) {
        (false, "p") => BlockKind::Paragraph,
        (false, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") => BlockKind::Heading,
        (false, "ul" | "ol") => BlockKind::List,
        (false, "table") => BlockKind::Table,
        (false, "blockquote") => BlockKind::Quote,
        (false, "hr") => BlockKind::Rule,
        (true, "task-list") => BlockKind::TaskList,
        (true, "image") => BlockKind::Image,
        _ => BlockKind::Preserved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<BlockKind> {
        parse(src).unwrap().blocks.iter().map(|b| b.kind).collect()
    }

    /// The invariant everything else rests on.
    fn assert_spans_exact(src: &str) {
        let doc = parse(src).unwrap();
        let mut prev_end = 0;
        for b in &doc.blocks {
            assert!(b.span.0 >= prev_end, "overlapping spans in {src:?}");
            assert!(b.span.1 <= src.len());
            assert!(!src[b.span.0..b.span.1].trim().is_empty());
            prev_end = b.span.1;
        }
    }

    #[test]
    fn top_level_shapes() {
        let src = concat!(
            "<p>text</p>",
            "<h2>Heading</h2>",
            "<ul><li>a</li></ul>",
            "<table><tbody><tr><td>x</td></tr></tbody></table>",
            "<blockquote><p>q</p></blockquote>",
            "<hr/>",
        );
        assert_eq!(
            kinds(src),
            vec![
                BlockKind::Paragraph,
                BlockKind::Heading,
                BlockKind::List,
                BlockKind::Table,
                BlockKind::Quote,
                BlockKind::Rule,
            ]
        );
        assert_spans_exact(src);
    }

    #[test]
    fn macro_shapes() {
        let src = concat!(
            r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[x]]></ac:plain-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="warning"><ac:rich-text-body><p>careful</p></ac:rich-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="expand"><ac:rich-text-body><p>more</p></ac:rich-text-body></ac:structured-macro>"#,
            r#"<ac:structured-macro ac:name="jira"><ac:parameter ac:name="key">P-1</ac:parameter></ac:structured-macro>"#,
            r#"<ac:task-list><ac:task><ac:task-status>complete</ac:task-status><ac:task-body>done</ac:task-body></ac:task></ac:task-list>"#,
            r#"<ac:image><ri:attachment ri:filename="a.png"/></ac:image>"#,
            r#"<ac:layout><ac:layout-section><p>x</p></ac:layout-section></ac:layout>"#,
        );
        assert_eq!(
            kinds(src),
            vec![
                BlockKind::Code,
                BlockKind::Admonition,
                BlockKind::Expand,
                BlockKind::Preserved,
                BlockKind::TaskList,
                BlockKind::Image,
                BlockKind::Preserved,
            ]
        );
        assert_spans_exact(src);
    }

    #[test]
    fn spans_are_the_exact_source_bytes() {
        let src = "  <p>one</p>\n  <p>two</p>\n";
        let doc = parse(src).unwrap();
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(&src[doc.blocks[0].span.0..doc.blocks[0].span.1], "<p>one</p>");
        assert_eq!(&src[doc.blocks[1].span.0..doc.blocks[1].span.1], "<p>two</p>");
    }

    #[test]
    fn whitespace_between_blocks_is_not_a_block() {
        assert_eq!(kinds("<p>a</p>\n\n   \n<p>b</p>"), vec![BlockKind::Paragraph; 2]);
    }

    #[test]
    fn loose_top_level_text_is_a_paragraph() {
        assert_eq!(kinds("bare text<p>a</p>"), vec![BlockKind::Paragraph; 2]);
    }

    #[test]
    fn unrenderable_content_downgrades_the_block_to_preserved() {
        let src = r#"<p>see <ac:structured-macro ac:name="drawio"><ac:parameter ac:name="diagramName">x</ac:parameter></ac:structured-macro></p>"#;
        assert_eq!(kinds(src), vec![BlockKind::Preserved]);
    }

    #[test]
    fn empty_document_has_no_blocks() {
        assert!(parse("").unwrap().blocks.is_empty());
        assert!(parse("   \n  ").unwrap().blocks.is_empty());
    }
}
