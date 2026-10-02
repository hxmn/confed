//! Everything a command needs: resolved configuration, the workspace, and a
//! client.
//!
//! Credentials are resolved here, synchronously, before the async runtime
//! starts — the OS keyring is a blocking API and must not be called from inside
//! the runtime.

use crate::cli::GlobalArgs;
use crate::output::Style;
use crate::prompt::TtyPrompter;
use confed_api::SpaceId;
use confed_api::{Auth, CloudClient, ConfluenceClient, DcClient, Flavor, Secret};
use confed_core::config::{self, ConfigResolver, NoPrompt, Resolved};
use confed_core::error::{ConfedError, Result};
use confed_core::session::{AuthMethod, Session, SessionStore};
use confed_core::sync::SyncEngine;
use confed_core::workspace::Workspace;
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

    /// Move the workspace out of the context.
    ///
    /// The TUI needs to own it: long operations run on the tokio runtime, and
    /// the workspace (a SQLite connection) travels with them.
    pub fn take_workspace(&mut self) -> Result<Workspace> {
        self.workspace.take().ok_or_else(ConfedError::not_initialized)
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
                self.resolver.lookup(&config::CONCURRENCY, None).and_then(|r| r.value.parse().ok())
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

        let base_url = match self
            .resolver
            .lookup(&config::BASE_URL, self.global.base_url.as_deref())
        {
            Some(resolved) => resolved.value,
            None => session.as_ref().map(|s| s.base_url.clone()).ok_or_else(|| {
                ConfedError::missing_value("Confluence base URL", "--base-url", "CONFED_BASE_URL")
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
        let space = SpaceId { key: ws.space_key()?, numeric: ws.space_numeric_id()? };
        Ok(SyncEngine::new(client, space, self.concurrency()).with_progress(self.progress()))
    }

    /// A progress reporter, or a silent one when nobody is watching.
    pub fn progress(&self) -> confed_core::progress::ProgressRef {
        if crate::progress::TerminalProgress::should_show(
            self.global.silent,
            self.global.quiet,
            self.global.json,
        ) {
            Arc::new(crate::progress::TerminalProgress::new())
        } else {
            confed_core::progress::none()
        }
    }

    /// Resolve a user-supplied page reference (path or id) to a page id.
    /// The current directory relative to the workspace root (`Handbook 1`),
    /// or empty at the root or outside it.
    fn cwd_prefix(&self) -> String {
        let Ok(ws) = self.workspace() else { return String::new() };
        let root = ws.root().canonicalize().unwrap_or_else(|_| ws.root().to_path_buf());
        let cwd = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
        cwd.strip_prefix(&root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    }

    /// A path argument as the workspace knows it. Like git, a path is relative
    /// to the current directory; one that names nothing there is tried from
    /// the workspace root, which is how confed always read it.
    pub fn workspace_path(&self, arg: &str) -> String {
        let arg = arg.replace('\\', "/");
        let Ok(ws) = self.workspace() else { return arg };
        // A page id is not a path.
        if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit()) {
            return arg;
        }
        if std::path::Path::new(&arg).is_absolute() {
            let root = ws.root().canonicalize().unwrap_or_else(|_| ws.root().to_path_buf());
            let abs = std::path::Path::new(&arg);
            let abs = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
            return abs
                .strip_prefix(&root)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or(arg);
        }
        let prefix = self.cwd_prefix();
        if prefix.is_empty() {
            return normalize_rel(&arg);
        }
        let joined = normalize_rel(&format!("{prefix}/{arg}"));
        let names_something = |p: &str| {
            !p.is_empty()
                && (ws.absolute(p).exists()
                    || ws.absolute(&format!("{p}.md")).exists()
                    || ws.state().get_page_by_path(p).ok().flatten().is_some()
                    || ws.state().get_page_by_path(&format!("{p}.md")).ok().flatten().is_some())
        };
        // A glob is a pattern, not a file: it is relative to here, like git's.
        if arg.contains('*') || names_something(&joined) || !names_something(&normalize_rel(&arg)) {
            joined
        } else {
            normalize_rel(&arg)
        }
    }

    /// [`Self::workspace_path`] over a list of scope arguments.
    pub fn workspace_paths(&self, args: &[String]) -> Vec<String> {
        args.iter().map(|a| self.workspace_path(a)).collect()
    }

    pub fn resolve_page(&self, reference: &str) -> Result<String> {
        let ws = self.workspace()?;
        let normalized = self.workspace_path(reference);

        for candidate in [normalized.clone(), format!("{normalized}.md")] {
            if let Some(record) = ws.state().get_page_by_path(&candidate)? {
                return Ok(record.page_id);
            }
        }
        for id in [reference.trim(), normalized.as_str()] {
            if ws.state().get_page(id)?.is_some() {
                return Ok(id.to_string());
            }
            // A page that exists on the server but has not been materialized yet.
            if ws.state().get_remote(id)?.is_some() {
                return Ok(id.to_string());
            }
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

        let prefix = self.cwd_prefix();
        let looked = if prefix.is_empty() {
            format!("`{normalized}` from the workspace root")
        } else {
            format!("`{normalized}` (from `{prefix}/`) and `{reference}` from the workspace root")
        };
        let hint = closest_page(ws, reference)
            .map(|p| format!("did you mean `{p}`? Paths are relative to the current directory, then the workspace root"))
            .unwrap_or_else(|| {
                "paths are relative to the current directory, then the workspace root; \
                 `confed status` lists every page"
                    .to_string()
            });
        Err(ConfedError::NotFound(format!(
            "no page matching `{reference}`: looked for {looked}; {hint}"
        )))
    }
}

/// `a/./b/../c` → `a/c`, without touching the filesystem.
fn normalize_rel(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The tracked page whose path is nearest to `reference`, for a "did you
/// mean": same file name first, then the smallest edit distance.
fn closest_page(ws: &confed_core::workspace::Workspace, reference: &str) -> Option<String> {
    let pages = ws.state().all_pages().ok()?;
    let wanted = reference.trim_end_matches(".md").to_lowercase();
    let name = wanted.rsplit('/').next().unwrap_or(&wanted).to_string();
    pages
        .iter()
        .map(|p| p.local_path.clone())
        .min_by_key(|path| {
            let lower = path.trim_end_matches(".md").to_lowercase();
            let file = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            (file != name, edit_distance(&file, &name))
        })
        .filter(|path| {
            let lower = path.trim_end_matches(".md").to_lowercase();
            let file = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            file == name || edit_distance(&file, &name) <= name.chars().count().max(3) / 3
        })
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn relative_paths_normalize() {
        assert_eq!(normalize_rel("Handbook 1/./Test.md"), "Handbook 1/Test.md");
        assert_eq!(normalize_rel("Handbook 1/../Other.md"), "Other.md");
        assert_eq!(normalize_rel("./Test.md"), "Test.md");
    }

    #[test]
    fn edit_distance_counts_edits() {
        assert_eq!(edit_distance("test", "test"), 0);
        assert_eq!(edit_distance("tset", "test"), 2);
        assert_eq!(edit_distance("фраза", "фразы"), 1);
    }
}
