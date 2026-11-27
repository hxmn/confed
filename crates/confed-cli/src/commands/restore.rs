//! `confed restore` — bring an old version of a page back into its file.
//!
//! The version becomes a local change, as if typed in: `status` shows the
//! page modified, `diff` shows what goes back, and `push` sends it — so a
//! restore is reviewed like any edit, and is itself a new version on the
//! server, not a rewrite of history.

use crate::cli::RestoreArgs;
use crate::commands::log::{fetch_version, Version};
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::sync::{write_atomic, PushOptions};
use serde_json::json;

pub async fn run(ctx: &mut Context, args: &RestoreArgs) -> Result<Output> {
    let page_id = ctx.resolve_page(&args.page)?;
    let record = ctx
        .workspace()?
        .state()
        .get_page(&page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("page {page_id} is not tracked here")))?;
    let path = record.local_path.clone();
    let Version { page, markdown } = fetch_version(ctx, &page_id, args.version).await?;

    let abs = ctx.workspace()?.absolute(&path);
    let text =
        std::fs::read_to_string(&abs).map_err(|e| ConfedError::io(format!("reading {path}"), e))?;
    let mut file = confed_core::frontmatter::parse(&text, &path)?;
    // The text and the title of that version; the managed block stays the
    // file's, so the restore is a change on top of what it is based on.
    file.frontmatter.title = page.summary.title.clone();
    file.marks.retain(|m| m.id.is_new());
    file.set_body(markdown);
    {
        let ws = ctx.workspace()?;
        let _lock = ws.lock()?;
        write_atomic(&abs, &file.render()?)?;
    }

    let mut result = json!({
        "page_id": page_id,
        "path": path,
        "version": args.version,
        "from_base_version": record.version,
    });
    let mut human = format!(
        "Wrote version {} of {path} into the file; review it with `confed diff {path}`.\n",
        args.version
    );
    if !args.push {
        human.push_str("Push it with `confed push` when it is right.\n");
        return Ok(Output::new(result, human));
    }

    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    let outcome = engine
        .push(
            ws,
            &PushOptions {
                scope: vec![page_id.clone()],
                message: Some(format!("Restore version {}", args.version)),
                ..Default::default()
            },
        )
        .await?;
    if let Some(pushed) = outcome.pushed.first() {
        human = format!(
            "Restored version {} of {path} as version {}.\n",
            args.version,
            pushed.to_version.unwrap_or_default()
        );
    }
    result["push"] = serde_json::to_value(&outcome)?;
    Ok(Output::new(result, human).with_push_outcome(&outcome))
}
