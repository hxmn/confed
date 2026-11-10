//! API-layer errors. `confed-core` maps these onto process exit codes.

use thiserror::Error;

pub type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Debug, Error)]
pub enum ApiError {
    /// 401/403: bad or insufficient credentials.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Transport failure, TLS problem, timeout, or retries exhausted.
    /// `message` already names the causes, for whoever prints only it;
    /// `source` keeps the error itself, for whoever walks the chain.
    #[error("network error: {message}")]
    Network {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    /// 404 for a page/space/attachment/comment.
    #[error("not found: {0}")]
    NotFound(String),

    /// 409 or a stale version: someone else changed the page first.
    #[error("version conflict: {0}")]
    Conflict(String),

    /// The connected flavor has no API for this operation.
    #[error("unsupported on {flavor}: {operation}")]
    Unsupported { flavor: &'static str, operation: String },

    /// Retries exhausted against 429.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// The server understood the request and refused it as stated — e.g. an
    /// inline comment whose text selection does not match the page (412).
    #[error("{0}")]
    Rejected(String),

    /// The server refused to create a space, for a reason confed names.
    #[error("{message}")]
    SpaceRefused { reason: crate::types::SpaceRefusal, message: String },

    /// Any other non-success status.
    #[error("server returned {status}: {body}")]
    Server { status: u16, body: String },

    /// The response did not look like what the endpoint promised.
    #[error("could not parse response from {context}: {source}")]
    Decode {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid URL: {0}")]
    Url(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl ApiError {
    /// Classify a non-success HTTP response.
    pub fn from_status(status: u16, body: String, context: &str) -> Self {
        match status {
            401 | 403 => ApiError::Auth(format!("{context}: HTTP {status}: {}", truncate(&body))),
            404 => ApiError::NotFound(format!("{context}: {}", truncate(&body))),
            // Confluence answers some writes to deleted content with a 400 that
            // says so, rather than a 404.
            400 if body.contains("content.doesnt.exist") => {
                ApiError::NotFound(format!("{context}: {}", truncate(&body)))
            }
            // A stale version, in the server's own words; judged from the body
            // as it came, before it is prefixed and cut short for the message.
            409 | 400 if is_stale_version(status, &body) => {
                ApiError::Conflict(format!("{context}: {}", truncate(&body)))
            }
            // A gateway that still cannot reach Confluence after the retries:
            // the network, from where confed stands, not a server answer.
            502..=504 => ApiError::network(format!(
                "{context}: HTTP {status} from a gateway in front of Confluence"
            )),
            429 => ApiError::RateLimited(format!("{context}: {}", truncate(&body))),
            _ => ApiError::Server { status, body: format!("{context}: {}", truncate(&body)) },
        }
    }

    /// A network failure with nothing beneath it to keep.
    pub fn network(message: impl Into<String>) -> Self {
        ApiError::Network { message: message.into(), source: None }
    }

    pub fn unsupported(flavor: crate::types::Flavor, operation: impl Into<String>) -> Self {
        ApiError::Unsupported { flavor: flavor.as_str(), operation: operation.into() }
    }

    /// True when the failure is worth surfacing as "try again later".
    pub fn is_transient(&self) -> bool {
        matches!(self, ApiError::Network { .. } | ApiError::RateLimited(_))
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest's own message is only the outermost layer ("error sending
        // request for url …"); why — a refused certificate, a reset, a name
        // that does not resolve — is further down the chain.
        let detail = with_causes(&e);
        let message = if e.is_timeout() {
            format!("request timed out: {detail}")
        } else if e.is_connect() {
            format!("could not connect: {detail}")
        } else {
            detail
        };
        ApiError::Network { message, source: Some(Box::new(e)) }
    }
}

/// Whether a refused write says the version it was based on is stale — the
/// one refusal a pull and a retry fix. A 409 always does. Some servers send
/// a 400 instead, and then it is the server's own messages, read from its
/// JSON error body, that say so: a 400 that merely mentions a version (a
/// macro's, the API's) is some other refusal, and calling it a conflict
/// would send the user to pull for nothing.
pub fn is_stale_version(status: u16, body: &str) -> bool {
    match status {
        409 => true,
        400 => error_messages(body).iter().any(|m| names_stale_version(m)),
        _ => false,
    }
}

fn names_stale_version(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("version must be incremented")
        || m.contains("current version")
        || (m.contains("version") && (m.contains("stale") || m.contains("conflict")))
}

/// The human-readable messages in a Confluence error body: `message` on Data
/// Center, `errors[].title` and `detail` on Cloud, `message.translation`
/// inside `data.errors[]` on both. A body that is not JSON is its own message.
fn error_messages(body: &str) -> Vec<String> {
    fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    match (key.as_str(), value) {
                        (
                            "message" | "title" | "detail" | "translation",
                            serde_json::Value::String(s),
                        ) => out.push(s.clone()),
                        _ => walk(value, out),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => {
            let mut out = Vec::new();
            walk(&value, &mut out);
            out
        }
        Err(_) => vec![body.to_string()],
    }
}

/// An error's message followed by each cause beneath it that adds something.
pub fn with_causes(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        let message = c.to_string();
        if !message.is_empty() && !out.contains(&message) {
            out.push_str(": ");
            out.push_str(&message);
        }
        cause = c.source();
    }
    out
}

/// Whether a network failure was the server's certificate being refused.
pub fn is_certificate_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("certificate") || m.contains("unknownissuer") || m.contains("unknown issuer")
}

impl From<url::ParseError> for ApiError {
    fn from(e: url::ParseError) -> Self {
        ApiError::Url(e.to_string())
    }
}

fn truncate(body: &str) -> String {
    const MAX: usize = 400;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(the server sent no body)".to_string();
    }
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification() {
        assert!(matches!(ApiError::from_status(401, "no".into(), "whoami"), ApiError::Auth(_)));
        assert!(matches!(ApiError::from_status(404, "no".into(), "page"), ApiError::NotFound(_)));
        assert!(matches!(ApiError::from_status(409, "no".into(), "page"), ApiError::Conflict(_)));
        assert!(matches!(
            ApiError::from_status(429, "slow".into(), "page"),
            ApiError::RateLimited(_)
        ));
        assert!(matches!(
            ApiError::from_status(500, "boom".into(), "page"),
            ApiError::Server { status: 500, .. }
        ));
    }

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|l| l as _)
        }
    }

    #[test]
    fn a_network_error_keeps_its_causes() {
        let e = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "client error (Connect)",
                Some(Box::new(Layer("invalid peer certificate: UnknownIssuer", None))),
            ))),
        );
        let message = with_causes(&e);
        assert_eq!(
            message,
            "error sending request: client error (Connect): invalid peer certificate: UnknownIssuer"
        );
        assert!(is_certificate_error(&message));
        assert!(!is_certificate_error("could not connect: connection refused"));
    }

    /// What the clients see: the classification made from the body as the
    /// server sent it, not from the message built around it.
    #[test]
    fn a_stale_version_400_is_classified_as_a_conflict() {
        let stale = r#"{"statusCode":400,"message":"Version must be incremented when updating a page. Current Version is: 5. Version in request: 5"}"#;
        let err = ApiError::from_status(400, stale.into(), "PUT rest/api/content/1001");
        assert!(matches!(err, ApiError::Conflict(_)), "{err:?}");
        // The same words inside a body too long to keep whole.
        let long = format!(
            r#"{{"statusCode":400,"data":{{"padding":"{}"}},"message":"Version must be incremented"}}"#,
            "x".repeat(2000)
        );
        let err = ApiError::from_status(400, long, "PUT rest/api/content/1001");
        assert!(matches!(err, ApiError::Conflict(_)), "{err:?}");
        let other = r#"{"statusCode":400,"message":"Unsupported macro version for macro 'code'"}"#;
        let err = ApiError::from_status(400, other.into(), "PUT rest/api/content/1001");
        assert!(matches!(err, ApiError::Server { status: 400, .. }), "{err:?}");
        let err = ApiError::from_status(409, "anything".into(), "PUT rest/api/content/1001");
        assert!(matches!(err, ApiError::Conflict(_)), "{err:?}");
    }

    #[test]
    fn only_a_stale_version_is_a_conflict() {
        assert!(is_stale_version(409, "anything"));
        // Data Center's words for it.
        assert!(is_stale_version(
            400,
            r#"{"statusCode":400,"message":"Version must be incremented when updating a page. Current Version is: 5. Version in request: 5"}"#
        ));
        // Cloud's.
        assert!(is_stale_version(
            400,
            r#"{"errors":[{"status":400,"code":"INVALID_REQUEST_PARAMETER","title":"Version number must be greater than the current version"}]}"#
        ));
        // A 400 that mentions a version and is about something else.
        assert!(!is_stale_version(
            400,
            r#"{"statusCode":400,"message":"Unsupported macro version for macro 'code'"}"#
        ));
        assert!(!is_stale_version(400, r#"{"message":"API version 1 is not supported here"}"#));
        assert!(!is_stale_version(500, r#"{"message":"Version must be incremented"}"#));
    }

    #[test]
    fn long_bodies_are_truncated_on_char_boundaries() {
        let body = "é".repeat(500);
        let e = ApiError::from_status(500, body, "ctx");
        assert!(e.to_string().len() < 600);
    }
}
