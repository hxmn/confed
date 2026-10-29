//! `confed instance mv`, `resume` and `abort`: moving pages between spaces.
//! The work is [`confed_core::crossmove`]'s; this is the part that reads
//! arguments, takes the locks, asks, and reports.

use super::Connection;
use crate::cli::{InstanceJournalArgs, InstanceMvArgs};
use crate::context::{split_space_ref, Context};
use crate::output::{plural, Output};
use confed_api::{ConfluenceClient, MovePosition, SpaceId};
use confed_core::crossmove::{self, MovePlan, MoveRequest, OnConflict, SpaceRef};
use confed_core::error::{ConfedError, Result};
use confed_core::instance::{Instance, JournalEntry, Member, Step};
use confed_core::lock::WorkspaceLock;
use confed_core::paths;
use confed_core::sync::{FetchOptions, PushOptions, SyncEngine};
use confed_core::workspace::Workspace;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A page argument, found in one of the instance's workspaces.
enum Found {
    /// A page the workspace tracks.
    Page { member: Member, page_id: String },
    /// A file that names no page on the server yet.
    LocalOnly { member: Member, path: String },
    /// `KEY:` alone: the top of that space.
    SpaceTop { member: Member },
    /// A path that is no page yet: where a page would go, and what it would be called.
    NewPath { member: Member, parent: Option<String>, title: String, path: String },
}

impl Found {
    fn member(&self) -> &Member {
        match self {
            Found::Page { member, .. }
            | Found::LocalOnly { member, .. }
            | Found::SpaceTop { member }
            | Found::NewPath { member, .. } => member,
        }
    }
}

/// Where in the instance an argument points: `KEY:path`, a path from the
/// current directory, or a page id looked up in every registered space.
fn locate(
    ctx: &Context,
    instance: &Instance,
    reference: &str,
    as_destination: bool,
) -> Result<Found> {
    let members = instance.enabled_members()?;
    let (member, path) = if let Some((key, path)) = split_space_ref(reference) {
        let member = instance.member(key)?.ok_or_else(|| unregistered(key, &members))?;
        (member, path.trim_start_matches('/').to_string())
    } else if !reference.is_empty() && reference.chars().all(|c| c.is_ascii_digit()) {
        for member in &members {
            let Ok(ws) = instance.open_workspace(member) else { continue };
            if ws.state().get_page(reference)?.is_some() {
                return Ok(Found::Page { member: member.clone(), page_id: reference.to_string() });
            }
        }
        return Err(ConfedError::NotFound(format!(
            "page {reference} is not tracked in any registered space"
        )));
    } else {
        let absolute = normalize(&ctx.cwd.join(reference));
        let member = instance.member_for_path(&absolute)?.ok_or_else(|| {
            ConfedError::usage_with_hint(
                format!("`{reference}` is not inside a registered workspace"),
                "name it as KEY:path, a path into a space's directory, or a page id",
            )
        })?;
        let dir = normalize(&instance.member_dir(&member));
        let dir = dir.canonicalize().unwrap_or(dir);
        let absolute = absolute.canonicalize().unwrap_or(absolute);
        let rel = absolute
            .strip_prefix(&dir)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        (member, rel)
    };

    if path.is_empty() {
        if as_destination {
            return Ok(Found::SpaceTop { member });
        }
        return Err(ConfedError::usage(format!(
            "`{reference}` names space {}, not a page in it",
            member.space_key
        )));
    }

    let ws = instance.open_workspace(&member)?;
    let with_md = if path.ends_with(".md") { path.clone() } else { format!("{path}.md") };
    for candidate in [path.clone(), with_md.clone()] {
        if let Some(record) = ws.state().get_page_by_path(&candidate)? {
            return Ok(Found::Page { member, page_id: record.page_id });
        }
    }
    let file = ws.absolute(&with_md);
    if file.exists() {
        let text = std::fs::read_to_string(&file)
            .map_err(|e| ConfedError::io(format!("reading {with_md}"), e))?;
        let parsed = confed_core::frontmatter::parse(&text, &with_md)?;
        return Ok(match parsed.frontmatter.page_id() {
            Some(id) => Found::Page { member, page_id: id.to_string() },
            None => Found::LocalOnly { member, path: with_md },
        });
    }
    if !as_destination {
        return Err(ConfedError::NotFound(format!(
            "no page `{with_md}` in {} ({})",
            member.space_key, member.dir
        )));
    }
    // A new path: its directory names the parent, its file name the title.
    let stem = with_md.trim_end_matches(".md");
    let (dir, name) = match stem.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, stem),
    };
    let parent = match dir {
        Some(dir) => Some(
            ws.state()
                .get_page_by_path(&format!("{dir}.md"))?
                .ok_or_else(|| {
                    ConfedError::NotFound(format!(
                        "no page `{dir}.md` in {} to put it under",
                        member.space_key
                    ))
                })?
                .page_id,
        ),
        None => None,
    };
    Ok(Found::NewPath { member, parent, title: name.to_string(), path: with_md })
}

fn unregistered(key: &str, members: &[Member]) -> ConfedError {
    ConfedError::usage_with_hint(
        format!("space {key} is not registered in this instance"),
        format!(
            "registered: {}; `confed instance clone {key}` adds it",
            members.iter().map(|m| m.space_key.as_str()).collect::<Vec<_>>().join(", ")
        ),
    )
}

/// `a/./b/../c` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

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
    client: &Arc<dyn ConfluenceClient>,
    ws: &Workspace,
    concurrency: usize,
) -> Result<SyncEngine> {
    Ok(SyncEngine::new(
        client.clone(),
        SpaceId { key: ws.space_key()?, numeric: ws.space_numeric_id()? },
        concurrency,
    ))
}

pub async fn run(
    ctx: &mut Context,
    args: &InstanceMvArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let source = locate(ctx, &instance, &args.source, false)?;
    let dest = locate(ctx, &instance, &args.destination, true)?;
    if source.member().space_key == dest.member().space_key {
        return Err(ConfedError::usage_with_hint(
            format!("both are in space {}", source.member().space_key),
            format!("within a space, use `confed -C {} mv`", source.member().dir),
        ));
    }
    if let Found::LocalOnly { member, path } = &source {
        return move_local_file(&instance, member, path, &dest);
    }
    let Found::Page { member: src_member, page_id: root_id } = &source else {
        return Err(ConfedError::usage(format!("`{}` is not a page", args.source)));
    };
    if let Found::LocalOnly { path, .. } = &dest {
        return Err(ConfedError::state_with_hint(
            format!("{path} has not been pushed, so nothing can be put under it on the server"),
            "push it first",
        ));
    }
    if args.push_first && args.message.as_deref().is_none_or(str::is_empty) {
        return Err(ConfedError::usage("--push-first needs -m \"why\""));
    }

    let client = connection.client(ctx)?;
    let concurrency = ctx.concurrency();
    let dest_member = dest.member().clone();
    let mut spaces = open_all(&instance)?;
    let src_ref = space_ref(src_member, &spaces[&src_member.space_key])?;
    let dst_ref = space_ref(&dest_member, &spaces[&dest_member.space_key])?;

    // Both sides locked and fetched before the plan.
    let mut locks = lock_all(&spaces, &[src_ref.key.clone(), dst_ref.key.clone()])?;
    if args.push_first {
        let ws = spaces.get_mut(&src_ref.key).expect("opened");
        let root_path = ws.state().get_page(root_id)?.map(|p| p.local_path).unwrap_or_default();
        let engine = engine_for(&client, ws, concurrency)?;
        let pushed = engine
            .push(
                ws,
                &PushOptions {
                    scope: vec![
                        root_path.clone(),
                        format!("{}/**", paths::children_dir(&root_path)),
                    ],
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
        engine_for(&client, ws, concurrency)?.fetch(ws, &FetchOptions::default()).await?;
    }

    let (position, target_id, new_parent_id) =
        placement(&client, &spaces[&dst_ref.key], &dest, args).await?;
    let request = MoveRequest {
        root_id: root_id.clone(),
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
    let mut plan = crossmove::plan(&client, &spaces, &request).await?;

    if let Some(reason) = plan.blocked.clone() {
        // Everything else checked out: show the plan, and why it cannot run.
        let mut output = describe(ctx, &plan, None, true);
        output.result["blocked"] = json!(reason);
        output.exit = confed_core::ExitCode::Unsupported;
        return Ok(output.with_errors(vec![crate::output::ErrorEntry {
            code: confed_core::ExitCode::Unsupported.name(),
            message: reason,
            hint: Some(
                "move the pages in the browser instead; then `confed instance pull` brings both \
                 workspaces up to date (the source drops them, the destination downloads them)"
                    .into(),
            ),
        }]));
    }
    if args.dry_run {
        return Ok(describe(ctx, &plan, None, true));
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
    Ok(describe(ctx, &plan, Some(&id), false))
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

/// How the server should place the subtree's root, and its parent afterwards.
async fn placement(
    client: &Arc<dyn ConfluenceClient>,
    dst: &Workspace,
    dest: &Found,
    args: &InstanceMvArgs,
) -> Result<(MovePosition, String, Option<String>)> {
    let parent_of = |id: &str| -> Result<Option<String>> {
        Ok(dst.state().get_remote(id)?.and_then(|r| r.parent_id))
    };
    let sibling = |reference: &str| -> Result<String> {
        let path = split_space_ref(reference).map(|(_, p)| p).unwrap_or(reference);
        if path.chars().all(|c| c.is_ascii_digit()) {
            return Ok(path.to_string());
        }
        let with_md = if path.ends_with(".md") { path.to_string() } else { format!("{path}.md") };
        dst.state()
            .get_page_by_path(&with_md)?
            .map(|p| p.page_id)
            .ok_or_else(|| ConfedError::NotFound(format!("no page `{with_md}` in the destination")))
    };
    if let Some(before) = &args.before {
        let id = sibling(before)?;
        return Ok((MovePosition::Before, id.clone(), parent_of(&id)?));
    }
    if let Some(after) = &args.after {
        let id = sibling(after)?;
        return Ok((MovePosition::After, id.clone(), parent_of(&id)?));
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

/// The plan, or what the move did: before and after, as trees.
fn describe(ctx: &Context, plan: &MovePlan, journal_id: Option<&str>, dry_run: bool) -> Output {
    let style = &ctx.style;
    let mut human = String::new();
    if dry_run {
        let _ = writeln!(human, "{}", style.dim("Dry run — nothing was changed."));
    }
    let _ = writeln!(
        human,
        "{} {} from {} to {}:",
        if dry_run { "Would move" } else { "Moved" },
        plural(plan.pages.len(), "page", "pages"),
        style.bold(&plan.source.key),
        style.bold(&plan.dest.key)
    );
    for page in &plan.pages {
        let indent = "  ".repeat(page.depth);
        let _ =
            writeln!(human, "  {} {indent}{}:{}", style.red("-"), plan.source.key, page.from_path);
    }
    for page in &plan.pages {
        let indent = "  ".repeat(page.depth);
        let to = plan
            .moved
            .iter()
            .find(|m| m.page_id == page.page_id)
            .map(|m| m.to.path.clone())
            .unwrap_or_else(|| page.to_path.clone());
        let renamed = page
            .new_title
            .as_ref()
            .map(|t| style.dim(&format!("  (now \"{t}\")")))
            .unwrap_or_default();
        let _ = writeln!(human, "  {} {indent}{}:{to}{renamed}", style.green("+"), plan.dest.key);
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
    Output::new(
        json!({
            "dry_run": dry_run,
            "journal_id": journal_id,
            "source": plan.source.key,
            "dest": plan.dest.key,
            "position": plan.position,
            "target_id": plan.target_id,
            "new_parent_id": plan.new_parent_id,
            "pages": plan.pages,
            "links": plan.links,
            "moved": moved,
            "links_rewritten": rewritten,
            "links_by_server": plan.links_by_server,
            "links_broken": plan.links_broken,
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
