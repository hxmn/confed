//! `confed init` — authenticate, bind the directory to a space, create state.
//!
//! Split in two: [`prepare`] runs synchronously (it may prompt and it reads the
//! OS keyring), then [`run`] does the network work inside the runtime.

use crate::cli::InitArgs;
use crate::context::Context;
use crate::output::Output;
use crate::prompt;
use confed_api::{Auth, ConfluenceClient, Flavor, Secret};
use confed_core::config;
use confed_core::error::{ConfedError, Result};
use confed_core::session::{AuthMethod, SecretBackend, Session, SessionStore};
use confed_core::state::now;
use confed_core::workspace::{self, Workspace};
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;

/// Credentials gathered before the runtime starts.
pub struct InitInput {
    pub base_url: String,
    pub username: Option<String>,
    pub secret: Secret,
    pub flavor_hint: Option<Flavor>,
    pub space: Option<String>,
    pub credential_store: Option<SecretBackend>,
}

pub fn prepare(ctx: &mut Context, args: &InitArgs) -> Result<InitInput> {
    let base_url = ctx.base_url()?.value.trim_end_matches('/').to_string();

    let flavor_hint = match &ctx.global.flavor {
        Some(flag) => Some(flag.parse().map_err(ConfedError::usage)?),
        None => confed_api::guess_flavor(&base_url),
    };

    // Cloud always needs an email alongside the API token; DC uses a bearer PAT.
    let username = match ctx.resolver.lookup(&config::USERNAME, ctx.global.username.as_deref()) {
        Some(resolved) => Some(resolved.value),
        None if flavor_hint == Some(Flavor::Cloud) => {
            Some(ctx.resolver.require(&config::USERNAME, None)?.value)
        }
        None => None,
    };

    let secret = match ctx.resolver.lookup(&config::TOKEN, ctx.global.token.as_deref()) {
        Some(resolved) => Secret::new(resolved.value),
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

    let credential_store = args.credential_store.as_deref().map(|s| match s {
        "keyring" => SecretBackend::Keyring,
        _ => SecretBackend::Sqlite,
    });

    Ok(InitInput {
        base_url,
        username,
        secret,
        flavor_hint,
        space: ctx.resolver.lookup(&config::SPACE, ctx.global.space.as_deref()).map(|r| r.value),
        credential_store,
    })
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

    let flavor = match input.flavor_hint {
        Some(flavor) => flavor,
        None => {
            let auth = build_auth(&input, AuthMethod::Pat);
            confed_api::detect_flavor(&input.base_url, auth).await?
        }
    };

    let auth_method = match (flavor, &input.username) {
        (Flavor::Cloud, _) => AuthMethod::ApiToken,
        (Flavor::DataCenter, None) => AuthMethod::Pat,
        (Flavor::DataCenter, Some(_)) => AuthMethod::Basic,
    };
    let client = ctx.client_for(flavor, &input.base_url, build_auth(&input, auth_method))?;

    // Verify before storing anything: a bad token should not leave state behind.
    let user = client.whoami().await.map_err(|e| match e {
        confed_api::ApiError::Auth(msg) => {
            ConfedError::Auth(format!("could not authenticate against {}: {msg}", input.base_url))
        }
        other => other.into(),
    })?;

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
        crate::commands::agent_docs::write(dir, &input.base_url, flavor, &space.id.key)?
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
