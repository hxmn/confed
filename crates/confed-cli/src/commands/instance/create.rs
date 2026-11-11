//! `confed instance create KEY --name …` — create a space with the root's
//! shared credential, then clone it into `ROOT/KEY` as `instance clone KEY`
//! does, which registers it and lists it in the root's `CLAUDE.md`.
//!
//! Everything that can be checked is checked before the space exists: the
//! key, the name, the folder it would be cloned into, and whether a space has
//! the key already. A clone that fails after the space was made says so and
//! names `confed instance clone KEY`, which finishes it: the space is never
//! created twice.
//!
//! `--permissions-from SRC` then grants in the new space what subjects hold in
//! SRC, as `confed admin perms copy` does. What it needs is checked first too
//! — that SRC can be read, that every `--map` names somebody who holds
//! something there, that whoever it maps to exists or is a group to be made —
//! and a copy that fails after the create says so the same way.

use super::{setup, Connection};
use crate::cli::{InstanceCloneArgs, InstanceCreateArgs};
use crate::commands::{admin, space};
use crate::context::Context;
use crate::output::{ErrorEntry, Output};
use confed_core::error::{ConfedError, ExitCode, Result};
use confed_core::instance;
use serde_json::{json, Value};
use std::fmt::Write;

pub async fn run(
    ctx: &mut Context,
    args: &InstanceCreateArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let client = connection.client(ctx)?;
    let new = space::new_space(ctx, &args.space, connection.flavor)?;
    let key = new.key.clone();
    let rel = instance::default_dir(&key);

    // The folder the clone needs, before the server is asked anything.
    let dir = instance.root().join(&rel);
    let folder_taken =
        !args.no_clone && dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false);
    let folder_error = || {
        ConfedError::refused(
            "dir_not_empty",
            ExitCode::State,
            format!("{rel}/ exists and is not empty, so the new space could not be cloned there"),
            Some(format!("move {rel}/ away, or create the space with --no-clone")),
        )
    };

    let checks = space::check(client.as_ref(), &new, args.space.dry_run).await?;
    let permissions = match &args.permissions_from {
        Some(source) => Some(
            admin::plan_permissions(&client, source, &key, &args.map, args.create_groups).await?,
        ),
        None => None,
    };
    if args.space.dry_run {
        let mut steps = vec![space::create_step(&new, &connection.base_url)];
        if !args.no_clone {
            steps.push(format!("clone it into {rel}/ and register it in this root"));
            if !args.no_agent_docs {
                steps.push("list it in the root's CLAUDE.md and AGENTS.md".into());
            }
        }
        if let Some(plan) = &permissions {
            steps.extend(plan.steps());
        }
        let mut output = space::plan(ctx, &new, &checks, steps);
        if let Some(plan) = &permissions {
            output.result["permissions"] = plan.preview();
        }
        // A taken key's folder is that space's own workspace, as a rule: one
        // refusal says it all.
        if folder_taken && checks.taken_by.is_none() {
            let e = folder_error();
            if output.exit == ExitCode::Ok {
                output.exit = e.exit_code();
            }
            output.errors.push(ErrorEntry::from_error(&e));
        }
        output.result["root"] = json!(instance.root().display().to_string());
        output.result["dir"] = if args.no_clone { Value::Null } else { json!(rel) };
        return Ok(output);
    }

    checks.refuse_taken()?;
    if folder_taken {
        return Err(folder_error());
    }
    let question = format!("Create space {key} ({}) on {}?", new.name, connection.base_url);
    if !space::confirmed(ctx, &question)? {
        return Ok(space::cancelled());
    }

    let created = client.create_space(&new).await?;
    let granted = match &permissions {
        Some(plan) => Some((plan.from(), admin::apply_permissions(&client, plan, &key).await)),
        None => None,
    };
    let output = clone(ctx, args, &connection, client.clone(), &created, &rel).await;
    let Some((source, granted)) = granted else { return Ok(output) };
    Ok(with_permissions(output, &key, source, granted))
}

/// The space is there: say so, and clone it unless told not to.
async fn clone(
    ctx: &mut Context,
    args: &InstanceCreateArgs,
    connection: &Connection,
    client: std::sync::Arc<dyn confed_api::ConfluenceClient>,
    created: &confed_api::Space,
    rel: &str,
) -> Output {
    let key = &created.id.key;
    let mut human = space::created_line(ctx, created, client.as_ref());
    let space_json = space::space_json(created, client.as_ref());
    if args.no_clone {
        let _ = writeln!(
            human,
            "{}",
            ctx.style.dim(&format!("`confed instance clone {key}` clones it into {rel}/."))
        );
        return Output::new(
            json!({ "dry_run": false, "space": space_json, "cloned": Value::Null }),
            human,
        );
    }

    let clone_args = InstanceCloneArgs {
        keys: vec![created.id.key.clone()],
        all: false,
        exclude: Vec::new(),
        space_type: None,
        include_archived: false,
        force: false,
        dry_run: false,
        no_agent_docs: args.no_agent_docs,
        no_attachments: false,
        no_comments: false,
    };
    let cloned = setup::clone_spaces(
        ctx,
        &clone_args,
        connection,
        client.clone(),
        vec![created.clone()],
        Vec::new(),
    )
    .await;
    let (entry, failure, warnings) = match cloned {
        Ok(out) => {
            let entry = out.result["spaces"][0].clone();
            let failure =
                (out.exit != ExitCode::Ok || entry["status"] != json!("cloned")).then(|| {
                    out.errors
                        .first()
                        .map(|e| e.message.clone())
                        .unwrap_or_else(|| format!("it ended with exit code {}", out.exit.as_i32()))
                });
            (entry, failure, out.warnings)
        }
        Err(e) => (Value::Null, Some(e.to_string()), Vec::new()),
    };

    if let Some(why) = failure {
        let e = ConfedError::refused(
            "clone_failed",
            ExitCode::Partial,
            format!(
                "space {key} was created on the server, but cloning it into {rel}/ failed: {why}"
            ),
            Some(format!(
                "`confed instance clone {key}` finishes the job; the space exists, so never \
                 create it again"
            )),
        );
        let mut output = Output::new(
            json!({ "dry_run": false, "space": space_json, "cloned": Value::Null, "clone": entry }),
            human,
        )
        .with_errors(vec![ErrorEntry::from_error(&e)])
        .warn_all(warnings);
        output.exit = ExitCode::Partial;
        return output;
    }

    let pages = entry["pages"].as_u64().unwrap_or(0);
    let dir = entry["dir"].as_str().unwrap_or(rel).to_string();
    let _ = writeln!(
        human,
        "{} into {dir}/ ({})",
        ctx.style.green("Cloned"),
        crate::output::plural(pages as usize, "page", "pages")
    );
    Output::new(
        json!({
            "dry_run": false,
            "space": space_json,
            "cloned": { "dir": dir, "pages": pages },
        }),
        human,
    )
    .warn_all(warnings)
}

/// Add what `--permissions-from` came to. Whatever failed, the space exists:
/// it is a partial success, and `confed admin perms copy` finishes the job.
fn with_permissions(
    mut output: Output,
    key: &str,
    source: &str,
    granted: Result<admin::PermissionsApplied>,
) -> Output {
    let finish = format!(
        "`confed admin perms copy --from {source} --to {key}` finishes the job, and grants \
         nothing twice; the space exists, so never create it again"
    );
    let failed = match granted {
        Ok(applied) => {
            output.result["permissions"] = applied.result;
            let _ = writeln!(output.human, "Permissions from {source}:");
            for line in applied.human.lines() {
                let _ = writeln!(output.human, "  {line}");
            }
            applied.failed
        }
        Err(e) => {
            output.result["permissions"] = Value::Null;
            vec![ConfedError::refused(
                "permissions_failed",
                ExitCode::Partial,
                format!(
                    "space {key} was created, but copying the permissions of {source} into it \
                     failed: {e}"
                ),
                Some(finish.clone()),
            )]
        }
    };
    if !failed.is_empty() {
        output.exit = ExitCode::Partial;
        output.errors.extend(failed.iter().map(ErrorEntry::from_error));
        output = output.warn(format!("not every permission of {source} was copied: {finish}"));
    }
    output
}
