//! Storage format → Markdown, plus the block map that makes push non-destructive.
//!
//! Every top-level storage block produces a contiguous run of Markdown lines and
//! a `BlockEntry` tying the two together. Blocks are separated by exactly one
//! blank line, and the line range recorded for a block runs from its first line
//! up to the first line of the next block — the same convention
//! [`crate::mdblock::split_blocks`] uses when it re-splits the Markdown on push.
//! Those two agreeing is what makes an untouched block recognisable later.

use std::collections::HashMap;

use crate::blockmap::{hash_text, BlockEntry, BlockKind, BlockMap};
use crate::dom::{self, Element, Node};
use crate::error::ConvertResult;
use crate::macros::{self, TOC_MARKER};
use crate::storage_parse::StorageDoc;
use crate::{ConvertOptions, Converted};

pub fn render(doc: &StorageDoc, storage: &str, opts: &ConvertOptions) -> ConvertResult<Converted> {
    let nodes = dom::parse_fragment(storage)?;
    let by_start: HashMap<usize, &Node> = nodes.iter().map(|n| (n.span().0, n)).collect();

    let mut r = Renderer { opts, last_bullet: None, unresolved_user: false };

    // Render first, assemble second: a block that renders to nothing is dropped
    // from the map entirely and its bytes become part of the inter-block gap,
    // which the patcher re-emits verbatim. That keeps the `<p></p>` spacer
    // paragraphs Confluence emits constantly out of the Markdown without ever
    // losing them.
    let mut rendered: Vec<(BlockKind, (usize, usize), String)> = Vec::new();
    for block in &doc.blocks {
        let raw = &storage[block.span.0..block.span.1];
        r.unresolved_user = false;
        let mut text = match by_start.get(&block.span.0) {
            Some(node) => r.render_top(node, block.kind, raw),
            None => preserved_fence(raw),
        };
        // A mention confed could not resolve would render as a link to the
        // wrong profile. Keeping the block verbatim is both honest and lossless,
        // and it heals itself on the next pull that can resolve the person.
        let kind = if r.unresolved_user && block.kind != BlockKind::Preserved {
            text = preserved_fence(raw);
            BlockKind::Preserved
        } else {
            block.kind
        };
        let text = text.trim_matches('\n').to_string();
        if text.trim().is_empty() {
            r.last_bullet = None;
            continue;
        }
        rendered.push((kind, block.span, text));
    }

    let mut markdown = String::new();
    let mut blocks = Vec::with_capacity(rendered.len());
    let mut line = 0usize;
    let total = rendered.len();
    for (i, (kind, span, text)) in rendered.into_iter().enumerate() {
        let start_line = line;
        markdown.push_str(&text);
        markdown.push('\n');
        line += text.matches('\n').count() + 1;
        if i + 1 < total {
            markdown.push('\n');
            line += 1;
        }
        blocks.push(BlockEntry {
            kind,
            storage_span: span,
            md_span: (start_line, line),
            hash: hash_text(&text),
        });
    }

    Ok(Converted { markdown, block_map: BlockMap { blocks } })
}

struct Renderer<'a> {
    opts: &'a ConvertOptions,
    /// Set while rendering a block that mentions somebody confed cannot resolve.
    unresolved_user: bool,
    /// The bullet character used by the previous top-level list, if any.
    ///
    /// Two lists separated by a blank line merge into a *single* CommonMark list
    /// when they share a bullet character, which would fuse two storage blocks
    /// into one Markdown block and break the block map. Alternating the marker
    /// keeps them distinct.
    last_bullet: Option<char>,
}

impl Renderer<'_> {
    fn render_top(&mut self, node: &Node, kind: BlockKind, raw: &str) -> String {
        if kind == BlockKind::Preserved {
            self.last_bullet = None;
            return preserved_fence(raw);
        }
        let el = match node {
            Node::Element(el) => el,
            Node::Text(c) => {
                self.last_bullet = None;
                return escape_line_starts(&escape_md(&collapse_ws(&dom::unescape(&c.raw))));
            }
            Node::CData(c) => {
                self.last_bullet = None;
                return escape_line_starts(&escape_md(&collapse_ws(&c.raw)));
            }
            _ => {
                self.last_bullet = None;
                return preserved_fence(raw);
            }
        };

        match kind {
            BlockKind::List => {
                let ordered = el.local() == "ol";
                let bullet = if ordered {
                    '1'
                } else if self.last_bullet == Some('-') {
                    '*'
                } else {
                    '-'
                };
                let delim = if ordered && self.last_bullet == Some('1') { ')' } else { '.' };
                self.last_bullet = Some(bullet);
                self.render_list(el, bullet, delim)
            }
            BlockKind::TaskList => {
                let bullet = if self.last_bullet == Some('-') { '*' } else { '-' };
                self.last_bullet = Some(bullet);
                self.render_task_list(el, bullet)
            }
            _ => {
                self.last_bullet = None;
                self.render_element(el, raw)
            }
        }
    }

    /// Render one block-level element (lists go through `render_top`, which owns
    /// the bullet-alternation state).
    fn render_element(&mut self, el: &Element, raw: &str) -> String {
        if let Some(name) = macros::macro_name(el) {
            return self.render_macro(el, name, raw);
        }
        match el.local() {
            "p" => self.render_paragraph(el),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = el.local()[1..].parse::<usize>().unwrap_or(1);
                let text = self.inline(&el.children);
                let text = text.trim();
                if text.is_empty() {
                    String::new()
                } else {
                    format!("{} {}", "#".repeat(level), text.replace('\n', " ").replace('\\', ""))
                }
            }
            "ul" => self.render_list(el, '-', '.'),
            "ol" => self.render_list(el, '1', '.'),
            "hr" => "---".to_string(),
            "table" => self.render_table(el),
            "blockquote" => {
                let inner = self.render_children_blocks(&el.children);
                prefix_lines(inner.trim(), "> ", ">")
            }
            "task-list" => self.render_task_list(el, '-'),
            "image" => self.render_image(el),
            "br" => String::new(),
            _ => self.render_children_blocks(&el.children),
        }
    }

    fn render_paragraph(&mut self, el: &Element) -> String {
        let text = self.inline(&el.children);
        let text = trim_hard_breaks(text.trim());
        if text.trim().is_empty() {
            return String::new();
        }
        escape_line_starts(&text)
    }

    fn render_macro(&mut self, el: &Element, name: &str, raw: &str) -> String {
        match name {
            "code" | "noformat" => {
                let params = macros::macro_params(el);
                let lang = macros::macro_param(&params, "language").unwrap_or("").trim();
                let body = macros::plain_text_body(el).unwrap_or_default();
                let body = body.trim_matches('\n');
                let fence = "`".repeat(fence_len(body));
                format!("{fence}{lang}\n{body}\n{fence}")
            }
            n if macros::is_admonition(n) => {
                let alert = macros::admonition_to_alert(n).unwrap_or("NOTE");
                let params = macros::macro_params(el);
                let title = macros::macro_param(&params, "title").unwrap_or("").trim().to_string();
                let body = macros::rich_text_body(el)
                    .map(|b| self.render_children_blocks(&b.children))
                    .unwrap_or_default();
                let mut inner = String::new();
                if !title.is_empty() {
                    // Its own paragraph, not merely a bold first line, so the
                    // inverse mapping can tell a title from body text that
                    // happens to start bold.
                    inner.push_str(&format!("**{}**", escape_md(&title)));
                    if !body.trim().is_empty() {
                        inner.push_str("\n\n");
                    }
                }
                inner.push_str(body.trim());
                let quoted = prefix_lines(inner.trim(), "> ", ">");
                format!("> [!{alert}]\n{quoted}")
            }
            "expand" => {
                let params = macros::macro_params(el);
                let title = macros::macro_param(&params, "title").unwrap_or("").trim().to_string();
                let title = if title.is_empty() { "Details".to_string() } else { title };
                let body = macros::rich_text_body(el)
                    .map(|b| self.render_children_blocks(&b.children))
                    .unwrap_or_default();
                // A blank line would terminate the HTML block and split this
                // into three Markdown blocks, so the body is packed tight.
                let body = squeeze_blank_lines(body.trim());
                format!(
                    "<details><summary>{}</summary>\n{}\n</details>",
                    dom::escape_text(&title),
                    body
                )
            }
            "toc" => TOC_MARKER.to_string(),
            "status" => {
                let params = macros::macro_params(el);
                let title = macros::macro_param(&params, "title").unwrap_or("").trim().to_string();
                if title.is_empty() {
                    String::new()
                } else {
                    // The brackets are escaped so this is already the form a
                    // re-render produces: `**[X]**` would come back as
                    // `**\[X\]**` on the next pull and read as a spurious edit.
                    format!("**{}**", escape_md(&format!("[{title}]")))
                }
            }
            _ => preserved_fence(raw),
        }
    }

    fn render_list(&mut self, el: &Element, bullet: char, delim: char) -> String {
        let ordered = el.local() == "ol";
        let mut out = String::new();
        let start = el.attr("start").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
        for (index, li) in (start..).zip(el.child_elements().filter(|c| c.local() == "li")) {
            let marker = if ordered { format!("{index}{delim} ") } else { format!("{bullet} ") };
            let content = self.render_item_content(&li.children);
            if content.trim().is_empty() {
                out.push_str(marker.trim_end());
            } else {
                out.push_str(&indent_item(content.trim_end(), &marker));
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    fn render_task_list(&mut self, el: &Element, bullet: char) -> String {
        let mut out = String::new();
        for task in el.child_elements().filter(|c| c.local() == "task") {
            let done = task
                .child_local("task-status")
                .map(|s| s.text().trim().eq_ignore_ascii_case("complete"))
                .unwrap_or(false);
            let body = task
                .child_local("task-body")
                .map(|b| self.render_item_content(&b.children))
                .unwrap_or_default();
            let marker = format!("{bullet} [{}]", if done { 'x' } else { ' ' });
            let body = body.trim_end();
            if body.trim().is_empty() {
                out.push_str(&marker);
            } else {
                out.push_str(&indent_item(body, &format!("{marker} ")));
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    /// The blocks inside one list item / task body, joined so that nested lists
    /// hug their parent item and paragraphs stay loose.
    fn render_item_content(&mut self, nodes: &[Node]) -> String {
        let parts = self.blocks_in(nodes);
        let mut out = String::new();
        for (i, (is_list, text)) in parts.iter().enumerate() {
            if i > 0 {
                out.push_str(if *is_list { "\n" } else { "\n\n" });
            }
            out.push_str(text);
        }
        out
    }

    fn render_children_blocks(&mut self, nodes: &[Node]) -> String {
        self.blocks_in(nodes).into_iter().map(|(_, t)| t).collect::<Vec<_>>().join("\n\n")
    }

    /// Split a run of children into block-level chunks, gathering loose inline
    /// content into implicit paragraphs.
    fn blocks_in(&mut self, nodes: &[Node]) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        let mut pending: Vec<Node> = Vec::new();
        for node in nodes {
            let block_el = match node {
                Node::Element(el) if is_block_element(el) => Some(el),
                _ => None,
            };
            let Some(el) = block_el else {
                pending.push(node.clone());
                continue;
            };
            self.flush_inline(&mut pending, &mut out);
            let is_list = matches!(el.local(), "ul" | "ol" | "task-list");
            let text = match el.local() {
                "ul" => self.render_list(el, '-', '.'),
                "ol" => self.render_list(el, '1', '.'),
                "task-list" => self.render_task_list(el, '-'),
                _ => self.render_element(el, ""),
            };
            if !text.trim().is_empty() {
                out.push((is_list, text));
            }
        }
        self.flush_inline(&mut pending, &mut out);
        out
    }

    fn flush_inline(&mut self, pending: &mut Vec<Node>, out: &mut Vec<(bool, String)>) {
        if pending.is_empty() {
            return;
        }
        let text = self.inline(pending);
        pending.clear();
        let text = trim_hard_breaks(text.trim());
        if !text.trim().is_empty() {
            out.push((false, escape_line_starts(&text)));
        }
    }

    // -- tables ------------------------------------------------------------

    fn render_table(&mut self, el: &Element) -> String {
        let rows = collect_rows(el);
        let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if rows.is_empty() || width == 0 {
            return String::new();
        }
        let mut cells: Vec<Vec<String>> = Vec::new();
        for row in &rows {
            let mut line: Vec<String> = row
                .iter()
                .map(|c| {
                    let text = self.inline(&c.children);
                    let text = text.trim().replace("\\\n", "<br/>").replace('\n', " ");
                    let text = text.replace('|', "\\|");
                    if text.is_empty() {
                        " ".to_string()
                    } else {
                        text
                    }
                })
                .collect();
            line.resize(width, " ".to_string());
            cells.push(line);
        }
        format_table(&cells, width)
    }

    // -- inline ------------------------------------------------------------

    fn inline(&mut self, nodes: &[Node]) -> String {
        let mut out = String::new();
        for n in nodes {
            self.inline_node(n, &mut out);
        }
        out
    }

    fn inline_node(&mut self, node: &Node, out: &mut String) {
        match node {
            Node::Text(c) => out.push_str(&escape_md(&collapse_ws(&dom::unescape(&c.raw)))),
            Node::CData(c) => out.push_str(&escape_md(&c.raw)),
            Node::Comment(_) | Node::Raw(_) => {}
            Node::Element(el) => self.inline_element(el, out),
        }
    }

    fn inline_element(&mut self, el: &Element, out: &mut String) {
        if let Some(name) = macros::macro_name(el) {
            let rendered = self.render_macro(el, name, "");
            out.push_str(rendered.trim());
            return;
        }
        match el.local() {
            "strong" | "b" => self.wrap(el, "**", "**", out),
            "em" | "i" => self.wrap(el, "*", "*", out),
            "del" | "s" | "strike" => self.wrap(el, "~~", "~~", out),
            // No CommonMark spelling; inline HTML is the honest representation
            // and survives a round trip untouched.
            "u" | "ins" => self.wrap(el, "<u>", "</u>", out),
            "sub" => self.wrap(el, "<sub>", "</sub>", out),
            "sup" => self.wrap(el, "<sup>", "</sup>", out),
            "code" | "tt" => {
                let text = el.text();
                if text.is_empty() {
                    return;
                }
                let ticks = "`".repeat(longest_run(&text, '`') + 1);
                let pad = if text.starts_with('`') || text.ends_with('`') { " " } else { "" };
                out.push_str(&format!("{ticks}{pad}{text}{pad}{ticks}"));
            }
            "br" => out.push_str("\\\n"),
            "a" => {
                let label = self.inline(&el.children);
                let href = el.attr("href").unwrap_or_default().to_string();
                let label = if label.trim().is_empty() { escape_md(&href) } else { label };
                out.push_str(&md_link(label.trim(), &href));
            }
            "img" => {
                let alt = el.attr("alt").unwrap_or_default().to_string();
                let src = el.attr("src").unwrap_or_default().to_string();
                out.push_str(&md_image(&alt, &src));
            }
            "image" => out.push_str(&self.render_image(el)),
            "link" => self.render_link(el, out),
            "emoticon" => {
                if let Some(e) = el.attr_local("name").and_then(macros::emoticon) {
                    out.push_str(e);
                }
            }
            "time" => {
                if let Some(d) = el.attr_local("datetime") {
                    out.push_str(&escape_md(d));
                }
            }
            "p" => {
                let text = self.inline(&el.children);
                out.push_str(text.trim());
            }
            // span, ac:inline-comment-marker, ac:link-body, anything else we
            // recognised: render the children and drop the wrapper. This is the
            // "unmapped inline styling is accepted lossiness" of design 03 §3.
            _ => {
                let inner = self.inline(&el.children);
                out.push_str(&inner);
            }
        }
    }

    fn wrap(&mut self, el: &Element, open: &str, close: &str, out: &mut String) {
        let inner = self.inline(&el.children);
        if inner.trim().is_empty() {
            out.push_str(&inner);
            return;
        }
        // Emphasis delimiters cannot hug whitespace, so push it outside.
        let lead: String = inner.chars().take_while(|c| c.is_whitespace()).collect();
        let trail_len = inner
            .chars()
            .rev()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum::<usize>();
        let core = &inner[lead.len()..inner.len() - trail_len];
        out.push_str(&lead);
        out.push_str(open);
        out.push_str(core);
        out.push_str(close);
        out.push_str(&inner[inner.len() - trail_len..]);
    }

    fn render_image(&mut self, el: &Element) -> String {
        let alt = el.attr_local("alt").or_else(|| el.attr_local("title")).unwrap_or("").to_string();
        let src = el
            .child_elements()
            .find_map(|c| match c.local() {
                "attachment" => Some(self.attachment_path(c.attr_local("filename").unwrap_or(""))),
                "url" => Some(c.attr_local("value").unwrap_or("").to_string()),
                _ => None,
            })
            .unwrap_or_default();
        md_image(&alt, &src)
    }

    fn attachment_path(&self, filename: &str) -> String {
        if self.opts.attachment_dir.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", self.opts.attachment_dir.trim_end_matches('/'), filename)
        }
    }

    fn render_link(&mut self, el: &Element, out: &mut String) {
        let anchor = el.attr_local("anchor").unwrap_or("").to_string();
        let body =
            el.child_elements().find(|c| matches!(c.local(), "plain-text-link-body" | "link-body"));
        let mut label = match body {
            Some(b) if b.local() == "plain-text-link-body" => escape_md(&b.text()),
            Some(b) => self.inline(&b.children),
            None => String::new(),
        };
        let label_from_body = !label.trim().is_empty();

        let target = el.child_elements().find(|c| {
            matches!(c.local(), "page" | "user" | "attachment" | "url" | "blog-post" | "space")
        });
        let mut href = match target {
            Some(t) if matches!(t.local(), "page" | "blog-post") => {
                let title = t.attr_local("content-title").unwrap_or("").to_string();
                let space =
                    t.attr_local("space-key").unwrap_or(self.opts.space_key.as_str()).to_string();
                let id = t.attr_local("content-id").unwrap_or("").to_string();
                if !label_from_body {
                    label = escape_md(&title);
                }
                self.page_href(&id, &title, &space)
            }
            Some(t) if t.local() == "user" => {
                let id = t
                    .attr_local("account-id")
                    .or_else(|| t.attr_local("userkey"))
                    .or_else(|| t.attr_local("username"))
                    .unwrap_or("")
                    .to_string();
                match self.opts.users.get(&id) {
                    Some(user) => {
                        if !label_from_body {
                            label = format!("@{}", escape_md(&user.display_name));
                        }
                        user.profile_url.clone()
                    }
                    None => {
                        // Nothing sensible can be written; the caller keeps the
                        // block as it was.
                        self.unresolved_user = true;
                        String::new()
                    }
                }
            }
            Some(t) if t.local() == "attachment" => {
                let file = t.attr_local("filename").unwrap_or("").to_string();
                if !label_from_body {
                    label = escape_md(&file);
                }
                self.attachment_path(&file)
            }
            Some(t) if t.local() == "url" => t.attr_local("value").unwrap_or("").to_string(),
            Some(t) if t.local() == "space" => {
                let key = t.attr_local("space-key").unwrap_or("").to_string();
                if !label_from_body {
                    label = escape_md(&key);
                }
                format!("{}/spaces/{}", self.opts.base_url.trim_end_matches('/'), key)
            }
            _ => String::new(),
        };
        if !anchor.is_empty() {
            href.push('#');
            href.push_str(&anchor.replace(' ', "-"));
        }
        if label.trim().is_empty() {
            label = escape_md(&href);
        }
        out.push_str(&md_link(label.trim(), &href));
    }

    /// A page link becomes a relative `.md` path when confed knows where that
    /// page lives locally, and an absolute site URL when it does not.
    fn page_href(&self, id: &str, title: &str, space: &str) -> String {
        let qualified = format!("{space}:{title}");
        for key in [id, title, qualified.as_str()] {
            if key.is_empty() {
                continue;
            }
            if let Some(path) = self.opts.page_links.get(key) {
                return path.clone();
            }
        }
        let base = self.opts.base_url.trim_end_matches('/');
        let title_enc = percent_encode_path(title);
        if space.is_empty() {
            format!("{base}/{title_enc}")
        } else {
            format!("{base}/display/{space}/{title_enc}")
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wrap raw storage bytes in a fence that their own content cannot close early.
/// The body is byte-identical to the source subtree — that is the whole point
/// of a preserved block.
pub fn preserved_fence(raw: &str) -> String {
    let fence = "`".repeat(fence_len(raw));
    format!("{fence}confluence\n{raw}\n{fence}")
}

fn fence_len(body: &str) -> usize {
    (longest_run(body, '`') + 1).max(3)
}

fn longest_run(s: &str, c: char) -> usize {
    let mut best = 0;
    let mut run = 0;
    for ch in s.chars() {
        if ch == c {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

fn is_block_element(el: &Element) -> bool {
    if let Some(name) = macros::macro_name(el) {
        return name != "status";
    }
    matches!(
        el.local(),
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "table"
            | "blockquote"
            | "hr"
            | "div"
            | "task-list"
    )
}

fn collect_rows(table: &Element) -> Vec<Vec<Element>> {
    let mut rows = Vec::new();
    collect_rows_into(table, &mut rows);
    rows
}

fn collect_rows_into(el: &Element, rows: &mut Vec<Vec<Element>>) {
    for child in el.child_elements() {
        match child.local() {
            "tr" => rows.push(
                child
                    .child_elements()
                    .filter(|c| matches!(c.local(), "td" | "th"))
                    .cloned()
                    .collect(),
            ),
            "thead" | "tbody" | "tfoot" => collect_rows_into(child, rows),
            _ => {}
        }
    }
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// Escape the characters that would otherwise be read as Markdown syntax.
///
/// Deliberately conservative: `_` inside a word and a lone `~` are left alone,
/// because escaping them turns ordinary prose (`snake_case`, `~5 min`) into
/// line noise and neither is emphasis in that position.
pub fn escape_md(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let escape = match c {
            '\\' | '`' | '*' | '[' | ']' | '<' => true,
            '_' => {
                let prev_word = i > 0 && chars[i - 1].is_alphanumeric();
                let next_word = chars.get(i + 1).is_some_and(|c| c.is_alphanumeric());
                !(prev_word && next_word)
            }
            '~' => chars.get(i + 1) == Some(&'~'),
            // Only when it looks like an entity comrak would decode.
            '&' => looks_like_entity(&chars[i + 1..]),
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn looks_like_entity(rest: &[char]) -> bool {
    let mut n = 0;
    let mut iter = rest.iter();
    let mut first = iter.next();
    if first == Some(&'#') {
        first = iter.next();
    }
    let mut cur = first;
    while let Some(c) = cur {
        if *c == ';' {
            return n > 0;
        }
        if !c.is_ascii_alphanumeric() || n > 10 {
            return false;
        }
        n += 1;
        cur = iter.next();
    }
    false
}

/// Neutralise leading characters that would turn a paragraph line into a
/// heading, list item, quote or thematic break.
fn escape_line_starts(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let trimmed = line.trim_start();
            let indent = &line[..line.len() - trimmed.len()];
            let escaped = match trimmed.chars().next() {
                Some(c @ ('#' | '>' | '-' | '+' | '=')) => {
                    format!("\\{c}{}", &trimmed[c.len_utf8()..])
                }
                Some(c) if c.is_ascii_digit() => {
                    let digits: String =
                        trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
                    let rest = &trimmed[digits.len()..];
                    if rest.starts_with('.') || rest.starts_with(')') {
                        format!("{digits}\\{rest}")
                    } else {
                        trimmed.to_string()
                    }
                }
                _ => trimmed.to_string(),
            };
            format!("{indent}{escaped}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn prefix_lines(text: &str, prefix: &str, blank_prefix: &str) -> String {
    text.split('\n')
        .map(
            |l| {
                if l.trim().is_empty() {
                    blank_prefix.to_string()
                } else {
                    format!("{prefix}{l}")
                }
            },
        )
        .collect::<Vec<_>>()
        .join("\n")
}

/// Prefix the first line with `marker` and indent the rest to line up under it.
fn indent_item(content: &str, marker: &str) -> String {
    let pad = " ".repeat(marker.chars().count());
    let mut out = String::new();
    for (i, line) in content.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if i == 0 {
            out.push_str(marker);
            out.push_str(line);
        } else if !line.trim().is_empty() {
            out.push_str(&pad);
            out.push_str(line);
        }
    }
    out
}

fn squeeze_blank_lines(text: &str) -> String {
    text.split('\n').filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join("\n")
}

fn trim_hard_breaks(text: &str) -> String {
    text.trim_end_matches('\n').trim_end_matches('\\').to_string()
}

fn md_link(label: &str, href: &str) -> String {
    format!("[{}]({})", label, wrap_destination(href))
}

fn md_image(alt: &str, src: &str) -> String {
    format!("![{}]({})", escape_md(alt), wrap_destination(src))
}

/// Markdown link destinations cannot contain spaces or parentheses unless they
/// are wrapped in angle brackets.
fn wrap_destination(href: &str) -> String {
    if href.contains(char::is_whitespace) || href.contains('(') || href.contains(')') {
        format!("<{}>", href.replace('<', "%3C").replace('>', "%3E"))
    } else {
        href.to_string()
    }
}

fn percent_encode_path(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(storage: &str) -> String {
        let opts = ConvertOptions { attachment_dir: ".page".into(), ..Default::default() };
        crate::storage_to_markdown(storage, &opts).unwrap().markdown
    }

    #[test]
    fn headings_and_emphasis() {
        assert_eq!(md("<h2>Title</h2>"), "## Title\n");
        assert_eq!(md("<p><strong>a</strong> <em>b</em> <del>c</del></p>"), "**a** *b* ~~c~~\n");
        assert_eq!(
            md("<p><u>u</u><sub>s</sub><sup>p</sup></p>"),
            "<u>u</u><sub>s</sub><sup>p</sup>\n"
        );
    }

    #[test]
    fn emphasis_does_not_hug_whitespace() {
        assert_eq!(md("<p><strong> bold </strong>text</p>"), "**bold** text\n");
    }

    #[test]
    fn entities_are_expanded() {
        assert_eq!(md("<p>a&nbsp;&mdash;&nbsp;b</p>"), "a \u{2014} b\n");
    }

    #[test]
    fn hr_and_br() {
        assert_eq!(md("<hr/>"), "---\n");
        assert_eq!(md("<p>a<br/>b</p>"), "a\\\nb\n");
    }

    #[test]
    fn nested_lists() {
        assert_eq!(md("<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>"), "- a\n  - b\n- c\n");
    }

    #[test]
    fn ordered_lists_number_up() {
        assert_eq!(md("<ol><li>a</li><li>b</li></ol>"), "1. a\n2. b\n");
    }

    #[test]
    fn adjacent_lists_alternate_markers_so_they_stay_separate_blocks() {
        assert_eq!(md("<ul><li>a</li></ul><ul><li>b</li></ul>"), "- a\n\n* b\n");
    }

    #[test]
    fn task_lists() {
        let out = md(concat!(
            "<ac:task-list>",
            "<ac:task><ac:task-status>complete</ac:task-status><ac:task-body>done</ac:task-body></ac:task>",
            "<ac:task><ac:task-status>incomplete</ac:task-status><ac:task-body>todo</ac:task-body></ac:task>",
            "</ac:task-list>"
        ));
        assert_eq!(out, "- [x] done\n- [ ] todo\n");
    }

    #[test]
    fn code_macro_becomes_a_fence() {
        let out = md(
            r#"<ac:structured-macro ac:name="code"><ac:parameter ac:name="language">rust</ac:parameter><ac:plain-text-body><![CDATA[fn a() {}]]></ac:plain-text-body></ac:structured-macro>"#,
        );
        assert_eq!(out, "```rust\nfn a() {}\n```\n");
    }

    #[test]
    fn code_body_with_backticks_gets_a_longer_fence() {
        let out = md(
            r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[a ``` b]]></ac:plain-text-body></ac:structured-macro>"#,
        );
        assert_eq!(out, "````\na ``` b\n````\n");
    }

    #[test]
    fn admonitions_become_alerts() {
        let out = md(
            r#"<ac:structured-macro ac:name="warning"><ac:parameter ac:name="title">Careful</ac:parameter><ac:rich-text-body><p>Do not.</p></ac:rich-text-body></ac:structured-macro>"#,
        );
        assert_eq!(out, "> [!WARNING]\n> **Careful**\n>\n> Do not.\n");
    }

    #[test]
    fn simple_table_becomes_gfm() {
        let out = md("<table><tbody><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></tbody></table>");
        assert_eq!(out, "| A   | B   |\n| --- | --- |\n| 1   | 2   |\n");
    }

    #[test]
    fn colspan_table_is_preserved_verbatim() {
        let src = r#"<table><tbody><tr><td colspan="2">wide</td></tr></tbody></table>"#;
        assert_eq!(md(src), format!("```confluence\n{src}\n```\n"));
    }

    #[test]
    fn unknown_macro_is_preserved_verbatim() {
        let src = r#"<ac:structured-macro ac:name="jira" ac:macro-id="7f1a"><ac:parameter ac:name="key">PROJ-142</ac:parameter></ac:structured-macro>"#;
        assert_eq!(md(src), format!("```confluence\n{src}\n```\n"));
    }

    #[test]
    fn attachment_image_uses_the_attachment_dir() {
        assert_eq!(
            md(r#"<ac:image ac:alt="Chart"><ri:attachment ri:filename="chart.png"/></ac:image>"#),
            "![Chart](.page/chart.png)\n"
        );
    }

    #[test]
    fn image_with_width_falls_back_to_preserved() {
        let src = r#"<ac:image ac:width="300"><ri:attachment ri:filename="c.png"/></ac:image>"#;
        assert_eq!(md(src), format!("```confluence\n{src}\n```\n"));
    }

    #[test]
    fn emoticons_render_as_emoji() {
        assert_eq!(md(r#"<p>ok <ac:emoticon ac:name="thumbs-up"/></p>"#), "ok \u{1f44d}\n");
    }

    #[test]
    fn empty_paragraphs_are_dropped_but_their_bytes_are_not() {
        let storage = "<p>a</p><p></p><p>b</p>";
        let c = crate::storage_to_markdown(storage, &ConvertOptions::default()).unwrap();
        assert_eq!(c.markdown, "a\n\nb\n");
        assert_eq!(c.block_map.len(), 2);
    }

    #[test]
    fn block_map_invariants_hold() {
        let storage = "<h1>T</h1><p>a</p><ul><li>x</li></ul><hr/>";
        let c = crate::storage_to_markdown(storage, &ConvertOptions::default()).unwrap();
        c.block_map.validate_spans(storage.len()).unwrap();
        c.block_map.validate_md_spans().unwrap();
        for (i, b) in c.block_map.blocks.iter().enumerate() {
            let text = c.block_map.block_markdown(i, &c.markdown).unwrap();
            assert_eq!(b.hash, hash_text(&text));
            assert_eq!(
                &storage[b.storage_span.0..b.storage_span.1].len(),
                &(b.storage_span.1 - b.storage_span.0)
            );
        }
    }

    #[test]
    fn markdown_metacharacters_in_text_are_escaped() {
        assert_eq!(md("<p>a * b [d] &lt;e&gt;</p>"), "a \\* b \\[d\\] \\<e>\n");
        assert_eq!(md("<p>snake_case_name</p>"), "snake_case_name\n");
    }

    #[test]
    fn a_paragraph_that_starts_like_a_list_is_escaped() {
        assert_eq!(md("<p>- not a list</p>"), "\\- not a list\n");
        assert_eq!(md("<p>1. not ordered</p>"), "1\\. not ordered\n");
    }

    #[test]
    fn expand_becomes_a_single_html_block() {
        let out = md(
            r#"<ac:structured-macro ac:name="expand"><ac:parameter ac:name="title">More</ac:parameter><ac:rich-text-body><p>hidden</p></ac:rich-text-body></ac:structured-macro>"#,
        );
        assert_eq!(out, "<details><summary>More</summary>\nhidden\n</details>\n");
    }

    #[test]
    fn page_links_resolve_to_relative_paths_when_known() {
        let mut opts = ConvertOptions::default();
        opts.page_links.insert("Runbook".into(), "runbook.md".into());
        let src = r#"<p><ac:link><ri:page ri:content-title="Runbook"/><ac:plain-text-link-body><![CDATA[the runbook]]></ac:plain-text-link-body></ac:link></p>"#;
        let out = crate::storage_to_markdown(src, &opts).unwrap().markdown;
        assert_eq!(out, "[the runbook](runbook.md)\n");
    }

    #[test]
    fn unresolvable_page_links_fall_back_to_an_absolute_url() {
        let opts =
            ConvertOptions { base_url: "https://wiki.example.com".into(), ..Default::default() };
        let src = r#"<p><ac:link><ri:page ri:space-key="DEV" ri:content-title="Other Page"/></ac:link></p>"#;
        let out = crate::storage_to_markdown(src, &opts).unwrap().markdown;
        assert_eq!(out, "[Other Page](https://wiki.example.com/display/DEV/Other%20Page)\n");
    }
}

#[cfg(test)]
mod user_mention_tests {
    use crate::{ConvertOptions, UserLink};
    use std::collections::HashMap;

    fn opts_with_user() -> ConvertOptions {
        let mut users = HashMap::new();
        users.insert(
            "6cb6d404f61e0043d34f805b8eca16d6".to_string(),
            UserLink {
                display_name: "Alice Ng".into(),
                profile_url: "https://wiki.corp/display/~alice.ng".into(),
                id_attr: "userkey".into(),
                id_value: "6cb6d404f61e0043d34f805b8eca16d6".into(),
            },
        );
        ConvertOptions { base_url: "https://wiki.corp".into(), users, ..Default::default() }
    }

    const MENTION: &str = "<p>Ask <ac:link>\
        <ri:user ri:userkey=\"6cb6d404f61e0043d34f805b8eca16d6\"/>\
        </ac:link> about it.</p>";

    #[test]
    fn a_resolved_mention_becomes_a_named_profile_link() {
        let md = crate::storage_to_markdown(MENTION, &opts_with_user()).unwrap().markdown;
        assert_eq!(md.trim(), "Ask [@Alice Ng](https://wiki.corp/display/~alice.ng) about it.");
        assert!(!md.contains("6cb6d404"), "the opaque key is not shown to the reader");
    }

    #[test]
    fn an_unresolved_mention_preserves_its_block_instead_of_guessing() {
        let md = crate::storage_to_markdown(MENTION, &ConvertOptions::default()).unwrap().markdown;
        assert!(md.contains("```confluence"), "got: {md}");
        assert!(md.contains("ri:userkey=\"6cb6d404f61e0043d34f805b8eca16d6\""));
        assert!(!md.contains("display/~6cb6d404"), "no link to a profile that does not exist");
    }

    #[test]
    fn an_edited_mention_goes_back_as_the_element_it_came_from() {
        let opts = opts_with_user();
        let edited = "Please ask [@Alice Ng](https://wiki.corp/display/~alice.ng) first.";
        let storage = crate::markdown_to_storage(edited, &opts).unwrap();

        assert!(
            storage.contains(
                r#"<ac:link><ri:user ri:userkey="6cb6d404f61e0043d34f805b8eca16d6" /></ac:link>"#
            ),
            "a mention must not degrade into a plain link: {storage}"
        );
    }

    #[test]
    fn an_account_id_mention_round_trips_through_its_own_attribute() {
        let mut users = HashMap::new();
        users.insert(
            "557058:abc".to_string(),
            UserLink {
                display_name: "Bob Kaur".into(),
                profile_url: "https://acme.atlassian.net/wiki/people/557058:abc".into(),
                id_attr: "account-id".into(),
                id_value: "557058:abc".into(),
            },
        );
        let opts = ConvertOptions { users, ..Default::default() };

        let storage = "<p>cc <ac:link><ri:user ri:account-id=\"557058:abc\"/></ac:link></p>";
        let md = crate::storage_to_markdown(storage, &opts).unwrap().markdown;
        assert!(md.contains("[@Bob Kaur](https://acme.atlassian.net/wiki/people/557058:abc)"));

        let back = crate::markdown_to_storage(&md, &opts).unwrap();
        assert!(back.contains(r#"ri:account-id="557058:abc""#), "got: {back}");
    }

    #[test]
    fn an_explicit_link_body_still_wins_over_the_display_name() {
        let storage = "<p><ac:link><ri:user ri:userkey=\"6cb6d404f61e0043d34f805b8eca16d6\"/>\
            <ac:plain-text-link-body><![CDATA[our reviewer]]></ac:plain-text-link-body></ac:link></p>";
        let md = crate::storage_to_markdown(storage, &opts_with_user()).unwrap().markdown;
        assert!(md.contains("[our reviewer](https://wiki.corp/display/~alice.ng)"), "got: {md}");
    }
}

/// Widest column confed will pad to.
///
/// Aligning to a very long cell would push every other row out to the same
/// length, which is harder to read than leaving that column ragged — and makes
/// the diff of a one-word edit span the whole table.
const MAX_PADDED_WIDTH: usize = 60;

/// Lay a table out with its columns lined up.
///
/// Alignment is cosmetic to Markdown but not to the person reading the file, and
/// these files are read far more often in an editor than rendered.
fn format_table(cells: &[Vec<String>], width: usize) -> String {
    let widths: Vec<usize> = (0..width)
        .map(|column| {
            let widest = cells
                .iter()
                .filter_map(|row| row.get(column))
                .map(|cell| display_width(cell))
                .max()
                .unwrap_or(1);
            // Three, because the separator row is at least `---`.
            widest.clamp(3, MAX_PADDED_WIDTH)
        })
        .collect();

    let mut out = String::new();
    write_row(&mut out, &cells[0], &widths);
    out.push('|');
    for target in &widths {
        out.push_str(&format!(" {} |", "-".repeat(*target)));
    }
    out.push('\n');
    for row in &cells[1..] {
        write_row(&mut out, row, &widths);
    }
    out.trim_end().to_string()
}

fn write_row(out: &mut String, row: &[String], widths: &[usize]) {
    out.push('|');
    for (column, target) in widths.iter().enumerate() {
        let cell = row.get(column).map(String::as_str).unwrap_or(" ").trim_end();
        let padding = target.saturating_sub(display_width(cell));
        out.push_str(&format!(" {cell}{} |", " ".repeat(padding)));
    }
    out.push('\n');
}

/// Columns a string occupies in a fixed-width terminal or editor, so a table
/// containing wide characters still lines up.
fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod table_layout_tests {
    use super::*;
    use crate::ConvertOptions;

    fn render(storage: &str) -> String {
        crate::storage_to_markdown(storage, &ConvertOptions::default()).unwrap().markdown
    }

    #[test]
    fn columns_line_up() {
        let storage = "<table><tbody>\
            <tr><th>Name</th><th>Owner</th></tr>\
            <tr><td>Onboarding</td><td>HR</td></tr>\
            <tr><td>DB</td><td>Platform</td></tr>\
            </tbody></table>";

        assert_eq!(
            render(storage).trim_end(),
            "\
| Name       | Owner    |
| ---------- | -------- |
| Onboarding | HR       |
| DB         | Platform |"
        );
    }

    #[test]
    fn a_very_wide_column_is_left_ragged() {
        let long = "x".repeat(200);
        let storage = format!(
            "<table><tbody><tr><th>A</th></tr><tr><td>{long}</td></tr>\
             <tr><td>short</td></tr></tbody></table>"
        );
        let md = render(&storage);

        // The short row is padded to the cap, not out to 200 columns.
        let shortest = md.lines().map(|l| l.chars().count()).min().unwrap();
        assert!(shortest <= MAX_PADDED_WIDTH + 4, "rows blew out to {shortest} columns:\n{md}");
        assert!(md.contains(&long), "the long cell is still intact");
    }

    #[test]
    fn wide_characters_are_measured_by_the_space_they_take() {
        // Each CJK character occupies two columns, so a naive char count would
        // misalign these rows against the ASCII one.
        let storage = "<table><tbody><tr><th>Key</th><th>Name</th></tr>\
            <tr><td>jp</td><td>日本語</td></tr>\
            <tr><td>en</td><td>English</td></tr></tbody></table>";
        let md = render(storage);

        let widths: Vec<usize> = md.lines().map(display_width).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every row should be the same width:\n{md}"
        );
    }

    #[test]
    fn an_empty_cell_still_holds_its_column_open() {
        let storage = "<table><tbody><tr><th>A</th><th>B</th></tr>\
            <tr><td></td><td>filled</td></tr></tbody></table>";
        let md = render(storage);
        for line in md.lines() {
            assert_eq!(line.matches('|').count(), 3, "row lost a column: {line}");
        }
    }

    #[test]
    fn the_separator_row_matches_the_column_widths() {
        let md =
            render("<table><tbody><tr><th>Column</th></tr><tr><td>v</td></tr></tbody></table>");
        let lines: Vec<&str> = md.lines().collect();
        assert_eq!(display_width(lines[0]), display_width(lines[1]));
        assert!(lines[1].contains("------"), "got {}", lines[1]);
    }

    #[test]
    fn an_aligned_table_still_parses_back_to_the_same_table() {
        let storage = "<table><tbody><tr><th>Name</th><th>Owner</th></tr>\
            <tr><td>Onboarding</td><td>HR</td></tr></tbody></table>";
        let md = render(storage);
        let back = crate::markdown_to_storage(&md, &ConvertOptions::default()).unwrap();

        assert!(back.contains("<table>"), "got {back}");
        assert!(back.contains("Onboarding"));
        // Padding is layout, not content: it must not survive into the cells.
        assert!(!back.contains("Onboarding "), "padding leaked into the cell: {back}");
    }
}
