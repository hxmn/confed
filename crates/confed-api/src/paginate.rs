//! The two pagination dialects Confluence speaks, behind one collector each.
//!
//! Cloud v2 is cursor based: every response carries `_links.next`, a ready-made URL
//! that already contains the opaque cursor. Data Center v1 (and the v1 endpoints
//! Cloud still exposes) is offset based: the caller advances `start` itself.

use crate::error::ApiResult;
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
    let mut out: Vec<T> = Vec::new();
    let mut page: CursorPage<T> = http.get_json(path, query).await?;
    for _ in 0..MAX_REQUESTS {
        out.append(&mut page.results);
        if limit.is_some_and(|l| out.len() >= l) {
            break;
        }
        let Some(next) = page.links.next.take() else { break };
        let url = resolve_under_base(http.base_url(), &next)?;
        page = http.get_json(url.as_str(), &[]).await?;
    }
    if let Some(l) = limit {
        out.truncate(l);
    }
    Ok(out)
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
    let page_size = page_size.max(1);
    let mut out: Vec<T> = Vec::new();
    let mut start = 0usize;
    for _ in 0..MAX_REQUESTS {
        let want = limit.map(|l| (l - out.len()).min(page_size)).unwrap_or(page_size);
        let mut q: Vec<(&str, String)> = query.to_vec();
        q.push(("start", start.to_string()));
        q.push(("limit", want.to_string()));

        let mut page: OffsetPage<T> = http.get_json(path, &q).await?;
        let got = page.results.len();
        out.append(&mut page.results);
        start += got;

        if limit.is_some_and(|l| out.len() >= l) {
            break;
        }
        // A short page is the end of the collection on every Confluence version we
        // support; `_links.next` is only a corroborating hint.
        if got == 0 || (got < want && page.links.next.is_none()) {
            break;
        }
    }
    if let Some(l) = limit {
        out.truncate(l);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
