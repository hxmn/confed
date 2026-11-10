//! The shared HTTP stack against a mock server: which failures are retried,
//! and how long a request may take.

use confed_api::http::{Auth, Http, RetryPolicy, Timeouts};
use confed_api::ApiError;
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn http(server: &MockServer) -> Http {
    Http::new(&server.uri(), Auth::None, 4).unwrap().with_policy(RetryPolicy::immediate())
}

async fn requests_to(server: &MockServer, route: &str) -> usize {
    let received = server.received_requests().await.unwrap();
    received.iter().filter(|r| r.url.path() == route).count()
}

/// A gateway's 503 can arrive after Confluence took the request. Sending a
/// create again would make a second page; a read may be repeated freely.
#[tokio::test]
async fn a_gateway_error_is_retried_only_for_requests_safe_to_repeat() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/content"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/api/content"))
        .respond_with(ResponseTemplate::new(504))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/api/content"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;

    let http = http(&server);
    let err = http
        .post_json::<_, Value>("rest/api/content", &json!({ "title": "Team Handbook" }))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Network { .. }), "{err:?}");
    assert_eq!(requests_to(&server, "/rest/api/content").await, 1, "the create was sent once");

    let read: Value = http.get_json("rest/api/content", &[]).await.unwrap();
    assert_eq!(read, json!({ "ok": true }));
    assert_eq!(requests_to(&server, "/rest/api/content").await, 3, "the read was retried");
}

/// A 429 means nothing was done, so even a create is sent again.
#[tokio::test]
async fn a_rate_limited_create_is_sent_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/content"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rest/api/content"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "1001" })))
        .mount(&server)
        .await;

    let created: Value = http(&server)
        .post_json("rest/api/content", &json!({ "title": "Onboarding" }))
        .await
        .unwrap();
    assert_eq!(created["id"], "1001");
}

fn short() -> Timeouts {
    Timeouts {
        request: Duration::from_millis(300),
        stall: Duration::from_secs(5),
        after_upload: Duration::from_secs(5),
    }
}

/// An API call is bounded as a whole; a transfer only by silence, so one that
/// takes longer than an API call may is not cut off for it.
#[tokio::test]
async fn a_slow_transfer_outlives_the_api_deadline() {
    let server = MockServer::start().await;
    let slow = Duration::from_millis(800);
    Mock::given(method("GET"))
        .and(path("/rest/api/space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})).set_delay(slow))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download/attachments/1001/diagram.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 4096]).set_delay(slow))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rest/api/content/1001/child/attachment"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "results": [] })).set_delay(slow),
        )
        .mount(&server)
        .await;

    let http = Http::new(&server.uri(), Auth::None, 4)
        .unwrap()
        .with_policy(RetryPolicy { max_attempts: 1, ..RetryPolicy::immediate() })
        .with_timeouts(short());

    let err = http.get_json::<Value>("rest/api/space", &[]).await.unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("diagram.png");
    let written = http.download_to("download/attachments/1001/diagram.png", &dest).await.unwrap();
    assert_eq!(written, 4096);
    assert_eq!(std::fs::read(&dest).unwrap().len(), 4096);

    let file = dir.path().join("upload.bin");
    std::fs::write(&file, vec![1u8; 256 * 1024]).unwrap();
    let _: Value = http
        .upload_multipart("rest/api/content/1001/child/attachment", &file, "file", &[])
        .await
        .unwrap();

    let received = server.received_requests().await.unwrap();
    let upload = received.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let body = &upload.body;
    assert!(body.len() > 256 * 1024, "the whole file went up: {} bytes", body.len());
    let header = String::from_utf8_lossy(&body[..body.len().min(400)]);
    assert!(header.contains(r#"filename="upload.bin""#), "{header}");
}

/// A download whose server stops answering fails after the stall limit, and
/// leaves no partial file behind.
#[tokio::test]
async fn a_download_with_no_answer_fails_after_the_stall_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/download/attachments/1001/notes.txt"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("x").set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let http = Http::new(&server.uri(), Auth::None, 4)
        .unwrap()
        .with_policy(RetryPolicy { max_attempts: 1, ..RetryPolicy::immediate() })
        .with_timeouts(Timeouts { stall: Duration::from_millis(200), ..short() });

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("notes.txt");
    let err = http.download_to("download/attachments/1001/notes.txt", &dest).await.unwrap_err();
    assert!(matches!(err, ApiError::Network { .. }), "{err:?}");
    assert!(err.to_string().contains("no answer"), "{err}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing left behind");
}

/// An upload the server has whole but never answers is not sent again: the
/// server may have stored it.
#[tokio::test]
async fn an_unanswered_upload_fails_once_and_is_not_repeated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/content/1001/child/attachment"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;

    let http = Http::new(&server.uri(), Auth::None, 4)
        .unwrap()
        .with_policy(RetryPolicy::immediate())
        .with_timeouts(Timeouts { after_upload: Duration::from_millis(200), ..short() });

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("report.pdf");
    std::fs::write(&file, b"%PDF-1.4").unwrap();
    let err = http
        .upload_multipart::<Value>("rest/api/content/1001/child/attachment", &file, "file", &[])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no answer came"), "{err}");
    assert_eq!(requests_to(&server, "/rest/api/content/1001/child/attachment").await, 1);
}

/// A link in a response can name any host. The credential goes only to the
/// one confed was pointed at: a next-page link elsewhere is followed on the
/// base, and a download elsewhere is fetched without it.
#[tokio::test]
async fn the_credential_never_leaves_the_configured_host() {
    use wiremock::matchers::header;

    let wiki = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/api/space"))
        .and(wiremock::matchers::query_param("cursor", "2"))
        .and(header("authorization", "Bearer pat-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": ["ENG"] })))
        .mount(&wiki)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/api/space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": ["DOCS"],
            "_links": { "next": format!("{}/rest/api/space?cursor=2", other.uri()) }
        })))
        .mount(&wiki)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"file".to_vec()))
        .mount(&other)
        .await;

    let http = Http::new(&wiki.uri(), Auth::Bearer(confed_api::Secret::new("pat-token")), 4)
        .unwrap()
        .with_policy(RetryPolicy::immediate());

    let spaces: Vec<String> =
        confed_api::paginate::collect_cursor(&http, "rest/api/space", &[], None).await.unwrap();
    assert_eq!(spaces, ["DOCS", "ENG"]);

    let dir = tempfile::tempdir().unwrap();
    let foreign = format!("{}/media/file.png", other.uri());
    http.download_to(&foreign, &dir.path().join("file.png")).await.unwrap();

    let at_other = other.received_requests().await.unwrap();
    assert_eq!(at_other.len(), 1, "only the download went there");
    assert!(
        at_other.iter().all(|r| !r.headers.contains_key("authorization")),
        "no credential sent to another host"
    );

    let err = http.get_json::<Value>(&foreign, &[]).await.unwrap_err();
    assert!(matches!(err, ApiError::Url(_)), "an API call elsewhere is refused: {err:?}");
}

/// A transport failure keeps the error beneath it, for whoever walks the
/// chain, and still names the cause in its own message, for whoever prints it.
#[tokio::test]
async fn a_network_error_keeps_its_source() {
    // Nothing listens on port 1.
    let http = Http::new("http://127.0.0.1:1", Auth::None, 1)
        .unwrap()
        .with_policy(RetryPolicy { max_attempts: 1, ..RetryPolicy::immediate() });
    let err = http.get_json::<Value>("rest/api/space", &[]).await.unwrap_err();
    let ApiError::Network { message, source } = &err else { panic!("{err:?}") };
    assert!(message.contains("could not connect"), "{message}");
    assert!(source.is_some(), "the reqwest error is kept");
    assert!(std::error::Error::source(&err).is_some());
}
