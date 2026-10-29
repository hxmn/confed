# 07 — Instance roots and moves between spaces

Status: implemented in 0.11.0 (`confed instance`, alias `confed i`); the move API
findings in §5.1 corrected in 0.12.0. `confed instance cp` is not implemented yet (§8).

## 1. Problem

A confed workspace is one directory bound to one space. Someone working in several
spaces of one Confluence ends up with a folder of workspaces that confed knows nothing
about:

```
Confluence/          ← not a workspace; nothing ties the three together
├── DOCS/   .state.db, .session.db, CLAUDE.md, pages…
├── ENG/    .state.db, .session.db, …
└── OPS/    .state.db, .session.db, … (rules_page_id set)
```

- The credential is stored three times, so rotating a token means `confed init` three
  times.
- Every command runs in one space; nothing reports across them.
- A page cannot move to another space. Moved in the browser, both workspaces drift: the
  source still tracks the page, the destination does not know it.
- Links between the spaces are common, and a move turns links within a space into links
  across spaces and back.

## 2. The model

An **instance root** is a directory holding one workspace per space of one instance. It
is marked by `.confed-instance.db` (SQLite, like `.state.db`):

| Table | What it holds |
|---|---|
| `meta` | `schema_version`, `base_url`, `flavor`, `created_at` |
| `spaces` | `space_key`, `space_id`, `dir` (relative to the root), `added_at`, `enabled` |
| `journal` | operations that span workspaces: `id`, `op`, `state` (last step finished), `plan` (JSON), `error` |

- **Members are ordinary workspaces.** Nothing about a workspace changes when it joins:
  `confed -C ENG status` works as before, and a workspace taken out of the root keeps
  working on its own. A root cannot be a workspace, nor be inside one.
- **Discovery** walks up from the current directory, as for `.state.db`: from inside a
  member, confed finds the member first and the root above it. `-C` works as everywhere.
- **The root's `.gitignore`** covers `.confed-instance.db`,
  `.confed-instance-session.db`, and `*/.state.db`, `*/.session.db`, `*/.pages.db`,
  `*/.confed.lock`.
- **Directories** are named for the space key. A personal space's key starts with `~`,
  which a shell expands to a home directory, so a personal space goes to **`@` and its
  owner's username** (`@alice`). On newer Data Center spaces and on Cloud the key is
  `~` and a long id, so the owner is looked up. If that fails, or the username is not a
  safe directory name, the folder is `@` and the id. No space key can contain `@`, and
  the name never needs quoting. Personal spaces are left out of
  `clone --all` unless `--type personal` asks for them, as with `confed clone --all`.

### Credentials

The root keeps one credential in `.confed-instance-session.db` (mode 0600), or its
keyring entry, which has the same shape as a workspace's `.session.db`. A workspace
resolves its credential in this order:

1. `--token`
2. `CONFED_TOKEN`
3. its own `.session.db`
4. the instance root's shared session, found by walking up
5. a prompt, at a terminal

`confed instance adopt --move-credentials` verifies the shared credential with
`whoami` before it removes any workspace's own. A workspace's keyring entry is deleted
only if it is not the same entry the shared session now uses: a workspace and the root
on the same base URL and user share one keyring account, and clearing it would take the
shared secret with it.

Fan-out commands keep the order. A member that still has its own `.session.db` uses that
credential, and the others use the root's. Every client draws on **one request
semaphore**, so `--concurrency` caps requests across all spaces together, not per space.

## 3. Fan-out

`status`, `fetch`, `pull`, `push` and `doctor` run the single-space command in each
member, in-process, with a context of its own. That context never prompts and prints
nothing directly. The JSON is:

```json
{ "confed": { "command": "instance", … },
  "result": { "action": "status", "spaces": [
    { "space_key": "DOCS", "dir": "DOCS", "ok": true,  "exit_code": 0, "result": { … } },
    { "space_key": "ENG",  "dir": "ENG",  "ok": false, "exit_code": 3, "errors": [ … ] } ] },
  "errors": [ { "code": "AUTH", "message": "ENG: …" } ], "warnings": [ … ] }
```

A space's `result` is the single-space result unchanged, so its schema still applies.
The exit code is 0 when every space succeeded, the shared code when every space failed
the same way, and 8 otherwise. `clone` exits 8 on any failure, as `clone --all` does.
`push` needs `-m`. It always runs every space's dry run first, then asks (or takes
`--yes`) and pushes only the spaces that have work.

The envelope gains no top-level field: `errors` may now hold several entries, one per
failed space. `confed.schema` stays 1.

## 4. Addressing

From the root a page is a path from the current directory (`DOCS/Team Handbook.md`),
`KEY:path` (`DOCS:Team Handbook.md`), or a page id, looked up in every registered
`.state.db`. A key is capitals and digits, or `~user`, so `C:\…`, URLs and file names
with a colon in them are never taken for one. A single-space command only reads
`KEY:path` when `KEY` is its own space or a registered member.

As a move destination, an existing page becomes the new parent. `KEY:` alone means the
top of that space: under its home page, or beside it with `--root`. A path that is not a
page yet names the parent (its directory) and the new title (its file name), as
`confed mv` does. `--before`, `--after` and `--position` work as in `confed mv`.

## 5. Moving pages between spaces

### 5.1 Which server API moves a page to another space

*Corrected in 0.12.0.* The first version of this note said the REST move endpoint was
documented for Data Center. That was wrong: Atlassian documents
`PUT …/content/{id}/move/{position}/{targetId}` for Cloud only, and a probe of a 9.x Data
Center found no such route.

| Candidate | Data Center 9.x | Cloud | confed |
|---|---|---|---|
| `PUT /rest/api/content/{id}/move/{position}/{targetId}` (`append`, `before`, `after`) | Not documented, and absent on the server probed. | Documented, under `/wiki`. The target's space becomes the page's; descendants move with it. | **Used first**, on both flavors. |
| `POST /pages/movepage.action`, the web UI's "Move page" dialog | What the browser uses. Keeps page ids, history, comments and attachments, and moves the descendants. Not a documented API. | — | **Data Center fallback**, when the REST route is missing. |
| `PUT /rest/api/content/{id}` with another `space` and `ancestors`, version + 1 | Not documented as a space change. Whether the server accepts it, and whether children follow, is not something confed can rely on. | v2 has no space field on update. | Not used. |
| Copy, then delete | Always possible. | Always possible. | **Never.** New ids, no history, comments and inline markers lost. |

`move/append/{target}` puts the page under the target. `move/before|after/{target}`
makes it the target's sibling: `KEY: --root` is `after` the home page, and `--position N`
is `before` the Nth sibling.

**The Data Center fallback**, as a 9.5.4 browser was seen to do it. When the REST route
answers 405, or a 404 that is Jersey's `null for uri` rather than "no such content",
confed sends the browser's form:

- `POST pages/movepage.action`, with `X-Requested-With: XMLHttpRequest`
- fields: `pageId`, `spaceKey` (the destination), `targetTitle` (the new parent **by
  title**, not by id), `position` (`append`; `above`/`below` for before/after, not yet
  seen in a browser and checked by the read-back), `mode=ASYNC`, and `atl_token`
- the session behind the token is opened as a browser's is, by viewing the page
  (`pages/viewpage.action`): its cookie goes with the form
- `X-Atlassian-Token: no-check` goes too. The browser does not send it, but it lets a
  script through where no session can be had.

A 403 "Session Expired" page from the XSRF check is exit 9.

The server answers **before it has moved anything**: `200` with
`{"complete": false, "status": "Starting…", "longTaskId": …}`, the page still in its
old space. It moves the pages in a background task. confed follows that task through
`GET rest/api/longtask/{id}` until it reports finished (or 100%). A task that finishes
unsuccessfully is an error carrying its messages. 0.13.0 read the page back at once,
saw it still in the old space, and wrongly reported exit 9 for a move that was
happening.

**Read-back, on every move.** confed then waits, polling with a growing interval for up
to `--wait` seconds (120 by default), until the root and every page under it are in the
destination space. If they are not by then, the result is exit 7: the server may still
be moving them. A later `confed instance resume` waits again and **never asks the
server to move them a second time**, since the plan records that it already asked. If it is in the right space under the wrong
parent, confed moves it under the right one with the in-space update Data Center does
support (`PUT content/{id}` with new `ancestors`). If that does not take either, the
journal stops at `planned` with exit 7 and a hint. `abort` asks the server where the
page is before undoing anything.

**Capability probe.** It runs in every `confed instance mv` pre-flight, dry run
included, last, so a server that cannot move does not hide the other problems. It also
runs on request in `confed whoami --probe-move`, `confed doctor --probe-move` and
`confed instance doctor --probe-move`, and never unasked: it is a write request against
a real page, even though nothing moves.

- **REST route:** `PUT rest/api/content/0/move/append/0`. Nothing has id 0, and no real
  page is named in the write. "No such page" and "no such route" are both a 404; the body
  tells them apart. A server with the route reaches its resource, which answers in its
  JSON that it found no content. One without it answers Jersey's `null for uri`, or 405.
- **Web action (Data Center):** a POST of the move form for page **0**, with
  `X-Atlassian-Token: no-check`, then if refused with a session token. The XSRF check
  runs before the action looks for the page, and nothing has id 0, so nothing can move.
  The probe never sends the form for a real page: past the check, a request with no
  target could make the action do something with the page.

Results:

- **available:** the REST route exists.
- **web_action:** a script gets past the action's XSRF check.
- **web_action_refused:** it does not, with either method. Exit 9, and the dry run still
  shows the full plan, with the reason in `result.blocked`.
- **unavailable:** neither route exists (Struts' "There is no Action mapped…").
- **unknown:** no pulled page to ask about, or a 404 that does not say which. This is a
  warning; the move finds out.

**What was verified, and how.** The REST route's absence on Data Center comes from
Atlassian's documentation and a probe of a 9.x server. The fallback was built against
the browser's endpoint and tested against a stateful fake of these routes
(`crates/confed/tests/instance.rs`) and wire-format tests (`crates/confed-dc/tests/dc.rs`).
**It has not yet moved a page on a live Data Center.** On one 9.5.4 server, the form
without a token was refused with the 403 "Session Expired" page. Whether no-check or a
session token passes the check there is the open question. The form field names are those of
the move dialog and are not a published contract, which is why every move is read back.
A first real move should be a test page in a test space.

**Within one space.** `confed mv --before/--after --push` on Data Center uses the same
REST route for sibling ordering, so it probably fails on a server without it. Moving
under another parent within a space uses the update with `ancestors`, which Data Center
supports.

### 5.2 Does Confluence rewrite links when a page changes space?

Confluence's link refactoring is documented for the browser's move and rename. It updates
links to the moved page from other pages, and gives the links from the moved page to
pages left behind their old space's key. Whether the REST move endpoint triggers the same
refactoring on every 9.x release, and whether it saves new page versions when it does,
is not documented. confed therefore depends on neither:

- After the move, every page the plan expected to need fixing is read from the server and
  checked again. A page the server already fixed is left alone and listed in
  `links_by_server`, with a warning.
- A page still broken is fixed on the server, as one new version with the move's `-m`
  message, and listed in `links_rewritten`.

So whatever a server does, links end up resolving, and the output says who fixed them.

### 5.3 Pre-flight: every check before any write

Both spaces are fetched first, under both locks. Then:

1. **Source and destination** are registered workspaces on this instance and are
   different spaces. A file that was never pushed has no page to move on the server, so it
   moves as a file into the destination's tree, and the next push there creates it.
2. **In sync.** Every page in the subtree, and the new parent, must be unchanged
   locally and on the server: no local edits, conflicts, tampered frontmatter, unpushed
   comment drafts, attachment changes, or new pages filed under a moved one.
   `--push-first -m …` pushes the subtree's work first.
3. **Titles.** Every title is checked against the destination's page listing from the
   server, case-insensitively. `--on-conflict suffix` appends ` (KEY)`; `--title-map
   FILE` takes a JSON object of new titles keyed by page id or title. Titles are changed
   on the server just before the move, and `abort` changes them back.
4. **Permissions.** If the server answers `expand=operations`, the user needs
   `update` and `delete` on the root and `create` where it lands. Otherwise there is a
   warning, and a 403 from the move itself is reported as exit 3 with what is needed.
5. **Special pages.** A space's home page is refused. If the subtree holds the source's
   `rules_page_id`, the move needs `--yes`, and the source's setting is cleared afterwards.
6. **Restrictions** on any page in the subtree are listed. They move with the pages.
7. **Links.** The stored body of every page in every registered workspace is scanned
   (§6). With `--rewrite-links` (the default), a page that needs a fix must not have
   local changes.
8. **Placement.** Each page's path in the destination is worked out by the
   destination's own rules (the same `plan_from_records` pull uses), and nothing may be
   in the way on disk.

Problems are collected and reported together, as exit 7 (exit 2 for bad arguments).
`--dry-run` stops here and prints the plan.

### 5.4 Steps and the journal

Each step is recorded in the journal when it finishes. Each step checks what is already
true before it acts, so running it again is safe:

| Step | What it does | On resume |
|---|---|---|
| `planned` | journal entry created | — |
| `server_moved` | title changes, then `move_page_to` on the root, then read back | asks the server where the root is; does not move twice |
| `local_dst_written` | page cache (`page_versions`, `page_extras`), then in one transaction the `pages`, `remote_pages`, `attachments`, `comments` and `removed_attachments` rows and any `users` the destination lacks; then each `.md` and sidecar, moved to its destination path; `confed.space_key` and the root's `parent_id` rewritten | skips rows already there; moves files from wherever they are |
| `local_src_removed` | rows deleted from the source (and its cache, its fetch queue), empty directories pruned | idempotent |
| `links_rewritten` | §5.2 | skips pages already done |
| `done` | a scoped pull in every space written to; checks that each page is tracked once, in the destination, clean | — |

Bodies are not downloaded again. A moved page keeps its base storage and cache, and
its render key is cleared. The closing pull then renders it again in the destination's
context, which is what turns `[Team Handbook](../Team Handbook.md)` into
`[Team Handbook](<base>/display/DOCS/Team%20Handbook)`. A page whose server version
changed beyond the move is fetched by that pull like any other.

**Locks** are taken in space-key order, and a held lock fails at once with exit 7: the
source and destination first, then any other space whose links are rewritten.

**Failure.** A move that stops part-way keeps its codes for network (5), auth (3),
conflict (4) and not found (6). Anything else is exit 7, as a local state to finish. The
hint names `confed instance resume <id>`, and `confed instance doctor` reports the
unfinished entry. `confed instance abort` is allowed only before `server_moved`, and
reverts any title changes. After that, the way back is a move in the other direction.

`confed instance doctor` also reports pages tracked by two workspaces at once, and
(online) pages whose space on the server is not their workspace's.

## 6. Links

A link by title names a space only when it is another one: `<ri:page
ri:content-title="T"/>` means "T in this space". So a move changes four kinds of links:

| Link | Before | After |
|---|---|---|
| from a page left behind, to a moved page | `ri:content-title="T"` | `ri:space-key="DEST" ri:content-title="T"` |
| from a moved page, to a page left behind | `ri:content-title="Q"` | `ri:space-key="SRC" ri:content-title="Q"` |
| between two moved pages | `ri:content-title="T"` | unchanged |
| from any page naming the source explicitly | `ri:space-key="SRC" …T` | `ri:space-key="DEST" …T` (or no key with `--relativize`, when it ends up in its own space) |

A renamed page's links also get its new `ri:content-title`. The rewrite edits only
those two attributes, in place. Everything else in the storage stays byte for byte: macro
bodies, `ac:macro-id`, the inside of ```` ```confluence ```` blocks, and links by
`ri:content-id` (Cloud), which survive a move as they are. Titles are compared as text,
with XML entities decoded, so Cyrillic titles and `&amp;` match. Running the rewrite
twice changes nothing the second time.

In the Markdown, a link to a page in another space is written
`[Title](<base url>/display/KEY/Title)`, as the workspace agent docs already say. A page
in the destination that names a moved page by its new space renders as a relative file
link on the next pull. With the explicit key it still resolves; `--relativize` drops the
key.

### 6.1 Judged by what exists (0.14.0)

Reading links by how they are written alone went wrong on a real server:

- **Partial, silent refactoring.** Data Center 9.5.4's own link refactoring fixed only
  links that named the old space explicitly. It did not fix plain links to or from the
  moved pages, and it changed the storage it did fix in place, without a new version.
- **A plain link that was already broken.** On the way back, such a link was read as
  meaning "this space", and rewritten to point there, where the page was not.

So every link the move concerns is now judged by what exists. Existence comes from the
registered workspaces' records, with the moved pages where the move put them. For
other spaces, confed asks the server with an exact title lookup
(`GET rest/api/content?spaceKey=&title=`, from the database, not the search index).

- If the page a link meant does not exist, the link is left as it is, and reported in
  `links_broken`. If the move happens to make it resolve, as a plain link carried back
  to its page's space does, it is not reported.
- A link that already reaches the page it meant is left as it is.
- Otherwise, `ri:space-key` and `ri:content-title` are set so that it does.

A page counts as fixed by the server (`links_by_server`) only when the server's storage
really differs from confed's. Such silent changes, with no new version, are invisible
to `fetch`, which goes by versions. So the closing step compares the server's storage
of every page the move touched with confed's, and takes in what differs (clean pages
only), so the files show the server's text.

In the Markdown, a link that names another space is never written as a link to a local
file of the same title. Before 0.14.0 it could be, which made a link into the wrong
space look correct.

## 7. Single-space guardrails

- A `parent_id:` naming a page this workspace does not track is looked up. If it is in
  another space, `push` refuses that page: it goes into `skipped` and `cross_space`,
  with an `errors` entry, exit 7, and a hint naming `confed instance mv`. Pages in this
  space push as before.
- `confed mv` with a destination inside another member's directory, or `KEY:path` of
  another member, exits 2 and prints the equivalent `confed instance mv` command.
- Inside a member, a single-space command given another member's `KEY:path` exits 2.
  At the root, a single-space command says that the root is not a workspace and points
  at `confed instance` and `-C <KEY>`.

## 8. Not done

- **`confed instance cp`**, copying a page tree into another space as new pages, is not
  implemented. The design stands for when it is. It would use the server's
  page-hierarchy copy if a probe finds it, and otherwise create pages parent first
  through the push path. Attachments would be uploaded again and labels copied, but not
  comments. Links inside the tree would point at the copies, and links out of it at the
  originals. It would share the pre-flight, title handling and journal of `mv`.
- Moving or copying between instances, moving whole spaces, and space permissions or
  settings are out of scope, as is an instance-wide MkDocs site.
