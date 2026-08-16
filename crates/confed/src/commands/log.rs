//! `confed log` — server version history, or confed's own sync log.

use crate::cli::LogArgs;
use crate::context::Context;
use crate::output::Output;
use confed_api::PageId;
use confed_core::error::Result;
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, args: &LogArgs) -> Result<Output> {
    let page_id = ctx.resolve_page(&args.page)?;

    if args.local {
        let entries = ctx.workspace()?.state().recent_log(args.limit, Some(&page_id))?;
        let mut human = String::new();
        for entry in &entries {
            let _ = writeln!(
                human,
                "{}  {:<14} {} {}",
                ctx.style.dim(&entry.ts),
                entry.op,
                entry.result,
                entry.detail.clone().unwrap_or_default()
            );
        }
        return Ok(Output::new(
            json!({
                "page_id": page_id,
                "entries": entries.iter().map(|e| json!({
                    "ts": e.ts, "op": e.op, "result": e.result,
                    "from_version": e.from_version, "to_version": e.to_version,
                    "detail": e.detail,
                })).collect::<Vec<_>>()
            }),
            human,
        ));
    }

    let base_version = ctx.workspace()?.state().get_page(&page_id)?.map(|p| p.version);
    let client = ctx.build_client()?;
    let versions = client.get_page_versions(&PageId::new(&page_id), args.limit).await?;

    let mut human = String::new();
    for version in &versions {
        let marker = if Some(version.number) == base_version { "*" } else { " " };
        let _ = writeln!(
            human,
            "{marker} v{:<4} {:<20} {} {}",
            version.number,
            version.author.clone().unwrap_or_default(),
            ctx.style.dim(&version.when.clone().unwrap_or_default()),
            version.message.clone().unwrap_or_default()
        );
    }
    if base_version.is_some() {
        let _ = writeln!(human, "\n{}", ctx.style.dim("* the version your local file is based on"));
    }

    Ok(Output::new(
        json!({
            "page_id": page_id,
            "base_version": base_version,
            "versions": versions.iter().map(|v| json!({
                "number": v.number, "author": v.author,
                "when": v.when, "message": v.message,
            })).collect::<Vec<_>>()
        }),
        human,
    ))
}
