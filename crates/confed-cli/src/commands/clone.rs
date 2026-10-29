//! `confed clone` — `init` plus a first `pull`, for one space or, with
//! `--all`, for every space you can see.
//!
//! Like `init` it is split in two: [`prepare`] runs before the runtime starts
//! (it may prompt, and it reads the OS keyring), then [`run`] does the network
//! work.

use crate::cli::CloneArgs;
use crate::commands::init::{self, Borrowed, Connected, InitInput, Known};
use crate::context::Context;
use crate::output::{plural, Output};
use crate::prompt;
use confed_api::Space;
use confed_core::config::{self, Source};
use confed_core::error::{ConfedError, ExitCode, Result};
use confed_core::session::SessionStore;
use confed_core::state::STATE_DB_FILENAME;
use confed_core::sync::PullOptions;
use confed_core::workspace::Workspace;
use serde_json::json;
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Gather the credential: from flags and the environment, from `--from`'s
/// workspace, or, at a terminal, from a lone workspace beside the target.
pub fn prepare(ctx: &mut Context, args: &CloneArgs) -> Result<InitInput> {
    let mut known = Known::default();
    if !args.all {
        let source = args.source.clone().unwrap_or_default();
        // A space URL carries both the base URL and the key.
        match parse_space_url(&source) {
            Some((base_url, key)) => {
                known.base_url = Some(base_url);
                known.space = Some(key);
            }
            None => known.space = Some(source),
        }
    }
    known.from = match &args.from {
        Some(path) => Some(Borrowed::load(&ctx.cwd.join(path))?),
        None => {
            let parent = match &known.space {
                Some(key) => single_target(ctx, args, key).parent().map(Path::to_path_buf),
                None => Some(all_root(ctx, args)),
            };
            match parent {
                Some(parent) => offer_neighbour(ctx, &parent, known.base_url.as_deref())?,
                None => None,
            }
        }
    };
    init::prepare_with(ctx, &args.init, known)
}

/// Without `--from`, a workspace already beside the new one probably holds
/// the credential. When it is the only one there for this instance, offer it;
/// never take it unasked.
fn offer_neighbour(
    ctx: &Context,
    parent: &Path,
    base_url: Option<&str>,
) -> Result<Option<Borrowed>> {
    let explicit = |spec, flag: Option<&str>| {
        ctx.resolver
            .lookup(spec, flag)
            .filter(|r| matches!(r.source, Source::Flag | Source::Env))
            .map(|r| r.value)
    };
    if !ctx.is_interactive() || explicit(&config::TOKEN, ctx.global.token.as_deref()).is_some() {
        return Ok(None);
    }
    let base_url = base_url
        .map(str::to_string)
        .or_else(|| explicit(&config::BASE_URL, ctx.global.base_url.as_deref()));

    let Ok(entries) = std::fs::read_dir(parent) else { return Ok(None) };
    let mut found: Vec<Borrowed> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| dir.join(STATE_DB_FILENAME).exists() && SessionStore::exists(dir))
        .filter_map(|dir| Borrowed::load(&dir).ok())
        .filter(|b| base_url.as_deref().is_none_or(|url| init::same_instance(url, &b.base_url)))
        .collect();
    if found.len() != 1 {
        return Ok(None);
    }
    let candidate = found.remove(0);
    let who = match &candidate.session.username {
        Some(user) => format!("{user} on {}", candidate.base_url),
        None => candidate.base_url.clone(),
    };
    let question = format!("Use the credential of {} ({who})?", candidate.root.display());
    Ok(prompt::confirm(&question, true)?.then_some(candidate))
}

/// Where a single space is cloned: DIRECTORY, or a folder named after the key.
fn single_target(ctx: &Context, args: &CloneArgs, key: &str) -> PathBuf {
    match &args.directory {
        Some(dir) => ctx.cwd.join(dir),
        None => ctx.cwd.join(key),
    }
}

/// The folder `--all` clones into: ROOT, or the current one.
fn all_root(ctx: &Context, args: &CloneArgs) -> PathBuf {
    match &args.source {
        Some(root) => ctx.cwd.join(root),
        None => ctx.cwd.clone(),
    }
}

pub async fn run(ctx: &mut Context, args: &CloneArgs, input: InitInput) -> Result<Output> {
    if args.all {
        return run_all(ctx, args, input).await;
    }
    let space_key = input.space.clone().unwrap_or_default();
    let dir = single_target(ctx, args, &space_key);
    if is_occupied(&dir) {
        return Err(ConfedError::state_with_hint(
            format!("{} already exists and is not empty", dir.display()),
            "choose another directory, or run `confed init` inside the existing one",
        ));
    }

    let init_output = init::initialize(ctx, &args.init, input, &dir).await?;

    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    let pull = engine.pull(ws, &PullOptions::everything()).await?;

    let mut human = init_output.human.clone();
    let _ = writeln!(
        human,
        "\nPulled {} into {}",
        plural(pull.created.len(), "page", "pages"),
        dir.display()
    );

    Ok(Output::new(
        json!({ "init": init_output.result, "pull": serde_json::to_value(&pull)? }),
        human,
    )
    .warn_all(init_output.warnings)
    .warn_all(pull.warnings))
}

/// What became of one space under `--all`.
struct Entry {
    key: String,
    name: String,
    dir: PathBuf,
    outcome: Result<(Status, usize)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Cloned = 0,
    Pulled = 1,
    Skipped = 2,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Cloned => "cloned",
            Status::Pulled => "pulled",
            Status::Skipped => "skipped",
        }
    }
}

/// `clone --all`: every space, one after another. A space that fails is
/// recorded and the rest still run; one that is already cloned is left as it
/// is, or pulled with `--pull`, so an interrupted run can be resumed.
async fn run_all(ctx: &mut Context, args: &CloneArgs, mut input: InitInput) -> Result<Output> {
    let root = all_root(ctx, args);

    // One check up front: a rejected credential fails here, before any folder exists.
    let connected = init::connect(ctx, &input).await?;
    input.flavor_hint = Some(connected.flavor);
    let (chosen, unknown) = choose(connected.client.list_spaces(None).await?, args);

    let announce = crate::progress::TerminalProgress::should_show(
        ctx.global.silent,
        ctx.global.quiet,
        ctx.global.json,
    );
    let mut entries = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (index, space) in chosen.iter().enumerate() {
        let key = space.id.key.clone();
        let dir = root.join(&key);
        if announce {
            eprintln!("[{}/{}] {key}", index + 1, chosen.len());
        }
        let outcome = clone_one(ctx, args, &input, &connected, &key, &dir, &mut warnings).await;
        entries.push(Entry { key, name: space.name.clone(), dir, outcome });
    }
    for key in unknown {
        entries.push(Entry {
            dir: root.join(&key),
            name: String::new(),
            outcome: Err(ConfedError::NotFound(format!(
                "space {key} is not visible to this account"
            ))),
            key,
        });
    }

    let style = &ctx.style;
    let mut human = String::new();
    let _ = writeln!(
        human,
        "Authenticated as {} on Confluence {}\n",
        style.bold(&connected.user.display_name),
        connected.flavor
    );
    let mut counts = [0usize; 4];
    for entry in &entries {
        let (label, detail) = match &entry.outcome {
            Ok((status, pages)) => {
                counts[*status as usize] += 1;
                let pages = plural(*pages, "page", "pages");
                let detail = match status {
                    Status::Skipped => format!("{pages}, already cloned; --pull updates it"),
                    _ => pages,
                };
                (status.as_str().to_string(), detail)
            }
            Err(e) => {
                counts[3] += 1;
                (style.red("failed"), error_text(e))
            }
        };
        let _ = writeln!(human, "  {label:<8} {:<12} {detail}", entry.key);
    }
    if entries.is_empty() {
        let _ = writeln!(human, "No spaces matched.");
    } else {
        let [cloned, pulled, skipped, failed] = counts;
        let _ = writeln!(
            human,
            "\n{} in {}: {cloned} cloned, {pulled} pulled, {skipped} skipped, {failed} failed",
            plural(entries.len(), "space", "spaces"),
            root.display()
        );
    }

    let spaces: Vec<_> = entries
        .iter()
        .map(|entry| {
            let (status, pages, error, code) = match &entry.outcome {
                Ok((status, pages)) => (status.as_str(), Some(*pages), None, None),
                Err(e) => ("failed", None, Some(error_text(e)), Some(e.code())),
            };
            json!({
                "key": entry.key,
                "name": entry.name,
                "dir": entry.dir.display().to_string(),
                "status": status,
                "pages": pages,
                "error": error,
                "code": code,
            })
        })
        .collect();
    let result = json!({
        "root": root.display().to_string(),
        "base_url": input.base_url,
        "flavor": connected.flavor.as_str(),
        "user": {
            "account_id": connected.user.account_id,
            "display_name": connected.user.display_name,
            "email": connected.user.email,
        },
        "spaces": spaces,
    });

    let mut output = Output::new(result, human).warn_all(warnings);
    if counts[3] > 0 {
        output = output.with_exit(ExitCode::Partial);
    }
    Ok(output)
}

/// The spaces `--all` takes: those named by `--include`, of any type, or else
/// every space of `--type`; less `--exclude`. Also returns the included keys
/// the server did not list.
fn choose(spaces: Vec<Space>, args: &CloneArgs) -> (Vec<Space>, Vec<String>) {
    let named = |list: &[String], key: &str| list.iter().any(|k| k.eq_ignore_ascii_case(key));
    let want_personal = args.space_type.as_deref() == Some("personal");
    let mut chosen: Vec<Space> = spaces
        .into_iter()
        .filter(|s| match args.include.is_empty() {
            true => is_personal(s) == want_personal,
            false => named(&args.include, &s.id.key),
        })
        .filter(|s| !named(&args.exclude, &s.id.key))
        .collect();
    chosen.sort_by(|a, b| a.id.key.cmp(&b.id.key));
    chosen.dedup_by(|a, b| a.id.key == b.id.key);

    let mut unknown: Vec<String> = Vec::new();
    for key in &args.include {
        let listed = chosen.iter().any(|s| s.id.key.eq_ignore_ascii_case(key));
        if !listed && !named(&args.exclude, key) && !named(&unknown, key) {
            unknown.push(key.clone());
        }
    }
    (chosen, unknown)
}

/// Personal spaces are typed so on both flavors, and keyed `~user`.
fn is_personal(space: &Space) -> bool {
    space.kind.as_deref() == Some("personal") || space.id.key.starts_with('~')
}

/// Clone one space into `dir`, or deal with what is already there.
async fn clone_one(
    ctx: &mut Context,
    args: &CloneArgs,
    input: &InitInput,
    connected: &Connected,
    key: &str,
    dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<(Status, usize)> {
    let elsewhere = format!("move it, or leave the space out with --exclude {key}");
    let status = if dir.join(STATE_DB_FILENAME).exists() {
        let ws = Workspace::open(dir)?;
        let bound = ws.space_key()?;
        if bound != key {
            return Err(ConfedError::state_with_hint(
                format!("{} is already a workspace for space {bound}", dir.display()),
                elsewhere,
            ));
        }
        if let Some(url) = ws.base_url()? {
            if !init::same_instance(&url, &input.base_url) {
                return Err(ConfedError::state_with_hint(
                    format!("{} is a workspace on {url}", dir.display()),
                    elsewhere,
                ));
            }
        }
        if !args.pull {
            return Ok((Status::Skipped, ws.state().all_pages()?.len()));
        }
        ctx.set_workspace(ws);
        Status::Pulled
    } else if is_occupied(dir) {
        return Err(ConfedError::state_with_hint(
            format!("{} exists and is not empty", dir.display()),
            elsewhere,
        ));
    } else {
        let mut input = input.clone();
        input.space = Some(key.to_string());
        let init_output = init::initialize(ctx, &args.init, input, dir).await?;
        // Said once, not once per space.
        for warning in init_output.warnings {
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
        Status::Cloned
    };

    let engine = ctx.engine(connected.client.clone())?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    let pull = engine.pull(ws, &PullOptions::everything()).await?;
    warnings.extend(pull.warnings.iter().map(|w| format!("{key}: {w}")));
    if status == Status::Pulled {
        let rules = crate::commands::agent_docs::sync_rules_after(ctx.workspace()?);
        warnings.extend(rules.into_iter().map(|w| format!("{key}: {w}")));
    }
    if !pull.conflicted.is_empty() {
        return Err(ConfedError::Conflict(format!(
            "{} left conflict markers; edit them in {}, then run `confed resolve <page>`",
            plural(pull.conflicted.len(), "page", "pages"),
            dir.display()
        )));
    }
    Ok((status, ctx.workspace()?.state().all_pages()?.len()))
}

fn is_occupied(dir: &Path) -> bool {
    dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false)
}

/// A failure as one line: the message, and what to do about it.
fn error_text(e: &ConfedError) -> String {
    match e.hint() {
        Some(hint) => format!("{e} ({hint})"),
        None => e.to_string(),
    }
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

        let (base, key) = parse_space_url("https://wiki.corp/confluence/display/OPS").unwrap();
        assert_eq!(base, "https://wiki.corp/confluence");
        assert_eq!(key, "OPS");
    }

    fn space(key: &str, kind: &str) -> Space {
        Space {
            id: confed_api::SpaceId::from_key(key),
            name: key.to_string(),
            kind: Some(kind.to_string()),
            homepage_id: None,
            status: None,
        }
    }

    fn args(extra: &[&str]) -> CloneArgs {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrapper {
            #[command(flatten)]
            clone: CloneArgs,
        }
        let mut argv = vec!["confed", "--all"];
        argv.extend(extra);
        Wrapper::parse_from(argv).clone
    }

    fn keys(spaces: &[Space]) -> Vec<&str> {
        spaces.iter().map(|s| s.id.key.as_str()).collect()
    }

    fn listed() -> Vec<Space> {
        vec![
            space("OPS", "global"),
            space("DOCS", "global"),
            space("~alice", "personal"),
            space("ENG", "collaboration"),
        ]
    }

    #[test]
    fn all_takes_the_global_spaces_by_default() {
        let (chosen, unknown) = choose(listed(), &args(&[]));
        assert_eq!(keys(&chosen), ["DOCS", "ENG", "OPS"], "anything not personal, by key");
        assert!(unknown.is_empty());

        let (chosen, _) = choose(listed(), &args(&["--type", "personal"]));
        assert_eq!(keys(&chosen), ["~alice"]);
    }

    #[test]
    fn include_names_spaces_of_either_type_and_exclude_wins() {
        let (chosen, unknown) = choose(
            listed(),
            &args(&["--include", "docs", "--include", "~alice", "--include", "NOPE"]),
        );
        assert_eq!(keys(&chosen), ["DOCS", "~alice"]);
        assert_eq!(unknown, ["NOPE"], "a key the server did not list is reported");

        let (chosen, unknown) = choose(
            listed(),
            &args(&[
                "--include",
                "DOCS",
                "--include",
                "NOPE",
                "--exclude",
                "nope",
                "--exclude",
                "DOCS",
            ]),
        );
        assert!(chosen.is_empty());
        assert!(unknown.is_empty());

        let (chosen, _) = choose(listed(), &args(&["--exclude", "OPS"]));
        assert_eq!(keys(&chosen), ["DOCS", "ENG"]);
    }

    #[test]
    fn a_bare_space_key_is_not_a_url() {
        assert!(parse_space_url("DOCS").is_none());
        assert!(parse_space_url("https://acme.atlassian.net/wiki").is_none());
    }
}
