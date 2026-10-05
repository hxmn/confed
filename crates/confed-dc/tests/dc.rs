//! Wiremock fixtures for the Confluence Data Center (REST v1) client.
//!
//! Every client is built with [`RetryPolicy::immediate`] so the retry tests finish in
//! milliseconds instead of sleeping through real backoff.

use confed_api::{
    ApiError, Attachment, AttachmentId, Auth, BodyFormat, CommentId, CommentKind, ConfluenceClient,
    Http, InlineAnchor, NewPage, PageId, PageStatus, PageUpdate, Position, RetryPolicy, Secret,
    SpaceId,
};
use confed_dc::DcClient;
use serde_json::json;
use wiremock::matchers::{body_json, header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A Data Center install served from a context path, like most real deployments.
fn client_with(server: &MockServer, auth: Auth) -> DcClient {
    let base = format!("{}/confluence", server.uri());
    let http = Http::new(&base, auth, 8).unwrap().with_policy(RetryPolicy::immediate());
    DcClient::new(&base, Auth::None, 8).unwrap().with_http(http)
}

fn client(server: &MockServer) -> DcClient {
    client_with(server, Auth::Bearer(Secret::new("pat-token")))
}

#[tokio::test]
async fn whoami_works_with_a_personal_access_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/user/current"))
        .and(header("authorization", "Bearer pat-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "known", "username": "tester", "userKey": "ff8081",
            "displayName": "Test User", "email": "tester@corp.example"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let user = client(&server).whoami().await.unwrap();
    assert_eq!(user.display_name, "Test User");
    assert_eq!(user.username.as_deref(), Some("tester"));
    assert_eq!(user.email.as_deref(), Some("tester@corp.example"));
}

#[tokio::test]
async fn whoami_also_works_with_basic_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/user/current"))
        // base64("tester:hunter2")
        .and(header("authorization", "Basic dGVzdGVyOmh1bnRlcjI="))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "displayName": "Test User" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let auth = Auth::Basic { user: "tester".into(), secret: Secret::new("hunter2") };
    assert_eq!(client_with(&server, auth).whoami().await.unwrap().display_name, "Test User");
}

#[tokio::test]
async fn get_space_captures_the_numeric_id_alongside_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/space"))
        .and(query_param("spaceKey", "DOCS"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                // Data Center returns space ids as JSON numbers, not strings.
                "id": 500, "key": "DOCS", "name": "Documentation", "type": "global",
                "homepage": { "id": "1000", "type": "page", "title": "Home" }
            }],
            "size": 1, "start": 0, "limit": 1, "_links": {}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let space = client(&server).get_space("DOCS").await.unwrap();
    assert_eq!(space.id.key, "DOCS");
    assert_eq!(space.id.numeric.as_deref(), Some("500"));
    assert_eq!(space.name, "Documentation");
    assert_eq!(space.kind.as_deref(), Some("global"));
    assert_eq!(space.homepage_id, Some(PageId::new("1000")));
}

#[tokio::test]
async fn get_space_reports_an_unknown_key_as_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [], "size": 0 })))
        .mount(&server)
        .await;

    let err = client(&server).get_space("NOPE").await.unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn list_pages_walks_start_and_limit_and_returns_every_item_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content"))
        .and(query_param("spaceKey", "DOCS"))
        .and(query_param("type", "page"))
        .and(query_param("start", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "id": "1", "type": "page", "status": "current", "title": "Alpha",
                  "space": { "key": "DOCS" }, "version": { "number": 3 }, "ancestors": [] },
                { "id": "2", "type": "page", "status": "current", "title": "Beta",
                  "space": { "key": "DOCS" }, "version": { "number": 1 },
                  "ancestors": [{ "id": "1", "type": "page" }] }
            ],
            "start": 0, "limit": 2, "size": 2,
            "_links": { "next": "/rest/api/content?spaceKey=DOCS&start=2&limit=2" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content"))
        .and(query_param("start", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "id": "3", "type": "page", "status": "archived", "title": "Gamma",
                  "space": { "key": "DOCS" }, "version": { "number": 9 },
                  "ancestors": [{ "id": "1", "type": "page" }, { "id": "2", "type": "page" }] }
            ],
            "start": 2, "limit": 2, "size": 1, "_links": {}
        })))
        .mount(&server)
        .await;

    let pages = client(&server).list_pages(&SpaceId::from_key("DOCS")).await.unwrap();
    let ids: Vec<&str> = pages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["1", "2", "3"], "each item exactly once, in order");
    assert!(pages[0].parent_id.is_none());
    assert_eq!(pages[1].parent_id, Some(PageId::new("1")));
    assert_eq!(pages[2].parent_id, Some(PageId::new("2")), "deepest ancestor wins");
    assert_eq!(pages[2].status, PageStatus::Archived);
    assert!(pages.iter().all(|p| p.space_key == "DOCS"));
}

#[tokio::test]
async fn an_empty_collection_yields_no_items_and_stops_immediately() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [], "start": 0, "limit": 100, "size": 0, "_links": {}
        })))
        .expect(1)
        .mount(&server)
        .await;

    assert!(client(&server).list_pages(&SpaceId::from_key("DOCS")).await.unwrap().is_empty());
}

#[tokio::test]
async fn get_page_parses_body_version_parent_and_labels() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001"))
        .and(query_param("expand", "body.storage,version,ancestors,metadata.labels,space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001", "type": "page", "status": "current", "title": "Onboarding",
            "space": { "id": 500, "key": "DOCS", "name": "Documentation" },
            "version": { "number": 7, "when": "2026-02-02T00:00:00Z",
                         "by": { "displayName": "Alice Ng" }, "message": "tweak" },
            "ancestors": [{ "id": "1", "type": "page" }, { "id": "900", "type": "page" }],
            "body": { "storage": { "value": "<p>Welcome</p>", "representation": "storage" } },
            "metadata": { "labels": { "results": [
                { "name": "guide", "prefix": "global" }, { "name": "hr", "prefix": "global" }
            ] } },
            "history": { "createdDate": "2026-01-01T00:00:00Z" }
        })))
        .mount(&server)
        .await;

    let page = client(&server).get_page(&PageId::new("1001"), BodyFormat::Storage).await.unwrap();
    assert_eq!(page.body_storage, "<p>Welcome</p>");
    assert_eq!(page.summary.version, 7);
    assert_eq!(page.summary.parent_id, Some(PageId::new("900")));
    assert_eq!(page.summary.labels, vec!["guide", "hr"]);
    assert_eq!(page.summary.space_key, "DOCS");
    assert_eq!(page.summary.author.as_deref(), Some("Alice Ng"));
    assert_eq!(page.summary.created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(page.summary.updated_at.as_deref(), Some("2026-02-02T00:00:00Z"));
}

#[tokio::test]
async fn update_page_sends_the_version_number_verbatim() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/confluence/rest/api/content/1001"))
        .and(body_json(json!({
            "id": "1001",
            "type": "page",
            "title": "Onboarding",
            "version": { "number": 8, "message": "confed push" },
            "body": { "storage": { "value": "<p>New</p>", "representation": "storage" } },
            "ancestors": [{ "id": "900" }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001", "type": "page", "status": "current", "title": "Onboarding",
            "space": { "key": "DOCS" }, "version": { "number": 8 },
            "body": { "storage": { "value": "<p>New</p>" } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let update = PageUpdate {
        title: "Onboarding".into(),
        body_storage: Some("<p>New</p>".into()),
        version: 8,
        parent_id: Some(PageId::new("900")),
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
        .and(path("/confluence/rest/api/content/1001"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "statusCode": 409,
            "message": "Version must be incremented on update."
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
async fn create_page_posts_the_v1_shape_and_applies_labels() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/confluence/rest/api/content"))
        .and(body_json(json!({
            "type": "page",
            "title": "New Page",
            "space": { "key": "DOCS" },
            "ancestors": [{ "id": "900" }],
            "body": { "storage": { "value": "<p>Body</p>", "representation": "storage" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "2002", "type": "page", "status": "current", "title": "New Page",
            "space": { "key": "DOCS" }, "version": { "number": 1 }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/confluence/rest/api/content/2002/label"))
        .and(body_json(json!([{ "prefix": "global", "name": "draft" }])))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": [] })))
        .expect(1)
        .mount(&server)
        .await;

    let new = NewPage {
        space: SpaceId::from_key("DOCS"),
        title: "New Page".into(),
        parent_id: Some(PageId::new("900")),
        body_storage: "<p>Body</p>".into(),
        labels: vec!["draft".into()],
    };
    let page = client(&server).create_page(&new).await.unwrap();
    assert_eq!(page.summary.id, PageId::new("2002"));
    assert_eq!(page.summary.parent_id, Some(PageId::new("900")));
    assert_eq!(page.body_storage, "<p>Body</p>");
    assert_eq!(page.summary.labels, vec!["draft"]);
}

#[tokio::test]
async fn moving_a_page_re_parents_it_with_a_version_bump() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001", "type": "page", "title": "Onboarding", "version": { "number": 7 }
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/confluence/rest/api/content/1001"))
        .and(body_json(json!({
            "id": "1001", "type": "page", "title": "Onboarding",
            "ancestors": [{ "id": "900" }],
            "version": { "number": 8, "message": "moved by confed" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "1001" })))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .move_page(&PageId::new("1001"), &PageId::new("900"), Position::Append)
        .await
        .unwrap();
}

#[tokio::test]
async fn sibling_ordering_uses_the_move_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/confluence/rest/api/content/1001/move/before/2002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "pageId": "1001" })))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .move_page(&PageId::new("1001"), &PageId::new("900"), Position::Before(2002))
        .await
        .unwrap();
}

#[tokio::test]
async fn labels_round_trip_through_the_v1_label_endpoints() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001/label"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "name": "guide", "prefix": "global" },
                        { "name": "hr", "prefix": "global" }],
            "size": 2, "_links": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/confluence/rest/api/content/1001/label"))
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
async fn comments_split_into_footer_and_inline_by_inline_properties() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001/child/comment"))
        .and(query_param("depth", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {
                    "id": "10", "type": "comment",
                    "container": { "id": "1001", "type": "page" },
                    "body": { "storage": { "value": "<p>Nice page</p>" } },
                    "history": { "createdDate": "2026-07-30T10:00:00Z",
                                 "createdBy": { "displayName": "Bob Lee" } }
                },
                {
                    "id": "11", "type": "comment",
                    "container": { "id": "1001", "type": "page" },
                    "ancestors": [{ "id": "1001", "type": "page" },
                                  { "id": "10", "type": "comment" }],
                    "body": { "storage": { "value": "<p>Agreed</p>" } },
                    "history": { "createdDate": "2026-07-30T10:01:00Z" }
                },
                {
                    "id": "12", "type": "comment",
                    "container": { "id": "1001", "type": "page" },
                    "body": { "storage": { "value": "<p>Typo here</p>" } },
                    "extensions": {
                        "inlineProperties": { "originalSelection": "first week checklist",
                                              "markerRef": "m-7" },
                        "resolution": { "status": "resolved" }
                    },
                    "history": { "createdDate": "2026-07-30T10:02:00Z",
                                 "createdBy": { "displayName": "Alice Ng" } }
                }
            ],
            "size": 3, "_links": {}
        })))
        .mount(&server)
        .await;

    let comments = client(&server).list_comments(&PageId::new("1001")).await.unwrap();
    assert_eq!(comments.len(), 3);

    assert_eq!(comments[0].kind, CommentKind::Footer);
    assert_eq!(comments[0].author.as_deref(), Some("Bob Lee"));
    assert!(comments[0].parent_comment_id.is_none());

    assert_eq!(comments[1].kind, CommentKind::Footer);
    assert_eq!(comments[1].parent_comment_id, Some(CommentId::new("10")), "reply keeps its parent");

    assert_eq!(comments[2].kind, CommentKind::Inline);
    assert!(comments[2].resolved);
    let anchor = comments[2].anchor.as_ref().unwrap();
    assert_eq!(anchor.text, "first week checklist");
    assert_eq!(anchor.marker_ref.as_deref(), Some("m-7"));
    assert!(!anchor.orphaned);
}

#[tokio::test]
async fn footer_comments_are_created_as_child_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/confluence/rest/api/content"))
        .and(body_json(json!({
            "type": "comment",
            "container": { "id": "1001", "type": "page" },
            "body": { "storage": { "value": "<p>Reply</p>", "representation": "storage" } },
            "ancestors": [{ "id": "10" }]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "13", "type": "comment",
            "container": { "id": "1001", "type": "page" },
            "body": { "storage": { "value": "<p>Reply</p>" } }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let comment = client(&server)
        .add_footer_comment(&PageId::new("1001"), "<p>Reply</p>", Some(&CommentId::new("10")))
        .await
        .unwrap();
    assert_eq!(comment.id, CommentId::new("13"));
    assert_eq!(comment.kind, CommentKind::Footer);
    assert_eq!(comment.parent_comment_id, Some(CommentId::new("10")));
}

// --------------------------------------------- inline comments (private API) ----
//
// Data Center has no public API for inline comments; confed uses the plugin API
// the page view calls. These replay the requests captured from DC 9.5.4 in
// `fixtures/dc-inline`.

const TEST_PAGE: &str = "900000099218";

fn fixture(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/fixtures/dc-inline/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// The applinks manifest, as Data Center serves it (XML) to anyone.
async fn mount_manifest(server: &MockServer, version: &str) {
    Mock::given(method("GET"))
        .and(path("/confluence/rest/applinks/1.0/manifest"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<manifest><id>abc</id><name>Confluence</name><typeId>confluence</typeId>\
             <version>{version}</version><buildNumber>10000</buildNumber></manifest>"
        )))
        .mount(server)
        .await;
}

async fn mount_page_version(server: &MockServer, version: u32) {
    Mock::given(method("GET"))
        .and(path(format!("/confluence/rest/api/content/{TEST_PAGE}")))
        .and(query_param("expand", "version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TEST_PAGE, "type": "page", "title": "Test", "status": "current",
            "version": { "number": version }
        })))
        .mount(server)
        .await;
}

fn selection() -> InlineAnchor {
    InlineAnchor {
        text: "Фраза 3.".into(),
        match_index: Some(0),
        match_count: Some(1),
        ..Default::default()
    }
}

#[tokio::test]
async fn an_inline_comment_is_created_the_way_the_page_view_does_it() {
    let server = MockServer::start().await;
    mount_manifest(&server, "9.5.4").await;
    mount_page_version(&server, 1).await;
    let captured = fixture("create.request.json");
    Mock::given(method("POST"))
        .and(path("/confluence/rest/inlinecomments/1.0/comments"))
        .and(header("authorization", "Bearer pat-token"))
        .and(header("content-type", "application/json"))
        .and(header("x-atlassian-token", "no-check"))
        .and(move |req: &Request| {
            let sent: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            // Everything the server validates matches the browser's request.
            [
                "originalSelection",
                "body",
                "matchIndex",
                "numMatches",
                "containerId",
                "containerVersion",
                "parentCommentId",
                "deleted",
            ]
            .iter()
            .all(|k| sent[k] == captured[k])
                && sent["serializedHighlights"].is_string()
                && sent["lastFetchTime"].as_str().is_some_and(|t| t.parse::<u64>().is_ok())
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("create.response.json")))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server)
        .add_inline_comment(&PageId::new(TEST_PAGE), &selection(), "<p>test comment</p>")
        .await
        .unwrap();
    assert_eq!(c.id, CommentId::new("900000099220"));
    assert_eq!(c.kind, CommentKind::Inline);
    let anchor = c.anchor.unwrap();
    assert_eq!(anchor.text, "Фраза 3.");
    assert_eq!(anchor.marker_ref.as_deref(), Some("0f8c1a52-4e7b-4c3d-9a6e-2b5d7e9f1c30"));
}

#[tokio::test]
async fn a_reply_to_an_inline_thread_goes_to_its_replies() {
    let server = MockServer::start().await;
    mount_manifest(&server, "9.5.4").await;
    Mock::given(method("POST"))
        .and(path("/confluence/rest/inlinecomments/1.0/comments/900000099222/replies"))
        .and(query_param("containerId", TEST_PAGE))
        .and(header("x-atlassian-token", "no-check"))
        .and(body_json(json!({ "body": "<p>replya</p>", "commentId": 900000099222u64 })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("reply.response.json")))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server)
        .add_inline_reply(&PageId::new(TEST_PAGE), &CommentId::new("900000099222"), "<p>replya</p>")
        .await
        .unwrap();
    assert_eq!(c.id, CommentId::new("900000099224"));
    assert_eq!(c.parent_comment_id, Some(CommentId::new("900000099222")));
}

#[tokio::test]
async fn resolving_sends_the_comment_back_to_its_resolve_endpoint() {
    let server = MockServer::start().await;
    mount_manifest(&server, "9.5.4").await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/900000099220"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "900000099220", "type": "comment", "status": "current",
            "container": { "id": TEST_PAGE, "type": "page" },
            "body": { "storage": { "value": "<p>test comment</p>", "representation": "storage" } },
            "extensions": {
                "location": "inline",
                "inlineProperties": {
                    "originalSelection": "Фраза 3.",
                    "markerRef": "0f8c1a52-4e7b-4c3d-9a6e-2b5d7e9f1c30"
                },
                "resolution": { "status": "open" }
            }
        })))
        .mount(&server)
        .await;
    let captured = fixture("resolve.request.json");
    Mock::given(method("PUT"))
        .and(path(
            "/confluence/rest/inlinecomments/1.0/comments/900000099220/resolve/true/dangling/false",
        ))
        .and(header("x-atlassian-token", "no-check"))
        .and(move |req: &Request| {
            let sent: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            ["id", "originalSelection", "body", "containerId", "markerRef", "parentCommentId"]
                .iter()
                .all(|k| sent[k] == captured[k])
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("resolve.response.json")))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).resolve_comment(&CommentId::new("900000099220")).await.unwrap();
}

#[tokio::test]
async fn a_footer_comment_cannot_be_resolved_on_data_center() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "10", "type": "comment", "container": { "id": "1001", "type": "page" },
            "body": { "storage": { "value": "<p>x</p>", "representation": "storage" } }
        })))
        .mount(&server)
        .await;
    let err = client(&server).resolve_comment(&CommentId::new("10")).await.unwrap_err();
    assert!(matches!(err, ApiError::Unsupported { .. }), "{err:?}");
}

#[tokio::test]
async fn a_missing_inline_api_is_unsupported_and_names_the_step() {
    let server = MockServer::start().await;
    mount_manifest(&server, "7.13.0").await;
    mount_page_version(&server, 1).await;
    Mock::given(method("POST"))
        .and(path("/confluence/rest/inlinecomments/1.0/comments"))
        .respond_with(ResponseTemplate::new(405).set_body_string("Method Not Allowed"))
        .mount(&server)
        .await;
    let err = client(&server)
        .add_inline_comment(&PageId::new(TEST_PAGE), &selection(), "<p>x</p>")
        .await
        .unwrap_err();
    match err {
        ApiError::Unsupported { operation, .. } => {
            assert!(operation.contains("create inline comment"), "{operation}");
            assert!(operation.contains("7.13.0"), "says which server: {operation}");
        }
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[tokio::test]
async fn a_refused_selection_is_rejected_not_a_generic_error() {
    let server = MockServer::start().await;
    mount_manifest(&server, "9.5.4").await;
    mount_page_version(&server, 1).await;
    Mock::given(method("POST"))
        .and(path("/confluence/rest/inlinecomments/1.0/comments"))
        .respond_with(ResponseTemplate::new(412).set_body_string("The text selection is wrong"))
        .mount(&server)
        .await;
    let err = client(&server)
        .add_inline_comment(&PageId::new(TEST_PAGE), &selection(), "<p>x</p>")
        .await
        .unwrap_err();
    match err {
        ApiError::Rejected(message) => {
            assert!(message.contains("create inline comment"), "{message}");
            assert!(message.contains("412"), "{message}");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn the_server_version_comes_from_the_applinks_manifest() {
    let server = MockServer::start().await;
    mount_manifest(&server, "9.5.4").await;
    assert_eq!(client(&server).server_version().await.unwrap().as_deref(), Some("9.5.4"));
}

#[tokio::test]
async fn search_passes_cql_through_and_builds_browser_urls() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/search"))
        .and(query_param("cql", "text ~ \"onboarding\""))
        .and(query_param("limit", "25"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "content": { "id": "1001", "type": "page", "title": "Onboarding" },
                "title": "@@@hl@@@Onboarding@@@endhl@@@",
                "excerpt": "your @@@hl@@@first week@@@endhl@@@",
                "url": "/display/DOCS/Onboarding",
                "resultGlobalContainer": { "title": "Documentation", "displayUrl": "/display/DOCS" }
            }],
            "size": 1, "limit": 25, "start": 0, "totalSize": 1, "_links": {}
        })))
        .mount(&server)
        .await;

    let hits = client(&server).search_cql("text ~ \"onboarding\"", 25).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page_id, PageId::new("1001"));
    assert_eq!(hits[0].title, "Onboarding");
    assert_eq!(hits[0].space_key.as_deref(), Some("DOCS"));
    assert_eq!(hits[0].excerpt.as_deref(), Some("your first week"));
    assert_eq!(hits[0].url, format!("{}/confluence/display/DOCS/Onboarding", server.uri()));
}

#[tokio::test]
async fn version_history_and_page_at_version() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "number": 3, "when": "2026-03-03T00:00:00Z",
                  "by": { "displayName": "Alice Ng" }, "message": "third" },
                { "number": 2, "when": "2026-02-02T00:00:00Z",
                  "by": { "displayName": "Bob Lee" }, "message": "" }
            ],
            "size": 2, "_links": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001"))
        .and(query_param("version", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001", "type": "page", "title": "Onboarding",
            "space": { "key": "DOCS" },
            "version": { "number": 2, "when": "2026-02-02T00:00:00Z",
                         "by": { "displayName": "Bob Lee" } },
            "body": { "storage": { "value": "<p>Old</p>" } }
        })))
        .mount(&server)
        .await;

    let c = client(&server);
    let versions = c.get_page_versions(&PageId::new("1001"), 10).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].number, 3);
    assert_eq!(versions[0].author.as_deref(), Some("Alice Ng"));
    assert_eq!(versions[1].message, None, "empty messages normalize to None");

    let old = c.get_page_at_version(&PageId::new("1001"), 2).await.unwrap();
    assert_eq!(old.body_storage, "<p>Old</p>");
    assert_eq!(old.summary.version, 2);
    assert_eq!(old.summary.space_key, "DOCS");
}

#[tokio::test]
async fn a_401_maps_to_an_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/user/current"))
        .respond_with(ResponseTemplate::new(401).set_body_string("PAT rejected"))
        .mount(&server)
        .await;

    let err = client(&server).whoami().await.unwrap_err();
    assert!(matches!(err, ApiError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn a_403_also_maps_to_an_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001"))
        .respond_with(ResponseTemplate::new(403).set_body_string("no permission"))
        .mount(&server)
        .await;

    let err =
        client(&server).get_page(&PageId::new("1001"), BodyFormat::Storage).await.unwrap_err();
    assert!(matches!(err, ApiError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn a_503_is_retried_and_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/user/current"))
        .respond_with(ResponseTemplate::new(503).set_body_string("maintenance"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/user/current"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "displayName": "Test User" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(client(&server).whoami().await.unwrap().display_name, "Test User");
}

#[tokio::test]
async fn a_persistent_429_ends_as_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/user/current"))
        .respond_with(
            ResponseTemplate::new(429).insert_header("retry-after", "0").set_body_string("busy"),
        )
        .expect(3)
        .mount(&server)
        .await;

    let err = client(&server).whoami().await.unwrap_err();
    assert!(matches!(err, ApiError::RateLimited(_)), "got {err:?}");
}

#[tokio::test]
async fn attachments_list_upload_and_download() {
    let server = MockServer::start().await;
    let bytes: &[u8] = b"%PDF-1.4 fake pdf bytes";

    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001/child/attachment"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "att123", "type": "attachment", "title": "spec.pdf",
                "container": { "id": "1001", "type": "page" },
                "version": { "number": 2 },
                "extensions": { "mediaType": "application/pdf", "fileSize": 23 },
                "_links": { "download": "/download/attachments/1001/spec.pdf?version=2" }
            }],
            "size": 1, "_links": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/confluence/download/attachments/1001/spec.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/confluence/rest/api/content/1001/child/attachment"))
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
                || !body.contains("spec.pdf")
                || !body.contains("minorEdit")
            {
                return ResponseTemplate::new(400)
                    .set_body_string(format!("bad upload: {content_type}"));
            }
            ResponseTemplate::new(200).set_body_json(json!({
                "results": [{
                    "id": "att123", "type": "attachment", "title": "spec.pdf",
                    "container": { "id": "1001", "type": "page" },
                    "version": { "number": 3 },
                    "extensions": { "mediaType": "application/pdf", "fileSize": 23 }
                }],
                "size": 1
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("spec.pdf");
    tokio::fs::write(&source, bytes).await.unwrap();

    let c = client(&server);
    let listed = c.list_attachments(&PageId::new("1001")).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, AttachmentId::new("att123"));
    assert_eq!(listed[0].media_type.as_deref(), Some("application/pdf"));

    let dest = dir.path().join("nested/spec.pdf");
    let written = c.download_attachment(&listed[0], &dest).await.unwrap();
    assert_eq!(written, bytes.len() as u64);
    assert_eq!(tokio::fs::read(&dest).await.unwrap(), bytes);

    let uploaded = c.upload_attachment(&PageId::new("1001"), &source, None).await.unwrap();
    assert_eq!(uploaded.version, 3);
    assert_eq!(uploaded.page_id, PageId::new("1001"));
}

#[tokio::test]
async fn deleting_a_page_or_attachment_uses_the_content_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/confluence/rest/api/content/1001"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/confluence/rest/api/content/att123"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    c.delete_page(&PageId::new("1001")).await.unwrap();
    c.delete_attachment(&AttachmentId::new("att123")).await.unwrap();
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
async fn list_spaces_respects_a_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/space"))
        .and(query_param("limit", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                { "id": 500, "key": "DOCS", "name": "Documentation" },
                { "id": 501, "key": "OPS", "name": "Operations" }
            ],
            "size": 2, "start": 0, "limit": 2, "_links": {}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let spaces = client(&server).list_spaces(Some(2)).await.unwrap();
    assert_eq!(spaces.len(), 2);
    assert_eq!(spaces[1].id.key, "OPS");
    assert_eq!(spaces[1].id.numeric.as_deref(), Some("501"));
}

#[tokio::test]
async fn capabilities_advertise_the_data_center_gaps() {
    let server = MockServer::start().await;
    let c = client(&server);
    assert!(c.capabilities().inline_comment_create);
    assert!(c.capabilities().comment_resolve);
    assert!(!c.capabilities().adf);
    assert_eq!(
        c.page_url(&PageId::new("1001"), "DOCS"),
        format!("{}/confluence/pages/viewpage.action?pageId=1001", server.uri())
    );
}

#[tokio::test]
async fn a_comment_is_edited_and_deleted_through_rest_v1() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "77", "type": "comment", "status": "current",
            "container": { "id": "1001", "type": "page" },
            "version": { "number": 2 }
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/confluence/rest/api/content/77"))
        .and(body_json(json!({
            "id": "77",
            "type": "comment",
            "version": { "number": 3 },
            "body": { "storage": { "value": "<p>new</p>", "representation": "storage" } },
            "container": { "id": "1001", "type": "page" }
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": "77", "type": "comment" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/confluence/rest/api/content/77"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server);
    c.update_comment(&CommentId::new("77"), CommentKind::Inline, "<p>new</p>").await.unwrap();
    c.delete_comment(&CommentId::new("77"), CommentKind::Inline).await.unwrap();
}

/// Data Center 9.5.4 has no `rest/api/content/{id}/version`. History comes from
/// the experimental endpoint when there is one, else version by version.
#[tokio::test]
async fn version_history_without_the_version_endpoint() {
    let server = MockServer::start().await;
    // Unmatched requests answer 404, as the missing endpoint does.
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/1001"))
        .and(query_param("expand", "version"))
        .and(|req: &Request| !req.url.query().unwrap_or("").contains("historical"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "1001", "type": "page",
            "version": { "number": 3, "when": "2026-03-03T00:00:00Z", "by": { "displayName": "Alice Ng" } }
        })))
        .mount(&server)
        .await;
    for (n, who) in [(2, "Bob Lee"), (1, "Carol")] {
        Mock::given(method("GET"))
            .and(path("/confluence/rest/api/content/1001"))
            .and(query_param("status", "historical"))
            .and(query_param("version", n.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "1001", "type": "page",
                "version": { "number": n, "by": { "displayName": who }, "message": "m" }
            })))
            .mount(&server)
            .await;
    }

    let versions = client(&server).get_page_versions(&PageId::new("1001"), 10).await.unwrap();
    let numbers: Vec<u32> = versions.iter().map(|v| v.number).collect();
    assert_eq!(numbers, [3, 2, 1]);
    assert_eq!(versions[1].author.as_deref(), Some("Bob Lee"));

    let two = client(&server).get_page_versions(&PageId::new("1001"), 2).await.unwrap();
    assert_eq!(two.len(), 2, "the limit holds");
}

#[tokio::test]
async fn version_history_from_the_experimental_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/experimental/content/1001/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "number": 2, "by": { "displayName": "Bob Lee" } }, { "number": 1 }],
            "size": 2, "_links": {}
        })))
        .mount(&server)
        .await;
    let versions = client(&server).get_page_versions(&PageId::new("1001"), 10).await.unwrap();
    assert_eq!(versions.iter().map(|v| v.number).collect::<Vec<_>>(), [2, 1]);
}

#[tokio::test]
async fn version_history_of_a_missing_page_is_not_found() {
    let server = MockServer::start().await;
    let err = client(&server).get_page_versions(&PageId::new("404"), 10).await.unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err:?}");
}

/// Data Center's inline-comment API answers 500 to a body with `<ac:link>`.
/// The comment is created without them, and its real body put right after
/// through the content API — one comment, mentions and links intact.
#[tokio::test]
async fn an_inline_comment_with_a_mention_and_a_page_link() {
    let server = MockServer::start().await;
    mount_manifest(&server, "9.5.4").await;
    mount_page_version(&server, 1).await;
    let body = "<p>Ask <ac:link><ri:user ri:userkey=\"ff8081\" /></ac:link> about \
                <ac:link><ri:page ri:content-title=\"CH-200.1\" /></ac:link></p>";
    Mock::given(method("POST"))
        .and(path("/confluence/rest/inlinecomments/1.0/comments"))
        .and(|req: &Request| {
            let sent: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let posted = sent["body"].as_str().unwrap_or_default();
            !posted.contains("<ac:link")
                && posted.contains("@ff8081")
                && posted.contains("CH-200.1")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("create.response.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/content/900000099220"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "900000099220", "type": "comment",
            "container": { "id": TEST_PAGE, "type": "page" },
            "version": { "number": 1 }
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/confluence/rest/api/content/900000099220"))
        .and(move |req: &Request| {
            let sent: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            sent["body"]["storage"]["value"] == json!(body) && sent["version"]["number"] == json!(2)
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "id": "900000099220", "type": "comment" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let c = client(&server)
        .add_inline_comment(&PageId::new(TEST_PAGE), &selection(), body)
        .await
        .unwrap();
    assert_eq!(c.id, CommentId::new("900000099220"));
    assert_eq!(c.body_storage, body, "recorded with its links");
}

#[tokio::test]
async fn people_are_found_by_name_with_their_userkey() {
    let server = MockServer::start().await;
    // No user search endpoint (404): the general search answers instead.
    Mock::given(method("GET"))
        .and(path("/confluence/rest/api/search"))
        .and(query_param("cql", "type = user and user.fullname ~ \"Danny\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{ "user": {
                "type": "known", "username": "kimball", "userKey": "8a8b81",
                "displayName": "Kimball Danny"
            } }],
            "size": 1, "_links": {}
        })))
        .mount(&server)
        .await;
    let people = client(&server).search_users("Danny", 10).await.unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].user_key.as_deref(), Some("8a8b81"));
    assert_eq!(people[0].display_name, "Kimball Danny");
}
