//! `confed instance mv`, `resume` and `abort`: moving pages between spaces.
//! The work is [`confed_core::crossmove`]'s; this is the part that reads
//! arguments, takes the locks, asks, and reports.

use super::locate::{locate, Found};
use super::Connection;
use crate::cli::{InstanceJournalArgs, InstanceMvArgs};
use crate::context::Context;
use crate::output::{plural, Output};
use confed_api::{ConfluenceClient, MovePosition, SpaceId};
use confed_core::crossmove::{self, MovePlan, MoveRequest, OnConflict, SpaceRef};
use confed_core::error::{ConfedError, Result};
use confed_core::instance::{Instance, JournalEntry, Member, Step};
use confed_core::lock::WorkspaceLock;
use confed_core::paths;
use confed_core::sync::{FetchOptions, PullOptions, PushOptions, SyncEngine};
use confed_core::workspace::Workspace;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::Path;
use std::sync::Arc;

fn space_ref(member: &Member, ws: &Workspace) -> Result<SpaceRef> {
    Ok(SpaceRef {
        key: member.space_key.clone(),
        dir: member.dir.clone(),
        numeric: ws.space_numeric_id()?.or(member.space_id.clone()),
    })
}

/// Take the locks of these spaces, in key order, so two instance commands
/// never wait on each other in opposite orders. A held one is exit 7.
fn lock_all(spaces: &BTreeMap<String, Workspace>, keys: &[String]) -> Result<Vec<WorkspaceLock>> {
    let mut keys = keys.to_vec();
    keys.sort();
    keys.dedup();
    let mut locks = Vec::new();
    for key in keys {
        if let Some(ws) = spaces.get(&key) {
            locks.push(ws.lock().map_err(|e| match e {
                ConfedError::State { message, hint } => {
                    ConfedError::State { message: format!("{key}: {message}"), hint }
                }
                other => other,
            })?);
        }
    }
    Ok(locks)
}

fn open_all(instance: &Instance) -> Result<BTreeMap<String, Workspace>> {
    let mut spaces = BTreeMap::new();
    for member in instance.enabled_members()? {
        if let Ok(ws) = instance.open_workspace(&member) {
            spaces.insert(member.space_key.clone(), ws);
        }
    }
    Ok(spaces)
}

fn engine_for(
    ctx: &Context,
    client: &Arc<dyn ConfluenceClient>,
    ws: &Workspace,
    concurrency: usize,
) -> Result<SyncEngine> {
    let key = ws.space_key()?;
    Ok(SyncEngine::new(
        client.clone(),
        SpaceId { key: key.clone(), numeric: ws.space_numeric_id()? },
        concurrency,
    )
    .with_progress(ctx.progress_for(Some(key))))
}

pub async fn run(
    ctx: &mut Context,
    args: &InstanceMvArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let client = connection.client(ctx)?;
    let named = args.sources();
    match (&args.children_of, named.is_empty()) {
        (None, true) => {
            return Err(ConfedError::usage_with_hint(
                "name the pages to move, then where to",
                "`confed instance mv SOURCE… DEST`, or `--children-of PAGE DEST`",
            ))
        }
        (Some(_), false) => {
            return Err(ConfedError::usage(
                "with --children-of, give only the destination; the pages are its children",
            ))
        }
        _ => {}
    }
    let dest = locate(ctx, &instance, Some(&client), args.destination(), true).await?;
    let mut sources = Vec::new();
    for reference in named {
        sources.push(locate(ctx, &instance, Some(&client), reference, false).await?);
    }
    // The parent whose children move: a page, or a space's home page.
    let children_of = match &args.children_of {
        Some(reference) => {
            Some(match locate(ctx, &instance, Some(&client), reference, true).await? {
                Found::Page { member, page_id } => (member, page_id),
                Found::SpaceTop { member } => {
                    let home = client.get_space(&member.space_key).await?.homepage_id.ok_or_else(
                        || {
                            ConfedError::NotFound(format!(
                                "space {} has no home page",
                                member.space_key
                            ))
                        },
                    )?;
                    (member, home.0)
                }
                Found::LocalOnly { path, .. } | Found::NewPath { path, .. } => {
                    return Err(ConfedError::NotFound(format!(
                        "{path} is no page on the server yet"
                    )))
                }
            })
        }
        None => None,
    };
    let src_member = match (&children_of, sources.first()) {
        (Some((member, _)), _) => member.clone(),
        (None, Some(first)) => first.member().clone(),
        (None, None) => unreachable!("refused above"),
    };
    if let Some(other) = sources.iter().find(|s| s.member().space_key != src_member.space_key) {
        return Err(ConfedError::usage_with_hint(
            format!(
                "the pages are in spaces {} and {}; one move takes pages from one space",
                src_member.space_key,
                other.member().space_key
            ),
            "move each space's pages with a `confed instance mv` of its own",
        ));
    }
    if src_member.space_key == dest.member().space_key {
        return Err(ConfedError::usage_with_hint(
            format!("both are in space {}", src_member.space_key),
            format!("within a space, use `confed -C {} mv`", src_member.dir),
        ));
    }
    if let [Found::LocalOnly { member, path }] = sources.as_slice() {
        return move_local_file(&instance, member, path, &dest);
    }
    let mut root_ids = Vec::new();
    for (source, reference) in sources.iter().zip(named) {
        match source {
            Found::Page { page_id, .. } => root_ids.push(page_id.clone()),
            Found::LocalOnly { path, .. } => {
                return Err(ConfedError::usage_with_hint(
                    format!("{path} has not been pushed, so it cannot move with the others"),
                    "push it first, or move it on its own",
                ))
            }
            _ => return Err(ConfedError::usage(format!("`{reference}` is not a page"))),
        }
    }
    if let Found::LocalOnly { path, .. } = &dest {
        return Err(ConfedError::state_with_hint(
            format!("{path} has not been pushed, so nothing can be put under it on the server"),
            "push it first",
        ));
    }
    let several = root_ids.len() > 1 || children_of.is_some();
    if several && matches!(dest, Found::NewPath { .. }) {
        return Err(ConfedError::usage(
            "a new path names one page; to move several, name the parent they go under",
        ));
    }
    if args.push_first && args.message.as_deref().is_none_or(str::is_empty) {
        return Err(ConfedError::usage("--push-first needs -m \"why\""));
    }

    let concurrency = ctx.concurrency();
    let dest_member = dest.member().clone();
    let mut spaces = open_all(&instance)?;
    let src_ref = space_ref(&src_member, &spaces[&src_member.space_key])?;
    let dst_ref = space_ref(&dest_member, &spaces[&dest_member.space_key])?;

    // Both sides locked and fetched before the plan.
    let mut locks = lock_all(&spaces, &[src_ref.key.clone(), dst_ref.key.clone()])?;
    if args.push_first {
        let ws = spaces.get_mut(&src_ref.key).expect("opened");
        let roots = match &children_of {
            Some((_, parent)) => children(ws, parent)?,
            None => root_ids.clone(),
        };
        let mut scope = Vec::new();
        for root in &roots {
            if let Some(page) = ws.state().get_page(root)? {
                scope.push(page.local_path.clone());
                scope.push(format!("{}/**", paths::children_dir(&page.local_path)));
            }
        }
        let engine = engine_for(ctx, &client, ws, concurrency)?;
        let pushed = engine
            .push(
                ws,
                &PushOptions {
                    scope,
                    message: args.message.clone(),
                    with_attachments: true,
                    with_comments: true,
                    ..Default::default()
                },
            )
            .await?;
        if !pushed.failed.is_empty() || !pushed.skipped.is_empty() {
            return Err(ConfedError::state(format!(
                "--push-first could not push everything: {}",
                pushed
                    .failed
                    .iter()
                    .map(|f| format!("{}: {}", f.page_id, f.error))
                    .chain(pushed.skipped.iter().map(|s| format!("{}: {}", s.path, s.reason)))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
    }
    for key in [&src_ref.key, &dst_ref.key] {
        let ws = spaces.get_mut(key).expect("opened");
        engine_for(ctx, &client, ws, concurrency)?.fetch(ws, &FetchOptions::default()).await?;
    }
    if let Some((_, parent)) = &children_of {
        root_ids = children(&spaces[&src_ref.key], parent)?;
        if root_ids.is_empty() {
            return Err(ConfedError::NotFound(format!(
                "page {parent} has no pages under it in space {}",
                src_ref.key
            )));
        }
    }

    // What the move names but the workspaces do not have yet: the pages to
    // move, the destination and the pages beside it. Pulled, and only them.
    let mut pulled = Vec::new();
    {
        let ws = spaces.get_mut(&src_ref.key).expect("opened");
        let wanted = with_subtrees(ws, &root_ids)?;
        pulled.extend(pull_missing(ctx, &client, ws, wanted, concurrency).await?);
    }
    {
        let mut named_there = Vec::new();
        if let Found::Page { page_id, .. } = &dest {
            named_there.push(page_id.clone());
        }
        if let Found::NewPath { parent: Some(parent), .. } = &dest {
            named_there.push(parent.clone());
        }
        for sibling in [&args.before, &args.after].into_iter().flatten() {
            if let Found::Page { member, page_id } =
                locate(ctx, &instance, Some(&client), sibling, false).await?
            {
                if member.space_key != dst_ref.key {
                    return Err(ConfedError::usage(format!(
                        "`{sibling}` is in space {}, not in {}",
                        member.space_key, dst_ref.key
                    )));
                }
                named_there.push(page_id);
            }
        }
        let ws = spaces.get_mut(&dst_ref.key).expect("opened");
        let wanted = with_ancestors(ws, &named_there)?;
        pulled.extend(pull_missing(ctx, &client, ws, wanted, concurrency).await?);
    }

    let (position, target_id, new_parent_id) =
        placement(ctx, &instance, &client, &spaces[&dst_ref.key], &dest, args).await?;
    let request = MoveRequest {
        roots: root_ids,
        source: src_ref,
        dest: dst_ref,
        position,
        target_id,
        new_parent_id,
        new_title: match &dest {
            Found::NewPath { title, .. } => Some(title.clone()),
            _ => None,
        },
        on_conflict: on_conflict(args)?,
        rewrite_links: !args.no_rewrite_links,
        relativize: args.relativize,
        message: args.message.clone().or_else(|| Some("Moved by confed instance mv".into())),
        wait_secs: args.wait,
    };
    let mut plan = crossmove::draft(&client, &spaces, &request).await?;
    let with_pulled = |mut output: Output| {
        output.result["pulled"] = json!(pulled);
        if !pulled.is_empty() {
            output.human = format!(
                "{}\n{}",
                ctx.style.dim(&format!(
                    "Pulled {} the move needs: {}",
                    plural(pulled.len(), "page", "pages"),
                    pulled
                        .iter()
                        .map(|p| format!(
                            "{}:{}",
                            p["space"].as_str().unwrap_or_default(),
                            p["path"].as_str().unwrap_or_default()
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                output.human
            );
        }
        output
    };

    if let Some(refusal) = plan.refusal() {
        // Everything is in the plan to read; what stops it, too.
        let mut output = describe(ctx, &plan, None, true);
        output.exit = refusal.exit_code();
        return Ok(with_pulled(
            output.with_errors(vec![crate::output::ErrorEntry::from_error(&refusal)]),
        ));
    }
    if let Some(reason) = plan.blocked.clone() {
        // Everything else checked out: show the plan, and why it cannot run.
        let mut output = describe(ctx, &plan, None, true);
        output.result["blocked"] = json!(reason);
        output.exit = confed_core::ExitCode::Unsupported;
        return Ok(with_pulled(output.with_errors(vec![crate::output::ErrorEntry {
            code: confed_core::ExitCode::Unsupported.name(),
            kind: None,
            message: reason,
            hint: Some(
                "move the pages in the browser instead; then `confed instance pull` brings both \
                 workspaces up to date (the source drops them, the destination downloads them)"
                    .into(),
            ),
        }])));
    }
    if args.dry_run {
        return Ok(with_pulled(describe(ctx, &plan, None, true)));
    }
    if plan.rules_page.is_some() && !ctx.global.yes {
        return Err(ConfedError::usage_with_hint(
            format!("the move takes the rules page of {} with it", plan.source.key),
            "pass --yes to move it anyway; the source workspace's agent docs lose their rules",
        ));
    }
    if !ctx.global.yes {
        if !ctx.is_interactive() {
            return Err(ConfedError::usage_with_hint(
                format!(
                    "moving {} to {} changes the server; confed asks first",
                    plural(plan.pages.len(), "page", "pages"),
                    plan.dest.key
                ),
                "review it with --dry-run, then pass --yes",
            ));
        }
        eprintln!("{}", describe(ctx, &plan, None, true).human.trim_end());
        if !crate::prompt::confirm("Move them?", false)? {
            return Ok(Output::new(json!({ "cancelled": true }), "Cancelled.\n"));
        }
    }

    // Spaces whose links are rewritten are written to as well.
    let others: Vec<String> = plan
        .spaces_written()
        .into_iter()
        .filter(|k| k != &plan.source.key && k != &plan.dest.key)
        .collect();
    locks.extend(lock_all(&spaces, &others)?);

    let id = instance.journal_begin(crossmove::OP, &serde_json::to_value(&plan)?)?;
    crossmove::execute(&instance, &id, &client, &mut spaces, &mut plan, Step::Planned, concurrency)
        .await
        .map_err(|e| with_resume_hint(e, &id))?;
    after_move(&mut spaces, &plan)?;
    drop(locks);
    Ok(with_pulled(describe(ctx, &plan, Some(&id), false)))
}

/// The pages directly under `parent`, in their order, as the last fetch listed them.
fn children(ws: &Workspace, parent: &str) -> Result<Vec<String>> {
    let mut kids: Vec<_> = ws
        .state()
        .all_remote()?
        .into_iter()
        .filter(|r| !r.deleted && r.parent_id.as_deref() == Some(parent))
        .collect();
    kids.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));
    Ok(kids.into_iter().map(|r| r.page_id).collect())
}

/// These pages, every page under them, and the pages above them, as listed.
fn with_subtrees(ws: &Workspace, roots: &[String]) -> Result<Vec<String>> {
    let remote = ws.state().all_remote()?;
    let mut out = with_ancestors(ws, roots)?;
    let mut index = 0;
    let mut queue: Vec<String> = roots.to_vec();
    while index < queue.len() {
        let id = queue[index].clone();
        queue.extend(
            remote
                .iter()
                .filter(|r| !r.deleted && r.parent_id.as_deref() == Some(id.as_str()))
                .map(|r| r.page_id.clone()),
        );
        index += 1;
    }
    out.extend(queue);
    out.sort();
    out.dedup();
    Ok(out)
}

/// These pages and every page above them, as listed.
fn with_ancestors(ws: &Workspace, ids: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for id in ids {
        let mut at = Some(id.clone());
        while let Some(page) = at {
            if out.contains(&page) {
                break;
            }
            at = ws.state().get_remote(&page)?.and_then(|r| r.parent_id);
            out.push(page);
        }
    }
    Ok(out)
}

/// Pull the pages of `wanted` the workspace does not track yet — and nothing
/// else — from what its fetch just listed. Returns what it pulled.
async fn pull_missing(
    ctx: &Context,
    client: &Arc<dyn ConfluenceClient>,
    ws: &mut Workspace,
    wanted: Vec<String>,
    concurrency: usize,
) -> Result<Vec<serde_json::Value>> {
    let mut missing = Vec::new();
    for id in wanted {
        let listed = ws.state().get_remote(&id)?.is_some_and(|r| !r.deleted);
        if listed && ws.state().get_page(&id)?.is_none() {
            missing.push(id);
        }
    }
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let key = ws.space_key()?;
    engine_for(ctx, client, ws, concurrency)?
        .pull(
            ws,
            &PullOptions {
                scope: missing.clone(),
                no_fetch: true,
                with_attachments: true,
                with_comments: true,
                ..Default::default()
            },
        )
        .await?;
    let mut pulled = Vec::new();
    for id in missing {
        if let Some(page) = ws.state().get_page(&id)? {
            pulled.push(json!({ "space": key, "page_id": id, "path": page.local_path }));
        }
    }
    Ok(pulled)
}

/// The source loses its rules page with the move: unset it there, and take
/// the rules out of its agent docs.
fn after_move(spaces: &mut BTreeMap<String, Workspace>, plan: &MovePlan) -> Result<()> {
    if plan.rules_page.is_some() {
        if let Some(ws) = spaces.get(&plan.source.key) {
            ws.state().delete_meta(confed_core::config::RULES_PAGE.key)?;
            let _ = crate::commands::agent_docs::refresh_member(ws);
        }
    }
    Ok(())
}

fn with_resume_hint(e: ConfedError, id: &str) -> ConfedError {
    let next = format!(
        "the move is recorded as {id}: fix the cause, then `confed instance resume {id}`; \
         `confed instance abort {id}` undoes it if the server has not moved anything yet"
    );
    match e {
        // A hint of its own says what this situation needs; only name the move.
        ConfedError::State { message, hint: Some(hint) } => ConfedError::State {
            message,
            hint: Some(
                hint.replace("`confed instance resume`", &format!("`confed instance resume {id}`")),
            ),
        },
        ConfedError::State { message, hint: None } => {
            ConfedError::State { message, hint: Some(next) }
        }
        ConfedError::Usage { message, .. } => ConfedError::Usage { message, hint: Some(next) },
        other => {
            let code = other.exit_code();
            let message = other.to_string();
            match code {
                confed_core::ExitCode::Unsupported => {
                    ConfedError::Unsupported(format!("{message} ({next})"))
                }
                confed_core::ExitCode::Auth => ConfedError::Auth(format!("{message} ({next})")),
                confed_core::ExitCode::Conflict => {
                    ConfedError::Conflict(format!("{message} ({next})"))
                }
                confed_core::ExitCode::Network => {
                    ConfedError::Network(format!("{message} ({next})"))
                }
                confed_core::ExitCode::NotFound => {
                    ConfedError::NotFound(format!("{message} ({next})"))
                }
                _ => ConfedError::State { message, hint: Some(next) },
            }
        }
    }
}

fn on_conflict(args: &InstanceMvArgs) -> Result<OnConflict> {
    if let Some(file) = &args.title_map {
        let text = std::fs::read_to_string(file)
            .map_err(|e| ConfedError::io(format!("reading {}", file.display()), e))?;
        let map: HashMap<String, String> = serde_json::from_str(&text).map_err(|e| {
            ConfedError::usage(format!(
                "{} is not a JSON object of titles ({{\"<page id or title>\": \"<new title>\"}}): {e}",
                file.display()
            ))
        })?;
        return Ok(OnConflict::Map(map));
    }
    Ok(match args.on_conflict.as_str() {
        "suffix" => OnConflict::Suffix,
        _ => OnConflict::Fail,
    })
}

/// How the server should place the roots, and their parent afterwards.
async fn placement(
    ctx: &Context,
    instance: &Instance,
    client: &Arc<dyn ConfluenceClient>,
    dst: &Workspace,
    dest: &Found,
    args: &InstanceMvArgs,
) -> Result<(MovePosition, String, Option<String>)> {
    let parent_of = |id: &str| -> Result<Option<String>> {
        Ok(dst.state().get_remote(id)?.and_then(|r| r.parent_id))
    };
    for (sibling, position) in
        [(&args.before, MovePosition::Before), (&args.after, MovePosition::After)]
    {
        let Some(reference) = sibling else { continue };
        let id = match locate(ctx, instance, Some(client), reference, false).await? {
            Found::Page { page_id, .. } => page_id,
            _ => {
                return Err(ConfedError::NotFound(format!(
                    "`{reference}` is no page of the destination"
                )))
            }
        };
        return Ok((position, id.clone(), parent_of(&id)?));
    }

    // The parent the page goes under, or `None` for the top of the space.
    let parent: Option<String> = match dest {
        Found::Page { page_id, .. } => Some(page_id.clone()),
        Found::NewPath { parent: Some(parent), .. } => Some(parent.clone()),
        Found::SpaceTop { .. } | Found::NewPath { parent: None, .. } => {
            let key = dst.space_key()?;
            let home = client.get_space(&key).await?.homepage_id.map(|h| h.0);
            match (home, args.root) {
                (Some(home), false) => Some(home),
                (home, _) => {
                    // A top-level page: beside the home page, or the last root page.
                    let roots: Vec<_> = dst
                        .state()
                        .all_remote()?
                        .into_iter()
                        .filter(|r| !r.deleted && r.parent_id.is_none())
                        .collect();
                    let anchor = home
                        .filter(|h| roots.iter().any(|r| &r.page_id == h))
                        .or_else(|| roots.last().map(|r| r.page_id.clone()))
                        .ok_or_else(|| {
                            ConfedError::Unsupported(format!(
                                "space {key} has no page to place a top-level page beside"
                            ))
                        })?;
                    return Ok((MovePosition::After, anchor, None));
                }
            }
        }
        Found::LocalOnly { .. } => unreachable!("refused before"),
    };
    let parent = parent.expect("set above");

    if let Some(index) = args.position {
        let mut siblings: Vec<_> = dst
            .state()
            .all_remote()?
            .into_iter()
            .filter(|r| !r.deleted && r.parent_id.as_deref() == Some(parent.as_str()))
            .collect();
        siblings.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));
        if let Some(sibling) = siblings.get(index) {
            return Ok((MovePosition::Before, sibling.page_id.clone(), Some(parent)));
        }
    }
    Ok((MovePosition::Append, parent.clone(), Some(parent)))
}

/// A file that was never pushed is just a file: it moves like one, into the
/// destination's tree, and the next push there creates it.
fn move_local_file(
    instance: &Instance,
    member: &Member,
    path: &str,
    dest: &Found,
) -> Result<Output> {
    let src = instance.open_workspace(member)?;
    let dest_member = dest.member().clone();
    let dst = instance.open_workspace(&dest_member)?;
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let target = match dest {
        Found::Page { page_id, .. } => {
            let parent = dst.state().get_page(page_id)?.ok_or_else(|| {
                ConfedError::NotFound(format!(
                    "page {page_id} is not tracked in {}",
                    dest_member.dir
                ))
            })?;
            format!("{}/{name}", paths::children_dir(&parent.local_path))
        }
        Found::NewPath { path, .. } => path.clone(),
        Found::SpaceTop { .. } => name.clone(),
        Found::LocalOnly { path, .. } => {
            return Err(ConfedError::state(format!("{path} has not been pushed; push it first")))
        }
    };
    let _locks = {
        let mut both = [(member.space_key.clone(), &src), (dest_member.space_key.clone(), &dst)];
        both.sort_by(|a, b| a.0.cmp(&b.0));
        both.iter().map(|(_, ws)| ws.lock()).collect::<Result<Vec<_>>>()?
    };
    if dst.absolute(&target).exists() {
        return Err(ConfedError::state(format!(
            "{}:{target} already exists",
            dest_member.space_key
        )));
    }
    for (from, to) in [
        (path.to_string(), target.clone()),
        (paths::children_dir(path), paths::children_dir(&target)),
        (paths::sidecar_for(path), paths::sidecar_for(&target)),
    ] {
        let from_abs = src.absolute(&from);
        if !from_abs.exists() {
            continue;
        }
        let to_abs = dst.absolute(&to);
        if let Some(parent) = to_abs.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
        }
        std::fs::rename(&from_abs, &to_abs)
            .map_err(|e| ConfedError::io(format!("moving {from}"), e))?;
    }
    // A parent named by id belonged to the other space.
    let text = std::fs::read_to_string(dst.absolute(&target))
        .map_err(|e| ConfedError::io(format!("reading {target}"), e))?;
    let mut file = confed_core::frontmatter::parse(&text, &target)?;
    if file.frontmatter.parent_id.take().is_some() {
        confed_core::sync::write_atomic(&dst.absolute(&target), &file.render()?)?;
    }
    if let Some(dir) = Path::new(path).parent() {
        confed_core::sync::prune_empty_dirs(src.root(), &src.root().join(dir));
    }
    Ok(Output::new(
        json!({
            "local_only": true,
            "moved": [{
                "page_id": null,
                "from": { "space": member.space_key, "path": path },
                "to": { "space": dest_member.space_key, "path": target },
            }],
            "links_rewritten": [],
            "warnings": [],
            "journal_id": null,
        }),
        format!(
            "Moved {}:{path} -> {}:{target} (a new page: `confed -C {} push` creates it)\n",
            member.space_key, dest_member.space_key, dest_member.dir
        ),
    ))
}

/// Plans with more pages than this are shown by their roots, unless -v.
const LISTED_PAGES: usize = 30;

/// The plan, or what the move did: before and after, as trees.
fn describe(ctx: &Context, plan: &MovePlan, journal_id: Option<&str>, dry_run: bool) -> Output {
    let style = &ctx.style;
    let mut human = String::new();
    if dry_run {
        let _ = writeln!(human, "{}", style.dim("Dry run — nothing was changed."));
    }
    let roots: Vec<_> = plan.pages.iter().filter(|p| p.depth == 0).collect();
    let trees =
        if roots.len() > 1 { format!(" ({} page trees)", roots.len()) } else { String::new() };
    let _ = writeln!(
        human,
        "{} {}{trees} from {} to {}:",
        if dry_run && plan.blocking.is_empty() {
            "Would move"
        } else if dry_run {
            "Would have moved"
        } else {
            "Moved"
        },
        plural(plan.pages.len(), "page", "pages"),
        style.bold(&plan.source.key),
        style.bold(&plan.dest.key)
    );
    let to_path = |page: &crossmove::PlannedPage| {
        plan.moved
            .iter()
            .find(|m| m.page_id == page.page_id)
            .map(|m| m.to.path.clone())
            .unwrap_or_else(|| page.to_path.clone())
    };
    if plan.pages.len() > LISTED_PAGES && ctx.global.verbose == 0 {
        // Each root, and how many pages go with it.
        for root in &roots {
            let under = plan.subtree_of(&root.page_id).len() - 1;
            let with = if under > 0 {
                style.dim(&format!("  (+{} under it)", plural(under, "page", "pages")))
            } else {
                String::new()
            };
            let _ = writeln!(
                human,
                "  {}:{} → {}:{}{with}",
                plan.source.key,
                root.from_path,
                plan.dest.key,
                to_path(root)
            );
        }
        let _ = writeln!(human, "  {}", style.dim("(-v lists every page)"));
    } else {
        for page in &plan.pages {
            let indent = "  ".repeat(page.depth);
            let _ = writeln!(
                human,
                "  {} {indent}{}:{}",
                style.red("-"),
                plan.source.key,
                page.from_path
            );
        }
        for page in &plan.pages {
            let indent = "  ".repeat(page.depth);
            let _ = writeln!(
                human,
                "  {} {indent}{}:{}",
                style.green("+"),
                plan.dest.key,
                to_path(page)
            );
        }
    }
    let renames = plan.renames();
    if !renames.is_empty() {
        let _ = writeln!(human, "\n{}:", if dry_run { "Titles that change" } else { "Renamed" });
        for (page, to) in &renames {
            let why = match page.rename_reason.as_deref() {
                Some("conflict_suffix") => "taken in the destination",
                Some("title_map") => "--title-map",
                Some("destination_path") => "the destination path",
                _ => "",
            };
            let _ = writeln!(human, "  \"{}\" → \"{to}\"  {}", page.title, style.dim(why));
        }
    }
    if !plan.links.is_empty() {
        let verb = match (dry_run, plan.rewrite_links) {
            (_, false) => "Links left as they are (--no-rewrite-links)",
            (true, true) => "Links to rewrite",
            (false, true) => "Links checked",
        };
        let _ = writeln!(human, "\n{verb}:");
        for link in &plan.links {
            let done = plan.links_rewritten.iter().find(|r| r.page_id == link.page_id);
            let note = match (done, plan.links_by_server.contains(&link.page_id)) {
                (Some(r), _) => {
                    format!("rewrote {}, now v{}", plural(r.count, "link", "links"), r.version)
                }
                (None, true) => "already fixed by the server".to_string(),
                (None, false) => plural(link.count, "link", "links"),
            };
            let _ = writeln!(human, "  {}:{}  {}", link.space, link.path, style.dim(&note));
        }
    }
    if !plan.links_broken.is_empty() {
        let _ = writeln!(human, "\nLinks that reach no page (left as they were):");
        for broken in &plan.links_broken {
            let _ = writeln!(
                human,
                "  {}: page {} → \"{}\" in {}",
                broken.space, broken.page_id, broken.title, broken.reaches
            );
        }
    }
    if !plan.blocking.is_empty() {
        let _ = writeln!(human, "\n{}", style.red("What stops it:"));
        for blocker in &plan.blocking {
            let suggestion = blocker
                .suggested_title
                .as_ref()
                .map(|t| style.dim(&format!("  (--on-conflict suffix: \"{t}\")")))
                .unwrap_or_default();
            let _ = writeln!(human, "  - {}{suggestion}", blocker.message);
        }
    }
    if let Some(verified) = &plan.verified {
        let left = match verified.source_remaining_count {
            0 => format!("nothing is left in {}", plan.source.key),
            n => format!("{} left in {}", plural(n, "page", "pages"), plan.source.key),
        };
        let clean = if verified.workspaces_clean {
            "both workspaces clean".to_string()
        } else {
            style.yellow("not every page in the two workspaces is clean; `confed instance status`")
        };
        let _ = writeln!(
            human,
            "\nVerified: {} of {} tracked in {} only; {left}; {clean}.",
            verified.at_destination,
            plural(verified.pages, "page", "pages"),
            plan.dest.key
        );
    }
    if let Some(id) = journal_id {
        let _ = writeln!(human, "\n{}", style.dim(&format!("journal: {id}")));
    }

    let moved: Vec<_> = if plan.moved.is_empty() {
        plan.pages
            .iter()
            .map(|p| {
                json!({
                    "page_id": p.page_id,
                    "title": p.final_title(),
                    "from": { "space": plan.source.key, "path": p.from_path },
                    "to": { "space": plan.dest.key, "path": p.to_path },
                    "version": null,
                })
            })
            .collect()
    } else {
        plan.moved.iter().map(|m| serde_json::to_value(m).unwrap_or_default()).collect()
    };
    let rewritten: Vec<_> = plan
        .links_rewritten
        .iter()
        .map(|r| json!({ "page_id": r.page_id, "space": r.space, "count": r.count, "version": r.version }))
        .collect();
    let renamed: Vec<_> = renames
        .iter()
        .map(|(page, to)| {
            json!({
                "page_id": page.page_id,
                "from_title": page.title,
                "to_title": to,
                "reason": page.rename_reason,
            })
        })
        .collect();
    Output::new(
        json!({
            "dry_run": dry_run,
            "journal_id": journal_id,
            "source": plan.source.key,
            "dest": plan.dest.key,
            "roots": plan.root_ids(),
            "position": plan.position,
            "target_id": plan.target_id,
            "new_parent_id": plan.new_parent_id,
            "summary": {
                "pages": plan.pages.len(),
                "roots": roots.len(),
                "attachments": plan.pages.iter().map(|p| p.attachments).sum::<usize>(),
                "comments": plan.pages.iter().map(|p| p.comments).sum::<usize>(),
                "renamed": renames.len(),
                "links": plan.links.len(),
                "blocking": plan.blocking.len(),
            },
            "pages": plan.pages,
            "renamed": renamed,
            "blocking": plan.blocking,
            "links": plan.links,
            "moved": moved,
            "links_rewritten": rewritten,
            "links_by_server": plan.links_by_server,
            "links_broken": plan.links_broken,
            "verified": plan.verified,
            "warnings": plan.warnings,
        }),
        human,
    )
    .warn_all(plan.warnings.clone())
}

/// The journal entry a resume or abort means: the one named, or the only
/// unfinished one.
fn pick(instance: &Instance, id: Option<&str>) -> Result<Option<JournalEntry>> {
    if let Some(id) = id {
        return instance
            .journal_entry(id)?
            .map(Some)
            .ok_or_else(|| ConfedError::NotFound(format!("no journal entry {id}")));
    }
    let unfinished = instance.journal_unfinished()?;
    match unfinished.len() {
        0 => Ok(None),
        1 => Ok(unfinished.into_iter().next()),
        _ => Err(ConfedError::usage_with_hint(
            format!("{} moves are unfinished", unfinished.len()),
            format!(
                "name one: {}",
                unfinished.iter().map(|e| e.id.as_str()).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

pub async fn resume(
    ctx: &mut Context,
    args: &InstanceJournalArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let Some(entry) = pick(&instance, args.id.as_deref())? else {
        return Ok(Output::new(json!({ "resumed": null }), "Nothing to resume.\n"));
    };
    if entry.state.is_final() {
        return Ok(Output::new(
            json!({ "resumed": null, "journal_id": entry.id, "state": entry.state }),
            format!("{} is already {}.\n", entry.id, entry.state.as_str()),
        ));
    }
    let mut plan: MovePlan = serde_json::from_value(entry.plan.clone())?;
    let client = connection.client(ctx)?;
    let mut spaces = open_all(&instance)?;
    for key in plan.spaces_written() {
        if !spaces.contains_key(&key) {
            return Err(ConfedError::state(format!(
                "space {key}, which the move writes to, is not a registered workspace any more"
            )));
        }
    }
    let locks = lock_all(&spaces, &plan.spaces_written())?;
    crossmove::execute(
        &instance,
        &entry.id,
        &client,
        &mut spaces,
        &mut plan,
        entry.state,
        ctx.concurrency(),
    )
    .await
    .map_err(|e| with_resume_hint(e, &entry.id))?;
    after_move(&mut spaces, &plan)?;
    drop(locks);
    let mut output = describe(ctx, &plan, Some(&entry.id), false);
    output.result["resumed_from"] = json!(entry.state);
    Ok(output)
}

pub async fn abort(
    ctx: &mut Context,
    args: &InstanceJournalArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let Some(entry) = pick(&instance, args.id.as_deref())? else {
        return Ok(Output::new(json!({ "aborted": null }), "Nothing to abort.\n"));
    };
    let mut plan: MovePlan = serde_json::from_value(entry.plan.clone())?;
    let client = connection.client(ctx)?;
    crossmove::abort(&instance, &entry.id, &client, &mut plan, entry.state).await?;
    Ok(Output::new(
        json!({ "aborted": entry.id, "journal_id": entry.id, "renamed_back": entry.plan["renamed"] }),
        format!("Aborted {}; nothing was moved.\n", entry.id),
    ))
}
