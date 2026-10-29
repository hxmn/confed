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
    // Only Data Center says (anonymously, from its applinks manifest); its
    // inline comments rely on an API that may change between releases.
    let server_version = client.server_version().await.ok().flatten();
    let _ = writeln!(
        human,
        "  server      {} ({}{})",
        client.base_url(),
        caps.flavor,
        server_version.as_deref().map(|v| format!(" {v}")).unwrap_or_default()
    );
    let _ = writeln!(
        human,
        "  inline comments  {}",
        if caps.inline_comment_create { "create + resolve" } else { "read only" }
    );

    // Asks the move endpoint to move a real page under one that cannot exist;
    // nothing moves. Without a pulled page there is nothing to ask about.
    let moves = match crate::commands::doctor::probe_page(ctx) {
        Some(page) => client.probe_move(&page).await.unwrap_or(confed_api::Probe::Unknown),
        None => confed_api::Probe::Unknown,
    };
    let _ = writeln!(human, "  move between spaces  {}", moves.as_str());

    // Record that these credentials worked, for `doctor`.
    if let Ok(Some((store, _))) = ctx.session_store() {
        let _ = store.mark_verified();
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
            "server_version": server_version,
            "capabilities": {
                "inline_comment_create": caps.inline_comment_create,
                "comment_resolve": caps.comment_resolve,
                "adf": caps.adf,
                "max_request_concurrency": caps.max_request_concurrency,
                "move_between_spaces": moves.as_str(),
            },
        }),
        human,
    ))
}
