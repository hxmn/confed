//! `confed clone` — `init` plus a first `pull`.

use crate::cli::CloneArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::sync::PullOptions;
use serde_json::json;
use std::fmt::Write;
use std::path::PathBuf;

pub async fn run(
    ctx: &mut Context,
    args: &CloneArgs,
    mut input: crate::commands::init::InitInput,
) -> Result<Output> {
    // A space URL carries both the base URL and the key.
    if let Some((base_url, space_key)) = parse_space_url(&args.source) {
        input.base_url = base_url;
        input.space = Some(space_key);
    } else {
        input.space = Some(args.source.clone());
    }
    let space_key = input.space.clone().unwrap_or_default();

    let dir: PathBuf = match &args.directory {
        Some(dir) => dir.clone(),
        None => ctx.cwd.join(&space_key),
    };
    if dir.exists() && dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(ConfedError::state_with_hint(
            format!("{} already exists and is not empty", dir.display()),
            "choose another directory, or run `confed init` inside the existing one",
        ));
    }

    let init_output =
        crate::commands::init::initialize(ctx, &args.init, input, &dir).await?;

    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    let pull = engine.pull(ws, &PullOptions::everything()).await?;

    let mut human = init_output.human.clone();
    let _ = writeln!(
        human,
        "\nPulled {} into {}",
        crate::output::plural(pull.created.len(), "page", "pages"),
        dir.display()
    );

    Ok(Output::new(
        json!({ "init": init_output.result, "pull": serde_json::to_value(&pull)? }),
        human,
    )
    .warn_all(init_output.warnings))
}

/// `https://site.atlassian.net/wiki/spaces/DOCS/...` → base URL + `DOCS`.
/// Also handles Data Center's `/display/DOCS`.
fn parse_space_url(source: &str) -> Option<(String, String)> {
    if !source.starts_with("http://") && !source.starts_with("https://") {
        return None;
    }
    let url = url::Url::parse(source).ok()?;
    let segments: Vec<&str> = url.path().trim_matches('/').split('/').collect();
    let index = segments.iter().position(|s| *s == "spaces" || *s == "display")?;
    let key = segments.get(index + 1)?.to_string();

    let context_path = segments[..index].join("/");
    let origin = format!("{}://{}", url.scheme(), url.host_str()?);
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let base = if context_path.is_empty() {
        format!("{origin}{port}")
    } else {
        format!("{origin}{port}/{context_path}")
    };
    Some((base, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_space_urls_are_understood() {
        let (base, key) =
            parse_space_url("https://acme.atlassian.net/wiki/spaces/DOCS/overview").unwrap();
        assert_eq!(base, "https://acme.atlassian.net/wiki");
        assert_eq!(key, "DOCS");
    }

    #[test]
    fn data_center_urls_are_understood() {
        let (base, key) =
            parse_space_url("https://wiki.corp.example.com/display/DOCS/Home").unwrap();
        assert_eq!(base, "https://wiki.corp.example.com");
        assert_eq!(key, "DOCS");

        let (base, key) =
            parse_space_url("https://wiki.corp/confluence/display/OPS").unwrap();
        assert_eq!(base, "https://wiki.corp/confluence");
        assert_eq!(key, "OPS");
    }

    #[test]
    fn a_bare_space_key_is_not_a_url() {
        assert!(parse_space_url("DOCS").is_none());
        assert!(parse_space_url("https://acme.atlassian.net/wiki").is_none());
    }
}
