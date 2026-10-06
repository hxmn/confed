# Troubleshooting

Every confed failure has a numbered exit code, and every code has a section here. If you
are holding a JSON envelope, `errors[0].code` is the code's name and `errors[0].hint` is
usually the fix already.

`confed doctor` is the fastest first move for anything that smells like configuration: it
checks connectivity, credentials, permissions, `.gitignore` coverage, database integrity,
frontmatter, unparseable files, fetch age, and the converter, in one pass. `--fix` repairs
the safe ones.

- [Exit 1 — internal error](#exit-1--internal-error)
- [Exit 2 — usage or missing value](#exit-2--usage-or-missing-value)
- [Exit 3 — authentication failed](#exit-3--authentication-failed)
- [Exit 4 — conflict](#exit-4--conflict)
- [Exit 5 — network](#exit-5--network)
- [Exit 6 — not found](#exit-6--not-found)
- [Exit 7 — local state](#exit-7--local-state)
- [Exit 8 — partial success](#exit-8--partial-success)
- [Exit 9 — unsupported on this server](#exit-9--unsupported-on-this-server)
- [Exit 10 — differences exist](#exit-10--differences-exist)
- [Situations](#situations)

---

## Exit 1 — internal error

**Symptom.** `error: …` with no hint, often mentioning a file, SQLite, YAML, or a
conversion failure. Also produced by `confed doctor` when any check failed.

**Cause.** Something confed did not classify: an I/O error, a corrupt `.state.db`, YAML
that is syntactically broken, or a converter bug. For `doctor` it just means "read the
report".

**Fix.** Run `confed doctor` and read the failing check. If the state database is
reported corrupt, delete `.state.db` and rebuild it: `confed init` followed by
`confed fetch`. Your Markdown files are not affected — confed re-associates them by
`page_id` (they will pass through `untracked` until the first fetch). If it is a
conversion failure, `-vv` names the page and the block; the block is almost always a
` ```confluence ` fence that is no longer well-formed XML.

## Exit 2 — usage or missing value

**Symptom.** Either clap's own `error: unexpected argument …` with a usage line, or:

```text
error: missing API token
hint: pass --token, set CONFED_TOKEN, or run `confed init` in this directory (interactive prompts are disabled without a TTY)
```

**Cause.** An argument confed does not accept, or a required value that could not be
resolved from flag, environment, or stored config — and prompting was off because there
is no TTY, or because `--json` or `--non-interactive` was passed. This is the deliberate
alternative to hanging.

**Fix.** The hint names the flag and the environment variable. In CI, set the environment
variable; see [auth.md](auth.md#ci-and-automation). `confed config --list` shows what
resolved and from where.

`push --interactive` also exits 2 without a terminal — use `--dry-run` to preview instead.

## Exit 3 — authentication failed

**Symptom.** `error: authentication failed: …` or
`could not authenticate against https://…`, with the hint
`run 'confed init' to refresh credentials, or check CONFED_TOKEN`.

**Cause.** The server answered 401 or 403. The token is wrong, expired, revoked, or
belongs to an account without access to the space. On Cloud it is also what you get if the
e-mail address does not match the token.

**Fix.**

1. `confed whoami` — the cheapest possible check.
2. On Cloud, confirm `--user` is the e-mail of the account that created the token, and
   that the base URL includes `/wiki`.
3. On Data Center, confirm the PAT has not expired and that the base URL is the Confluence
   *context root* (often `https://host/confluence`, not `https://host`).
4. Re-run `confed init` to store a fresh credential, or export `CONFED_TOKEN`, which wins
   over anything stored.

A 403 on one page rather than the whole instance means space or page restrictions; confed
cannot tell the two apart from the status code alone.

## Exit 4 — conflict

**Symptom.** One of:

- `confed pull` printing `conflict <path>` and `N pages left conflict markers`.
- `confed status` exiting 4 with pages listed under "Conflicted".
- `confed push` reporting a skipped page whose reason is
  `remote is at version 9 but this file is based on 7; run 'confed pull'`.

**Cause.** Either a merge that left markers in a file, or a page whose base version is
behind the last fetched remote version. `status` and `pull` exit 4 for a conflicted page
whether or not `--exit-code` was given, because a conflict blocks pushing.

**Fix.** Always the same shape: `confed pull`, resolve, `confed push`. The full
walkthrough with real marker output is in
[sync.md](sync.md#the-conflict-workflow). Never treat exit 4 as "retry harder" — the retry
will be refused identically, by design.

## Exit 5 — network

**Symptom.** `error: network error: …` — a connection refused, a TLS failure, a timeout,
or `429` after the retries were exhausted.

**Cause.** Connectivity, a proxy, or rate limiting. confed already retries retryable
statuses up to six times with exponential backoff and full jitter, honouring a
`Retry-After` header when the server sends one, and it paces subsequent requests more
slowly after a 429. Reaching exit 5 means that was not enough.

**Fix.** Retry later. If it is persistent:

- Check the base URL with `curl -I <base-url>/rest/api/space` (Data Center) or
  `.../wiki/api/v2/spaces` (Cloud).
- Behind a corporate proxy, set `HTTPS_PROXY`; confed's HTTP client honours the standard
  proxy environment variables.
- If you are being rate limited during a large sync, lower `--concurrency` (see
  [large spaces](#a-large-space-is-slow-or-gets-rate-limited)).

## Exit 6 — not found

**Symptom.** `error: not found: no page matching 'Handbook/Onboarding.md'`, or
`not found: space DOCS`.

**Cause.** confed resolves a page reference by trying, in order: the path as given, the
path with `.md` appended, the argument as a page id, a page id known from the last fetch,
and finally the `page_id` in the file itself. Nothing matched. For a space, the key does
not exist or your account cannot see it.

**Fix.** Use the workspace-relative path exactly as `confed status` prints it (quote it —
titles contain spaces), or use the page id from the file's `confed:` block. For a page
somebody just created, `confed fetch` first. For a space, `confed spaces` lists what your
credentials can see.

## Exit 7 — local state

The busiest code. It always means a local precondition failed, and confed refused rather
than guessed. The message says which.

| Message | Meaning | Fix |
|---|---|---|
| `this directory is not a confed workspace (.state.db not found)` | You are outside a workspace. | `cd` into one, use `-C <dir>`, or run `confed init` / `confed clone`. |
| `N page(s) have local changes that 'pull' would overwrite` | A pull would destroy local work it cannot merge. **Nothing was written.** | `confed diff` to see what is at stake, then push your changes, or re-run with `--force` to discard them. |
| `tool-managed frontmatter was edited (…)` (as a `push` skip reason) | The `confed:` block no longer matches `.state.db`. | `confed pull --force <page>` rebuilds the block. |
| `another confed process (pid N) is using this directory` | The workspace lock is held. | Wait, or delete `.confed.lock` if the process is genuinely gone. |
| `.session.db is readable by other users (mode 644)` | Credential file permissions widened. | `chmod 600 .session.db`. The error's own hint offers `confed doctor --fix` as an alternative, but that does not work: `doctor` reads the session file too, so it fails with the same error before it can fix anything. Use `chmod`. |
| `<path>: frontmatter is missing 'title'` / `no YAML frontmatter found` | A page file is not parseable. | Add the key back, or `confed pull --force <page>`. |
| `<path> already exists` (`confed new`) | Name taken. | Pick another name or edit the existing file. |
| `this directory is already bound to space DOCS` | `confed init` with a different space. | `--force` if you meant it. |
| `unresolved` from `confed resolve` | Conflict markers remain in the file. | Edit the listed line numbers, or use `--ours` / `--theirs`. |

### A broken inline comment draft

`push` exits 7 with the file and line when a `<!--c new …-->` mark is never closed, has
no comment text, wraps no text, or sits inside a code block. Nothing has been uploaded.
Fix the mark — `<!--c new Your comment-->the text<!--/c new-->`, one line, no `--` — or
move the draft to the sidecar as `<!-- confed:new anchor="the text" -->`. A malformed
mark for an *existing* comment (a numeric id) is never an error: the next pull repairs
it.

## Exit 8 — partial success

**Symptom.** `confed fetch` or `confed push` finishes, reports some successes, and exits 8.
`result.failed` lists each page with its error. For a fetch, that includes a page whose
attachments or comments could not be read: the ones already in the workspace are kept —
a list that could not be read is never taken for an empty one — and the next fetch asks
again.

**Cause.** Some operations succeeded and some did not. confed does not roll back the
successes: a fetch that could not read one restricted page still updated the other three
hundred, and a push that had one page rejected still uploaded the rest.

**Fix.** Read `result.failed`. Each entry carries the page id, title and server error.
Common causes are a page restriction (fix the permissions), a 409 from a page that changed
between your fetch and your push (pull and retry), and a transient 5xx that outlived the
retries (just retry). Re-running the command retries only what is still outstanding.

```bash
confed push --json | jq -r '.result.failed[] | "\(.title): \(.error)"'
```

## Exit 9 — unsupported on this server

**Symptom.** `error: create inline comment (Data Center 7.13.0 does not answer
`rest/inlinecomments/1.0/comments` …) is not available on Confluence datacenter`, or
`resolve a page (footer) comment — Data Center only resolves inline threads`.

**Cause.** Data Center's public REST API cannot create, reply to or resolve inline
comments. confed uses the undocumented plugin API the page view itself calls
(`rest/inlinecomments/1.0`), captured on 9.5.4. A server that answers it with 404 or 405
does not have it (or has changed it); confed says which step failed. Footer comments have
no resolve on Data Center at all. On a major other than 9 confed warns before the first
call.

**Fix.** Do it in the browser, and report the server version (`confed whoami`). Reading
inline comments works everywhere.

## Exit 7 — "the text selection is wrong"

**Symptom.** Creating an inline comment fails with `refused it (HTTP 412)`, or confed
stops before sending with `is in <file> but not in the page on the server`.

**Cause.** Confluence checks the comment's text, which occurrence it is, and how many
there are, against its own copy of the page. confed computes those from the page's
storage, the way the server extracts text (non-breaking spaces kept, macros and code
excluded). A 412 means the two disagree — usually because the page changed on the server.

**Fix.** `confed pull`, then add the comment again with the text exactly as the page has
it; `--occurrence N` picks among repeats.

## Exit 10 — differences exist

Not an error. It is only ever produced by `--exit-code` on `status` or `diff`, and it sits
outside the error range precisely so scripts can distinguish "there are changes" from
"something went wrong".

```bash
if ! confed diff --remote --exit-code >/dev/null; then
  echo "the server has moved since our last pull"
fi
```

---

## Situations

### A stale base — "run `confed pull`"

**Symptom.** `push` skips a page with `remote is at version 9 but this file is based on 7`
and exits 4.

**Cause.** Your file's base version is older than what the last fetch saw on the server.
Pushing would either be rejected by Confluence or overwrite somebody's work.

**Fix.** `confed pull` merges the two sides — cleanly if the edits do not overlap — and
then `confed push` succeeds. There is deliberately no way to force a push over a newer
remote version.

Related: `confed status` warns when the last fetch is more than seven days old, because a
stale *remote* snapshot makes `status` optimistic. `confed fetch` refreshes it without
touching any file.

### Tampered frontmatter

**Symptom.** `confed status` lists the page under "Tool-managed frontmatter was edited",
`push` skips it, and `confed doctor` fails its `frontmatter` check.

**Cause.** A field inside the `confed:` block — `page_id`, `space_key`, `version` or
`schema` — no longer matches what `.state.db` recorded. Usually a find-and-replace that
was too broad, a merge tool that touched the file, or an agent that "tidied" the
frontmatter.

**Fix.** `confed pull --force <page>` rebuilds the block from the server. Your body is
overwritten by that, so if you have unsaved work in the file, copy it out first, then pull
and paste it back. To avoid it entirely: edit `title`, `labels`, `parent_id` and your own
keys freely, and leave everything under `confed:` alone.

### The lock is held

**Symptom.** `error: another confed process (pid 4711) is using this directory`, exit 7.

**Cause.** `.confed.lock` exists and its owning process is still alive. The lock is taken
by every mutating command — `fetch`, `pull`, `push`, and the `--push` forms of `new`, `mv`,
`rm` and `attach` — so two runs cannot interleave writes.

**Fix.** Wait for the other run. confed already removes a lock whose owner is dead, so a
persistent complaint about a pid that no longer exists means the pid was recycled; delete
`.confed.lock` by hand in that case. Note that the lock does not protect against your
editor saving a file mid-push; confed hashes at plan time for that.

### No OS keyring

**Symptom.** After `confed init`:

```text
warning: no OS keyring was available, so the token is stored in .session.db (mode 0600); prefer CONFED_TOKEN in CI
```

`confed doctor` reports the `keyring` check as `warn`, with the detail
`no OS keyring on this machine`.

**Cause.** Headless Linux, a container, or a CI runner with no Secret Service (GNOME
Keyring, KWallet). It is a warning, not a failure: confed falls back to the SQLite session
file, created `0600`.

**Fix.** In CI, set `CONFED_TOKEN` and ignore the fallback entirely — the environment
variable takes precedence over anything stored, and nothing sensitive is written to disk.
On a workstation, start a Secret Service and re-run `confed init --credential-store
keyring`, which fails loudly rather than downgrading silently.

If the *opposite* happens — `credentials are stored in the OS keyring but could not be
read` — the keyring is locked or the entry was removed. Unlock it, set `CONFED_TOKEN`, or
re-run `confed init`.

### Rate limits

**Symptom.** A long `pull` or `fetch` slows down, then eventually exits 5 with a message
mentioning 429.

**Cause.** Confluence Cloud rate limits per account. confed retries with exponential
backoff and jitter, honours `Retry-After`, and paces subsequent requests more slowly once
it has seen a 429 — but a hard limit will still win.

**Fix.** Lower the concurrency and try again:

```bash
confed pull --concurrency 2
confed config --set concurrency 2        # make it stick for this workspace
```

The default is 4 concurrent requests on Cloud and 8 on Data Center. Since `fetch` is
resumable, an interrupted large sync picks up where it stopped rather than starting over.

### A large space is slow or gets rate limited

**Symptom.** The first `clone` of a space with thousands of pages takes a long time, or
trips the limits above.

**Cause.** The first sync downloads every page body once. After that, `fetch` only
downloads pages whose version actually moved, so subsequent syncs are cheap.

**What helps.**

- **Scope the work.** `confed pull "Handbook/**"`, `confed pull --label runbook`, or
  `confed pull --cql '…'` materialize a subset. `confed fetch --since <RFC3339>` still
  lists the whole space (one cheap paged request) but only downloads the bodies of pages
  modified since that timestamp.
- **Separate the phases.** `confed fetch` then `confed pull --no-fetch` lets the slow
  network part run on its own; the fetch resumes if it is interrupted.
- **Tune concurrency** as above.
- **Do not commit `.state.db`.** It contains every page body, so it is large, it is
  rebuildable from the server, and it would conflict on every single sync.

### A comment changed in Confluence but not here

**Symptom.** `comments.md` or `confed comment list` shows a comment's old text, or a
comment that was deleted or resolved in the browser, after a `confed pull`.

**Cause.** A comment has a version of its own; changing one does not change its page's.
`pull` finds comments that were **added or edited** by searching for them, on every run.
It cannot find one that was **deleted** — a search does not return what is gone — and
Data Center may not report **resolving** a thread as a modification. A pull that printed
`could not ask the server which comments changed` did not search at all.

**Fix.** Name the page. Every one of these reads its comments directly:

```bash
confed pull "Team Handbook/Onboarding.md"
confed pull --page 1001
confed comment list 1001 --refresh
```

`confed comment list --json` reports `checked_at`: when confed last asked the server
about comment changes. See [sync.md](sync.md#comments-change-without-their-page).

### A file attached in Confluence is not here, or a deleted one still is

**Symptom.** A file somebody attached to a page — or dropped into a comment on it — is
missing from the page's sidecar and from `confed attach <page> --list`, while `confed
attach <page> --list --remote` shows it. Or the other way round: a file deleted in the
browser is still in the sidecar.

**Cause.** An attachment has a version of its own; attaching, replacing or deleting one
does not change its page's. `pull` finds files that were **added or replaced** by
searching for them, on every run, and downloads them. It cannot find one that was
**deleted** — a search does not return what is gone. A pull that printed `could not ask
the server which attachments changed` did not search at all. (confed 0.9.0 and older did
not look at all unless the page itself changed; the first fetch after upgrading lists
every page's attachments once and catches up.)

**Fix.** Name the page. Each of these lists its attachments directly, downloads what is
new and removes the copy of what was deleted:

```bash
confed pull "Team Handbook/Onboarding.md"
confed pull --page 1001
```

If the pull instead warns that a file `is not the copy confed downloaded, so it is kept`,
the file was changed locally and confed will not overwrite or remove it on its own:
`confed pull --force <page>` takes the server's version, `confed push` uploads yours.
See [sync.md](sync.md#attachments-change-without-their-page).

### Files that confed seems to ignore

**Symptom.** A Markdown file never appears in `confed status` as a page.

**Cause.** One of three by-design exclusions: the file has no YAML frontmatter (it is
listed under `untracked_files` instead), it is `CLAUDE.md` or `AGENTS.md` (generated,
skipped by name), or it lives in a directory whose name starts with `.` (sidecars,
`.git`, and confed's own state are never scanned).

**Fix.** Give the file a frontmatter block with a `title:`, or scaffold it properly with
`confed new <path>`, and it becomes a page that the next `push` creates.

### "untracked" pages after rebuilding `.state.db`

**Symptom.** Every page shows state `untracked` and `confed status` is not clean.

**Cause.** The files carry a `page_id` that `.state.db` has no base record for — exactly
what you get after deleting the database, or cloning a git repository that contains the
Markdown but (correctly) not the state.

**Fix.** `confed pull` stops with exit code 7 and lists the affected files rather than
writing over them, because with no base record confed cannot tell whether a file holds
work you never pushed.

If you know the files match the server — the usual case for a fresh clone — run
`confed pull --force`, which takes the server's copy for every one of them.

If you might have local edits, save them first (`cp -r . ../backup`), then pull with
`--force` and diff your backup back in. `pull --force` discards local content for these
pages without further warning, so make the copy before you run it.

This is covered by `pull_refuses_to_overwrite_an_untracked_file` in
`crates/confed/tests/cli.rs`.

### A page has no `storage.xml`, or it looks wrong

**Symptom.** A page's sidecar has no `storage.xml`, or its contents do not match what
Confluence shows.

**Cause.** The workspace was last synced by a version of confed that did not keep the
copy, the file was deleted, or something edited it. The copy is derived from the base
recorded in `.state.db`; it is never an input to a push.

**Fix.** Run `confed pull`. It rewrites the copy for every page in scope whose file is
missing or does not match, even when the page itself needs no other change.
