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
    let width = column(found.iter().map(|f| f.key.clone()), "");
    if args.dry_run {
        let _ = writeln!(human, "{}", style.dim("Dry run — nothing was written."));
        for f in &found {
            let _ = writeln!(human, "  would register  {:<width$}  {}", f.key, f.rel);
        }
        let credentials = credential_plan(ctx, args, &found)?;
        if let Some(text) = credentials.get("summary").and_then(Value::as_str) {
            let _ = writeln!(human, "\n{text}");
        }
        return Ok(Output::new(
            json!({
                "root": root.display().to_string(),
                "base_url": first.base_url,
                "flavor": first.flavor.as_str(),
                "dry_run": true,
                "registered": registered,
                "credentials": credentials,
            }),
            human,
        ));
    }

    let instance = Instance::create(&root, &first.base_url, first.flavor)?;
    for f in &found {
        instance.add_member(&f.key, f.numeric.as_deref(), &f.rel)?;
        let _ = writeln!(human, "  registered  {} {}", style.bold(&pad(&f.key, width)), f.rel);
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

/// What `--move-credentials` would do, read without touching any secret: where
/// the credential comes from, where it would go, and what would be removed.
fn credential_plan(ctx: &Context, args: &InstanceAdoptArgs, found: &[Found]) -> Result<Value> {
    let mut sessions: Vec<(&Found, Session)> = Vec::new();
    for f in found.iter().filter(|f| f.has_session) {
        if let Some(session) = Workspace::open(&f.dir)?.session()? {
            sessions.push((f, session));
        }
    }
    if !args.move_credentials {
        let summary = match sessions.len() {
            0 => "No workspace stores a credential of its own.".to_string(),
            n => format!(
                "{} {} a credential of its own; --move-credentials would share one instead.",
                plural(n, "workspace", "workspaces"),
                if n == 1 { "keeps" } else { "each keep" }
            ),
        };
        return Ok(json!({ "would_share": false, "summary": summary }));
    }

    let flagged = ctx.resolver.lookup(&confed_core::config::TOKEN, ctx.global.token.as_deref());
    let source_session = sessions.first().map(|(_, s)| s.clone());
    let source = match (&flagged, sessions.first()) {
        (Some(r), _) => r.source.as_str().to_string(),
        (None, Some((f, s))) => format!(
            "{}/.session.db ({})",
            f.rel,
            if s.secret_backend == SecretBackend::Keyring {
                "the OS keyring"
            } else {
                "in the file"
            }
        ),
        (None, None) => {
            return Err(ConfedError::missing_value(
                "API token to share (no workspace stores one)",
                "--token",
                "CONFED_TOKEN",
            ))
        }
    };
    let store = match args.credential_store.as_deref() {
        Some("keyring") => SecretBackend::Keyring,
        Some(_) => SecretBackend::Sqlite,
        None => source_session.as_ref().map(|s| s.secret_backend).unwrap_or(SecretBackend::Keyring),
    };
    // The shared session as it would be saved: its keyring entry is per base
    // URL and user, so a workspace using that same entry keeps it.
    let shared = source_session.map(|s| Session { secret_backend: store, ..s });
    let removed: Vec<String> =
        sessions.iter().map(|(f, _)| format!("{}/.session.db", f.rel)).collect();
    let mut keyring_deleted: Vec<String> = sessions
        .iter()
        .filter(|(_, own)| own.secret_backend == SecretBackend::Keyring)
        .filter(|(_, own)| shared.as_ref().is_none_or(|sh| !session::same_keyring_entry(own, sh)))
        .map(|(_, own)| {
            format!("confed: {}|{}", own.base_url, own.username.clone().unwrap_or_default())
        })
        .collect();
    keyring_deleted.sort();
    keyring_deleted.dedup();

    let mut summary = format!(
        "With --move-credentials it would:\n  \
         - check the credential from {source} with `whoami` first, and stop if it fails\n  \
         - store it in {}\n",
        describe_backend(store)
    );
    if !removed.is_empty() {
        summary.push_str(&format!("  - remove {}\n", removed.join(", ")));
    }
    for entry in &keyring_deleted {
        summary.push_str(&format!(
            "  - delete the keyring entry \"{entry}\" (other copies of these workspaces that \
             use it would lose their credential)\n"
        ));
    }
    Ok(json!({
        "would_share": true,
        "source": source,
        "store": store.as_str(),
        "remove": removed,
        "keyring_entries_deleted": keyring_deleted,
        "verified_first": true,
        "summary": summary.trim_end(),
    }))
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
        // Where each space not cloned yet would go: a personal one's owner is
        // looked up, all of them at once under the client's request ceiling.
        let registered: Vec<String> =
            instance.members()?.into_iter().map(|m| m.space_key).collect();
        let folders: std::collections::HashMap<String, String> =
            futures::stream::iter(listed.iter().filter(|s| !registered.contains(&s.id.key)))
                .map(|space| {
                    let (client, instance) = (client.clone(), &instance);
                    async move {
                        (space.id.key.clone(), folder_for(client.as_ref(), instance, space).await)
                    }
                })
                .buffer_unordered(client.capabilities().max_request_concurrency.max(1))
                .collect()
                .await;
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
                    "dir": folders.get(&space.id.key).cloned().unwrap_or_default(),
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
    let text = |row: &Value, k: &str| row[k].as_str().unwrap_or("").to_string();
    let num =
        |row: &Value, k: &str| row[k].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "-".into());
    let dir_of = |row: &Value| match row["cloned"].as_bool().unwrap_or(false) {
        true => text(row, "dir"),
        false => format!("not cloned ({})", text(row, "dir")),
    };
    // Columns as wide as what is in them: keys of personal spaces are long ids.
    let key_w = column(rows.iter().map(|r| text(r, "space_key")), "KEY");
    let name_w = column(rows.iter().map(|r| truncate(&text(r, "name"), 32)), "NAME");
    let dir_w = column(rows.iter().map(dir_of), "DIR");
    let mut human = format!(
        "{} {} {} {:>6} {:>6} {:>6}  LAST FETCH\n",
        pad("KEY", key_w),
        pad("NAME", name_w),
        pad("DIR", dir_w),
        "PAGES",
        "DIRTY",
        "CONFL"
    );
    for row in &rows {
        let cloned = row["cloned"].as_bool().unwrap_or(false);
        let dir = pad(&dir_of(row), dir_w);
        let fetched = text(row, "last_fetch");
        let _ = writeln!(
            human,
            "{} {} {} {:>6} {:>6} {:>6}  {}",
            pad(&text(row, "space_key"), key_w),
            pad(&truncate(&text(row, "name"), 32), name_w),
            if cloned { dir } else { style.dim(&dir) },
            num(row, "pages"),
            num(row, "dirty"),
            num(row, "conflicts"),
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

/// The width a column needs for its heading and every cell.
fn column(cells: impl Iterator<Item = String>, heading: &str) -> usize {
    cells.map(|c| display_width(&c)).chain([display_width(heading)]).max().unwrap_or(0)
}

/// Columns on a terminal: wide characters take two cells.
fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// `text`, padded with spaces to `width` terminal cells. Pad before styling:
/// colour codes take no cells but count as characters.
pub(crate) fn pad(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    out.push_str(&" ".repeat(width.saturating_sub(display_width(text))));
    out
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(width - 1).collect::<String>())
    }
}

/// The directory a space is cloned into: its key, or for a personal space
/// `@` and its owner's name.
///
/// A personal space's key is `~` and an id of its owner: the username on
/// older Data Center spaces, the user key on newer ones (a long hex string),
/// the account id on Cloud. In that order of preference the folder is named
/// for the owner's username, as the server reports it; for an id that is a
/// username already; or for the space's name, which is the owner's display
/// name (`@Alice-Ng`) — what is left when the owner cannot be looked up, as
/// for a deactivated account. Only when none of them makes a safe directory
/// name, or another space has it, is the folder `@` and the id.
async fn folder_for(
    client: &dyn confed_api::ConfluenceClient,
    instance: &Instance,
    space: &Space,
) -> String {
    let fallback = instance::default_dir(&space.id.key);
    let Some(id) = space.id.key.strip_prefix('~').filter(|id| !id.is_empty()) else {
        return fallback;
    };
    let reference = match client.flavor() {
        Flavor::DataCenter => confed_api::UserReference::UserKey(id.to_string()),
        Flavor::Cloud => confed_api::UserReference::AccountId(id.to_string()),
    };
    let username = client.lookup_user(&reference).await.ok().and_then(|u| u.username);
    let candidates = [
        username.filter(|n| safe_dir_name(n)),
        Some(id.to_string()).filter(|id| safe_dir_name(id) && !looks_like_an_id(id)),
        name_as_dir(&space.name),
    ];
    let members = instance.members().unwrap_or_default();
    for name in candidates.into_iter().flatten() {
        let dir = format!("@{name}");
        let taken =
            members.iter().any(|m| m.dir.eq_ignore_ascii_case(&dir) && m.space_key != space.id.key);
        if !taken {
            return dir;
        }
    }
    fallback
}

/// A user key or account id rather than a name: long, and hex, or with the
/// `:` of a Cloud account id.
fn looks_like_an_id(id: &str) -> bool {
    id.contains(':') || (id.len() >= 20 && id.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A display name as a directory name: letters and digits of any script
/// kept, spaces made `-`, anything else dropped. `Alice Ng` → `Alice-Ng`.
fn name_as_dir(name: &str) -> Option<String> {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if c.is_whitespace() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches(|c| c == '-' || c == '.').to_string();
    safe_dir_name(&out).then_some(out)
}

/// A username that can be a directory name as it is, on every platform.
fn safe_dir_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '+'))
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
        let rel = match instance.member(&key)? {
            Some(member) => member.dir,
            None => folder_for(client.as_ref(), &instance, &space).await,
        };
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
    let kw =
        column(decided.iter().map(|(s, _, _)| s.id.key.clone()).chain(unknown.iter().cloned()), "");
    let kcol = |k: &str| pad(k, kw);
    let label = |l: &str| pad(l, 8);

    // Bind the new workspaces and register them, one after another: quick,
    // and the register is one database.
    let mut work: Vec<(Space, String, Workspace)> = Vec::new();
    let mut own_credentials = false;
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
                let _ = writeln!(human, "  {} {} {rel} ({why})", label("skipped"), kcol(&key));
                entries.push(json!({
                    "space_key": key, "dir": rel, "ok": true, "exit_code": 0,
                    "status": "skipped",
                }));
            }
            Plan::Fail(e) => {
                codes.push(e.exit_code());
                let _ = writeln!(human, "  {} {} {e}", style.red(&label("failed")), kcol(&key));
                entries.push(failed_entry(&key, &rel, &e));
                errors.push(ErrorEntry::from_error(&e).in_space(&key));
            }
            Plan::Clone { reinit } if args.dry_run => {
                codes.push(ExitCode::Ok);
                let what = if reinit { "re-init" } else { "clone" };
                let _ = writeln!(human, "  {} {} {rel}", label(what), kcol(&key));
                entries.push(json!({
                    "space_key": key, "dir": rel, "ok": true, "exit_code": 0,
                    "status": format!("would_{}", if reinit { "reinit" } else { "clone" }),
                }));
            }
            Plan::Clone { .. } => match bind(&root.join(&rel), &connection, &space)
                .and_then(|ws| give_credential(&root, &ws, &connection).map(|own| (ws, own)))
            {
                Ok((ws, own)) => {
                    own_credentials |= own;
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
                    let _ = writeln!(human, "  {} {} {e}", style.red(&label("failed")), kcol(&key));
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
                    "  {} {} {rel} ({})",
                    style.green(&label("cloned")),
                    kcol(&key),
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
                let _ = writeln!(human, "  {} {} {e}", style.red(&label("failed")), kcol(&key));
                entries.push(failed_entry(&key, &rel, &e));
                errors.push(ErrorEntry::from_error(&e).in_space(&key));
            }
        }
    }
    for key in unknown {
        let e = ConfedError::NotFound(format!("space {key} is not visible to this account"));
        codes.push(e.exit_code());
        let _ = writeln!(human, "  {} {} {e}", style.red(&label("failed")), kcol(&key));
        entries.push(failed_entry(&key, &instance::default_dir(&key), &e));
        errors.push(ErrorEntry::from_error(&e).in_space(&key));
    }
    entries.sort_by(|a, b| a["space_key"].as_str().cmp(&b["space_key"].as_str()));
    if entries.is_empty() {
        let _ = writeln!(human, "No spaces matched.");
    }

    if own_credentials {
        let _ = writeln!(
            human,
            "\n{}",
            style.dim(
                "The root keeps no shared credential, so each new workspace got its own copy \
                 of the one used here; `confed instance adopt --move-credentials` shares one."
            )
        );
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

/// Give a new workspace the credential it was cloned with, when the root
/// keeps none to share — otherwise it could not be used on its own. Stored
/// the way its source stores it. Returns whether it wrote one.
fn give_credential(root: &Path, ws: &Workspace, connection: &Connection) -> Result<bool> {
    if SessionStore::instance_exists(root) {
        return Ok(false);
    }
    let session = Session {
        base_url: connection.base_url.clone(),
        flavor: connection.flavor,
        auth_method: connection.auth_method,
        username: connection.username.clone(),
        secret_backend: SecretBackend::Keyring,
        created_at: now(),
        last_verified_at: Some(now()),
    };
    let backend = connection.session.as_ref().map(|s| s.secret_backend);
    SessionStore::open(ws.root())?.save(&session, &connection.secret, backend)?;
    Ok(true)
}

/// A new workspace for `space`, bound and ignored. Its credential is the
/// root's, or — when the root keeps none — see [`give_credential`].
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_usernames_become_directory_names() {
        for good in ["alice", "alice.ng", "a_lee-2", "ana+docs"] {
            assert!(safe_dir_name(good), "{good}");
        }
        for bad in ["", ".", "..", ".hidden", "a/b", "a\\b", "ali ce", "a:b"] {
            assert!(!safe_dir_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_display_name_becomes_a_directory_name_and_an_id_is_not_a_name() {
        assert_eq!(name_as_dir("Alice Ng").as_deref(), Some("Alice-Ng"));
        assert_eq!(name_as_dir("  Иван  Петров ").as_deref(), Some("Иван-Петров"));
        assert_eq!(name_as_dir("Dana (Deactivated)").as_deref(), Some("Dana-Deactivated"));
        assert_eq!(name_as_dir("???"), None);
        assert!(looks_like_an_id("ff8081817a1b2c3d4e5f6071"));
        assert!(looks_like_an_id("900000:0a1b2c3d-aaaa"));
        assert!(!looks_like_an_id("alice"));
    }

    #[test]
    fn padding_counts_terminal_cells_not_bytes() {
        assert_eq!(pad("DOCS", 6), "DOCS  ");
        assert_eq!(pad("Фраза", 7), "Фраза  ");
        assert_eq!(pad("文档", 6), "文档  ", "wide characters take two cells");
        assert_eq!(pad("~9000000a1b2c3d", 4), "~9000000a1b2c3d", "never cut");
    }
}
