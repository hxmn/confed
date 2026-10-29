//! The commands that run in every registered space: `status`, `fetch`,
//! `pull`, `push`, `doctor` and `search`.
//!
//! Each runs the single-space command in each workspace, unchanged, and
//! reports the results side by side ([`super::report`]): a space's `result`
//! is exactly what the command prints there on its own.

use super::{each_space, report, selected, Connection};
use crate::cli::{
    DoctorArgs, FetchArgs, InstanceDoctorArgs, InstanceFetchArgs, InstancePullArgs,
    InstancePushArgs, InstanceSearchArgs, InstanceStatusArgs, PullArgs, PushArgs, StatusArgs,
};
use crate::commands::doctor::{self, Check};
use crate::commands::{agent_docs, status};
use crate::commands::{fetch as fetch_cmd, pull as pull_cmd, push as push_cmd};
use crate::context::Context;
use crate::output::{plural, Output};
use confed_api::{BodyFormat, PageId};
use confed_core::crossmove;
use confed_core::error::{ConfedError, ExitCode, Result};
use confed_core::instance;
use confed_core::session::SessionStore;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write;

pub async fn status(
    ctx: &mut Context,
    args: &InstanceStatusArgs,
    connection: Option<Connection>,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let members = selected(ctx, &instance)?;
    let clients = connection.map(|c| c.clients(ctx)).transpose()?;
    let single = StatusArgs { fetch: args.fetch, short: args.short, exit_code: args.exit_code };
    let single = &single;
    let runs = each_space(ctx, &instance, &members, clients.as_ref(), |mut child| async move {
        if single.fetch {
            status::run_with_fetch(&mut child, single).await
        } else {
            status::run(&mut child, single)
        }
    })
    .await;
    Ok(report(ctx, runs, json!({})))
}

pub async fn fetch(
    ctx: &mut Context,
    args: &InstanceFetchArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let members = selected(ctx, &instance)?;
    let clients = connection.clients(ctx)?;
    let single = FetchArgs { pages: Vec::new(), since: args.since.clone() };
    let single = &single;
    let runs = each_space(ctx, &instance, &members, Some(&clients), |mut child| async move {
        fetch_cmd::run(&mut child, single).await
    })
    .await;
    Ok(report(ctx, runs, json!({})))
}

pub async fn pull(
    ctx: &mut Context,
    args: &InstancePullArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let members = selected(ctx, &instance)?;
    let clients = connection.clients(ctx)?;
    let single = PullArgs {
        paths: Vec::new(),
        pages: Vec::new(),
        label: None,
        cql: None,
        no_fetch: args.no_fetch,
        force: false,
        reset: false,
        no_merge: args.no_merge,
        dry_run: args.dry_run,
        no_attachments: args.no_attachments,
        no_comments: args.no_comments,
    };
    let single = &single;
    let runs = each_space(ctx, &instance, &members, Some(&clients), |mut child| async move {
        pull_cmd::run(&mut child, single).await
    })
    .await;
    Ok(report(ctx, runs, json!({})))
}

/// Whether a push's JSON result has anything to send.
fn has_work(result: &Value) -> bool {
    [
        "created",
        "pushed",
        "deleted",
        "attachments_uploaded",
        "attachments_deleted",
        "comments_pending",
    ]
    .iter()
    .any(|k| result[*k].as_array().is_some_and(|a| !a.is_empty()))
}

/// Every space's dry run first, all of it shown; then, once confirmed, the
/// push in each space that has something to send.
pub async fn push(
    ctx: &mut Context,
    args: &InstancePushArgs,
    connection: Connection,
) -> Result<Output> {
    let message = args.message.clone();
    if message.as_deref().is_none_or(str::is_empty) && !args.dry_run {
        return Err(ConfedError::usage_with_hint(
            "`confed instance push` needs a version comment",
            "pass -m \"why\"; it is recorded on every page it updates, in every space",
        ));
    }
    let instance = ctx.instance()?;
    let members = selected(ctx, &instance)?;
    let clients = connection.clients(ctx)?;
    let single = |dry_run: bool| PushArgs {
        paths: Vec::new(),
        dry_run,
        show_storage: false,
        interactive: false,
        message: message.clone(),
        allow_delete: args.allow_delete,
        no_attachments: args.no_attachments,
        no_comments: args.no_comments,
    };

    let preview = single(true);
    let preview = &preview;
    let planned = each_space(ctx, &instance, &members, Some(&clients), |mut child| async move {
        push_cmd::run(&mut child, preview).await
    })
    .await;
    let with_work: Vec<_> = planned
        .iter()
        .filter(|run| run.outcome.as_ref().is_ok_and(|o| has_work(&o.result)))
        .map(|run| run.member.clone())
        .collect();

    if args.dry_run || with_work.is_empty() {
        let mut output = report(ctx, planned, json!({ "dry_run": true }));
        if with_work.is_empty() && !args.dry_run {
            output.human.push_str(&ctx.style.green("Nothing to push in any space.\n"));
        }
        return Ok(output);
    }

    // Show it all, then ask — or be told.
    let preview = report(ctx, planned, json!({ "dry_run": true }));
    if !ctx.global.yes {
        if !ctx.is_interactive() {
            return Err(ConfedError::usage_with_hint(
                format!(
                    "{} would push; `confed instance push` asks before sending",
                    plural(with_work.len(), "space", "spaces")
                ),
                "review with `confed instance push --dry-run`, then pass --yes",
            ));
        }
        eprintln!("{}", preview.human.trim_end());
        let question = format!(
            "Push {} with \"{}\"?",
            plural(with_work.len(), "space", "spaces"),
            message.clone().unwrap_or_default()
        );
        if !crate::prompt::confirm(&question, false)? {
            return Ok(Output::new(json!({ "cancelled": true, "spaces": [] }), "Cancelled.\n"));
        }
    }

    let real = single(false);
    let real = &real;
    let runs = each_space(ctx, &instance, &with_work, Some(&clients), |mut child| async move {
        push_cmd::run(&mut child, real).await
    })
    .await;
    Ok(report(ctx, runs, json!({ "dry_run": false })))
}

pub async fn doctor(
    ctx: &mut Context,
    args: &InstanceDoctorArgs,
    connection: Option<Connection>,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let members = selected(ctx, &instance)?;
    let root = instance.root().to_path_buf();
    let mut checks: Vec<Check> = Vec::new();
    checks.push(Check::pass("instance root", root.display().to_string()));

    // --- the root's own files -------------------------------------------
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
    let missing: Vec<&str> = instance::IGNORED_PATTERNS
        .iter()
        .filter(|p| !gitignore.lines().any(|l| l.trim().trim_start_matches('/') == **p))
        .copied()
        .collect();
    if missing.is_empty() {
        checks.push(Check::pass(
            "root gitignore",
            "instance state and every space's state are ignored",
        ));
    } else if args.fix {
        instance::ensure_gitignore(&root)?;
        checks.push(
            Check::warn("root gitignore", format!("not ignored: {}", missing.join(", "))).fixed(),
        );
    } else {
        checks.push(Check::warn(
            "root gitignore",
            format!("not ignored: {} (run with --fix)", missing.join(", ")),
        ));
    }

    let (missing_docs, stale_docs) = agent_docs::audit(&root);
    if missing_docs.is_empty() && stale_docs.is_empty() {
        checks.push(Check::pass(
            "root agent docs",
            format!("written by confed {}", agent_docs::VERSION),
        ));
    } else if args.fix {
        super::setup::refresh_root_docs(&instance).ok();
        if !missing_docs.is_empty() {
            let base_url = instance.base_url()?.unwrap_or_default();
            let flavor = instance.flavor()?.unwrap_or(confed_api::Flavor::DataCenter);
            agent_docs::write_instance(&root, &base_url, flavor, &instance.members()?)?;
        }
        checks.push(Check::warn("root agent docs", "regenerated").fixed());
    } else {
        let mut problems: Vec<String> =
            missing_docs.iter().map(|n| format!("{n} is missing")).collect();
        problems.extend(
            stale_docs
                .iter()
                .map(|(n, by)| format!("{n} was written by {}", agent_docs::writer(by))),
        );
        checks.push(Check::warn(
            "root agent docs",
            format!("{} (run with --fix)", problems.join("; ")),
        ));
    }

    if SessionStore::instance_exists(&root) {
        checks.push(Check::pass(
            "shared credential",
            confed_core::session::INSTANCE_SESSION_DB_FILENAME,
        ));
    } else {
        checks.push(Check::warn(
            "shared credential",
            "none in the root; each workspace uses its own (`confed instance adopt \
             --move-credentials` shares one)",
        ));
    }

    // --- every member --------------------------------------------------
    let mut spaces: BTreeMap<String, confed_core::workspace::Workspace> = BTreeMap::new();
    for member in instance.members()? {
        match instance.open_workspace(&member) {
            Ok(ws) => {
                let bound = ws.space_key().unwrap_or_default();
                if !bound.eq_ignore_ascii_case(&member.space_key) {
                    checks.push(Check::fail(
                        "registration",
                        format!(
                            "{} is registered for {} but bound to {bound}",
                            member.dir, member.space_key
                        ),
                    ));
                }
                spaces.insert(member.space_key.clone(), ws);
            }
            Err(e) => checks.push(Check::fail("registration", e.to_string())),
        }
    }

    let duplicates = crossmove::duplicates(&spaces)?;
    if duplicates.is_empty() {
        checks.push(Check::pass("pages tracked once", "no page is in two workspaces"));
    } else {
        let list = duplicates
            .iter()
            .map(|d| format!("{} in {}", d.page_id, d.spaces.join(" and ")))
            .collect::<Vec<_>>()
            .join("; ");
        checks.push(Check::fail(
            "pages tracked once",
            format!("{list}; an interrupted move: `confed instance resume`"),
        ));
    }

    let unfinished = instance.journal_unfinished()?;
    if unfinished.is_empty() {
        checks.push(Check::pass("journal", "no unfinished moves"));
    } else {
        let list = unfinished
            .iter()
            .map(|e| {
                let error =
                    e.error.as_deref().map(|m| format!(", stopped: {m}")).unwrap_or_default();
                format!("{} after `{}`{error}", e.id, e.state.as_str())
            })
            .collect::<Vec<_>>()
            .join("; ");
        checks
            .push(Check::fail("journal", format!("{list}; `confed instance resume` finishes it")));
    }

    // --- the server: where each tracked page really is ------------------
    let clients = connection.map(|c| c.clients(ctx)).transpose()?;
    if let Some(client) = clients.as_ref().map(|c| &c.shared) {
        if args.probe_move {
            let page = spaces
                .values()
                .find_map(|ws| ws.state().all_pages().ok()?.into_iter().next())
                .map(|p| PageId::new(p.page_id));
            checks.push(doctor::move_check(client.as_ref(), page.as_ref()).await);
        }
        let mut astray = Vec::new();
        for (key, ws) in &spaces {
            let numeric = ws.space_numeric_id()?;
            let listed: std::collections::HashSet<String> =
                match client.list_pages(&confed_api::SpaceId { key: key.clone(), numeric }).await {
                    Ok(pages) => pages.into_iter().map(|p| p.id.0).collect(),
                    Err(e) => {
                        checks.push(Check::warn("page spaces", format!("{key}: {e}")));
                        continue;
                    }
                };
            for page in ws.state().all_pages()? {
                if listed.contains(&page.page_id) {
                    continue;
                }
                if let Ok(found) =
                    client.get_page(&PageId::new(&page.page_id), BodyFormat::Storage).await
                {
                    if !found.summary.space_key.is_empty() && &found.summary.space_key != key {
                        astray.push(format!(
                            "{key}:{} is in space {} on the server",
                            page.local_path, found.summary.space_key
                        ));
                    }
                }
            }
        }
        if astray.is_empty() {
            checks
                .push(Check::pass("page spaces", "every tracked page is in its workspace's space"));
        } else {
            checks.push(Check::fail(
                "page spaces",
                format!(
                    "{}; moved outside confed — pull the space it left, then pull the one \
                     it is in now",
                    astray.join("; ")
                ),
            ));
        }
    }
    drop(spaces);

    let root_report = doctor::report(ctx, checks);
    let failed = root_report.exit != ExitCode::Ok;

    // --- and `doctor` in each space ---------------------------------------
    // The probe is asked once, in the root; never in every space.
    let single = DoctorArgs { fix: args.fix, offline: args.offline, probe_move: false };
    let single = &single;
    let runs = each_space(ctx, &instance, &members, clients.as_ref(), |mut child| async move {
        doctor::run(&mut child, single).await
    })
    .await;
    let mut output = report(ctx, runs, json!({ "instance": root_report.result }));
    output.human = format!("{}\n{}", root_report.human.trim_end(), output.human);
    if failed && output.exit == ExitCode::Ok {
        output.exit = ExitCode::Error;
    }
    Ok(output)
}

/// CQL across every registered space; each result names its space, and the
/// workspace and file it is in when it has been pulled.
pub async fn search(
    ctx: &mut Context,
    args: &InstanceSearchArgs,
    connection: Connection,
) -> Result<Output> {
    if args.query.is_empty() {
        return Err(ConfedError::usage("give a search term or a CQL query"));
    }
    let instance = ctx.instance()?;
    let members = selected(ctx, &instance)?;
    let keys: Vec<String> = members.iter().map(|m| m.space_key.clone()).collect();
    let cql = build_cql(&args.query.join(" "), &keys);
    let client = connection.client(ctx)?;
    let results = client.search_cql(&cql, args.limit).await?;

    let mut paths: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    for member in &members {
        let Ok(ws) = instance.open_workspace(member) else { continue };
        for page in ws.state().all_pages()? {
            paths.insert(
                page.page_id,
                (member.space_key.clone(), member.dir.clone(), page.local_path),
            );
        }
    }

    let style = &ctx.style;
    let mut human = String::new();
    let mut rows = Vec::new();
    for result in &results {
        let local = paths.get(result.page_id.as_str());
        let space = result.space_key.clone().or_else(|| local.map(|l| l.0.clone()));
        let dir = local.map(|l| l.1.clone()).or_else(|| {
            space
                .as_ref()
                .and_then(|s| members.iter().find(|m| &m.space_key == s))
                .map(|m| m.dir.clone())
        });
        let where_ = match local {
            Some((_, dir, path)) => format!("{dir}/{path}"),
            None => result.url.clone(),
        };
        let _ = writeln!(
            human,
            "{:<8} {:<40} {}",
            space.clone().unwrap_or_default(),
            result.title,
            style.dim(&where_)
        );
        rows.push(json!({
            "page_id": result.page_id.0,
            "title": result.title,
            "space": space,
            "dir": dir,
            "local_path": local.map(|l| l.2.clone()),
            "url": result.url,
            "excerpt": result.excerpt,
        }));
    }
    if results.is_empty() {
        let _ = writeln!(human, "No results for {cql}");
    }
    Ok(Output::new(json!({ "cql": cql, "results": rows }), human))
}

/// Plain words become a text search in the registered spaces; CQL is scoped
/// to them too, unless it names spaces itself.
fn build_cql(raw: &str, keys: &[String]) -> String {
    let lower = raw.to_lowercase();
    let looks_like_cql =
        ["=", "~", " and ", " or ", "(", "currentuser()"].iter().any(|t| lower.contains(t));
    let spaces = keys
        .iter()
        .map(|k| format!("\"{}\"", k.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(",");
    let scope = if keys.is_empty() { String::new() } else { format!("space in ({spaces}) and ") };
    if looks_like_cql {
        if lower.contains("space") {
            return raw.to_string();
        }
        return format!("{scope}({raw})");
    }
    format!("{scope}text ~ \"{}\"", raw.replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_scoped_to_the_registered_spaces() {
        let keys = vec!["DOCS".to_string(), "ENG".to_string()];
        assert_eq!(
            build_cql("onboarding", &keys),
            r#"space in ("DOCS","ENG") and text ~ "onboarding""#
        );
        assert_eq!(
            build_cql("label = runbook", &keys),
            r#"space in ("DOCS","ENG") and (label = runbook)"#
        );
        assert_eq!(
            build_cql(r#"space = "OPS" and type = page"#, &keys),
            r#"space = "OPS" and type = page"#
        );
    }

    #[test]
    fn a_push_result_with_nothing_in_it_has_no_work() {
        assert!(!has_work(&json!({ "pushed": [], "created": [], "skipped": [{}] })));
        assert!(has_work(&json!({ "pushed": [{ "page_id": "1" }] })));
        assert!(has_work(&json!({ "comments_pending": ["1001: add comment"] })));
    }
}
