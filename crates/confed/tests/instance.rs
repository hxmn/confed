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
        json!({
            "id": numeric, "key": key, "name": name, "type": "global", "status": "current",
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
        if method != "GET" {
            world.writes.push(format!("{method} {path}"));
        }
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

        match (method.as_str(), segments.as_slice()) {
            ("GET", ["rest", "api", "user", "current"]) => ok(json!({
                "type": "known", "username": "tester", "userKey": "ff8081",
                "displayName": "Test User", "email": "tester@example.com",
            })),
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
            _ => error(404, &format!("null for uri: {path}")),
        }
    }
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
    assert!(message.contains("space ENG") && message.contains("confed instance mv"), "{message}");
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
