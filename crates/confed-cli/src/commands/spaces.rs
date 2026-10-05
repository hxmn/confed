//! `confed spaces` — list spaces visible to the authenticated user.

use crate::cli::SpacesArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::Result;
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, args: &SpacesArgs) -> Result<Output> {
    let client = ctx.build_client()?;
    let spaces = client.list_spaces(Some(args.limit)).await?;
    let bound = ctx.workspace().ok().and_then(|w| w.space_key().ok());

    let mut human = String::new();
    for space in &spaces {
        let marker = if bound.as_deref() == Some(space.id.key.as_str()) { "*" } else { " " };
        let _ = writeln!(human, "{marker} {:<12} {}", space.id.key, space.name);
    }
    if spaces.is_empty() {
        human.push_str("No spaces are visible to this account.\n");
    }

    Ok(Output::new(
        json!({
            "spaces": spaces.iter().map(|s| json!({
                "key": s.id.key,
                "id": s.id.numeric,
                "name": s.name,
                "type": s.kind,
                "current": bound.as_deref() == Some(s.id.key.as_str()),
            })).collect::<Vec<_>>()
        }),
        human,
    ))
}
