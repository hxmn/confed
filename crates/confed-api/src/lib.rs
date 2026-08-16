//! Confluence API clients for confed.
//!
//! [`ConfluenceClient`] is the contract; [`cloud::CloudClient`] speaks REST v2 and
//! [`dc::DcClient`] speaks REST v1. Capability differences are reported through
//! [`types::Capabilities`] rather than by branching on the flavor.

pub mod client;
pub mod cloud;
pub mod dc;
pub mod error;
pub mod http;
pub mod mock;
pub mod paginate;
pub mod secret;
pub mod types;
pub mod wire;

pub use client::ConfluenceClient;
pub use cloud::CloudClient;
pub use dc::DcClient;
pub use error::{ApiError, ApiResult};
pub use http::{Auth, Http, RetryPolicy};
pub use mock::MockClient;
pub use secret::Secret;
pub use types::*;

/// Guess the flavor from the base URL. Callers should still confirm with a probe
/// (see [`detect_flavor`]) unless the user passed `--flavor`.
pub fn guess_flavor(base_url: &str) -> Option<Flavor> {
    let host = url::Url::parse(base_url).ok()?.host_str()?.to_ascii_lowercase();
    if host.ends_with(".atlassian.net") || host.ends_with(".jira.com") {
        Some(Flavor::Cloud)
    } else {
        None
    }
}

/// Probe the server to decide which API flavor it speaks.
///
/// Tries the URL heuristic first, then a cheap unauthenticated-ish probe of each
/// API root. Auth failures still identify the flavor: a 401 from `/rest/api/space`
/// means the endpoint exists.
pub async fn detect_flavor(base_url: &str, auth: Auth) -> ApiResult<Flavor> {
    if let Some(flavor) = guess_flavor(base_url) {
        return Ok(flavor);
    }
    let http = Http::new(base_url, auth, 2)?;
    // Cloud sites answer under /wiki; DC installs answer at the context root.
    for (path, flavor) in [
        ("wiki/api/v2/spaces?limit=1", Flavor::Cloud),
        ("api/v2/spaces?limit=1", Flavor::Cloud),
        ("rest/api/space?limit=1", Flavor::DataCenter),
    ] {
        match http.get_json::<serde_json::Value>(path, &[]).await {
            Ok(_) => return Ok(flavor),
            Err(ApiError::Auth(_)) => return Ok(flavor),
            Err(_) => continue,
        }
    }
    Err(ApiError::Network(format!(
        "could not determine whether {base_url} is Confluence Cloud or Data Center; pass --flavor"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_hosts_are_recognized_without_a_probe() {
        assert_eq!(guess_flavor("https://acme.atlassian.net/wiki"), Some(Flavor::Cloud));
        assert_eq!(guess_flavor("https://wiki.corp.example.com"), None);
        assert_eq!(guess_flavor("not a url"), None);
    }
}
