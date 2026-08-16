//! `confed config` — inspect and change stored settings.
//!
//! `--list` shows where each value came from, which is what makes the
//! flag → env → stored → prompt precedence debuggable.

use crate::cli::ConfigArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::config::SETTABLE;
use confed_core::error::{ConfedError, Result};
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
        _ => None,
    }
}

pub fn run(ctx: &mut Context, args: &ConfigArgs) -> Result<Output> {
    if let Some(key) = &args.unset {
        let meta = meta_key(key).ok_or_else(|| unknown_key(key))?;
        ctx.workspace()?.state().delete_meta(meta)?;
        return Ok(Output::new(json!({ "unset": key }), format!("Unset {key}.\n")));
    }

    if !args.set.is_empty() {
        let (key, value) = (&args.set[0], &args.set[1]);
        let meta = meta_key(key).ok_or_else(|| unknown_key(key))?;
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

fn unknown_key(key: &str) -> ConfedError {
    let known: Vec<&str> = SETTABLE.iter().map(|s| s.key).collect();
    ConfedError::usage_with_hint(
        format!("unknown setting `{key}`"),
        format!("settable keys: {}", known.join(", ")),
    )
}
