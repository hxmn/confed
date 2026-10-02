//! Inline comment marks: the layer that shows a page's open inline threads in
//! the Markdown body (design 06).
//!
//! A mark is a pair of HTML comments around the commented span:
//!
//! ```markdown
//! Complete your <!--c 77120 Alice Ng: Link the template?-->first week checklist<!--/c 77120--> today.
//! ```
//!
//! Marks are a *layer*, not content. [`strip`] takes them out and everything
//! that hashes, diffs, merges or uploads a body works on the stripped text;
//! [`apply`] puts them back. Because a mark contains no newline, stripping never
//! moves a line, so block line ranges are the same with and without the layer.

use std::collections::HashMap;

use similar::{Algorithm, DiffOp, TextDiff};

/// Longest preview a mark carries. Long enough to act on, short enough to read
/// past.
pub const PREVIEW_CHARS: usize = 60;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MarkId {
    /// A comment that exists on the server, by its comment id.
    Comment(String),
    /// A draft the user wrote into the body; `push` creates it.
    New,
}

impl MarkId {
    pub fn as_str(&self) -> &str {
        match self {
            MarkId::Comment(id) => id,
            MarkId::New => "new",
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, MarkId::New)
    }
}

/// A mark found in a body, with offsets into the *stripped* text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub id: MarkId,
    /// Byte offset of the span's start in the stripped body.
    pub start: usize,
    /// Byte offset of the span's end; `None` for an opener that was never closed.
    pub end: Option<usize>,
    /// The span's current text (empty for an unterminated mark).
    pub text: String,
    /// For a `new` mark, the draft comment body; for others, the preview.
    pub note: String,
    /// 1-based line of the opener within the body.
    pub line: usize,
}

impl Mark {
    pub fn draft_body(&self) -> Option<&str> {
        match self.id {
            MarkId::New => Some(self.note.trim()).filter(|s| !s.is_empty()),
            MarkId::Comment(_) => None,
        }
    }
}

/// Something wrong with the layer. A malformed mark for an existing comment is
/// harmless — the database still knows where the comment is — but a broken
/// `new` mark must stop a push rather than post a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkIssue {
    /// An opener with no matching closer.
    Unterminated { id: MarkId, line: usize },
    /// A closer with no matching opener.
    UnmatchedClose { id: MarkId, line: usize },
    /// A mark inside a fenced code block, where it is literal text.
    InFence { id: MarkId, line: usize },
}

impl MarkIssue {
    pub fn line(&self) -> usize {
        match self {
            MarkIssue::Unterminated { line, .. }
            | MarkIssue::UnmatchedClose { line, .. }
            | MarkIssue::InFence { line, .. } => *line,
        }
    }

    pub fn id(&self) -> &MarkId {
        match self {
            MarkIssue::Unterminated { id, .. }
            | MarkIssue::UnmatchedClose { id, .. }
            | MarkIssue::InFence { id, .. } => id,
        }
    }
}

impl std::fmt::Display for MarkIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MarkIssue::Unterminated { id, line } => {
                write!(f, "line {line}: `<!--c {}-->` is never closed", id.as_str())
            }
            MarkIssue::UnmatchedClose { id, line } => {
                write!(f, "line {line}: `<!--/c {}-->` has no opener", id.as_str())
            }
            MarkIssue::InFence { id, line } => write!(
                f,
                "line {line}: `<!--c {}-->` is inside a code block, where it is literal text",
                id.as_str()
            ),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stripped {
    pub body: String,
    pub marks: Vec<Mark>,
    pub issues: Vec<MarkIssue>,
}

impl Stripped {
    pub fn drafts(&self) -> impl Iterator<Item = &Mark> {
        self.marks.iter().filter(|m| m.id.is_new())
    }
}

/// A mark to write into a clean body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedMark {
    pub id: MarkId,
    pub start: usize,
    pub end: usize,
    /// Preview (or, for `new`, the draft body) written into the opener.
    pub note: String,
}

// ---------------------------------------------------------------------------
// Grammar
// ---------------------------------------------------------------------------

/// One marker, parsed from the inside of an HTML comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marker {
    Open { id: MarkId, note: String },
    Close { id: MarkId },
}

/// Parse `<!-- … -->` text (the whole comment, delimiters included).
pub fn parse_marker(comment: &str) -> Option<Marker> {
    let inner = comment.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
    let (closing, rest) = match inner.strip_prefix("/c") {
        Some(rest) => (true, rest),
        None => (false, inner.strip_prefix('c')?),
    };
    // `c` must be a whole word: `<!--comment-->` is not a mark.
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let (id_text, note) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    let id = parse_id(id_text)?;
    if closing {
        Some(Marker::Close { id })
    } else {
        Some(Marker::Open { id, note: note.to_string() })
    }
}

fn parse_id(s: &str) -> Option<MarkId> {
    if s == "new" {
        Some(MarkId::New)
    } else if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        Some(MarkId::Comment(s.to_string()))
    } else {
        None
    }
}

pub fn open_marker(id: &MarkId, note: &str) -> String {
    let note = sanitize_note(note);
    if note.is_empty() {
        format!("<!--c {}-->", id.as_str())
    } else {
        format!("<!--c {} {}-->", id.as_str(), note)
    }
}

pub fn close_marker(id: &MarkId) -> String {
    format!("<!--/c {}-->", id.as_str())
}

/// Make a note safe to sit inside an HTML comment on one line.
///
/// `--` is not allowed inside a CommonMark HTML comment (and would end it early
/// as `-->`), a leading `>` or `-` would be read as part of the delimiter, and a
/// newline would break the "marks never move lines" invariant.
pub fn sanitize_note(note: &str) -> String {
    let collapsed: String = note.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = collapsed.replace("--", "\u{2013}").replace('\\', "");
    while out.starts_with('>') || out.starts_with('-') {
        out.remove(0);
        out = out.trim_start().to_string();
    }
    while out.ends_with('-') {
        out.pop();
        out = out.trim_end().to_string();
    }
    out
}

/// The preview written into a mark for an existing comment: author, first line
/// of the body, reply count.
pub fn preview(author: Option<&str>, body_markdown: &str, replies: usize) -> String {
    let first = body_markdown.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut text = sanitize_note(first);
    if text.chars().count() > PREVIEW_CHARS {
        let cut: String = text.chars().take(PREVIEW_CHARS - 1).collect();
        text = format!("{}\u{2026}", cut.trim_end());
    }
    let mut out = String::new();
    if let Some(author) = author.map(str::trim).filter(|a| !a.is_empty()) {
        out.push_str(&sanitize_note(author));
        out.push_str(": ");
    }
    out.push_str(&text);
    if replies > 0 {
        out.push_str(&format!(" (+{replies})"));
    }
    sanitize_note(&out)
}

// ---------------------------------------------------------------------------
// Strip
// ---------------------------------------------------------------------------

/// Take the mark layer out of a body.
///
/// Fenced code blocks are left alone: a mark inside one is literal text (and is
/// reported as an issue). Inline code spans are skipped on the same principle.
pub fn strip(body: &str) -> Stripped {
    let mut out = String::with_capacity(body.len());
    let mut marks: Vec<Mark> = Vec::new();
    let mut issues = Vec::new();
    // Openers awaiting their closer. Numeric ids may repeat (a comment split
    // across blocks), so the latest opener wins; `new` nests by a stack.
    let mut pending: HashMap<MarkId, Vec<(usize, usize, String)>> = HashMap::new();

    let mut fence: Option<(char, usize)> = None;
    for (index, line) in body.split_inclusive('\n').enumerate() {
        let line_no = index + 1;
        let content = line.trim_end_matches(['\n', '\r']);
        let unquoted = strip_quote_prefix(content);
        if let Some((ch, len)) = fence {
            if is_fence_close(unquoted, ch, len) {
                fence = None;
            }
            report_marks_in_fence(content, line_no, &mut issues);
            out.push_str(line);
            continue;
        }
        if let Some(open) = fence_open(unquoted) {
            fence = Some(open);
            out.push_str(line);
            continue;
        }
        strip_line(line, line_no, &mut out, &mut marks, &mut issues, &mut pending);
    }

    for (id, opens) in pending {
        for (start, line, note) in opens {
            issues.push(MarkIssue::Unterminated { id: id.clone(), line });
            marks.push(Mark { id: id.clone(), start, end: None, text: String::new(), note, line });
        }
    }
    marks.sort_by_key(|m| (m.start, m.end.unwrap_or(usize::MAX)));
    issues.sort_by_key(MarkIssue::line);
    Stripped { body: out, marks, issues }
}

fn strip_line(
    line: &str,
    line_no: usize,
    out: &mut String,
    marks: &mut Vec<Mark>,
    issues: &mut Vec<MarkIssue>,
    pending: &mut HashMap<MarkId, Vec<(usize, usize, String)>>,
) {
    let mut rest = line;
    while !rest.is_empty() {
        // Inline code is opaque: copy a balanced backtick span through.
        if rest.starts_with('`') {
            let ticks = rest.chars().take_while(|&c| c == '`').count();
            let run = &rest[..ticks];
            if let Some(close) = rest[ticks..].find(run).map(|i| i + ticks) {
                let mut end = close + ticks;
                // The closing run must be exactly as long as the opener.
                while rest[end..].starts_with('`') {
                    end += 1;
                }
                if rest[close..end].len() == ticks {
                    out.push_str(&rest[..end]);
                    rest = &rest[end..];
                    continue;
                }
            }
            out.push_str(run);
            rest = &rest[ticks..];
            continue;
        }
        let Some(lt) = rest.find("<!--") else {
            out.push_str(rest);
            break;
        };
        // Copy everything up to the comment, but re-check that stretch for a
        // code span that would swallow the comment.
        if let Some(tick) = rest[..lt].find('`') {
            out.push_str(&rest[..tick]);
            rest = &rest[tick..];
            continue;
        }
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        let Some(gt) = rest.find("-->") else {
            out.push_str(rest);
            break;
        };
        let comment = &rest[..gt + 3];
        match parse_marker(comment) {
            Some(Marker::Open { id, note }) => {
                pending.entry(id).or_default().push((out.len(), line_no, note));
            }
            Some(Marker::Close { id }) => match pending.get_mut(&id).and_then(Vec::pop) {
                Some((start, line, note)) => {
                    let end = out.len();
                    marks.push(Mark {
                        id,
                        start,
                        end: Some(end),
                        text: out[start..end].to_string(),
                        note,
                        line,
                    });
                }
                None => issues.push(MarkIssue::UnmatchedClose { id, line: line_no }),
            },
            None => out.push_str(comment),
        }
        rest = &rest[gt + 3..];
    }
}

fn report_marks_in_fence(line: &str, line_no: usize, issues: &mut Vec<MarkIssue>) {
    let mut rest = line;
    while let Some(lt) = rest.find("<!--") {
        rest = &rest[lt..];
        let Some(gt) = rest.find("-->") else { break };
        if let Some(Marker::Open { id, .. }) = parse_marker(&rest[..gt + 3]) {
            issues.push(MarkIssue::InFence { id, line: line_no });
        }
        rest = &rest[gt + 3..];
    }
}

/// `> ` prefixes and list indentation, so a fence inside a quote or item is
/// still recognised.
fn strip_quote_prefix(line: &str) -> &str {
    let mut s = line;
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix('>') {
            s = rest;
        } else {
            return t;
        }
    }
}

fn fence_open(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = line.chars().take_while(|&c| c == first).count();
    if len < 3 {
        return None;
    }
    // A backtick fence's info string cannot contain backticks.
    if first == '`' && line[len..].contains('`') {
        return None;
    }
    Some((first, len))
}

fn is_fence_close(line: &str, ch: char, len: usize) -> bool {
    let run = line.chars().take_while(|&c| c == ch).count();
    run >= len && line[run..].trim().is_empty()
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// Write marks into a clean body. Offsets are bytes into `body`; closers sort
/// before openers at the same offset so adjacent spans do not nest by accident.
///
/// CommonMark reads a line whose content begins with `<!--` as an HTML *block*,
/// which would change how the paragraph parses, so a marker is never left at
/// the start of a line's content: at a continuation line it moves to the end
/// of the previous line, and at a block's first line it moves one character
/// in (past emphasis delimiters, or past a whole code span or image).
///
/// A mark that would land inside text [`strip`] treats as opaque — a fenced
/// block, a code span, an HTML tag — is left out: there it would be content,
/// not a mark, and would corrupt a raw ```` ```confluence ```` block.
pub fn apply(body: &str, marks: &[PlacedMark]) -> String {
    let opaque = opaque_ranges(body);
    let mut inserts: Vec<(usize, u8, String)> = Vec::with_capacity(marks.len() * 2);
    for m in marks {
        let (start, end) = (m.start.min(body.len()), m.end.min(body.len()));
        if start > end || !body.is_char_boundary(start) || !body.is_char_boundary(end) {
            continue;
        }
        // Line breaks at a span's edges belong to no span. Left in, an edge
        // after a blank line would be read as the next block's first line and
        // stepped into it — into a fence's opening backticks, say.
        let is_break = |c: char| c == '\n' || c == '\r';
        let start =
            start + (body[start..end].len() - body[start..end].trim_start_matches(is_break).len());
        let end = start + body[start..end].trim_end_matches(is_break).len();
        if start == end && m.start != m.end {
            continue;
        }
        if opaque.iter().any(|r| r.blocks(start, end)) {
            continue;
        }
        inserts.push((start, 1, open_marker(&m.id, &m.note)));
        inserts.push((end, 0, close_marker(&m.id)));
    }
    inserts.sort_by_key(|(offset, order, _)| (*offset, *order));

    let mut out =
        String::with_capacity(body.len() + inserts.iter().map(|i| i.2.len()).sum::<usize>());
    let mut cursor = 0;
    for (offset, _, text) in inserts {
        out.push_str(&body[cursor..offset]);
        out.push_str(&text);
        cursor = offset;
    }
    out.push_str(&body[cursor..]);
    normalize_line_starts(&out)
}

/// A stretch of a body a mark must not open or close inside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Opaque {
    start: usize,
    end: usize,
    /// A fenced block is opaque as a whole: a span may not enter it, and may not
    /// enclose it either. Inline constructs only forbid an edge strictly inside.
    block: bool,
}

impl Opaque {
    fn blocks(&self, start: usize, end: usize) -> bool {
        if self.block {
            start < self.end && end > self.start
        } else {
            let inside = |o: usize| self.start < o && o < self.end;
            inside(start) || inside(end)
        }
    }
}

/// Fenced blocks, code spans and HTML tags in a clean body — the text
/// [`strip`] copies through verbatim, so a mark placed there would never come
/// back out.
fn opaque_ranges(body: &str) -> Vec<Opaque> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize, usize)> = None; // (char, len, start offset)
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let unquoted = strip_quote_prefix(content);
        if let Some((ch, len, start)) = fence {
            if is_fence_close(unquoted, ch, len) {
                out.push(Opaque { start, end: offset, block: true });
                fence = None;
            }
            continue;
        }
        if let Some((ch, len)) = fence_open(unquoted) {
            fence = Some((ch, len, line_start));
            continue;
        }
        inline_opaque(content, line_start, &mut out);
    }
    if let Some((_, _, start)) = fence {
        out.push(Opaque { start, end: body.len(), block: true });
    }
    out
}

/// Code spans and `<…>` tags on one line, the same way [`strip_line`] skips
/// them.
fn inline_opaque(line: &str, base: usize, out: &mut Vec<Opaque>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let ticks = line[i..].bytes().take_while(|&b| b == b'`').count();
                let run = &line[i..i + ticks];
                let mut search = i + ticks;
                let mut closed = None;
                while let Some(found) = line[search..].find(run).map(|k| k + search) {
                    let len = line[found..].bytes().take_while(|&b| b == b'`').count();
                    if len == ticks {
                        closed = Some(found + ticks);
                        break;
                    }
                    search = found + len;
                }
                match closed {
                    Some(end) => {
                        out.push(Opaque { start: base + i, end: base + end, block: false });
                        i = end;
                    }
                    None => i += ticks,
                }
            }
            b'<' if line[i + 1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?')) =>
            {
                let close = if line[i..].starts_with("<!--") {
                    line[i..].find("-->").map(|k| i + k + 3)
                } else {
                    line[i..].find('>').map(|k| i + k + 1)
                };
                match close {
                    Some(end) => {
                        out.push(Opaque { start: base + i, end: base + end, block: false });
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
}

/// Move markers off the start of any line's content (see [`apply`]). Idempotent,
/// and the identity on text with no marker in that position — so a body a user
/// wrote with a `new` mark at a line start is repaired the same way before push
/// parses it.
pub fn normalize_line_starts(body: &str) -> String {
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence: Option<(char, usize)> = None;

    for line in lines {
        let content_only = line.trim_end_matches(['\n', '\r']);
        let unquoted = strip_quote_prefix(content_only);
        if let Some((ch, len)) = in_fence {
            if is_fence_close(unquoted, ch, len) {
                in_fence = None;
            }
            out.push(line.to_string());
            continue;
        }
        if let Some(open) = fence_open(unquoted) {
            in_fence = Some(open);
            out.push(line.to_string());
            continue;
        }

        let prefix_len = content_prefix_len(content_only);
        let (markers, rest) = leading_markers(&content_only[prefix_len..]);
        if markers.is_empty() {
            out.push(line.to_string());
            continue;
        }
        let newline = &line[content_only.len()..];
        let prefix = &content_only[..prefix_len];

        // A continuation line: the markers belong at the end of the previous
        // line, before any hard-break backslash or trailing spaces.
        let prev_is_text = out.last().is_some_and(|p| {
            let t = p.trim_end_matches(['\n', '\r']);
            !t.trim().is_empty() && !t.trim_end().ends_with("-->")
        });
        if prev_is_text && prefix.trim().is_empty() {
            let prev = out.pop().unwrap();
            let prev_content = prev.trim_end_matches(['\n', '\r']);
            let prev_newline = &prev[prev_content.len()..];
            let keep = prev_content.trim_end_matches(' ');
            let keep = keep.strip_suffix('\\').unwrap_or(keep);
            let tail = &prev_content[keep.len()..];
            out.push(format!("{keep}{markers}{tail}{prev_newline}"));
            out.push(format!("{prefix}{rest}{newline}"));
            continue;
        }

        // First line of a block: step one character into the content.
        let skip = first_char_span(rest);
        if skip == 0 {
            out.push(line.to_string());
            continue;
        }
        out.push(format!("{prefix}{}{markers}{}{newline}", &rest[..skip], &rest[skip..]));
    }
    out.concat()
}

/// Undo [`normalize_line_starts`] for an opener at `start`: when it sits exactly
/// where a marker written at the start of the line's content is moved to, the
/// span it opens begins at that line start. A mark written to cover `Welcome`
/// at a paragraph's start reads `W<!--c …-->elcome`, and still means `Welcome`.
pub fn intended_start(body: &str, start: usize) -> usize {
    if start > body.len() || !body.is_char_boundary(start) {
        return start;
    }
    let line_start = body[..start].rfind('\n').map_or(0, |i| i + 1);
    let line = &body[line_start..];
    let line = &line[..line.find('\n').unwrap_or(line.len())];
    let content = line_start + content_prefix_len(line);
    if content < start
        && content + first_char_span(&body[content..line_start + line.len()]) == start
    {
        content
    } else {
        start
    }
}

/// Indentation, quote and list prefixes at the start of a line, up to where
/// the inline content begins.
fn content_prefix_len(line: &str) -> usize {
    let mut i = 0;
    loop {
        let rest = &line[i..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        i += rest.len() - trimmed.len();
        if let Some(after) = trimmed.strip_prefix('>') {
            i += 1;
            i += after.len() - after.trim_start_matches(' ').len();
            continue;
        }
        // `- `, `* `, `+ `, `1. `, `1) `, then an optional task box.
        let after_bullet = match trimmed.chars().next() {
            Some('-' | '*' | '+') if trimmed[1..].starts_with(' ') => Some(&trimmed[1..]),
            Some(c) if c.is_ascii_digit() => {
                let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
                let after = &trimmed[digits..];
                if (after.starts_with('.') || after.starts_with(')')) && after[1..].starts_with(' ')
                {
                    Some(&after[1..])
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(after) = after_bullet {
            i += trimmed.len() - after.len();
            let spaces = after.len() - after.trim_start_matches(' ').len();
            i += spaces;
            let after = &after[spaces..];
            for boxed in ["[ ] ", "[x] ", "[X] "] {
                if let Some(rest) = after.strip_prefix(boxed) {
                    i += boxed.len();
                    i += rest.len() - rest.trim_start_matches(' ').len();
                    break;
                }
            }
        }
        return i;
    }
}

/// The run of mark markers at the start of `s`, and what follows them.
fn leading_markers(s: &str) -> (String, &str) {
    let mut markers = String::new();
    let mut rest = s;
    while rest.starts_with("<!--") {
        let Some(gt) = rest.find("-->") else { break };
        let comment = &rest[..gt + 3];
        if parse_marker(comment).is_none() {
            break;
        }
        markers.push_str(comment);
        rest = &rest[gt + 3..];
    }
    (markers, rest)
}

/// How far into `s` a marker has to move to be safely inline: past emphasis
/// delimiters, past a whole code span, image or tag, then one character.
fn first_char_span(s: &str) -> usize {
    let delims = s.len() - s.trim_start_matches(['*', '_', '~']).len();
    let rest = &s[delims..];
    let first = match rest.chars().next() {
        Some(c) => c,
        None => return 0,
    };
    let unit = match first {
        '`' => {
            let ticks = rest.chars().take_while(|&c| c == '`').count();
            let run = &rest[..ticks];
            match rest[ticks..].find(run) {
                Some(i) => i + 2 * ticks,
                None => ticks,
            }
        }
        '!' if rest[1..].starts_with('[') => balanced_link(rest).unwrap_or(1),
        '<' => rest.find('>').map(|i| i + 1).unwrap_or(1),
        // A backslash escape is one unit with the character it escapes.
        '\\' => 1 + rest[1..].chars().next().map(char::len_utf8).unwrap_or(0),
        c => c.len_utf8(),
    };
    delims + unit
}

/// Length of a `![alt](src)` or `[label](href)` at the start of `s`.
fn balanced_link(s: &str) -> Option<usize> {
    let close = s.find("](")?;
    let end = s[close..].find(')')? + close + 1;
    Some(end)
}

// ---------------------------------------------------------------------------
// Placement from a sentinel-carrying render
// ---------------------------------------------------------------------------

/// First code point of the private-use range the renderer uses for sentinels.
pub const SENTINEL_BASE: u32 = 0xE000;

/// Which mark a sentinel stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sentinel {
    pub id: MarkId,
    pub open: bool,
    pub note: String,
}

pub fn sentinel_char(index: usize) -> char {
    char::from_u32(SENTINEL_BASE + index as u32).unwrap_or('\u{E000}')
}

fn sentinel_index(c: char) -> Option<usize> {
    let code = c as u32;
    (SENTINEL_BASE..SENTINEL_BASE + 0x1000).contains(&code).then(|| (code - SENTINEL_BASE) as usize)
}

/// Place marks into `canonical`, the block rendered without marks, using
/// `with_sentinels`, the same block rendered with a sentinel character wherever
/// a marker opened or closed.
///
/// The two renders can differ in shape around a marker (whitespace pushed out
/// of emphasis, a leading `-` escaped), so sentinel offsets are mapped onto the
/// canonical text by aligning the two. The result always strips back to
/// `canonical` exactly; marks that would change how the block parses are
/// nudged outside emphasis delimiters, and dropped if that does not help.
pub fn place(canonical: &str, with_sentinels: &str, table: &[Sentinel]) -> String {
    // 1. Take the sentinels out, remembering where each one was.
    let mut plain = String::with_capacity(with_sentinels.len());
    let mut found: Vec<(usize, usize)> = Vec::new(); // (offset in `plain`, sentinel index)
    for c in with_sentinels.chars() {
        match sentinel_index(c) {
            Some(i) if i < table.len() => found.push((plain.len(), i)),
            _ => plain.push(c),
        }
    }
    if found.is_empty() {
        return canonical.to_string();
    }

    // 2. Map offsets in `plain` to offsets in `canonical`.
    let mapped: Vec<(usize, usize)> =
        found.iter().map(|&(off, i)| (map_offset(&plain, canonical, off), i)).collect();

    // 3. Fold sentinels into spans: first opener and last closer per id.
    let mut spans: Vec<PlacedMark> = Vec::new();
    for &(off, i) in &mapped {
        let s = &table[i];
        match spans.iter_mut().find(|p| p.id == s.id) {
            Some(existing) => {
                if s.open {
                    existing.start = existing.start.min(off);
                    if existing.note.is_empty() {
                        existing.note = s.note.clone();
                    }
                } else {
                    existing.end = existing.end.max(off);
                }
            }
            None => spans.push(PlacedMark {
                id: s.id.clone(),
                start: off,
                end: off,
                note: if s.open { s.note.clone() } else { String::new() },
            }),
        }
    }
    spans.retain(|p| p.end > p.start);
    if spans.is_empty() {
        return canonical.to_string();
    }

    // 4. Insert, checking that the block still reads the same.
    let shape = inline_shape(canonical);
    let attempt = apply(canonical, &spans);
    if inline_shape(&attempt) == shape {
        return attempt;
    }
    let nudged: Vec<PlacedMark> = spans.iter().map(|p| nudge(canonical, p)).collect();
    let attempt = apply(canonical, &nudged);
    if inline_shape(&attempt) == shape {
        return attempt;
    }
    // Keep whichever marks survive on their own.
    let kept: Vec<PlacedMark> = nudged
        .into_iter()
        .filter(|p| inline_shape(&apply(canonical, std::slice::from_ref(p))) == shape)
        .collect();
    let attempt = apply(canonical, &kept);
    if inline_shape(&attempt) == shape {
        attempt
    } else {
        canonical.to_string()
    }
}

/// Where `offset` in `from` lands in `to`, by character alignment.
fn map_offset(from: &str, to: &str, offset: usize) -> usize {
    if from == to {
        return offset;
    }
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).diff_chars(from, to);
    let from_chars: Vec<&str> = diff.old_slices().to_vec();
    let to_chars: Vec<&str> = diff.new_slices().to_vec();
    let byte_at = |chars: &[&str], n: usize| {
        chars[..n.min(chars.len())].iter().map(|c| c.len()).sum::<usize>()
    };
    // The character index in `from` at `offset`.
    let mut acc = 0;
    let mut from_idx = from_chars.len();
    for (i, c) in from_chars.iter().enumerate() {
        if acc >= offset {
            from_idx = i;
            break;
        }
        acc += c.len();
    }

    // Characters `to` has that `from` does not (an escaping backslash, emphasis
    // delimiters) belong with what follows them: a sentinel at that boundary
    // goes before the insertion, not after.
    let mut insert_start: Option<usize> = None;
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, new_index, len } => {
                if from_idx < old_index + len {
                    if from_idx == old_index {
                        if let Some(at) = insert_start {
                            return byte_at(&to_chars, at);
                        }
                    }
                    return byte_at(&to_chars, new_index + from_idx.saturating_sub(old_index));
                }
                insert_start = None;
            }
            DiffOp::Delete { old_index, old_len, new_index } => {
                if from_idx < old_index + old_len {
                    return byte_at(&to_chars, new_index);
                }
                insert_start = None;
            }
            DiffOp::Insert { new_index, .. } => {
                insert_start.get_or_insert(new_index);
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                if from_idx < old_index + old_len {
                    // Inside a replaced stretch: land at its start if the
                    // sentinel was at its start, otherwise at its end.
                    let at = if from_idx == old_index { new_index } else { new_index + new_len };
                    return byte_at(&to_chars, at);
                }
                insert_start = None;
            }
        }
    }
    to.len()
}

/// Move a span's edges outside any emphasis delimiter run they touch.
fn nudge(text: &str, p: &PlacedMark) -> PlacedMark {
    let is_delim = |c: char| matches!(c, '*' | '_' | '~');
    let mut start = p.start;
    while start > 0 {
        let prev = text[..start].chars().next_back().unwrap();
        if is_delim(prev) {
            start -= prev.len_utf8();
        } else {
            break;
        }
    }
    let mut end = p.end;
    while let Some(next) = text[end..].chars().next() {
        if is_delim(next) {
            end += next.len_utf8();
        } else {
            break;
        }
    }
    PlacedMark { id: p.id.clone(), start, end, note: p.note.clone() }
}

/// The text a reader sees for a stretch of inline Markdown — what Confluence
/// matches a `textSelection` against.
pub fn plain_text(markdown: &str) -> String {
    use comrak::nodes::NodeValue;
    let arena = comrak::Arena::new();
    let root = crate::mdblock::parse(&arena, markdown);
    let mut out = String::new();
    fn walk<'a>(node: &'a comrak::nodes::AstNode<'a>, out: &mut String) {
        match &node.data.borrow().value {
            NodeValue::Text(t) => out.push_str(t),
            NodeValue::Code(c) => out.push_str(&c.literal),
            NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
            NodeValue::HtmlInline(_) | NodeValue::HtmlBlock(_) => {}
            _ => {
                for child in node.children() {
                    walk(child, out);
                }
            }
        }
    }
    walk(root, &mut out);
    out
}

/// A fingerprint of how a block parses, ignoring marks.
fn inline_shape(markdown: &str) -> String {
    use comrak::nodes::NodeValue;
    let arena = comrak::Arena::new();
    let root = crate::mdblock::parse(&arena, markdown);
    let mut tokens: Vec<String> = Vec::new();
    fn walk<'a>(node: &'a comrak::nodes::AstNode<'a>, tokens: &mut Vec<String>) {
        let data = node.data.borrow();
        match &data.value {
            NodeValue::HtmlInline(h) if parse_marker(h.trim()).is_some() => {}
            NodeValue::Text(t) => match tokens.last_mut() {
                Some(last) if last.starts_with("T:") => last.push_str(t),
                _ => tokens.push(format!("T:{t}")),
            },
            NodeValue::HtmlInline(h) => tokens.push(format!("H:{h}")),
            NodeValue::HtmlBlock(h) => tokens.push(format!("HB:{}", h.literal)),
            other => {
                tokens.push(format!("{:?}<", std::mem::discriminant(other)));
                for child in node.children() {
                    walk(child, tokens);
                }
                tokens.push(">".into());
            }
        }
    }
    walk(root, &mut tokens);
    tokens.join("\u{1}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str) -> MarkId {
        MarkId::Comment(id.into())
    }

    #[test]
    fn markers_parse_and_print() {
        assert_eq!(
            parse_marker("<!--c 77120 Alice: hi-->"),
            Some(Marker::Open { id: c("77120"), note: "Alice: hi".into() })
        );
        assert_eq!(
            parse_marker("<!-- c 1 -->"),
            Some(Marker::Open { id: c("1"), note: String::new() })
        );
        assert_eq!(parse_marker("<!--/c 1-->"), Some(Marker::Close { id: c("1") }));
        assert_eq!(
            parse_marker("<!--c new Is this right?-->"),
            Some(Marker::Open { id: MarkId::New, note: "Is this right?".into() })
        );
        assert_eq!(parse_marker("<!--/c new-->"), Some(Marker::Close { id: MarkId::New }));
        assert_eq!(parse_marker("<!--comment-->"), None);
        assert_eq!(parse_marker("<!--c is for cookie-->"), None);
        assert_eq!(parse_marker("<!-- confed:toc -->"), None);
        assert_eq!(open_marker(&c("1"), ""), "<!--c 1-->");
        assert_eq!(open_marker(&c("1"), "note"), "<!--c 1 note-->");
        assert_eq!(close_marker(&MarkId::New), "<!--/c new-->");
    }

    #[test]
    fn strip_returns_the_clean_text_and_the_spans() {
        let body = "Complete your <!--c 77120 Alice: Link it?-->first week checklist<!--/c 77120--> today.\n";
        let s = strip(body);
        assert_eq!(s.body, "Complete your first week checklist today.\n");
        assert_eq!(s.marks.len(), 1);
        let m = &s.marks[0];
        assert_eq!(m.id, c("77120"));
        assert_eq!(m.text, "first week checklist");
        assert_eq!(&s.body[m.start..m.end.unwrap()], "first week checklist");
        assert_eq!(m.note, "Alice: Link it?");
        assert_eq!(m.line, 1);
        assert!(s.issues.is_empty());
    }

    #[test]
    fn strip_is_the_identity_on_a_body_without_marks() {
        let body = "# Title\n\nSome <!-- ordinary --> comment and `code`.\n\n```\nfenced\n```\n";
        let s = strip(body);
        assert_eq!(s.body, body);
        assert!(s.marks.is_empty());
        assert!(s.issues.is_empty());
    }

    #[test]
    fn new_marks_carry_their_draft_body() {
        let s = strip("The <!--c new Is this still right?-->platform team<!--/c new--> owns it.\n");
        assert_eq!(s.body, "The platform team owns it.\n");
        let d = s.drafts().next().unwrap();
        assert_eq!(d.draft_body(), Some("Is this still right?"));
        assert_eq!(d.text, "platform team");
    }

    #[test]
    fn overlapping_spans_are_resolved_by_id() {
        let s = strip("<!--c 1-->aa <!--c 2-->bb<!--/c 1--> cc<!--/c 2-->\n");
        assert_eq!(s.body, "aa bb cc\n");
        assert_eq!(s.marks[0].text, "aa bb");
        assert_eq!(s.marks[1].text, "bb cc");
    }

    #[test]
    fn nested_new_marks_match_innermost_first() {
        let s = strip("<!--c new outer-->a <!--c new inner-->b<!--/c new--> c<!--/c new-->\n");
        assert_eq!(s.body, "a b c\n");
        let texts: Vec<&str> = s.marks.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["a b c", "b"]);
    }

    #[test]
    fn a_split_comment_yields_one_mark_per_fragment() {
        let s = strip("<!--c 5 note-->para one<!--/c 5-->\n\n<!--c 5-->para two<!--/c 5-->\n");
        assert_eq!(s.marks.len(), 2);
        assert!(s.marks.iter().all(|m| m.id == c("5")));
        assert_eq!(s.marks[1].line, 3);
    }

    #[test]
    fn broken_marks_are_reported_not_fatal() {
        let s = strip("<!--c 1-->never closed\n\nclosed twice<!--/c 2-->\n");
        assert_eq!(s.body, "never closed\n\nclosed twice\n");
        assert_eq!(
            s.issues,
            vec![
                MarkIssue::Unterminated { id: c("1"), line: 1 },
                MarkIssue::UnmatchedClose { id: c("2"), line: 3 },
            ]
        );
        assert_eq!(s.marks.len(), 1, "an unterminated mark is still reported");
        assert_eq!(s.marks[0].end, None);
    }

    #[test]
    fn fences_and_code_spans_are_opaque() {
        let body =
            "```\n<!--c 1-->literal<!--/c 1-->\n```\n\nand `<!--c 2-->x<!--/c 2-->` inline\n";
        let s = strip(body);
        assert_eq!(s.body, body);
        assert!(s.marks.is_empty());
        assert_eq!(s.issues, vec![MarkIssue::InFence { id: c("1"), line: 2 }]);
    }

    #[test]
    fn a_fence_inside_a_quote_is_still_a_fence() {
        let body = "> ```\n> <!--c 1-->x<!--/c 1-->\n> ```\n";
        assert_eq!(strip(body).body, body);
    }

    #[test]
    fn apply_then_strip_round_trips() {
        let clean = "Complete your first week checklist today.\n";
        let placed = vec![PlacedMark { id: c("7"), start: 14, end: 34, note: "A: hi".into() }];
        let marked = apply(clean, &placed);
        assert_eq!(
            marked,
            "Complete your <!--c 7 A: hi-->first week checklist<!--/c 7--> today.\n"
        );
        let s = strip(&marked);
        assert_eq!(s.body, clean);
        assert_eq!(s.marks[0].start, 14);
        assert_eq!(s.marks[0].end, Some(34));
    }

    #[test]
    fn apply_never_writes_into_a_fence_a_code_span_or_a_tag() {
        let m = |start, end| PlacedMark { id: c("1"), start, end, note: String::new() };
        let raw = "x\n\n```confluence\n<p>(<code>Time</code>)</p>\n```\n";
        let inside = raw.find("Time").unwrap();
        assert_eq!(apply(raw, &[m(inside, inside + 4)]), raw, "inside a fence");
        assert_eq!(apply(raw, &[m(0, raw.len())]), raw, "around a fence");

        let code = "a `b c` d\n";
        assert_eq!(apply(code, &[m(3, 5)]), code, "inside a code span");
        assert_eq!(apply(code, &[m(2, 7)]), "a <!--c 1-->`b c`<!--/c 1--> d\n", "around one");

        let tag = "a <span class=\"x\">b</span> c\n";
        assert_eq!(apply(tag, &[m(5, 12)]), tag, "inside a tag");
        assert_eq!(strip(&apply(tag, &[m(18, 19)])).body, tag, "between tags is fine");
        assert_eq!(apply(tag, &[m(18, 19)]).matches("<!--c 1-->").count(), 1);
    }

    #[test]
    fn whatever_apply_writes_strips_back_to_the_clean_body() {
        // Every offset pair on a body with each opaque construct: the layer
        // must always come back out, leaving fences byte-identical.
        let body = "Intro `co de` and <b>bold</b>.\n\n```confluence\n<p>(<code>x</code>)</p>\n```\n\n- tail\n";
        let offsets: Vec<usize> = (0..=body.len()).filter(|&i| body.is_char_boundary(i)).collect();
        for &start in &offsets {
            for &end in offsets.iter().filter(|&&e| e > start) {
                let marked =
                    apply(body, &[PlacedMark { id: c("1"), start, end, note: "n".into() }]);
                let s = strip(&marked);
                assert_eq!(s.body, body, "{start}..{end}: {marked}");
                assert!(s.issues.is_empty(), "{start}..{end}: {marked}");
            }
        }
    }

    #[test]
    fn adjacent_spans_close_before_the_next_opens() {
        let clean = "xab\n";
        let placed = vec![
            PlacedMark { id: c("1"), start: 1, end: 2, note: String::new() },
            PlacedMark { id: c("2"), start: 2, end: 3, note: String::new() },
        ];
        assert_eq!(apply(clean, &placed), "x<!--c 1-->a<!--/c 1--><!--c 2-->b<!--/c 2-->\n");
    }

    #[test]
    fn notes_are_made_safe_for_a_comment() {
        assert_eq!(sanitize_note("a -- b\nc"), "a \u{2013} b c");
        assert_eq!(sanitize_note("> quoted"), "quoted");
        assert_eq!(sanitize_note("ends -"), "ends");
        assert_eq!(sanitize_note("back\\slash"), "backslash");
    }

    #[test]
    fn previews_are_short_and_attributed() {
        assert_eq!(
            preview(Some("Alice Ng"), "Link the template?\n\nMore.", 0),
            "Alice Ng: Link the template?"
        );
        assert_eq!(preview(None, "x", 2), "x (+2)");
        let long = "word ".repeat(40);
        let p = preview(Some("A"), &long, 0);
        assert!(p.chars().count() <= PREVIEW_CHARS + 4, "{p}");
        assert!(p.ends_with('\u{2026}'));
    }

    fn table(id: &str, note: &str) -> Vec<Sentinel> {
        vec![
            Sentinel { id: c(id), open: true, note: note.into() },
            Sentinel { id: c(id), open: false, note: String::new() },
        ]
    }

    #[test]
    fn placement_maps_sentinels_onto_the_canonical_text() {
        let t = table("1", "n");
        let canonical = "Complete your first week checklist today.";
        let with = "Complete your \u{E000}first week checklist\u{E001} today.";
        assert_eq!(
            place(canonical, with, &t),
            "Complete your <!--c 1 n-->first week checklist<!--/c 1--> today."
        );
    }

    #[test]
    fn placement_survives_whitespace_pushed_out_of_emphasis() {
        let t = table("1", "");
        // The marker sat inside <strong> around " bold "; the canonical render
        // moved the spaces outside the delimiters.
        let canonical = "a **bold** b";
        let with = "a **\u{E000} bold \u{E001}** b";
        let placed = place(canonical, with, &t);
        assert_eq!(strip(&placed).body, canonical);
        assert!(placed.contains("<!--c 1-->"), "{placed}");
    }

    #[test]
    fn placement_survives_an_escaped_line_start() {
        let t = table("1", "");
        let canonical = "\\- not a list";
        let with = "\u{E000}- not a list\u{E001}";
        let placed = place(canonical, with, &t);
        assert_eq!(strip(&placed).body, canonical);
        assert_eq!(placed, "\\-<!--c 1--> not a list<!--/c 1-->");
    }

    #[test]
    fn a_mark_that_would_break_emphasis_is_nudged_outside_it() {
        let t = table("1", "");
        let canonical = "**bold**text";
        let with = "**\u{E000}bold\u{E001}**text";
        let placed = place(canonical, with, &t);
        assert_eq!(strip(&placed).body, canonical);
        assert_eq!(placed, "**b<!--c 1-->old**<!--/c 1-->text");
    }

    #[test]
    fn fragments_of_one_comment_become_one_span() {
        let t = table("1", "n");
        let canonical = "**a b** c";
        let with = "**\u{E000}a b\u{E001}**\u{E000} c\u{E001}";
        assert_eq!(place(canonical, with, &t), "**<!--c 1 n-->a b** c<!--/c 1-->");
    }

    #[test]
    fn placement_inside_a_link_label_is_fine() {
        let t = table("1", "");
        let canonical = "see [the docs](https://x.test) now";
        let with = "see [the \u{E000}docs\u{E001}](https://x.test) now";
        assert_eq!(
            place(canonical, with, &t),
            "see [the <!--c 1-->docs<!--/c 1-->](https://x.test) now"
        );
    }

    #[test]
    fn a_marker_never_starts_a_line() {
        // At a block's first line it steps one character in.
        assert_eq!(
            apply(
                "first week\n",
                &[PlacedMark { id: c("1"), start: 0, end: 10, note: String::new() }]
            ),
            "f<!--c 1-->irst week<!--/c 1-->\n"
        );
        // Past emphasis delimiters, so the emphasis still parses.
        assert_eq!(
            apply(
                "**bold** x\n",
                &[PlacedMark { id: c("1"), start: 0, end: 8, note: String::new() }]
            ),
            "**b<!--c 1-->old**<!--/c 1--> x\n"
        );
        // Past a whole code span.
        assert_eq!(
            apply(
                "`code` x\n",
                &[PlacedMark { id: c("1"), start: 0, end: 6, note: String::new() }]
            ),
            "`code`<!--c 1--><!--/c 1--> x\n"
        );
        // In a list item the content starts after the bullet.
        assert_eq!(
            apply("- item\n", &[PlacedMark { id: c("1"), start: 2, end: 6, note: String::new() }]),
            "- i<!--c 1-->tem<!--/c 1-->\n"
        );
        // At a continuation line it goes to the end of the previous line.
        assert_eq!(
            apply(
                "one\ntwo\n",
                &[PlacedMark { id: c("1"), start: 4, end: 7, note: String::new() }]
            ),
            "one<!--c 1-->\ntwo<!--/c 1-->\n"
        );
        // …before a hard break, so the break survives.
        assert_eq!(
            apply(
                "one\\\ntwo\n",
                &[PlacedMark { id: c("1"), start: 5, end: 8, note: String::new() }]
            ),
            "one<!--c 1-->\\\ntwo<!--/c 1-->\n"
        );
        // A closer that ended at a line break moves back too.
        assert_eq!(
            apply(
                "one\ntwo\n",
                &[PlacedMark { id: c("1"), start: 1, end: 4, note: String::new() }]
            ),
            "o<!--c 1-->ne<!--/c 1-->\ntwo\n"
        );
    }

    #[test]
    fn the_intended_start_undoes_the_line_start_step() {
        let marked = apply(
            "Welcome to the team.\n",
            &[PlacedMark { id: MarkId::New, start: 0, end: 7, note: "n".into() }],
        );
        let s = strip(&marked);
        let m = &s.marks[0];
        assert_eq!(m.text, "elcome", "the mark itself sits one character in");
        assert_eq!(&s.body[intended_start(&s.body, m.start)..m.end.unwrap()], "Welcome");

        let body = "- **bold** item\n";
        assert_eq!(intended_start(body, 5), 2, "past the bullet and the delimiters");
        assert_eq!(intended_start("a Welcome\n", 3), 3, "mid-line opens stay put");
    }

    #[test]
    fn normalizing_a_user_written_draft_at_a_line_start() {
        let body = "<!--c new Is this right?-->The platform team<!--/c new--> owns it.\n";
        let fixed = normalize_line_starts(body);
        assert_eq!(fixed, "T<!--c new Is this right?-->he platform team<!--/c new--> owns it.\n");
        assert_eq!(normalize_line_starts(&fixed), fixed, "idempotent");
        assert_eq!(normalize_line_starts("plain\n"), "plain\n");
    }

    #[test]
    fn plain_text_drops_markup_and_marks() {
        assert_eq!(
            plain_text("a **bold** `code` <!--c 1-->x<!--/c 1--> [l](u)"),
            "a bold code x l"
        );
    }

    #[test]
    fn a_block_with_no_sentinels_is_returned_unchanged() {
        assert_eq!(place("x", "x", &[]), "x");
    }
}
