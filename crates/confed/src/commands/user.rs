//! `confed user search` — find people, for writing mentions.

use crate::cli::UserCommand;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::Result;
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, command: &UserCommand) -> Result<Output> {
    let UserCommand::Search { query, limit } = command;
    let client = ctx.build_client()?;
    let people = client.search_users(query, *limit).await?;

    let mut human = String::new();
    let mut entries = Vec::new();
    for user in &people {
        // The id a mention carries: the userkey on Data Center, the account id
        // on Cloud.
        let mention = match (&user.user_key, &user.account_id) {
            (Some(key), _) => Some(format!("[@{}](user:{key})", user.display_name)),
            (None, Some(id)) => Some(format!("[@{}](user:account-id={id})", user.display_name)),
            _ => None,
        };
        let _ = writeln!(
            human,
            "{:<28} {:<20} {}",
            user.display_name,
            user.username.clone().unwrap_or_default(),
            ctx.style.dim(mention.as_deref().unwrap_or(""))
        );
        entries.push(json!({
            "display_name": user.display_name,
            "username": user.username,
            "userkey": user.user_key,
            "account_id": user.account_id,
            "mention": mention,
        }));
    }
    if entries.is_empty() {
        let _ = writeln!(human, "Nobody matches \"{query}\".");
    }
    Ok(Output::new(json!({ "query": query, "users": entries }), human))
}
