//! The two pagination dialects Confluence speaks, behind one collector each.
//!
//! Cloud v2 is cursor based: every response carries `_links.next`, a ready-made URL
//! that already contains the opaque cursor. Data Center v1 (and the v1 endpoints
//! Cloud still exposes) is offset based: the caller advances `start` itself.

use crate::error::{ApiError, ApiResult};
use crate::http::Http;
use crate::wire::resolve_under_base;
use serde::de::DeserializeOwned;
use serde::Deserialize;

/// A server that keeps handing out a `next` link must not spin us forever.
const MAX_REQUESTS: usize = 10_000;

#[derive(Debug, Default, Deserialize)]
pub struct Links {
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
}

/// `{ "results": [...], "_links": { "next": "…" } }`
#[derive(Debug, Deserialize)]
pub struct CursorPage<T> {
    #[serde(default = "Vec::new")]
    pub results: Vec<T>,
    #[serde(rename = "_links", default)]
    pub links: Links,
}

impl<T> Default for CursorPage<T> {
    fn default() -> Self {
        Self { results: Vec::new(), links: Links::default() }
    }
}

/// `{ "results": [...], "start": 0, "limit": 25, "size": 25, "_links": {...} }`
#[derive(Debug, Deserialize)]
pub struct OffsetPage<T> {
    #[serde(default = "Vec::new")]
    pub results: Vec<T>,
    #[serde(rename = "_links", default)]
    pub links: Links,
}

/// Follow `_links.next` until it is absent, concatenating `results`.
///
/// `query` applies to the first request only — every `next` URL already repeats it.
/// `limit` caps the number of items returned (not requested).
pub async fn collect_cursor<T: DeserializeOwned>(
    http: &Http,
    path: &str,
    query: &[(&str, String)],
    limit: Option<usize>,
) -> ApiResult<Vec<T>> {
    collect_cursor_within(http, path, query, limit, MAX_REQUESTS).await
}

async fn collect_cursor_within<T: DeserializeOwned>(
    http: &Http,
    path: &str,
    query: &[(&str, String)],
    limit: Option<usize>,
    max_requests: usize,
) -> ApiResult<Vec<T>> {
    let mut out: Vec<T> = Vec::new();
    let mut page: CursorPage<T> = http.get_json(path, query).await?;
    let mut followed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..max_requests {
        out.append(&mut page.results);
        if limit.is_some_and(|l| out.len() >= l) {
            if let Some(l) = limit {
                out.truncate(l);
            }
            return Ok(out);
        }
        let Some(next) = page.links.next.take() else { return Ok(out) };
        let mut url = resolve_under_base(http.base_url(), &next)?;
        // A next link on another host — a proxy's own name, a misconfigured
        // base URL — is followed on this one: the request carries the
        // credential, and only the path and cursor are the server's to say.
        if !http.same_origin(&url) {
            tracing::debug!(target: "confed::http", next = %url, "next page link on another host; following it on the base");
            let tail = match url.query() {
                Some(q) => format!("{}?{q}", url.path()),
                None => url.path().to_string(),
            };
            url = resolve_under_base(http.base_url(), &tail)?;
        }
        // The same page again would loop for ever; a list read that far is
        // not complete, and must not be taken for it.
        if !followed.insert(url.to_string()) {
            return Err(ApiError::Server {
                status: 200,
                body: format!("{path}: the server handed out the same next page twice ({url})"),
            });
        }
        page = http.get_json(url.as_str(), &[]).await?;
    }
    Err(cut_short(path, max_requests))
}

/// A listing that would not end: an error, since returning what was read
/// would pass a partial list for the whole — for a page listing, the rest of
/// the space taken for deleted.
fn cut_short(path: &str, max_requests: usize) -> ApiError {
    ApiError::Server {
        status: 200,
        body: format!("{path}: still more after {max_requests} requests; the list is not complete"),
    }
}

/// Walk `start`/`limit` until the server returns a short page (or stops offering
/// `_links.next`), concatenating `results`.
pub async fn collect_offset<T: DeserializeOwned>(
    http: &Http,
    path: &str,
    query: &[(&str, String)],
    page_size: usize,
    limit: Option<usize>,
) -> ApiResult<Vec<T>> {
    collect_offset_within(http, path, query, page_size, limit, MAX_REQUESTS).await
}

async fn collect_offset_within<T: DeserializeOwned>(
    http: &Http,
    path: &str,
    query: &[(&str, String)],
    page_size: usize,
    limit: Option<usize>,
    max_requests: usize,
) -> ApiResult<Vec<T>> {
    let page_size = page_size.max(1);
    let mut out: Vec<T> = Vec::new();
    let mut start = 0usize;
    let mut complete = false;
    for _ in 0..max_requests {
        let want = limit.map(|l| (l - out.len()).min(page_size)).unwrap_or(page_size);
        let mut q: Vec<(&str, String)> = query.to_vec();
        q.push(("start", start.to_string()));
        q.push(("limit", want.to_string()));

        let mut page: OffsetPage<T> = http.get_json(path, &q).await?;
        let got = page.results.len();
        out.append(&mut page.results);
        start += got;

        if limit.is_some_and(|l| out.len() >= l) {
            complete = true;
            break;
        }
        // A short page is the end of the collection on every Confluence version we
        // support; `_links.next` is only a corroborating hint.
        if got == 0 || (got < want && page.links.next.is_none()) {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(cut_short(path, max_requests));
    }
    if let Some(l) = limit {
        out.truncate(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::http::Auth;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    #[tokio::test]
    async fn a_listing_that_never_ends_is_an_error_not_a_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(|req: &Request| {
                let n: u64 = req
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "cursor")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [n],
                    "_links": { "next": format!("/pages?cursor={}", n + 1) }
                }))
            })
            .mount(&server)
            .await;
        let http = Http::new(&server.uri(), Auth::None, 1).unwrap();
        let err = collect_cursor_within::<u64>(&http, "pages", &[], None, 5).await.unwrap_err();
        assert!(err.to_string().contains("not complete"), "{err}");
    }

    #[tokio::test]
    async fn a_repeated_cursor_is_named_for_what_it_is() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [1], "_links": { "next": "/pages?cursor=same" }
            })))
            .mount(&server)
            .await;
        let http = Http::new(&server.uri(), Auth::None, 1).unwrap();
        let err = collect_cursor::<u64>(&http, "pages", &[], None).await.unwrap_err();
        assert!(err.to_string().contains("same next page twice"), "{err}");
    }

    #[tokio::test]
    async fn an_offset_listing_that_never_ends_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [1, 2], "_links": { "next": "/more" }
            })))
            .mount(&server)
            .await;
        let http = Http::new(&server.uri(), Auth::None, 1).unwrap();
        let err = collect_offset_within::<u64>(&http, "items", &[], 2, None, 4).await.unwrap_err();
        assert!(err.to_string().contains("not complete"), "{err}");
    }

    #[test]
    fn a_missing_results_array_decodes_as_empty() {
        let p: CursorPage<String> = serde_json::from_str("{}").unwrap();
        assert!(p.results.is_empty());
        assert!(p.links.next.is_none());

        let p: OffsetPage<String> = serde_json::from_str("{\"size\":0}").unwrap();
        assert!(p.results.is_empty());
    }

    #[test]
    fn cursor_links_are_read_from_underscore_links() {
        let p: CursorPage<serde_json::Value> = serde_json::from_str(
            r#"{"results":[1,2],"_links":{"next":"/wiki/api/v2/pages?cursor=x","base":"b"}}"#,
        )
        .unwrap();
        assert_eq!(p.results.len(), 2);
        assert_eq!(p.links.next.as_deref(), Some("/wiki/api/v2/pages?cursor=x"));
    }
}
