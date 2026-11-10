//! Serde DTOs for the Confluence wire formats, plus the small shared helpers both
//! flavors need (id coercion, URL re-anchoring, search-result cleanup).
//!
//! Nothing here is part of the public API: the flavor clients convert these into
//! [`crate::types`] before anything leaves the crate.

pub mod v1;

use crate::error::ApiResult;
use serde::Deserialize;
use std::fmt;
use url::Url;

/// Confluence is inconsistent about whether ids arrive as JSON strings or numbers
/// (v2 spaces, v1 space ids and `size` fields all differ). Accept both.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Id {
    Str(String),
    Num(i64),
}

impl Id {
    pub fn as_string(&self) -> String {
        match self {
            Id::Str(s) => s.clone(),
            Id::Num(n) => n.to_string(),
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Str(s) => f.write_str(s),
            Id::Num(n) => write!(f, "{n}"),
        }
    }
}

/// Resolve a server-supplied link against the API base URL.
///
/// Confluence hands out links in three shapes and we have to accept all of them:
///
/// * absolute (`https://acme.atlassian.net/wiki/...`) — used verbatim;
/// * rooted at the site and already carrying the context path
///   (`/wiki/api/v2/pages?cursor=…`) — resolved against the origin;
/// * rooted at the site but *missing* the context path
///   (`/download/attachments/1/f.png`) — re-anchored under the base path, which is
///   what Cloud attachment `downloadLink`s and v1 search `url`s need.
pub fn resolve_under_base(base: &Url, link: &str) -> ApiResult<Url> {
    if link.starts_with("http://") || link.starts_with("https://") {
        return Ok(Url::parse(link)?);
    }
    let joined = base.join(link)?;
    let prefix = base.path().trim_end_matches('/');
    if prefix.is_empty() || joined.path().starts_with(prefix) {
        return Ok(joined);
    }
    // The link dropped the context path (`/wiki`, `/confluence`, …); put it back.
    Ok(base.join(link.trim_start_matches('/'))?)
}

/// CQL search wraps matched terms in `@@@hl@@@…@@@endhl@@@`. Strip the markers so
/// titles and excerpts are readable.
pub fn strip_highlight_markers(s: &str) -> String {
    s.replace("@@@hl@@@", "").replace("@@@endhl@@@", "")
}

/// Pull a space key out of a search result's container URL: `/spaces/DOCS` on Cloud,
/// `/display/DOCS` on Data Center.
pub fn space_key_from_display_url(url: &str) -> Option<String> {
    let mut segments = url.split('/').filter(|s| !s.is_empty());
    let first = segments.next()?;
    if !matches!(first, "spaces" | "display") {
        return None;
    }
    segments.next().filter(|s| !s.is_empty()).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn absolute_links_pass_through() {
        let u = resolve_under_base(&base("https://acme.atlassian.net/wiki/"), "https://cdn.test/x")
            .unwrap();
        assert_eq!(u.as_str(), "https://cdn.test/x");
    }

    #[test]
    fn cursor_links_that_carry_the_context_path_resolve_against_the_origin() {
        let u = resolve_under_base(
            &base("https://acme.atlassian.net/wiki/"),
            "/wiki/api/v2/pages?cursor=abc",
        )
        .unwrap();
        assert_eq!(u.as_str(), "https://acme.atlassian.net/wiki/api/v2/pages?cursor=abc");
    }

    #[test]
    fn links_missing_the_context_path_are_re_anchored() {
        let u = resolve_under_base(
            &base("https://acme.atlassian.net/wiki/"),
            "/download/attachments/1/f.png?version=2",
        )
        .unwrap();
        assert_eq!(
            u.as_str(),
            "https://acme.atlassian.net/wiki/download/attachments/1/f.png?version=2"
        );

        let dc = resolve_under_base(
            &base("https://wiki.corp/confluence/"),
            "/download/attachments/1/f.png",
        )
        .unwrap();
        assert_eq!(dc.as_str(), "https://wiki.corp/confluence/download/attachments/1/f.png");
    }

    #[test]
    fn a_root_context_never_re_anchors() {
        let u = resolve_under_base(&base("https://wiki.corp/"), "/download/x.png").unwrap();
        assert_eq!(u.as_str(), "https://wiki.corp/download/x.png");
    }

    #[test]
    fn relative_links_resolve_under_the_base() {
        let u =
            resolve_under_base(&base("https://wiki.corp/confluence/"), "rest/api/space?start=25")
                .unwrap();
        assert_eq!(u.as_str(), "https://wiki.corp/confluence/rest/api/space?start=25");
    }

    #[test]
    fn ids_coerce_from_either_json_type() {
        let s: Id = serde_json::from_str("\"131009\"").unwrap();
        let n: Id = serde_json::from_str("131009").unwrap();
        assert_eq!(s.as_string(), "131009");
        assert_eq!(n.as_string(), "131009");
    }

    #[test]
    fn highlight_markers_are_stripped() {
        assert_eq!(
            strip_highlight_markers("the @@@hl@@@onboarding@@@endhl@@@ guide"),
            "the onboarding guide"
        );
    }

    #[test]
    fn space_keys_come_from_container_urls_of_either_flavor() {
        assert_eq!(space_key_from_display_url("/spaces/DOCS"), Some("DOCS".into()));
        assert_eq!(space_key_from_display_url("/display/OPS"), Some("OPS".into()));
        assert_eq!(space_key_from_display_url("/pages/1"), None);
        assert_eq!(space_key_from_display_url(""), None);
    }
}

// ------------------------------------------------------- space creation ---

/// What a refusal to create a space says, in the server's words: the
/// validation messages Data Center lists (`data.errors[].message.translation`),
/// else `message` or `errors[].title`, else the body as it came.
pub fn server_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.trim().chars().take(400).collect();
    };
    let listed: Vec<String> = value["data"]["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(value["errors"].as_array().into_iter().flatten())
        .filter_map(|e| {
            e["message"]["translation"]
                .as_str()
                .or_else(|| e["message"].as_str())
                .or_else(|| e["title"].as_str())
                .or_else(|| e["detail"].as_str())
        })
        .map(str::to_string)
        .collect();
    if !listed.is_empty() {
        return listed.join("; ");
    }
    value["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| body.trim().chars().take(400).collect())
}

/// Why a server refused to create a space, when its answer says: 403 is a
/// missing permission on both flavors, 409 a taken key, and a 400 says which
/// in its message. `None` for a refusal of another kind (a name too long, a
/// server problem), which the caller reports as the server put it.
pub fn space_refusal(status: u16, body: &str) -> Option<crate::types::SpaceRefusal> {
    use crate::types::SpaceRefusal as R;
    let said = server_message(body).to_ascii_lowercase();
    let unauthorized = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["data"]["authorized"].as_bool())
        == Some(false);
    match status {
        403 => Some(R::NoPermission),
        409 => Some(R::KeyTaken),
        400 if unauthorized || said.contains("permission") => Some(R::NoPermission),
        400 if ["already exist", "exists", "taken", "in use", "not unique"]
            .iter()
            .any(|w| said.contains(w)) =>
        {
            Some(R::KeyTaken)
        }
        400 if said.contains("key") => Some(R::InvalidKey),
        _ => None,
    }
}

#[cfg(test)]
mod space_tests {
    use super::*;
    use crate::types::SpaceRefusal as R;

    #[test]
    fn refusals_are_read_from_status_and_message() {
        let dc = |m: &str| {
            format!(
                r#"{{"statusCode":400,"data":{{"authorized":true,"valid":false,
                "errors":[{{"message":{{"translation":"{m}","args":[]}}}}]}},
                "message":"com.atlassian.confluence.api.service.exceptions.BadRequestException"}}"#
            )
        };
        assert_eq!(
            space_refusal(400, &dc("A space with key DOCS already exists")),
            Some(R::KeyTaken)
        );
        assert_eq!(space_refusal(400, &dc("Invalid space key")), Some(R::InvalidKey));
        assert_eq!(space_refusal(403, "{}"), Some(R::NoPermission));
        assert_eq!(space_refusal(409, "{}"), Some(R::KeyTaken));
        assert_eq!(space_refusal(400, &dc("Space name is too long")), None);
        assert_eq!(server_message(&dc("Invalid space key")), "Invalid space key");
        let unauthorized =
            r#"{"statusCode":400,"data":{"authorized":false,"valid":false,"errors":[]}}"#;
        assert_eq!(space_refusal(400, unauthorized), Some(R::NoPermission));
    }
}
