//! `confed whoami` — verify credentials and report what the server supports.

use crate::context::Context;
use crate::output::Output;
use confed_core::error::Result;
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context) -> Result<Output> {
    let client = ctx.build_client()?;
    let user = client.whoami().await?;
    let caps = client.capabilities();

    let mut human = String::new();
    let _ = writeln!(human, "{}", ctx.style.bold(&user.display_name));
    if let Some(email) = &user.email {
        let _ = writeln!(human, "  email       {email}");
    }
    let _ = writeln!(human, "  server      {} ({})", client.base_url(), caps.flavor);
    let _ = writeln!(
        human,
        "  inline comments  {}",
        if caps.inline_comment_create { "create + resolve" } else { "read only" }
    );

    // Record that these credentials worked, for `doctor`.
    if let Ok(ws) = ctx.workspace() {
        if let Ok(store) = ws.session_store() {
            let _ = store.mark_verified();
        }
    }

    Ok(Output::new(
        json!({
            "user": {
                "account_id": user.account_id,
                "username": user.username,
                "display_name": user.display_name,
                "email": user.email,
            },
            "base_url": client.base_url(),
            "flavor": caps.flavor.as_str(),
            "capabilities": {
                "inline_comment_create": caps.inline_comment_create,
                "comment_resolve": caps.comment_resolve,
                "adf": caps.adf,
                "max_request_concurrency": caps.max_request_concurrency,
            },
        }),
        human,
    ))
}
