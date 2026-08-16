//! `confed search` — CQL passthrough, with plain text wrapped into a query.

use crate::cli::SearchArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, args: &SearchArgs) -> Result<Output> {
    if args.query.is_empty() {
        return Err(ConfedError::usage("give a search term or a CQL query"));
    }
    let raw = args.query.join(" ");
    let space = ctx.workspace().ok().and_then(|w| w.space_key().ok());
    let cql = build_cql(&raw, space.as_deref(), args.all_spaces);

    let client = ctx.build_client()?;
    let results = client.search_cql(&cql, args.limit).await?;

    // Mark results that already exist locally, so people know where to edit.
    let local: std::collections::HashMap<String, String> = ctx
        .workspace()
        .ok()
        .map(|ws| ws.state().all_pages())
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.page_id, p.local_path))
        .collect();

    let mut human = String::new();
    for result in &results {
        match local.get(result.page_id.as_str()) {
            Some(path) => {
                let _ = writeln!(human, "{:<40} {}", result.title, ctx.style.dim(path));
            }
            None => {
                let _ = writeln!(human, "{:<40} {}", result.title, ctx.style.dim(&result.url));
            }
        }
    }
    if results.is_empty() {
        let _ = writeln!(human, "No results for {cql}");
    }

    Ok(Output::new(
        json!({
            "cql": cql,
            "results": results.iter().map(|r| json!({
                "page_id": r.page_id.0,
                "title": r.title,
                "space": r.space_key,
                "url": r.url,
                "local_path": local.get(r.page_id.as_str()),
                "excerpt": r.excerpt,
            })).collect::<Vec<_>>()
        }),
        human,
    ))
}

/// Anything containing a CQL operator is passed through untouched; plain words
/// become a text search, scoped to the bound space unless --all-spaces.
fn build_cql(raw: &str, space: Option<&str>, all_spaces: bool) -> String {
    let looks_like_cql = ["=", "~", " and ", " or ", "(", "currentUser()"]
        .iter()
        .any(|token| raw.to_lowercase().contains(token));

    if looks_like_cql {
        return raw.to_string();
    }
    let escaped = raw.replace('"', "\\\"");
    match (space, all_spaces) {
        (Some(space), false) => format!("space = \"{space}\" and text ~ \"{escaped}\""),
        _ => format!("text ~ \"{escaped}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_scoped_to_the_current_space() {
        assert_eq!(
            build_cql("database failover", Some("DOCS"), false),
            "space = \"DOCS\" and text ~ \"database failover\""
        );
        assert_eq!(build_cql("failover", Some("DOCS"), true), "text ~ \"failover\"");
        assert_eq!(build_cql("failover", None, false), "text ~ \"failover\"");
    }

    #[test]
    fn a_real_cql_query_is_passed_through_untouched() {
        let cql = "label = \"runbook\" and lastmodified > now(\"-7d\")";
        assert_eq!(build_cql(cql, Some("DOCS"), false), cql);
    }

    #[test]
    fn quotes_in_plain_text_are_escaped() {
        assert!(build_cql("the \"big\" outage", None, true).contains("\\\"big\\\""));
    }
}
