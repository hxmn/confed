//! `confed instance` — an instance root: one workspace per space of one
//! Confluence, managed together. Design note: docs/design/07-instance-roots.md.
//!
//! Like `init`, each command is split in two: [`prepare`] runs before the
//! async runtime starts — it may prompt, and it reads the OS keyring — and
//! [`run`] does the rest. Commands that run in every space (`fanout`) call
//! the single-space command in each workspace, in this process, over one
//! client: one credential, and one request ceiling for all of them.

mod fanout;
mod mv;
mod setup;

use crate::cli::InstanceCommand;
use crate::context::Context;
use crate::output::{ErrorEntry, Output};
use confed_api::{Auth, ConfluenceClient, Flavor, Secret};
use confed_core::config;
use confed_core::error::{ConfedError, ExitCode, Result};
use confed_core::instance::{Instance, Member};
use confed_core::session::{AuthMethod, Session, SessionStore};
use futures::StreamExt;
use serde_json::{json, Value};
use std::fmt::Write;
use std::sync::Arc;

/// What [`prepare`] gathered before the runtime started.
pub enum Prepared {
    /// Nothing: the command is offline.
    Offline,
    /// A credential for the whole instance.
    Connection(Connection),
    /// `instance init`: what `init` would gather.
    Init(crate::commands::init::InitInput),
    /// `instance adopt`, with the credential to share when asked to.
    Adopt(Option<Connection>),
}

/// The instance's base URL, flavor and credential, read once.
#[derive(Clone)]
pub struct Connection {
    pub base_url: String,
    pub flavor: Flavor,
    pub auth_method: AuthMethod,
    pub username: Option<String>,
    pub secret: Secret,
    /// Where the credential came from, for messages.
    pub source: String,
    /// The session it was read from, when it was read from one.
    pub session: Option<Session>,
    /// Members that keep a credential of their own, which — below a flag or
    /// `CONFED_TOKEN` — wins over the root's, as it does when the workspace
    /// is used alone. An `Err` is a credential that could not be read.
    pub members: std::collections::BTreeMap<String, std::result::Result<Connection, String>>,
}

/// The clients a fan-out uses: the instance's, and one per member with a
/// credential of its own, all drawing on one request ceiling.
pub struct Clients {
    pub shared: Arc<dyn ConfluenceClient>,
    own: std::collections::BTreeMap<String, std::result::Result<Arc<dyn ConfluenceClient>, String>>,
}

impl Clients {
    fn for_member(&self, key: &str) -> Result<Arc<dyn ConfluenceClient>> {
        match self.own.get(key) {
            Some(Ok(client)) => Ok(client.clone()),
            Some(Err(problem)) => Err(ConfedError::Auth(format!(
                "the credential in this workspace's .session.db could not be read: {problem}"
            ))),
            None => Ok(self.shared.clone()),
        }
    }
}

impl Connection {
    pub fn auth(&self) -> Auth {
        match self.auth_method {
            AuthMethod::Pat => Auth::Bearer(self.secret.clone()),
            AuthMethod::ApiToken | AuthMethod::Basic => Auth::Basic {
                user: self.username.clone().unwrap_or_default(),
                secret: self.secret.clone(),
            },
        }
    }

    pub fn client(&self, ctx: &Context) -> Result<Arc<dyn ConfluenceClient>> {
        ctx.client_for(self.flavor, &self.base_url, self.auth())
    }

    /// Every client a fan-out needs, sharing one request ceiling.
    pub fn clients(&self, ctx: &Context) -> Result<Clients> {
        let shared = self.client(ctx)?;
        let limit = Arc::new(tokio::sync::Semaphore::new(
            match ctx.concurrency() {
                0 => shared.capabilities().max_request_concurrency,
                n => n,
            }
            .max(1),
        ));
        let shared =
            ctx.client_with_limit(self.flavor, &self.base_url, self.auth(), limit.clone())?;
        let mut own = std::collections::BTreeMap::new();
        for (key, member) in &self.members {
            let client = match member {
                Ok(c) => {
                    Ok(ctx.client_with_limit(c.flavor, &c.base_url, c.auth(), limit.clone())?)
                }
                Err(problem) => Err(problem.clone()),
            };
            own.insert(key.clone(), client);
        }
        Ok(Clients { shared, own })
    }
}

/// The synchronous half: prompts, keyring reads.
pub fn prepare(ctx: &mut Context, command: &InstanceCommand) -> Result<Prepared> {
    use InstanceCommand as I;
    Ok(match command {
        I::Init(args) => Prepared::Init(setup::prepare_init(ctx, args)?),
        I::Adopt(args) => Prepared::Adopt(setup::prepare_adopt(ctx, args)?),
        I::Spaces(args) if !args.remote => Prepared::Offline,
        I::Status(args) if !args.fetch => Prepared::Offline,
        I::Add(_) | I::Remove(_) => Prepared::Offline,
        I::Doctor(args) if args.offline => Prepared::Offline,
        _ => Prepared::Connection(connection(ctx, &ctx.instance()?)?),
    })
}

pub async fn run(
    ctx: &mut Context,
    command: InstanceCommand,
    prepared: Prepared,
) -> Result<Output> {
    use InstanceCommand as I;
    let action = command.name();
    let connection = match prepared {
        Prepared::Init(input) => {
            let I::Init(args) = &command else { unreachable!("prepared for init") };
            return tag(setup::init(ctx, args, input).await?, action);
        }
        Prepared::Adopt(connection) => {
            let I::Adopt(args) = &command else { unreachable!("prepared for adopt") };
            return tag(setup::adopt(ctx, args, connection).await?, action);
        }
        Prepared::Connection(connection) => Some(connection),
        Prepared::Offline => None,
    };
    let output = match &command {
        I::Init(_) | I::Adopt(_) => unreachable!("handled above"),
        I::Spaces(args) => setup::spaces(ctx, args, connection).await?,
        I::Clone(args) => setup::clone(ctx, args, need(connection)?).await?,
        I::Add(args) => setup::add(ctx, args)?,
        I::Remove(args) => setup::remove(ctx, args)?,
        I::Status(args) => fanout::status(ctx, args, connection).await?,
        I::Fetch(args) => fanout::fetch(ctx, args, need(connection)?).await?,
        I::Pull(args) => fanout::pull(ctx, args, need(connection)?).await?,
        I::Push(args) => fanout::push(ctx, args, need(connection)?).await?,
        I::Doctor(args) => fanout::doctor(ctx, args, connection).await?,
        I::Search(args) => fanout::search(ctx, args, need(connection)?).await?,
        I::Mv(args) => mv::run(ctx, args, need(connection)?).await?,
        I::Resume(args) => mv::resume(ctx, args, need(connection)?).await?,
        I::Abort(args) => mv::abort(ctx, args, need(connection)?).await?,
    };
    tag(output, action)
}

fn need(connection: Option<Connection>) -> Result<Connection> {
    connection.ok_or_else(|| ConfedError::Other("this command needs a connection".into()))
}

/// Name the subcommand in the result, beside the envelope's `instance`.
fn tag(mut output: Output, action: &str) -> Result<Output> {
    if let Value::Object(map) = &mut output.result {
        map.insert("action".into(), json!(action));
    }
    Ok(output)
}

/// Read the instance's credential: a flag or `CONFED_TOKEN` first, then the
/// root's shared session, then — for a root whose workspaces still keep their
/// own — the first of those, then a prompt.
pub fn connection(ctx: &Context, instance: &Instance) -> Result<Connection> {
    let base_url = instance.base_url()?.ok_or_else(|| {
        ConfedError::state("this instance root records no base URL; run `confed instance init`")
    })?;
    let flavor = match &ctx.global.flavor {
        Some(flag) => flag.parse().map_err(ConfedError::usage)?,
        None => instance.flavor()?.unwrap_or(Flavor::DataCenter),
    };

    let mut source = String::new();
    let mut found: Option<(SessionStore, Session)> = None;
    if SessionStore::instance_exists(instance.root()) {
        let store = SessionStore::open_instance(instance.root())?;
        if let Some(session) = store.load()? {
            source = confed_core::session::INSTANCE_SESSION_DB_FILENAME.to_string();
            found = Some((store, session));
        }
    }
    if found.is_none() {
        for member in instance.enabled_members()? {
            let dir = instance.member_dir(&member);
            if !SessionStore::exists(&dir) {
                continue;
            }
            let store = SessionStore::open(&dir)?;
            if let Some(session) = store.load()? {
                source = format!("{}/.session.db", member.dir);
                found = Some((store, session));
                break;
            }
        }
    }

    let flagged = ctx.resolver.lookup(&config::TOKEN, ctx.global.token.as_deref());
    let secret = match (&flagged, &found) {
        (Some(resolved), _) => {
            source = resolved.source.as_str().to_string();
            Secret::new(resolved.value.clone())
        }
        (None, Some((store, session))) => match store.load_secret(session)? {
            Some(secret) => secret,
            None => prompt_secret(ctx)?,
        },
        (None, None) => prompt_secret(ctx)?,
    };
    let session = found.map(|(_, session)| session);
    let username = ctx
        .resolver
        .lookup(&config::USERNAME, ctx.global.username.as_deref())
        .map(|r| r.value)
        .or_else(|| session.as_ref().and_then(|s| s.username.clone()));
    let auth_method = match (&session, flavor, &username) {
        (Some(session), _, _) => session.auth_method,
        (None, Flavor::Cloud, _) => AuthMethod::ApiToken,
        (None, Flavor::DataCenter, Some(_)) => AuthMethod::Basic,
        (None, Flavor::DataCenter, None) => AuthMethod::Pat,
    };

    // A workspace's own credential wins over the root's, below a flag.
    let mut members = std::collections::BTreeMap::new();
    if flagged.is_none() {
        for member in instance.enabled_members()? {
            let dir = instance.member_dir(&member);
            if !SessionStore::exists(&dir) {
                continue;
            }
            let own = (|| -> Result<Option<Connection>> {
                let store = SessionStore::open(&dir)?;
                let Some(session) = store.load()? else { return Ok(None) };
                let secret = store
                    .load_secret(&session)?
                    .ok_or_else(|| ConfedError::Auth("nothing is stored".into()))?;
                Ok(Some(Connection {
                    base_url: session.base_url.clone(),
                    flavor: session.flavor,
                    auth_method: session.auth_method,
                    username: session.username.clone(),
                    secret,
                    source: format!("{}/.session.db", member.dir),
                    session: Some(session),
                    members: Default::default(),
                }))
            })();
            match own {
                Ok(Some(connection)) => {
                    members.insert(member.space_key.clone(), Ok(connection));
                }
                Ok(None) => {}
                Err(e) => {
                    members.insert(member.space_key.clone(), Err(e.to_string()));
                }
            }
        }
    }
    Ok(Connection { base_url, flavor, auth_method, username, secret, source, session, members })
}

fn prompt_secret(ctx: &Context) -> Result<Secret> {
    if ctx.is_interactive() {
        let value = crate::prompt::read_secret("API token: ")?;
        if !value.is_empty() {
            return Ok(Secret::new(value));
        }
    }
    Err(ConfedError::usage_with_hint(
        "no credential for this instance",
        "pass --token or set CONFED_TOKEN, or run `confed instance adopt --move-credentials` \
         (or `confed instance init`) to store one in the root",
    ))
}

/// The registered, enabled spaces a command visits: all of them, or those
/// named with `--space`.
pub fn selected(ctx: &Context, instance: &Instance) -> Result<Vec<Member>> {
    let members = instance.enabled_members()?;
    if ctx.global.space.is_empty() {
        return Ok(members);
    }
    let mut out = Vec::new();
    for key in &ctx.global.space {
        match instance.member(key)? {
            Some(member) => {
                if !out.iter().any(|m: &Member| m.space_key == member.space_key) {
                    out.push(member);
                }
            }
            None => {
                return Err(ConfedError::usage_with_hint(
                    format!("space {key} is not registered in this instance"),
                    format!(
                        "registered: {}; `confed instance clone {key}` adds it",
                        members.iter().map(|m| m.space_key.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                ))
            }
        }
    }
    Ok(out)
}

/// What one space's run of a command came to.
pub struct SpaceRun {
    pub member: Member,
    pub outcome: Result<Output>,
}

/// Run a single-space command in each member, `parallel` at a time, each in
/// its own context over the shared client. Results come back in key order.
pub async fn each_space<F, Fut>(
    ctx: &Context,
    instance: &Instance,
    members: &[Member],
    clients: Option<&Clients>,
    run: F,
) -> Vec<SpaceRun>
where
    F: Fn(Context) -> Fut,
    Fut: std::future::Future<Output = Result<Output>>,
{
    let parallel = parallelism(ctx, clients.map(|c| c.shared.as_ref()), members.len());
    let run = &run;
    let mut runs: Vec<SpaceRun> = futures::stream::iter(members.iter().cloned())
        .map(|member| async move {
            let outcome = async {
                let client = clients.map(|c| c.for_member(&member.space_key)).transpose()?;
                let ws = instance.open_workspace(&member)?;
                let child = Context::for_member(ctx, ws, client)?;
                run(child).await
            }
            .await;
            SpaceRun { member, outcome }
        })
        .buffer_unordered(parallel)
        .collect()
        .await;
    runs.sort_by(|a, b| a.member.space_key.cmp(&b.member.space_key));
    runs
}

/// How many spaces run at once. The client's own ceiling still caps the
/// requests in flight across all of them, so this only bounds the local work.
fn parallelism(ctx: &Context, client: Option<&dyn ConfluenceClient>, spaces: usize) -> usize {
    let cap = match ctx.concurrency() {
        0 => client.map(|c| c.capabilities().max_request_concurrency).unwrap_or(4),
        n => n,
    };
    cap.clamp(1, spaces.max(1))
}

/// Exit code of a fan-out: 0 when every space succeeded, the shared code
/// when every one failed the same way, 8 otherwise.
pub fn overall_exit(codes: &[ExitCode]) -> ExitCode {
    let Some(first) = codes.first() else { return ExitCode::Ok };
    if codes.iter().all(|c| c == first) {
        return *first;
    }
    if codes.iter().all(|c| matches!(c, ExitCode::Ok | ExitCode::Differences)) {
        return ExitCode::Differences;
    }
    ExitCode::Partial
}

/// The fan-out envelope: one entry per space, the single-space result as it
/// is, every failure also in the envelope's `errors`.
pub fn report(ctx: &Context, runs: Vec<SpaceRun>, extra: Value) -> Output {
    let style = &ctx.style;
    let mut human = String::new();
    let mut spaces = Vec::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut codes = Vec::new();

    for run in runs {
        let key = run.member.space_key.clone();
        match run.outcome {
            Ok(output) => {
                let ok = matches!(output.exit, ExitCode::Ok | ExitCode::Differences);
                codes.push(output.exit);
                let mut entry = json!({
                    "space_key": key,
                    "dir": run.member.dir,
                    "ok": ok,
                    "exit_code": output.exit.as_i32(),
                    "result": output.result,
                });
                let mut space_errors: Vec<ErrorEntry> = output.errors.clone();
                if !ok && space_errors.is_empty() {
                    space_errors.push(ErrorEntry {
                        code: output.exit.name(),
                        message: format!("finished with exit code {}", output.exit.as_i32()),
                        hint: None,
                    });
                }
                if !space_errors.is_empty() {
                    entry["errors"] = json!(space_errors);
                    errors.extend(space_errors.into_iter().map(|e| e.in_space(&key)));
                }
                if !output.warnings.is_empty() {
                    entry["warnings"] = json!(output.warnings);
                    warnings.extend(output.warnings.iter().map(|w| format!("{key}: {w}")));
                }
                spaces.push(entry);
                let mark = if ok { style.bold(&key) } else { style.red(&key) };
                let _ = writeln!(human, "{mark} {}", style.dim(&format!("({})", run.member.dir)));
                for line in output.human.trim_end().lines() {
                    let _ = writeln!(human, "  {line}");
                }
                let _ = writeln!(human);
            }
            Err(e) => {
                codes.push(e.exit_code());
                let entry = ErrorEntry::from_error(&e);
                spaces.push(json!({
                    "space_key": key,
                    "dir": run.member.dir,
                    "ok": false,
                    "exit_code": e.exit_code().as_i32(),
                    "errors": [entry.clone()],
                }));
                let _ = writeln!(
                    human,
                    "{} {} {e}\n",
                    style.red(&key),
                    style.dim(&format!("({})", run.member.dir))
                );
                errors.push(entry.in_space(&key));
            }
        }
    }
    if spaces.is_empty() {
        let _ =
            writeln!(human, "No spaces are registered; `confed instance clone <KEY>` adds one.");
    }

    let mut result = extra;
    result["spaces"] = json!(spaces);
    let mut output = Output::new(result, human).with_errors(errors).warn_all(warnings);
    output.exit = overall_exit(&codes);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overall_exit_code_follows_the_contract() {
        use ExitCode::*;
        assert_eq!(overall_exit(&[]), Ok);
        assert_eq!(overall_exit(&[Ok, Ok]), Ok);
        assert_eq!(overall_exit(&[Auth, Auth]), Auth, "all failed the same way");
        assert_eq!(overall_exit(&[Ok, Auth]), Partial);
        assert_eq!(overall_exit(&[Auth, Conflict]), Partial);
        assert_eq!(overall_exit(&[Ok, Differences]), Differences);
    }
}
