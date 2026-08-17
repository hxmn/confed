//! Markdown → storage format.
//!
//! Two entry points with very different risk profiles:
//!
//! * [`generate`] rewrites Markdown into storage from scratch. Everything the
//!   Markdown does not say is gone — that is fine for a new page, and it is the
//!   documented cost of *editing* a block.
//! * [`patch`] is what push actually uses. It diffs the new Markdown against the
//!   base Markdown at block granularity and copies the original storage bytes
//!   for every block that did not change, so [`generate`]'s lossiness is
//!   confined to blocks the user actually touched.

use comrak::nodes::{AstNode, ListType, NodeValue};
use comrak::Arena;
use similar::{capture_diff_slices, Algorithm, DiffOp};

use crate::blockmap::{hash_text, slice_lines, BlockMap};
use crate::dom;
use crate::error::{ConvertError, ConvertResult};
use crate::macros;
use crate::mdblock;
use crate::ConvertOptions;

/// The fence language that marks a block of verbatim storage format.
pub const PRESERVED_LANG: &str = "confluence";

pub fn generate(markdown: &str, opts: &ConvertOptions) -> ConvertResult<String> {
    let body = generate_at(markdown, opts, 0)?;
    dom::check_well_formed(&body)
        .map_err(|e| ConvertError::Generate(format!("generated body is not well-formed: {e}")))?;
    Ok(body)
}

/// `line_offset` shifts reported line numbers so errors inside a single patched
/// block still point at the right line of the whole document.
fn generate_at(markdown: &str, opts: &ConvertOptions, line_offset: usize) -> ConvertResult<String> {
    let arena = Arena::new();
    let root = mdblock::parse(&arena, markdown);
    let g = Generator { opts, line_offset };
    let mut out = String::new();
    g.blocks(root, &mut out)?;
    Ok(out)
}

struct Generator<'a> {
    opts: &'a ConvertOptions,
    line_offset: usize,
}

impl<'a> Generator<'a> {
    fn blocks(&self, parent: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        for child in parent.children() {
            self.block(child, out)?;
        }
        Ok(())
    }

    fn block(&self, node: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        let data = node.data.borrow();
        let line = data.sourcepos.start.line + self.line_offset;
        match &data.value {
            NodeValue::Document => self.blocks(node, out)?,
            NodeValue::Paragraph => {
                out.push_str("<p>");
                self.inlines(node, out)?;
                out.push_str("</p>");
            }
            NodeValue::Heading(h) => {
                let level = h.level.clamp(1, 6);
                out.push_str(&format!("<h{level}>"));
                self.inlines(node, out)?;
                out.push_str(&format!("</h{level}>"));
            }
            NodeValue::ThematicBreak => out.push_str("<hr />"),
            NodeValue::CodeBlock(c) => {
                let info = c.info.trim();
                if info == PRESERVED_LANG {
                    // The lossless path: the user's bytes go up untouched. The
                    // only thing standing between a mangled edit and a corrupted
                    // page is this check.
                    let raw = c.literal.strip_suffix('\n').unwrap_or(&c.literal);
                    dom::check_well_formed(raw)
                        .map_err(|detail| ConvertError::InvalidPreservedBlock { line, detail })?;
                    out.push_str(raw);
                } else {
                    let lang = info.split_whitespace().next().unwrap_or("");
                    out.push_str(r#"<ac:structured-macro ac:name="code" ac:schema-version="1">"#);
                    if !lang.is_empty() {
                        out.push_str(&format!(
                            r#"<ac:parameter ac:name="language">{}</ac:parameter>"#,
                            dom::escape_text(lang)
                        ));
                    }
                    out.push_str("<ac:plain-text-body>");
                    out.push_str(&cdata(c.literal.strip_suffix('\n').unwrap_or(&c.literal)));
                    out.push_str("</ac:plain-text-body></ac:structured-macro>");
                }
            }
            NodeValue::List(l) => {
                let is_tasks = node
                    .children()
                    .any(|c| matches!(c.data.borrow().value, NodeValue::TaskItem(_)));
                if is_tasks {
                    out.push_str("<ac:task-list>");
                    for item in node.children() {
                        self.task(item, out)?;
                    }
                    out.push_str("</ac:task-list>");
                } else {
                    let (open, close) = match l.list_type {
                        ListType::Ordered => ("<ol>", "</ol>"),
                        _ => ("<ul>", "</ul>"),
                    };
                    out.push_str(open);
                    for item in node.children() {
                        self.item(item, out)?;
                    }
                    out.push_str(close);
                }
            }
            NodeValue::Item(_) | NodeValue::TaskItem(_) => self.item(node, out)?,
            NodeValue::BlockQuote => {
                out.push_str("<blockquote>");
                self.blocks(node, out)?;
                out.push_str("</blockquote>");
            }
            NodeValue::Alert(a) => {
                let name = macros::alert_to_admonition(&format!("{:?}", a.alert_type));
                self.admonition(node, name, a.title.clone(), out)?;
            }
            NodeValue::Table(_) => self.table(node, out)?,
            NodeValue::HtmlBlock(h) => self.html_block(h.literal.trim_end(), out)?,
            // Anything else (footnotes, description lists, math…) is not
            // reachable with our parser options; render its text rather than
            // dropping it.
            _ => {
                out.push_str("<p>");
                self.inlines(node, out)?;
                out.push_str("</p>");
            }
        }
        Ok(())
    }

    fn item(&self, node: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        out.push_str("<li>");
        self.item_content(node, out)?;
        out.push_str("</li>");
        Ok(())
    }

    /// A list item holding one paragraph is written inline, the way Confluence
    /// writes its own lists; anything richer keeps its block structure.
    fn item_content(&self, node: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        let children: Vec<_> = node.children().collect();
        let single_para =
            children.len() == 1 && matches!(children[0].data.borrow().value, NodeValue::Paragraph);
        if single_para {
            self.inlines(children[0], out)?;
            return Ok(());
        }
        for (i, child) in children.iter().enumerate() {
            let is_para = matches!(child.data.borrow().value, NodeValue::Paragraph);
            if is_para && i == 0 {
                self.inlines(child, out)?;
            } else {
                self.block(child, out)?;
            }
        }
        Ok(())
    }

    fn task(&self, node: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        let done = matches!(node.data.borrow().value, NodeValue::TaskItem(Some(_)));
        out.push_str("<ac:task><ac:task-status>");
        out.push_str(if done { "complete" } else { "incomplete" });
        out.push_str("</ac:task-status><ac:task-body>");
        self.item_content(node, out)?;
        out.push_str("</ac:task-body></ac:task>");
        Ok(())
    }

    fn admonition(
        &self,
        node: &'a AstNode<'a>,
        name: &str,
        explicit_title: Option<String>,
        out: &mut String,
    ) -> ConvertResult<()> {
        let mut children: Vec<&'a AstNode<'a>> = node.children().collect();
        // `> [!NOTE]\n> **Title**\n>\n> body` — a first paragraph that is
        // *entirely* bold is the title the renderer wrote. Requiring the whole
        // paragraph keeps a body that merely starts bold from being stolen.
        let mut title = explicit_title;
        if title.is_none() {
            if let Some(first) = children.first() {
                if let Some(t) = sole_strong_text(first) {
                    title = Some(t);
                    children.remove(0);
                }
            }
        }
        out.push_str(&format!(
            r#"<ac:structured-macro ac:name="{}" ac:schema-version="1">"#,
            dom::escape_attr(name)
        ));
        if let Some(t) = title {
            if !t.trim().is_empty() {
                out.push_str(&format!(
                    r#"<ac:parameter ac:name="title">{}</ac:parameter>"#,
                    dom::escape_text(t.trim())
                ));
            }
        }
        out.push_str("<ac:rich-text-body>");
        for child in children {
            self.block(child, out)?;
        }
        out.push_str("</ac:rich-text-body></ac:structured-macro>");
        Ok(())
    }

    fn table(&self, node: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        out.push_str("<table><tbody>");
        for row in node.children() {
            let header = matches!(row.data.borrow().value, NodeValue::TableRow(true));
            out.push_str("<tr>");
            for cell in row.children() {
                let tag = if header { "th" } else { "td" };
                out.push_str(&format!("<{tag}><p>"));
                self.inlines(cell, out)?;
                out.push_str(&format!("</p></{tag}>"));
            }
            out.push_str("</tr>");
        }
        out.push_str("</tbody></table>");
        Ok(())
    }

    fn html_block(&self, literal: &str, out: &mut String) -> ConvertResult<()> {
        if literal.contains("confed:toc") {
            out.push_str(r#"<ac:structured-macro ac:name="toc" ac:schema-version="1" />"#);
            return Ok(());
        }
        if let Some((title, body)) = split_details(literal) {
            out.push_str(r#"<ac:structured-macro ac:name="expand" ac:schema-version="1">"#);
            let title = dom::unescape(&title);
            if !title.trim().is_empty() && title.trim() != "Details" {
                out.push_str(&format!(
                    r#"<ac:parameter ac:name="title">{}</ac:parameter>"#,
                    dom::escape_text(title.trim())
                ));
            }
            out.push_str("<ac:rich-text-body>");
            out.push_str(&generate_at(&format!("{}\n", body.trim()), self.opts, self.line_offset)?);
            out.push_str("</ac:rich-text-body></ac:structured-macro>");
            return Ok(());
        }
        // Hand-written HTML: pass it through when it is well-formed, otherwise
        // show it as text rather than uploading something that breaks the page.
        match dom::check_well_formed(literal) {
            Ok(()) => out.push_str(literal),
            Err(_) => {
                out.push_str("<p>");
                out.push_str(&dom::escape_text(literal));
                out.push_str("</p>");
            }
        }
        Ok(())
    }

    // -- inline ------------------------------------------------------------

    fn inlines(&self, parent: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        for child in parent.children() {
            self.inline(child, out)?;
        }
        Ok(())
    }

    fn inline(&self, node: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        let data = node.data.borrow();
        match &data.value {
            NodeValue::Text(t) => out.push_str(&dom::escape_text(t)),
            NodeValue::SoftBreak => out.push(' '),
            NodeValue::LineBreak => out.push_str("<br />"),
            NodeValue::Code(c) => {
                out.push_str("<code>");
                out.push_str(&dom::escape_text(&c.literal));
                out.push_str("</code>");
            }
            // `<u>`, `<sub>`, `<sup>` come back through here; they are already
            // storage-legal XHTML.
            NodeValue::HtmlInline(h) => out.push_str(h),
            NodeValue::Strong => {
                out.push_str("<strong>");
                self.inlines(node, out)?;
                out.push_str("</strong>");
            }
            NodeValue::Emph => {
                out.push_str("<em>");
                self.inlines(node, out)?;
                out.push_str("</em>");
            }
            NodeValue::Strikethrough => {
                out.push_str("<del>");
                self.inlines(node, out)?;
                out.push_str("</del>");
            }
            NodeValue::Link(l) => {
                let label = {
                    let mut s = String::new();
                    self.inlines(node, &mut s)?;
                    s
                };
                out.push_str(&self.link(&l.url, &label));
            }
            NodeValue::Image(l) => {
                let mut alt = String::new();
                self.inlines(node, &mut alt)?;
                out.push_str(&self.image(&l.url, &alt));
            }
            NodeValue::Escaped => self.inlines(node, out)?,
            NodeValue::Paragraph => self.inlines(node, out)?,
            _ => self.inlines(node, out)?,
        }
        Ok(())
    }

    fn link(&self, url: &str, label: &str) -> String {
        let url = unwrap_destination(url);

        // A link to somebody's profile is a mention, and goes back as the
        // element it came from — identified the way this site identifies people.
        if let Some(user) = self.opts.users.values().find(|u| u.profile_url == url) {
            return format!(
                r#"<ac:link><ri:user ri:{}="{}" /></ac:link>"#,
                user.id_attr,
                dom::escape_attr(&user.id_value)
            );
        }

        if let Some(target) = self.opts.link_targets.get(url.as_str()) {
            let attr = if target.chars().all(|c| c.is_ascii_digit()) {
                format!(r#"ri:content-id="{}""#, dom::escape_attr(target))
            } else {
                format!(r#"ri:content-title="{}""#, dom::escape_attr(target))
            };
            // A label carrying inline markup needs the rich body; CDATA would
            // flatten it to plain text.
            let body = if label.contains('<') {
                format!("<ac:link-body>{label}</ac:link-body>")
            } else {
                format!("<ac:plain-text-link-body>{}</ac:plain-text-link-body>", cdata_text(label))
            };
            return format!("<ac:link><ri:page {attr} />{body}</ac:link>");
        }
        format!(r#"<a href="{}">{}</a>"#, dom::escape_attr(&url), label)
    }

    fn image(&self, url: &str, alt: &str) -> String {
        let url = unwrap_destination(url);
        let alt_attr = if alt.trim().is_empty() {
            String::new()
        } else {
            format!(r#" ac:alt="{}""#, dom::escape_attr(&strip_tags(alt)))
        };
        match self.attachment_filename(&url) {
            Some(file) => format!(
                r#"<ac:image{alt_attr}><ri:attachment ri:filename="{}" /></ac:image>"#,
                dom::escape_attr(&file)
            ),
            None => format!(
                r#"<ac:image{alt_attr}><ri:url ri:value="{}" /></ac:image>"#,
                dom::escape_attr(&url)
            ),
        }
    }

    /// An image whose path points into this page's sidecar directory is an
    /// attachment; anything else is an external URL.
    fn attachment_filename(&self, url: &str) -> Option<String> {
        if url.contains("://") {
            return None;
        }
        let dir = self.opts.attachment_dir.trim_end_matches('/');
        if !dir.is_empty() {
            if let Some(rest) = url.strip_prefix(&format!("{dir}/")) {
                return Some(rest.to_string());
            }
            return None;
        }
        // No sidecar configured: a bare filename is still an attachment.
        if url.contains('/') {
            None
        } else {
            Some(url.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Patching
// ---------------------------------------------------------------------------

pub fn patch(
    base_storage: &str,
    base_map: &BlockMap,
    base_markdown: &str,
    new_markdown: &str,
    opts: &ConvertOptions,
) -> ConvertResult<String> {
    check_map_matches(base_storage, base_map, base_markdown)?;

    let new_ranges = mdblock::split_blocks(new_markdown);
    let base_hashes: Vec<String> = base_map.blocks.iter().map(|b| b.hash.clone()).collect();
    let new_texts: Vec<String> =
        new_ranges.iter().map(|(s, e)| slice_lines(new_markdown, *s, *e)).collect();
    let new_hashes: Vec<String> = new_texts.iter().map(|t| hash_text(t)).collect();

    // Everything before the first block (a stray comment, indentation) is not
    // owned by any block, so it rides along at the front.
    let prefix = match base_map.blocks.first() {
        Some(b) => &base_storage[..b.storage_span.0],
        None => "",
    };
    let mut out = String::with_capacity(base_storage.len());
    out.push_str(prefix);

    let ops = capture_diff_slices(Algorithm::Myers, &base_hashes, &new_hashes);
    for op in ops {
        match op {
            DiffOp::Equal { old_index, len, .. } => {
                for i in old_index..old_index + len {
                    out.push_str(verbatim_with_gap(base_storage, base_map, i));
                }
            }
            DiffOp::Delete { .. } => {}
            DiffOp::Insert { new_index, new_len, .. } => {
                self_generate(&new_texts, &new_ranges, new_index, new_len, opts, &mut out)?;
            }
            DiffOp::Replace { new_index, new_len, .. } => {
                self_generate(&new_texts, &new_ranges, new_index, new_len, opts, &mut out)?;
            }
        }
    }

    // No trimming: with every block unchanged, `prefix + Σ(span + gap)` is the
    // base document byte for byte, and that identity is a tested guarantee.
    let body = out;
    dom::check_well_formed(&body)
        .map_err(|e| ConvertError::Generate(format!("patched body is not well-formed: {e}")))?;
    Ok(body)
}

fn self_generate(
    new_texts: &[String],
    new_ranges: &[(usize, usize)],
    start: usize,
    len: usize,
    opts: &ConvertOptions,
    out: &mut String,
) -> ConvertResult<()> {
    for i in start..start + len {
        let text = &new_texts[i];
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(&generate_at(text, opts, new_ranges[i].0)?);
        out.push('\n');
    }
    Ok(())
}

/// A block's original bytes *plus* the whitespace that followed it, so copying
/// a run of unchanged blocks reproduces the base byte-for-byte.
fn verbatim_with_gap<'s>(storage: &'s str, map: &BlockMap, index: usize) -> &'s str {
    let (start, end) = map.blocks[index].storage_span;
    let gap_end = map.blocks.get(index + 1).map(|b| b.storage_span.0).unwrap_or(storage.len());
    &storage[start..gap_end.max(end)]
}

/// Refuse to patch against a block map that no longer describes the base.
///
/// Getting this wrong is the one failure mode that silently corrupts a page, so
/// the check is deliberately strict: any disagreement sends the caller to a full
/// regeneration instead.
fn check_map_matches(
    base_storage: &str,
    base_map: &BlockMap,
    base_markdown: &str,
) -> ConvertResult<()> {
    base_map.validate_spans(base_storage.len()).map_err(ConvertError::StaleBlockMap)?;
    base_map.validate_md_spans().map_err(ConvertError::StaleBlockMap)?;

    let ranges = mdblock::split_blocks(base_markdown);
    if ranges.len() != base_map.blocks.len() {
        return Err(ConvertError::StaleBlockMap(format!(
            "base Markdown splits into {} blocks but the map has {}",
            ranges.len(),
            base_map.blocks.len()
        )));
    }
    for (i, (range, entry)) in ranges.iter().zip(&base_map.blocks).enumerate() {
        if *range != entry.md_span {
            return Err(ConvertError::StaleBlockMap(format!(
                "block {i}: base Markdown occupies lines {:?} but the map says {:?}",
                range, entry.md_span
            )));
        }
        let text = slice_lines(base_markdown, range.0, range.1);
        if hash_text(&text) != entry.hash {
            return Err(ConvertError::StaleBlockMap(format!(
                "block {i}: base Markdown does not match the recorded hash"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// CDATA cannot contain `]]>`; split the section around every occurrence.
fn cdata(body: &str) -> String {
    format!("<![CDATA[{}]]>", body.replace("]]>", "]]]]><![CDATA[>"))
}

fn cdata_text(label: &str) -> String {
    cdata(&strip_tags(label))
}

/// Inline markup has no place in a CDATA link body or an `alt` attribute.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    dom::unescape(&out)
}

/// Undo `to_markdown`'s angle-bracket wrapping of destinations with spaces.
fn unwrap_destination(url: &str) -> String {
    url.strip_prefix('<').and_then(|u| u.strip_suffix('>')).unwrap_or(url).to_string()
}

/// The title and body of a `<details><summary>…</summary>…</details>` block.
fn split_details(literal: &str) -> Option<(String, String)> {
    let l = literal.trim();
    if !l.starts_with("<details") {
        return None;
    }
    let open = l.find("<summary")?;
    let title_start = l[open..].find('>')? + open + 1;
    let title_end = l[title_start..].find("</summary>")? + title_start;
    let body_start = title_end + "</summary>".len();
    let body_end = l.rfind("</details>").unwrap_or(l.len());
    if body_end < body_start {
        return None;
    }
    Some((l[title_start..title_end].to_string(), l[body_start..body_end].to_string()))
}

/// A first child that is a paragraph consisting solely of bold text.
fn sole_strong_text<'a>(node: &'a AstNode<'a>) -> Option<String> {
    if !matches!(node.data.borrow().value, NodeValue::Paragraph) {
        return None;
    }
    let children: Vec<_> = node.children().collect();
    if children.len() != 1 {
        return None;
    }
    if !matches!(children[0].data.borrow().value, NodeValue::Strong) {
        return None;
    }
    let mut text = String::new();
    collect_plain(children[0], &mut text);
    Some(text)
}

fn collect_plain<'a>(node: &'a AstNode<'a>, out: &mut String) {
    match &node.data.borrow().value {
        NodeValue::Text(t) => out.push_str(t),
        NodeValue::Code(c) => out.push_str(&c.literal),
        NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
        _ => {
            for c in node.children() {
                collect_plain(c, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(md: &str) -> String {
        generate(md, &ConvertOptions { attachment_dir: ".page".into(), ..Default::default() })
            .unwrap()
    }

    #[test]
    fn paragraphs_and_headings() {
        assert_eq!(gen("hello\n"), "<p>hello</p>");
        assert_eq!(gen("## Title\n"), "<h2>Title</h2>");
    }

    #[test]
    fn inline_emphasis() {
        assert_eq!(
            gen("**a** *b* ~~c~~ `d`\n"),
            "<p><strong>a</strong> <em>b</em> <del>c</del> <code>d</code></p>"
        );
    }

    #[test]
    fn inline_html_survives() {
        assert_eq!(gen("x<sub>1</sub>\n"), "<p>x<sub>1</sub></p>");
    }

    #[test]
    fn text_is_escaped() {
        assert_eq!(gen("a < b & c > d\n"), "<p>a &lt; b &amp; c &gt; d</p>");
    }

    #[test]
    fn lists() {
        assert_eq!(gen("- a\n- b\n"), "<ul><li>a</li><li>b</li></ul>");
        assert_eq!(gen("1. a\n2. b\n"), "<ol><li>a</li><li>b</li></ol>");
        assert_eq!(gen("- a\n  - b\n"), "<ul><li>a<ul><li>b</li></ul></li></ul>");
    }

    #[test]
    fn task_lists() {
        assert_eq!(
            gen("- [x] done\n- [ ] todo\n"),
            concat!(
                "<ac:task-list>",
                "<ac:task><ac:task-status>complete</ac:task-status><ac:task-body>done</ac:task-body></ac:task>",
                "<ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>todo</ac:task-body></ac:task>",
                "</ac:task-list>"
            )
        );
    }

    #[test]
    fn code_fence_becomes_the_code_macro() {
        assert_eq!(
            gen("```rust\nfn a() {}\n```\n"),
            concat!(
                r#"<ac:structured-macro ac:name="code" ac:schema-version="1">"#,
                r#"<ac:parameter ac:name="language">rust</ac:parameter>"#,
                "<ac:plain-text-body><![CDATA[fn a() {}]]></ac:plain-text-body>",
                "</ac:structured-macro>"
            )
        );
    }

    #[test]
    fn cdata_end_marker_inside_code_is_split() {
        let out = gen("```\na]]>b\n```\n");
        assert!(out.contains("<![CDATA[a]]]]><![CDATA[>b]]>"), "{out}");
        // And the result is still parseable back to the original text.
        let doc = dom::parse_fragment(&out).unwrap();
        let el = doc[0].as_element().unwrap();
        assert_eq!(macros::plain_text_body(el).as_deref(), Some("a]]>b"));
    }

    #[test]
    fn alerts_become_admonitions() {
        assert_eq!(
            gen("> [!WARNING]\n> **Careful**\n>\n> Do not.\n"),
            concat!(
                r#"<ac:structured-macro ac:name="warning" ac:schema-version="1">"#,
                r#"<ac:parameter ac:name="title">Careful</ac:parameter>"#,
                "<ac:rich-text-body><p>Do not.</p></ac:rich-text-body>",
                "</ac:structured-macro>"
            )
        );
    }

    #[test]
    fn a_body_that_merely_starts_bold_is_not_stolen_as_a_title() {
        let out = gen("> [!NOTE]\n> **Bold** start of a sentence.\n");
        assert!(!out.contains("ac:name=\"title\""), "{out}");
        assert!(out.contains("<strong>Bold</strong>"), "{out}");
    }

    #[test]
    fn plain_blockquotes_stay_blockquotes() {
        assert_eq!(gen("> quoted\n"), "<blockquote><p>quoted</p></blockquote>");
    }

    #[test]
    fn tables() {
        assert_eq!(
            gen("| a | b |\n| --- | --- |\n| 1 | 2 |\n"),
            concat!(
                "<table><tbody>",
                "<tr><th><p>a</p></th><th><p>b</p></th></tr>",
                "<tr><td><p>1</p></td><td><p>2</p></td></tr>",
                "</tbody></table>"
            )
        );
    }

    #[test]
    fn attachment_images_and_external_images() {
        assert_eq!(
            gen("![Chart](.page/chart.png)\n"),
            r#"<p><ac:image ac:alt="Chart"><ri:attachment ri:filename="chart.png" /></ac:image></p>"#
        );
        assert_eq!(
            gen("![](https://x.test/a.png)\n"),
            r#"<p><ac:image><ri:url ri:value="https://x.test/a.png" /></ac:image></p>"#
        );
    }

    #[test]
    fn links() {
        assert_eq!(
            gen("[docs](https://x.test/a)\n"),
            r#"<p><a href="https://x.test/a">docs</a></p>"#
        );
        let mut opts = ConvertOptions::default();
        opts.link_targets.insert("runbook.md".into(), "Runbook".into());
        assert_eq!(
            generate("[the runbook](runbook.md)\n", &opts).unwrap(),
            r#"<p><ac:link><ri:page ri:content-title="Runbook" /><ac:plain-text-link-body><![CDATA[the runbook]]></ac:plain-text-link-body></ac:link></p>"#
        );
    }

    #[test]
    fn expand_round_trips_through_details() {
        assert_eq!(
            gen("<details><summary>More</summary>\nhidden\n</details>\n"),
            concat!(
                r#"<ac:structured-macro ac:name="expand" ac:schema-version="1">"#,
                r#"<ac:parameter ac:name="title">More</ac:parameter>"#,
                "<ac:rich-text-body><p>hidden</p></ac:rich-text-body>",
                "</ac:structured-macro>"
            )
        );
    }

    #[test]
    fn toc_marker_round_trips() {
        assert_eq!(
            gen("<!-- confed:toc -->\n"),
            r#"<ac:structured-macro ac:name="toc" ac:schema-version="1" />"#
        );
    }

    #[test]
    fn preserved_fence_is_emitted_verbatim() {
        let raw = r#"<ac:structured-macro ac:name="jira" ac:macro-id="7f1a"><ac:parameter ac:name="key">PROJ-142</ac:parameter></ac:structured-macro>"#;
        assert_eq!(gen(&format!("```confluence\n{raw}\n```\n")), raw);
    }

    #[test]
    fn a_corrupted_preserved_fence_is_refused_with_its_line() {
        let md = "intro\n\n```confluence\n<ac:structured-macro ac:name=\"jira\">\n```\n";
        let err = generate(md, &ConvertOptions::default()).unwrap_err();
        match err {
            ConvertError::InvalidPreservedBlock { line, .. } => assert_eq!(line, 3),
            other => panic!("expected InvalidPreservedBlock, got {other:?}"),
        }
    }

    #[test]
    fn hard_breaks_become_br() {
        assert_eq!(gen("a\\\nb\n"), "<p>a<br />b</p>");
    }

    #[test]
    fn thematic_break() {
        assert_eq!(gen("---\n"), "<hr />");
    }
}
