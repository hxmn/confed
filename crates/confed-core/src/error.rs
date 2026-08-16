//! Errors and the exit-code contract. Agents script against these numbers, so
//! the mapping is part of the public interface and is covered by tests.

use confed_api::ApiError;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, ConfedError>;

/// Process exit codes. Documented in docs/design/04-command-reference.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    Error = 1,
    Usage = 2,
    Auth = 3,
    Conflict = 4,
    Network = 5,
    NotFound = 6,
    State = 7,
    Partial = 8,
    Unsupported = 9,
    /// `--exit-code` and differences exist. Deliberately outside the error range.
    Differences = 10,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    pub fn name(self) -> &'static str {
        match self {
            ExitCode::Ok => "OK",
            ExitCode::Error => "ERROR",
            ExitCode::Usage => "USAGE",
            ExitCode::Auth => "AUTH",
            ExitCode::Conflict => "CONFLICT",
            ExitCode::Network => "NETWORK",
            ExitCode::NotFound => "NOT_FOUND",
            ExitCode::State => "STATE",
            ExitCode::Partial => "PARTIAL",
            ExitCode::Unsupported => "UNSUPPORTED",
            ExitCode::Differences => "DIFFERENCES",
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfedError {
    /// Bad or missing arguments, including a required value that could not be
    /// resolved in non-interactive mode.
    #[error("{message}")]
    Usage { message: String, hint: Option<String> },

    #[error("authentication failed: {0}")]
    Auth(String),

    /// Version conflict or unresolved merge conflict.
    #[error("{0}")]
    Conflict(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("not found: {0}")]
    NotFound(String),

    /// A local precondition failed: not initialized, dirty files in the way,
    /// tampered frontmatter, lock held, corrupt preserved block.
    #[error("{message}")]
    State { message: String, hint: Option<String> },

    /// Some operations succeeded and some failed.
    #[error("{0}")]
    Partial(String),

    #[error("{0}")]
    Unsupported(String),

    #[error("{0}")]
    Other(String),

    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error("conversion failed: {0}")]
    Convert(#[from] confed_convert::ConvertError),

    #[error("could not parse YAML frontmatter: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

impl ConfedError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            ConfedError::Usage { .. } => ExitCode::Usage,
            ConfedError::Auth(_) => ExitCode::Auth,
            ConfedError::Conflict(_) => ExitCode::Conflict,
            ConfedError::Network(_) => ExitCode::Network,
            ConfedError::NotFound(_) => ExitCode::NotFound,
            ConfedError::State { .. } => ExitCode::State,
            ConfedError::Partial(_) => ExitCode::Partial,
            ConfedError::Unsupported(_) => ExitCode::Unsupported,
            ConfedError::Other(_)
            | ConfedError::Sqlite(_)
            | ConfedError::Io { .. }
            | ConfedError::Convert(_)
            | ConfedError::Yaml(_)
            | ConfedError::Json(_) => ExitCode::Error,
        }
    }

    /// Stable machine-readable code for `--json` output.
    pub fn code(&self) -> &'static str {
        self.exit_code().name()
    }

    /// The actionable next step, when there is one.
    pub fn hint(&self) -> Option<&str> {
        match self {
            ConfedError::Usage { hint, .. } | ConfedError::State { hint, .. } => hint.as_deref(),
            ConfedError::Conflict(_) => Some("run `confed pull` to merge, then retry"),
            ConfedError::Auth(_) => Some("run `confed init` to refresh credentials, or check CONFED_TOKEN"),
            _ => None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        ConfedError::Usage { message: message.into(), hint: None }
    }

    pub fn usage_with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        ConfedError::Usage { message: message.into(), hint: Some(hint.into()) }
    }

    pub fn state(message: impl Into<String>) -> Self {
        ConfedError::State { message: message.into(), hint: None }
    }

    pub fn state_with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        ConfedError::State { message: message.into(), hint: Some(hint.into()) }
    }

    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        ConfedError::Io { context: context.into(), source }
    }

    /// A value that could not be resolved from flag, environment, or stored config.
    pub fn missing_value(what: &str, flag: &str, env: &str) -> Self {
        ConfedError::Usage {
            message: format!("missing {what}"),
            hint: Some(format!(
                "pass {flag}, set {env}, or run `confed init` in this directory \
                 (interactive prompts are disabled without a TTY)"
            )),
        }
    }

    pub fn not_initialized() -> Self {
        ConfedError::State {
            message: "this directory is not a confed workspace (.state.db not found)".into(),
            hint: Some("run `confed init` or `confed clone <space>` first".into()),
        }
    }
}

impl From<ApiError> for ConfedError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Auth(m) => ConfedError::Auth(m),
            ApiError::Network(m) => ConfedError::Network(m),
            ApiError::RateLimited(m) => ConfedError::Network(m),
            ApiError::NotFound(m) => ConfedError::NotFound(m),
            ApiError::Conflict(m) => ConfedError::Conflict(m),
            ApiError::Unsupported { flavor, operation } => ConfedError::Unsupported(format!(
                "{operation} is not available on Confluence {flavor}"
            )),
            ApiError::Server { status, body } => {
                ConfedError::Other(format!("server returned {status}: {body}"))
            }
            ApiError::Decode { context, source } => {
                ConfedError::Other(format!("unexpected response from {context}: {source}"))
            }
            ApiError::Url(m) => ConfedError::Usage { message: m, hint: None },
            ApiError::Io(e) => ConfedError::Io { context: "api".into(), source: e },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_documented_table() {
        assert_eq!(ExitCode::Ok.as_i32(), 0);
        assert_eq!(ExitCode::Error.as_i32(), 1);
        assert_eq!(ExitCode::Usage.as_i32(), 2);
        assert_eq!(ExitCode::Auth.as_i32(), 3);
        assert_eq!(ExitCode::Conflict.as_i32(), 4);
        assert_eq!(ExitCode::Network.as_i32(), 5);
        assert_eq!(ExitCode::NotFound.as_i32(), 6);
        assert_eq!(ExitCode::State.as_i32(), 7);
        assert_eq!(ExitCode::Partial.as_i32(), 8);
        assert_eq!(ExitCode::Unsupported.as_i32(), 9);
        assert_eq!(ExitCode::Differences.as_i32(), 10);
    }

    #[test]
    fn every_error_variant_maps_to_its_code() {
        let cases: Vec<(ConfedError, ExitCode)> = vec![
            (ConfedError::usage("x"), ExitCode::Usage),
            (ConfedError::Auth("x".into()), ExitCode::Auth),
            (ConfedError::Conflict("x".into()), ExitCode::Conflict),
            (ConfedError::Network("x".into()), ExitCode::Network),
            (ConfedError::NotFound("x".into()), ExitCode::NotFound),
            (ConfedError::state("x"), ExitCode::State),
            (ConfedError::Partial("x".into()), ExitCode::Partial),
            (ConfedError::Unsupported("x".into()), ExitCode::Unsupported),
            (ConfedError::Other("x".into()), ExitCode::Error),
        ];
        for (err, expected) in cases {
            assert_eq!(err.exit_code(), expected, "{err:?}");
        }
    }

    #[test]
    fn api_errors_carry_their_classification_across_the_boundary() {
        let err: ConfedError = ApiError::Conflict("stale".into()).into();
        assert_eq!(err.exit_code(), ExitCode::Conflict);
        let err: ConfedError =
            ApiError::unsupported(confed_api::Flavor::DataCenter, "resolve comment").into();
        assert_eq!(err.exit_code(), ExitCode::Unsupported);
        assert!(err.to_string().contains("datacenter"));
        // Rate limiting is a network condition from the user's point of view.
        let err: ConfedError = ApiError::RateLimited("429".into()).into();
        assert_eq!(err.exit_code(), ExitCode::Network);
    }

    #[test]
    fn missing_values_explain_all_three_resolution_paths() {
        let err = ConfedError::missing_value("space key", "--space", "CONFED_SPACE");
        let hint = err.hint().unwrap();
        assert!(hint.contains("--space"));
        assert!(hint.contains("CONFED_SPACE"));
        assert!(hint.contains("confed init"));
    }
}
