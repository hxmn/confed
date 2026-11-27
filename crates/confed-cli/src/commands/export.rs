//! `confed export` — write pages out as standalone HTML or raw storage format.

use crate::cli::ExportArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::sync::path_matches;
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, args: &ExportArgs) -> Result<Output> {
    let ws = ctx.workspace()?;
    let pages = ws.state().all_pages()?;
    let out_dir = ctx.cwd.join(&args.out);

    let mut exported = Vec::new();
    let mut human = String::new();

    let scope = ctx.workspace_paths(&args.paths);
    for page in &pages {
        if !scope.is_empty()
            && !scope.iter().any(|p| {
                p == &page.page_id || p == &page.local_path || path_matches(p, &page.local_path)
            })
        {
            continue;
        }

        let (extension, content) = match args.format.as_str() {
            "storage" => ("xml", page.storage_body.clone()),
            _ => {
                let opts = confed_core::sync::page_convert_options(ws, &page.local_path)?;
                let markdown = confed_converter::storage_to_markdown(&page.storage_body, &opts)?;
                ("html", standalone_html(&page.title, &markdown.markdown))
            }
        };

        let relative = page.local_path.strip_suffix(".md").unwrap_or(&page.local_path).to_string();
        let dest = out_dir.join(format!("{relative}.{extension}"));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
        }
        std::fs::write(&dest, &content)
            .map_err(|e| ConfedError::io(format!("writing {}", dest.display()), e))?;

        let _ = writeln!(human, "  {}", dest.display());
        exported.push(json!({ "path": page.local_path, "out": dest.to_string_lossy() }));
    }

    if exported.is_empty() {
        human.push_str("Nothing matched.\n");
    } else {
        let _ = writeln!(
            human,
            "\nExported {} to {}",
            crate::output::plural(exported.len(), "page", "pages"),
            out_dir.display()
        );
    }

    Ok(Output::new(json!({ "format": args.format, "exported": exported }), human))
}

/// A readable page, rendered from the page's Markdown. Storage is not HTML —
/// a code macro's CDATA is a bogus comment to an HTML parser, and the code
/// after its first `>` would be live markup — so it is never written as is.
/// Confluence's own CSS is not reproduced.
fn standalone_html(title: &str, markdown: &str) -> String {
    let body = confed_converter::markdown_to_html(markdown);
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\n\
         <title>{}</title>\n\
         <style>body{{font:16px/1.6 system-ui,sans-serif;max-width:46rem;margin:3rem auto;padding:0 1rem}}\n\
         pre{{background:#f5f5f5;padding:1rem;overflow:auto}}\n\
         table{{border-collapse:collapse}}td,th{{border:1px solid #ddd;padding:.4rem}}</style>\n\
         </head><body>\n<h1>{}</h1>\n{}\n</body></html>\n",
        escape(title),
        escape(title),
        body.trim_end()
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_export_renders_the_page() {
        let html = standalone_html("Onboarding & Setup", "hello\n");
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<p>hello</p>"));
        assert!(html.contains("Onboarding &amp; Setup"), "the title must be escaped");
    }

    #[test]
    fn code_is_exported_as_text_never_as_markup() {
        let storage = r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[if a > b { run("<img src=x>") }]]></ac:plain-text-body></ac:structured-macro>"#;
        let md = confed_converter::storage_to_markdown(storage, &Default::default()).unwrap();
        let html = standalone_html("Code", &md.markdown);
        assert!(html.contains("<pre>"), "{html}");
        assert!(html.contains("run(&quot;&lt;img src=x&gt;&quot;)"), "{html}");
        assert!(!html.contains("<img"), "{html}");
    }
}
