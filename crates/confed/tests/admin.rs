//! End-to-end tests for `confed admin`: the real binary against a [`wiremock`]
//! server impersonating Confluence Data Center. Admin commands need no
//! workspace, only a base URL and a credential, so none is created. Every
//! `--json` result is validated against `docs/reference/json/admin.schema.json`.

mod schema;

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ENV_VARS: &[&str] = &[
    "CONFED_JSON",
    "CONFED_NON_INTERACTIVE",
    "CONFED_BASE_URL",
    "CONFED_TOKEN",
    "CONFED_USERNAME",
    "CONFED_SPACE",
    "CONFED_FLAVOR",
    "CONFED_CONCURRENCY",
    "CONFED_LOG",
    "CONFED_EDITOR",
];

/// `confed admin …` against `server` as a Data Center administrator, with a
/// closed stdin so a stray prompt fails instead of hanging.
fn admin(dir: &Path, server: &MockServer, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_confed"));
    for var in ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env("NO_COLOR", "1").env("CONFED_TOKEN", "pat-token");
    cmd.arg("-C").arg(dir);
    cmd.args(["--base-url", &server.uri(), "--flavor", "dc", "--space", "DOCS", "admin"]);
    cmd.args(args);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

fn run(mut cmd: Command) -> Output {
    cmd.output().expect("running the confed binary")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("confed exited via a signal")
}

fn envelope(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}):\n{text}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    schema::check("envelope", &value);
    assert_eq!(value["confed"]["command"], json!("admin"));
    if value["result"].is_object() {
        schema::check("admin", &value["result"]);
    }
    value
}

/// Every mutating request the mock server has seen, as `METHOD /path`.
async fn mutations(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| matches!(r.method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH"))
        .map(|r| format!("{} {}", r.method.as_str(), r.url.path()))
        .collect()
}

#[tokio::test]
async fn a_group_is_created_and_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/admin/group"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "name": "writers" })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(admin(dir.path(), &server, &["group", "create", "writers", "--json"]));
    assert_eq!(exit_code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["action"], json!("group.create"));
    assert_eq!(value["result"]["group"]["name"], json!("writers"));
}

#[tokio::test]
async fn a_dry_run_says_what_it_would_do_and_sends_nothing() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    for args in [
        &["group", "delete", "writers", "--dry-run", "--json"][..],
        &["group", "add", "writers", "alice.ng", "bob.lee", "--dry-run", "--json"],
        &["user", "disable", "bob.lee", "--dry-run", "--json"],
        &[
            "perms",
            "grant",
            "group:writers",
            "--op",
            "space:read,page:create",
            "--dry-run",
            "--json",
        ],
    ] {
        let out = run(admin(dir.path(), &server, args));
        assert_eq!(exit_code(&out), 0, "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        let value = envelope(&out);
        assert_eq!(value["result"]["dry_run"], json!(true), "{args:?}");
        assert!(!value["result"]["planned"].as_array().unwrap().is_empty(), "{args:?}");
    }
    assert!(mutations(&server).await.is_empty(), "{:?}", mutations(&server).await);
}

#[tokio::test]
async fn an_unknown_operation_is_refused_before_anything_is_sent() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let out =
        run(admin(dir.path(), &server, &["perms", "grant", "group:writers", "--op", "page:bogus"]));
    assert_eq!(exit_code(&out), 2);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("space:read"), "the hint lists the valid operations: {stderr}");
    assert!(mutations(&server).await.is_empty());
}

#[tokio::test]
async fn a_role_on_data_center_exits_9() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let out =
        run(admin(dir.path(), &server, &["perms", "grant", "group:writers", "--role", "Viewer"]));
    assert_eq!(exit_code(&out), 9, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(mutations(&server).await.is_empty());
}

#[tokio::test]
async fn a_new_user_needs_a_password_or_notify_and_the_password_is_never_logged() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rest/api/admin/user"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "userKey": "ff80a1" })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let create =
        ["user", "create", "alice.ng", "--full-name", "Alice Ng", "--email", "alice@example.com"];

    // No terminal to ask on, and nothing on stdin.
    let out = run(admin(dir.path(), &server, &create));
    assert_eq!(exit_code(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--password-stdin"));

    let mut cmd = admin(dir.path(), &server, &create);
    cmd.args(["--password-stdin", "--json", "--log", "confed::http=trace"]);
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"s3cret-Pass\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(exit_code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["user"]["userkey"], json!("ff80a1"));

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("<redacted>"), "the request is traced, its body is not: {stderr}");
    assert!(!stderr.contains("s3cret-Pass"), "{stderr}");
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    assert_eq!(body["password"], json!("s3cret-Pass"));
}

#[tokio::test]
async fn without_a_terminal_a_delete_goes_ahead() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/rest/api/admin/user/bob.lee"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(admin(dir.path(), &server, &["user", "delete", "bob.lee", "--json"]));
    assert_eq!(exit_code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(envelope(&out)["result"]["username"], json!("bob.lee"));
}

#[tokio::test]
async fn adding_several_users_reports_the_ones_that_failed() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/rest/api/user/alice.ng/group/writers"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/rest/api/user/nobody/group/writers"))
        .respond_with(ResponseTemplate::new(404).set_body_string("no such user"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(admin(
        dir.path(),
        &server,
        &["group", "add", "writers", "alice.ng", "nobody", "--json"],
    ));
    assert_eq!(exit_code(&out), 8, "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["users"], json!(["alice.ng"]));
    assert_eq!(value["errors"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn space_permissions_are_listed_with_who_holds_them() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rest/api/space/DOCS/permissions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "operation": { "targetType": "space", "operationKey": "read" },
              "subject": { "type": "group", "name": "writers" } },
            { "operation": { "targetType": "page", "operationKey": "create" },
              "subject": { "type": "group", "name": "writers" } },
            { "operation": { "targetType": "space", "operationKey": "administer" },
              "subject": { "type": "user", "userKey": "ff80a1" } }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest/api/user"))
        .and(query_param("key", "ff80a1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "username": "alice.ng", "userKey": "ff80a1", "displayName": "Alice Ng"
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(admin(dir.path(), &server, &["perms", "list", "--json"]));
    assert_eq!(exit_code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let value = envelope(&out);
    assert_eq!(value["result"]["mode"], json!("permissions"));
    assert_eq!(value["result"]["permissions"].as_array().unwrap().len(), 3);
    assert_eq!(value["result"]["permissions"][2]["subject"]["name"], json!("Alice Ng"));

    let human = run(admin(dir.path(), &server, &["perms", "list"]));
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("group:writers") && text.contains("space:read page:create"), "{text}");
}
