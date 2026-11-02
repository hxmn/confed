//! Confluence's own links, read as page references: what a user copies from
//! the browser's address bar names a page as well as `KEY:path` does.

/// What a Confluence URL names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlRef {
    /// A page, by id: `/spaces/KEY/pages/ID/…`, `/pages/viewpage.action?pageId=ID`.
    Page { id: String, space: Option<String> },
    /// A page, by space and title: `/display/KEY/Title`.
    Titled { space: String, title: String },
    /// A space, which as a destination means its top, and as a page its home:
    /// `/spaces/KEY`, `/display/KEY`.
    Space { space: String },
}

/// A parsed URL: where it points, and the server it is on (`host[:port]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedUrl {
    pub server: String,
    pub target: UrlRef,
}

/// Whether `reference` is a URL at all, whatever it names.
pub fn is_url(reference: &str) -> bool {
    reference.starts_with("https://") || reference.starts_with("http://")
}

/// Read a Confluence page or space URL; `None` when it names neither.
pub fn parse(reference: &str) -> Option<ParsedUrl> {
    if !is_url(reference) {
        return None;
    }
    let url = url::Url::parse(reference).ok()?;
    let server = server_of(&url)?;
    let query =
        |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    let segments: Vec<String> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).map(decode).collect())
        .unwrap_or_default();
    // Cloud puts everything under /wiki; a Data Center may sit under a context path.
    let at = |name: &str| segments.iter().position(|s| s == name);
    let target = if let Some(id) = query("pageId").filter(|id| is_id(id)) {
        UrlRef::Page { id, space: None }
    } else if let Some(i) = at("spaces") {
        let rest = &segments[i + 1..];
        match rest {
            [action, ..] if action == "viewspace.action" => {
                UrlRef::Space { space: query("key").or_else(|| query("spaceKey"))? }
            }
            [key, pages, id, ..] if pages == "pages" && is_id(id) => {
                UrlRef::Page { id: id.clone(), space: Some(key.clone()) }
            }
            [key, pages, kind, id, ..] if pages == "pages" && !is_id(kind) && is_id(id) => {
                // `/spaces/KEY/pages/edit-v2/ID` and the like.
                UrlRef::Page { id: id.clone(), space: Some(key.clone()) }
            }
            [key] => UrlRef::Space { space: key.clone() },
            [key, overview] if overview == "overview" => UrlRef::Space { space: key.clone() },
            _ => return None,
        }
    } else {
        let i = at("display")?;
        match &segments[i + 1..] {
            [key] => UrlRef::Space { space: key.clone() },
            [key, title, ..] => UrlRef::Titled { space: key.clone(), title: title.clone() },
            _ => return None,
        }
    };
    Some(ParsedUrl { server, target })
}

/// `host[:port]` of a base URL or a page URL, for comparing the two.
pub fn server(base_url: &str) -> Option<String> {
    server_of(&url::Url::parse(base_url).ok()?)
}

fn server_of(url: &url::Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn is_id(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// A path segment as text: `%XX` decoded, and `+` a space, as Confluence
/// writes titles in `/display/` links.
fn decode(segment: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> UrlRef {
        parse(url).unwrap_or_else(|| panic!("{url} was not read")).target
    }

    #[test]
    fn page_urls_name_the_page_by_id() {
        assert_eq!(
            target("https://wiki.example.test/spaces/ENG/pages/9000000123/Team+Handbook"),
            UrlRef::Page { id: "9000000123".into(), space: Some("ENG".into()) }
        );
        assert_eq!(
            target("https://example.atlassian.net/wiki/spaces/DOCS/pages/1001/Onboarding"),
            UrlRef::Page { id: "1001".into(), space: Some("DOCS".into()) }
        );
        assert_eq!(
            target("https://wiki.example.test/pages/viewpage.action?pageId=1001"),
            UrlRef::Page { id: "1001".into(), space: None }
        );
        assert_eq!(
            target("https://wiki.example.test/confluence/spaces/DOCS/pages/edit-v2/1001"),
            UrlRef::Page { id: "1001".into(), space: Some("DOCS".into()) }
        );
    }

    #[test]
    fn display_urls_name_a_page_by_title_or_the_space() {
        assert_eq!(
            target("https://wiki.example.test/display/DOCS/Team+Handbook"),
            UrlRef::Titled { space: "DOCS".into(), title: "Team Handbook".into() }
        );
        assert_eq!(
            target("https://wiki.example.test/display/DOCS/%D0%A4%D1%80%D0%B0%D0%B7%D0%B0+1."),
            UrlRef::Titled { space: "DOCS".into(), title: "Фраза 1.".into() }
        );
        assert_eq!(
            target("https://wiki.example.test/display/DOCS"),
            UrlRef::Space { space: "DOCS".into() }
        );
        assert_eq!(
            target("https://wiki.example.test/display/DOCS/"),
            UrlRef::Space { space: "DOCS".into() }
        );
    }

    #[test]
    fn space_urls_name_the_space() {
        for url in [
            "https://wiki.example.test/spaces/ENG",
            "https://wiki.example.test/spaces/ENG/",
            "https://example.atlassian.net/wiki/spaces/ENG/overview",
            "https://wiki.example.test/spaces/viewspace.action?key=ENG",
        ] {
            assert_eq!(target(url), UrlRef::Space { space: "ENG".into() }, "{url}");
        }
    }

    #[test]
    fn the_server_is_compared_by_host_and_port() {
        assert_eq!(
            parse("https://Wiki.Example.Test/display/DOCS").unwrap().server,
            "wiki.example.test"
        );
        assert_eq!(server("http://127.0.0.1:8090").as_deref(), Some("127.0.0.1:8090"));
    }

    #[test]
    fn anything_else_is_not_a_page_url() {
        assert!(parse("DOCS:Team Handbook.md").is_none());
        assert!(parse("1001").is_none());
        assert!(parse("https://wiki.example.test/plugins/servlet/whatever").is_none());
        assert!(parse("https://wiki.example.test/spaces/ENG/blog/2026/01/01/1").is_none());
    }

    #[test]
    fn a_stray_percent_is_kept() {
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("100%2"), "100%2");
        assert_eq!(decode("a%20b"), "a b");
    }
}
