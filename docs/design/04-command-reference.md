# 04 — Command Reference (draft)

Status: draft for review · JSON output schema version: 1

## Global behavior

### Flags (every command)

| Flag | Env | Meaning |
|---|---|---|
| `--json` | `CONFED_JSON=1` | versioned machine-readable output; implies `--non-interactive` |
| `--non-interactive` | `CONFED_NON_INTERACTIVE=1` | never prompt; missing input → exit 2 |
| `--base-url <URL>` | `CONFED_BASE_URL` | server base URL |
| `--token <TOKEN>` | `CONFED_TOKEN` | API token (Cloud) or PAT (DC) |
| `--user <EMAIL>` | `CONFED_USERNAME` | Cloud email / DC basic-auth user |
| `--space <KEY>` | `CONFED_SPACE` | space key |
| `--flavor cloud\|dc` | `CONFED_FLAVOR` | skip auto-detection |
| `--concurrency <N>` | `CONFED_CONCURRENCY` | worker pool size |
| `-C <dir>` | — | run as if started in `<dir>` |
| `-v/-q`, `--log <filter>` | `CONFED_LOG` | tracing verbosity (stderr only) |
| `--yes` | — | auto-approve confirmations (destructive ops still listed in output) |

Resolution precedence for every parameter: **flag → env → stored config (`init`) → TTY
prompt**; in non-interactive mode step 4 becomes a fast, specific exit-2 error.

### Exit codes

| Code | Name | Meaning |
|---|---|---|
| 0 | OK | success (incl. "nothing to do") |
| 1 | ERROR | unexpected/internal error |
| 2 | USAGE | bad/missing arguments, incl. unresolvable required parameter |
| 3 | AUTH | authentication/authorization failure |
| 4 | CONFLICT | version conflict / merge conflict present |
| 5 | NETWORK | connectivity, TLS, timeout, rate-limit exhaustion after retries |
| 6 | NOT_FOUND | page/space/attachment/comment not found |
| 7 | STATE | local precondition: not initialized, dirty files would be clobbered, tampered frontmatter, lock held, invalid preserved block |
| 8 | PARTIAL | some operations succeeded, some failed (details in output) |
| 9 | UNSUPPORTED | operation not available on this Confluence flavor |

`diff` and `status` additionally: `--exit-code` makes "differences exist" return 10
(kept out of the error range so scripts can distinguish it).

### JSON envelope (all commands)

```json
{
  "confed": { "schema": 1, "version": "0.3.0", "command": "push", "ok": true,
              "exit_code": 0, "duration_ms": 4180 },
  "result": { … command-specific … },
  "errors": [ { "code": "CONFLICT", "page_id": "163842", "path": "…", "message": "…",
                "hint": "run `confed pull` then retry" } ],
  "warnings": [ … ]
}
```

Schemas are published in `docs/reference/json/*.schema.json` and only change with a
`confed.schema` bump. Long outputs stream NDJSON with `--json --stream` (one envelope-
free event object per line, final line = summary envelope).

Pages are addressed by **path or id** everywhere: `confed diff "Team Handbook/Onboarding.md"`
≡ `confed diff --page 163842`. Globs allowed where noted.

---

## Core commands

### `confed init`

Authenticate, bind directory to a space, create state.

- Flow: resolve base-url/credentials → detect flavor (host heuristic + API probe;
  `--flavor` overrides) → `whoami` verify (failure → exit 3) → store credentials
  (keyring; `--credential-store sqlite|keyring` to force) → resolve space (`--space`, or
  TTY picker listing writable spaces) → create `.state.db`, extend `.gitignore`
  (`.session.db`, `.state.db`, `.confed.lock`), generate `CLAUDE.md` + `AGENTS.md`.
- Flags: `--space KEY`, `--flavor`, `--credential-store`, `--no-agent-docs`, `--force`
  (re-init over an existing binding after confirmation).
- Idempotent: re-running with same space refreshes credentials only; different space →
  exit 7 unless `--force`.
- JSON `result`: `{ "base_url", "flavor", "user": {"account_id","display_name","email"},
  "space": {"key","id","name"}, "credential_store": "keyring", "created": [".state.db", …] }`

```bash
confed init --base-url https://acme.atlassian.net --space DOCS       # human: prompts for email+token
CONFED_TOKEN=$TOKEN confed init --base-url https://wiki.corp --space DOCS --json   # agent/CI
```

### `confed clone <url-or-space> [dir]`

`init` + `pull` in one step. `confed clone https://acme.atlassian.net/wiki/spaces/DOCS`
infers base-url and space from the URL; creates `dir` (default: space key). Same flags as
both; JSON result = init result + pull summary.

```bash
confed clone https://acme.atlassian.net/wiki/spaces/DOCS
confed clone DOCS ./docs --base-url https://wiki.corp --json
```

### `confed fetch`

Update `.state.db remote_pages` (+ attachment metadata, comment snapshots) without
touching working files. Resumable (`fetch_queue`); re-run continues after interruption.

- Flags: `--page <path|id>…`, `--since <ISO8601>` (CQL `lastmodified >=` narrowing),
  `--prune` is implicit (deletions are recorded, files untouched).
- Exit: 0; 5 network; 8 if some pages failed after retries.
- JSON `result`: `{ "fetched": 42, "unchanged": 310, "deleted_on_remote": 2,
  "failed": [], "resumed": false, "duration_ms": … }`

```bash
confed fetch
confed fetch --since 2026-08-01T00:00:00Z --json
```

### `confed pull [path|glob …]`

Fetch (skippable with `--no-fetch`) + materialize files/hierarchy/attachments/comments.

- Scope: positional paths/globs, `--page <id>`, `--label <l>`, `--cql '<query>'`.
- **Safety rule**: pages that are locally dirty and remotely changed are 3-way merged by
  default (`--merge`, see design 03 §5); with `--no-merge`, or for non-mergeable clobber
  (e.g. remote-deleted + locally-modified), pull **stops before writing anything in
  scope**, prints highlighted diffs of what would be lost, exits 7. `--force` overwrites
  local (after listing). Merge conflicts → markers in file, exit 4.
- `--dry-run`: report planned writes/merges without touching disk.
- JSON `result`: `{ "updated": [{"page_id","path","from_version","to_version"}…],
  "created": […], "deleted": […], "merged": […], "conflicted": [{"page_id","path"}…],
  "skipped_dirty": […], "attachments_downloaded": 7 }`

```bash
confed pull                                   # whole space
confed pull "Team Handbook/**" --json         # subtree, agent mode
confed pull --cql 'label = "runbook"' --dry-run
```

### `confed push [path|glob …]`

Upload local changes: bodies, title renames, moves (`parent_id`), labels, new pages,
deletions, attachments, comments (drafts in sidecars).

- Plan → verify (every page's base version must equal last-fetched remote version;
  stale → exit 4 with "pull first") → apply parent-first / delete child-first →
  commit base + rewrite managed frontmatter (`version` bumps).
- Flags: `--dry-run`/`--preview` (rendered per-page diff of exactly what will be sent —
  including storage-level diff for changed blocks), `--interactive` (per-page TTY
  confirm), `--message "<note>"` (version comment on the server), `--no-comments`,
  `--no-attachments`, `--allow-delete` (deletions are otherwise listed and skipped),
  `--force-version` (**dangerous**: push over a newer remote; explicit opt-in only).
- Conflicted pages are always refused (exit 4) until `confed resolve`.
- Partial failures: remaining ops continue; exit 8 with per-page errors.
- JSON `result`: `{ "pushed": [{"page_id","path","from_version","to_version","ops":
  ["body","title","labels"]}…], "created": […], "deleted": […],
  "attachments_uploaded": […], "comments_added": […], "skipped": [{"path","reason"}…] }`

```bash
confed push --dry-run                          # always preview first
confed push "Runbooks/**" --message "quarterly review" --json
confed push --interactive                      # human: confirm page by page
```

### `confed status`

Git-style summary from local hashes + last fetch (offline; `--fetch` to refresh first).

- Sections: modified / new / deleted (local), behind / remote-new / remote-deleted,
  diverged, **conflicted**, plus stale-base warning ("last fetch 9 days ago").
- Flags: `--fetch`, `--short` (porcelain `M/A/D/B/V/C` + path), `--exit-code`.
- JSON `result`: `{ "space": "DOCS", "last_fetch_at": "…", "clean": false, "pages": [
  {"page_id","path","local":"modified","remote":"ahead","state":"diverged",
   "base_version":7,"remote_version":9} … ] }`

```bash
confed status
confed status --fetch --json | jq '.result.pages[] | select(.state=="diverged")'
```

### `confed diff [path|glob …]`

- Default: base ⇄ local (what push would change), rendered Markdown diff (`similar`,
  colored, `--stat` summary mode).
- `--remote`: local ⇄ freshly-fetched remote. `--base`: full 3-way view (base/ours/
  theirs). `--storage`: show the storage-format diff that push would upload.
- Flags: `--exit-code` (10 if differences), `--name-only`.
- JSON `result`: `{ "pages": [{"page_id","path","change":"modified",
  "hunks":[{"header","lines":[{"tag":"+","text":"…"}…]}…],
  "frontmatter_changes":{"labels":{"added":["x"]}}}…] }`

```bash
confed diff "Team Handbook/Onboarding.md"
confed diff --remote --name-only --exit-code    # scriptable "am I behind?"
```

### `confed resolve [--ours|--theirs] <path|id>…`

Finish a merge: verify no conflict markers remain (or take a side wholesale), clear
`Conflicted`, advance base to the remote version. `--list` shows open conflicts.
Exit 7 if markers remain. JSON: `{ "resolved": [...], "remaining": [...] }`.

---

## Content & navigation commands

### `confed new <path>`

Scaffold `<path>.md` with writable frontmatter (`title` from filename, `parent_id`
inferred from directory) and no `confed:` block — push creates it. Flags: `--title`,
`--label <l>…`, `--template <file>`, `--push` (create immediately).
JSON: `{ "created_file": "…", "would_create_under": {"parent_id": "…"} }`.

```bash
confed new "Runbooks/Database Failover" --label runbook
confed new "Notes/2026-08-16" --push --json
```

### `confed mv <src> <dst>`

Rename and/or move: updates filename, frontmatter `title` (if `--rename-title`, default
when the slug changed) and `parent_id`; moves the sidecar dir; server move happens at
`push` (or immediately with `--push`). Also handles reordering: `--before <sib>` /
`--after <sib>` / `--position N`. JSON: `{ "page_id", "from": "…", "to": "…",
"title_changed": true, "parent_changed": false, "pushed": false }`.

```bash
confed mv "Drafts/Plan.md" "Roadmap/2026 Plan.md"
confed mv "Roadmap/2026 Plan.md" --after "Roadmap/2025 Plan.md" --push
```

### `confed rm <path|id>…`

Mark for deletion: deletes local file + sidecar, records tombstone; server delete happens
at `push --allow-delete` (or `--push` here, with confirmation / `--yes`). `--keep-local`
only tombstones. JSON: `{ "removed": [{"page_id","path","server_deleted":false}…] }`.

### `confed attach <page> <file>…`

Copy files into the sidecar dir, add to frontmatter attachment list; upload at push (or
`--push`). `confed attach --list <page>`; `confed attach --rm <page> <name>`.
JSON: `{ "page_id", "attached": [{"file","size","sha256"}…] }`.

### `confed comment <subcommand>`

Primary store is the sidecar (`.<slug>/comments.md`, design 02 §5); these commands are
structured accessors over it. All write ops edit the sidecar; `--push` syncs immediately.

- `list <page> [--unresolved] [--inline]` — threads with anchors/context.
- `add <page> [--body|-m TEXT | --editor] [--push]` — footer comment draft.
- `reply <comment-id> -m TEXT [--push]`
- `resolve <comment-id> [--push]` — Cloud only (exit 9 on DC).
- `add --inline <page> --anchor "text to highlight" -m TEXT` — Cloud only; anchor must
  match page text uniquely (exit 7 with candidates listed otherwise).
- JSON: `{ "comments": [{"id","kind","author","created","resolved","reply_to",
  "anchor":{"text","orphaned"},"body_markdown"}…] }`

```bash
confed comment list "Team Handbook/Onboarding.md" --unresolved
confed comment add 163842 -m "Reviewed for Q3." --push --json
```

### `confed log [path|id]`

Server version history (`--limit N`, default 20): version, author, date, message, plus
local base marker. `--local` shows the local `sync_log` instead. `confed diff
--versions 6..9 <page>` companion for historical diffs (stretch).
JSON: `{ "page_id", "base_version": 7, "versions": [{"number","author","when","message"}…] }`

With no page it widens to the bound space: `space = "KEY" and type = page order by
lastmodified desc` through CQL, so the server orders and `--limit` really is the N most
recent — one request on a space of any size. JSON: `{ "space", "cql", "pages":
[{"page_id","title","version","author","when","url","local_path"}…] }`. `--local` with no
page is the whole workspace's `sync_log`, each entry naming its page.

### `confed open <path|id>`

Open the page (or `--space` home, `--comment <id>`) in the browser via the stored base
URL. `--print` just prints the URL (default when not a TTY). JSON: `{ "url": "…" }`.

### `confed search <cql|text>`

CQL passthrough (auto-wraps bare text as `text ~ "…"` + space filter). Flags: `--limit`,
`--all-spaces`. Human output marks which results exist locally. JSON:
`{ "results": [{"page_id","title","space","url","local_path":null,"excerpt"}…] }`.

```bash
confed search 'label = "runbook" and lastmodified > now("-7d")'
confed search "database failover" --json
```

---

## Introspection & maintenance

### `confed spaces` — list visible spaces (`--mine`, `--limit`). JSON: `{ "spaces": [{"key","id","name","type"}…] }`.

### `confed whoami` — verify credentials, print user + flavor + capabilities. Exit 3 on bad auth (doctor's cheap cousin; ideal CI probe). JSON: `{ "user": {…}, "flavor": "cloud", "capabilities": {…} }`.

### `confed config`

`--list` (every resolved value **with its source**: flag/env/stored/default), `--get k`,
`--set k v` (writable: `space`, `concurrency`, `editor`, `default-labels`…), `--unset k`.
JSON: `{ "entries": [{"key","value","source"}…] }` (secrets always `"***"`).

### `confed doctor`

Diagnostics, each check `pass|warn|fail`: connectivity & TLS to base URL; auth validity
& token expiry (DC PATs); flavor/API version probe; keyring availability; `.session.db`
permissions; `.state.db` schema version & integrity (`PRAGMA integrity_check`);
`.gitignore` coverage; orphaned files (no `page_id` & never pushed); tampered managed
frontmatter; stale fetch age; converter self-test on a sample page. `--fix` applies safe
fixes (chmod, gitignore, schema migration). Exit 0 all pass / 1 any fail.
JSON: `{ "checks": [{"name","status","detail","fix_applied"}…] }`.

### `confed export <path|glob…> --format html|pdf|storage --out <dir>`

Offline export from state/working files (storage → standalone HTML; PDF via headless
conversion is stretch). JSON: `{ "exported": [{"path","out"}…] }`.

### `confed completion <shell>` / `confed --version`

Shell completions (bash/zsh/fish); version prints binary + JSON-schema + state-schema
versions (`--json`).

### `confed tui`

Full-screen browser: page tree with status badges, preview, diff view, conflict
resolver, push/pull actions. Requires TTY (exit 2 otherwise). No JSON mode.
