//! API-layer errors. `confed-core` maps these onto process exit codes.

use thiserror::Error;

pub type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Debug, Error)]
pub enum ApiError {
    /// 401/403: bad or insufficient credentials.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Transport failure, TLS problem, timeout, or retries exhausted.
    #[error("network error: {0}")]
    Network(String),

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
            409 => ApiError::Conflict(format!("{context}: {}", truncate(&body))),
            429 => ApiError::RateLimited(format!("{context}: {}", truncate(&body))),
            _ => ApiError::Server { status, body: format!("{context}: {}", truncate(&body)) },
        }
    }

    pub fn unsupported(flavor: crate::types::Flavor, operation: impl Into<String>) -> Self {
        ApiError::Unsupported { flavor: flavor.as_str(), operation: operation.into() }
    }

    /// True when the failure is worth surfacing as "try again later".
    pub fn is_transient(&self) -> bool {
        matches!(self, ApiError::Network(_) | ApiError::RateLimited(_))
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            ApiError::Network(format!("request timed out: {e}"))
        } else if e.is_connect() {
            ApiError::Network(format!("could not connect: {e}"))
        } else {
            ApiError::Network(e.to_string())
        }
    }
}

impl From<url::ParseError> for ApiError {
    fn from(e: url::ParseError) -> Self {
        ApiError::Url(e.to_string())
    }
}

fn truncate(body: &str) -> String {
    const MAX: usize = 400;
    let trimmed = body.trim();
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

    #[test]
    fn long_bodies_are_truncated_on_char_boundaries() {
        let body = "é".repeat(500);
        let e = ApiError::from_status(500, body, "ctx");
        assert!(e.to_string().len() < 600);
    }
}
