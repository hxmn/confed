//! `confed config` — inspect and change stored settings.
//!
//! `--list` shows where each value came from, which is what makes the
//! flag → env → stored → prompt precedence debuggable.

use crate::cli::ConfigArgs;
use crate::commands::agent_docs;
use crate::context::Context;
use crate::output::Output;
use crate::PagePicker;
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
        "rules_page_id" => Some(config::RULES_PAGE.key),
        _ => None,
    }
}

pub fn run(ctx: &mut Context, args: &ConfigArgs, pick_page: PagePicker) -> Result<Output> {
    if args.no_keychain || args.force_keychain {
        let target = if args.no_keychain { SecretBackend::Sqlite } else { SecretBackend::Keyring };
        return switch_credential_store(ctx, target);
    }

    if let Some(key) = &args.unset {
        let meta = meta_key(key).ok_or_else(|| unknown_key(key))?;
        ctx.workspace()?.state().delete_meta(meta)?;
        let mut human = format!("Unset {key}.\n");
        let mut result = json!({ "unset": key });
        if meta == config::RULES_PAGE.key {
            let written = agent_docs::sync_rules(ctx.workspace()?)?.written;
            if !written.is_empty() {
                let _ = writeln!(human, "Removed its rules from {}.", written.join(" and "));
            }
            result["agent_docs"] = json!(written);
        }
        return Ok(Output::new(result, human));
    }

    if !args.set.is_empty() {
        let key = &args.set[0];
        let meta = meta_key(key).ok_or_else(|| unknown_key(key))?;
        if meta == config::RULES_PAGE.key {
            return set_rules_page(ctx, args.set.get(1).map(String::as_str), pick_page);
        }
        let Some(value) = args.set.get(1) else {
            return Err(ConfedError::usage_with_hint(
                format!("`--set {key}` needs a value"),
                format!("confed config --set {key} <VALUE>"),
            ));
        };
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

/// Name the page whose content heads `CLAUDE.md` and `AGENTS.md`, and copy it
/// there. With no `reference`, the user chooses the page from the space.
fn set_rules_page(ctx: &Context, reference: Option<&str>, pick_page: PagePicker) -> Result<Output> {
    let key = config::RULES_PAGE.key;
    let ws = ctx.workspace()?;

    let page_id = match reference {
        Some("") => {
            return Err(ConfedError::usage_with_hint(
                format!("`--set {key}` was given an empty page id"),
                format!("to stop using a rules page: confed config --unset {key}"),
            ))
        }
        Some(reference) => ctx.resolve_page(reference).map_err(|e| match e {
            ConfedError::NotFound(_) if reference.chars().all(|c| c.is_ascii_digit()) => {
                ConfedError::NotFound(format!(
                    "page {reference} is not in this workspace: run `confed fetch` if it is new \
                     on the server, or `confed config --set {key}` to choose from the pages here"
                ))
            }
            other => other,
        })?,
        None => {
            if !ctx.is_interactive() {
                return Err(ConfedError::usage_with_hint(
                    format!("`--set {key}` with no page id opens a picker, which needs a terminal"),
                    format!("pass the page: confed config --set {key} <PAGE ID>"),
                ));
            }
            match pick_page(ws, ws.rules_page_id()?.as_deref())? {
                Some(page_id) => page_id,
                None => {
                    return Ok(Output::new(
                        json!({ "cancelled": true }),
                        format!("Cancelled; {key} is unchanged.\n"),
                    ))
                }
            }
        }
    };

    ws.state().set_meta(key, &page_id)?;
    let synced = agent_docs::sync_rules(ws)?;

    let mut human = String::new();
    let mut output = match &synced.rules {
        Some(rules) => {
            let _ = writeln!(human, "Set {key} = {page_id} (\"{}\")", rules.title);
            let files = agent_docs::FILENAMES.join(" and ");
            let _ = if synced.written.is_empty() {
                writeln!(human, "{files} already start with it.")
            } else {
                writeln!(human, "Copied version {} of it to the top of {files}.", rules.version)
            };
            Output::new(
                json!({
                    "key": key, "value": page_id, "title": rules.title, "path": rules.path,
                    "version": rules.version, "agent_docs": synced.written,
                }),
                String::new(),
            )
        }
        // Fetched but not written to disk: there is no content to copy yet.
        None => {
            let _ = writeln!(human, "Set {key} = {page_id}");
            Output::new(
                json!({ "key": key, "value": page_id, "agent_docs": synced.written }),
                String::new(),
            )
            .warn(format!(
                "page {page_id} has not been pulled, so there are no rules to copy yet; \
                 `confed pull` adds them to {}",
                agent_docs::FILENAMES.join(" and ")
            ))
        }
    };
    output.human = human;
    Ok(output)
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
