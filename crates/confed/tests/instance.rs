//! End-to-end tests for `confed instance`: the real binary against a small
//! stateful Data Center.
//!
//! A move changes what the server answers afterwards — which space a page is
//! in, which pages a space lists — so instead of fixed responses the mock
//! server runs one responder over an in-memory instance: two spaces, their
//! pages, and the REST v1 routes confed uses. Everything else is as in
//! `cli.rs`: a temp dir per test, no network, every `--json` envelope checked
//! against the published schemas.

mod schema;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "pat-token";
const PERSONAL: &str = "~ff8081817a1b2c3d4e5f6071";

// ------------------------------------------------------------ the fixture ---

#[derive(Clone)]
struct Page {
    title: String,
    space: String,
    parent: Option<String>,
    version: u32,
    body: String,
}

#[derive(Default)]
struct World {
    spaces: BTreeMap<String, (u64, String, Option<String>)>,
    pages: BTreeMap<String, Page>,
    /// Spaces whose content answers 401, as if the token had no access.
    locked_out: Vec<String>,
    /// Mutating requests, `METHOD path`.
    writes: Vec<String>,
    /// Pages whose updates fail with a server error.
    broken_updates: Vec<String>,
    /// The status every move answers with instead of moving.
    refuse_moves: Option<u16>,
    /// A Data Center without the REST move endpoint, as Atlassian documents it.
    no_rest_move: bool,
    /// …and without the web UI's move action either.
    no_web_move: bool,
    /// How the web forms' XSRF check treats a script.
    xsrf: Xsrf,
    /// Every request, reads included.
    requests: usize,
    /// Requests to a move route, probes included.
    move_requests: usize,
    /// Background move tasks: page, target, position, polls until done.
    tasks: BTreeMap<String, (String, String, String, u32)>,
}

/// Confluence's check against forged form submissions.
#[derive(Clone, Copy, Default, PartialEq)]
enum Xsrf {
    /// `X-Atlassian-Token: no-check` passes it.
    #[default]
    NoCheckPasses,
    /// Only a session's `atl_token`, with that session's cookie, passes it.
    TokenOnly,
    /// Nothing a script sends passes it.
    RefusesAll,
}

const SESSION: &str = "JSESSIONID=9000000abc";
const ATL_TOKEN: &str = "9000000def";

fn xsrf_page() -> ResponseTemplate {
    ResponseTemplate::new(403).set_body_string(
        "<html><head><title>Session Expired - Confluence</title></head><body>\
         XSRF Security Token Missing: a required security token was not present</body></html>",
    )
}

#[derive(Clone)]
struct Confluence(Arc<Mutex<World>>);

impl Confluence {
    /// `DOCS` and `ENG`:
    ///
    /// ```text
    /// DOCS  Team Handbook (home)      ENG  Engineering Home (home)
    ///         Onboarding                     Runbooks → DOCS:Week One
    ///           Week One
    ///         Policies → Onboarding
    /// OPS   Operations (home)
    /// ```
    fn new() -> Self {
        let mut world = World::default();
        world.spaces.insert("DOCS".into(), (500, "Documentation".into(), Some("1001".into())));
        world.spaces.insert("ENG".into(), (501, "Engineering".into(), Some("2001".into())));
        world.spaces.insert("OPS".into(), (502, "Operations".into(), Some("3001".into())));
        // A personal space on a newer Data Center: keyed by its owner's user key.
        world.spaces.insert(PERSONAL.into(), (503, "Alice Ng".into(), Some("4001".into())));
        let mut page = |id: &str, title: &str, space: &str, parent: Option<&str>, body: &str| {
            world.pages.insert(
                id.into(),
                Page {
                    title: title.into(),
                    space: space.into(),
                    parent: parent.map(str::to_string),
                    version: 1,
                    body: body.into(),
                },
            );
        };
        page("1001", "Team Handbook", "DOCS", None, "<p>Welcome to the team.</p>");
        page(
            "1002",
            "Onboarding",
            "DOCS",
            Some("1001"),
            r#"<p>Read <ac:link><ri:page ri:content-title="Team Handbook" /></ac:link> first.</p>"#,
        );
        page("1003", "Week One", "DOCS", Some("1002"), "<p>План работ, день за днём.</p>");
        page(
            "1004",
            "Policies",
            "DOCS",
            Some("1001"),
            r#"<p>New here? <ac:link><ri:page ri:content-title="Onboarding" /></ac:link>.</p>"#,
        );
        page("2001", "Engineering Home", "ENG", None, "<p>Engineering.</p>");
        page(
            "2002",
            "Runbooks",
            "ENG",
            Some("2001"),
            r#"<p>Start: <ac:link><ri:page ri:space-key="DOCS" ri:content-title="Week One" /></ac:link>.</p>"#,
        );
        page("3001", "Operations", "OPS", None, "<p>Ops.</p>");
        page("4001", "Alice Ng", PERSONAL, None, "<p>Notes.</p>");
        Self(Arc::new(Mutex::new(world)))
    }

    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap()
    }
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn error(status: u16, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "statusCode": status, "message": message }))
}

fn listing(results: Vec<Value>) -> Value {
    json!({ "results": results, "start": 0, "limit": 500, "size": 0, "_links": {} })
}

impl World {
    fn ancestors(&self, id: &str) -> Vec<Value> {
        let mut chain = Vec::new();
        let mut at = self.pages.get(id).and_then(|p| p.parent.clone());
        while let Some(parent) = at {
            chain.push(json!({ "id": parent, "type": "page" }));
            at = self.pages.get(&parent).and_then(|p| p.parent.clone());
        }
        chain.reverse();
        chain
    }

    fn content(&self, id: &str, body: bool) -> Value {
        let page = &self.pages[id];
        let (numeric, name, _) = &self.spaces[&page.space];
        let mut out = json!({
            "id": id,
            "type": "page",
            "status": "current",
            "title": page.title,
            "space": { "id": numeric, "key": page.space, "name": name },
            "version": { "number": page.version, "when": "2026-08-01T00:00:00Z",
                         "by": { "displayName": "Alice Ng" } },
            "ancestors": self.ancestors(id),
            "metadata": { "labels": { "results": [] } },
            "history": { "createdDate": "2026-01-01T00:00:00Z" },
        });
        if body {
            out["body"] = json!({ "storage": { "value": page.body, "representation": "storage" } });
        }
        out
    }

    fn space(&self, key: &str) -> Value {
        let (numeric, name, home) = &self.spaces[key];
        let kind = if key.starts_with('~') { "personal" } else { "global" };
        json!({
            "id": numeric, "key": key, "name": name, "type": kind, "status": "current",
            "homepage": home.as_ref().map(|h| json!({ "id": h, "type": "page", "title": self.pages[h].title })),
        })
    }

    fn subtree(&self, root: &str) -> Vec<String> {
        let mut out = vec![root.to_string()];
        let mut i = 0;
        while i < out.len() {
            let parent = out[i].clone();
            out.extend(
                self.pages
                    .iter()
                    .filter(|(_, p)| p.parent.as_deref() == Some(&parent))
                    .map(|(id, _)| id.clone()),
            );
            i += 1;
        }
        out
    }
}

impl Respond for Confluence {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let authed = req
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {TOKEN}"));
        if !authed {
            return error(401, "not authenticated");
        }
        let path = req.url.path().to_string();
        let query: BTreeMap<String, String> = req.url.query_pairs().into_owned().collect();
        let method = req.method.as_str().to_string();
        let mut world = self.world();
        world.requests += 1;
        if path.contains("/move/") || path.ends_with("movepage.action") {
            world.move_requests += 1;
        }
        // A probe names a page or target that cannot exist: it cannot write.
        let form = if method == "POST" { parse_form(&req.body) } else { BTreeMap::new() };
        let probe =
            path.ends_with("/move/append/0") || form.get("pageId").is_some_and(|p| p == "0");
        if method != "GET" && !probe {
            world.writes.push(format!("{method} {path}"));
        }
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

        match (method.as_str(), segments.as_slice()) {
            ("GET", ["rest", "api", "user", "current"]) => ok(json!({
                "type": "known", "username": "tester", "userKey": "ff8081",
                "displayName": "Test User", "email": "tester@example.com",
            })),
            ("GET", ["rest", "api", "user"]) => match query.get("key").map(String::as_str) {
                Some(key) if PERSONAL.ends_with(key) => ok(json!({
                    "type": "known", "username": "alice", "userKey": key, "displayName": "Alice Ng",
                })),
                _ => error(404, "No user found"),
            },
            ("GET", ["rest", "api", "space"]) => {
                if query.get("status").map(String::as_str) == Some("archived") {
                    return ok(listing(vec![]));
                }
                let spaces = match query.get("spaceKey") {
                    Some(key) if world.spaces.contains_key(key) => vec![world.space(key)],
                    Some(_) => vec![],
                    None => world.spaces.keys().map(|k| world.space(k)).collect(),
                };
                ok(listing(spaces))
            }
            ("GET", ["rest", "api", "content"]) => {
                let key = query.get("spaceKey").cloned().unwrap_or_default();
                if world.locked_out.contains(&key) {
                    return error(401, "no access to this space");
                }
                let ids: Vec<String> = world
                    .pages
                    .iter()
                    .filter(|(_, p)| p.space == key)
                    .map(|(id, _)| id.clone())
                    .collect();
                ok(listing(ids.iter().map(|id| world.content(id, false)).collect()))
            }
            ("GET", ["rest", "api", "content", id]) => match world.pages.get(*id) {
                Some(_) => ok(world.content(id, true)),
                None => error(404, &format!("No content found with id: {id}")),
            },
            ("GET", ["rest", "api", "content", _, "child", _]) => ok(listing(vec![])),
            ("GET", ["rest", "api", "content", _, "restriction", "byOperation"]) => ok(json!({})),
            ("GET", ["rest", "api", "search"]) => ok(listing(vec![])),
            ("PUT", ["rest", "api", "content", _, "move", _, _]) if world.no_rest_move => {
                error(404, &format!("null for uri: {path}"))
            }
            ("GET", ["pages", "viewpage.action"]) => ResponseTemplate::new(200)
                .insert_header("Set-Cookie", format!("{SESSION}; Path=/; HttpOnly").as_str())
                .set_body_string(format!(
                    r#"<html><head><meta name="ajs-atl-token" content="{ATL_TOKEN}"></head></html>"#
                )),
            ("POST", ["pages", "movepage.action"]) if !world.no_web_move => {
                let no_check = req
                    .headers
                    .get("x-atlassian-token")
                    .is_some_and(|v| v.to_str().unwrap_or_default() == "no-check");
                let token = form.get("atl_token").is_some_and(|t| t == ATL_TOKEN)
                    && req
                        .headers
                        .get("cookie")
                        .is_some_and(|c| c.to_str().unwrap_or_default().contains(SESSION));
                let passes = match world.xsrf {
                    Xsrf::NoCheckPasses => no_check || token,
                    Xsrf::TokenOnly => token,
                    Xsrf::RefusesAll => false,
                };
                if !passes {
                    return xsrf_page();
                }
                if form["pageId"] == "0" {
                    return ResponseTemplate::new(200).set_body_string("<html>Page not found</html>");
                }
                // As a 9.5 server does: the new parent by title, the move
                // started in the background, answered before it is done.
                let id = form["pageId"].clone();
                let (space, title) = (&form["spaceKey"], &form["targetTitle"]);
                let Some(target) = world
                    .pages
                    .iter()
                    .find(|(_, p)| &p.space == space && &p.title == title)
                    .map(|(id, _)| id.clone())
                else {
                    return ResponseTemplate::new(200).set_body_string("<html>No such page</html>");
                };
                let task = format!("9000000t{}", world.tasks.len() + 1);
                world.tasks.insert(task.clone(), (id.clone(), target, form["position"].clone(), 2));
                ok(json!({
                    "complete": false, "status": "Starting", "longTaskId": task,
                    "page": { "id": id, "space": { "key": world.pages[&id].space } },
                }))
            }
            ("GET", ["rest", "api", "longtask", task]) => {
                let Some((id, target, position, left)) = world.tasks.get(*task).cloned() else {
                    return error(404, "No long task found");
                };
                if left > 0 {
                    world.tasks.get_mut(*task).unwrap().3 = left - 1;
                    return ok(json!({ "percentageComplete": 50, "successful": true }));
                }
                if world.pages[&id].space != world.pages[&target].space
                    || world.pages[&id].parent.as_deref() != Some(target.as_str())
                {
                    let space = world.pages[&target].space.clone();
                    let parent = match position.as_str() {
                        "append" => Some(target.clone()),
                        _ => world.pages[&target].parent.clone(),
                    };
                    for moved in world.subtree(&id) {
                        world.pages.get_mut(&moved).unwrap().space = space.clone();
                    }
                    world.pages.get_mut(&id).unwrap().parent = parent;
                }
                ok(json!({ "percentageComplete": 100, "successful": true }))
            }
            ("PUT", ["rest", "api", "content", id, "move", position, target]) => {
                if let Some(status) = world.refuse_moves {
                    return error(status, "you may not move this page");
                }
                let (id, target) = (id.to_string(), target.to_string());
                if !world.pages.contains_key(&id) || !world.pages.contains_key(&target) {
                    return error(404, "No content found with id: 0");
                }
                let parent = match *position {
                    "append" => Some(target.clone()),
                    _ => world.pages[&target].parent.clone(),
                };
                let space = world.pages[&target].space.clone();
                for moved in world.subtree(&id) {
                    world.pages.get_mut(&moved).unwrap().space = space.clone();
                }
                world.pages.get_mut(&id).unwrap().parent = parent;
                ok(json!({ "pageId": id }))
            }
            ("PUT", ["rest", "api", "content", id])
                if world.broken_updates.iter().any(|b| b == id) =>
            {
                error(500, "the server fell over")
            }
            ("PUT", ["rest", "api", "content", id]) => {
                let sent: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let Some(page) = world.pages.get_mut(*id) else {
                    return error(404, "No content found");
                };
                let wanted = sent["version"]["number"].as_u64().unwrap_or(0) as u32;
                if wanted != page.version + 1 {
                    return error(409, "version conflict");
                }
                page.version = wanted;
                if let Some(title) = sent["title"].as_str() {
                    page.title = title.to_string();
                }
                if let Some(body) = sent["body"]["storage"]["value"].as_str() {
                    page.body = body.to_string();
                }
                if let Some(parent) = sent["ancestors"][0]["id"].as_str() {
                    page.parent = Some(parent.to_string());
                }
                let id = id.to_string();
                ok(world.content(&id, true))
            }
            ("GET" | "POST", ["pages", _]) => ResponseTemplate::new(404).set_body_string(format!(
                "<html>There is no Action mapped for namespace [/pages] and action name [{}]</html>",
                segments[1].trim_end_matches(".action")
            )),
            _ => error(404, &format!("null for uri: {path}")),
        }
    }
}

/// `a=1&b=x+y%26z` → {a: 1, b: "x y&z"}.
fn parse_form(body: &[u8]) -> BTreeMap<String, String> {
    let decode = |s: &str| {
        let bytes = s.replace('+', " ").into_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    };
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

async fn server() -> (MockServer, Confluence) {
    let server = MockServer::start().await;
    let confluence = Confluence::new();
    Mock::given(any()).respond_with(confluence.clone()).mount(&server).await;
    (server, confluence)
}

// ------------------------------------------------------- running the tool ---

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

fn run(dir: &Path, args: &[&str]) -> Output {
    confed(dir, args).output().expect("running the confed binary")
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("confed exited via a signal")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn envelope(output: &Output, command: &str) -> Value {
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!("stdout is not JSON ({e}):\n{text}\n--- stderr ---\n{}", stderr(output))
    });
    schema::check("envelope", &value);
    assert_eq!(value["confed"]["command"], json!(command));
    assert_eq!(value["confed"]["exit_code"], json!(code(output)));
    if value["result"].is_object() {
        schema::check_result(command, &value["result"]);
    }
    value
}

/// Clone a space the ordinary way, into `dir/KEY`, with its own credential.
fn clone_alone(dir: &Path, server: &MockServer, key: &str) {
    let out = run(
        dir,
        &[
            "clone",
            key,
            "--json",
            "--base-url",
            &server.uri(),
            "--flavor",
            "dc",
            "--token",
            TOKEN,
            "--credential-store",
            "sqlite",
        ],
    );
    assert_eq!(code(&out), 0, "clone {key}: {}", stderr(&out));
}

/// An instance root with DOCS and ENG cloned into it, sharing one credential.
fn instance_root(server: &MockServer) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let out = run(
        root.path(),
        &[
            "instance",
            "init",
            "--json",
            "--base-url",
            &server.uri(),
            "--flavor",
            "dc",
            "--token",
            TOKEN,
            "--credential-store",
            "sqlite",
        ],
    );
    assert_eq!(code(&out), 0, "instance init: {}", stderr(&out));
    envelope(&out, "instance");
    let out = run(root.path(), &["instance", "clone", "DOCS", "ENG", "--json"]);
    assert_eq!(code(&out), 0, "instance clone: {}", stderr(&out));
    root
}

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative))
        .unwrap_or_else(|e| panic!("reading {relative}: {e}"))
}

// ------------------------------------------------------------------ tests ---

#[tokio::test]
async fn adopt_registers_existing_workspaces_and_shares_their_credential() {
    let (server, _) = server().await;
    let root = tempfile::tempdir().unwrap();
    clone_alone(root.path(), &server, "DOCS");
    clone_alone(root.path(), &server, "ENG");

    // A dry run says what it would do and does nothing.
    let out = run(root.path(), &["instance", "adopt", "--dry-run", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(!root.path().join(".confed-instance.db").exists());

    // …and with --move-credentials, exactly what would happen to them.
    let out = run(
        root.path(),
        &[
            "instance",
            "adopt",
            "--dry-run",
            "--move-credentials",
            "--credential-store",
            "sqlite",
            "--json",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let credentials = envelope(&out, "instance")["result"]["credentials"].clone();
    assert_eq!(credentials["would_share"], true);
    assert_eq!(credentials["source"], "DOCS/.session.db (in the file)");
    assert_eq!(credentials["store"], "sqlite");
    assert_eq!(credentials["remove"], json!(["DOCS/.session.db", "ENG/.session.db"]));
    assert_eq!(credentials["keyring_entries_deleted"], json!([]));
    let out = run(root.path(), &["instance", "adopt", "--dry-run", "--move-credentials"]);
    let human = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(human.contains("check the credential from DOCS/.session.db"), "{human}");
    assert!(human.contains("remove DOCS/.session.db, ENG/.session.db"), "{human}");
    assert!(root.path().join("DOCS/.session.db").exists(), "a dry run removes nothing");

    let out = run(
        root.path(),
        &["instance", "adopt", "--move-credentials", "--credential-store", "sqlite", "--json"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let registered: Vec<&str> = env["result"]["registered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["space_key"].as_str().unwrap())
        .collect();
    assert_eq!(registered, ["DOCS", "ENG"]);
    assert_eq!(env["result"]["credentials"]["moved_from"], json!(["DOCS", "ENG"]));

    // One credential, in the root; none left in the workspaces.
    assert!(root.path().join(".confed-instance-session.db").exists());
    for key in ["DOCS", "ENG"] {
        assert!(!root.path().join(key).join(".session.db").exists(), "{key} keeps no credential");
        assert!(read(root.path(), &format!("{key}/CLAUDE.md")).contains("## Part of an instance"));
    }
    let gitignore = read(root.path(), ".gitignore");
    for pattern in [".confed-instance.db", ".confed-instance-session.db", "*/.session.db"] {
        assert!(gitignore.contains(pattern), "{pattern} in {gitignore}");
    }
    assert!(read(root.path(), "CLAUDE.md").contains("confed instance root"));

    // Every workspace still works on its own, through the root's credential.
    let out = run(&root.path().join("ENG"), &["status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(&root.path().join("ENG"), &["fetch", "--json"]);
    assert_eq!(code(&out), 0, "fetch uses the shared credential: {}", stderr(&out));
}

#[tokio::test]
async fn adopt_refuses_workspaces_of_two_instances_and_names_them() {
    let (one, _) = server().await;
    let (two, _) = server().await;
    let root = tempfile::tempdir().unwrap();
    clone_alone(root.path(), &one, "DOCS");
    clone_alone(root.path(), &two, "ENG");

    let out = run(root.path(), &["instance", "adopt", "--json"]);
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let message = env["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("DOCS: ") && message.contains("ENG: "), "{message}");
    assert!(!root.path().join(".confed-instance.db").exists());
}

#[tokio::test]
async fn clone_takes_only_the_missing_spaces_and_a_second_run_does_nothing() {
    let (server, confluence) = server().await;
    let root = tempfile::tempdir().unwrap();
    let out = run(
        root.path(),
        &[
            "instance",
            "init",
            "--base-url",
            &server.uri(),
            "--flavor",
            "dc",
            "--token",
            TOKEN,
            "--credential-store",
            "sqlite",
            "--json",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let out = run(root.path(), &["instance", "clone", "--all", "--exclude", "OPS", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let statuses: Vec<(String, String)> = env["result"]["spaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["space_key"].as_str().unwrap().into(), s["status"].as_str().unwrap().into()))
        .collect();
    assert_eq!(statuses, [("DOCS".into(), "cloned".into()), ("ENG".into(), "cloned".into())]);
    assert!(root.path().join("DOCS/Team Handbook/Onboarding/Week One.md").exists());
    assert!(!root.path().join("OPS").exists());
    assert!(!root.path().join("DOCS/.session.db").exists(), "the root's credential is used");

    let writes = confluence.world().writes.len();
    let out = run(root.path(), &["instance", "clone", "--all", "--exclude", "OPS", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    assert!(env["result"]["spaces"].as_array().unwrap().iter().all(|s| s["status"] == "skipped"));
    assert_eq!(confluence.world().writes.len(), writes);
}

#[tokio::test]
async fn status_reports_every_space_and_one_failing_space_is_exit_8() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);

    let out = run(root.path(), &["instance", "status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    assert_eq!(env["result"]["action"], "status");
    let spaces = env["result"]["spaces"].as_array().unwrap();
    assert_eq!(spaces.len(), 2);
    for (space, key) in spaces.iter().zip(["DOCS", "ENG"]) {
        assert_eq!(space["space_key"], key);
        assert_eq!(space["dir"], key);
        assert_eq!(space["ok"], true);
        assert_eq!(space["exit_code"], 0);
        // The single-space result, as it is.
        assert!(space["result"]["pages"].is_array(), "{space}");
    }

    // ENG's content now refuses this token; DOCS is unaffected.
    confluence.world().locked_out.push("ENG".into());
    let out = run(root.path(), &["instance", "status", "--fetch", "--json"]);
    assert_eq!(code(&out), 8, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let spaces = env["result"]["spaces"].as_array().unwrap();
    assert_eq!(spaces[0]["ok"], true);
    assert!(spaces[0]["result"].is_object(), "the space that worked is intact");
    assert_eq!(spaces[1]["ok"], false);
    assert_eq!(spaces[1]["exit_code"], 3);
    assert!(env["errors"][0]["message"].as_str().unwrap().starts_with("ENG: "));

    // --space narrows it.
    let out = run(root.path(), &["instance", "status", "--space", "DOCS", "--fetch", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

#[tokio::test]
async fn a_page_tree_moves_to_another_space_and_both_end_clean() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);

    let src = "DOCS:Team Handbook/Onboarding.md";
    let dst = "ENG:Engineering Home/Runbooks.md";
    let out = run(root.path(), &["instance", "mv", src, dst, "--dry-run", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let plan = &env["result"];
    let pages: Vec<&str> =
        plan["pages"].as_array().unwrap().iter().map(|p| p["page_id"].as_str().unwrap()).collect();
    assert_eq!(pages, ["1002", "1003"], "the whole subtree");
    let linkers: Vec<&str> =
        plan["links"].as_array().unwrap().iter().map(|l| l["page_id"].as_str().unwrap()).collect();
    assert!(
        linkers.contains(&"1004") && linkers.contains(&"2002") && linkers.contains(&"1002"),
        "{linkers:?}"
    );
    assert!(confluence.world().writes.is_empty(), "a dry run writes nothing");

    // Without --yes, a run that cannot ask refuses.
    let out = run(root.path(), &["instance", "mv", src, dst, "--json"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));

    let out = run(
        root.path(),
        &["instance", "mv", src, dst, "-m", "Engineering owns onboarding", "-y", "--json"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let result = &env["result"];
    assert!(result["journal_id"].as_str().unwrap().starts_with("mv-"));
    let moved: Vec<&str> = result["moved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["page_id"].as_str().unwrap())
        .collect();
    assert_eq!(moved, ["1002", "1003"], "same ids");
    assert_eq!(result["moved"][0]["to"]["path"], "Engineering Home/Runbooks/Onboarding.md");

    {
        let world = confluence.world();
        assert_eq!(world.pages["1002"].space, "ENG");
        assert_eq!(world.pages["1003"].space, "ENG");
        assert!(world.pages["1004"]
            .body
            .contains(r#"ri:space-key="ENG" ri:content-title="Onboarding""#));
        assert!(world.pages["2002"]
            .body
            .contains(r#"ri:space-key="ENG" ri:content-title="Week One""#));
    }

    let moved_file = read(root.path(), "ENG/Engineering Home/Runbooks/Onboarding.md");
    assert!(moved_file.contains("space_key: ENG"), "{moved_file}");
    assert!(root.path().join("ENG/Engineering Home/Runbooks/Onboarding/Week One.md").exists());
    assert!(!root.path().join("DOCS/Team Handbook/Onboarding.md").exists());
    assert!(!root.path().join("DOCS/Team Handbook/Onboarding").exists());

    for key in ["DOCS", "ENG"] {
        let out = run(&root.path().join(key), &["status", "--json"]);
        assert_eq!(code(&out), 0, "{key}: {}", stderr(&out));
        let pages = envelope(&out, "status")["result"]["pages"].as_array().unwrap().clone();
        assert!(pages.iter().all(|p| p["state"] == "unchanged"), "{key}: {pages:?}");
    }
    let out = run(root.path(), &["instance", "doctor", "--json"]);
    let env = envelope(&out, "instance");
    let checks = env["result"]["instance"]["checks"].as_array().unwrap();
    for name in ["pages tracked once", "journal", "page spaces"] {
        let check = checks.iter().find(|c| c["name"] == name).unwrap();
        assert_eq!(check["status"], "pass", "{check}");
    }
}

#[tokio::test]
async fn moves_that_cannot_be_made_are_refused_before_any_write() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);

    // The home page.
    let out = run(root.path(), &["instance", "mv", "1001", "ENG:", "--dry-run", "--json"]);
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    assert!(envelope(&out, "instance")["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("home page"));

    // A dirty page in the subtree and a title the destination has, together.
    let week = root.path().join("DOCS/Team Handbook/Onboarding/Week One.md");
    let mut text = std::fs::read_to_string(&week).unwrap();
    text.push_str("\nUnpushed.\n");
    std::fs::write(&week, text).unwrap();
    {
        let mut world = confluence.world();
        world.pages.insert(
            "2005".into(),
            Page {
                title: "Onboarding".into(),
                space: "ENG".into(),
                parent: Some("2001".into()),
                version: 1,
                body: "<p/>".into(),
            },
        );
    }
    let out =
        run(root.path(), &["instance", "mv", "DOCS:Team Handbook/Onboarding.md", "ENG:", "--json"]);
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    let message = envelope(&out, "instance")["errors"][0]["message"].as_str().unwrap().to_string();
    assert!(message.contains("Week One.md: has local changes"), "{message}");
    assert!(message.contains(r#"title "Onboarding""#), "{message}");

    // A lock held by a live process.
    std::fs::write(root.path().join("ENG/.confed.lock"), format!("{}\n", std::process::id()))
        .unwrap();
    let out = run(
        root.path(),
        &["instance", "mv", "DOCS:Team Handbook/Policies.md", "ENG:", "-y", "--json"],
    );
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    std::fs::remove_file(root.path().join("ENG/.confed.lock")).unwrap();

    // Bad arguments are exit 2.
    let out =
        run(root.path(), &["instance", "mv", "DOCS:Team Handbook/Policies.md", "DOCS:", "--json"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let out = run(root.path(), &["instance", "mv", "NOPE:x.md", "ENG:", "--json"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));

    assert!(confluence.world().writes.is_empty(), "{:?}", confluence.world().writes);
}

#[tokio::test]
async fn single_space_commands_point_at_instance_mv_for_another_space() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    let docs = root.path().join("DOCS");

    // `confed mv` into another workspace: exit 2, with the command to use.
    let out =
        run(&docs, &["mv", "Team Handbook/Policies.md", "../ENG/Engineering Home/Policies.md"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("confed instance mv \"DOCS:Team Handbook/Policies.md\" \"ENG:Engineering Home/Policies.md\""), "{err}");
    assert!(docs.join("Team Handbook/Policies.md").exists());

    // A parent_id in another space: push refuses the page, exit 7.
    let path = docs.join("Team Handbook/Policies.md");
    let text =
        std::fs::read_to_string(&path).unwrap().replace("parent_id: '1001'", "parent_id: '2001'");
    std::fs::write(&path, text).unwrap();
    let out = run(&docs, &["push", "-m", "move it", "--json"]);
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    let env = envelope(&out, "push");
    let message = env["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.contains(r#"confed instance mv "DOCS:Team Handbook/Policies.md" 2001"#),
        "the hint is a command that works as it is: {message}"
    );
    assert!(confluence.world().writes.is_empty(), "{:?}", confluence.world().writes);
}

#[tokio::test]
async fn an_interrupted_move_is_reported_and_resumed_to_the_same_end() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    // The move itself succeeds; rewriting one page's links afterwards does not.
    confluence.world().broken_updates.push("1004".into());

    let src = "DOCS:Team Handbook/Onboarding.md";
    let out = run(root.path(), &["instance", "mv", src, "ENG:", "-y", "-m", "move", "--json"]);
    // Stopped part-way: a local state to finish, said with the way to finish it.
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let hint = env["errors"][0]["hint"].as_str().unwrap_or_default().to_string();
    assert!(hint.contains("confed instance resume mv-"), "{env}");
    assert_eq!(confluence.world().pages["1002"].space, "ENG", "the server moved");

    // Every command keeps working, and doctor says what is unfinished.
    let out = run(&root.path().join("DOCS"), &["status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(root.path(), &["instance", "doctor", "--json"]);
    let env = envelope(&out, "instance");
    let journal = env["result"]["instance"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "journal")
        .unwrap()
        .clone();
    assert_eq!(journal["status"], "fail");
    assert!(journal["detail"].as_str().unwrap().contains("after `local_src_removed`"), "{journal}");

    // Abort is too late now.
    let out = run(root.path(), &["instance", "abort", "--json"]);
    assert_eq!(code(&out), 7, "{}", stderr(&out));

    confluence.world().broken_updates.clear();
    let out = run(root.path(), &["instance", "resume", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    assert_eq!(env["result"]["resumed_from"], "local_src_removed");
    assert!(confluence.world().pages["1004"].body.contains(r#"ri:space-key="ENG""#));

    let out = run(root.path(), &["instance", "doctor", "--json"]);
    let env = envelope(&out, "instance");
    for check in env["result"]["instance"]["checks"].as_array().unwrap() {
        if ["journal", "pages tracked once", "page spaces"]
            .contains(&check["name"].as_str().unwrap())
        {
            assert_eq!(check["status"], "pass", "{check}");
        }
    }
    for key in ["DOCS", "ENG"] {
        let out = run(&root.path().join(key), &["status", "--json"]);
        let pages = envelope(&out, "status")["result"]["pages"].as_array().unwrap().clone();
        assert!(pages.iter().all(|p| p["state"] == "unchanged"), "{key}: {pages:?}");
    }
    let out = run(root.path(), &["instance", "resume", "--json"]);
    assert_eq!(code(&out), 0, "nothing left to resume: {}", stderr(&out));
}

#[tokio::test]
async fn a_refused_move_changes_nothing_and_can_be_aborted() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    confluence.world().refuse_moves = Some(403);

    let src = "DOCS:Team Handbook/Onboarding.md";
    let out = run(root.path(), &["instance", "mv", src, "ENG:", "-y", "--json"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let message = env["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("HTTP 403") && message.contains("permission"), "{message}");
    assert_eq!(confluence.world().pages["1002"].space, "DOCS");
    assert!(root.path().join("DOCS/Team Handbook/Onboarding.md").exists());

    let out = run(root.path(), &["instance", "abort", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(root.path(), &["instance", "doctor", "--offline", "--json"]);
    let env = envelope(&out, "instance");
    let journal = env["result"]["instance"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "journal")
        .unwrap()
        .clone();
    assert_eq!(journal["status"], "pass");
}

#[tokio::test]
async fn on_data_center_without_the_rest_move_the_web_action_moves_the_tree() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    confluence.world().no_rest_move = true;

    let src = "DOCS:Team Handbook/Onboarding.md";
    let dst = "ENG:Engineering Home/Runbooks.md";
    let out = run(root.path(), &["instance", "mv", src, dst, "--dry-run", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let warned = env["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("pages/movepage.action"));
    assert!(warned, "the dry run says which way it will move: {env}");
    assert!(confluence.world().writes.is_empty(), "{:?}", confluence.world().writes);

    let out = run(root.path(), &["instance", "mv", src, dst, "-y", "-m", "move", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    {
        let world = confluence.world();
        assert_eq!(world.pages["1002"].space, "ENG");
        assert_eq!(world.pages["1003"].space, "ENG");
        assert_eq!(world.pages["1002"].parent.as_deref(), Some("2002"));
        assert!(world.writes.iter().any(|w| w == "POST /pages/movepage.action"));
    }
    for key in ["DOCS", "ENG"] {
        let out = run(&root.path().join(key), &["status", "--json"]);
        let pages = envelope(&out, "status")["result"]["pages"].as_array().unwrap().clone();
        assert!(pages.iter().all(|p| p["state"] == "unchanged"), "{key}: {pages:?}");
    }
}

#[tokio::test]
async fn a_dry_run_on_a_server_that_cannot_move_is_exit_9() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    {
        let mut world = confluence.world();
        world.no_rest_move = true;
        world.no_web_move = true;
    }
    let src = "DOCS:Team Handbook/Onboarding.md";
    let out = run(root.path(), &["instance", "mv", src, "ENG:", "--dry-run", "--json"]);
    assert_eq!(code(&out), 9, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let message = env["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("cannot move pages between spaces"), "{message}");
    assert!(confluence.world().writes.is_empty(), "{:?}", confluence.world().writes);

    // whoami says the same when asked, without moving anything.
    let out = run(&root.path().join("DOCS"), &["whoami", "--probe-move", "--json"]);
    let env = envelope(&out, "whoami");
    assert_eq!(env["result"]["capabilities"]["move_between_spaces"], "unavailable");
}

#[tokio::test]
async fn a_personal_space_is_cloned_under_its_owners_name_and_tables_line_up() {
    let (server, _) = server().await;
    let root = instance_root(&server);

    let out = run(root.path(), &["instance", "clone", PERSONAL, "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    assert_eq!(env["result"]["spaces"][0]["dir"], "@alice");
    assert!(root.path().join("@alice/Alice Ng.md").exists());

    let out = run(root.path(), &["instance", "spaces", "--remote"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let table = String::from_utf8_lossy(&out.stdout).into_owned();
    let lines: Vec<&str> = table.lines().filter(|l| !l.trim().is_empty()).collect();
    // Every row's second column starts where the heading's does.
    let name_at = lines[0].find("NAME").unwrap();
    for line in &lines[1..] {
        let key_end = line.find(' ').unwrap();
        assert!(key_end <= name_at, "{table}");
        assert_ne!(&line[name_at..name_at + 1], " ", "misaligned row:\n{table}");
        assert_eq!(&line[name_at - 1..name_at], " ", "misaligned row:\n{table}");
    }
    assert!(table.contains("@alice"), "{table}");
}

#[tokio::test]
async fn a_web_action_that_wants_a_session_token_gets_one() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    {
        let mut world = confluence.world();
        world.no_rest_move = true;
        world.xsrf = Xsrf::TokenOnly;
    }
    let src = "DOCS:Team Handbook/Onboarding.md";
    let out = run(root.path(), &["instance", "mv", src, "ENG:", "--dry-run", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(root.path(), &["instance", "mv", src, "ENG:", "-y", "-m", "move", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(confluence.world().pages["1002"].space, "ENG");
}

#[tokio::test]
async fn a_server_whose_web_action_refuses_scripts_still_shows_the_whole_plan() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    {
        let mut world = confluence.world();
        world.no_rest_move = true;
        world.xsrf = Xsrf::RefusesAll;
    }
    let src = "DOCS:Team Handbook/Onboarding.md";
    let out = run(root.path(), &["instance", "mv", src, "ENG:", "--dry-run", "--json"]);
    assert_eq!(code(&out), 9, "{}", stderr(&out));
    let env = envelope(&out, "instance");
    let result = &env["result"];
    assert_eq!(result["pages"].as_array().unwrap().len(), 2, "the plan is there: {env}");
    assert!(!result["links"].as_array().unwrap().is_empty());
    assert!(result["blocked"].as_str().unwrap().contains("XSRF"), "{env}");
    let error = &env["errors"][0];
    assert_eq!(error["code"], "UNSUPPORTED");
    assert!(error["hint"].as_str().unwrap().contains("confed instance pull"));

    // Without --dry-run and without -y: still the server's answer, and the plan.
    let out = run(root.path(), &["instance", "mv", src, "ENG:", "--json"]);
    assert_eq!(code(&out), 9, "{}", stderr(&out));

    // Other problems are not hidden behind it: they come first, with it.
    let out = run(root.path(), &["instance", "mv", "1001", "ENG:", "--dry-run", "--json"]);
    assert_eq!(code(&out), 7, "{}", stderr(&out));
    let message = envelope(&out, "instance")["errors"][0]["message"].as_str().unwrap().to_string();
    assert!(message.contains("home page") && message.contains("XSRF"), "{message}");
    assert!(confluence.world().writes.is_empty(), "{:?}", confluence.world().writes);

    let out = run(&root.path().join("DOCS"), &["doctor", "--probe-move", "--json"]);
    let env = envelope(&out, "doctor");
    let check = env["result"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "move between spaces")
        .unwrap()
        .clone();
    assert!(check["detail"].as_str().unwrap().contains("403"), "{check}");
}

#[tokio::test]
async fn nothing_probes_a_page_unless_asked_and_offline_asks_the_server_nothing() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    let docs = root.path().join("DOCS");

    let before = confluence.world().requests;
    let moves_before = confluence.world().move_requests;
    for args in [&["whoami", "--json"][..], &["doctor", "--json"][..]] {
        let out = run(&docs, args);
        assert!(code(&out) == 0 || code(&out) == 1, "{}", stderr(&out));
    }
    let out = run(root.path(), &["instance", "doctor", "--json"]);
    let env = envelope(&out, "instance");
    let checks = env["result"]["instance"]["checks"].as_array().unwrap();
    assert!(!checks.iter().any(|c| c["name"] == "move between spaces"));
    assert!(confluence.world().requests > before);
    assert_eq!(confluence.world().move_requests, moves_before, "no move request, not even a probe");
    let out = run(&docs, &["whoami", "--json"]);
    assert!(envelope(&out, "whoami")["result"]["capabilities"]
        .get("move_between_spaces")
        .is_none());

    let before = confluence.world().requests;
    let out = run(root.path(), &["instance", "doctor", "--offline", "--json"]);
    assert!(code(&out) == 0 || code(&out) == 1, "{}", stderr(&out));
    assert_eq!(confluence.world().requests, before, "--offline sends nothing, in any space");
}

#[tokio::test]
async fn reordering_happens_on_the_server_or_not_at_all() {
    let (server, confluence) = server().await;
    let root = instance_root(&server);
    let docs = root.path().join("DOCS");
    let args = ["mv", "Team Handbook/Policies.md", "--before", "Team Handbook/Onboarding.md"];

    let out = run(&docs, &args);
    assert_eq!(code(&out), 2, "without --push there is nothing that could carry it");

    let mut with_push = args.to_vec();
    with_push.extend(["--push", "--json"]);
    let out = run(&docs, &with_push);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(envelope(&out, "mv")["result"]["reordered"], true);
    assert!(confluence
        .world()
        .writes
        .iter()
        .any(|w| w == "PUT /rest/api/content/1004/move/before/1002"));

    // Data Center without the REST route: the web action, as for a move.
    confluence.world().no_rest_move = true;
    let out = run(&docs, &with_push);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(confluence.world().writes.iter().any(|w| w == "POST /pages/movepage.action"));
}

#[tokio::test]
async fn a_space_cloned_into_a_root_without_a_shared_credential_gets_its_own() {
    let (server, _) = server().await;
    let root = tempfile::tempdir().unwrap();
    clone_alone(root.path(), &server, "DOCS");
    let out = run(root.path(), &["instance", "adopt", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(!root.path().join(".confed-instance-session.db").exists());

    let out = run(root.path(), &["instance", "clone", "OPS", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(root.path().join("OPS/.session.db").exists());
    let out = run(&root.path().join("OPS"), &["fetch", "--json"]);
    assert_eq!(code(&out), 0, "the new space works on its own: {}", stderr(&out));
}

#[tokio::test]
async fn doctor_says_unreachable_the_way_other_commands_do() {
    let (server, _) = server().await;
    let root = instance_root(&server);
    // Nothing listens on the discard port.
    let out =
        run(&root.path().join("DOCS"), &["doctor", "--base-url", "http://127.0.0.1:9", "--json"]);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    let out = run(
        &root.path().join("DOCS"),
        &["status", "--fetch", "--base-url", "http://127.0.0.1:9", "--json"],
    );
    assert_eq!(code(&out), 5, "{}", stderr(&out));
}
