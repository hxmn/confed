//! `confed space create` — make a space on the server, and the checks and
//! results `confed instance create` shares with it.
//!
//! A key is checked here before anything is asked of the server: uppercase
//! letters and digits only, at most [`MAX_KEY_LEN`]. Then the server is asked
//! whether a space has the key — archived and personal spaces included — and,
//! for `--dry-run`, whether the account may create spaces at all. Each refusal
//! has a `kind` of its own in `errors`: `invalid_key` and `name_required`
//! (exit 2), `key_taken` and `no_permission` (exit 7).

use crate::cli::{SpaceCommand, SpaceCreateArgs};
use crate::context::Context;
use crate::output::{ErrorEntry, Output};
use confed_api::{ConfluenceClient, Flavor, NewSpace, Probe, Space};
use confed_core::error::{ConfedError, ExitCode, Result, SPACE_KEY_RULE};
use serde_json::{json, Value};
use std::fmt::Write;

/// The longest key either flavor takes.
pub const MAX_KEY_LEN: usize = 255;

pub async fn run(ctx: &mut Context, command: &SpaceCommand) -> Result<Output> {
    match command {
        SpaceCommand::Create(args) => create(ctx, args).await,
    }
}

async fn create(ctx: &mut Context, args: &SpaceCreateArgs) -> Result<Output> {
    let client = ctx.build_client()?;
    let new = new_space(ctx, args, client.flavor())?;
    let checks = check(client.as_ref(), &new, args.dry_run).await?;
    if args.dry_run {
        let steps = vec![create_step(&new, client.base_url())];
        return Ok(plan(ctx, &new, &checks, steps));
    }
    checks.refuse_taken()?;
    if !confirmed(
        ctx,
        &format!("Create space {} ({}) on {}?", new.key, new.name, client.base_url()),
    )? {
        return Ok(cancelled());
    }
    let space = client.create_space(&new).await?;
    let mut human = created_line(ctx, &space, client.as_ref());
    let _ = writeln!(
        human,
        "{}",
        ctx.style.dim(&format!("`confed clone {}` makes a workspace of it.", space.id.key))
    );
    Ok(Output::new(
        json!({ "action": "create", "dry_run": false, "space": space_json(&space, client.as_ref()) }),
        human,
    ))
}

// -------------------------------------------------------------- checking ---

/// The space to create, from the arguments: its key checked, its name given
/// or asked for.
pub fn new_space(ctx: &Context, args: &SpaceCreateArgs, flavor: Flavor) -> Result<NewSpace> {
    // A `~` key is a personal space's: [`check`] says whether it is taken
    // before calling it invalid.
    if !args.key.starts_with('~') {
        validate_key(&args.key, flavor)?;
    }
    let name = match args.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        Some(_) => return Err(name_required("--name is empty")),
        None if ctx.is_interactive() => {
            let name = crate::prompt::read_line(&format!("Name of space {}: ", args.key))?;
            if name.is_empty() {
                return Err(name_required("no name was given"));
            }
            name
        }
        None => return Err(name_required("--name is missing")),
    };
    Ok(NewSpace {
        key: args.key.clone(),
        name,
        description: args.description.clone().filter(|d| !d.trim().is_empty()),
        private: args.private,
    })
}

fn name_required(why: &str) -> ConfedError {
    ConfedError::refused(
        "name_required",
        ExitCode::Usage,
        format!("a new space needs a name: {why}"),
        Some("pass --name \"…\" (prompts are off without a terminal, and under --json)".into()),
    )
}

/// A key either flavor takes: `A-Z` and `0-9`, 1 to [`MAX_KEY_LEN`] long.
/// Personal spaces (`~name`) are made from a person's profile, never here.
pub fn validate_key(key: &str, flavor: Flavor) -> Result<()> {
    let flavor_name = match flavor {
        Flavor::DataCenter => "Data Center",
        Flavor::Cloud => "Cloud",
    };
    let problem = if key.is_empty() {
        Some("it is empty".to_string())
    } else if key.starts_with('~') {
        Some("`~` keys are personal spaces, which are made from a person's profile".to_string())
    } else if key.len() > MAX_KEY_LEN {
        Some(format!(
            "it is {} characters, and Confluence {flavor_name} takes at most {MAX_KEY_LEN}",
            key.len()
        ))
    } else if !key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        let upper = key.to_ascii_uppercase();
        if upper.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
            Some(format!("it is not uppercase; did you mean {upper}?"))
        } else {
            Some("it has characters other than letters A-Z and digits".to_string())
        }
    } else {
        None
    };
    match problem {
        None => Ok(()),
        Some(why) => Err(ConfedError::refused(
            "invalid_key",
            ExitCode::Usage,
            format!("{key:?} is not a space key: {why}"),
            Some(SPACE_KEY_RULE.to_string()),
        )),
    }
}

/// What the server said about creating the space, before creating it.
pub struct Checks {
    /// The space that has the key already, if one does.
    pub taken_by: Option<Space>,
    /// Asked only for a dry run; `None` otherwise.
    pub permission: Option<Probe>,
}

impl Checks {
    /// Every reason the create would be refused, as errors with their kind.
    pub fn problems(&self, key: &str) -> Vec<ConfedError> {
        let mut out = Vec::new();
        if let Some(space) = &self.taken_by {
            out.push(key_taken(key, space));
        }
        if self.permission == Some(Probe::Unavailable) {
            out.push(ConfedError::refused(
                "no_permission",
                ExitCode::State,
                "this account may not create spaces: it lacks the global \"Create Space\" \
                 permission",
                Some("ask a Confluence administrator for it, or to create the space".into()),
            ));
        }
        out
    }

    /// Refuse a key a space has, before asking the server to create it.
    pub fn refuse_taken(&self) -> Result<()> {
        match &self.taken_by {
            Some(space) => Err(key_taken(&space.id.key, space)),
            None => Ok(()),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "key_free": self.taken_by.is_none(),
            "taken_by": self.taken_by.as_ref().map(|s| json!({
                "key": s.id.key, "name": s.name, "status": s.status, "type": s.kind,
            })),
            "may_create": match self.permission {
                Some(Probe::Available) => json!(true),
                Some(Probe::Unavailable) => json!(false),
                _ => Value::Null,
            },
        })
    }
}

fn key_taken(key: &str, space: &Space) -> ConfedError {
    let what = match (space.status.as_deref(), space.id.key.starts_with('~')) {
        (_, true) => "a personal space".to_string(),
        (Some("archived"), _) => format!("the archived space \"{}\"", space.name),
        _ => format!("the space \"{}\"", space.name),
    };
    ConfedError::refused(
        "key_taken",
        ExitCode::State,
        format!("space key {key} is taken: it belongs to {what}"),
        Some("choose another key; archived and personal spaces keep theirs".into()),
    )
}

/// Ask the server whether the key is free and — for a dry run — whether this
/// account may create spaces. A space this account cannot see is not found
/// here; the create itself says so then.
pub async fn check(client: &dyn ConfluenceClient, new: &NewSpace, dry_run: bool) -> Result<Checks> {
    let taken_by = client.find_space(&new.key).await?;
    if taken_by.is_none() {
        validate_key(&new.key, client.flavor())?;
    }
    let permission = if dry_run { Some(client.probe_create_space().await?) } else { None };
    Ok(Checks { taken_by, permission })
}

// ------------------------------------------------------------- reporting ---

/// The browser URL of a space.
pub fn space_url(client: &dyn ConfluenceClient, key: &str) -> String {
    match client.flavor() {
        Flavor::DataCenter => format!("{}/display/{key}", client.base_url()),
        Flavor::Cloud => format!("{}/spaces/{key}", client.base_url()),
    }
}

pub fn space_json(space: &Space, client: &dyn ConfluenceClient) -> Value {
    json!({
        "key": space.id.key,
        "id": space.id.numeric,
        "name": space.name,
        "home_page_id": space.homepage_id.as_ref().map(|p| p.as_str().to_string()),
        "url": space_url(client, &space.id.key),
    })
}

pub fn create_step(new: &NewSpace, base_url: &str) -> String {
    format!(
        "create space {} \"{}\"{} on {base_url}",
        new.key,
        new.name,
        if new.private { ", private" } else { "" }
    )
}

/// A dry run: what would be done, what the server said, and — when it would
/// be refused — each reason in `errors`, with exit 7 (or 2).
pub fn plan(ctx: &Context, new: &NewSpace, checks: &Checks, steps: Vec<String>) -> Output {
    let style = &ctx.style;
    let problems = checks.problems(&new.key);
    let mut human = format!("{}\nWould:\n", style.dim("Dry run — nothing was written."));
    for step in &steps {
        let _ = writeln!(human, "  {step}");
    }
    let _ = writeln!(human, "Checks:");
    let key_line = match &checks.taken_by {
        None => style.green("yes"),
        Some(space) => format!("{} — \"{}\" has it", style.red("no"), space.name),
    };
    let key_label = format!("key {} is free", new.key);
    let width = key_label.chars().count().max("may create spaces".len());
    let _ = writeln!(human, "  {key_label:<width$}  {key_line}");
    let may = match checks.permission {
        Some(Probe::Available) => style.green("yes"),
        Some(Probe::Unavailable) => style.red("no"),
        _ => style.dim("the server would not say; the create itself will"),
    };
    let _ = writeln!(human, "  {:<width$}  {may}", "may create spaces");
    let mut output = Output::new(
        json!({
            "action": "create",
            "dry_run": true,
            "space": {
                "key": new.key, "name": new.name, "description": new.description,
                "private": new.private,
            },
            "checks": checks.json(),
            "planned": steps,
        }),
        human,
    );
    if checks.permission == Some(Probe::Unknown) {
        output = output.warn(
            "the server did not say whether this account may create spaces; the create itself will",
        );
    }
    if let Some(first) = problems.first() {
        output.exit = first.exit_code();
    }
    output.with_errors(problems.iter().map(ErrorEntry::from_error).collect())
}

pub fn created_line(ctx: &Context, space: &Space, client: &dyn ConfluenceClient) -> String {
    let home = match &space.homepage_id {
        Some(id) => format!(", home page {id}"),
        None => String::new(),
    };
    format!(
        "{} space {} ({}){home}\n  {}\n",
        ctx.style.green("Created"),
        space.id.key,
        space.name,
        space_url(client, &space.id.key)
    )
}

/// As every confed write: asked on a terminal unless `--yes`; agents and
/// scripts (no terminal, `--json`) are not asked.
pub fn confirmed(ctx: &Context, question: &str) -> Result<bool> {
    if ctx.global.yes || !ctx.is_interactive() {
        return Ok(true);
    }
    crate::prompt::confirm(question, false)
}

pub fn cancelled() -> Output {
    Output::new(
        json!({ "action": "create", "dry_run": false, "cancelled": true }),
        "Cancelled: nothing was created.\n".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_uppercase_letters_and_digits_and_not_too_long() {
        for key in ["NOTES", "ENG2", "A", "9"] {
            assert!(validate_key(key, Flavor::DataCenter).is_ok(), "{key}");
        }
        let kind = |key: &str| validate_key(key, Flavor::Cloud).unwrap_err();
        for key in ["", "notes", "NO-TES", "ЗАМЕТКИ", "~alice", "NOTES "] {
            let e = kind(key);
            assert_eq!(e.kind(), Some("invalid_key"), "{key}");
            assert_eq!(e.exit_code(), ExitCode::Usage, "{key}");
        }
        assert!(kind("notes").to_string().contains("did you mean NOTES"));
        let long = "A".repeat(MAX_KEY_LEN + 1);
        assert!(kind(&long).to_string().contains("at most 255"));
        assert!(validate_key(&"A".repeat(MAX_KEY_LEN), Flavor::DataCenter).is_ok());
    }
}
