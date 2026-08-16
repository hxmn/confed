//! Everything a command needs: resolved configuration, the workspace, and a
//! client.
//!
//! Credentials are resolved here, synchronously, before the async runtime
//! starts — the OS keyring is a blocking API and must not be called from inside
//! the runtime.

use crate::cli::GlobalArgs;
use crate::output::Style;
use crate::prompt::TtyPrompter;
use confed_api::{Auth, CloudClient, ConfluenceClient, DcClient, Flavor, Secret};
use confed_core::config::{self, ConfigResolver, NoPrompt, Resolved};
use confed_core::error::{ConfedError, Result};
use confed_core::session::{AuthMethod, Session, SessionStore};
use confed_core::sync::SyncEngine;
use confed_core::workspace::Workspace;
use confed_api::SpaceId;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Context {
    pub global: GlobalArgs,
    pub cwd: PathBuf,
    pub resolver: ConfigResolver,
    pub style: Style,
    workspace: Option<Workspace>,
    /// Resolved once, before the async runtime starts: the OS keyring is a
    /// blocking API and must not be called from inside it.
    secret: Option<Secret>,
}

impl Context {
    /// Build without requiring an initialized workspace.
    pub fn build(global: GlobalArgs) -> Result<Self> {
        let cwd = match &global.directory {
            Some(dir) => dir.clone(),
            None => std::env::current_dir()
                .map_err(|e| ConfedError::io("resolving the current directory", e))?,
        };

        let workspace = Workspace::discover(&cwd).ok();
        let interactive = global.interactive();
        let mut resolver = if interactive {
            ConfigResolver::from_env(true, Box::new(TtyPrompter))
        } else {
            ConfigResolver::from_env(false, Box::new(NoPrompt))
        };

        if let Some(ws) = &workspace {
            resolver = resolver.with_stored(ws.stored_config()?);
        }

        let style = Style::detect(global.json);
        Ok(Self { global, cwd, resolver, style, workspace, secret: None })
    }

    /// The workspace, or the "run confed init" error.
    pub fn workspace(&self) -> Result<&Workspace> {
        self.workspace.as_ref().ok_or_else(ConfedError::not_initialized)
    }

    pub fn workspace_mut(&mut self) -> Result<&mut Workspace> {
        self.workspace.as_mut().ok_or_else(ConfedError::not_initialized)
    }

    pub fn has_workspace(&self) -> bool {
        self.workspace.is_some()
    }

    pub fn set_workspace(&mut self, ws: Workspace) {
        self.workspace = Some(ws);
    }

    pub fn is_interactive(&self) -> bool {
        self.resolver.is_interactive()
    }

    /// Resolve the base URL through the full precedence chain.
    pub fn base_url(&self) -> Result<Resolved<String>> {
        self.resolver.require(&config::BASE_URL, self.global.base_url.as_deref())
    }

    #[allow(dead_code)]
    pub fn space_key(&self) -> Result<Resolved<String>> {
        self.resolver.require(&config::SPACE, self.global.space.as_deref())
    }

    pub fn concurrency(&self) -> usize {
        self.global
            .concurrency
            .or_else(|| {
                self.resolver
                    .lookup(&config::CONCURRENCY, None)
                    .and_then(|r| r.value.parse().ok())
            })
            .unwrap_or(0)
    }

    /// The stored session, if this directory has been initialized.
    pub fn session(&self) -> Result<Option<Session>> {
        let Some(ws) = &self.workspace else { return Ok(None) };
        if !SessionStore::exists(ws.root()) {
            return Ok(None);
        }
        ws.session()
    }

    /// Read credentials into memory. Call this from `main` before entering the
    /// runtime, and only for commands that actually talk to the server — an
    /// offline `status` should never wake the OS keyring.
    pub fn preload_credentials(&mut self) -> Result<()> {
        let session = self.session()?;
        self.secret = Some(self.resolve_secret(session.as_ref())?);
        Ok(())
    }

    /// Provide the secret directly, for `init` (which prompts for it itself).
    #[allow(dead_code)]
    pub fn set_secret(&mut self, secret: Secret) {
        self.secret = Some(secret);
    }

    /// Work out flavor, credentials and base URL, then build the right client.
    ///
    /// [`preload_credentials`](Self::preload_credentials) must have run first.
    pub fn build_client(&self) -> Result<Arc<dyn ConfluenceClient>> {
        let session = self.session()?;

        let base_url = match self.resolver.lookup(&config::BASE_URL, self.global.base_url.as_deref())
        {
            Some(resolved) => resolved.value,
            None => session
                .as_ref()
                .map(|s| s.base_url.clone())
                .ok_or_else(|| {
                    ConfedError::missing_value(
                        "Confluence base URL",
                        "--base-url",
                        "CONFED_BASE_URL",
                    )
                })?,
        };

        let flavor = self.resolve_flavor(&base_url, session.as_ref())?;
        let auth_method = session
            .as_ref()
            .map(|s| s.auth_method)
            .unwrap_or_else(|| AuthMethod::default_for(flavor));

        let username = self
            .resolver
            .lookup(&config::USERNAME, self.global.username.as_deref())
            .map(|r| r.value)
            .or_else(|| session.as_ref().and_then(|s| s.username.clone()));

        let secret = match &self.secret {
            Some(secret) => secret.clone(),
            None => self.resolve_secret(session.as_ref())?,
        };

        let auth = match auth_method {
            AuthMethod::Pat => Auth::Bearer(secret),
            AuthMethod::ApiToken | AuthMethod::Basic => Auth::Basic {
                user: username.clone().ok_or_else(|| {
                    ConfedError::missing_value(
                        "username or email (required for this auth method)",
                        "--user",
                        "CONFED_USERNAME",
                    )
                })?,
                secret,
            },
        };

        self.client_for(flavor, &base_url, auth)
    }

    /// Build a client from explicit values, used by `init` before a session exists.
    pub fn client_for(
        &self,
        flavor: Flavor,
        base_url: &str,
        auth: Auth,
    ) -> Result<Arc<dyn ConfluenceClient>> {
        let concurrency = self.concurrency();
        Ok(match flavor {
            Flavor::Cloud => Arc::new(CloudClient::new(base_url, auth, concurrency)?),
            Flavor::DataCenter => Arc::new(DcClient::new(base_url, auth, concurrency)?),
        })
    }

    fn resolve_flavor(&self, base_url: &str, session: Option<&Session>) -> Result<Flavor> {
        if let Some(flag) = &self.global.flavor {
            return flag.parse().map_err(ConfedError::usage);
        }
        if let Some(resolved) = self.resolver.lookup(&config::FLAVOR, None) {
            if let Ok(flavor) = resolved.value.parse() {
                return Ok(flavor);
            }
        }
        if let Some(session) = session {
            return Ok(session.flavor);
        }
        confed_api::guess_flavor(base_url).ok_or_else(|| {
            ConfedError::usage_with_hint(
                format!("could not tell whether {base_url} is Confluence Cloud or Data Center"),
                "pass --flavor cloud or --flavor dc (or run `confed init`, which probes the server)",
            )
        })
    }

    /// Token precedence: flag, `CONFED_TOKEN`, stored session (keyring or DB).
    fn resolve_secret(&self, session: Option<&Session>) -> Result<Secret> {
        if let Some(resolved) = self.resolver.lookup(&config::TOKEN, self.global.token.as_deref()) {
            return Ok(Secret::new(resolved.value));
        }
        if let (Some(session), Some(ws)) = (session, &self.workspace) {
            if let Some(secret) = ws.session_store()?.load_secret(session)? {
                return Ok(secret);
            }
        }
        if self.is_interactive() {
            let value = crate::prompt::read_secret("API token: ")?;
            if !value.is_empty() {
                return Ok(Secret::new(value));
            }
        }
        Err(ConfedError::missing_value("API token", "--token", "CONFED_TOKEN"))
    }

    /// A sync engine bound to this workspace's space.
    pub fn engine(&self, client: Arc<dyn ConfluenceClient>) -> Result<SyncEngine> {
        let ws = self.workspace()?;
        let space = SpaceId {
            key: ws.space_key()?,
            numeric: ws.space_numeric_id()?,
        };
        Ok(SyncEngine::new(client, space, self.concurrency()))
    }

    /// Resolve a user-supplied page reference (path or id) to a page id.
    pub fn resolve_page(&self, reference: &str) -> Result<String> {
        let ws = self.workspace()?;
        let normalized = reference.trim_start_matches("./").replace('\\', "/");

        for candidate in [normalized.clone(), format!("{normalized}.md")] {
            if let Some(record) = ws.state().get_page_by_path(&candidate)? {
                return Ok(record.page_id);
            }
        }
        if ws.state().get_page(&normalized)?.is_some() {
            return Ok(normalized);
        }
        // A page that exists on the server but has not been materialized yet.
        if ws.state().get_remote(&normalized)?.is_some() {
            return Ok(normalized);
        }
        // Fall back to the file itself, which knows its own id.
        let path = ws.absolute(&normalized);
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| ConfedError::io(format!("reading {normalized}"), e))?;
            if let Ok(file) = confed_core::frontmatter::parse(&content, &normalized) {
                if let Some(id) = file.frontmatter.page_id() {
                    return Ok(id.to_string());
                }
            }
        }
        Err(ConfedError::NotFound(format!("no page matching `{reference}`")))
    }
}
