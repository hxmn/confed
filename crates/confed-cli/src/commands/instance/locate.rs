//! Naming a page from an instance root, and finding it: a path from here,
//! `KEY:path`, a page id, or a Confluence URL. [`locate`] is what every
//! instance command that takes a page uses; `confed instance locate` and
//! `confed instance tree` show what it finds.

use super::Connection;
use crate::cli::{InstanceLocateArgs, InstanceTreeArgs};
use crate::commands::tree;
use crate::context::{split_space_ref, Context};
use crate::output::{ErrorEntry, Output};
use crate::pageref::{self, UrlRef};
use confed_api::{ApiError, BodyFormat, ConfluenceClient, PageId, SpaceId};
use confed_core::error::{ConfedError, ExitCode, Result};
use confed_core::instance::{Instance, Member};
use confed_core::worktree;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A page argument, found in one of the instance's workspaces.
pub enum Found {
    /// A page of a registered space: tracked in its workspace, listed by its
    /// last fetch, or, when it is in neither, found on the server.
    Page { member: Member, page_id: String },
    /// A file that names no page on the server yet.
    LocalOnly { member: Member, path: String },
    /// `KEY:` alone, or a space's URL: the top of that space.
    SpaceTop { member: Member },
    /// A path that is no page yet: where a page would go, and what it would be called.
    NewPath { member: Member, parent: Option<String>, title: String, path: String },
}

impl Found {
    pub fn member(&self) -> &Member {
        match self {
            Found::Page { member, .. }
            | Found::LocalOnly { member, .. }
            | Found::SpaceTop { member }
            | Found::NewPath { member, .. } => member,
        }
    }
}

/// Where in the instance an argument points: a Confluence URL, `KEY:path`, a
/// path from the current directory, or a page id looked up in every
/// registered space — and, with a client, on the server when no workspace
/// knows it.
pub async fn locate(
    ctx: &Context,
    instance: &Instance,
    client: Option<&Arc<dyn ConfluenceClient>>,
    reference: &str,
    as_destination: bool,
) -> Result<Found> {
    let members = instance.enabled_members()?;
    let (member, path) = if pageref::is_url(reference) {
        let parsed = pageref::parse(reference).ok_or_else(|| {
            ConfedError::usage_with_hint(
                format!("`{reference}` is not a Confluence page or space URL"),
                "confed reads /spaces/KEY/pages/ID/…, /pages/viewpage.action?pageId=ID, \
                 /display/KEY/Title, /spaces/KEY and /display/KEY",
            )
        })?;
        if let Some(base) = instance.base_url()? {
            let ours = pageref::server(&base).unwrap_or_default();
            if !ours.is_empty() && parsed.server != ours {
                return Err(ConfedError::usage(format!(
                    "`{reference}` is on {}, but this instance is {base}",
                    parsed.server
                )));
            }
        }
        match parsed.target {
            UrlRef::Page { id, space } => {
                return by_id(instance, client, &members, &id, space.as_deref()).await
            }
            UrlRef::Space { space } => {
                let member =
                    instance.member(&space)?.ok_or_else(|| unregistered(&space, &members))?;
                (member, String::new())
            }
            UrlRef::Titled { space, title } => {
                let member =
                    instance.member(&space)?.ok_or_else(|| unregistered(&space, &members))?;
                let page_id = by_title(instance, client, &member, &title).await?;
                return Ok(Found::Page { member, page_id });
            }
        }
    } else if let Some((key, path)) = split_space_ref(reference) {
        let member = instance.member(key)?.ok_or_else(|| unregistered(key, &members))?;
        (member, path.trim_start_matches('/').to_string())
    } else if !reference.is_empty() && reference.chars().all(|c| c.is_ascii_digit()) {
        return by_id(instance, client, &members, reference, None).await;
    } else {
        let absolute = normalize(&ctx.cwd.join(reference));
        let member = instance.member_for_path(&absolute)?.ok_or_else(|| {
            ConfedError::usage_with_hint(
                format!("`{reference}` is not inside a registered workspace"),
                "name it as KEY:path, a path into a space's directory, a page id or its URL",
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
    for candidate in [path, with_md.clone()] {
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

/// A page by id: tracked or listed in a registered workspace (the one its
/// URL names first), or else wherever the server says it is.
async fn by_id(
    instance: &Instance,
    client: Option<&Arc<dyn ConfluenceClient>>,
    members: &[Member],
    id: &str,
    space: Option<&str>,
) -> Result<Found> {
    let mut ordered: Vec<&Member> =
        members.iter().filter(|m| Some(m.space_key.as_str()) == space).collect();
    ordered.extend(members.iter().filter(|m| Some(m.space_key.as_str()) != space));
    for member in &ordered {
        let Ok(ws) = instance.open_workspace(member) else { continue };
        if ws.state().get_page(id)?.is_some()
            || ws.state().get_remote(id)?.is_some_and(|r| !r.deleted)
        {
            return Ok(Found::Page { member: (*member).clone(), page_id: id.to_string() });
        }
    }
    let Some(client) = client else {
        return Err(ConfedError::NotFound(format!(
            "page {id} is not tracked in any registered space; --remote asks the server"
        )));
    };
    let page = match client.get_page(&PageId::new(id), BodyFormat::Storage).await {
        Ok(page) => page,
        Err(ApiError::NotFound(_)) => {
            return Err(ConfedError::NotFound(format!("there is no page {id} on the server")))
        }
        Err(e) => return Err(e.into()),
    };
    let key = page.summary.space_key;
    let member = instance.member(&key)?.ok_or_else(|| {
        ConfedError::usage_with_hint(
            format!("page {id} is in space {key}, which is not registered in this instance"),
            format!("`confed instance clone {key}` adds it"),
        )
    })?;
    Ok(Found::Page { member, page_id: id.to_string() })
}

/// A page by its exact title in one space: what the workspace knows, then
/// what the server answers.
async fn by_title(
    instance: &Instance,
    client: Option<&Arc<dyn ConfluenceClient>>,
    member: &Member,
    title: &str,
) -> Result<String> {
    if let Ok(ws) = instance.open_workspace(member) {
        if let Some(page) = ws.state().all_pages()?.into_iter().find(|p| p.title == title) {
            return Ok(page.page_id);
        }
        if let Some(page) =
            ws.state().all_remote()?.into_iter().find(|r| !r.deleted && r.title == title)
        {
            return Ok(page.page_id);
        }
    }
    let not_found = || {
        ConfedError::NotFound(format!("no page titled \"{title}\" in space {}", member.space_key))
    };
    match client {
        Some(client) => match client.find_page_by_title(&member.space_key, title).await {
            Ok(Some(page)) => Ok(page.id.0),
            Ok(None) | Err(ApiError::NotFound(_)) => Err(not_found()),
            Err(e) => Err(e.into()),
        },
        None => Err(not_found()),
    }
}

pub fn unregistered(key: &str, members: &[Member]) -> ConfedError {
    ConfedError::usage_with_hint(
        format!("space {key} is not registered in this instance"),
        format!(
            "registered: {}; `confed instance clone {key}` adds it",
            members.iter().map(|m| m.space_key.as_str()).collect::<Vec<_>>().join(", ")
        ),
    )
}

/// `a/./b/../c` without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
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

// --------------------------------------------------------------- locate ---

/// `confed instance locate`: where each named page is, locally and on the server.
pub async fn run(
    ctx: &mut Context,
    args: &InstanceLocateArgs,
    connection: Option<Connection>,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let client = connection.map(|c| c.client(ctx)).transpose()?;
    let style = &ctx.style;
    let mut human = String::new();
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut exit = ExitCode::Ok;
    let mut scans: BTreeMap<String, worktree::Scan> = BTreeMap::new();

    for reference in &args.pages {
        let found = match locate(ctx, &instance, client.as_ref(), reference, false).await {
            Ok(found) => found,
            Err(e) => {
                exit = e.exit_code();
                let _ = writeln!(human, "{}  {}", style.red(reference), e);
                errors.push(ErrorEntry::from_error(&e));
                rows.push(json!({ "reference": reference, "found": false }));
                continue;
            }
        };
        let row = describe(&instance, client.as_ref(), &found, reference, &mut scans).await?;
        let space = row["space"].as_str().unwrap_or_default().to_string();
        let title = row["title"].as_str().unwrap_or_default().to_string();
        let place = match row["file"].as_str() {
            Some(file) => file.to_string(),
            None => style.dim("not pulled"),
        };
        let version = row["remote_version"].as_u64().map(|v| format!("  v{v}")).unwrap_or_default();
        let _ = writeln!(
            human,
            "{space:<8} {place}  {}{}",
            style.dim(&format!("{} \"{title}\"", row["page_id"].as_str().unwrap_or("-"))),
            style.dim(&version)
        );
        rows.push(row);
    }
    let mut output = Output::new(json!({ "pages": rows }), human).with_errors(errors);
    output.exit = exit;
    Ok(output)
}

/// One located page, as `locate` reports it.
async fn describe(
    instance: &Instance,
    client: Option<&Arc<dyn ConfluenceClient>>,
    found: &Found,
    reference: &str,
    scans: &mut BTreeMap<String, worktree::Scan>,
) -> Result<Value> {
    let member = found.member();
    let ws = instance.open_workspace(member)?;
    let (page_id, path) = match found {
        Found::Page { page_id, .. } => (Some(page_id.clone()), None),
        Found::LocalOnly { path, .. } => (None, Some(path.clone())),
        Found::SpaceTop { .. } | Found::NewPath { .. } => (None, None),
    };
    let base = page_id.as_deref().map(|id| ws.state().get_page(id)).transpose()?.flatten();
    let remote = page_id.as_deref().map(|id| ws.state().get_remote(id)).transpose()?.flatten();
    let path = path.or_else(|| base.as_ref().map(|b| b.local_path.clone()));
    let mut title =
        base.as_ref().map(|b| b.title.clone()).or(remote.as_ref().map(|r| r.title.clone()));
    let mut remote_version = remote.as_ref().filter(|r| !r.deleted).map(|r| r.version);
    let mut parent_id = remote.as_ref().and_then(|r| r.parent_id.clone());
    let mut on_server = remote.as_ref().map(|r| !r.deleted);
    if let (Some(client), Some(id)) = (client, page_id.as_deref()) {
        match client.get_page(&PageId::new(id), BodyFormat::Storage).await {
            Ok(page) => {
                remote_version = Some(page.summary.version);
                title = Some(page.summary.title);
                parent_id = page.summary.parent_id.map(|p| p.0);
                on_server = Some(true);
            }
            Err(ApiError::NotFound(_)) => on_server = Some(false),
            Err(_) => {}
        }
    }
    let state = match &path {
        Some(path) => {
            if !scans.contains_key(&member.space_key) {
                scans.insert(member.space_key.clone(), worktree::scan(&ws)?);
            }
            scans[&member.space_key]
                .pages
                .iter()
                .find(|s| &s.path == path || (page_id.is_some() && s.page_id == page_id))
                .map(|s| s.state.as_str())
        }
        None => None,
    };
    Ok(json!({
        "reference": reference,
        "found": true,
        "space": member.space_key,
        "dir": member.dir,
        "page_id": page_id,
        "title": title,
        "parent_id": parent_id,
        "path": path,
        "file": path.as_ref().map(|p| format!("{}/{p}", member.dir)),
        "local": base.is_some(),
        "state": state,
        "local_version": base.as_ref().map(|b| b.version),
        "remote_version": remote_version,
        "on_server": on_server,
    }))
}

// ----------------------------------------------------------------- tree ---

/// `confed instance tree`: the pages under one page, or a whole space, as the
/// last fetch (or, with `--remote`, the server) lists them.
pub async fn tree(
    ctx: &mut Context,
    args: &InstanceTreeArgs,
    connection: Option<Connection>,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let client = connection.map(|c| c.client(ctx)).transpose()?;
    let found = locate(ctx, &instance, client.as_ref(), &args.page, true).await?;
    let (member, root) = match &found {
        Found::Page { member, page_id } => (member.clone(), Some(page_id.clone())),
        Found::SpaceTop { member } => (member.clone(), None),
        Found::LocalOnly { path, .. } | Found::NewPath { path, .. } => {
            return Err(ConfedError::NotFound(format!("{path} is no page on the server yet")))
        }
    };
    let ws = instance.open_workspace(&member)?;
    let nodes = match &client {
        Some(client) => {
            let numeric = ws.space_numeric_id()?.or(member.space_id.clone());
            let listed =
                client.list_pages(&SpaceId { key: member.space_key.clone(), numeric }).await?;
            tree::from_listing(&ws, &listed)?
        }
        None => tree::from_state(&ws)?,
    };
    let mut output = tree::render(ctx, &nodes, root.as_deref(), args.depth, &member.space_key)?;
    output.result["dir"] = json!(member.dir);
    if output.result["pages"].as_array().is_some_and(|p| p.is_empty()) && root.is_some() {
        output.human.push_str(&format!("{}\n", ctx.style.dim("(no pages under it)")));
    }
    Ok(output)
}
