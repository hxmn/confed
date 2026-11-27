//! Properties over generated input, not only the fixture corpus.
//!
//! The corpus tests prove what they prove for 35 documents. The bugs that got
//! past them — a bare `<br>` that blocked every push of its page, `Vec<T>` in
//! prose sent up as an unclosed element, a heading that dropped its
//! backslashes — were all in input no fixture happened to contain. These
//! strategies build storage and Markdown out of exactly those ingredients:
//! void elements written both ways, angle brackets and backslashes in text,
//! entities, Cyrillic, nesting.

use confed_converter::{
    markdown_to_storage, markdown_to_storage_patched, storage_to_markdown, ConvertOptions,
};
use proptest::prelude::*;

fn opts() -> ConvertOptions {
    ConvertOptions { attachment_dir: ".page".into(), ..Default::default() }
}

// ------------------------------------------------------------ storage ---

/// Character data as storage carries it: escaped, with entities.
fn storage_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => "[a-zA-Z0-9 ]{1,8}",
            2 => "[а-яА-Я ]{1,6}",
            1 => Just("&amp;".to_string()),
            1 => Just("&lt;T&gt;".to_string()),
            1 => Just("&nbsp;".to_string()),
            1 => Just("&mdash;".to_string()),
            1 => Just("C:\\tmp".to_string()),
            1 => Just("*not emphasis*".to_string()),
            1 => Just("_x_".to_string()),
            1 => Just("`tick`".to_string()),
            1 => Just("# hash".to_string()),
            1 => Just("[a](b)".to_string()),
            // Punctuation at the edge of an emphasis, touching a letter.
            1 => Just("Note:".to_string()),
            1 => Just("(bar)".to_string()),
            1 => Just("注意：".to_string()),
        ],
        1..6,
    )
    .prop_map(|parts| parts.concat())
}

fn inline() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        6 => storage_text(),
        1 => Just("<br/>".to_string()),
        1 => Just("<br>".to_string()),
        1 => storage_text().prop_map(|t| format!("<code>{t}</code>")),
    ];
    leaf.prop_recursive(4, 24, 4, |inner| {
        let run = prop::collection::vec(inner, 1..4).prop_map(|v| v.concat());
        prop_oneof![
            run.clone().prop_map(|t| format!("<strong>{t}</strong>")),
            run.clone().prop_map(|t| format!("<em>{t}</em>")),
            run.clone().prop_map(|t| format!("<u>{t}</u>")),
            run.prop_map(|t| format!("<span style=\"color: red\">{t}</span>")),
        ]
    })
}

fn inline_run() -> impl Strategy<Value = String> {
    prop::collection::vec(inline(), 1..4).prop_map(|v| v.concat())
}

fn block() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        5 => inline_run().prop_map(|t| format!("<p>{t}</p>")),
        2 => (1u8..=6, inline_run()).prop_map(|(l, t)| format!("<h{l}>{t}</h{l}>")),
        1 => Just("<hr/>".to_string()),
        1 => Just("<p></p>".to_string()),
        1 => storage_text().prop_map(|t| format!(
            "<ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[{t}]]></ac:plain-text-body></ac:structured-macro>"
        )),
        1 => storage_text().prop_map(|t| format!(
            "<ac:structured-macro ac:name=\"unknown-macro\"><ac:parameter ac:name=\"x\">{t}</ac:parameter></ac:structured-macro>"
        )),
        1 => prop::collection::vec(inline_run(), 1..3).prop_map(|cells| format!(
            "<table><tbody><tr>{}</tr></tbody></table>",
            cells.iter().map(|c| format!("<td>{c}</td>")).collect::<String>()
        )),
    ];
    leaf.prop_recursive(3, 16, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inline_run(), 1..4).prop_map(|items| format!(
                "<ul>{}</ul>",
                items.iter().map(|i| format!("<li>{i}</li>")).collect::<String>()
            )),
            prop::collection::vec(inline_run(), 1..4).prop_map(|items| format!(
                "<ol>{}</ol>",
                items.iter().map(|i| format!("<li>{i}</li>")).collect::<String>()
            )),
            prop::collection::vec(inner, 1..3)
                .prop_map(|b| format!("<blockquote>{}</blockquote>", b.concat())),
        ]
    })
}

fn storage_doc() -> impl Strategy<Value = String> {
    prop::collection::vec(block(), 1..6).prop_map(|b| b.concat())
}

// ----------------------------------------------------------- markdown ---

/// A line of prose with everything CommonMark might read as syntax.
fn md_line() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => "[a-zA-Z0-9 ]{1,8}",
            2 => "[а-яА-Я ]{1,6}",
            1 => Just("Vec<T>".to_string()),
            1 => Just("a <b".to_string()),
            1 => Just("</u>".to_string()),
            1 => Just("<br>".to_string()),
            1 => Just("<u>x</u>".to_string()),
            1 => Just("C:\\tmp".to_string()),
            1 => Just("\\<T>".to_string()),
            1 => Just("**bold**".to_string()),
            1 => Just("*em*".to_string()),
            1 => Just("`code`".to_string()),
            1 => Just("&amp;".to_string()),
            1 => Just("R&D".to_string()),
            1 => Just("~~del~~".to_string()),
            1 => Just("[link](https://example.com)".to_string()),
        ],
        1..6,
    )
    // Words apart, as people type them. Glued together, emphasis tokens make
    // delimiter runs (`**a***b**c***`) that no one writes and CommonMark
    // resolves by rules no renderer can invert; elements side by side are
    // covered from the storage side, where Confluence does produce them.
    .prop_map(|parts| parts.join(" ").trim().to_string())
    // A line that is nothing but a break shows nothing, and comes back as
    // nothing: an empty item, not one holding an invisible break.
    .prop_filter("a line has text", |l| !l.is_empty() && l != "<br>")
}

fn md_block() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => md_line(),
        // A heading is one line, so a break in one is dropped by design.
        2 => (1usize..=6, md_line().prop_filter("no break", |l| !l.contains("<br>")))
            .prop_map(|(l, t)| format!("{} {t}", "#".repeat(l))),
        2 => prop::collection::vec(md_line(), 1..4)
            .prop_map(|items| items.iter().map(|i| format!("- {i}\n")).collect::<String>()),
        1 => prop::collection::vec(md_line(), 1..3)
            .prop_map(|lines| lines.iter().map(|l| format!("> {l}\n")).collect::<String>()),
        1 => md_line().prop_map(|l| format!("```\n{l}\n```")),
    ]
}

fn md_doc() -> impl Strategy<Value = String> {
    prop::collection::vec(md_block(), 1..6)
        .prop_map(|blocks| format!("{}\n", blocks.join("\n\n").trim_end()))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Any storage renders, with a block map that describes it.
    #[test]
    fn generated_storage_renders_with_a_valid_block_map(storage in storage_doc()) {
        let c = storage_to_markdown(&storage, &opts()).unwrap();
        c.block_map.validate_spans(&storage).unwrap();
        c.block_map.validate_md_spans().unwrap();
    }

    /// Pushing a page nobody edited sends back exactly what the server has —
    /// whatever the page contains.
    #[test]
    fn an_unedited_page_patches_to_itself(storage in storage_doc()) {
        let c = storage_to_markdown(&storage, &opts()).unwrap();
        let out = markdown_to_storage_patched(&storage, &c.block_map, &c.markdown, &c.markdown, &opts())
            .unwrap();
        prop_assert_eq!(out, storage);
    }

    /// An edit to a paragraph can always be pushed, whatever the blocks
    /// around it contain — a bare `<br>` elsewhere must not block it.
    #[test]
    fn an_added_paragraph_can_always_be_pushed(storage in storage_doc()) {
        let c = storage_to_markdown(&storage, &opts()).unwrap();
        let edited = format!("{}\nФраза 1.\n", c.markdown);
        let out = markdown_to_storage_patched(&storage, &c.block_map, &c.markdown, &edited, &opts());
        let out = out.unwrap();
        prop_assert!(out.contains("Фраза 1."));
    }

    /// The Markdown a page renders to always converts back.
    #[test]
    fn rendered_markdown_converts_back(storage in storage_doc()) {
        let c = storage_to_markdown(&storage, &opts()).unwrap();
        markdown_to_storage(&c.markdown, &opts()).unwrap();
    }

    /// Anything a person can type converts — no prose is an unclosed element.
    #[test]
    fn any_markdown_converts(md in md_doc()) {
        markdown_to_storage(&md, &opts()).unwrap();
    }

    /// Storage written from Markdown survives a trip down and back up: what
    /// the user wrote is what the page keeps, up to what nobody can see.
    #[test]
    fn written_storage_is_a_fixed_point(md in md_doc()) {
        let first = markdown_to_storage(&md, &opts()).unwrap();
        let down = storage_to_markdown(&first, &opts()).unwrap();
        let second = markdown_to_storage(&down.markdown, &opts()).unwrap();
        prop_assert_eq!(
            invisible_removed(&second),
            invisible_removed(&first),
            "markdown went down as:\n{}",
            down.markdown
        );
    }
}

/// What the Markdown does not carry taken out: runs of spaces collapsed
/// (rendering collapses them, as every browser does), a space at the start of
/// a line, a line break that ends its element, and so a paragraph of nothing
/// but breaks. A push keeps those
/// in the storage it patches; regenerating from Markdown, as this test does,
/// cannot.
fn invisible_removed(storage: &str) -> String {
    // Each removal can expose another (a space gone puts a break before a
    // closing tag), so they run until nothing changes.
    let mut text = collapse_spaces(storage);
    loop {
        let before = text.clone();
        for (from, to) in [
            ("<br /> ", "<br />"),
            ("<br/> ", "<br/>"),
            (" </", "</"),
            ("<br /></", "</"),
            ("<br/></", "</"),
            ("<p></p>", ""),
        ] {
            text = text.replace(from, to);
        }
        if text == before {
            return text;
        }
    }
}

/// Runs of spaces and newlines as one space: in character data, a newline is
/// shown as a space.
fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let c = if c == '\n' { ' ' } else { c };
        if c == ' ' && out.ends_with(' ') {
            continue;
        }
        out.push(c);
    }
    out
}

// ------------------------------------------------- from storage, edited ---
//
// The round trips above start from Markdown, which only produces what the
// generator writes. These start from storage as Confluence writes it, with
// the shapes that once went wrong on an edit: emphasis against punctuation,
// a panel whose body is one bold paragraph, a bullet that starts with `[x]`,
// a heading that ends in `#`. An edit regenerates a block from its Markdown,
// so each block's Markdown must convert back to the same text in the same
// elements.

/// Text as Confluence stores it, with the edges that matter.
fn edge_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => "[a-z]{1,6}",
            1 => Just("Note:".to_string()),
            1 => Just("(bar)".to_string()),
            1 => Just("注意：".to_string()),
            1 => Just("请阅读".to_string()),
            1 => Just("Фраза".to_string()),
            1 => Just("x, y".to_string()),
        ],
        1..4,
    )
    .prop_map(|parts| parts.join(" "))
}

/// A run of inline content: text and emphasis, touching or not.
fn edge_inline() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            3 => edge_text(),
            2 => edge_text().prop_map(|t| format!("<strong>{t}</strong>")),
            2 => edge_text().prop_map(|t| format!("<em>{t}</em>")),
            1 => edge_text().prop_map(|t| format!("<code>{t}</code>")),
        ],
        1..4,
    )
    .prop_map(|parts| parts.concat())
}

fn edge_block() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => edge_inline().prop_map(|t| format!("<p>{t}</p>")),
        2 => (1u8..=4, edge_text(), any::<bool>()).prop_map(|(l, t, hash)| {
            let t = if hash { format!("{t} #") } else { t };
            format!("<h{l}>{t}</h{l}>")
        }),
        2 => (prop::collection::vec(edge_inline(), 1..3), any::<bool>()).prop_map(|(items, boxed)| {
            let items: String = items
                .iter()
                .enumerate()
                .map(|(i, t)| if boxed && i == 0 { format!("<li>[x] {t}</li>") } else { format!("<li>{t}</li>") })
                .collect();
            format!("<ul>{items}</ul>")
        }),
        1 => prop::collection::vec(edge_inline(), 1..3)
            .prop_map(|items| format!("<ol>{}</ol>", items.iter().map(|t| format!("<li>{t}</li>")).collect::<String>())),
        1 => (edge_text(), any::<bool>()).prop_map(|(t, titled)| {
            let title = if titled { "<ac:parameter ac:name=\"title\">Heads up</ac:parameter>" } else { "" };
            format!(
                "<ac:structured-macro ac:name=\"info\">{title}<ac:rich-text-body><p><strong>{t}</strong></p></ac:rich-text-body></ac:structured-macro>"
            )
        }),
        1 => (edge_text(), edge_inline(), edge_inline()).prop_map(|(h, a, b)| format!(
            "<table><tbody><tr><th><p>{h}</p></th><th><p>{h}</p></th></tr><tr><td><p>{a}</p></td><td><p>{b}</p></td></tr></tbody></table>"
        )),
    ]
}

/// The text and the elements of storage, without what is not content:
/// attributes other than a macro's or parameter's name, whitespace, and the
/// spellings Markdown normalizes (`ac:schema-version`, `<b>` for `<strong>`).
fn shape(storage: &str) -> String {
    let mut out = String::new();
    let mut rest = storage;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else { break };
        let tag = &rest[open + 1..open + close];
        let (slash, tag_body) = match tag.strip_prefix('/') {
            Some(rest) => ("/", rest),
            None => ("", tag),
        };
        let name: String = format!(
            "{slash}{}",
            tag_body.chars().take_while(|c| !c.is_whitespace() && *c != '/').collect::<String>()
        );
        let named = tag.split("ac:name=\"").nth(1).and_then(|r| r.split('"').next());
        let closing = tag.ends_with('/');
        out.push('<');
        out.push_str(&name);
        if let Some(n) = named {
            out.push_str(&format!(" {n}"));
        }
        if closing {
            out.push('/');
        }
        out.push('>');
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    // Two runs of the same emphasis side by side read as one, and are merged.
    let mut out = collapse_spaces(&out).replace("> ", ">").replace(" <", "<");
    for tag in ["strong", "em", "code"] {
        out = out.replace(&format!("</{tag}><{tag}>"), "");
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Every block of storage Confluence writes, once its Markdown is
    /// regenerated — what an edit to it does — keeps its text in the same
    /// elements.
    #[test]
    fn an_edited_block_keeps_its_text_and_its_elements(block in edge_block()) {
        let c = storage_to_markdown(&block, &opts()).unwrap();
        let back = markdown_to_storage(&c.markdown, &opts()).unwrap();
        prop_assert_eq!(shape(&back), shape(&block), "markdown:\n{}", c.markdown);
    }

    /// The same through a real patch: one block of a page edited, the others
    /// copied as they were.
    #[test]
    fn editing_one_block_keeps_the_others_byte_for_byte(
        blocks in prop::collection::vec(edge_block(), 2..5),
        which in any::<prop::sample::Index>(),
    ) {
        let storage = blocks.concat();
        let c = storage_to_markdown(&storage, &opts()).unwrap();
        let edit = which.index(c.block_map.blocks.len());
        let (start, end) = c.block_map.blocks[edit].md_span;
        let lines: Vec<&str> = c.markdown.split_inclusive('\n').collect();
        // The edit adds a word at the end of the block's first line.
        let mut edited = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i == start && start < end && !line.trim().is_empty() && !line.starts_with("```") {
                edited.push_str(&format!("{} zz\n", line.trim_end_matches('\n')));
            } else {
                edited.push_str(line);
            }
        }
        let out = markdown_to_storage_patched(&storage, &c.block_map, &c.markdown, &edited, &opts()).unwrap();
        for (i, block) in c.block_map.blocks.iter().enumerate() {
            if i != edit {
                let raw = &storage[block.storage_span.0..block.storage_span.1];
                prop_assert!(out.contains(raw), "block {} changed:\n{}\n--- out ---\n{}", i, raw, out);
            }
        }
    }
}
