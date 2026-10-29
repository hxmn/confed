//! `confed init` — authenticate, bind the directory to a space, create state.
//!
//! Split in two: [`prepare`] runs synchronously (it may prompt and it reads the
//! OS keyring), then [`run`] does the network work inside the runtime.

use crate::cli::InitArgs;
use crate::context::Context;
use crate::output::Output;
use crate::prompt;
use confed_api::{Auth, ConfluenceClient, Flavor, Secret, User};
use confed_core::config::{self, Source};
use confed_core::error::{ConfedError, Result};
use confed_core::session::{AuthMethod, SecretBackend, Session, SessionStore};
use confed_core::state::now;
use confed_core::workspace::{self, Workspace};
use serde_json::json;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Credentials gathered before the runtime starts.
#[derive(Clone)]
pub struct InitInput {
    pub base_url: String,
    pub username: Option<String>,
    pub secret: Secret,
    pub flavor_hint: Option<Flavor>,
    pub space: Option<String>,
    pub credential_store: Option<SecretBackend>,
}

/// What `clone` knows before credentials are resolved.
#[derive(Default)]
pub struct Known {
    /// The base URL out of a space URL given as the source.
    pub base_url: Option<String>,
    pub space: Option<String>,
    /// The workspace `clone --from` borrows from.
    pub from: Option<Borrowed>,
}

/// An existing workspace whose instance and credential a new one reuses. Its
/// settings stand where a workspace's own stored ones would: below flags and
/// the environment, above prompting.
pub struct Borrowed {
    pub root: PathBuf,
    pub base_url: String,
    pub flavor: Flavor,
    pub session: Session,
}

impl Borrowed {
    /// Read `.state.db` and `.session.db` under `path`. The secret itself is
    /// read later, and only if no flag or variable supplies one.
    pub fn load(path: &Path) -> Result<Self> {
        let ws = Workspace::discover(path).map_err(|_| {
            ConfedError::usage_with_hint(
                format!("{} is not a confed workspace", path.display()),
                "point --from at a folder made by `confed init` or `confed clone`",
            )
        })?;
        let root = ws.root().to_path_buf();
        // Opening the store would create it, in somebody else's workspace.
        let session = match SessionStore::exists(&root) {
            true => ws.session()?,
            false => None,
        }
        .ok_or_else(|| {
            ConfedError::usage_with_hint(
                format!("{} has no stored credential", root.display()),
                "run `confed init` there first, or pass --token",
            )
        })?;
        let base_url = ws.base_url()?.unwrap_or_else(|| session.base_url.clone());
        let flavor = ws.flavor()?.unwrap_or(session.flavor);
        Ok(Self { root, base_url, flavor, session })
    }

    /// The stored secret. `None` means it could not be read and the caller may
    /// prompt instead; without a terminal that is an error with a way out.
    fn secret(&self, interactive: bool) -> Result<Option<Secret>> {
        let root = self.root.display();
        // A keyring read can raise a password dialog nobody is there to answer.
        if self.session.secret_backend == SecretBackend::Keyring && !interactive {
            return Err(ConfedError::usage_with_hint(
                format!("the credential of {root} is in the OS keyring, and reading it may prompt"),
                format!(
                    "pass --token or set CONFED_TOKEN, or run `confed -C {root} config --no-keychain` \
                     so it can be read without a prompt"
                ),
            ));
        }
        let problem = match SessionStore::open(&self.root)?.load_secret(&self.session) {
            Ok(Some(secret)) if !secret.is_empty() => return Ok(Some(secret)),
            Ok(_) => "nothing is stored".to_string(),
            Err(e) => e.to_string(),
        };
        if interactive {
            eprintln!("warning: could not read the credential of {root} ({problem})");
            return Ok(None);
        }
        Err(ConfedError::usage_with_hint(
            format!("could not read the credential of {root}: {problem}"),
            "pass --token or set CONFED_TOKEN",
        ))
    }
}

pub fn prepare(ctx: &mut Context, args: &InitArgs) -> Result<InitInput> {
    prepare_with(ctx, args, Known::default())
}

pub fn prepare_with(ctx: &mut Context, args: &InitArgs, known: Known) -> Result<InitInput> {
    let from = known.from.as_ref();

    // A stored base URL is this directory's own setting, which --from overrides.
    let explicit_base = ctx
        .resolver
        .lookup(&config::BASE_URL, ctx.global.base_url.as_deref())
        .filter(|r| matches!(r.source, Source::Flag | Source::Env))
        .map(|r| r.value);
    if let Some(from) = from {
        // The borrowed token must not be sent anywhere but where it came from.
        for asked in known.base_url.iter().chain(explicit_base.iter()) {
            if !same_instance(asked, &from.base_url) {
                return Err(ConfedError::usage_with_hint(
                    format!("{} belongs to {}, not {asked}", from.root.display(), from.base_url),
                    "drop --base-url to clone from that instance, or leave out --from \
                     and pass --token for this one",
                ));
            }
        }
    }
    let base_url = match (from, known.base_url.or(explicit_base)) {
        (Some(from), _) => from.base_url.clone(),
        (None, Some(url)) => url,
        (None, None) => ctx.base_url()?.value,
    }
    .trim_end_matches('/')
    .to_string();

    let flag_flavor: Option<Flavor> = match &ctx.global.flavor {
        Some(flag) => Some(flag.parse().map_err(ConfedError::usage)?),
        None => None,
    };
    if let (Some(flag), Some(from)) = (flag_flavor, from) {
        if flag != from.flavor {
            return Err(ConfedError::usage_with_hint(
                format!(
                    "{} is a Confluence {} workspace, not {flag}",
                    from.root.display(),
                    from.flavor
                ),
                "drop --flavor, or leave out --from",
            ));
        }
    }
    let flavor_hint =
        flag_flavor.or(from.map(|f| f.flavor)).or_else(|| confed_api::guess_flavor(&base_url));

    // Cloud always needs an email alongside the API token; DC uses a bearer PAT.
    let username = match ctx.resolver.lookup(&config::USERNAME, ctx.global.username.as_deref()) {
        Some(resolved) => Some(resolved.value),
        None => match from.and_then(|f| f.session.username.clone()) {
            Some(username) => Some(username),
            None if flavor_hint == Some(Flavor::Cloud) => {
                Some(ctx.resolver.require(&config::USERNAME, None)?.value)
            }
            None => None,
        },
    };

    let secret = match ctx.resolver.lookup(&config::TOKEN, ctx.global.token.as_deref()) {
        Some(resolved) => Some(Secret::new(resolved.value)),
        None => match from {
            Some(from) => from.secret(ctx.is_interactive())?,
            None => None,
        },
    };
    let secret = match secret {
        Some(secret) => secret,
        None if ctx.is_interactive() => Secret::new(prompt::read_secret(&format!(
            "{} token: ",
            flavor_hint.map(|f| f.to_string()).unwrap_or_else(|| "API".into())
        ))?),
        None => {
            return Err(ConfedError::missing_value("API token", "--token", "CONFED_TOKEN"));
        }
    };
    if secret.is_empty() {
        return Err(ConfedError::Auth("no token was provided".into()));
    }

    // A borrowed credential is kept the way its workspace keeps it.
    let credential_store = args
        .credential_store
        .as_deref()
        .map(|s| match s {
            "keyring" => SecretBackend::Keyring,
            _ => SecretBackend::Sqlite,
        })
        .or(from.map(|f| f.session.secret_backend));

    Ok(InitInput {
        base_url,
        username,
        secret,
        flavor_hint,
        space: known.space.or_else(|| {
            ctx.resolver.lookup(&config::SPACE, ctx.global.space_flag()).map(|r| r.value)
        }),
        credential_store,
    })
}

/// Whether two base URLs name the same Confluence: scheme, host, port and
/// path, ignoring case in the host and a trailing slash.
pub fn same_instance(a: &str, b: &str) -> bool {
    confed_core::instance::same_instance(a, b)
}

pub async fn run(ctx: &mut Context, args: &InitArgs, input: InitInput) -> Result<Output> {
    let dir = ctx.cwd.clone();
    let outcome = initialize(ctx, args, input, &dir).await?;
    Ok(outcome)
}

/// Shared by `init` and `clone`.
pub async fn initialize(
    ctx: &mut Context,
    args: &InitArgs,
    input: InitInput,
    dir: &std::path::Path,
) -> Result<Output> {
    // Refuse to silently re-bind a directory to a different space.
    if let Ok(existing) = Workspace::open(dir) {
        if let Ok(bound) = existing.space_key() {
            let requested = input.space.as_deref();
            if requested.is_some_and(|s| s != bound) && !args.force {
                return Err(ConfedError::state_with_hint(
                    format!("this directory is already bound to space {bound}"),
                    "use --force to re-bind it, or run `confed init` in an empty directory",
                ));
            }
        }
    }

    // Verify before storing anything: a bad token should not leave state behind.
    let Connected { flavor, auth_method, client, user } = connect(ctx, &input).await?;

    let space_key = resolve_space(ctx, &client, input.space.clone()).await?;
    let space = client.get_space(&space_key).await?;

    let ws = Workspace::create(dir)?;
    let state = ws.state();
    state.set_meta("base_url", &input.base_url)?;
    state.set_meta("flavor", flavor.as_str())?;
    state.set_meta("space_key", &space.id.key)?;
    if let Some(numeric) = &space.id.numeric {
        state.set_meta("space_id", numeric)?;
    }
    state.set_meta("space_name", &space.name)?;

    let session = Session {
        base_url: input.base_url.clone(),
        flavor,
        auth_method,
        username: input.username.clone(),
        secret_backend: SecretBackend::Keyring,
        created_at: now(),
        last_verified_at: Some(now()),
    };
    let store = SessionStore::open(dir)?;
    let backend = store.save(&session, &input.secret, input.credential_store)?;

    let gitignore = workspace::ensure_gitignore(dir)?;
    let agent_docs = if args.no_agent_docs {
        Vec::new()
    } else {
        // A directory being re-initialized may already name a rules page.
        let rules = crate::commands::agent_docs::current_rules(&ws).unwrap_or(None);
        crate::commands::agent_docs::write(
            dir,
            &input.base_url,
            flavor,
            &space.id.key,
            rules.as_ref(),
        )?
    };

    let mut created = vec![".state.db".to_string(), ".session.db".to_string()];
    created.extend(agent_docs.iter().cloned());

    let style = &ctx.style;
    let mut human = String::new();
    let _ = writeln!(
        human,
        "Authenticated as {} on Confluence {}",
        style.bold(&user.display_name),
        flavor
    );
    let _ = writeln!(
        human,
        "Bound this directory to space {} ({})",
        style.bold(&space.id.key),
        space.name
    );
    let _ = writeln!(
        human,
        "Credentials stored in {}",
        match backend {
            SecretBackend::Keyring => "the OS keyring",
            SecretBackend::Sqlite => ".session.db (mode 0600)",
        }
    );
    if !gitignore.is_empty() {
        let _ = writeln!(human, "Added to .gitignore: {}", gitignore.join(", "));
    }
    if !agent_docs.is_empty() {
        let _ = writeln!(human, "Wrote {}", agent_docs.join(" and "));
    }
    let _ = writeln!(human, "\n{}", style.dim("Next: `confed pull` to download the space."));

    let result = json!({
        "base_url": input.base_url,
        "flavor": flavor.as_str(),
        "user": {
            "account_id": user.account_id,
            "display_name": user.display_name,
            "email": user.email,
        },
        "space": { "key": space.id.key, "id": space.id.numeric, "name": space.name },
        "credential_store": backend.as_str(),
        "created": created,
        "gitignore_added": gitignore,
    });

    let mut output = Output::new(result, human);
    if backend == SecretBackend::Sqlite && input.credential_store.is_none() {
        output = output.warn(
            "no OS keyring was available, so the token is stored in .session.db (mode 0600); \
             prefer CONFED_TOKEN in CI",
        );
    }
    ctx.set_workspace(ws);
    Ok(output)
}

/// A client the server has accepted.
pub struct Connected {
    pub flavor: Flavor,
    pub auth_method: AuthMethod,
    pub client: Arc<dyn ConfluenceClient>,
    pub user: User,
}

/// Work out the flavor and the auth method, and check the credential with a
/// `whoami`. A rejected one is exit 3, before anything is written.
pub async fn connect(ctx: &Context, input: &InitInput) -> Result<Connected> {
    let flavor = match input.flavor_hint {
        Some(flavor) => flavor,
        None => {
            let auth = build_auth(input, AuthMethod::Pat);
            confed_api::detect_flavor(&input.base_url, auth).await?
        }
    };

    let auth_method = match (flavor, &input.username) {
        (Flavor::Cloud, _) => AuthMethod::ApiToken,
        (Flavor::DataCenter, None) => AuthMethod::Pat,
        (Flavor::DataCenter, Some(_)) => AuthMethod::Basic,
    };
    let client = ctx.client_for(flavor, &input.base_url, build_auth(input, auth_method))?;

    let user = client.whoami().await.map_err(|e| match e {
        confed_api::ApiError::Auth(msg) => {
            ConfedError::Auth(format!("could not authenticate against {}: {msg}", input.base_url))
        }
        other => other.into(),
    })?;
    Ok(Connected { flavor, auth_method, client, user })
}

fn build_auth(input: &InitInput, method: AuthMethod) -> Auth {
    match method {
        AuthMethod::Pat => Auth::Bearer(input.secret.clone()),
        AuthMethod::ApiToken | AuthMethod::Basic => Auth::Basic {
            user: input.username.clone().unwrap_or_default(),
            secret: input.secret.clone(),
        },
    }
}

/// Take the requested space, or let the user pick one interactively.
async fn resolve_space(
    ctx: &Context,
    client: &Arc<dyn ConfluenceClient>,
    requested: Option<String>,
) -> Result<String> {
    if let Some(key) = requested {
        return Ok(key);
    }
    if !ctx.is_interactive() {
        return Err(ConfedError::missing_value("space key", "--space", "CONFED_SPACE"));
    }

    let spaces = client.list_spaces(Some(200)).await?;
    if spaces.is_empty() {
        return Err(ConfedError::NotFound("no spaces are visible to this account".into()));
    }
    let labels: Vec<String> =
        spaces.iter().map(|s| format!("{:<12} {}", s.id.key, s.name)).collect();
    let choice = prompt::select("Which space should this directory track?", &labels)?;
    Ok(spaces[choice].id.key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_instance_is_recognised_however_it_is_spelled() {
        assert!(same_instance("https://wiki.example.test", "https://wiki.example.test/"));
        assert!(same_instance(
            "https://Wiki.Example.Test/confluence",
            "https://wiki.example.test/confluence/"
        ));
        assert!(same_instance("https://wiki.example.test:443", "https://wiki.example.test"));

        assert!(!same_instance("https://wiki.example.test", "http://wiki.example.test"));
        assert!(!same_instance("https://wiki.example.test", "https://wiki.corp"));
        assert!(!same_instance(
            "https://wiki.example.test",
            "https://wiki.example.test/confluence"
        ));
        assert!(!same_instance("http://127.0.0.1:8090", "http://127.0.0.1:8091"));
    }

    /// Without a terminal nobody can answer a keychain dialog, so a credential
    /// in the keyring is not even read: the error says how to get past it.
    #[test]
    fn a_keyring_credential_is_not_read_without_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let borrowed = Borrowed {
            root: dir.path().to_path_buf(),
            base_url: "https://wiki.example.test".into(),
            flavor: Flavor::DataCenter,
            session: Session {
                base_url: "https://wiki.example.test".into(),
                flavor: Flavor::DataCenter,
                auth_method: AuthMethod::Pat,
                username: None,
                secret_backend: SecretBackend::Keyring,
                created_at: now(),
                last_verified_at: None,
            },
        };
        let err = borrowed.secret(false).unwrap_err();
        assert_eq!(err.exit_code(), confed_core::ExitCode::Usage);
        let hint = err.hint().unwrap();
        assert!(hint.contains("--token"), "{hint}");
        assert!(hint.contains("config --no-keychain"), "{hint}");
        assert!(!dir.path().join(".session.db").exists(), "nothing was opened");
    }

    #[test]
    fn a_source_that_is_not_a_workspace_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = Borrowed::load(dir.path()).err().unwrap();
        assert_eq!(err.exit_code(), confed_core::ExitCode::Usage);

        // A workspace with no credential is refused without creating one.
        Workspace::create(dir.path()).unwrap();
        let err = Borrowed::load(dir.path()).err().unwrap();
        assert!(err.to_string().contains("no stored credential"), "{err}");
        assert!(!SessionStore::exists(dir.path()));
    }
}
