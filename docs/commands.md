# Command reference

Every flag listed here is one the binary actually accepts today; every example was run
against the mock server the end-to-end tests use. Run `confed <command> --help` for the
same list from the binary itself.

- [Global flags](#global-flags)
- [Output and JSON](#output-and-json)
- [Exit codes](#exit-codes)
- Setting up: [`init`](#confed-init), [`clone`](#confed-clone)
- Syncing: [`fetch`](#confed-fetch), [`pull`](#confed-pull), [`push`](#confed-push)
- Inspecting: [`status`](#confed-status), [`diff`](#confed-diff), [`log`](#confed-log)
- Resolving: [`resolve`](#confed-resolve)
- Authoring: [`new`](#confed-new), [`mv`](#confed-mv), [`rm`](#confed-rm),
  [`attach`](#confed-attach), [`comment`](#confed-comment)
- Finding things: [`search`](#confed-search), [`spaces`](#confed-spaces),
  [`open`](#confed-open)
- Maintenance: [`whoami`](#confed-whoami), [`config`](#confed-config),
  [`doctor`](#confed-doctor), [`export`](#confed-export),
  [`version`](#confed-version), [`completion`](#confed-completion)
- Interactive: [`tui`](#confed-tui)

## Global flags

These work before or after the subcommand: `confed --json status` and
`confed status --json` are the same command.

| Flag | Environment | Meaning |
|---|---|---|
| `--json` | `CONFED_JSON` | Machine-readable output on stdout. Implies `--non-interactive`. |
| `--non-interactive` | `CONFED_NON_INTERACTIVE` | Never prompt; a missing required value exits 2. |
| `--base-url <URL>` | `CONFED_BASE_URL` | Confluence base URL (Cloud includes `/wiki`; Data Center is the context root). |
| `--token <TOKEN>` | `CONFED_TOKEN` | API token (Cloud) or Personal Access Token (Data Center). |
| `--user <NAME>` | `CONFED_USERNAME` | Cloud account e-mail, or a Data Center basic-auth username. |
| `--space <KEY>` | `CONFED_SPACE` | Space key bound to this directory. |
| `--flavor <cloud\|dc\|datacenter>` | `CONFED_FLAVOR` | Skip flavor auto-detection. |
| `--concurrency <N>` | `CONFED_CONCURRENCY` | Maximum concurrent API requests. 0 or unset uses the client's default. |
| `-C <DIR>` | — | Run as if confed had been started in `<DIR>`. |
| `-y`, `--yes` | — | Answer yes to confirmations. *Accepted, but nothing prompts for confirmation today; see [known gaps](#known-gaps).* |
| `-v`, `-vv`, `-vvv` | — | More detail on stderr. |
| `-q`, `--quiet` | — | Errors only, and no progress. Conflicts with `-v`. |
| `--silent` | `CONFED_SILENT` | Hide the progress display; everything else is unchanged. |
| `--log <FILTER>` | `CONFED_LOG` | Tracing filter, e.g. `confed_api=debug`. Overrides `-v`/`-q`. |

Resolution order for every parameter is flag → environment → stored config → prompt; see
[auth.md](auth.md#precedence).

Pages are addressed by workspace-relative path *or* by page id almost everywhere, and
where a command takes several it also takes globs: `Handbook/**` matches a subtree, `*.md`
matches the top level, and a plain prefix such as `Handbook` selects everything under it.

## Output and JSON

Human output goes to stdout; log lines, warnings and errors go to stderr. Colour is
enabled only when stdout is a terminal and `NO_COLOR` is unset.

With `--json`, stdout carries exactly one envelope, success or failure:

```json
{
  "confed": {
    "schema": 1,
    "version": "0.1.0",
    "command": "status",
    "ok": true,
    "exit_code": 0,
    "duration_ms": 12
  },
  "result": { "space": "DOCS", "clean": true, "pages": [] },
  "errors": [],
  "warnings": []
}
```

`confed.schema` is bumped only for a breaking change. Adding a field to a `result` is
additive and does not bump it, so parse defensively and ignore what you do not know.
`errors[]` entries carry `code` (the exit code's name), `message`, and often `hint`.
`ok` is true for exit codes 0 and 10.

The schemas live in [`reference/json/`](reference/json/) — `envelope.schema.json` plus one
per command — and the end-to-end tests validate real output against them.

## Exit codes

| Code | Name | Meaning |
|---|---|---|
| 0 | `OK` | Success, including "nothing to do". |
| 1 | `ERROR` | Unexpected or internal failure: I/O, SQLite, a conversion bug. Also `doctor` with a failing check. |
| 2 | `USAGE` | Bad arguments, or a required value that could not be resolved without prompting. |
| 3 | `AUTH` | The server rejected the credentials (401/403). |
| 4 | `CONFLICT` | A version conflict, or an unresolved merge conflict. |
| 5 | `NETWORK` | Connectivity, TLS, timeout, or rate limiting that survived the retries. |
| 6 | `NOT_FOUND` | No such page, space, attachment, or comment. |
| 7 | `STATE` | A local precondition failed: not a workspace, local edits would be clobbered, tool-managed frontmatter was edited, the lock is held, `.session.db` is too permissive. |
| 8 | `PARTIAL` | Some operations succeeded and some failed; the details are in `result`. |
| 9 | `UNSUPPORTED` | The operation does not exist on this Confluence flavor (inline comment creation and comment resolution on Data Center). |
| 10 | `DIFFERENCES` | Differences exist. Only ever produced by `--exit-code`, and deliberately outside the error range. |

Two behaviours are worth knowing because they are not obvious from the table:

- **`status` and `pull` exit 4 whenever a page is conflicted**, with or without
  `--exit-code`. A conflict is not a difference to be reported, it is a state that blocks
  pushing.
- **`resolve` exits 7, not 4, when markers remain.** The file is a local state problem at
  that point: you were asked to edit it and it still has markers in it.

[troubleshooting.md](troubleshooting.md) has one section per code with symptoms and fixes.

---

## confed init

Authenticate, bind the directory to a space, and create local state. Verifies the
credential with a `whoami` call before writing anything, so a failure leaves no
half-built workspace behind.

| Flag | Meaning |
|---|---|
| `--credential-store <keyring\|sqlite>` | Force where the token is stored instead of preferring the keyring. |
| `--no-agent-docs` | Do not generate `CLAUDE.md` and `AGENTS.md`. |
| `--force` | Re-bind a directory that is already bound to a different space. |

Creates `.state.db` and `.session.db`, extends `.gitignore` with `.state.db`,
`.session.db` and `.confed.lock`, and writes the two agent contract files.

```bash
# Human, Cloud: prompts for the token with hidden input.
confed init --base-url https://acme.atlassian.net/wiki --user you@example.com --space DOCS

# Agent or CI: nothing prompts, everything is checked from the envelope.
CONFED_TOKEN=$TOKEN confed init --json \
  --base-url https://wiki.corp.example.com/confluence --space DOCS \
  --credential-store sqlite

# Point an existing workspace at a different space, deliberately.
confed init --space RUNBOOKS --force
```

## confed clone

`init` plus a first `pull`, into a new directory.

```
confed clone <SOURCE> [DIRECTORY] [init flags]
```

`SOURCE` is a space key or a space URL. A URL supplies both the base URL and the key, and
both Cloud (`/wiki/spaces/DOCS/...`) and Data Center (`/display/DOCS/...`, with or without
a context path) are understood. `DIRECTORY` defaults to the space key and must not already
exist with content in it.

```bash
confed clone https://acme.atlassian.net/wiki/spaces/DOCS --user you@example.com
confed clone DOCS ./docs --base-url https://wiki.corp.example.com/confluence
confed clone DOCS --json | jq '.result.pull.created | length'
```

## confed fetch

Download remote state into `.state.db`. Working files are never touched, so this is the
safe way to find out what changed. Interrupted fetches resume rather than restart.

| Flag | Meaning |
|---|---|
| `--page <PAGE>` | Restrict to these pages (path or id), and read their comments again. Repeatable. |
| `--since <RFC3339>` | Only pages modified at or after this timestamp. |

A comment added or edited in Confluence does not change its page's version, so `fetch`
also asks the server which pages were commented on since its last check and reads those
pages' comments again: `result.comments_refreshed` counts them, `result.comments_changed`
lists the ones that had changed. `--page` reads the named pages' comments regardless,
which is also what sees a deleted comment. `--since` skips the check. See
[sync.md](sync.md#comments-change-without-their-page).

Exits 8 if some pages failed after retries, or the comments of some could not be read;
the failures are listed in `result.failed` and repeated as warnings. A comment search that
fails is a warning (`result.comment_check_failed`), not a failure.

```bash
confed fetch
confed fetch --since 2026-08-01T00:00:00Z --json
confed fetch --json | jq '{fetched: .result.fetched, unchanged: .result.unchanged}'
```

## confed pull

Fetch, then materialize pages, attachments and comment sidecars into files.

| Flag | Meaning |
|---|---|
| *(positional)* | Paths or globs to pull. Default is the whole space. |
| `--page <ID>` | Restrict to these page ids. Repeatable. |
| `--label <LABEL>` | Only pages carrying this label (resolved on the server with CQL). |
| `--cql <QUERY>` | Restrict with an arbitrary CQL query. |
| `--no-fetch` | Use the state already in `.state.db`. |
| `--force` | Overwrite local changes instead of stopping. |
| `--reset` | Make every tracked page match the server again, discarding local edits, merges, conflicts, comment drafts and modified attachments. Files that exist only locally are left alone. Preview it with `--reset --dry-run`. |
| `--no-merge` | Do not three-way merge diverged pages; stop instead. |
| `--dry-run` | Report what would be written without writing it. |
| `--no-attachments` | Skip downloading attachments. |
| `--no-comments` | Skip writing comment sidecars. |

Safety rule: `pull` decides the whole plan before writing anything. If any page in scope
would lose local work, nothing is written at all and the command exits 7 listing what was
blocked. Diverged pages are merged by default; a merge that leaves markers exits 4.

Comments are pulled even when their page did not change: a page whose comments were
added or edited on the server is listed in `result.updated` with `"ops": ["comments"]`.
Naming pages (paths, `--page`, `--label`, `--cql`), `--force` and `--reset` read the
comments of every page in scope directly, which is what also picks up a comment deleted
on the server. See [sync.md](sync.md#comments-change-without-their-page).

```bash
confed pull                                  # the whole space
confed pull "Team Handbook/**"               # one subtree
confed pull --cql 'label = "runbook"' --dry-run
confed pull --json | jq '.result | {created: (.created|length), merged: (.merged|length), conflicted: (.conflicted|length)}'
```

## confed push

Upload local changes: bodies, titles, labels, parents, new pages, deletions, attachments,
and comment drafts.

| Flag | Meaning |
|---|---|
| *(positional)* | Paths or globs to push. Default is everything with local changes. |
| `--dry-run`, `--preview` | Show exactly what would be sent, without sending it. |
| `--interactive` | Confirm each page before uploading. Requires a terminal; exits 2 without one. |
| `-m`, `--message <TEXT>` | Version comment recorded on the server. |
| `--allow-delete` | Actually delete pages and attachments on the server that were deleted locally. |
| `--no-attachments` | Do not upload attachments. |
| `--no-comments` | Do not post comment drafts. |

`push` refuses, rather than forces, three situations, listing them under `result.skipped`:
an unresolved conflict, hand-edited tool-managed frontmatter, and a base version older
than the last fetched remote version. The first and third make the command exit 4; a
local deletion without `--allow-delete` is merely skipped and the command still exits 0.
A page the server rejects lands in `result.failed` and the command exits 8.

Attachment work is reported too, and `--dry-run` reports exactly what the real push
would do: `result.attachments_uploaded` and `result.attachments_deleted` hold
sidecar-relative paths, and an attachment gone locally without `--allow-delete` joins
`result.skipped` rather than being dropped quietly.

confed's own partial downloads (`*.confed-part`, left by a `pull` that died
mid-stream) are never push candidates and never appear in a plan. The next `pull` of
the page sweeps them out of the sidecar.

```bash
confed push --dry-run                                    # always look first
confed push "Runbooks/**" -m "quarterly review"
confed push --interactive                                # confirm page by page
confed push --dry-run --json | jq -r '.result.pushed[] | "\(.path) v\(.from_version)->\(.to_version) [\(.ops|join(","))]"'
```

## confed status

A git-style summary, computed from local state only.

| Flag | Meaning |
|---|---|
| `--fetch` | Refresh remote state from the server first. |
| `--short` | One line per changed page: a status letter and the path. |
| `--exit-code` | Exit 10 when anything differs. |

Status letters, in the spirit of `git status --short`: `M` modified, `B` behind, `V`
diverged, `A` new locally, `R` new on the server, `D` deleted locally, `X` deleted on the
server, `C` conflicted, `?` untracked.

```bash
confed status
confed status --short
confed status --fetch --json | jq -r '.result.pages[] | select(.state=="diverged") | .path'
```

Two comment counts ride along with each page: `comment_drafts` (body `new` marks plus
sidecar `confed:new` entries, posted by the next push) and `orphaned_comments` (open
inline comments whose text is gone). Both are omitted from the JSON when zero, and the
human output lists pages with drafts under *Comment drafts* — with `c` as the short
code when the page is otherwise unchanged.

## confed diff

Compare snapshots. By default it is base against your working file — what `push` would
change — and it needs no network at all.

| Flag | Meaning |
|---|---|
| *(positional)* | Paths or globs to diff. |
| `--remote` | Fetch, then compare local files against the remote state. |
| `--conf-format` | Diff the Confluence markup that `push` would upload, not the Markdown. `--storage` is accepted as an alias. |
| `--stat` | Summarize with per-page insertion and deletion counts. |
| `--name-only` | List changed paths only. |
| `--exit-code` | Exit 10 when there are differences. |
| `--base` | Also show what the *server* changed since the same base, so both sides of a divergence are visible together. |

```bash
confed diff "Team Handbook/Onboarding.md"
confed diff --remote --name-only --exit-code     # scriptable "has the server moved?"
confed diff --json | jq -r '.result.pages[] | "\(.path) +\(.additions) -\(.deletions)"'
```

## confed resolve

Finish a merge and clear the conflicted state.

| Flag | Meaning |
|---|---|
| *(positional)* | Pages to mark resolved. Default is every conflicted page. |
| `--ours` | Keep the local side of every conflict block and drop the markers. |
| `--theirs` | Keep the remote side instead. |
| `--list` | List unresolved conflicts and change nothing. |

Without `--ours`/`--theirs`, confed checks that you removed the markers yourself. If any
remain, nothing is cleared and the command exits 7 naming the line numbers.

```bash
confed resolve --list
confed resolve "Team Handbook/Onboarding.md"
confed resolve --theirs "Drafts/Scratch.md" --json | jq '.result.resolved'
```

## confed new

Scaffold a page file with writable frontmatter and no `confed:` block, so the next `push`
creates it on the server. The parent is inferred from the directory.

| Flag | Meaning |
|---|---|
| `--title <TEXT>` | Page title. Defaults to the filename. |
| `--label <LABEL>` | Label to apply. Repeatable. |
| `--template <FILE>` | Start from this Markdown file. |
| `--push` | Create it on the server immediately. |

```bash
confed new "Runbooks/Database Failover" --label runbook
confed new "Notes/2026-08-16" --push --json | jq -r '.result.push.created[0].page_id'
```

## confed mv

Rename, move, or reorder a page. The change is applied on the server by the next `push`
unless `--push` is given.

| Flag | Meaning |
|---|---|
| *(positional)* | `<SOURCE> [DESTINATION]` — source is a path or id. |
| `--before <SIBLING>` | Place before this sibling. |
| `--after <SIBLING>` | Place after this sibling. |
| `--position <N>` | Absolute position among siblings. |
| `--rename-title` | Also change the page title to match the new filename. |
| `--push` | Apply on the server immediately. |

```bash
confed mv "Drafts/Plan.md" "Roadmap/2026 Plan.md" --rename-title
confed mv "Roadmap/2026 Plan.md" --after "Roadmap/2025 Plan.md" --push
```

## confed rm

Delete a page locally and record the deletion. The server is not touched until
`confed push --allow-delete`, or immediately with `--push`.

| Flag | Meaning |
|---|---|
| `--keep-local` | Record the deletion but keep the file on disk. |
| `--push` | Delete on the server immediately — only the pages named; nothing else is pushed. |
| `--dry-run` | Say what would be removed (and, with `--push`, deleted on the server) and change nothing. |

`--json` reports each page under `removed` with `server_deleted`, true when the push
deleted it.

```bash
confed rm "Drafts/Obsolete.md" --push --dry-run
confed rm "Drafts/Obsolete.md"
confed push --allow-delete
confed rm 163842 --push --json
```

After a page is gone from the workspace its id still works where the server or the
history can answer: `confed log <id>`, `confed log --local <id>`, and `confed comment
list <id>` (read from the server, with a warning saying so).

## confed attach

Manage a page's attachments. Files are copied into the page's sidecar directory and
uploaded by the next `push`.

| Flag | Meaning |
|---|---|
| *(positional)* | `<PAGE> [FILES...]` |
| `--list` | List the page's attachments as of the last fetch. |
| `--remote` | With `--list`, ask the server instead of the local state. |
| `--rm <FILENAME>` | Remove an attachment by filename. |
| `--push` | Upload immediately; with `--rm`, delete on the server immediately. |

`--list` reads the local state, so it is only as fresh as the last `fetch`; the JSON
says which it is in `result.source` (`cache` or `server`).

`--rm` always removes the local file, and reports what happened on the server rather
than assuming. With `--push` it deletes the attachment there and then, and
`result.removed_on_server` is `true`. Without it the deletion is staged:
`result.staged` is `true`, and the next `confed push --allow-delete` applies it.
Removing a name that is neither in the sidecar nor on the page is exit 6, not a
silent success. `--rm --push` cannot delete the page itself, whatever else is
staged for it — only the attachment.

```bash
confed attach "Team Handbook/Onboarding.md" ./diagram.png
confed attach "Team Handbook/Onboarding.md" --list --remote --json | jq -r '.result.attachments[].file'
confed attach 163842 --rm old-diagram.png --push
```

## confed comment

Read and write page comments. The sidecar file `.<page>/comments.md` is the primary
store; these subcommands are structured accessors over it, and `--push` syncs
immediately.

| Subcommand | Flags |
|---|---|
| `comment list <PAGE>` | `--unresolved`, `--inline`, `--refresh` (read the page's comments from the server first) |
| `comment add <PAGE>` | `-m`, `--body <TEXT>`, `--anchor <TEXT>`, `--occurrence <N>`, `--sidecar`, `--push` |
| `comment reply <COMMENT_ID>...` | `-m`, `--body <TEXT>` (required), `--push` |
| `comment resolve <COMMENT_ID>...` | `--all <PAGE>`, `--push` (on Data Center, inline threads only) |
| `comment edit <COMMENT_ID>` | `-m`, `--body <TEXT>` (required); applied on the server at once |
| `comment rm <COMMENT_ID>...` | global `--yes` to skip the question; deletes on the server at once, replies included |

`list --json` gives each comment `resolved` and `thread_resolved` (a reply carries its
thread's status) and the page's `orphan_markers`: inline markers no comment claims.
`list` reads the local copy: `checked_at` is when confed last asked the server about
comment changes (null if it never has), and an edit made in Confluence since then is not
in it. `list --refresh` reads the page's comments from the server first, updates
`comments.md` and the marks in the page to match, and reports `refreshed: true` and
whether anything had `changed`.
A push reports `comments_added`, `replies_added` and `comments_resolved` separately;
`reply --push` lists each reply with the `parent` it was posted under — an inline thread
is one level deep, so a reply to a reply there goes to the thread's root.

On Data Center inline comments go through the server's undocumented inline-comment API;
page comments cannot be resolved there (exit 9; `resolve --all` lists them as skipped).

`--anchor` falls back to a `comments.md` draft when the text cannot carry a body mark
(inside a ```` ```confluence ```` block, read differently in the Markdown, or a
multi-line comment); `--json` says `"written_to": "sidecar"` and why.

Comment bodies are converted in the page's context: `[@Name](user:<userkey>)` (or
`user:account-id=…` on Cloud) is a mention, `[Title](Other.md)` a page link. `confed
user search <name> --json` gives each person's `userkey`, `account_id` and the mention to
paste.

Page arguments everywhere are relative to the current directory, like git's, then the
workspace root; a miss names where it looked and suggests the closest page.

`--anchor` writes the draft into the page body as a `<!--c new …-->` mark around the
text (see [format.md](format.md#inline-comments-in-the-page-body)). Text that appears
more than once is an error listing the occurrences with their lines; `--occurrence N`
picks one. `--sidecar` writes a `confed:new anchor="…"` entry into the sidecar instead,
which requires the text to be unique. `comment list --json` reports, for each inline
comment, whether it is `placed` in the body and on which `line`.

```bash
confed comment list "Team Handbook/Onboarding.md" --unresolved
confed comment add 163842 -m "Reviewed for Q3." --push
confed comment list 163842 --json | jq -r '.result.comments[] | "\(.author): \(.body_markdown)"'
```

## confed log

Version history for one page, or — with no page — recent activity across the space this
directory is bound to, most recently changed first.

| Flag | Meaning |
|---|---|
| `--limit <N>` | How many versions, or pages, to show. Default 20. |
| `--local` | Show confed's own sync log instead of the server's history. |

For one page the human output marks the version your local file is based on with `*`.
For the space it prints when, version, author, title and — for pages you have pulled —
the file to edit. The server does the ordering, so a space log is one request whatever
the space's size. `--local` never touches the network or your credential store.

```bash
confed log                                   # what changed in the space lately
confed log --limit 5 --json | jq -r '.result.pages[] | "\(.when) \(.title)"'
confed log --local                           # every sync confed has made here
confed log "Team Handbook/Onboarding.md"
confed log 163842 --limit 5 --json | jq -r '.result.versions[] | "v\(.number) \(.author)"'
confed log 163842 --local --json | jq -r '.result.entries[] | "\(.ts) \(.op) \(.result)"'
```

## confed search

Search the server. Plain words are wrapped into a CQL text query scoped to the bound
space; anything containing a CQL operator is passed through untouched. Results that exist
locally are annotated with their path.

| Flag | Meaning |
|---|---|
| `--limit <N>` | Maximum results. Default 25. |
| `--all-spaces` | Search every space, not just the bound one. |

```bash
confed search "database failover"
confed search 'label = "runbook" and lastmodified > now("-7d")'
confed search "vpn" --json | jq -r '.result.results[] | "\(.title)\t\(.local_path // .url)"'
```

## confed spaces

List the spaces visible to your credentials. `--limit <N>` defaults to 50. Useful before
`init` when you do not know the key.

```bash
confed spaces
confed spaces --limit 200 --json | jq -r '.result.spaces[] | "\(.key)\t\(.name)"'
```

## confed open

Open a page in the browser using the stored base URL. Omit the page to open the space.
`--print` prints the URL instead of launching anything.

```bash
confed open "Team Handbook/Onboarding.md"
confed open 163842 --print
confed open --print --json | jq -r '.result.url'
```

## confed whoami

Verify the credentials and report the user and the server's capabilities. The cheapest
possible CI probe: exit 3 means the token is wrong or expired.

```bash
confed whoami
confed whoami --json | jq '{user: .result.user.display_name, flavor: .result.flavor}'
confed whoami --json | jq -e '.result.capabilities.inline_comment_create'  # 1 on Data Center
```

## confed config

Inspect and change stored settings. Writable keys are `space`, `concurrency`, `editor`,
`comments.marks` (`full`, `ids` or `off` — how inline comments are shown in page bodies),
`base_url`, `flavor` and `rules_page_id` (the page that holds the space's
[rules for agents](#rules-for-agents-rules_page_id)).

| Flag | Meaning |
|---|---|
| `--list` | Show every resolved value and where it came from. Secrets show as `***`. |
| `--get <KEY>` | Read one value. |
| `--set <KEY> <VALUE>` | Write one value. `--set rules_page_id` with no value opens a page picker. |
| `--no-keychain` | Move the credential into `.session.db` so reading it never prompts. |
| `--force-keychain` | Move the credential back into the OS keychain. |
| `--unset <KEY>` | Remove one value. |

```bash
confed config --list
confed config --set concurrency 4
confed config --list --json | jq -r '.result.entries[] | "\(.key)=\(.value) (\(.source))"'
```

### Rules for agents: `rules_page_id`

A space can keep its own rules for coding agents — house style, what not to touch, who to
ask — on an ordinary Confluence page. Name that page and confed copies its content, as
Markdown, to the top of `CLAUDE.md` and `AGENTS.md`, above the generated contract. Claude
Code reads the first file and Codex the second, so either agent starts every session in
the workspace with the same rules, and the team edits them in one place.

```bash
confed config --set rules_page_id 163842             # by page id
confed config --set rules_page_id "Agent rules.md"   # or by the page's path
confed config --set rules_page_id                    # or choose it from a list
confed config --unset rules_page_id                  # take the rules out again
```

With no value, `--set rules_page_id` opens a picker over every page of the space: the
page tree, a preview of the page under the cursor, and a search box. Type to narrow the
list to the pages whose **title** contains every word you typed, in any order and any
case; `↑` `↓` move, `Enter` chooses, `Esc` clears the search and then cancels. The picker
needs a terminal; without one (or with `--json`) the command exits 2 and asks for the id.

The page has to be one this workspace knows (exit 6 otherwise — `confed fetch` first if it
is new on the server). A page that has been fetched but not pulled is accepted with a
warning, and its rules arrive with the next `confed pull`.

What is copied is the page **as last synced with the server**, not the working file: an
unpushed edit, or a merge left half done, never becomes an instruction. So the copy
changes only when `pull` or `push` moves the page (or the setting changes), and when it
does the command says so in its warnings — an agent already at work has the old rules in
its context and should read the file again. Links and images in the page are rewritten to
work from the workspace root. The copy sits between `<!-- confed:rules … -->` and
`<!-- /confed:rules -->`; do not edit it there — `confed doctor` reports a copy that has
drifted from the page, and `--fix` restores it.

The setting is stored in the workspace only (it has no flag or environment variable), and
`.state.db` is not committed, so each clone sets it once. Bear in mind what it means:
anyone who can edit that page can change what agents in this workspace are told to do.

## confed doctor

Check connectivity, credentials, and local state. Each check reports `pass`, `warn` or
`fail` with a specific next step; any failure exits 1. `--fix` applies the safe repairs:
`.gitignore` entries, missing or stale agent docs, and a rules copy that has drifted.

Checks cover the workspace root, `.state.db` integrity and schema version, `.gitignore`
coverage, the stored credential and its backend, keyring availability, `.session.db`
permissions, hand-edited frontmatter, unparseable page files, fetch age, the agent
contract files, a converter round-trip self-test, and a live `whoami` against the server.

The agent-docs check warns both when `CLAUDE.md` / `AGENTS.md` are missing and when
they were written by a different confed than the one running — the version is stamped
into the first line of each file. `--fix` rewrites them in either case; see
[`confed version`](#confed-version) for reading what changed first.

Once a [rules page](#rules-for-agents-rules_page_id) is set, an "agent rules" check
compares the copy at the top of both files with that page: it warns when they differ
(`--fix` copies the page again), when the page has not been pulled, and when the
workspace has no such page.

```bash
confed doctor
confed doctor --fix
confed doctor --json | jq -r '.result.checks[] | select(.status!="pass") | "\(.status)\t\(.name)\t\(.detail)"'
```

## confed export

Write pages out in another format, from local state — no server access needed.

| Flag | Meaning |
|---|---|
| *(positional)* | Paths or globs to export. |
| `--format <html\|storage>` | Output format. Default `html`. |
| `--out <DIR>` | Output directory. Default `export`. |

```bash
confed export "Runbooks/**" --format html --out /tmp/runbooks
confed export --format storage --json | jq -r '.result.exported[].out'
```

## confed version

Report what this binary is, and what changed in it. Needs neither a workspace nor
credentials.

| Flag | Meaning |
|---|---|
| `--changelog` | Also print the release notes for this build. |
| `--changelog --since <VERSION>` | Print every release newer than `<VERSION>` instead — what an upgrade from it brought. A `--since` that is not a version number exits 2. |

Three versions make up the compatibility contract, and all three are reported here:
the binary's own release, the `--json` envelope schema (`json_schema`), and the highest
`.state.db` schema this build understands (`state_schema`).

```bash
confed version
confed version --changelog
confed version --json | jq -r '.result | "\(.version) envelope=\(.json_schema) state=\(.state_schema)"'
confed version --changelog --since 0.1.0 --json | jq -r '.result.changelog[].version'
```

The release notes are compiled into the binary, so this works offline and outside a
checkout. That is what makes it usable from the generated `CLAUDE.md` / `AGENTS.md`
contract: those files record the confed that wrote them, and an agent that finds a
different `confed version` reads `--changelog --since <stamped version>` before
trusting them, then runs `confed doctor --fix` to regenerate them.

An empty answer is not an error: `--since` the current version exits 0 with an empty
`changelog` array and a warning on stderr.

## confed completion

Print a shell completion script for `bash`, `zsh`, `fish`, `powershell` or `elvish`. Needs
neither a workspace nor credentials.

```bash
confed completion zsh > ~/.zfunc/_confed
confed completion bash | sudo tee /etc/bash_completion.d/confed
```

## confed tui

Browse the space, read diffs, resolve conflicts and sync, interactively. It takes no
flags. Everything it does goes through the same sync engine and the same resolution helper
the headless commands use, so anything the TUI can do a script can do without it.

It needs a real terminal on both stdin and stdout and refuses to run otherwise — exit 2,
with nothing drawn into the pipe. It also fails early with the usual "run `confed init`"
error rather than opening on a blank screen. A workspace with no usable credentials still
opens: browsing, diffing and resolving are entirely offline.

```bash
confed tui
confed -C ~/docs/DOCS tui
```

## Progress

`fetch`, `pull` and `push` report what they are doing on a single line rewritten in
place on **stderr**, so stdout stays parseable:

```
Fetching 27/163  Team Handbook/Onboarding
```

The stages are `Listing pages` (no total is known until the listing returns), `Fetching`
(page bodies), `Writing` (files on disk — `Checking` under `--dry-run`) and `Pushing`.
The line is cleared when the command finishes.

It appears only when someone is watching, which means all of the following hold: stderr
is a terminal, and none of `--silent`, `--quiet` or `--json` was given. A redirected or
piped run therefore produces no carriage returns or escape codes at all, so logs and CI
output stay clean without anyone having to pass a flag.

Use `--silent` (or `CONFED_SILENT=1`) to turn it off while keeping ordinary output.

## Known gaps

Places where the accepted command line is ahead of the behaviour, or where
[design 04](design/04-command-reference.md) describes something that has not shipped:

- **`confed doctor --fix`** repairs `.gitignore` and the agent contract files. It cannot
  repair `.session.db` permissions: it opens the session file itself, so a too-permissive
  one makes `doctor` exit 7 before any fix runs. Use `chmod 600 .session.db`.
- **`-y` / `--yes`** applies to `push --interactive` (approves every page) and to
  `rm --push` (skips the "delete on the server?" confirmation). Nothing else prompts, so
  it has no effect elsewhere.
- **`push --force-version`** (design 04) does not exist. There is no way to push over a
  newer remote version; pull and merge instead.
- **`--json --stream`** NDJSON output (design 04) does not exist.
- **`spaces --mine`**, **`open --space` / `--comment`**, **`comment add --editor`**,
  **`export --format pdf`** and **`config --set default-labels`** are described in
  design 04 but are not implemented.
- **`confed --version`** prints only the binary version; `confed version` is the one
  that reports the JSON-schema and state-schema versions too.
- **`errors[]` entries carry `code`, `message` and `hint` only** — the `page_id` and
  `path` fields sketched in design 04 are not emitted.
