//! Wiremock fixtures for the Confluence Cloud (REST v2) client.
//!
//! Every client is built with [`RetryPolicy::immediate`] so the retry tests finish in
//! milliseconds instead of sleeping through real backoff.

use confed_api::{
    ApiError, Attachment, AttachmentId, Auth, BodyFormat, CommentActivity, CommentKind,
    ConfluenceClient, Http, InlineAnchor, NewPage, PageId, PageStatus, PageUpdate, Position,
    RetryPolicy, SpaceId,
};
use confed_cloud::CloudClient;
use serde_json::json;
use wiremock::matchers::{body_json, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The site root a Cloud tenant exposes: origin + `/wiki`.
fn client(server: &MockServer) -> CloudClient {
    let base = format!("{}/wiki", server.uri());
    let http = Http::new(
        &base,
        Auth::Basic { user: "tester@example.com".into(), secret: confed_api::Secret::new("token") },
        4,
    )
    .unwrap()
    .with_policy(RetryPolicy::immediate());
    CloudClient::new(&base, Auth::None, 4).unwrap().with_http(http)
}

fn space_body() -> serde_json::Value {
    json!({ "results": [{ "id": "500", "key": "DOCS", "name": "Documentation", "type": "global",
                          "homepageId": "1000" }] })
}

/// The reverse lookup `get_page` performs to turn a `spaceId` back into a key.
async fn mount_space_lookup(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces/500"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "id": "500", "key": "DOCS", "name": "Documentation" })),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn whoami_reads_the_v1_current_user_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/user/current"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accountId": "acc-1",
            "displayName": "Test User",
            "email": "tester@example.com",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let user = client(&server).whoami().await.unwrap();
    assert_eq!(user.display_name, "Test User");
    assert_eq!(user.account_id.as_deref(), Some("acc-1"));
    assert_eq!(user.email.as_deref(), Some("tester@example.com"));
}

#[tokio::test]
async fn get_space_captures_the_numeric_id_alongside_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces"))
        .and(query_param("keys", "DOCS"))
        .respond_with(ResponseTemplate::new(200).set_body_json(space_body()))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let space = client.get_space("DOCS").await.unwrap();
    assert_eq!(space.id.key, "DOCS");
    assert_eq!(space.id.numeric.as_deref(), Some("500"));
    assert_eq!(space.name, "Documentation");
    assert_eq!(space.homepage_id.unwrap(), PageId::new("1000"));

    // The lookup is cached: listing pages by key alone must not re-request the space
    // (the `expect(1)` above would fail if it did).
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces/500/pages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
        .mount(&server)
        .await;
    assert!(client.list_pages(&SpaceId::from_key("DOCS")).await.unwrap().is_empty());
}

#[tokio::test]
async fn list_pages_follows_the_cursor_and_returns_every_item_once() {
    let server = MockServer::start().await;
    // Page 1: has `_links.next`, which is site-rooted and already carries `/wiki`.
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces/500/pages"))
        .and(query_param("status", "current"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "id": "1", "title": "Alpha", "status": "current", "spaceId": "500",
                  "version": { "number": 3 } },
                { "id": "2", "title": "Beta", "status": "current", "spaceId": "500",
                  "parentId": "1", "version": { "number": 1 } }
            ],
            "_links": { "next": "/wiki/api/v2/spaces/500/pages?cursor=PAGE2" }
        })))
        .mount(&server)
        .await;
    // Page 2: the cursor request, with no `next` — the walk stops here.
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces/500/pages"))
        .and(query_param("cursor", "PAGE2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "id": "3", "title": "Gamma", "status": "archived", "spaceId": "500",
                  "parentId": "2", "version": { "number": 9 } }
            ],
            "_links": {}
        })))
        .mount(&server)
        .await;

    let space = SpaceId { key: "DOCS".into(), numeric: Some("500".into()) };
    let pages = client(&server).list_pages(&space).await.unwrap();

    let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["1", "2", "3"], "each item exactly once, in order");
    assert!(pages.iter().all(|p| p.space_key == "DOCS"));
    assert_eq!(pages[0].version, 3);
    assert_eq!(pages[1].parent_id, Some(PageId::new("1")));
    assert_eq!(pages[2].status, PageStatus::Archived);
}

#[tokio::test]
async fn a_single_page_of_results_makes_exactly_one_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/spaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "id": "500", "key": "DOCS", "name": "Documentation" }],
            "_links": {}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let spaces = client(&server).list_spaces(None).await.unwrap();
    assert_eq!(spaces.len(), 1);
}

#[tokio::test]
async fn get_page_parses_body_version_parent_and_labels() {
    let server = MockServer::start().await;
    mount_space_lookup(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/1001"))
        .and(query_param("body-format", "storage"))
        .and(query_param("include-labels", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001",
            "status": "current",
            "title": "Onboarding",
            "spaceId": "500",
            "parentId": "900",
            "createdAt": "2026-01-01T00:00:00Z",
            "version": { "number": 7, "createdAt": "2026-02-02T00:00:00Z", "authorId": "acc-1",
                         "message": "tweak" },
            "body": { "storage": { "value": "<p>Welcome</p>", "representation": "storage" } },
            "labels": { "results": [{ "name": "guide" }, { "name": "hr" }] }
        })))
        .mount(&server)
        .await;

    let page = client(&server).get_page(&PageId::new("1001"), BodyFormat::Storage).await.unwrap();
    assert_eq!(page.body_storage, "<p>Welcome</p>");
    assert_eq!(page.summary.version, 7);
    assert_eq!(page.summary.parent_id, Some(PageId::new("900")));
    assert_eq!(page.summary.labels, vec!["guide", "hr"]);
    assert_eq!(page.summary.space_key, "DOCS", "spaceId resolved back to the key");
    assert_eq!(page.summary.title, "Onboarding");
    assert_eq!(page.summary.updated_at.as_deref(), Some("2026-02-02T00:00:00Z"));
}

#[tokio::test]
async fn update_page_sends_the_version_number_verbatim() {
    let server = MockServer::start().await;
    mount_space_lookup(&server).await;
    Mock::given(method("PUT"))
        .and(path("/wiki/api/v2/pages/1001"))
        .and(body_json(json!({
            "id": "1001",
            "status": "current",
            "title": "Onboarding",
            "version": { "number": 8, "message": "confed push" },
            "body": { "representation": "storage", "value": "<p>New</p>" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001", "title": "Onboarding", "status": "current", "spaceId": "500",
            "version": { "number": 8 },
            "body": { "storage": { "value": "<p>New</p>" } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let update = PageUpdate {
        title: "Onboarding".into(),
        body_storage: Some("<p>New</p>".into()),
        version: 8,
        parent_id: None,
        status: None,
        message: Some("confed push".into()),
    };
    let page = client(&server).update_page(&PageId::new("1001"), &update).await.unwrap();
    assert_eq!(page.summary.version, 8);
    assert_eq!(page.body_storage, "<p>New</p>");
}

#[tokio::test]
async fn a_stale_update_maps_409_to_conflict() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/wiki/api/v2/pages/1001"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "errors": [{ "title": "Version must be incremented" }]
        })))
        .mount(&server)
        .await;

    let update = PageUpdate {
        title: "Onboarding".into(),
        body_storage: Some("<p>mine</p>".into()),
        version: 5,
        parent_id: None,
        status: None,
        message: None,
    };
    let err = client(&server).update_page(&PageId::new("1001"), &update).await.unwrap_err();
    assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
}

#[tokio::test]
async fn a_400_that_complains_about_the_version_is_also_a_conflict() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/wiki/api/v2/pages/1001"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string("Version must be incremented by one"),
        )
        .mount(&server)
        .await;

    let update = PageUpdate {
        title: "T".into(),
        body_storage: None,
        version: 2,
        parent_id: None,
        status: None,
        message: None,
    };
    let err = client(&server).update_page(&PageId::new("1001"), &update).await.unwrap_err();
    assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
}

#[tokio::test]
async fn create_page_posts_the_v2_shape_and_applies_labels_through_v1() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wiki/api/v2/pages"))
        .and(body_json(json!({
            "spaceId": "500",
            "status": "current",
            "title": "New Page",
            "parentId": "900",
            "body": { "representation": "storage", "value": "<p>Body</p>" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "2002", "title": "New Page", "status": "current", "spaceId": "500",
            "parentId": "900", "version": { "number": 1 }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/wiki/rest/api/content/2002/label"))
        .and(body_json(json!([{ "name": "draft" }])))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
        .expect(1)
        .mount(&server)
        .await;
    mount_space_lookup(&server).await;

    let new = NewPage {
        space: SpaceId { key: "DOCS".into(), numeric: Some("500".into()) },
        title: "New Page".into(),
        parent_id: Some(PageId::new("900")),
        body_storage: "<p>Body</p>".into(),
        labels: vec!["draft".into()],
    };
    let page = client(&server).create_page(&new).await.unwrap();
    assert_eq!(page.summary.id, PageId::new("2002"));
    assert_eq!(page.body_storage, "<p>Body</p>", "echoed back when v2 omits the body");
    assert_eq!(page.summary.labels, vec!["draft"]);
}

#[tokio::test]
async fn delete_and_move_hit_the_expected_endpoints() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/wiki/api/v2/pages/1001"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/wiki/rest/api/content/1001/move/append/900"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "pageId": "1001" })))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    c.delete_page(&PageId::new("1001")).await.unwrap();
    c.move_page(&PageId::new("1001"), &PageId::new("900"), Position::Append).await.unwrap();
}

#[tokio::test]
async fn labels_read_from_v2_and_write_through_v1() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/1001/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "name": "guide" }, { "name": "hr" }], "_links": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/wiki/rest/api/content/1001/label"))
        .and(query_param("name", "needs review"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    assert_eq!(c.get_labels(&PageId::new("1001")).await.unwrap(), vec!["guide", "hr"]);
    c.remove_label(&PageId::new("1001"), "needs review").await.unwrap();
}

#[tokio::test]
async fn comments_merge_footer_and_inline_lists() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/1001/footer-comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "10", "pageId": "1001",
                "version": { "number": 1, "createdAt": "2026-07-30T10:00:00Z", "authorId": "acc-2" },
                "body": { "storage": { "value": "<p>Nice page</p>" } }
            }],
            "_links": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/1001/inline-comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "11", "pageId": "1001", "resolutionStatus": "resolved",
                "properties": { "inlineMarkerRef": "m-7",
                                "inlineOriginalSelection": "first week checklist" },
                "body": { "storage": { "value": "<p>Fixed</p>" } }
            }],
            "_links": {}
        })))
        .mount(&server)
        .await;

    let comments = client(&server).list_comments(&PageId::new("1001")).await.unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0].kind, CommentKind::Footer);
    assert_eq!(comments[0].body_storage, "<p>Nice page</p>");
    assert!(comments[0].anchor.is_none());
    assert_eq!(comments[1].kind, CommentKind::Inline);
    assert!(comments[1].resolved);
    assert_eq!(comments[1].anchor.as_ref().unwrap().text, "first week checklist");
}

#[tokio::test]
async fn inline_comment_creation_sends_the_anchor_selection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wiki/api/v2/inline-comments"))
        .and(body_json(json!({
            "pageId": "1001",
            "body": { "representation": "storage", "value": "<p>typo?</p>" },
            "inlineCommentProperties": {
                "textSelection": "first week checklist",
                "textSelectionMatchCount": 1,
                "textSelectionMatchIndex": 0
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "12", "pageId": "1001", "resolutionStatus": "open",
            "properties": { "inlineOriginalSelection": "first week checklist" },
            "body": { "storage": { "value": "<p>typo?</p>" } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let anchor = InlineAnchor { text: "first week checklist".into(), ..Default::default() };
    let comment = client(&server)
        .add_inline_comment(&PageId::new("1001"), &anchor, "<p>typo?</p>")
        .await
        .unwrap();
    assert_eq!(comment.kind, CommentKind::Inline);
    assert!(!comment.resolved);
}

#[tokio::test]
async fn resolving_an_inline_comment_puts_resolved_true() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/wiki/api/v2/inline-comments/12"))
        .and(body_json(json!({ "resolved": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "12" })))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).resolve_comment(&confed_api::CommentId::new("12")).await.unwrap();
}

#[tokio::test]
async fn search_passes_cql_through_to_v1_and_builds_browser_urls() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/search"))
        .and(query_param("cql", "text ~ \"onboarding\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "content": { "id": "1001", "type": "page", "title": "Onboarding" },
                "title": "@@@hl@@@Onboarding@@@endhl@@@",
                "excerpt": "your @@@hl@@@first week@@@endhl@@@",
                "url": "/spaces/DOCS/pages/1001/Onboarding",
                "resultGlobalContainer": { "title": "Documentation", "displayUrl": "/spaces/DOCS" }
            }],
            "size": 1, "limit": 25, "start": 0, "_links": {}
        })))
        .mount(&server)
        .await;

    let hits = client(&server).search_cql("text ~ \"onboarding\"", 25).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page_id, PageId::new("1001"));
    assert_eq!(hits[0].title, "Onboarding");
    assert_eq!(hits[0].space_key.as_deref(), Some("DOCS"));
    assert_eq!(hits[0].excerpt.as_deref(), Some("your first week"));
    assert_eq!(hits[0].url, format!("{}/wiki/spaces/DOCS/pages/1001/Onboarding", server.uri()));
}

#[tokio::test]
async fn version_history_comes_from_v2_and_historical_bodies_from_v1() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/1001/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "number": 3, "createdAt": "2026-03-03T00:00:00Z", "authorId": "acc-1",
                  "message": "third" },
                { "number": 2, "createdAt": "2026-02-02T00:00:00Z", "authorId": "acc-2",
                  "message": "" }
            ],
            "_links": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/content/1001"))
        .and(query_param("version", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001", "type": "page", "title": "Onboarding",
            "space": { "key": "DOCS" },
            "version": { "number": 2, "when": "2026-02-02T00:00:00Z",
                         "by": { "displayName": "Alice Ng" } },
            "body": { "storage": { "value": "<p>Old</p>" } }
        })))
        .mount(&server)
        .await;

    let c = client(&server);
    let versions = c.get_page_versions(&PageId::new("1001"), 10).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].number, 3);
    assert_eq!(versions[0].message.as_deref(), Some("third"));
    assert_eq!(versions[1].message, None, "empty messages normalize to None");

    let old = c.get_page_at_version(&PageId::new("1001"), 2).await.unwrap();
    assert_eq!(old.body_storage, "<p>Old</p>");
    assert_eq!(old.summary.version, 2);
    assert_eq!(old.summary.space_key, "DOCS");
    assert_eq!(old.summary.author.as_deref(), Some("Alice Ng"));
}

#[tokio::test]
async fn a_401_maps_to_an_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/user/current"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Basic auth failed"))
        .mount(&server)
        .await;

    let err = client(&server).whoami().await.unwrap_err();
    assert!(matches!(err, ApiError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn a_404_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/404"))
        .respond_with(ResponseTemplate::new(404).set_body_string("no such page"))
        .mount(&server)
        .await;

    let err = client(&server).get_page(&PageId::new("404"), BodyFormat::Storage).await.unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn a_503_is_retried_and_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/user/current"))
        .respond_with(ResponseTemplate::new(503).set_body_string("try later"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/user/current"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "displayName": "Test User" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let user = client(&server).whoami().await.unwrap();
    assert_eq!(user.display_name, "Test User");
}

#[tokio::test]
async fn a_persistent_429_ends_as_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/user/current"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string("slow down"),
        )
        // `RetryPolicy::immediate` allows three attempts in total.
        .expect(3)
        .mount(&server)
        .await;

    let err = client(&server).whoami().await.unwrap_err();
    assert!(matches!(err, ApiError::RateLimited(_)), "got {err:?}");
    assert!(err.is_transient());
}

#[tokio::test]
async fn attachments_list_upload_and_download() {
    let server = MockServer::start().await;
    let bytes: &[u8] = b"\x89PNG\r\n\x1a\nfake-image-bytes";

    Mock::given(method("GET"))
        .and(path("/wiki/api/v2/pages/1001/attachments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "att-1", "title": "diagram.png", "pageId": "1001",
                "mediaType": "image/png", "fileSize": 21, "version": { "number": 2 },
                // Cloud hands back a link that omits the `/wiki` context path.
                "downloadLink": "/download/attachments/1001/diagram.png?version=2"
            }],
            "_links": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wiki/download/attachments/1001/diagram.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    // v1 multipart upload: assert the body really is multipart with a `file` part.
    Mock::given(method("POST"))
        .and(path("/wiki/rest/api/content/1001/child/attachment"))
        .and(header_exists("x-atlassian-token"))
        .respond_with(|req: &Request| {
            let content_type = req
                .headers
                .get("content-type")
                .map(|v| v.to_str().unwrap_or_default().to_string())
                .unwrap_or_default();
            let body = String::from_utf8_lossy(&req.body).to_string();
            if !content_type.starts_with("multipart/form-data")
                || !body.contains("name=\"file\"")
                || !body.contains("minorEdit")
            {
                return ResponseTemplate::new(400).set_body_string(format!(
                    "expected multipart with a file part, got {content_type}"
                ));
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "results": [{
                    "id": "att-1", "title": "diagram.png", "type": "attachment",
                    "container": { "id": "1001", "type": "page" },
                    "version": { "number": 3 },
                    "extensions": { "mediaType": "image/png", "fileSize": 21 },
                    "_links": { "download": "/download/attachments/1001/diagram.png?version=3" }
                }],
                "size": 1
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("diagram.png");
    tokio::fs::write(&source, bytes).await.unwrap();

    let c = client(&server);
    let listed = c.list_attachments(&PageId::new("1001")).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].filename, "diagram.png");
    assert_eq!(listed[0].version, 2);
    assert_eq!(listed[0].file_size, Some(21));

    let dest = dir.path().join("nested/out.png");
    let written = c.download_attachment(&listed[0], &dest).await.unwrap();
    assert_eq!(written, bytes.len() as u64);
    assert_eq!(tokio::fs::read(&dest).await.unwrap(), bytes);

    let uploaded = c.upload_attachment(&PageId::new("1001"), &source, None).await.unwrap();
    assert_eq!(uploaded.id, AttachmentId::new("att-1"));
    assert_eq!(uploaded.version, 3);
    assert_eq!(uploaded.page_id, PageId::new("1001"));
}

#[tokio::test]
async fn replacing_an_attachment_posts_to_the_data_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/wiki/rest/api/content/1001/child/attachment/att-1/data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "att-1", "title": "diagram.png", "type": "attachment",
            "container": { "id": "1001", "type": "page" },
            "version": { "number": 4 },
            "extensions": { "mediaType": "image/png", "fileSize": 3 }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("diagram.png");
    tokio::fs::write(&source, b"abc").await.unwrap();

    let uploaded = client(&server)
        .upload_attachment(&PageId::new("1001"), &source, Some(&AttachmentId::new("att-1")))
        .await
        .unwrap();
    assert_eq!(uploaded.version, 4);
}

#[tokio::test]
async fn a_download_without_a_link_fails_before_any_request() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let attachment = Attachment {
        id: AttachmentId::new("att-9"),
        page_id: PageId::new("1001"),
        filename: "x.bin".into(),
        media_type: None,
        file_size: None,
        version: 1,
        download_url: String::new(),
    };
    let err = client(&server)
        .download_attachment(&attachment, &dir.path().join("x.bin"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn capabilities_advertise_the_cloud_feature_set() {
    let server = MockServer::start().await;
    let c = client(&server);
    let caps = c.capabilities();
    assert!(caps.inline_comment_create);
    assert!(caps.comment_resolve);
    assert!(caps.adf);
    assert_eq!(
        c.page_url(&PageId::new("1001"), "DOCS"),
        format!("{}/wiki/spaces/DOCS/pages/1001", server.uri())
    );
}

/// v2 lists comments page by page only, so the comments changed across a space
/// are asked of CQL, which v1 still serves — by key, even when only the
/// numeric id is at hand.
#[tokio::test]
async fn recent_comment_activity_searches_the_space_by_key() {
    let server = MockServer::start().await;
    mount_space_lookup(&server).await;
    Mock::given(method("GET"))
        .and(path("/wiki/rest/api/search"))
        .and(query_param(
            "cql",
            r#"space = "DOCS" and type = comment and lastmodified >= now("-20m")"#,
        ))
        .and(query_param("expand", "content.container"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "content": {
                    "id": "2001", "type": "comment", "status": "current",
                    "container": { "id": "1001", "type": "page", "title": "Team Handbook" }
                },
                "title": "Re: Team Handbook",
                "url": "/spaces/DOCS/pages/1001/Team+Handbook?focusedCommentId=2001",
                "lastModified": "2026-08-30T09:00:00.000Z"
            }],
            "start": 0, "limit": 100, "size": 1, "_links": {}
        })))
        .expect(2)
        .mount(&server)
        .await;

    let c = client(&server);
    for space in
        [SpaceId::from_key("DOCS"), SpaceId { key: String::new(), numeric: Some("500".into()) }]
    {
        let activity = c.recent_comment_activity(&space, 20).await.unwrap();
        assert_eq!(activity, CommentActivity::Pages(vec![PageId::new("1001")]));
    }
}
