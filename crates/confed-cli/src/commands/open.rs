//! `confed open` — open a page in the browser, or just print its URL.

use crate::cli::OpenArgs;
use crate::context::Context;
use crate::output::Output;
use confed_api::PageId;
use confed_core::error::{ConfedError, Result};
use serde_json::json;

pub async fn run(ctx: &mut Context, args: &OpenArgs) -> Result<Output> {
    let client = ctx.build_client()?;
    let space_key = ctx.workspace()?.space_key()?;

    let url = match &args.page {
        Some(reference) => {
            let page_id = ctx.resolve_page(reference)?;
            client.page_url(&PageId::new(&page_id), &space_key)
        }
        None => format!("{}/spaces/{space_key}", client.base_url()),
    };

    // Printing is the right default when nobody is watching a browser.
    let print_only = args.print || ctx.global.json || !ctx.is_interactive();
    if print_only {
        return Ok(Output::new(json!({ "url": url, "opened": false }), format!("{url}\n")));
    }

    launch(&url)?;
    Ok(Output::new(json!({ "url": url, "opened": true }), format!("Opened {url}\n")))
}

fn launch(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            ConfedError::io(format!("launching {opener} (try --print to get the URL)"), e)
        })?;
    Ok(())
}
