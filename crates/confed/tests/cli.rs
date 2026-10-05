//! End-to-end tests for the `confed` binary.
//!
//! Every test runs the real executable (`env!("CARGO_BIN_EXE_confed")`) against a
//! [`wiremock`] server impersonating Confluence Data Center (REST v1 — the same
//! request/response shapes `crates/confed-api/tests/dc.rs` pins down). Nothing
//! here sleeps, nothing reaches the network, and each test gets its own
//! `tempfile::tempdir()` plus its own mock server, so the whole file is safe to
//! run in parallel with the rest of the workspace.
//!
//! The JSON envelope produced by each `--json` run is validated against the
//! schemas in `docs/reference/json/`, so those files cannot drift from the code.

mod schema;

use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SPACE: &str = "DOCS";
const ROOT_PAGE: &str = "1001";
const CHILD_PAGE: &str = "1002";
const ROOT_FILE: &str = "Team Handbook.md";
const CHILD_FILE: &str = "Team Handbook/Onboarding.md";

// ------------------------------------------------------------ the fixture ---

/// A Data Center instance holding a two-page space: `Team Handbook` and its
/// child `Onboarding`.
async fn dc_server() -> MockServer {
    let server = MockServer::start().await;
    mount_whoami(
        &server,
        200,
        json!({
            "type": "known",
            "username": "tester",
            "userKey": "ff8081",
            "displayName": "Test User",
            "email": "tester@corp.example",
        }),
    )
    .await;

    Mock::given(method("GET"))
        .and(path("/rest/api/space"))
        .and(query_param("spaceKey", SPACE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": 500, "key": SPACE, "name": "Documentation", "type": "global",
                "homepage": { "id": ROOT_PAGE, "type": "page", "title": "Team Handbook" }
            }],
            "size": 1, "start": 0, "limit": 1, "_links": {}
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/rest/api/content"))
        .and(query_param("spaceKey", SPACE))
        .and(query_param("type", "page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                summary(ROOT_PAGE, "Team Handbook", None, 3),
                summary(CHILD_PAGE, "Onboarding", Some(ROOT_PAGE), 1),
            ],
            "start": 0, "limit": 100, "size": 2, "_links": {}
        })))
        .mount(&server)
        .await;

    mount_page(&server, ROOT_PAGE, "Team Handbook", None, 3, "<p>Welcome to the team.</p>").await;
    mount_page(
        &server,
        CHILD_PAGE,
        "Onboarding",
        Some(ROOT_PAGE),
        1,
        "<p>First week checklist.</p>",
    )
    .await;

    for id in [ROOT_PAGE, CHILD_PAGE] {
        mount_empty_collection(&server, &format!("/rest/api/content/{id}/child/attachment")).await;
        mount_empty_collection(&server, &format!("/rest/api/content/{id}/child/comment")).await;
    }

    // CQL search, as `confed log` (no page) and `confed search` use it. The
    // server does the ordering: the child was edited most recently.
    Mock::given(method("GET"))
        .and(path("/rest/api/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                search_hit(CHILD_PAGE, "Onboarding", 1, "2026-08-30T09:00:00Z"),
                search_hit(ROOT_PAGE, "Team Handbook", 3, "2026-08-01T00:00:00Z"),
            ],
            "start": 0, "limit": 25, "size": 2, "_links": {}
        })))
        .mount(&server)
        .await;

    // `push` updates the root page; the response echoes what was sent so the
    // test can assert the edit really left the machine.
    Mock::given(method("PUT"))
        .and(path(format!("/rest/api/content/{ROOT_PAGE}")))
        .respond_with(|req: &Request| {
            let sent: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            ResponseTemplate::new(200).set_body_json(json!({
                "id": ROOT_PAGE,
                "type": "page",
                "status": "current",
                "title": sent["title"],
                "space": { "key": SPACE },
                "version": { "number": sent["version"]["number"] },
                "body": { "storage": { "value": sent["body"]["storage"]["value"] } }
            }))
        })
        .mount(&server)
        .await;

    server
}

fn summary(id: &str, title: &str, parent: Option<&str>, version: u32) -> Value {
    json!({
        "id": id,
        "type": "page",
        "status": "current",
        "title": title,
        "space": { "id": 500, "key": SPACE, "name": "Documentation" },
        "version": {
            "number": version,
            "when": "2026-08-01T00:00:00Z",
            "by": { "displayName": "Alice Ng" }
        },
        "ancestors": parent.map(|p| json!([{ "id": p, "type": "page" }])).unwrap_or(json!([])),
        "metadata": { "labels": { "results": [] } },
        "history": { "createdDate": "2026-01-01T00:00:00Z" }
    })
}

/// One `/rest/api/search` hit, shaped the way `expand=content.version` makes it.
fn search_hit(id: &str, title: &str, version: u32, when: &str) -> Value {
    json!({
        "content": {
            "id": id,
            "type": "page",
            "status": "current",
            "title": title,
            "space": { "id": 500, "key": SPACE, "name": "Documentation" },
            "version": { "number": version, "when": when, "by": { "displayName": "Alice Ng" } }
        },
        "title": title,
        "url": format!("/pages/viewpage.action?pageId={id}"),
        "lastModified": when
    })
}

async fn mount_page(
    server: &MockServer,
    id: &str,
    title: &str,
    parent: Option<&str>,
    version: u32,
    body: &str,
) {
    let mut page = summary(id, title, parent, version);
    page["body"] = json!({ "storage": { "value": body, "representation": "storage" } });
    Mock::given(method("GET"))
        .and(path(format!("/rest/api/content/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(page))
        .mount(server)
        .await;
}

async fn mount_empty_collection(server: &MockServer, route: &str) {
    Mock::given(method("GET"))
        .and(path(route.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [], "start": 0, "limit": 100, "size": 0, "_links": {}
        })))
        .mount(server)
        .await;
}

async fn mount_whoami(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path("/rest/api/user/current"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

// ------------------------------------------------------- running the tool ---

/// Every `CONFED_*` variable, cleared before each run so a developer's shell
/// cannot change what the tests exercise.
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

/// A `confed` invocation rooted at `dir`, with a clean environment and a closed
/// stdin (so a stray prompt would fail rather than hang).
fn confed(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_confed"));
    for var in ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env("NO_COLOR", "1");
    cmd.arg("-C").arg(dir);
    cmd.args(args);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

/// The same, with working Data Center credentials in the environment.
fn confed_authed(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = confed(dir, args);
    cmd.env("CONFED_TOKEN", "pat-token");
    cmd.env("CONFED_USERNAME", "tester");
    cmd
}

fn run(mut cmd: Command) -> Output {
    cmd.output().expect("running the confed binary")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("confed exited via a signal")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Parse the JSON envelope, check it against `envelope.schema.json`, and check
/// the command's `result` against its own schema when one is published.
fn envelope(output: &Output, command: &str) -> Value {
    let text = stdout(output);
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!("stdout is not JSON ({e}):\n{text}\n--- stderr ---\n{}", stderr(output))
    });

    schema::check("envelope", &value);
    assert_eq!(value["confed"]["command"], json!(command), "wrong command in the envelope");
    assert_eq!(
        value["confed"]["exit_code"],
        json!(exit_code(output)),
        "the envelope's exit_code must equal the process exit status"
    );
    if value["result"].is_object() {
        schema::check_result(command, &value["result"]);
    }
    value
}

/// `confed init` against `server`, storing the token in `.session.db` so the
/// test never touches the developer's OS keyring.
fn init(dir: &Path, server: &MockServer) -> Output {
    run(confed_authed(
        dir,
        &[
            "init",
            "--json",
            "--base-url",
            &server.uri(),
            "--flavor",
            "dc",
            "--space",
            SPACE,
            "--credential-store",
            "sqlite",
        ],
    ))
}

fn read(dir: &Path, relative: &str) -> String {
    std::fs::read_to_string(dir.join(relative))
        .unwrap_or_else(|e| panic!("reading {relative}: {e}"))
}

// ------------------------------------------------------------------ tests ---

#[tokio::test]
async fn init_creates_the_workspace_and_reports_it_in_the_envelope() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();

    let output = init(dir.path(), &server);
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));

    let value = envelope(&output, "init");
    assert_eq!(value["confed"]["schema"], json!(1));
    assert_eq!(value["confed"]["ok"], json!(true));
    assert_eq!(value["errors"], json!([]));
    assert!(value["warnings"].is_array());

    let result = &value["result"];
    assert_eq!(result["flavor"], json!("datacenter"));
    assert_eq!(result["space"]["key"], json!(SPACE));
    assert_eq!(result["space"]["name"], json!("Documentation"));
    assert_eq!(result["user"]["display_name"], json!("Test User"));
    assert_eq!(result["credential_store"], json!("sqlite"));

    for name in [".state.db", ".session.db", ".gitignore", "CLAUDE.md", "AGENTS.md"] {
        assert!(dir.path().join(name).exists(), "`confed init` did not create {name}");
    }

    // The agent contract names the space it was generated for, and the confed
    // that generated it, so an agent can tell when it has gone out of date.
    let claude = read(dir.path(), "CLAUDE.md");
    assert!(claude.contains(SPACE), "the agent contract does not name the space");
    assert_eq!(claude, read(dir.path(), "AGENTS.md"), "both agent files must be identical");
    assert!(
        claude.starts_with(&format!(
            "<!-- confed:agent-docs version={} -->",
            env!("CARGO_PKG_VERSION")
        )),
        "the agent contract is not stamped with the confed that wrote it:\n{}",
        claude.lines().next().unwrap_or_default()
    );
    assert!(
        claude.contains("confed version --changelog --since"),
        "the agent contract does not say how to read what changed after an upgrade"
    );

    // Credentials and local state must never be committed.
    let gitignore = read(dir.path(), ".gitignore");
    for entry in [".state.db", ".session.db", ".confed.lock"] {
        assert!(gitignore.contains(entry), ".gitignore is missing {entry}");
    }

    // The token must not appear in anything confed printed.
    assert!(!stdout(&output).contains("pat-token"), "the token leaked into stdout");
    assert!(!stderr(&output).contains("pat-token"), "the token leaked into stderr");
}

#[cfg(unix)]
#[tokio::test]
async fn the_session_database_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;

    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    let mode = std::fs::metadata(dir.path().join(".session.db")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "found mode {:o}", mode & 0o777);
}

#[tokio::test]
async fn pull_materializes_the_hierarchy_and_status_is_then_clean() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    let output = run(confed_authed(dir.path(), &["pull", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "pull");

    let created = value["result"]["created"].as_array().expect("created is a list");
    let mut paths: Vec<&str> = created.iter().map(|c| c["path"].as_str().unwrap()).collect();
    paths.sort_unstable();
    assert_eq!(paths, [ROOT_FILE, CHILD_FILE]);
    assert_eq!(value["result"]["dry_run"], json!(false));

    // Children live in a directory named after their parent.
    assert!(dir.path().join(ROOT_FILE).exists());
    assert!(dir.path().join(CHILD_FILE).exists());

    let child = read(dir.path(), CHILD_FILE);
    assert!(child.contains("title: Onboarding"), "{child}");
    assert!(child.contains("First week checklist"), "{child}");
    assert!(child.contains("page_id: '1002'"), "the managed block records the page id: {child}");

    let output = run(confed_authed(dir.path(), &["status", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "status");
    assert_eq!(value["result"]["clean"], json!(true), "a fresh pull must leave a clean tree");
    assert_eq!(value["result"]["space"], json!(SPACE));
    assert_eq!(value["result"]["pages"].as_array().unwrap().len(), 2);
    for page in value["result"]["pages"].as_array().unwrap() {
        assert_eq!(page["state"], json!("unchanged"), "{page}");
    }
}

#[tokio::test]
async fn log_without_a_page_reports_recent_activity_across_the_space() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    let output = run(confed_authed(dir.path(), &["log", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "log");

    let result = &value["result"];
    assert_eq!(result["space"], json!(SPACE));
    assert_eq!(
        result["cql"],
        json!("space = \"DOCS\" and type = page order by lastmodified desc"),
        "the server must do the ordering, so --limit means the N most recent"
    );

    let pages = result["pages"].as_array().expect("pages is a list");
    assert_eq!(pages.len(), 2);
    // Newest first, as the server returned them.
    assert_eq!(pages[0]["title"], json!("Onboarding"));
    assert_eq!(pages[0]["page_id"], json!(CHILD_PAGE));
    assert_eq!(pages[0]["when"], json!("2026-08-30T09:00:00Z"));
    assert_eq!(pages[0]["author"], json!("Alice Ng"), "expanded version, not a bare stub");
    assert_eq!(pages[0]["version"], json!(1));
    assert_eq!(pages[0]["local_path"], json!(CHILD_FILE), "a pulled page says where to edit it");
    assert_eq!(pages[1]["title"], json!("Team Handbook"));

    // Version and author only arrive when search is asked to expand them.
    let requests = server.received_requests().await.unwrap_or_default();
    let search = requests
        .iter()
        .find(|r| r.url.path() == "/rest/api/search")
        .expect("confed log queried the search endpoint");
    let expand = search
        .url
        .query_pairs()
        .find(|(k, _)| k == "expand")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    assert!(expand.contains("content.version"), "search was not expanded: {expand}");

    // The human rendering names the page and where it lives.
    let output = run(confed_authed(dir.path(), &["log"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Onboarding"), "{text}");
    assert!(text.contains(CHILD_FILE), "{text}");
}

#[tokio::test]
async fn a_local_log_spans_the_space_and_needs_no_credentials() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    // Something worth logging: an edit pushed back to the root page.
    let mut content = read(dir.path(), ROOT_FILE);
    content.push_str("\nAn extra paragraph.\n");
    std::fs::write(dir.path().join(ROOT_FILE), content).unwrap();
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["push", "--json"]))), 0);

    // No token in the environment: --local must answer from `.state.db` alone.
    let output = run(confed(dir.path(), &["log", "--local", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "log");

    let result = &value["result"];
    assert_eq!(result["space"], json!(SPACE));
    let entries = result["entries"].as_array().expect("entries is a list");
    assert!(
        entries.iter().any(|e| e["op"] == json!("push-update") && e["page"] == json!(ROOT_FILE)),
        "a space-wide log says which page each entry is about: {entries:?}"
    );

    // Scoped back down to one page, the entries are only that page's.
    let output = run(confed(dir.path(), &["log", CHILD_FILE, "--local", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "log");
    assert_eq!(value["result"]["page_id"], json!(CHILD_PAGE));
    assert!(value["result"]["space"].is_null(), "a page log is not a space log");
    for entry in value["result"]["entries"].as_array().unwrap() {
        assert_eq!(entry["page_id"], json!(CHILD_PAGE), "{entry}");
    }
}

#[tokio::test]
async fn an_edited_page_shows_as_modified_and_diff_signals_it_with_exit_code_10() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    let mut content = read(dir.path(), ROOT_FILE);
    content.push_str("\nAn extra paragraph.\n");
    std::fs::write(dir.path().join(ROOT_FILE), content).unwrap();

    let output = run(confed_authed(dir.path(), &["status", "--json"]));
    let value = envelope(&output, "status");
    assert_eq!(value["result"]["clean"], json!(false));
    let edited = value["result"]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == json!(ROOT_FILE))
        .expect("the edited page is listed");
    assert_eq!(edited["state"], json!("modified"));
    assert_eq!(edited["base_version"], json!(3));
    assert_eq!(edited["remote_version"], json!(3));

    // `diff` is local-only, so it needs no credentials at all.
    let output = run(confed(dir.path(), &["diff", "--exit-code"]));
    assert_eq!(exit_code(&output), 10, "differences must exit 10: {}", stderr(&output));
    assert!(stdout(&output).contains("An extra paragraph"), "{}", stdout(&output));

    // Without --exit-code, a diff is still a success.
    let output = run(confed(dir.path(), &["diff", "--json"]));
    assert_eq!(exit_code(&output), 0);
    let value = envelope(&output, "diff");
    let pages = value["result"]["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["path"], json!(ROOT_FILE));
    assert!(pages[0]["additions"].as_u64().unwrap() > 0);

    // And a clean tree exits 0 even with --exit-code.
    let output = run(confed(dir.path(), &["diff", "--exit-code", CHILD_FILE]));
    assert_eq!(exit_code(&output), 0, "an unmodified page has no differences");
}

#[tokio::test]
async fn a_dry_run_push_reports_the_page_without_sending_anything() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    let mut content = read(dir.path(), ROOT_FILE);
    content.push_str("\nAn extra paragraph.\n");
    std::fs::write(dir.path().join(ROOT_FILE), content).unwrap();

    let output = run(confed_authed(dir.path(), &["push", "--dry-run", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "push");
    assert_eq!(value["result"]["dry_run"], json!(true));
    let pushed = value["result"]["pushed"].as_array().unwrap();
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0]["path"], json!(ROOT_FILE));
    assert_eq!(pushed[0]["from_version"], json!(3));
    assert_eq!(pushed[0]["to_version"], json!(4), "a dry run predicts the next version");
    assert_eq!(pushed[0]["ops"], json!(["body"]));

    assert_eq!(
        mutations(&server).await,
        Vec::<String>::new(),
        "--dry-run must not mutate anything"
    );

    // The real push does send it.
    let output = run(confed_authed(dir.path(), &["push", "--json", "-m", "from the test"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "push");
    assert_eq!(value["result"]["dry_run"], json!(false));
    let pushed = value["result"]["pushed"].as_array().unwrap();
    assert_eq!(pushed.len(), 1);
    assert_eq!(pushed[0]["to_version"], json!(4));

    assert_eq!(
        mutations(&server).await,
        vec![format!("PUT /rest/api/content/{ROOT_PAGE}")],
        "push sends exactly one update"
    );

    let sent = last_body(&server, "PUT").await;
    assert_eq!(sent["version"]["number"], json!(4), "push sends base + 1");
    assert_eq!(sent["version"]["message"], json!("from the test"));
    let storage = sent["body"]["storage"]["value"].as_str().unwrap();
    assert!(storage.contains("An extra paragraph"), "the edit reached the server: {storage}");
    assert!(storage.contains("Welcome to the team"), "untouched content survives: {storage}");

    // After a successful push the tree is clean again and the base has advanced.
    let value = envelope(&run(confed_authed(dir.path(), &["status", "--json"])), "status");
    assert_eq!(value["result"]["clean"], json!(true));
    assert!(read(dir.path(), ROOT_FILE).contains("version: 4"));
}

/// `attach --rm` must never report a removal it did not make. With `--push` it
/// deletes on the server; without it, it says the deletion is only staged.
#[tokio::test]
async fn attach_rm_deletes_on_the_server_or_says_it_did_not() {
    let server = dc_server().await;
    Mock::given(method("POST"))
        .and(path(format!("/rest/api/content/{ROOT_PAGE}/child/attachment")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": "att-9", "type": "attachment", "title": "diagram.png",
                "version": { "number": 1 },
                "extensions": { "fileSize": 7, "mediaType": "image/png" }
            }],
            "size": 1
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/rest/api/content/att-9"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    // Removing something that is not an attachment is an error, not a success.
    let output = run(confed_authed(dir.path(), &["attach", ROOT_FILE, "--rm", "ghost.png"]));
    assert_ne!(exit_code(&output), 0, "a no-op must not exit 0: {}", stdout(&output));
    assert!(stderr(&output).contains("ghost.png"), "stderr: {}", stderr(&output));

    let source = dir.path().join("diagram.png");
    std::fs::write(&source, b"content").unwrap();
    let output = run(confed_authed(
        dir.path(),
        &["attach", ROOT_FILE, source.to_str().unwrap(), "--push", "--json"],
    ));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "attach");
    assert_eq!(
        value["result"]["push"]["attachments_uploaded"],
        json!([".Team Handbook/diagram.png"]),
        "push names the file it uploaded"
    );

    // Without --push the file goes locally and the report says only that.
    let output =
        run(confed_authed(dir.path(), &["attach", ROOT_FILE, "--rm", "diagram.png", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "attach");
    assert_eq!(value["result"]["removed_locally"], json!(true));
    assert_eq!(value["result"]["removed_on_server"], json!(false));
    assert_eq!(value["result"]["staged"], json!(true));
    assert!(
        !mutations(&server).await.contains(&"DELETE /rest/api/content/att-9".to_string()),
        "nothing was deleted yet"
    );

    // A plain push reports the refusal rather than exiting clean.
    let value = envelope(&run(confed_authed(dir.path(), &["push", "--json"])), "push");
    assert!(value["result"]["attachments_deleted"].as_array().unwrap().is_empty());
    let blocked = value["result"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["path"] == json!(".Team Handbook/diagram.png"))
        .unwrap_or_else(|| panic!("the refusal is reported: {}", value["result"]));
    assert!(blocked["reason"].as_str().unwrap().contains("--allow-delete"));

    // And `--allow-delete` applies it, as the message said it would.
    let value =
        envelope(&run(confed_authed(dir.path(), &["push", "--allow-delete", "--json"])), "push");
    assert_eq!(
        value["result"]["attachments_deleted"],
        json!([".Team Handbook/diagram.png"]),
        "the deletion finally happens and is named"
    );
    assert!(
        mutations(&server).await.contains(&"DELETE /rest/api/content/att-9".to_string()),
        "the request really left the machine"
    );

    // With --push, --rm does the whole job itself.
    assert_eq!(
        exit_code(&run(confed_authed(
            dir.path(),
            &["attach", ROOT_FILE, source.to_str().unwrap(), "--push", "--json"]
        ))),
        0
    );
    let before = mutations(&server).await.len();
    let output = run(confed_authed(
        dir.path(),
        &["attach", ROOT_FILE, "--rm", "diagram.png", "--push", "--json"],
    ));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "attach");
    assert_eq!(value["result"]["removed_on_server"], json!(true));
    assert_eq!(value["result"]["staged"], json!(false));
    assert_eq!(
        value["result"]["push"]["attachments_deleted"],
        json!([".Team Handbook/diagram.png"])
    );
    assert!(mutations(&server).await.len() > before, "--push made a request");
    assert!(
        !dir.path().join(".Team Handbook/diagram.png").exists(),
        "and the local copy is gone too"
    );
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

async fn last_body(server: &MockServer, http_method: &str) -> Value {
    let requests = server.received_requests().await.unwrap_or_default();
    let request = requests
        .iter()
        .rev()
        .find(|r| r.method.as_str() == http_method)
        .unwrap_or_else(|| panic!("no {http_method} request was made"));
    serde_json::from_slice(&request.body).expect("the request body is JSON")
}

/// A file whose `page_id` has no base record is `untracked`: confed has nothing
/// to compare it against, so it cannot tell whether the file holds unpushed
/// work. Rebuilding `.state.db` — what a fresh clone of a repository that
/// (correctly) does not track it looks like — must not cost you those edits.
#[tokio::test]
async fn pull_refuses_to_overwrite_an_untracked_file() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    let mut content = read(dir.path(), ROOT_FILE);
    content.push_str("\nWork I have not pushed yet.\n");
    std::fs::write(dir.path().join(ROOT_FILE), content).unwrap();

    std::fs::remove_file(dir.path().join(".state.db")).unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    let value = envelope(&run(confed(dir.path(), &["status", "--json"])), "status");
    let page = value["result"]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == json!(ROOT_FILE))
        .expect("the file is still listed");
    assert_eq!(page["state"], json!("untracked"));

    let output = run(confed_authed(dir.path(), &["pull", "--json"]));
    assert_eq!(exit_code(&output), 7, "pull stops rather than overwriting: {}", stderr(&output));
    assert!(
        read(dir.path(), ROOT_FILE).contains("Work I have not pushed yet"),
        "the local edit survives"
    );

    // --force is the explicit way through, and it takes the server's copy.
    let forced = run(confed_authed(dir.path(), &["pull", "--force", "--json"]));
    assert_eq!(exit_code(&forced), 0, "stderr: {}", stderr(&forced));
    assert!(!read(dir.path(), ROOT_FILE).contains("Work I have not pushed yet"));
}

#[tokio::test]
async fn commands_outside_a_workspace_exit_7_and_say_how_to_fix_it() {
    let dir = tempfile::tempdir().unwrap();

    let output = run(confed(dir.path(), &["status", "--json"]));
    assert_eq!(exit_code(&output), 7, "stderr: {}", stderr(&output));

    let value = envelope(&output, "status");
    assert_eq!(value["confed"]["ok"], json!(false));
    assert_eq!(value["result"], Value::Null);
    let error = &value["errors"][0];
    assert_eq!(error["code"], json!("STATE"));
    assert!(error["message"].as_str().unwrap().contains(".state.db"), "{error}");
    assert!(error["hint"].as_str().unwrap().contains("confed init"), "{error}");

    // The human rendering says the same thing on stderr, and prints nothing on stdout.
    let output = run(confed(dir.path(), &["status"]));
    assert_eq!(exit_code(&output), 7);
    assert!(stdout(&output).is_empty(), "errors belong on stderr");
    assert!(stderr(&output).contains("error:"), "{}", stderr(&output));
    assert!(stderr(&output).contains("hint:"), "{}", stderr(&output));
}

#[tokio::test]
async fn a_missing_value_fails_fast_with_exit_2_instead_of_prompting() {
    let dir = tempfile::tempdir().unwrap();

    // No CONFED_TOKEN, no --token, no TTY: this must fail, not wait for input.
    let output = run(confed(dir.path(), &["--json", "whoami"]));
    assert_eq!(exit_code(&output), 2, "stderr: {}", stderr(&output));

    let value = envelope(&output, "whoami");
    let error = &value["errors"][0];
    assert_eq!(error["code"], json!("USAGE"));
    assert!(error["message"].as_str().unwrap().contains("API token"), "{error}");
    let hint = error["hint"].as_str().unwrap();
    assert!(hint.contains("--token"), "the hint names the flag: {hint}");
    assert!(hint.contains("CONFED_TOKEN"), "the hint names the env var: {hint}");

    // `--non-interactive` without `--json` behaves the same, on stderr.
    let output = run(confed(dir.path(), &["--non-interactive", "whoami"]));
    assert_eq!(exit_code(&output), 2);
    assert!(stderr(&output).contains("CONFED_TOKEN"), "{}", stderr(&output));

    // `init` reports the missing base URL the same way.
    let output = run(confed(dir.path(), &["init", "--json"]));
    assert_eq!(exit_code(&output), 2, "stderr: {}", stderr(&output));
    let value = envelope(&output, "init");
    assert!(
        value["errors"][0]["hint"].as_str().unwrap().contains("CONFED_BASE_URL"),
        "{}",
        value["errors"][0]
    );
}

/// The TUI is the one command with no non-interactive fallback: without a
/// terminal it must fail like any other usage error, not draw into a pipe.
#[tokio::test]
async fn the_tui_refuses_to_run_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(confed(dir.path(), &["tui"]));

    assert_eq!(exit_code(&output), 2, "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty(), "nothing may be drawn to a pipe");
    let stderr = stderr(&output);
    assert!(stderr.contains("interactive terminal"), "{stderr}");
    assert!(stderr.contains("hint:"), "{stderr}");

    let value = envelope(&run(confed(dir.path(), &["tui", "--json"])), "tui");
    assert_eq!(value["errors"][0]["code"], json!("USAGE"));
}

#[tokio::test]
async fn a_rejected_token_exits_3_and_leaves_no_workspace_behind() {
    let server = MockServer::start().await;
    mount_whoami(&server, 401, json!({ "message": "PAT rejected" })).await;

    let dir = tempfile::tempdir().unwrap();
    let output = init(dir.path(), &server);
    assert_eq!(exit_code(&output), 3, "stderr: {}", stderr(&output));

    let value = envelope(&output, "init");
    assert_eq!(value["confed"]["ok"], json!(false));
    let error = &value["errors"][0];
    assert_eq!(error["code"], json!("AUTH"));
    assert!(error["hint"].as_str().unwrap().contains("CONFED_TOKEN"), "{error}");

    assert!(
        !dir.path().join(".state.db").exists(),
        "a failed init must not leave a half-built workspace"
    );
}

#[tokio::test]
async fn help_and_a_bare_invocation_behave_like_a_normal_cli() {
    let dir = tempfile::tempdir().unwrap();

    let output = run(confed(dir.path(), &["--help"]));
    assert_eq!(exit_code(&output), 0);
    let help = stdout(&output);
    for command in ["init", "clone", "pull", "push", "status", "diff", "resolve", "doctor"] {
        assert!(help.contains(command), "`--help` does not mention `{command}`");
    }

    // No subcommand at all is a usage error, and clap says so on stderr.
    let output = run(Command::new(env!("CARGO_BIN_EXE_confed")));
    assert_eq!(exit_code(&output), 2, "a bare invocation is a usage error");
    assert!(stderr(&output).contains("Usage"), "{}", stderr(&output));

    let output = run(confed(dir.path(), &["--version"]));
    assert_eq!(exit_code(&output), 0);
    assert!(stdout(&output).starts_with("confed "), "{}", stdout(&output));
}

#[tokio::test]
async fn version_reports_the_compatibility_contract_and_the_release_notes() {
    // Deliberately not a workspace: an agent must be able to ask what confed is
    // before it has one.
    let dir = tempfile::tempdir().unwrap();

    let output = run(confed(dir.path(), &["version", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "version");
    let result = &value["result"];
    assert_eq!(result["version"], json!(env!("CARGO_PKG_VERSION")));
    assert_eq!(result["version"], value["confed"]["version"]);
    assert_eq!(result["json_schema"], value["confed"]["schema"]);
    assert!(result["state_schema"].as_u64().unwrap() >= 1);
    assert_eq!(result["changelog"], json!([]), "the notes are printed only on request");

    // --changelog answers from notes compiled into the binary, so it works with
    // no workspace, no network and no checkout.
    let output = run(confed(dir.path(), &["version", "--changelog", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "version");
    let entry = &value["result"]["changelog"][0];
    assert_eq!(entry["version"], json!(env!("CARGO_PKG_VERSION")), "{}", value["result"]);
    assert!(entry["date"].is_string(), "a released section carries its date");
    assert!(!entry["notes"].as_str().unwrap().is_empty());

    // Nothing has been released after the build being tested.
    let output =
        run(confed(dir.path(), &["version", "--changelog", "--since", env!("CARGO_PKG_VERSION")]));
    assert_eq!(exit_code(&output), 0);
    assert!(stderr(&output).contains("nothing newer"), "{}", stderr(&output));

    // A --since that is not a version is a usage error, not an empty answer.
    let output =
        run(confed(dir.path(), &["version", "--changelog", "--since", "latest", "--json"]));
    assert_eq!(exit_code(&output), 2);
    assert_eq!(envelope(&output, "version")["errors"][0]["code"], json!("USAGE"));
}

#[tokio::test]
async fn doctor_reports_an_agent_contract_written_by_another_confed_and_fixes_it() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    // What an upgrade leaves behind: a contract describing an older confed.
    let stale = format!(
        "<!-- confed:agent-docs version=0.0.1 -->\n# confed workspace\n\nrules for {SPACE}\n"
    );
    std::fs::write(dir.path().join("CLAUDE.md"), &stale).unwrap();

    let output = run(confed_authed(dir.path(), &["doctor", "--json"]));
    let value = envelope(&output, "doctor");
    let check = value["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("agent docs"))
        .expect("doctor has no agent docs check");
    assert_eq!(check["status"], json!("warn"), "{check}");
    let detail = check["detail"].as_str().unwrap();
    assert!(detail.contains("0.0.1") && detail.contains(env!("CARGO_PKG_VERSION")), "{detail}");
    assert_eq!(read(dir.path(), "CLAUDE.md"), stale, "doctor without --fix must not write");

    let output = run(confed_authed(dir.path(), &["doctor", "--fix", "--json"]));
    let value = envelope(&output, "doctor");
    let check = value["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("agent docs"))
        .expect("doctor has no agent docs check");
    assert_eq!(check["fix_applied"], json!(true), "{check}");
    let rewritten = read(dir.path(), "CLAUDE.md");
    assert!(
        rewritten.starts_with(&format!(
            "<!-- confed:agent-docs version={} -->",
            env!("CARGO_PKG_VERSION")
        )),
        "--fix did not restamp the contract:\n{}",
        rewritten.lines().next().unwrap_or_default()
    );
    assert_eq!(rewritten, read(dir.path(), "AGENTS.md"));
}

#[tokio::test]
async fn whoami_reports_the_server_capabilities() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    let output = run(confed_authed(dir.path(), &["whoami", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));

    let value = envelope(&output, "whoami");
    assert_eq!(value["result"]["flavor"], json!("datacenter"));
    assert_eq!(value["result"]["user"]["display_name"], json!("Test User"));
    assert_eq!(
        value["result"]["capabilities"]["inline_comment_create"],
        json!(true),
        "Data Center creates inline comments through its plugin API"
    );
}

#[tokio::test]
async fn fetch_refreshes_remote_state_without_touching_working_files() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    let output = run(confed_authed(dir.path(), &["fetch", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));

    let value = envelope(&output, "fetch");
    assert_eq!(value["result"]["fetched"], json!(2));
    assert_eq!(value["result"]["failed"], json!([]));

    assert!(!dir.path().join(ROOT_FILE).exists(), "`fetch` must not write page files");

    // The pages are now known but not materialized.
    let value = envelope(&run(confed(dir.path(), &["status", "--json"])), "status");
    assert_eq!(value["result"]["clean"], json!(false));
    for page in value["result"]["pages"].as_array().unwrap() {
        assert_eq!(page["state"], json!("remote_new"), "{page}");
    }
}

/// Progress is a courtesy for someone watching a terminal. Redirected output —
/// a pipe, a log file, CI — must stay free of carriage returns and escape
/// codes, and `--silent` must hold even if that ever changes.
#[tokio::test]
async fn progress_never_pollutes_redirected_output() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    for args in [
        vec!["pull", "--json"],
        vec!["pull", "--silent", "--json"],
        vec!["pull", "--silent"],
        vec!["fetch"],
    ] {
        let output = run(confed_authed(dir.path(), &args));
        assert_eq!(exit_code(&output), 0, "{args:?}: {}", stderr(&output));

        let noise = stderr(&output);
        assert!(!noise.contains('\r'), "{args:?} wrote a progress line: {noise:?}");
        assert!(!noise.contains("\x1b["), "{args:?} wrote escape codes: {noise:?}");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains('\r'), "{args:?} put progress on stdout: {stdout:?}");
    }
}

/// `--silent` is accepted everywhere, including alongside the flags it overlaps.
#[tokio::test]
async fn silent_is_accepted_with_the_flags_it_overlaps() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    for args in [
        vec!["pull", "--silent", "--quiet"],
        vec!["pull", "--silent", "--dry-run", "--json"],
        vec!["status", "--silent"],
    ] {
        let output = run(confed_authed(dir.path(), &args));
        assert_eq!(exit_code(&output), 0, "{args:?}: {}", stderr(&output));
    }
}

/// `pull` leaves the page's Confluence markup in the sidecar, and `diff
/// --conf-format` compares that markup rather than the Markdown.
#[tokio::test]
async fn confluence_markup_is_saved_and_can_be_diffed() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    // The sidecar holds the markup Confluence stores, not a rendering of it.
    let markup = read(dir.path(), ".Team Handbook/storage.xml");
    assert!(markup.contains('<'), "expected Confluence markup, got: {markup:?}");

    let mut content = read(dir.path(), ROOT_FILE);
    content.push_str("\nA sentence added locally.\n");
    std::fs::write(dir.path().join(ROOT_FILE), content).unwrap();

    // The Markdown diff shows Markdown; the markup diff shows tags.
    let markdown = run(confed(dir.path(), &["diff"]));
    assert_eq!(exit_code(&markdown), 0, "stderr: {}", stderr(&markdown));
    let markdown = String::from_utf8_lossy(&markdown.stdout).to_string();
    assert!(markdown.contains("A sentence added locally"));
    assert!(!markdown.contains("<p>"), "the default diff is Markdown: {markdown}");

    for flag in ["--conf-format", "--storage"] {
        let output = run(confed(dir.path(), &["diff", flag]));
        assert_eq!(exit_code(&output), 0, "{flag}: {}", stderr(&output));
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            text.contains("<p>A sentence added locally.</p>"),
            "{flag} should diff Confluence markup, got: {text}"
        );
    }
}

/// `config --no-keychain` keeps the credential in the database, where reading
/// it never prompts, and `--force-keychain` asks for it to go back.
#[tokio::test]
async fn the_credential_store_can_be_switched_from_the_command_line() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);

    let output = run(confed(dir.path(), &["config", "--no-keychain", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "config");
    assert_eq!(value["result"]["credential_store"], json!("sqlite"));

    // Asking twice is a no-op rather than an error.
    let again = run(confed(dir.path(), &["config", "--no-keychain", "--json"]));
    assert_eq!(exit_code(&again), 0);
    assert_eq!(envelope(&again, "config")["result"]["changed"], json!(false));

    // With the credential in the database, ordinary commands still authenticate
    // without any token in the environment.
    let pull = run(confed(dir.path(), &["pull", "--json"]));
    assert_eq!(exit_code(&pull), 0, "stderr: {}", stderr(&pull));
    assert!(read(dir.path(), ROOT_FILE).contains("Team Handbook"));

    // And the token never appears in output.
    for output in [&output, &again, &pull] {
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), stderr(output));
        assert!(!text.contains("pat-token"), "the token leaked into output");
    }
}

/// Switching stores needs a workspace, and says so.
#[tokio::test]
async fn switching_the_credential_store_outside_a_workspace_explains_itself() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(confed(dir.path(), &["config", "--no-keychain", "--json"]));
    assert_eq!(exit_code(&output), 7);
    let value = envelope(&output, "config");
    assert!(
        value["errors"][0]["hint"].as_str().unwrap_or_default().contains("confed init"),
        "got {value}"
    );
}

/// `confed mkdocs` scaffolds a site over the pulled pages without copying them.
#[tokio::test]
async fn mkdocs_scaffolds_a_site_over_the_workspace() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);

    let output = run(confed(dir.path(), &["mkdocs", "--json"]));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "mkdocs");

    for name in ["mkdocs.yml", "pyproject.toml", "Makefile"] {
        assert!(dir.path().join(name).exists(), "{name} was not generated");
    }
    assert!(value["result"]["pages_in_nav"].as_u64().unwrap() >= 1);

    // docs/ points back at the pages rather than copying them, so a pull is
    // enough to update the site.
    let link = dir.path().join("docs").join(ROOT_FILE);
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink(), "docs/ holds symlinks");
    assert_eq!(
        std::fs::read_to_string(&link).unwrap(),
        read(dir.path(), ROOT_FILE),
        "the link resolves to the real page"
    );

    // The navigation follows confed's hierarchy, and dependencies go through uv.
    let config = read(dir.path(), "mkdocs.yml");
    assert!(config.contains("docs_dir: docs"));
    assert!(config.contains("nav:"));
    assert!(read(dir.path(), "Makefile").contains("$(UV) run mkdocs serve"));
    assert!(read(dir.path(), "pyproject.toml").contains("mkdocs-material"));

    // The build output stays out of git.
    let gitignore = read(dir.path(), ".gitignore");
    for entry in ["site/", ".venv/", "docs/"] {
        assert!(gitignore.contains(entry), "{entry} is not ignored");
    }
}

/// Generated files are not clobbered unless asked, and --force refreshes them.
#[tokio::test]
async fn mkdocs_leaves_edited_config_alone_without_force() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull", "--json"]))), 0);
    assert_eq!(exit_code(&run(confed(dir.path(), &["mkdocs", "--json"]))), 0);

    std::fs::write(dir.path().join("mkdocs.yml"), "site_name: Mine\n").unwrap();

    let output = run(confed(dir.path(), &["mkdocs", "--json"]));
    assert_eq!(exit_code(&output), 0);
    let value = envelope(&output, "mkdocs");
    assert!(
        value["result"]["skipped"].as_array().unwrap().iter().any(|s| s == "mkdocs.yml"),
        "an existing config is reported as skipped: {value}"
    );
    assert_eq!(read(dir.path(), "mkdocs.yml"), "site_name: Mine\n", "the edit survives");

    let forced = run(confed(dir.path(), &["mkdocs", "--force", "--json"]));
    assert_eq!(exit_code(&forced), 0);
    assert!(read(dir.path(), "mkdocs.yml").contains("docs_dir: docs"), "--force regenerates");
}

#[tokio::test]
async fn an_inline_anchor_must_name_one_place_before_anything_is_sent() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull"]))), 0);
    let before = read(dir.path(), ROOT_FILE);

    // "e" occurs many times in "Welcome to the team."
    let output = run(confed_authed(
        dir.path(),
        &["comment", "add", ROOT_FILE, "--anchor", "e", "-m", "x", "--push", "--json"],
    ));
    assert_eq!(exit_code(&output), 2, "ambiguous anchor: {}", stderr(&output));

    let output = run(confed_authed(
        dir.path(),
        &["comment", "add", ROOT_FILE, "--anchor", "not on the page", "-m", "x", "--push"],
    ));
    assert_eq!(exit_code(&output), 6, "missing anchor: {}", stderr(&output));

    assert!(mutations(&server).await.is_empty(), "nothing was sent");
    assert_eq!(read(dir.path(), ROOT_FILE), before, "nothing was written");
}

/// The acceptance run, against a server replaying what DC 9.5.4 answered:
/// the comment is created through the inline-comment API, the version it
/// saved is adopted, and the tree is clean afterwards.
#[tokio::test]
async fn an_inline_comment_on_data_center_leaves_the_tree_clean() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull"]))), 0);

    let fixtures = format!("{}/../confed-dc/tests/fixtures/dc-inline", env!("CARGO_MANIFEST_DIR"));
    let mut created: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{fixtures}/create.response.json")).unwrap(),
    )
    .unwrap();
    created["originalSelection"] = json!("Welcome");
    let marker = created["markerRef"].as_str().unwrap().to_string();

    Mock::given(method("GET"))
        .and(path("/rest/applinks/1.0/manifest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<manifest><version>9.5.4</version></manifest>"),
        )
        .mount(&server)
        .await;

    // Before the POST the page is version 3; after it, version 4 with the
    // selection wrapped in the new marker — what Data Center does.
    let posted = Arc::new(AtomicBool::new(false));
    let seen = posted.clone();
    let wrapped = format!(
        "<p><ac:inline-comment-marker ac:ref=\"{marker}\">Welcome</ac:inline-comment-marker> to the team.</p>"
    );
    Mock::given(method("GET"))
        .and(path(format!("/rest/api/content/{ROOT_PAGE}")))
        .respond_with(move |_: &Request| {
            let (version, body) = if seen.load(Ordering::SeqCst) {
                (4, wrapped.clone())
            } else {
                (3, "<p>Welcome to the team.</p>".to_string())
            };
            let mut page = summary(ROOT_PAGE, "Team Handbook", None, version);
            page["body"] = json!({ "storage": { "value": body, "representation": "storage" } });
            ResponseTemplate::new(200).set_body_json(page)
        })
        .with_priority(1)
        .mount(&server)
        .await;
    let flag = posted.clone();
    Mock::given(method("POST"))
        .and(path("/rest/inlinecomments/1.0/comments"))
        .respond_with(move |req: &Request| {
            let sent: Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(sent["originalSelection"], json!("Welcome"));
            assert_eq!(
                (sent["matchIndex"].clone(), sent["numMatches"].clone()),
                (json!(0), json!(1))
            );
            assert_eq!(sent["containerVersion"], json!("3"));
            flag.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(created.clone())
        })
        .expect(1)
        .mount(&server)
        .await;

    let output = run(confed_authed(
        dir.path(),
        &["comment", "add", ROOT_FILE, "--anchor", "Welcome", "-m", "test", "--push", "--json"],
    ));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr(&output));
    let value = envelope(&output, "comment");
    let posted = &value["result"]["comments"][0];
    assert_eq!(posted["id"], json!("900000099220"), "{value}");
    assert_eq!(posted["kind"], json!("inline"));
    assert_eq!(posted["marker_ref"], json!(marker));
    assert_eq!(posted["anchor"], json!("Welcome"));

    let file = read(dir.path(), ROOT_FILE);
    assert!(file.contains("<!--c 900000099220 "), "the mark carries the real id: {file}");

    let status = envelope(&run(confed_authed(dir.path(), &["status", "--json"])), "status");
    assert_eq!(status["result"]["clean"], json!(true), "{status}");

    let before = mutations(&server).await.len();
    let dry = run(confed_authed(dir.path(), &["push", "--dry-run", "--json"]));
    assert_eq!(exit_code(&dry), 0, "stderr: {}", stderr(&dry));
    assert_eq!(mutations(&server).await.len(), before, "a dry run sends nothing");
    let dry = envelope(&dry, "push");
    assert!(
        dry["result"]["pushed"].as_array().is_none_or(Vec::is_empty),
        "nothing to upload: {dry}"
    );

    let list = envelope(
        &run(confed_authed(dir.path(), &["comment", "list", ROOT_FILE, "--json"])),
        "comment",
    );
    let listed = &list["result"]["comments"][0];
    assert_eq!(listed["kind"], json!("inline"), "{list}");
    assert_eq!(listed["anchor"]["text"], json!("Welcome"));
}

/// Like git, a page path is relative to the current directory; one that names
/// nothing there is read from the workspace root. A miss says where it looked
/// and suggests the closest page.
#[tokio::test]
async fn page_paths_resolve_from_the_current_directory() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull"]))), 0);
    let sub = dir.path().join("Team Handbook");

    for reference in ["Onboarding.md", "Onboarding", "./Onboarding.md", CHILD_FILE, "1002"] {
        let output = run(confed_authed(&sub, &["comment", "list", reference, "--json"]));
        assert_eq!(exit_code(&output), 0, "{reference}: {}", stderr(&output));
        assert_eq!(envelope(&output, "comment")["result"]["page_id"], json!(CHILD_PAGE));
    }
    let output = run(confed_authed(&sub, &["comment", "list", "../Team Handbook.md", "--json"]));
    assert_eq!(envelope(&output, "comment")["result"]["page_id"], json!(ROOT_PAGE));

    let output = run(confed_authed(&sub, &["comment", "list", "Onbaording.md"]));
    assert_eq!(exit_code(&output), 6);
    let err = stderr(&output);
    assert!(err.contains("Team Handbook/Onbaording.md"), "says where it looked: {err}");
    assert!(err.contains("did you mean `Team Handbook/Onboarding.md`"), "suggests: {err}");
    let output = run(confed_authed(&sub, &["comment", "list", "Onbaording.md"]));
    assert!(stderr(&output).contains("did you mean"), "a swap: {}", stderr(&output));
    let output = run(confed_authed(dir.path(), &["comment", "list", "Taem Handbook.md"]));
    assert!(
        stderr(&output).contains("did you mean `Team Handbook.md`"),
        "a swap at the root: {}",
        stderr(&output)
    );

    // Scopes too: `diff Onboarding.md` from the subdirectory is that page.
    std::fs::write(
        sub.join("Onboarding.md"),
        read(dir.path(), CHILD_FILE).replace("First week checklist.", "Edited."),
    )
    .unwrap();
    let output = run(confed_authed(&sub, &["diff", "--name-only", "Onboarding.md"]));
    assert!(stdout(&output).contains(CHILD_FILE), "{}", stdout(&output));
}

/// After an upgrade, the agent guide describes the old confed; every command
/// says so, in the JSON warnings an agent reads, until doctor rewrites it.
#[tokio::test]
async fn a_stale_agent_guide_is_flagged_on_every_command() {
    let server = dc_server().await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    let guide = dir.path().join("CLAUDE.md");
    let text = std::fs::read_to_string(&guide).expect("init writes CLAUDE.md");
    let first = text.lines().next().unwrap().to_string();
    std::fs::write(&guide, text.replacen(&first, "<!-- confed:agent-docs version=0.4.0 -->", 1))
        .unwrap();

    let value = envelope(&run(confed_authed(dir.path(), &["status", "--json"])), "status");
    let warnings = value["warnings"].to_string();
    assert!(warnings.contains("confed 0.4.0") && warnings.contains("doctor --fix"), "{value}");

    run(confed_authed(dir.path(), &["doctor", "--fix"]));
    let value = envelope(&run(confed_authed(dir.path(), &["status", "--json"])), "status");
    assert_eq!(value["warnings"], json!([]), "{value}");
}

/// A marker in the page that no comment claims is listed, not hidden.
#[tokio::test]
async fn markers_without_a_comment_are_reported() {
    let server = dc_server().await;
    Mock::given(method("GET"))
        .and(path(format!("/rest/api/content/{CHILD_PAGE}")))
        .respond_with({
            let mut page = summary(CHILD_PAGE, "Onboarding", Some(ROOT_PAGE), 1);
            page["body"] = json!({ "storage": { "value": "<p>First <ac:inline-comment-marker ac:ref=\"gone-1\">week</ac:inline-comment-marker> checklist.</p>", "representation": "storage" } });
            ResponseTemplate::new(200).set_body_json(page)
        })
        .with_priority(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull"]))), 0);

    let value = envelope(
        &run(confed_authed(dir.path(), &["comment", "list", CHILD_FILE, "--json"])),
        "comment",
    );
    assert_eq!(value["result"]["orphan_markers"], json!([{ "ref": "gone-1", "text": "week" }]));
}

/// `rm --dry-run` changes nothing; `rm --push` deletes the pages named and
/// nothing else — another page's edit stays unpushed — and says it did.
#[tokio::test]
async fn rm_previews_and_deletes_only_what_it_names() {
    let server = dc_server().await;
    Mock::given(method("DELETE"))
        .and(path(format!("/rest/api/content/{CHILD_PAGE}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull"]))), 0);
    let root = read(dir.path(), ROOT_FILE);
    std::fs::write(dir.path().join(ROOT_FILE), root.replace("Welcome", "Unpushed")).unwrap();

    let dry = envelope(
        &run(confed_authed(dir.path(), &["rm", CHILD_FILE, "--push", "--dry-run", "--json"])),
        "rm",
    );
    assert_eq!(dry["result"]["dry_run"], json!(true), "{dry}");
    assert!(dir.path().join(CHILD_FILE).exists(), "a dry run removes nothing");
    assert!(mutations(&server).await.is_empty());

    let output = run(confed_authed(dir.path(), &["rm", CHILD_FILE, "--push", "--yes", "--json"]));
    assert_eq!(exit_code(&output), 0, "{}", stderr(&output));
    let value = envelope(&output, "rm");
    assert_eq!(value["result"]["removed"][0]["server_deleted"], json!(true), "{value}");
    assert_eq!(
        mutations(&server).await,
        vec![format!("DELETE /rest/api/content/{CHILD_PAGE}")],
        "only the named page; the edited root was not pushed"
    );

    // The id still works where the server or the history can answer.
    let local = run(confed_authed(dir.path(), &["log", "--local", CHILD_PAGE, "--json"]));
    assert_eq!(exit_code(&local), 0, "{}", stderr(&local));
    let live = run(confed_authed(dir.path(), &["comment", "list", CHILD_PAGE, "--json"]));
    assert_eq!(exit_code(&live), 0, "{}", stderr(&live));
    let live = envelope(&live, "comment");
    assert_eq!(live["result"]["source"], json!("server"), "{live}");
}

/// Text that exists only inside a raw ```confluence table cannot carry a body
/// mark; the draft goes to the sidecar, the result says so, and the dry run
/// lists it. Never a success with nothing written.
#[tokio::test]
async fn an_anchor_inside_a_raw_table_is_drafted_in_the_sidecar() {
    let server = dc_server().await;
    let storage = "<p>Intro.</p><ac:structured-macro ac:name=\"mystery\"><ac:rich-text-body><table><tbody><tr><td>Ширину данной колонки пользователь может менять.</td></tr></tbody></table></ac:rich-text-body></ac:structured-macro>";
    Mock::given(method("GET"))
        .and(path(format!("/rest/api/content/{CHILD_PAGE}")))
        .respond_with({
            let mut page = summary(CHILD_PAGE, "Onboarding", Some(ROOT_PAGE), 1);
            page["body"] = json!({ "storage": { "value": storage, "representation": "storage" } });
            ResponseTemplate::new(200).set_body_json(page)
        })
        .with_priority(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(exit_code(&init(dir.path(), &server)), 0);
    assert_eq!(exit_code(&run(confed_authed(dir.path(), &["pull"]))), 0);
    assert!(read(dir.path(), CHILD_FILE).contains("```confluence"), "the table is raw");

    let anchor = "Ширину данной колонки пользователь может менять.";
    let output = run(confed_authed(
        dir.path(),
        &["comment", "add", CHILD_FILE, "--anchor", anchor, "-m", "Почему?", "--json"],
    ));
    assert_eq!(exit_code(&output), 0, "{}", stderr(&output));
    let value = envelope(&output, "comment");
    assert_eq!(value["result"]["written_to"], json!("sidecar"), "{value}");
    assert!(!read(dir.path(), CHILD_FILE).contains("<!--c new"), "nothing in the fence");

    let dry = envelope(&run(confed_authed(dir.path(), &["push", "--dry-run", "--json"])), "push");
    let pending = dry["result"]["comments_pending"].to_string();
    assert!(pending.contains(anchor), "the dry run lists the draft: {dry}");

    // A multi-line comment cannot be a one-line mark either; it is not squashed.
    let output = run(confed_authed(
        dir.path(),
        &["comment", "add", CHILD_FILE, "--anchor", "Intro.", "-m", "line one\nline two", "--json"],
    ));
    assert_eq!(envelope(&output, "comment")["result"]["written_to"], json!("sidecar"));
    let sidecar = read(dir.path(), "Team Handbook/.Onboarding/comments.md");
    assert!(sidecar.contains("line one\nline two"), "{sidecar}");
}
