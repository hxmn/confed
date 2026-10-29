//! Making an instance root and keeping its register: `init`, `adopt`,
//! `spaces`, `clone`, `add` and `remove`.

use super::Connection;
use crate::cli::{
    InitArgs, InstanceAddArgs, InstanceAdoptArgs, InstanceCloneArgs, InstanceInitArgs,
    InstanceRemoveArgs, InstanceSpacesArgs,
};
use crate::commands::{agent_docs, init};
use crate::context::Context;
use crate::output::{plural, ErrorEntry, Output};
use confed_api::{Flavor, Space};
use confed_core::error::{ConfedError, ExitCode, Result};
use confed_core::instance::{self, Instance, Member};
use confed_core::session::{self, SecretBackend, Session, SessionStore};
use confed_core::state::{now, STATE_DB_FILENAME};
use confed_core::sync::{PullOptions, SyncEngine};
use confed_core::workspace::{self, Workspace};
use confed_core::worktree;
use futures::StreamExt;
use serde_json::{json, Value};
use std::fmt::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- init ---

pub fn prepare_init(ctx: &mut Context, args: &InstanceInitArgs) -> Result<init::InitInput> {
    let init_args = InitArgs {
        credential_store: args.credential_store.clone(),
        no_agent_docs: args.no_agent_docs,
        force: false,
    };
    init::prepare(ctx, &init_args)
}

pub async fn init(
    ctx: &mut Context,
    args: &InstanceInitArgs,
    input: init::InitInput,
) -> Result<Output> {
    let root = match &args.root {
        Some(dir) => ctx.cwd.join(dir),
        None => ctx.cwd.clone(),
    };
    refuse_inside_workspace(&root)?;

    // A rejected credential fails here, before anything is written.
    let connected = init::connect(ctx, &input).await?;
    let instance = Instance::create(&root, &input.base_url, connected.flavor)?;
    let backend = store_shared(
        &root,
        &input_session(&input, &connected),
        &input.secret,
        input.credential_store,
    )?;
    let gitignore = instance::ensure_gitignore(&root)?;
    let docs = if args.no_agent_docs {
        Vec::new()
    } else {
        agent_docs::write_instance(&root, &input.base_url, connected.flavor, &instance.members()?)?
    };

    let style = &ctx.style;
    let mut human = String::new();
    let _ = writeln!(
        human,
        "Authenticated as {} on Confluence {}",
        style.bold(&connected.user.display_name),
        connected.flavor
    );
    let _ = writeln!(human, "Created the instance root {}", root.display());
    let _ = writeln!(human, "Credentials stored in {}", describe_backend(backend));
    if !docs.is_empty() {
        let _ = writeln!(human, "Wrote {}", docs.join(" and "));
    }
    let _ = writeln!(
        human,
        "\n{}",
        style.dim("Next: `confed instance clone <KEY>`, or `--all` for every space.")
    );
    Ok(Output::new(
        json!({
            "root": root.display().to_string(),
            "base_url": input.base_url,
            "flavor": connected.flavor.as_str(),
            "user": {
                "account_id": connected.user.account_id,
                "display_name": connected.user.display_name,
                "email": connected.user.email,
            },
            "credential_store": backend.as_str(),
            "gitignore_added": gitignore,
            "agent_docs": docs,
        }),
        human,
    ))
}

fn input_session(input: &init::InitInput, connected: &init::Connected) -> Session {
    Session {
        base_url: input.base_url.clone(),
        flavor: connected.flavor,
        auth_method: connected.auth_method,
        username: input.username.clone(),
        secret_backend: SecretBackend::Keyring,
        created_at: now(),
        last_verified_at: Some(now()),
    }
}

/// Store the root's shared credential. Returns where it went.
fn store_shared(
    root: &Path,
    session: &Session,
    secret: &confed_api::Secret,
    backend: Option<SecretBackend>,
) -> Result<SecretBackend> {
    SessionStore::open_instance(root)?.save(session, secret, backend)
}

fn describe_backend(backend: SecretBackend) -> &'static str {
    match backend {
        SecretBackend::Keyring => "the OS keyring",
        SecretBackend::Sqlite => ".confed-instance-session.db (mode 0600)",
    }
}

/// An instance root must not be inside a workspace: discovery would find the
/// workspace first, and the two would claim the same files.
fn refuse_inside_workspace(root: &Path) -> Result<()> {
    let probe = if root.exists() {
        root.to_path_buf()
    } else {
        root.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    if let Ok(ws) = Workspace::discover(&probe) {
        return Err(ConfedError::state_with_hint(
            format!("{} is inside the workspace {}", root.display(), ws.root().display()),
            "an instance root holds workspaces; make it the directory above them",
        ));
    }
    Ok(())
}

// --------------------------------------------------------------- adopt ---

/// A workspace found under the root being adopted.
struct Found {
    dir: PathBuf,
    rel: String,
    key: String,
    numeric: Option<String>,
    base_url: String,
    flavor: Flavor,
    has_session: bool,
}

/// Every workspace to adopt, checked to be on one instance.
fn survey(root: &Path, dirs: &[PathBuf]) -> Result<Vec<Found>> {
    if root.join(STATE_DB_FILENAME).exists() {
        return Err(ConfedError::state_with_hint(
            format!("{} is a workspace itself", root.display()),
            "run `confed instance adopt` in the directory above it",
        ));
    }
    let root = root
        .canonicalize()
        .map_err(|e| ConfedError::io(format!("resolving {}", root.display()), e))?;
    let candidates: Vec<PathBuf> = if dirs.is_empty() {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&root)
            .map_err(|e| ConfedError::io(format!("reading {}", root.display()), e))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join(STATE_DB_FILENAME).exists())
            .collect();
        found.sort();
        found
    } else {
        dirs.iter().map(|d| root.join(d)).collect()
    };

    let mut found = Vec::new();
    for dir in candidates {
        if !dir.join(STATE_DB_FILENAME).exists() {
            return Err(ConfedError::usage_with_hint(
                format!("{} is not a confed workspace", dir.display()),
                "name directories made by `confed init` or `confed clone`",
            ));
        }
        let dir = dir
            .canonicalize()
            .map_err(|e| ConfedError::io(format!("resolving {}", dir.display()), e))?;
        let rel = dir.strip_prefix(&root).map(|p| p.to_string_lossy().replace('\\', "/")).map_err(
            |_| ConfedError::usage(format!("{} is not under {}", dir.display(), root.display())),
        )?;
        let ws = Workspace::open(&dir)?;
        let has_session = SessionStore::exists(&dir);
        let session = if has_session { ws.session()? } else { None };
        let base_url = ws
            .base_url()?
            .or_else(|| session.as_ref().map(|s| s.base_url.clone()))
            .unwrap_or_default();
        let flavor =
            ws.flavor()?.or(session.as_ref().map(|s| s.flavor)).unwrap_or(Flavor::DataCenter);
        found.push(Found {
            key: ws.space_key()?,
            numeric: ws.space_numeric_id()?,
            dir,
            rel,
            base_url,
            flavor,
            has_session,
        });
    }
    if found.is_empty() {
        return Err(ConfedError::state_with_hint(
            format!("no workspaces in {}", root.display()),
            "clone spaces with `confed instance init` and `confed instance clone`, or name \
             the workspaces to adopt",
        ));
    }

    // One instance, one flavor: name every workspace that disagrees.
    let first = &found[0];
    let odd: Vec<&Found> = found
        .iter()
        .filter(|f| {
            !instance::same_instance(&f.base_url, &first.base_url) || f.flavor != first.flavor
        })
        .collect();
    if !odd.is_empty() {
        let list = found
            .iter()
            .map(|f| format!("\n  - {}: {} ({})", f.rel, f.base_url, f.flavor))
            .collect::<String>();
        return Err(ConfedError::state_with_hint(
            format!("these workspaces are not all on one Confluence:{list}"),
            "an instance root is for one instance; adopt the matching ones by name \
             (`confed instance adopt DIR…`), and keep the others elsewhere",
        ));
    }
    for (i, f) in found.iter().enumerate() {
        if let Some(other) = found[..i].iter().find(|o| o.key == f.key) {
            return Err(ConfedError::state(format!(
                "{} and {} are both workspaces for space {}",
                other.rel, f.rel, f.key
            )));
        }
    }
    if let Some(existing) = Instance::discover(&root)?.filter(|i| i.root() == root) {
        if let Some(url) = existing.base_url()? {
            if !instance::same_instance(&url, &first.base_url) {
                return Err(ConfedError::state(format!(
                    "{} is already the instance root for {url}",
                    root.display()
                )));
            }
        }
    }
    Ok(found)
}

pub fn prepare_adopt(ctx: &mut Context, args: &InstanceAdoptArgs) -> Result<Option<Connection>> {
    if args.dry_run {
        return Ok(None);
    }
    let found = survey(&ctx.cwd, &args.dirs)?;
    let with_session: Vec<&Found> = found.iter().filter(|f| f.has_session).collect();
    let wanted = args.move_credentials
        || (!with_session.is_empty()
            && ctx.is_interactive()
            && !SessionStore::instance_exists(&ctx.cwd)
            && crate::prompt::confirm(
                &format!(
                    "Keep one credential in the root for all {} (--move-credentials)?",
                    plural(found.len(), "workspace", "workspaces")
                ),
                false,
            )?);
    if !wanted {
        return Ok(None);
    }

    let first = &found[0];
    let flagged = ctx.resolver.lookup(&confed_core::config::TOKEN, ctx.global.token.as_deref());
    let source = with_session.first();
    let session = match source {
        Some(f) => Workspace::open(&f.dir)?.session()?,
        None => None,
    };
    let secret = match (&flagged, source, &session) {
        (Some(resolved), _, _) => confed_api::Secret::new(resolved.value.clone()),
        (None, Some(f), Some(session)) => {
            SessionStore::open(&f.dir)?.load_secret(session)?.ok_or_else(|| {
                ConfedError::Auth(format!("{} stores no secret to share; pass --token", f.rel))
            })?
        }
        _ => {
            return Err(ConfedError::missing_value(
                "API token to share (no workspace stores one)",
                "--token",
                "CONFED_TOKEN",
            ))
        }
    };
    let username = ctx
        .resolver
        .lookup(&confed_core::config::USERNAME, ctx.global.username.as_deref())
        .map(|r| r.value)
        .or_else(|| session.as_ref().and_then(|s| s.username.clone()));
    Ok(Some(Connection {
        base_url: first.base_url.trim_end_matches('/').to_string(),
        flavor: first.flavor,
        auth_method: session
            .as_ref()
            .map(|s| s.auth_method)
            .unwrap_or_else(|| session::AuthMethod::default_for(first.flavor)),
        username,
        secret,
        source: source
            .map(|f| format!("{}/.session.db", f.rel))
            .unwrap_or_else(|| "--token".into()),
        session,
        members: Default::default(),
    }))
}

pub async fn adopt(
    ctx: &mut Context,
    args: &InstanceAdoptArgs,
    connection: Option<Connection>,
) -> Result<Output> {
    let root = ctx.cwd.canonicalize().map_err(|e| ConfedError::io("resolving the root", e))?;
    let found = survey(&root, &args.dirs)?;
    let first = &found[0];
    let registered: Vec<Value> =
        found.iter().map(|f| json!({ "space_key": f.key, "dir": f.rel })).collect();

    let style = &ctx.style;
    let mut human = String::new();
    if args.dry_run {
        let _ = writeln!(human, "{}", style.dim("Dry run — nothing was written."));
        for f in &found {
            let _ = writeln!(human, "  would register  {:<10} {}", f.key, f.rel);
        }
        let sessions = found.iter().filter(|f| f.has_session).count();
        if args.move_credentials {
            let _ =
                writeln!(human, "  would share one credential, replacing {sessions} of their own");
        }
        return Ok(Output::new(
            json!({
                "root": root.display().to_string(),
                "base_url": first.base_url,
                "flavor": first.flavor.as_str(),
                "dry_run": true,
                "registered": registered,
            }),
            human,
        ));
    }

    let instance = Instance::create(&root, &first.base_url, first.flavor)?;
    for f in &found {
        instance.add_member(&f.key, f.numeric.as_deref(), &f.rel)?;
        let _ = writeln!(human, "  registered  {:<10} {}", style.bold(&f.key), f.rel);
    }
    let gitignore = instance::ensure_gitignore(&root)?;

    // The shared credential, only once it is known to work.
    let mut credentials =
        json!({ "shared": SessionStore::instance_exists(&root), "moved_from": [] });
    if let Some(connection) = connection {
        let client = connection.client(ctx)?;
        let user = client.whoami().await.map_err(|e| match e {
            confed_api::ApiError::Auth(m) => ConfedError::Auth(format!(
                "the credential from {} does not authenticate against {} ({m}); nothing was moved",
                connection.source, connection.base_url
            )),
            other => other.into(),
        })?;
        let session = Session {
            base_url: connection.base_url.clone(),
            flavor: connection.flavor,
            auth_method: connection.auth_method,
            username: connection.username.clone(),
            secret_backend: SecretBackend::Keyring,
            created_at: now(),
            last_verified_at: Some(now()),
        };
        let wanted = args
            .credential_store
            .as_deref()
            .map(|s| if s == "keyring" { SecretBackend::Keyring } else { SecretBackend::Sqlite })
            .or(connection.session.as_ref().map(|s| s.secret_backend));
        let backend = store_shared(&root, &session, &connection.secret, wanted)?;
        let stored = Session { secret_backend: backend, ..session };

        let mut moved_from = Vec::new();
        for f in found.iter().filter(|f| f.has_session) {
            let store = SessionStore::open(&f.dir)?;
            if let Some(own) = store.load()? {
                // The shared one may be the very same keyring entry.
                if !session::same_keyring_entry(&own, &stored) {
                    store.clear(&own)?;
                }
            }
            drop(store);
            let path = f.dir.join(session::SESSION_DB_FILENAME);
            std::fs::remove_file(&path)
                .map_err(|e| ConfedError::io(format!("removing {}", path.display()), e))?;
            moved_from.push(f.rel.clone());
        }
        let _ = writeln!(
            human,
            "Credentials of {} on {} now in {}; removed from {}",
            user.display_name,
            connection.base_url,
            describe_backend(backend),
            if moved_from.is_empty() { "no workspace".to_string() } else { moved_from.join(", ") }
        );
        credentials = json!({
            "shared": true,
            "store": backend.as_str(),
            "source": connection.source,
            "moved_from": moved_from,
        });
    }

    let mut docs = Vec::new();
    if !args.no_agent_docs {
        docs =
            agent_docs::write_instance(&root, &first.base_url, first.flavor, &instance.members()?)?;
        for f in &found {
            agent_docs::refresh_member(&Workspace::open(&f.dir)?)?;
        }
    }
    let _ = writeln!(
        human,
        "\n{} adopted in {}",
        plural(found.len(), "space", "spaces"),
        root.display()
    );
    Ok(Output::new(
        json!({
            "root": root.display().to_string(),
            "base_url": first.base_url,
            "flavor": first.flavor.as_str(),
            "dry_run": false,
            "registered": registered,
            "credentials": credentials,
            "gitignore_added": gitignore,
            "agent_docs": docs,
        }),
        human,
    ))
}

// -------------------------------------------------------------- spaces ---

pub async fn spaces(
    ctx: &mut Context,
    args: &InstanceSpacesArgs,
    connection: Option<Connection>,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let mut rows: Vec<Value> = Vec::new();
    for member in instance.members()? {
        rows.push(local_row(&instance, &member));
    }

    let mut warnings = Vec::new();
    if let Some(connection) = connection.filter(|_| args.remote) {
        let client = connection.client(ctx)?;
        let mut listed = match args.status.as_deref() {
            Some("archived") => client.list_archived_spaces().await?,
            _ => client.list_spaces(None).await?,
        };
        listed.retain(|s| match args.space_type.as_deref() {
            Some("personal") => is_personal(s),
            Some(_) => !is_personal(s),
            None => true,
        });
        listed.sort_by(|a, b| a.id.key.cmp(&b.id.key));
        for space in &listed {
            match rows.iter_mut().find(|r| r["space_key"] == json!(space.id.key)) {
                Some(row) => {
                    row["name"] = json!(space.name);
                    row["type"] = json!(space.kind);
                    row["status"] = json!(space.status);
                    row["on_server"] = json!(true);
                }
                None => rows.push(json!({
                    "space_key": space.id.key,
                    "name": space.name,
                    "type": space.kind,
                    "status": space.status,
                    "dir": instance::default_dir(&space.id.key),
                    "cloned": false,
                    "on_server": true,
                })),
            }
        }
        for row in rows.iter_mut().filter(|r| r.get("on_server").is_none()) {
            row["on_server"] = json!(false);
            if args.space_type.is_none() && args.status.is_none() {
                warnings.push(format!(
                    "{} is registered but not listed by the server",
                    row["space_key"].as_str().unwrap_or_default()
                ));
            }
        }
        if args.space_type.is_some() || args.status.is_some() {
            rows.retain(|r| r["on_server"] == json!(true));
        }
    }

    let style = &ctx.style;
    let mut human = String::new();
    let _ = writeln!(
        human,
        "{:<10} {:<24} {:<14} {:>6} {:>6} {:>6}  LAST FETCH",
        "KEY", "NAME", "DIR", "PAGES", "DIRTY", "CONFL"
    );
    for row in &rows {
        let text = |k: &str| row[k].as_str().unwrap_or("").to_string();
        let num = |k: &str| row[k].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "-".into());
        let cloned = row["cloned"].as_bool().unwrap_or(false);
        let dir = if cloned { text("dir") } else { style.dim("not cloned") };
        let fetched = text("last_fetch");
        let _ = writeln!(
            human,
            "{:<10} {:<24} {:<14} {:>6} {:>6} {:>6}  {}",
            text("space_key"),
            truncate(&text("name"), 24),
            dir,
            num("pages"),
            num("dirty"),
            num("conflicts"),
            if fetched.is_empty() { "-".to_string() } else { fetched }
        );
    }
    Ok(Output::new(json!({ "root": instance.root().display().to_string(), "spaces": rows }), human)
        .warn_all(warnings))
}

/// What a registered workspace looks like on disk.
fn local_row(instance: &Instance, member: &Member) -> Value {
    let mut row = json!({
        "space_key": member.space_key,
        "dir": member.dir,
        "enabled": member.enabled,
        "cloned": false,
    });
    let Ok(ws) = instance.open_workspace(member) else { return row };
    row["cloned"] = json!(true);
    row["name"] = json!(ws.state().get_meta("space_name").ok().flatten());
    row["last_fetch"] = json!(ws.state().get_meta("last_fetch_at").ok().flatten());
    row["pages"] = json!(ws.state().all_pages().map(|p| p.len()).unwrap_or(0));
    if let Ok(scan) = worktree::scan(&ws) {
        let dirty = scan.pages.iter().filter(|p| p.state.has_local_work()).count();
        let conflicts =
            scan.pages.iter().filter(|p| p.state == worktree::PageState::Conflicted).count();
        let behind = scan.pages.iter().filter(|p| p.state.has_remote_work()).count();
        row["dirty"] = json!(dirty);
        row["conflicts"] = json!(conflicts);
        row["behind"] = json!(behind);
    }
    row
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(width - 1).collect::<String>())
    }
}

fn is_personal(space: &Space) -> bool {
    space.kind.as_deref() == Some("personal") || space.id.key.starts_with('~')
}

// --------------------------------------------------------------- clone ---

/// What `clone` decided for one space.
enum Plan {
    Clone { reinit: bool },
    Skip(&'static str),
    Fail(ConfedError),
}

pub async fn clone(
    ctx: &mut Context,
    args: &InstanceCloneArgs,
    connection: Connection,
) -> Result<Output> {
    let instance = ctx.instance()?;
    let root = instance.root().to_path_buf();
    let client = connection.client(ctx)?;
    // One check up front: a rejected credential fails before any folder exists.
    client.whoami().await.map_err(|e| match e {
        confed_api::ApiError::Auth(m) => ConfedError::Auth(format!(
            "could not authenticate against {} with {}: {m}",
            connection.base_url, connection.source
        )),
        other => other.into(),
    })?;

    let mut listed = client.list_spaces(None).await?;
    if args.include_archived {
        listed.extend(client.list_archived_spaces().await?);
    }
    let named = |list: &[String], key: &str| list.iter().any(|k| k.eq_ignore_ascii_case(key));
    let want_personal = args.space_type.as_deref() == Some("personal");
    let mut chosen: Vec<Space> = listed
        .iter()
        .filter(|s| match args.all {
            true => is_personal(s) == want_personal,
            false => named(&args.keys, &s.id.key),
        })
        .filter(|s| !named(&args.exclude, &s.id.key))
        .cloned()
        .collect();
    chosen.sort_by(|a, b| a.id.key.cmp(&b.id.key));
    chosen.dedup_by(|a, b| a.id.key == b.id.key);
    let unknown: Vec<String> = args
        .keys
        .iter()
        .filter(|k| !listed.iter().any(|s| s.id.key.eq_ignore_ascii_case(k)))
        .cloned()
        .collect();

    // Decide every space before touching any.
    let mut decided: Vec<(Space, String, Plan)> = Vec::new();
    for space in chosen {
        let key = space.id.key.clone();
        let rel =
            instance.member(&key)?.map(|m| m.dir).unwrap_or_else(|| instance::default_dir(&key));
        let dir = root.join(&rel);
        let plan = if dir.join(STATE_DB_FILENAME).exists() {
            match Workspace::open(&dir).and_then(|ws| Ok((ws.space_key()?, ws.base_url()?))) {
                Ok((bound, _)) if !bound.eq_ignore_ascii_case(&key) => {
                    Plan::Fail(ConfedError::state_with_hint(
                        format!("{rel} is already a workspace for space {bound}"),
                        format!("move it, or leave the space out with --exclude {key}"),
                    ))
                }
                Ok((_, Some(url))) if !instance::same_instance(&url, &connection.base_url) => {
                    Plan::Fail(ConfedError::state(format!("{rel} is a workspace on {url}")))
                }
                Ok(_) if args.force => Plan::Clone { reinit: true },
                Ok(_) => Plan::Skip("already cloned"),
                Err(e) => Plan::Fail(e),
            }
        } else if dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
            Plan::Fail(ConfedError::state_with_hint(
                format!("{rel} exists and is not empty"),
                format!("move it, or leave the space out with --exclude {key}"),
            ))
        } else {
            Plan::Clone { reinit: false }
        };
        decided.push((space, rel, plan));
    }

    let style = &ctx.style;
    let mut human = String::new();
    if args.dry_run {
        let _ = writeln!(human, "{}", style.dim("Dry run — nothing was written."));
    }

    // Bind the new workspaces and register them, one after another: quick,
    // and the register is one database.
    let mut work: Vec<(Space, String, Workspace)> = Vec::new();
    let mut entries: Vec<Value> = Vec::new();
    let mut errors: Vec<ErrorEntry> = Vec::new();
    let mut codes: Vec<ExitCode> = Vec::new();
    for (space, rel, plan) in decided {
        let key = space.id.key.clone();
        match plan {
            Plan::Skip(why) => {
                if instance.member(&key)?.is_none() && !args.dry_run {
                    instance.add_member(&key, space.id.numeric.as_deref(), &rel)?;
                }
                codes.push(ExitCode::Ok);
                let _ = writeln!(human, "  {:<8} {key:<10} {rel} ({why})", "skipped");
                entries.push(json!({
                    "space_key": key, "dir": rel, "ok": true, "exit_code": 0,
                    "status": "skipped",
                }));
            }
            Plan::Fail(e) => {
                codes.push(e.exit_code());
                let _ = writeln!(human, "  {:<8} {key:<10} {e}", style.red("failed"));
                entries.push(failed_entry(&key, &rel, &e));
                errors.push(ErrorEntry::from_error(&e).in_space(&key));
            }
            Plan::Clone { reinit } if args.dry_run => {
                codes.push(ExitCode::Ok);
                let what = if reinit { "re-init" } else { "clone" };
                let _ = writeln!(human, "  {what:<8} {key:<10} {rel}");
                entries.push(json!({
                    "space_key": key, "dir": rel, "ok": true, "exit_code": 0,
                    "status": format!("would_{}", if reinit { "reinit" } else { "clone" }),
                }));
            }
            Plan::Clone { .. } => match bind(&root.join(&rel), &connection, &space) {
                Ok(ws) => {
                    instance.add_member(&key, space.id.numeric.as_deref(), &rel)?;
                    if !args.no_agent_docs {
                        agent_docs::write(
                            ws.root(),
                            &connection.base_url,
                            connection.flavor,
                            &key,
                            None,
                        )?;
                    }
                    work.push((space, rel, ws));
                }
                Err(e) => {
                    codes.push(e.exit_code());
                    let _ = writeln!(human, "  {:<8} {key:<10} {e}", style.red("failed"));
                    entries.push(failed_entry(&key, &rel, &e));
                    errors.push(ErrorEntry::from_error(&e).in_space(&key));
                }
            },
        }
    }

    // Then the pulls, in parallel, all through the one client.
    let parallel = match ctx.concurrency() {
        0 => client.capabilities().max_request_concurrency,
        n => n,
    }
    .clamp(1, work.len().max(1));
    let concurrency = ctx.concurrency();
    let opts = PullOptions {
        with_attachments: !args.no_attachments,
        with_comments: !args.no_comments,
        ..PullOptions::everything()
    };
    let pulled: Vec<(Space, String, Workspace, Result<confed_core::sync::PullOutcome>)> =
        futures::stream::iter(work)
            .map(|(space, rel, mut ws)| {
                let engine = SyncEngine::new(client.clone(), space.id.clone(), concurrency);
                let opts = opts.clone();
                async move {
                    let outcome = match ws.lock() {
                        Ok(_lock) => engine.pull(&mut ws, &opts).await,
                        Err(e) => Err(e),
                    };
                    (space, rel, ws, outcome)
                }
            })
            .buffer_unordered(parallel)
            .collect()
            .await;
    let mut warnings = Vec::new();
    for (space, rel, ws, outcome) in pulled {
        let key = space.id.key.clone();
        match outcome {
            Ok(pull) => {
                warnings.extend(pull.warnings.iter().map(|w| format!("{key}: {w}")));
                let code =
                    if pull.conflicted.is_empty() { ExitCode::Ok } else { ExitCode::Conflict };
                codes.push(code);
                let pages = ws.state().all_pages().map(|p| p.len()).unwrap_or(0);
                let _ = writeln!(
                    human,
                    "  {:<8} {key:<10} {rel} ({})",
                    style.green("cloned"),
                    plural(pages, "page", "pages")
                );
                entries.push(json!({
                    "space_key": key, "dir": rel, "ok": code == ExitCode::Ok,
                    "exit_code": code.as_i32(), "status": "cloned", "pages": pages,
                    "result": serde_json::to_value(&pull)?,
                }));
            }
            Err(e) => {
                codes.push(e.exit_code());
                let _ = writeln!(human, "  {:<8} {key:<10} {e}", style.red("failed"));
                entries.push(failed_entry(&key, &rel, &e));
                errors.push(ErrorEntry::from_error(&e).in_space(&key));
            }
        }
    }
    for key in unknown {
        let e = ConfedError::NotFound(format!("space {key} is not visible to this account"));
        codes.push(e.exit_code());
        let _ = writeln!(human, "  {:<8} {key:<10} {e}", style.red("failed"));
        entries.push(failed_entry(&key, &instance::default_dir(&key), &e));
        errors.push(ErrorEntry::from_error(&e).in_space(&key));
    }
    entries.sort_by(|a, b| a["space_key"].as_str().cmp(&b["space_key"].as_str()));
    if entries.is_empty() {
        let _ = writeln!(human, "No spaces matched.");
    }

    if !args.dry_run && !args.no_agent_docs {
        let members = instance.members()?;
        agent_docs::write_instance(&root, &connection.base_url, connection.flavor, &members)?;
    }

    let mut output = Output::new(
        json!({ "root": root.display().to_string(), "dry_run": args.dry_run, "spaces": entries }),
        human,
    )
    .with_errors(errors)
    .warn_all(warnings);
    // As `clone --all`: any space that failed makes the whole run exit 8.
    if codes.iter().any(|c| *c != ExitCode::Ok) {
        output.exit = ExitCode::Partial;
    }
    Ok(output)
}

fn failed_entry(key: &str, dir: &str, e: &ConfedError) -> Value {
    json!({
        "space_key": key, "dir": dir, "ok": false, "exit_code": e.exit_code().as_i32(),
        "status": "failed", "errors": [ErrorEntry::from_error(e)],
    })
}

/// A new workspace for `space`, bound and ignored, with no credential of its
/// own: it uses the root's.
fn bind(dir: &Path, connection: &Connection, space: &Space) -> Result<Workspace> {
    let ws = match Workspace::open(dir) {
        Ok(ws) if dir.join(STATE_DB_FILENAME).exists() => ws,
        _ => Workspace::create(dir)?,
    };
    let state = ws.state();
    state.set_meta("base_url", &connection.base_url)?;
    state.set_meta("flavor", connection.flavor.as_str())?;
    state.set_meta("space_key", &space.id.key)?;
    if let Some(numeric) = &space.id.numeric {
        state.set_meta("space_id", numeric)?;
    }
    state.set_meta("space_name", &space.name)?;
    workspace::ensure_gitignore(dir)?;
    Ok(ws)
}

// ---------------------------------------------------------- add, remove ---

pub fn add(ctx: &mut Context, args: &InstanceAddArgs) -> Result<Output> {
    let instance = ctx.instance()?;
    let rel = match &args.dir {
        Some(dir) => relative_to_root(&instance, &ctx.cwd.join(dir))?,
        None => instance::default_dir(&args.key),
    };
    let dir = instance.root().join(&rel);
    if !dir.join(STATE_DB_FILENAME).exists() {
        return Err(ConfedError::state_with_hint(
            format!("{rel} is not a workspace"),
            format!("`confed instance clone {}` clones the space there", args.key),
        ));
    }
    let ws = Workspace::open(&dir)?;
    let bound = ws.space_key()?;
    if !bound.eq_ignore_ascii_case(&args.key) {
        return Err(ConfedError::usage(format!(
            "{rel} is the workspace of space {bound}, not {}",
            args.key
        )));
    }
    if let (Some(url), Some(root_url)) = (ws.base_url()?, instance.base_url()?) {
        if !instance::same_instance(&url, &root_url) {
            return Err(ConfedError::state(format!(
                "{rel} is a workspace on {url}, and this root is for {root_url}"
            )));
        }
    }
    let member = instance.add_member(&bound, ws.space_numeric_id()?.as_deref(), &rel)?;
    agent_docs::refresh_member(&ws)?;
    refresh_root_docs(&instance)?;
    Ok(Output::new(
        json!({ "space_key": member.space_key, "dir": member.dir, "registered": true }),
        format!("Registered {} in {}\n", member.space_key, member.dir),
    ))
}

pub fn remove(ctx: &mut Context, args: &InstanceRemoveArgs) -> Result<Output> {
    let instance = ctx.instance()?;
    let member = instance.member(&args.key)?.ok_or_else(|| {
        ConfedError::NotFound(format!("space {} is not registered in this instance", args.key))
    })?;
    let dir = instance.member_dir(&member);
    let ws = instance.open_workspace(&member).ok();

    if args.delete_files {
        if let Some(ws) = &ws {
            let scan = worktree::scan(ws)?;
            let dirty: Vec<String> = scan
                .pages
                .iter()
                .filter(|p| {
                    p.state.has_local_work()
                        || p.state == worktree::PageState::Conflicted
                        || p.comment_drafts > 0
                        || !p.tampering.is_empty()
                })
                .map(|p| format!("\n  - {} ({})", p.path, p.state.as_str()))
                .collect();
            if !dirty.is_empty() {
                return Err(ConfedError::state_with_hint(
                    format!(
                        "{} has local changes that deleting it would lose:{}",
                        member.dir,
                        dirty.concat()
                    ),
                    format!(
                        "push them (`confed -C {} push`), or remove it without --delete-files",
                        member.dir
                    ),
                ));
            }
            let _lock = ws.lock()?;
        }
        // Never anything but a member directory strictly inside the root.
        let root = instance.root().canonicalize().unwrap_or_else(|_| instance.root().to_path_buf());
        let target = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if target == root || !target.starts_with(&root) {
            return Err(ConfedError::state(format!("refusing to delete {}", target.display())));
        }
    }

    instance.remove_member(&member.space_key)?;
    if args.delete_files {
        drop(ws);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| ConfedError::io(format!("deleting {}", dir.display()), e))?;
        }
    } else if let Some(ws) = &ws {
        agent_docs::refresh_member(ws)?;
    }
    refresh_root_docs(&instance)?;

    let human = if args.delete_files {
        format!("Removed {} and deleted {}\n", member.space_key, member.dir)
    } else {
        format!("Removed {}; its files stay in {}\n", member.space_key, member.dir)
    };
    Ok(Output::new(
        json!({
            "space_key": member.space_key,
            "dir": member.dir,
            "removed": true,
            "files_deleted": args.delete_files,
        }),
        human,
    ))
}

fn relative_to_root(instance: &Instance, path: &Path) -> Result<String> {
    let root = instance.root().canonicalize().unwrap_or_else(|_| instance.root().to_path_buf());
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(&root).map(|p| p.to_string_lossy().replace('\\', "/")).map_err(|_| {
        ConfedError::usage(format!("{} is not inside {}", path.display(), root.display()))
    })
}

/// Rewrite the root's contract, if it has one, to list the spaces as they are.
pub fn refresh_root_docs(instance: &Instance) -> Result<()> {
    let root = instance.root();
    if agent_docs::FILENAMES.iter().all(|n| !root.join(n).exists()) {
        return Ok(());
    }
    let base_url = instance.base_url()?.unwrap_or_default();
    let flavor = instance.flavor()?.unwrap_or(Flavor::DataCenter);
    agent_docs::write_instance(root, &base_url, flavor, &instance.members()?)?;
    Ok(())
}
