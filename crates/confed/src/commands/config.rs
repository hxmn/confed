//! `confed config` — inspect and change stored settings.
//!
//! `--list` shows where each value came from, which is what makes the
//! flag → env → stored → prompt precedence debuggable.

use crate::cli::ConfigArgs;
use crate::context::Context;
use crate::output::Output;
use confed_api::Secret;
use confed_core::config::{self, SETTABLE};
use confed_core::error::{ConfedError, Result};
use confed_core::session::SecretBackend;
use serde_json::json;
use std::fmt::Write;

/// Settings live in `.state.db`'s meta table under these keys.
fn meta_key(key: &str) -> Option<&'static str> {
    match key {
        "space" | "space_key" => Some("space_key"),
        "base_url" => Some("base_url"),
        "flavor" => Some("flavor"),
        "concurrency" => Some("concurrency"),
        "editor" => Some("editor"),
        "comments.marks" => Some(confed_core::sync::MARKS_MODE_KEY),
        _ => None,
    }
}

pub fn run(ctx: &mut Context, args: &ConfigArgs) -> Result<Output> {
    if args.no_keychain || args.force_keychain {
        let target = if args.no_keychain { SecretBackend::Sqlite } else { SecretBackend::Keyring };
        return switch_credential_store(ctx, target);
    }

    if let Some(key) = &args.unset {
        let meta = meta_key(key).ok_or_else(|| unknown_key(key))?;
        ctx.workspace()?.state().delete_meta(meta)?;
        return Ok(Output::new(json!({ "unset": key }), format!("Unset {key}.\n")));
    }

    if !args.set.is_empty() {
        let (key, value) = (&args.set[0], &args.set[1]);
        let meta = meta_key(key).ok_or_else(|| unknown_key(key))?;
        if key == "comments.marks" && confed_core::sync::MarksMode::parse(value).is_none() {
            return Err(ConfedError::usage_with_hint(
                format!("`{value}` is not a marks mode"),
                "use `full` (id and preview), `ids` (id only) or `off` (no marks in page bodies)",
            ));
        }
        ctx.workspace()?.state().set_meta(meta, value)?;
        return Ok(Output::new(
            json!({ "key": key, "value": value }),
            format!("Set {key} = {value}\n"),
        ));
    }

    if let Some(key) = &args.get {
        let spec =
            SETTABLE.iter().find(|s| s.key == key.as_str()).ok_or_else(|| unknown_key(key))?;
        let resolved = ctx.resolver.lookup(spec, None);
        let value =
            resolved
                .as_ref()
                .map(|r| if spec.secret { "***".to_string() } else { r.value.clone() });
        let human = value.clone().map(|v| format!("{v}\n")).unwrap_or_default();
        return Ok(Output::new(
            json!({
                "key": key,
                "value": value,
                "source": resolved.map(|r| r.source.as_str()),
            }),
            human,
        ));
    }

    let mut specs = SETTABLE.to_vec();
    specs.push(confed_core::config::USERNAME);
    specs.push(confed_core::config::TOKEN);

    let mut human = String::new();
    let mut entries = Vec::new();
    for (spec, resolved) in ctx.resolver.describe(&specs) {
        let (value, source) = match &resolved {
            Some(r) => (r.value.clone(), r.source.as_str()),
            None => (String::new(), "unset"),
        };
        let _ = writeln!(
            human,
            "{:<14} {:<44} {}",
            spec.key,
            if value.is_empty() { "—".to_string() } else { value.clone() },
            ctx.style.dim(&format!("({source})"))
        );
        entries.push(json!({
            "key": spec.key, "value": value, "source": source,
            "flag": spec.flag, "env": spec.env,
        }));
    }

    Ok(Output::new(json!({ "entries": entries }), human))
}

/// Move the stored credential between the OS keychain and `.session.db`.
///
/// Reading it may prompt once if it currently lives in the keychain; after a
/// move to the database, nothing prompts again.
fn switch_credential_store(ctx: &Context, target: SecretBackend) -> Result<Output> {
    let ws = ctx.workspace()?;
    let store = ws.session_store()?;
    let session = store.load()?.ok_or_else(|| {
        ConfedError::state_with_hint(
            "no credentials are stored in this directory",
            "run `confed init` first",
        )
    })?;

    let previous = session.secret_backend;
    if previous == target {
        return Ok(Output::new(
            json!({ "credential_store": target.as_str(), "changed": false }),
            format!("Credentials are already in {}.\n", describe(target)),
        ));
    }

    // If the current backend cannot be read, an explicitly supplied token is
    // enough to complete the move.
    let supplied = ctx
        .resolver
        .lookup(&config::TOKEN, ctx.global.token.as_deref())
        .map(|r| Secret::new(r.value));

    let moved = store.switch_backend(&session, target, supplied)?;

    let mut human = format!(
        "Moved credentials from {} to {}.\n",
        describe(previous),
        describe(moved.secret_backend)
    );
    let mut output = Output::new(
        json!({
            "credential_store": moved.secret_backend.as_str(),
            "previous": previous.as_str(),
            "changed": true,
        }),
        String::new(),
    );

    match moved.secret_backend {
        SecretBackend::Sqlite => {
            human.push_str(&ctx.style.dim(
                "Reading them no longer prompts. .session.db is mode 0600 and git-ignored, \n                 but it holds the token in plain text — prefer CONFED_TOKEN on shared machines.\n",
            ));
        }
        SecretBackend::Keyring => {
            human.push_str(&ctx.style.dim(
                "The keychain may now ask for permission the first time confed reads them.\n",
            ));
        }
    }

    if target == SecretBackend::Keyring && moved.secret_backend == SecretBackend::Sqlite {
        output =
            output.warn("no OS keychain was available, so the credential stayed in .session.db");
    }
    output.human = human;
    Ok(output)
}

fn describe(backend: SecretBackend) -> &'static str {
    match backend {
        SecretBackend::Keyring => "the OS keychain",
        SecretBackend::Sqlite => ".session.db",
    }
}

fn unknown_key(key: &str) -> ConfedError {
    let known: Vec<&str> = SETTABLE.iter().map(|s| s.key).collect();
    ConfedError::usage_with_hint(
        format!("unknown setting `{key}`"),
        format!("settable keys: {}", known.join(", ")),
    )
}
