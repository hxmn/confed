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
use crate::marks::{self, MarkId, Marker};
use crate::mdblock;
use crate::ConvertOptions;
use std::cell::RefCell;

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
    // A user may have written an inline comment mark at the start of a line,
    // where CommonMark would read it as an HTML block; the same repair the
    // renderer applies keeps it a paragraph.
    let markdown = marks::normalize_line_starts(markdown);
    let arena = Arena::new();
    let root = mdblock::parse(&arena, &markdown);
    let g = Generator { opts, line_offset, open_marks: RefCell::new(Vec::new()) };
    let mut out = String::new();
    g.blocks(root, &mut out)?;
    Ok(out)
}

struct Generator<'a> {
    opts: &'a ConvertOptions,
    line_offset: usize,
    /// Marker refs of the inline comment marks currently open, in opening
    /// order. Storage markers must nest, so an open mark is closed before any
    /// element boundary and reopened after it — the same fragmenting Confluence
    /// does itself.
    open_marks: RefCell<Vec<String>>,
}

fn marker_open(r: &str) -> String {
    format!(r#"<ac:inline-comment-marker ac:ref="{}">"#, dom::escape_attr(r))
}

const MARKER_CLOSE: &str = "</ac:inline-comment-marker>";

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
                self.inline_run(node, out)?;
                out.push_str("</p>");
            }
            NodeValue::Heading(h) => {
                let level = h.level.clamp(1, 6);
                out.push_str(&format!("<h{level}>"));
                self.inline_run(node, out)?;
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
                self.inline_run(node, out)?;
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
            self.inline_run(children[0], out)?;
            return Ok(());
        }
        for (i, child) in children.iter().enumerate() {
            let is_para = matches!(child.data.borrow().value, NodeValue::Paragraph);
            if is_para && i == 0 {
                self.inline_run(child, out)?;
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
                self.inline_run(cell, out)?;
                out.push_str(&format!("</p></{tag}>"));
            }
            out.push_str("</tr>");
        }
        out.push_str("</tbody></table>");
        Ok(())
    }

    fn html_block(&self, literal: &str, out: &mut String) -> ConvertResult<()> {
        // A line that is only inline comment marks (a closer left alone on a
        // line) is layer, not content.
        if literal.trim_start().starts_with("<!--")
            && marks::parse_marker(first_comment(literal)).is_some()
        {
            let stripped = marks::strip(literal);
            if !stripped.body.trim().is_empty() {
                out.push_str(&generate_at(&stripped.body, self.opts, self.line_offset)?);
            }
            return Ok(());
        }
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

    /// The inline content of one block. A mark still open at the end is closed
    /// here: markers never cross a block boundary in storage.
    fn inline_run(&self, parent: &'a AstNode<'a>, out: &mut String) -> ConvertResult<()> {
        self.inlines(parent, out)?;
        let open = std::mem::take(&mut *self.open_marks.borrow_mut());
        for r in open.iter().rev() {
            emit_marker_close(out, r);
        }
        Ok(())
    }

    /// Wrap `node`'s inline children in `open_tag`/`close_tag`, keeping every
    /// open comment marker properly nested around the element.
    fn wrapped(
        &self,
        node: &'a AstNode<'a>,
        open_tag: &str,
        close_tag: &str,
        out: &mut String,
    ) -> ConvertResult<()> {
        let outer = self.open_marks.borrow().clone();
        for r in outer.iter().rev() {
            emit_marker_close(out, r);
        }
        out.push_str(open_tag);
        for r in &outer {
            out.push_str(&marker_open(r));
        }
        self.inlines(node, out)?;
        let inner = self.open_marks.borrow().clone();
        for r in inner.iter().rev() {
            emit_marker_close(out, r);
        }
        out.push_str(close_tag);
        for r in &inner {
            out.push_str(&marker_open(r));
        }
        Ok(())
    }

    fn mark(&self, html: &str, out: &mut String) {
        match marks::parse_marker(html.trim()) {
            Some(Marker::Open { id: MarkId::Comment(id), .. }) => {
                if let Some(r) = self.opts.marker_ref_for(&id) {
                    out.push_str(&marker_open(r));
                    self.open_marks.borrow_mut().push(r.to_string());
                }
            }
            Some(Marker::Close { id: MarkId::Comment(id) }) => {
                let Some(r) = self.opts.marker_ref_for(&id) else { return };
                let mut open = self.open_marks.borrow_mut();
                let Some(pos) = open.iter().rposition(|o| o == r) else { return };
                // Close whatever opened after it, close it, reopen the rest.
                let inner: Vec<String> = open.drain(pos + 1..).collect();
                for i in inner.iter().rev() {
                    emit_marker_close(out, i);
                }
                open.pop();
                emit_marker_close(out, r);
                for i in inner {
                    out.push_str(&marker_open(&i));
                    open.push(i);
                }
            }
            // Drafts are created through the API, never written into storage.
            Some(_) | None => {}
        }
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
            // An inline comment mark becomes the marker Confluence attaches
            // the thread to.
            NodeValue::HtmlInline(h) if marks::parse_marker(h.trim()).is_some() => {
                self.mark(h, out)
            }
            // `<u>`, `<sub>`, `<sup>` come back through here; they are already
            // storage-legal XHTML.
            NodeValue::HtmlInline(h) => out.push_str(h),
            NodeValue::Strong => self.wrapped(node, "<strong>", "</strong>", out)?,
            NodeValue::Emph => self.wrapped(node, "<em>", "</em>", out)?,
            NodeValue::Strikethrough => self.wrapped(node, "<del>", "</del>", out)?,
            NodeValue::Link(l) => {
                let outer = self.open_marks.borrow().clone();
                for r in outer.iter().rev() {
                    emit_marker_close(out, r);
                }
                let mut label = String::new();
                for r in &outer {
                    label.push_str(&marker_open(r));
                }
                self.inlines(node, &mut label)?;
                let inner = self.open_marks.borrow().clone();
                for r in inner.iter().rev() {
                    emit_marker_close(&mut label, r);
                }
                out.push_str(&self.link(&l.url, &label));
                for r in &inner {
                    out.push_str(&marker_open(r));
                }
            }
            NodeValue::Image(l) => {
                // Alt text is an attribute: marks in it have nowhere to go.
                let saved = self.open_marks.borrow().clone();
                let mut alt = String::new();
                self.inlines(node, &mut alt)?;
                *self.open_marks.borrow_mut() = saved;
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

        // `[@Name](user:<key>)` names somebody directly — the way to write a
        // mention of someone confed has not resolved yet (`confed user search`
        // gives the key). `user:account-id=…` and `user:username=…` say which
        // attribute; a bare key is matched against known people, else a userkey.
        if let Some(id) = url.strip_prefix("user:") {
            let (attr, value) = match id.split_once('=') {
                Some((attr @ ("userkey" | "account-id" | "username"), value)) => {
                    (attr.to_string(), value.to_string())
                }
                _ => match self.opts.users.values().find(|u| u.id_value == id) {
                    Some(user) => (user.id_attr.clone(), id.to_string()),
                    None => ("userkey".to_string(), id.to_string()),
                },
            };
            return format!(
                r#"<ac:link><ri:user ri:{attr}="{}" /></ac:link>"#,
                dom::escape_attr(&value)
            );
        }

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

        // A link into this page's sidecar is a link to an attachment. Left as a
        // plain <a href> it would be a dead relative link once the page is back
        // in Confluence. Checked after pages, and only for paths that really are
        // inside the sidecar — a bare `Other.md` is a page, not a file.
        if let Some(file) = self.sidecar_filename(&url) {
            let body = if label.contains('<') {
                format!("<ac:link-body>{label}</ac:link-body>")
            } else {
                format!("<ac:plain-text-link-body>{}</ac:plain-text-link-body>", cdata_text(label))
            };
            return format!(
                r#"<ac:link><ri:attachment ri:filename="{}" />{body}</ac:link>"#,
                dom::escape_attr(&file)
            );
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

    /// A path inside this page's sidecar directory, and nothing else.
    ///
    /// Stricter than [`attachment_filename`](Self::attachment_filename), which
    /// also accepts a bare filename. That leniency is right for an image — a
    /// bare name can only be a file — but wrong for a link, where a bare name is
    /// far more likely to be a page.
    fn sidecar_filename(&self, url: &str) -> Option<String> {
        if url.contains("://") {
            return None;
        }
        let dir = self.opts.attachment_dir.trim_end_matches('/');
        if dir.is_empty() {
            return None;
        }
        url.strip_prefix(&format!("{dir}/")).map(str::to_string)
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

    // Marks keep every line count, so the repair cannot move a block.
    let new_markdown = &marks::normalize_line_starts(new_markdown);
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

/// Close a marker — or, when it was opened and nothing came between, take
/// the empty pair back out.
fn emit_marker_close(out: &mut String, r: &str) {
    let open = marker_open(r);
    if out.ends_with(&open) {
        out.truncate(out.len() - open.len());
    } else {
        out.push_str(MARKER_CLOSE);
    }
}

/// The first HTML comment in `s`, delimiters included.
fn first_comment(s: &str) -> &str {
    let s = s.trim_start();
    match s.find("-->") {
        Some(end) => &s[..end + 3],
        None => s,
    }
}

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

#[cfg(test)]
mod mention_link_tests {
    use crate::ConvertOptions;

    #[test]
    fn a_user_link_is_a_mention() {
        let out = crate::markdown_to_storage(
            "Ask [@Alice](user:ff8081) now\n",
            &ConvertOptions::default(),
        )
        .unwrap();
        assert_eq!(out, r#"<p>Ask <ac:link><ri:user ri:userkey="ff8081" /></ac:link> now</p>"#);
        let out = crate::markdown_to_storage(
            "[@Bo](user:account-id=5b10:ac)\n",
            &ConvertOptions::default(),
        )
        .unwrap();
        assert!(out.contains(r#"<ri:user ri:account-id="5b10:ac" />"#), "{out}");
    }
}

#[cfg(test)]
mod inline_mark_tests {
    use crate::{ConvertOptions, InlineMark};

    fn opts() -> ConvertOptions {
        let mut o = ConvertOptions::default();
        for (r, id) in [("r7", "7"), ("r8", "8")] {
            o.inline_marks.insert(r.into(), InlineMark { id: id.into(), preview: String::new() });
        }
        o
    }

    fn gen(md: &str) -> String {
        crate::markdown_to_storage(md, &opts()).unwrap()
    }

    const M7: &str = r#"<ac:inline-comment-marker ac:ref="r7">"#;
    const M8: &str = r#"<ac:inline-comment-marker ac:ref="r8">"#;
    const E: &str = "</ac:inline-comment-marker>";

    #[test]
    fn a_mark_becomes_the_storage_marker() {
        assert_eq!(gen("a <!--c 7 note-->b<!--/c 7--> c\n"), format!("<p>a {M7}b{E} c</p>"));
    }

    #[test]
    fn a_mark_across_emphasis_is_fragmented_so_the_xml_nests() {
        assert_eq!(
            gen("a <!--c 7-->b **c<!--/c 7--> d** e\n"),
            format!("<p>a {M7}b {E}<strong>{M7}c{E} d</strong> e</p>")
        );
        assert_eq!(
            gen("**a <!--c 7-->b** c<!--/c 7--> d\n"),
            format!("<p><strong>a {M7}b{E}</strong>{M7} c{E} d</p>")
        );
    }

    #[test]
    fn overlapping_marks_nest_by_fragmenting() {
        assert_eq!(
            gen("x <!--c 7-->a <!--c 8-->b<!--/c 7--> c<!--/c 8-->\n"),
            format!("<p>x {M7}a {M8}b{E}{E}{M8} c{E}</p>")
        );
    }

    #[test]
    fn a_mark_inside_a_link_label_stays_inside_the_link() {
        let out = gen("see [the <!--c 7-->docs<!--/c 7-->](https://x.test) now\n");
        assert_eq!(out, format!(r#"<p>see <a href="https://x.test">the {M7}docs{E}</a> now</p>"#));
    }

    #[test]
    fn drafts_and_unknown_ids_emit_nothing() {
        assert_eq!(gen("a <!--c new ask-->b<!--/c new--> c\n"), "<p>a b c</p>");
        assert_eq!(gen("a <!--c 999-->b<!--/c 999--> c\n"), "<p>a b c</p>");
    }

    #[test]
    fn a_draft_written_at_a_line_start_is_still_a_paragraph() {
        assert_eq!(
            gen("<!--c new Is this right?-->The team<!--/c new--> owns it.\nSecond line.\n"),
            "<p>The team owns it. Second line.</p>"
        );
        assert_eq!(gen("- <!--c 7-->item<!--/c 7-->\n"), format!("<ul><li>i{M7}tem{E}</li></ul>"));
    }

    #[test]
    fn a_mark_left_open_is_closed_at_the_block_end() {
        assert_eq!(gen("a <!--c 7-->b\n\nc\n"), format!("<p>a {M7}b{E}</p><p>c</p>"));
    }

    #[test]
    fn a_closer_alone_on_a_line_is_not_content() {
        assert_eq!(gen("a\n\n<!--/c 7-->\n\nb\n"), "<p>a</p><p>b</p>");
    }

    #[test]
    fn marks_never_reach_storage_verbatim() {
        for md in [
            "a <!--c 7-->b<!--/c 7-->\n",
            "<!--c new x-->a<!--/c new-->\n",
            "# <!--c 8-->T<!--/c 8-->\n",
            "| <!--c 7-->a<!--/c 7--> |\n| --- |\n| b |\n",
        ] {
            let out = gen(md);
            assert!(!out.contains("<!--c"), "{md:?} -> {out}");
            crate::dom::check_well_formed(&out).unwrap();
        }
    }
}

#[cfg(test)]
mod attachment_mapping_tests {
    use crate::ConvertOptions;

    fn opts() -> ConvertOptions {
        ConvertOptions {
            attachment_dir: ".Page".into(),
            base_url: "https://wiki.corp".into(),
            ..Default::default()
        }
    }

    /// Storage → Markdown → storage, for every way a page can point at a file.
    #[test]
    fn images_and_files_survive_the_round_trip() {
        let cases = [
            (
                "an attached image",
                "<p><ac:image><ri:attachment ri:filename=\"diagram.png\"/></ac:image></p>",
                "![](.Page/diagram.png)",
                "ri:attachment ri:filename=\"diagram.png\"",
            ),
            (
                "an external image",
                "<p><ac:image><ri:url ri:value=\"https://example.test/x.png\"/></ac:image></p>",
                "![](https://example.test/x.png)",
                "ri:url ri:value=\"https://example.test/x.png\"",
            ),
            (
                "a link to an attached file",
                "<p><ac:link><ri:attachment ri:filename=\"spec.pdf\"/>\
                 <ac:plain-text-link-body><![CDATA[the spec]]></ac:plain-text-link-body></ac:link></p>",
                "[the spec](.Page/spec.pdf)",
                "<ac:link><ri:attachment ri:filename=\"spec.pdf\" />",
            ),
        ];

        for (what, storage, expected_md, expected_back) in cases {
            let md = crate::storage_to_markdown(storage, &opts()).unwrap().markdown;
            assert_eq!(md.trim(), expected_md, "{what}: markdown");

            let back = crate::markdown_to_storage(&md, &opts()).unwrap();
            assert!(back.contains(expected_back), "{what}: went back as {back}");
            assert!(!back.contains("<a href"), "{what} must not become a plain link: {back}");
        }
    }

    #[test]
    fn an_alt_text_survives_in_both_directions() {
        let storage =
            "<p><ac:image ac:alt=\"Network diagram\"><ri:attachment ri:filename=\"net.png\"/>\
             </ac:image></p>";
        let md = crate::storage_to_markdown(storage, &opts()).unwrap().markdown;
        assert_eq!(md.trim(), "![Network diagram](.Page/net.png)");

        let back = crate::markdown_to_storage(&md, &opts()).unwrap();
        assert!(back.contains(r#"ac:alt="Network diagram""#), "got {back}");
        assert!(back.contains(r#"ri:filename="net.png""#));
    }

    #[test]
    fn an_ordinary_external_link_is_still_a_plain_link() {
        let md = "See [the site](https://example.test/page) and [a peer](../Other.md).";
        let back = crate::markdown_to_storage(md, &opts()).unwrap();
        assert!(back.contains(r#"<a href="https://example.test/page">"#), "got {back}");
        assert!(!back.contains("ri:attachment"), "only sidecar paths are attachments");
    }

    #[test]
    fn a_filename_with_spaces_or_markup_characters_is_handled() {
        let storage = "<p><ac:link><ri:attachment ri:filename=\"quarterly report.pdf\"/>\
             <ac:plain-text-link-body><![CDATA[report]]></ac:plain-text-link-body></ac:link></p>";
        let md = crate::storage_to_markdown(storage, &opts()).unwrap().markdown;
        assert!(md.contains("quarterly report.pdf"), "got {md}");

        let back = crate::markdown_to_storage(&md, &opts()).unwrap();
        assert!(back.contains(r#"ri:filename="quarterly report.pdf""#), "got {back}");
    }
}
