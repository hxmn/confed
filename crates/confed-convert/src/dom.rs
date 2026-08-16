//! A deliberately lenient XHTML-fragment parser.
//!
//! Confluence storage format is *not* a well-formed XML document: it is a
//! fragment with no root element, it uses the `ac:` / `ri:` namespace prefixes
//! without ever declaring them, and it happily contains HTML entities
//! (`&nbsp;`, `&mdash;`) that no DTD in the payload defines. A strict parser
//! rejects all of that, so we run `quick-xml` in its most forgiving mode and do
//! entity handling ourselves.
//!
//! Every node keeps the exact byte range it occupies in the source. Those spans
//! are the foundation of the whole "only regenerate what changed" guarantee:
//! a block we do not touch is re-emitted as `&storage[span.0..span.1]`.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::{ConvertError, ConvertResult};

#[derive(Clone, Debug)]
pub enum Node {
    Element(Element),
    /// Character data exactly as it appeared in the source (still escaped).
    Text(Chars),
    /// `<![CDATA[ ... ]]>`; `raw` is the *inner* text, `span` covers the markers.
    CData(Chars),
    Comment(Chars),
    /// Processing instructions, doctypes, XML declarations: kept verbatim.
    Raw(Chars),
}

impl Node {
    pub fn span(&self) -> (usize, usize) {
        match self {
            Node::Element(e) => e.span,
            Node::Text(c) | Node::CData(c) | Node::Comment(c) | Node::Raw(c) => c.span,
        }
    }

    pub fn as_element(&self) -> Option<&Element> {
        match self {
            Node::Element(e) => Some(e),
            _ => None,
        }
    }

    /// True when this node carries no visible content (whitespace-only text,
    /// comments). Used to decide whether a top-level node is a real block.
    pub fn is_blank(&self) -> bool {
        match self {
            Node::Text(c) => unescape(&c.raw).trim().is_empty(),
            Node::CData(c) => c.raw.trim().is_empty(),
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Chars {
    pub span: (usize, usize),
    pub raw: String,
}

#[derive(Clone, Debug)]
pub struct Element {
    /// Qualified name as written, e.g. `ac:structured-macro`.
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub span: (usize, usize),
}

impl Element {
    /// Name without its namespace prefix.
    pub fn local(&self) -> &str {
        match self.name.split_once(':') {
            Some((_, local)) => local,
            None => &self.name,
        }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Attribute lookup ignoring the namespace prefix (`ac:name` matches `name`).
    pub fn attr_local(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == local || k.split_once(':').map(|(_, l)| l) == Some(local))
            .map(|(_, v)| v.as_str())
    }

    pub fn child_elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(Node::as_element)
    }

    /// First descendant-of-depth-1 element with the given qualified name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.child_elements().find(|e| e.name == name)
    }

    pub fn child_local(&self, local: &str) -> Option<&Element> {
        self.child_elements().find(|e| e.local() == local)
    }

    /// Concatenated, unescaped text of the whole subtree.
    pub fn text(&self) -> String {
        let mut out = String::new();
        collect_text(&self.children, &mut out);
        out
    }
}

fn collect_text(nodes: &[Node], out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(c) => out.push_str(&unescape(&c.raw)),
            Node::CData(c) => out.push_str(&c.raw),
            Node::Element(e) => collect_text(&e.children, out),
            _ => {}
        }
    }
}

/// HTML void elements: storage format is *usually* XHTML (`<br/>`), but pages
/// imported from elsewhere contain bare `<br>`. Treat them as self-closing so a
/// stray one cannot swallow the rest of the document.
///
/// Only unprefixed names count — `<ac:link>` is a container that happens to be
/// spelled like the HTML void element `<link>`.
fn is_void(qname: &str) -> bool {
    !qname.contains(':')
        && matches!(
            qname,
            "br" | "hr" | "img" | "col" | "input" | "meta" | "link" | "area" | "base" | "wbr"
        )
}

/// Parse a storage fragment into a node forest with exact byte spans.
///
/// This never fails on undeclared entities or namespace prefixes; the only way
/// it errors is a truly unreadable byte stream.
pub fn parse_fragment(src: &str) -> ConvertResult<Vec<Node>> {
    let mut reader = Reader::from_str(src);
    {
        let cfg = reader.config_mut();
        cfg.check_end_names = false;
        cfg.allow_unmatched_ends = true;
        cfg.allow_dangling_amp = true;
        cfg.check_comments = false;
        cfg.expand_empty_elements = false;
        cfg.trim_text(false);
    }

    let mut stack: Vec<Element> = Vec::new();
    let mut roots: Vec<Node> = Vec::new();

    loop {
        let start = reader.buffer_position() as usize;
        let event =
            reader.read_event().map_err(|e| ConvertError::Parse(format!("byte {start}: {e}")))?;
        let end = reader.buffer_position() as usize;

        match event {
            Event::Eof => break,
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let attrs = decode_attrs(&e);
                let el = Element { name, attrs, children: Vec::new(), span: (start, end) };
                if is_void(&el.name) {
                    push(&mut stack, &mut roots, Node::Element(el));
                } else {
                    stack.push(el);
                }
            }
            Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let attrs = decode_attrs(&e);
                let el = Element { name, attrs, children: Vec::new(), span: (start, end) };
                push(&mut stack, &mut roots, Node::Element(el));
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match stack.iter().rposition(|el| el.name == name) {
                    Some(idx) => {
                        // Auto-close anything left open inside it.
                        while stack.len() > idx + 1 {
                            let mut inner = stack.pop().expect("len > idx + 1");
                            inner.span.1 = start;
                            push(&mut stack, &mut roots, Node::Element(inner));
                        }
                        let mut el = stack.pop().expect("rposition found it");
                        el.span.1 = end;
                        push(&mut stack, &mut roots, Node::Element(el));
                    }
                    // A close tag with nothing open: keep the bytes as text so
                    // spans still reconstruct the document.
                    None => push(
                        &mut stack,
                        &mut roots,
                        Node::Text(Chars { span: (start, end), raw: src[start..end].to_string() }),
                    ),
                }
            }
            Event::Text(_) | Event::GeneralRef(_) => {
                let chars = Chars { span: (start, end), raw: src[start..end].to_string() };
                push_text(&mut stack, &mut roots, chars);
            }
            Event::CData(_) => {
                let inner = src[start..end]
                    .strip_prefix("<![CDATA[")
                    .and_then(|s| s.strip_suffix("]]>"))
                    .unwrap_or("")
                    .to_string();
                push(&mut stack, &mut roots, Node::CData(Chars { span: (start, end), raw: inner }));
            }
            Event::Comment(_) => {
                let raw = src[start..end].to_string();
                push(&mut stack, &mut roots, Node::Comment(Chars { span: (start, end), raw }));
            }
            _ => {
                let raw = src[start..end].to_string();
                push(&mut stack, &mut roots, Node::Raw(Chars { span: (start, end), raw }));
            }
        }
    }

    // Unclosed elements run to the end of the fragment.
    while let Some(mut el) = stack.pop() {
        el.span.1 = src.len();
        push(&mut stack, &mut roots, Node::Element(el));
    }

    Ok(roots)
}

fn push(stack: &mut [Element], roots: &mut Vec<Node>, node: Node) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(node),
        None => roots.push(node),
    }
}

/// Text and entity-reference events arrive separately; stitch adjacent ones back
/// into a single node so callers see one run of characters.
fn push_text(stack: &mut [Element], roots: &mut Vec<Node>, chars: Chars) {
    let siblings = match stack.last_mut() {
        Some(parent) => &mut parent.children,
        None => &mut *roots,
    };
    if let Some(Node::Text(prev)) = siblings.last_mut() {
        if prev.span.1 == chars.span.0 {
            prev.span.1 = chars.span.1;
            prev.raw.push_str(&chars.raw);
            return;
        }
    }
    siblings.push(Node::Text(chars));
}

fn decode_attrs(e: &quick_xml::events::BytesStart<'_>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut attrs = e.attributes();
    attrs.with_checks(false);
    for a in attrs.flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let value = unescape(&String::from_utf8_lossy(a.value.as_ref()));
        out.push((key, value));
    }
    out
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// Named entities Confluence emits that XML does not define. Anything not in
/// here (and not numeric) is left literal rather than treated as an error —
/// losing an `&` is worse than showing one.
const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    // `&nbsp;` becomes a plain space: a non-breaking space is invisible in
    // Markdown and trips up every diff and hash we compute.
    ("nbsp", " "),
    ("ensp", " "),
    ("emsp", " "),
    ("thinsp", " "),
    ("shy", ""),
    ("mdash", "\u{2014}"),
    ("ndash", "\u{2013}"),
    ("hellip", "\u{2026}"),
    ("lsquo", "\u{2018}"),
    ("rsquo", "\u{2019}"),
    ("ldquo", "\u{201c}"),
    ("rdquo", "\u{201d}"),
    ("sbquo", "\u{201a}"),
    ("bdquo", "\u{201e}"),
    ("dagger", "\u{2020}"),
    ("Dagger", "\u{2021}"),
    ("bull", "\u{2022}"),
    ("middot", "\u{b7}"),
    ("copy", "\u{a9}"),
    ("reg", "\u{ae}"),
    ("trade", "\u{2122}"),
    ("deg", "\u{b0}"),
    ("plusmn", "\u{b1}"),
    ("times", "\u{d7}"),
    ("divide", "\u{f7}"),
    ("frac12", "\u{bd}"),
    ("frac14", "\u{bc}"),
    ("frac34", "\u{be}"),
    ("sup2", "\u{b2}"),
    ("sup3", "\u{b3}"),
    ("micro", "\u{b5}"),
    ("para", "\u{b6}"),
    ("sect", "\u{a7}"),
    ("laquo", "\u{ab}"),
    ("raquo", "\u{bb}"),
    ("euro", "\u{20ac}"),
    ("pound", "\u{a3}"),
    ("yen", "\u{a5}"),
    ("cent", "\u{a2}"),
    ("larr", "\u{2190}"),
    ("uarr", "\u{2191}"),
    ("rarr", "\u{2192}"),
    ("darr", "\u{2193}"),
    ("harr", "\u{2194}"),
    ("rArr", "\u{21d2}"),
    ("hArr", "\u{21d4}"),
    ("ne", "\u{2260}"),
    ("le", "\u{2264}"),
    ("ge", "\u{2265}"),
    ("infin", "\u{221e}"),
    ("alpha", "\u{3b1}"),
    ("beta", "\u{3b2}"),
    ("gamma", "\u{3b3}"),
    ("delta", "\u{3b4}"),
    ("lambda", "\u{3bb}"),
    ("mu", "\u{3bc}"),
    ("pi", "\u{3c0}"),
    ("sigma", "\u{3c3}"),
    ("Omega", "\u{3a9}"),
    ("check", "\u{2713}"),
    ("cross", "\u{2717}"),
    ("star", "\u{2605}"),
    ("hearts", "\u{2665}"),
];

/// Lenient XML/HTML entity expansion. Unknown references survive untouched.
pub fn unescape(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let ch_len = char_len(bytes[i]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        // Longest plausible entity is `&#x10FFFF;`; cap the scan so a lone `&`
        // in prose does not cost a linear search of the rest of the document.
        // Scanning bytes (not chars) is safe because an entity name is ASCII —
        // a multi-byte character just ends the scan.
        let limit = (i + 12).min(bytes.len());
        let semi = (i + 1..limit).find(|&j| bytes[j] == b';');
        match semi {
            Some(j) => {
                let name = std::str::from_utf8(&bytes[i + 1..j]).unwrap_or("");
                match expand_entity(name) {
                    Some(text) => {
                        out.push_str(&text);
                        i = j + 1;
                    }
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn expand_entity(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Some(digits) = name.strip_prefix('#') {
        let code = match digits.strip_prefix('x').or_else(|| digits.strip_prefix('X')) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse::<u32>().ok()?,
        };
        return char::from_u32(code).map(|c| c.to_string());
    }
    NAMED_ENTITIES.iter().find(|(k, _)| *k == name).map(|(_, v)| (*v).to_string())
}

/// Escape text for insertion into storage-format character data.
pub fn escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape text for insertion into a storage-format attribute value.
pub fn escape_attr(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Well-formedness gate for anything we are about to hand to Confluence.
///
/// Strict about tag balance (that is the failure mode that corrupts a page),
/// deliberately permissive about undeclared prefixes and entities (storage
/// format is full of both and they are not errors on the server).
pub fn check_well_formed(fragment: &str) -> Result<(), String> {
    let mut reader = Reader::from_str(fragment);
    {
        let cfg = reader.config_mut();
        cfg.check_end_names = true;
        cfg.allow_unmatched_ends = false;
        cfg.allow_dangling_amp = true;
        cfg.check_comments = false;
        cfg.expand_empty_elements = false;
        cfg.trim_text(false);
    }
    let mut depth = 0usize;
    loop {
        let pos = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if !is_void(&String::from_utf8_lossy(e.name().as_ref())) {
                    depth += 1;
                }
            }
            Ok(Event::End(_)) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("unexpected closing tag at byte {pos}"))?;
            }
            Ok(_) => {}
            Err(e) => return Err(format!("byte {pos}: {e}")),
        }
    }
    if depth != 0 {
        return Err(format!("{depth} unclosed element(s)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_reconstruct(src: &str) {
        let nodes = parse_fragment(src).unwrap();
        let mut cursor = 0usize;
        let mut rebuilt = String::new();
        for n in &nodes {
            let (s, e) = n.span();
            assert!(s >= cursor, "span {s}..{e} goes backwards in {src:?}");
            rebuilt.push_str(&src[cursor..s]);
            rebuilt.push_str(&src[s..e]);
            cursor = e;
        }
        rebuilt.push_str(&src[cursor..]);
        assert_eq!(rebuilt, src, "spans do not reconstruct {src:?}");
    }

    #[test]
    fn spans_are_exact() {
        for src in [
            "<p>hello</p>",
            "  <p>a</p>\n<p>b</p>  ",
            "<p>a &nbsp; b &mdash; c</p>",
            "<ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[x]]></ac:plain-text-body></ac:structured-macro>",
            "text at top level",
            "<hr/><p>after</p>",
            "<!-- a comment --><p>x</p>",
            "<p>unclosed",
            "</stray><p>ok</p>",
            "<br><p>void start tag</p>",
        ] {
            spans_reconstruct(src);
        }
    }

    #[test]
    fn undeclared_prefixes_and_entities_parse() {
        let src = "<ac:layout><ri:thing ri:x=\"1\"/>&nbsp;&unknown;</ac:layout>";
        let nodes = parse_fragment(src).unwrap();
        assert_eq!(nodes.len(), 1);
        let el = nodes[0].as_element().unwrap();
        assert_eq!(el.name, "ac:layout");
        assert_eq!(el.text(), " &unknown;");
    }

    #[test]
    fn attributes_keep_prefixes_and_unescape() {
        let nodes = parse_fragment(r#"<ri:attachment ri:filename="a &amp; b.png"/>"#).unwrap();
        let el = nodes[0].as_element().unwrap();
        assert_eq!(el.attr("ri:filename"), Some("a & b.png"));
        assert_eq!(el.attr_local("filename"), Some("a & b.png"));
    }

    #[test]
    fn cdata_inner_text_excludes_markers() {
        let nodes = parse_fragment("<x><![CDATA[a ]] b]]></x>").unwrap();
        let el = nodes[0].as_element().unwrap();
        assert_eq!(el.text(), "a ]] b");
    }

    #[test]
    fn unescape_is_lenient() {
        assert_eq!(unescape("a &amp; b"), "a & b");
        assert_eq!(unescape("&nbsp;x&nbsp;"), " x ");
        assert_eq!(unescape("&#65;&#x42;"), "AB");
        assert_eq!(unescape("50 &percnt; done"), "50 &percnt; done");
        assert_eq!(unescape("R&D and Q&A"), "R&D and Q&A");
        assert_eq!(unescape("&mdash;"), "\u{2014}");
        assert_eq!(unescape("no entities here"), "no entities here");
    }

    /// The entity scan looks ahead a fixed number of *bytes*; a multi-byte
    /// character landing inside that window must not be sliced through.
    #[test]
    fn unescape_never_splits_a_multibyte_character() {
        assert_eq!(unescape("Q1 &ndash; Q2 – done"), "Q1 \u{2013} Q2 – done");
        assert_eq!(unescape("R&D – 支払い"), "R&D – 支払い");
        assert_eq!(unescape("&👍;"), "&👍;");
        assert_eq!(unescape("a&b–c&d"), "a&b–c&d");
    }

    #[test]
    fn well_formedness_check() {
        assert!(check_well_formed("<p>ok</p>").is_ok());
        assert!(check_well_formed("<p>a &nbsp; b</p>").is_ok());
        assert!(check_well_formed("<ac:x ac:y=\"1\"><ri:z/></ac:x>").is_ok());
        assert!(check_well_formed("<p>oops").is_err());
        assert!(check_well_formed("<p>oops</div>").is_err());
        assert!(check_well_formed("</p>").is_err());
    }

    #[test]
    fn adjacent_text_and_entities_merge() {
        let nodes = parse_fragment("a &amp; b").unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].span(), (0, 9));
    }
}
