//! `confed instance create KEY --name …` — create a space with the root's
//! shared credential, then clone it into `ROOT/KEY` as `instance clone KEY`
//! does, which registers it and lists it in the root's `CLAUDE.md`.
//!
//! Everything that can be checked is checked before the space exists: the
//! key, the name, the folder it would be cloned into, and whether a space has
//! the key already. A clone that fails after the space was made says so and
//! names `confed instance clone KEY`, which finishes it: the space is never
//! created twice.

use super::{setup, Connection};
use crate::cli::{InstanceCloneArgs, InstanceCreateArgs};
use crate::commands::space;
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
    if args.space.dry_run {
        let mut steps = vec![space::create_step(&new, &connection.base_url)];
        if !args.no_clone {
            steps.push(format!("clone it into {rel}/ and register it in this root"));
            if !args.no_agent_docs {
                steps.push("list it in the root's CLAUDE.md and AGENTS.md".into());
            }
        }
        let mut output = space::plan(ctx, &new, &checks, steps);
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
    let mut human = space::created_line(ctx, &created, client.as_ref());
    let space_json = space::space_json(&created, client.as_ref());
    if args.no_clone {
        let _ = writeln!(
            human,
            "{}",
            ctx.style.dim(&format!("`confed instance clone {key}` clones it into {rel}/."))
        );
        return Ok(Output::new(
            json!({ "dry_run": false, "space": space_json, "cloned": Value::Null }),
            human,
        ));
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
        &connection,
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
        return Ok(output);
    }

    let pages = entry["pages"].as_u64().unwrap_or(0);
    let dir = entry["dir"].as_str().unwrap_or(&rel).to_string();
    let _ = writeln!(
        human,
        "{} into {dir}/ ({})",
        ctx.style.green("Cloned"),
        crate::output::plural(pages as usize, "page", "pages")
    );
    Ok(Output::new(
        json!({
            "dry_run": false,
            "space": space_json,
            "cloned": { "dir": dir, "pages": pages },
        }),
        human,
    )
    .warn_all(warnings))
}
