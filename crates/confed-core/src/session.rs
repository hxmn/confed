//! `.session.db` — how confed remembers who you are.
//!
//! The token itself goes to the OS keyring when one is available; the SQLite
//! file then holds only a pointer to it. Where there is no keyring (headless
//! Linux, CI containers) confed falls back to storing the token in the DB, which
//! is created `0600` and refused if its permissions are looser. `doctor` warns
//! about the fallback.
//!
//! These functions are synchronous and call into the platform keyring, so they
//! must run *before* the async runtime is entered.

use crate::error::{ConfedError, Result};
use confed_api::{Auth, Flavor, Secret};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub const SESSION_DB_FILENAME: &str = ".session.db";
const KEYRING_SERVICE: &str = "confed";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS session (
  id               INTEGER PRIMARY KEY CHECK (id = 1),
  base_url         TEXT NOT NULL,
  flavor           TEXT NOT NULL CHECK (flavor IN ('cloud','datacenter')),
  auth_method      TEXT NOT NULL CHECK (auth_method IN ('api_token','pat','basic')),
  username         TEXT,
  secret_backend   TEXT NOT NULL CHECK (secret_backend IN ('keyring','sqlite')),
  secret_value     TEXT,
  created_at       TEXT NOT NULL,
  last_verified_at TEXT
);
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    /// Cloud: email + API token (HTTP Basic).
    ApiToken,
    /// Data Center: Personal Access Token (Bearer).
    Pat,
    /// Username + password (HTTP Basic).
    Basic,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::ApiToken => "api_token",
            AuthMethod::Pat => "pat",
            AuthMethod::Basic => "basic",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "api_token" => Ok(AuthMethod::ApiToken),
            "pat" => Ok(AuthMethod::Pat),
            "basic" => Ok(AuthMethod::Basic),
            other => Err(ConfedError::Other(format!("unknown auth method `{other}`"))),
        }
    }

    /// What a flavor uses by default when the user does not say.
    pub fn default_for(flavor: Flavor) -> Self {
        match flavor {
            Flavor::Cloud => AuthMethod::ApiToken,
            Flavor::DataCenter => AuthMethod::Pat,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretBackend {
    Keyring,
    Sqlite,
}

impl SecretBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretBackend::Keyring => "keyring",
            SecretBackend::Sqlite => "sqlite",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub base_url: String,
    pub flavor: Flavor,
    pub auth_method: AuthMethod,
    pub username: Option<String>,
    pub secret_backend: SecretBackend,
    pub created_at: String,
    pub last_verified_at: Option<String>,
}

impl Session {
    /// Build the HTTP auth for this session, given the resolved secret.
    pub fn auth_with(&self, secret: Secret) -> Auth {
        match self.auth_method {
            AuthMethod::Pat => Auth::Bearer(secret),
            AuthMethod::ApiToken | AuthMethod::Basic => {
                Auth::Basic { user: self.username.clone().unwrap_or_default(), secret }
            }
        }
    }
}

#[derive(Debug)]
pub struct SessionStore {
    conn: Connection,
    path: PathBuf,
}

impl SessionStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(SESSION_DB_FILENAME);
        let existed = path.exists();
        if existed {
            check_permissions(&path)?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| ConfedError::Other(format!("could not open {}: {e}", path.display())))?;
        conn.execute_batch(SCHEMA)?;
        if !existed {
            restrict_permissions(&path)?;
        }
        Ok(Self { conn, path })
    }

    pub fn exists(dir: &Path) -> bool {
        dir.join(SESSION_DB_FILENAME).exists()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT base_url, flavor, auth_method, username, secret_backend,
                        created_at, last_verified_at
                 FROM session WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?
            .map(|(base_url, flavor, method, username, backend, created, verified)| {
                Ok::<_, ConfedError>(Session {
                    base_url,
                    flavor: flavor.parse().map_err(ConfedError::Other)?,
                    auth_method: AuthMethod::parse(&method)?,
                    username,
                    secret_backend: if backend == "keyring" {
                        SecretBackend::Keyring
                    } else {
                        SecretBackend::Sqlite
                    },
                    created_at: created,
                    last_verified_at: verified,
                })
            })
            .transpose()
    }

    /// Persist credentials, preferring the OS keyring.
    ///
    /// `force_backend` lets `--credential-store` override the preference; when
    /// the keyring is unavailable confed falls back to SQLite and says so in the
    /// returned backend.
    pub fn save(
        &self,
        session: &Session,
        secret: &Secret,
        force_backend: Option<SecretBackend>,
    ) -> Result<SecretBackend> {
        let account = keyring_account(&session.base_url, session.username.as_deref());
        let backend = match force_backend {
            Some(SecretBackend::Sqlite) => SecretBackend::Sqlite,
            Some(SecretBackend::Keyring) => {
                keyring_set(&account, secret).map_err(|e| {
                    ConfedError::Other(format!(
                        "--credential-store keyring was requested but the keyring is unavailable: {e}"
                    ))
                })?;
                SecretBackend::Keyring
            }
            None => match keyring_set(&account, secret) {
                Ok(()) => SecretBackend::Keyring,
                Err(e) => {
                    tracing::debug!(target: "confed::session", error = %e, "keyring unavailable, storing in .session.db");
                    SecretBackend::Sqlite
                }
            },
        };

        let stored_secret = match backend {
            SecretBackend::Keyring => None,
            SecretBackend::Sqlite => Some(secret.expose().to_string()),
        };

        self.conn.execute(
            "INSERT INTO session
                (id, base_url, flavor, auth_method, username, secret_backend, secret_value,
                 created_at, last_verified_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                base_url = excluded.base_url, flavor = excluded.flavor,
                auth_method = excluded.auth_method, username = excluded.username,
                secret_backend = excluded.secret_backend, secret_value = excluded.secret_value,
                last_verified_at = excluded.last_verified_at",
            params![
                session.base_url,
                session.flavor.as_str(),
                session.auth_method.as_str(),
                session.username,
                backend.as_str(),
                stored_secret,
                session.created_at,
                session.last_verified_at,
            ],
        )?;
        restrict_permissions(&self.path)?;
        Ok(backend)
    }

    /// Move the stored credential to another backend.
    ///
    /// Reading it first may prompt once, if it is in the keyring and the
    /// platform asks; after moving it to SQLite nothing prompts again. The old
    /// copy is removed, so the credential never lives in two places.
    pub fn switch_backend(
        &self,
        session: &Session,
        target: SecretBackend,
        fallback_secret: Option<Secret>,
    ) -> Result<Session> {
        if session.secret_backend == target {
            return Ok(session.clone());
        }

        let secret = match self.load_secret(session) {
            Ok(Some(secret)) => secret,
            other => fallback_secret.ok_or_else(|| {
                ConfedError::Auth(format!(
                    "the stored credential could not be read ({}), so there is nothing to move;                      pass --token or set CONFED_TOKEN to supply it",
                    match other {
                        Err(e) => e.to_string(),
                        _ => "nothing was stored".to_string(),
                    }
                ))
            })?,
        };

        let mut moved = session.clone();
        moved.secret_backend = target;
        let actual = self.save(&moved, &secret, Some(target))?;
        moved.secret_backend = actual;

        // Leave nothing behind in the backend we moved away from.
        match (session.secret_backend, actual) {
            (SecretBackend::Keyring, SecretBackend::Sqlite) => {
                let account = keyring_account(&session.base_url, session.username.as_deref());
                let _ = keyring_delete(&account);
            }
            (SecretBackend::Sqlite, SecretBackend::Keyring) => {
                self.conn.execute("UPDATE session SET secret_value = NULL WHERE id = 1", [])?;
            }
            _ => {}
        }
        Ok(moved)
    }

    /// Fetch the stored secret. Returns `None` when nothing is stored.
    pub fn load_secret(&self, session: &Session) -> Result<Option<Secret>> {
        match session.secret_backend {
            SecretBackend::Sqlite => Ok(self
                .conn
                .query_row("SELECT secret_value FROM session WHERE id = 1", [], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .optional()?
                .flatten()
                .map(Secret::new)),
            SecretBackend::Keyring => {
                let account = keyring_account(&session.base_url, session.username.as_deref());
                match keyring_get(&account) {
                    Ok(secret) => Ok(Some(secret)),
                    Err(e) => Err(ConfedError::Auth(format!(
                        "credentials are stored in the OS keyring but could not be read ({e}); \
                         set CONFED_TOKEN or re-run `confed init`"
                    ))),
                }
            }
        }
    }

    pub fn mark_verified(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET last_verified_at = ?1 WHERE id = 1",
            params![crate::state::now()],
        )?;
        Ok(())
    }

    /// Forget stored credentials (both backends).
    pub fn clear(&self, session: &Session) -> Result<()> {
        if session.secret_backend == SecretBackend::Keyring {
            let account = keyring_account(&session.base_url, session.username.as_deref());
            let _ = keyring_delete(&account);
        }
        self.conn.execute("DELETE FROM session WHERE id = 1", [])?;
        Ok(())
    }
}

fn keyring_account(base_url: &str, username: Option<&str>) -> String {
    format!("{base_url}|{}", username.unwrap_or(""))
}

/// Run a keyring call on a plain OS thread.
///
/// The Secret Service backend drives its own async runtime and blocks on it,
/// which panics outright if it happens on a thread already running one. confed
/// stores credentials from inside `init`, which is async, so every keyring call
/// goes through here rather than relying on callers to remember.
fn off_runtime<T, F>(call: F) -> std::result::Result<T, keyring::Error>
where
    T: Send + 'static,
    F: FnOnce() -> std::result::Result<T, keyring::Error> + Send + 'static,
{
    match std::thread::spawn(call).join() {
        Ok(result) => result,
        Err(_) => Err(keyring::Error::PlatformFailure(Box::new(std::io::Error::other(
            "the platform keyring panicked",
        )))),
    }
}

fn keyring_set(account: &str, secret: &Secret) -> std::result::Result<(), keyring::Error> {
    let (account, secret) = (account.to_string(), secret.expose().to_string());
    off_runtime(move || keyring::Entry::new(KEYRING_SERVICE, &account)?.set_password(&secret))
}

fn keyring_get(account: &str) -> std::result::Result<Secret, keyring::Error> {
    let account = account.to_string();
    let password =
        off_runtime(move || keyring::Entry::new(KEYRING_SERVICE, &account)?.get_password())?;
    Ok(Secret::new(password))
}

fn keyring_delete(account: &str) -> std::result::Result<(), keyring::Error> {
    let account = account.to_string();
    off_runtime(move || keyring::Entry::new(KEYRING_SERVICE, &account)?.delete_credential())
}

/// True when a working credential store is present.
pub fn keyring_available() -> bool {
    let probe = keyring_account("confed://probe", Some("probe"));
    !matches!(
        off_runtime(move || keyring::Entry::new(KEYRING_SERVICE, &probe)?.get_password()),
        Err(keyring::Error::PlatformFailure(_)) | Err(keyring::Error::Invalid(_, _))
    )
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .map_err(|e| ConfedError::io(format!("setting 0600 on {}", path.display()), e))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Refuse to use a credential file other users can read.
#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| ConfedError::io(format!("reading {}", path.display()), e))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(ConfedError::state_with_hint(
            format!("{} is readable by other users (mode {:o})", path.display(), mode),
            format!("run `chmod 600 {}`", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(backend: SecretBackend) -> Session {
        Session {
            base_url: "https://wiki.example.com".into(),
            flavor: Flavor::DataCenter,
            auth_method: AuthMethod::Pat,
            username: None,
            secret_backend: backend,
            created_at: crate::state::now(),
            last_verified_at: None,
        }
    }

    #[test]
    fn sqlite_fallback_round_trips_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let mut session = sample(SecretBackend::Sqlite);

        let backend =
            store.save(&session, &Secret::new("pat-123"), Some(SecretBackend::Sqlite)).unwrap();
        assert_eq!(backend, SecretBackend::Sqlite);
        session.secret_backend = backend;

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.base_url, "https://wiki.example.com");
        assert_eq!(loaded.flavor, Flavor::DataCenter);
        assert_eq!(loaded.auth_method, AuthMethod::Pat);
        assert_eq!(store.load_secret(&session).unwrap().unwrap().expose(), "pat-123");
    }

    #[test]
    #[cfg(unix)]
    fn the_session_file_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        store
            .save(&sample(SecretBackend::Sqlite), &Secret::new("x"), Some(SecretBackend::Sqlite))
            .unwrap();
        let mode = std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "found mode {mode:o}");
    }

    #[test]
    #[cfg(unix)]
    fn a_world_readable_session_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        {
            let _ = SessionStore::open(dir.path()).unwrap();
        }
        let path = dir.path().join(SESSION_DB_FILENAME);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = SessionStore::open(dir.path()).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::State);
        assert!(err.hint().unwrap().contains("chmod 600"));
    }

    #[test]
    fn auth_shape_follows_the_method() {
        let pat = sample(SecretBackend::Sqlite).auth_with(Secret::new("tok"));
        assert!(matches!(pat, Auth::Bearer(_)));

        let mut cloud = sample(SecretBackend::Sqlite);
        cloud.auth_method = AuthMethod::ApiToken;
        cloud.username = Some("me@example.com".into());
        match cloud.auth_with(Secret::new("tok")) {
            Auth::Basic { user, .. } => assert_eq!(user, "me@example.com"),
            other => panic!("expected basic auth, got {other:?}"),
        }
    }

    #[test]
    fn default_auth_method_per_flavor() {
        assert_eq!(AuthMethod::default_for(Flavor::Cloud), AuthMethod::ApiToken);
        assert_eq!(AuthMethod::default_for(Flavor::DataCenter), AuthMethod::Pat);
    }

    #[test]
    fn clearing_removes_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let session = sample(SecretBackend::Sqlite);
        store.save(&session, &Secret::new("x"), Some(SecretBackend::Sqlite)).unwrap();
        store.clear(&session).unwrap();
        assert!(store.load().unwrap().is_none());
    }
}

#[cfg(test)]
mod backend_switch_tests {
    use super::*;

    fn store(dir: &std::path::Path) -> (SessionStore, Session) {
        let store = SessionStore::open(dir).unwrap();
        let session = Session {
            base_url: "https://wiki.example.com".into(),
            flavor: Flavor::DataCenter,
            auth_method: AuthMethod::Pat,
            username: None,
            secret_backend: SecretBackend::Sqlite,
            created_at: crate::state::now(),
            last_verified_at: None,
        };
        store.save(&session, &Secret::new("pat-123"), Some(SecretBackend::Sqlite)).unwrap();
        (store, session)
    }

    #[test]
    fn switching_to_the_backend_already_in_use_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let (store, session) = store(dir.path());

        let moved = store.switch_backend(&session, SecretBackend::Sqlite, None).unwrap();
        assert_eq!(moved.secret_backend, SecretBackend::Sqlite);
        assert_eq!(store.load_secret(&moved).unwrap().unwrap().expose(), "pat-123");
    }

    #[test]
    fn a_credential_that_cannot_be_read_can_still_be_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        // A session claiming the keyring holds it, with nothing actually there.
        let session = Session {
            base_url: "https://wiki.example.com".into(),
            flavor: Flavor::DataCenter,
            auth_method: AuthMethod::Pat,
            username: Some("nobody-has-this-account".into()),
            secret_backend: SecretBackend::Keyring,
            created_at: crate::state::now(),
            last_verified_at: None,
        };

        let err = store.switch_backend(&session, SecretBackend::Sqlite, None).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::Auth);
        assert!(err.to_string().contains("CONFED_TOKEN"), "the message says how to fix it");

        let moved = store
            .switch_backend(&session, SecretBackend::Sqlite, Some(Secret::new("supplied")))
            .unwrap();
        assert_eq!(moved.secret_backend, SecretBackend::Sqlite);
        assert_eq!(store.load_secret(&moved).unwrap().unwrap().expose(), "supplied");
    }

    #[test]
    fn moving_to_sqlite_makes_the_secret_readable_without_a_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut session) = store(dir.path());
        session.secret_backend = SecretBackend::Keyring;

        // Whether or not this machine has a keyring, the fallback path lands the
        // credential in the database, where reading it never prompts.
        let moved = store
            .switch_backend(&session, SecretBackend::Sqlite, Some(Secret::new("pat-123")))
            .unwrap();
        assert_eq!(moved.secret_backend, SecretBackend::Sqlite);
        assert_eq!(store.load().unwrap().unwrap().secret_backend, SecretBackend::Sqlite);
        assert_eq!(store.load_secret(&moved).unwrap().unwrap().expose(), "pat-123");
    }
}

#[cfg(test)]
mod runtime_safety_tests {
    use super::*;

    /// The Secret Service backend blocks on its own runtime, which panics if it
    /// happens on a thread already driving one. `confed init` stores credentials
    /// from inside async code, so this must hold wherever a keyring exists.
    #[tokio::test]
    async fn keyring_calls_are_safe_from_inside_the_async_runtime() {
        // Any outcome is fine; a panic is not.
        let _ = keyring_available();

        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(dir.path()).unwrap();
        let session = Session {
            base_url: "https://wiki.example.com".into(),
            flavor: Flavor::DataCenter,
            auth_method: AuthMethod::Pat,
            username: Some("confed-test".into()),
            secret_backend: SecretBackend::Sqlite,
            created_at: crate::state::now(),
            last_verified_at: None,
        };

        // The keyring path is the one that used to panic; the fallback to
        // SQLite is what happens on a machine without one.
        let backend = store.save(&session, &Secret::new("token"), None).unwrap();
        let mut stored = session.clone();
        stored.secret_backend = backend;
        assert_eq!(store.load_secret(&stored).unwrap().unwrap().expose(), "token");

        store.clear(&stored).unwrap();
    }
}
