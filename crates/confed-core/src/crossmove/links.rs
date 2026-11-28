//! The link rewriter of a move between spaces: which `ri:page` links in a
//! page's storage the move breaks, and the same storage with those links
//! fixed. Pure — strings in, strings out, no workspace and no server — so it
//! can be read, tested and fuzzed on its own.

use std::collections::HashMap;

/// How the links in one page are read and what they should become.
pub struct LinkContext<'a> {
    /// The space a link that names none meant before the move: the page's
    /// space then.
    pub space_before: &'a str,
    /// The page's space after the move.
    pub space_after: &'a str,
    /// Old `(space, title)` → new `(space, title)`.
    pub moves: &'a HashMap<(String, String), (String, String)>,
    /// Drop `ri:space-key` from a link that ends up naming the page's own
    /// space, rather than writing that space's key.
    pub relativize: bool,
    /// Whether a page `(space, title)` exists. With it, a link is changed
    /// only when the page it meant exists and the link does not reach it
    /// already; a link that meant nothing is left as it is. Without it, links
    /// are judged by how they are written alone.
    pub exists: Option<&'a PageExists<'a>>,
}

/// Whether a page `(space, title)` exists.
pub type PageExists<'a> = dyn Fn(&str, &str) -> bool + 'a;

/// One link by title: what it reaches as written, and what it should.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkTarget {
    /// `(space, title)` the link resolves to, written as it is, from the
    /// page's space after the move.
    pub reaches: (String, String),
    /// `(space, title)` of the page it meant: what it named before the move,
    /// where that page is now.
    pub meant: (String, String),
    /// `(space, title)` it named before the move.
    pub named: (String, String),
}

/// Every link by title in `storage`, resolved as [`rewrite_links`] sees it.
pub fn link_targets(storage: &str, ctx: &LinkContext<'_>) -> Vec<LinkTarget> {
    let mut out = Vec::new();
    let mut rest = storage;
    while let Some(start) = find_page_tag(rest) {
        let tail = &rest[start..];
        let Some(end) = tag_end(tail) else { break };
        if let Some((target, _)) = resolve_tag(&tail[..end], ctx) {
            out.push(target);
        }
        rest = &tail[end..];
    }
    out
}

/// The parts of a link tag a rewrite needs: where it points and where it
/// should.
pub(super) fn resolve_tag(
    tag: &str,
    ctx: &LinkContext<'_>,
) -> Option<(LinkTarget, Option<String>)> {
    let attrs = attributes(tag);
    let title_attr = attrs.iter().find(|a| a.name == "ri:content-title")?;
    let named_space = attrs.iter().find(|a| a.name == "ri:space-key").map(|a| unescape(&a.value));
    let title = unescape(&title_attr.value);
    let before = named_space.clone().unwrap_or_else(|| ctx.space_before.to_string());
    let named = (before, title.clone());
    let meant = ctx.moves.get(&named).cloned().unwrap_or_else(|| named.clone());
    let reaches = (named_space.clone().unwrap_or_else(|| ctx.space_after.to_string()), title);
    Some((LinkTarget { reaches, meant, named }, named_space))
}

/// Rewrite the `ri:page` links in `storage` so each one still names the page
/// it named before the move. Returns the new storage and how many links
/// changed.
///
/// Only `ri:space-key` and `ri:content-title` are ever touched, and only on
/// links that would otherwise resolve to the wrong page or none. A link by
/// id (`ri:content-id`, as Cloud writes them) survives a move as it is. The
/// rest of the storage — macros, their `ac:macro-id`, and every byte between
/// the links — comes out exactly as it went in.
pub fn rewrite_links(storage: &str, ctx: &LinkContext<'_>) -> (String, usize) {
    let mut out = String::with_capacity(storage.len());
    let mut changed = 0;
    let mut rest = storage;
    while let Some(start) = find_page_tag(rest) {
        out.push_str(&rest[..start]);
        let tag_and_after = &rest[start..];
        let Some(end) = tag_end(tag_and_after) else {
            out.push_str(tag_and_after);
            return (out, changed);
        };
        let tag = &tag_and_after[..end];
        match rewrite_tag(tag, ctx) {
            Some(new) => {
                changed += 1;
                out.push_str(&new);
            }
            None => out.push_str(tag),
        }
        rest = &tag_and_after[end..];
    }
    out.push_str(rest);
    (out, changed)
}

pub(super) const PAGE_TAG: &str = "<ri:page";

/// The next `<ri:page` tag in `text` — outside CDATA sections and comments,
/// where it is text: a code sample showing how a link is stored, which must
/// stay byte for byte.
pub(super) fn find_page_tag(text: &str) -> Option<usize> {
    const OPAQUE: [(&str, &str); 2] = [("<![CDATA[", "]]>"), ("<!--", "-->")];
    let mut offset = 0;
    loop {
        let at = offset + text[offset..].find(PAGE_TAG)?;
        // A CDATA section or comment that opens before it, and is not closed
        // by then, hides it: skip past its end and look again.
        let hidden = OPAQUE
            .iter()
            .filter_map(|(open, close)| {
                let start = offset + text[offset..at].find(open)?;
                Some((start, open.len(), close))
            })
            .min_by_key(|(start, ..)| *start);
        if let Some((start, open_len, close)) = hidden {
            let body = start + open_len;
            offset = body + text[body..].find(close)? + close.len();
            continue;
        }
        let next = text[at + PAGE_TAG.len()..].chars().next();
        if matches!(next, Some(c) if c.is_whitespace() || c == '/' || c == '>') {
            return Some(at);
        }
        offset = at + PAGE_TAG.len();
    }
}

/// The length of the tag at the start of `text`, through its `>`.
pub(super) fn tag_end(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in text.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(i + 1),
            None => {}
        }
    }
    None
}

/// One attribute of a tag: where it is (with its leading whitespace) and its
/// raw value.
pub(super) struct Attr {
    name: String,
    value: String,
    quote: char,
    span: std::ops::Range<usize>,
    value_span: std::ops::Range<usize>,
}

pub(super) fn attributes(tag: &str) -> Vec<Attr> {
    let bytes: Vec<(usize, char)> = tag.char_indices().collect();
    let mut attrs = Vec::new();
    let mut i = PAGE_TAG.chars().count();
    while i < bytes.len() {
        let ws_start = bytes[i].0;
        while i < bytes.len() && bytes[i].1.is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || matches!(bytes[i].1, '/' | '>') {
            break;
        }
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].1.is_whitespace()
            && !matches!(bytes[i].1, '=' | '/' | '>')
        {
            i += 1;
        }
        let name: String = bytes[name_start..i].iter().map(|(_, c)| *c).collect();
        while i < bytes.len() && bytes[i].1.is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i].1 != '=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].1.is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || !matches!(bytes[i].1, '"' | '\'') {
            break;
        }
        let quote = bytes[i].1;
        i += 1;
        let value_start = i;
        while i < bytes.len() && bytes[i].1 != quote {
            i += 1;
        }
        let value_end = i;
        i += 1;
        let byte = |k: usize| bytes.get(k).map_or(tag.len(), |(b, _)| *b);
        attrs.push(Attr {
            name,
            value: tag[byte(value_start)..byte(value_end)].to_string(),
            quote,
            span: ws_start..byte(i),
            value_span: byte(value_start)..byte(value_end),
        });
    }
    attrs
}

pub(super) fn rewrite_tag(tag: &str, ctx: &LinkContext<'_>) -> Option<String> {
    let attrs = attributes(tag);
    let title_attr = attrs.iter().find(|a| a.name == "ri:content-title")?;
    let space_attr = attrs.iter().find(|a| a.name == "ri:space-key");
    let title = unescape(&title_attr.value);
    let (link, named_space) = resolve_tag(tag, ctx)?;
    let (target_space, target_title) = link.meant.clone();

    // Judged by what exists, when that is known: a link to nothing is left
    // as it is — rewriting it would only move where it fails — and one that
    // reaches its page already needs nothing.
    if let Some(exists) = ctx.exists {
        if !exists(&target_space, &target_title) || link.reaches == link.meant {
            return None;
        }
    }

    // What the tag must say for the link to resolve to the target from here.
    let wanted_space: Option<String> = if target_space == ctx.space_after {
        match &named_space {
            Some(_) if ctx.relativize => None,
            Some(_) => Some(target_space),
            None => None,
        }
    } else {
        Some(target_space)
    };
    if wanted_space == named_space && target_title == title {
        return None;
    }

    // Edit from the end, so earlier spans stay valid.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    if target_title != title {
        edits.push((title_attr.value_span.clone(), escape(&target_title, title_attr.quote)));
    }
    match (space_attr, &wanted_space) {
        (Some(attr), Some(space)) if Some(space) != named_space.as_ref() => {
            edits.push((attr.value_span.clone(), escape(space, attr.quote)));
        }
        (Some(attr), None) => edits.push((attr.span.clone(), String::new())),
        (None, Some(space)) => {
            let at = PAGE_TAG.len();
            edits.push((at..at, format!(r#" ri:space-key="{}""#, escape(space, '"'))));
        }
        _ => {}
    }
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut out = tag.to_string();
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    Some(out)
}

pub(super) fn escape(value: &str, quote: char) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote == '"' => out.push_str("&quot;"),
            '\'' if quote == '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

pub(super) fn unescape(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(end) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;

    /// Storage-ish text: tags, page links in their spellings, CDATA, comments,
    /// entities, and noise — whatever a page might hold, and some it should not.
    fn storage() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                3 => "[a-zA-Z .]{0,8}",
                2 => Just(r#"<ac:link><ri:page ri:content-title="Onboarding" /></ac:link>"#.to_string()),
                1 => Just(r#"<ri:page ri:space-key="DOCS" ri:content-title="Onboarding"/>"#.to_string()),
                1 => Just(r#"<ri:page ri:content-title='Team &amp; Handbook'>"#.to_string()),
                1 => Just("<![CDATA[<ri:page ri:content-title=\"Onboarding\" />]]>".to_string()),
                1 => Just("<!-- <ri:page ri:content-title=\"Onboarding\" /> -->".to_string()),
                1 => Just("<ri:page".to_string()),
                1 => Just("<![CDATA[".to_string()),
                1 => Just("&#x41;&bogus;&".to_string()),
                1 => "[<>\"'=/&;]{1,4}",
            ],
            0..10,
        )
        .prop_map(|parts| parts.concat())
    }

    fn rewrite(storage: &str) -> (String, usize) {
        let moves: HashMap<(String, String), (String, String)> = [(
            ("DOCS".to_string(), "Onboarding".to_string()),
            ("ENG".to_string(), "Onboarding".to_string()),
        )]
        .into();
        let ctx = LinkContext {
            space_before: "DOCS",
            space_after: "DOCS",
            moves: &moves,
            relativize: false,
            exists: None,
        };
        rewrite_links(storage, &ctx)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

        /// Whatever it is given, the rewriter returns, changes only what it
        /// counts, and leaves nothing for a second pass to change.
        #[test]
        fn the_rewriter_is_total_and_idempotent(storage in storage()) {
            let (once, n) = rewrite(&storage);
            if n == 0 {
                prop_assert_eq!(&once, &storage);
            }
            let (twice, again) = rewrite(&once);
            prop_assert_eq!(again, 0, "a second pass changed {}:\n{}", again, once);
            prop_assert_eq!(twice, once);
        }
    }
}
