//! Pages nested far deeper than anyone writes them — generated, imported, or
//! hostile — must not overflow the stack. Each test runs on a test thread,
//! whose stack (2 MiB) is no larger than the async runtime's workers.

use confed_converter::blockmap::BlockKind;
use confed_converter::dom::MAX_DEPTH;
use confed_converter::{markdown_to_storage, pretty, storage_to_markdown, ConvertOptions};

fn nested(open: &str, close: &str, depth: usize, inner: &str) -> String {
    format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
}

fn opts() -> ConvertOptions {
    ConvertOptions { attachment_dir: ".page".into(), ..Default::default() }
}

/// Too deep to render, so kept verbatim: the fence goes back up as the very
/// bytes it came down as, and the blocks around it render as usual.
#[test]
fn deeply_nested_storage_is_preserved_rather_than_overflowing() {
    for deep in [
        nested("<div>", "</div>", 50_000, "<p>Фраза 1.</p>"),
        nested("<ul><li>", "</li></ul>", 20_000, "item"),
        format!("<p>{}</p>", nested("<strong>", "</strong>", 50_000, "bold")),
        format!(
            "<table><tbody><tr><td>{}</td></tr></tbody></table>",
            nested("<div>", "</div>", 5_000, "x")
        ),
    ] {
        let storage = format!("<p>Before.</p>{deep}<p>After.</p>");
        let c = storage_to_markdown(&storage, &opts()).unwrap();
        let kinds: Vec<BlockKind> = c.block_map.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, [BlockKind::Paragraph, BlockKind::Preserved, BlockKind::Paragraph]);
        assert!(c.markdown.starts_with("Before.\n"), "{}", &c.markdown[..80]);
        assert!(c.markdown.trim_end().ends_with("After."));

        let fence = c.block_map.block_markdown(1, &c.markdown).unwrap();
        let back = markdown_to_storage(&fence, &opts()).unwrap();
        assert_eq!(pretty::minify(&back), pretty::minify(&deep), "kept byte for byte");
    }
}

/// Up to the limit, nothing changes: the page renders as it always did.
#[test]
fn nesting_up_to_the_limit_still_renders() {
    let lists = nested("<ul><li>", "</li></ul>", MAX_DEPTH / 2 - 1, "item");
    let c = storage_to_markdown(&lists, &opts()).unwrap();
    assert_eq!(c.block_map.blocks[0].kind, BlockKind::List);

    let bold = format!("<p>{}</p>", nested("<strong>", "</strong>", MAX_DEPTH - 2, "bold"));
    let c = storage_to_markdown(&bold, &opts()).unwrap();
    assert_eq!(c.block_map.blocks[0].kind, BlockKind::Paragraph);
}

/// Markdown nested past the limit is refused, naming its line; the rest of
/// the document would have been fine.
#[test]
fn deeply_nested_markdown_is_refused_with_its_line() {
    let mut lists = String::new();
    for i in 0..5_000 {
        lists.push_str(&"  ".repeat(i));
        lists.push_str("- x\n");
    }
    for deep in [
        format!("{}x\n", "> ".repeat(50_000)),
        lists,
        format!("{}x{}\n", "*a ".repeat(20_000), " a*".repeat(20_000)),
    ] {
        let md = format!("Fine.\n\n{deep}");
        let err = markdown_to_storage(&md, &opts()).unwrap_err().to_string();
        assert!(err.contains("line 3"), "{err}");
        assert!(err.contains("levels deep"), "{err}");
    }

    let quotes = format!("{}x\n", "> ".repeat(MAX_DEPTH / 2));
    assert!(markdown_to_storage(&quotes, &opts()).is_ok(), "within the limit");
}
