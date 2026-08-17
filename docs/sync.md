# How syncing works

confed's whole model is three snapshots and the relationships between them. Once you have
those, every command and every exit code follows.

- [The three snapshots](#the-three-snapshots)
- [Page states](#page-states)
- [What each command does](#what-each-command-does)
- [The conflict workflow](#the-conflict-workflow)
- [Optimistic version checking](#optimistic-version-checking)
- [Safety properties](#safety-properties)

## The three snapshots

| Snapshot | Storage | Written by |
|---|---|---|
| **base** | the `pages` table in `.state.db` | confed only, and only from something the server confirmed — a fetch result or a push response, never a local edit |
| **local** | the `.md` files in the working tree | you |
| **remote** | the `remote_pages` table in `.state.db` | `confed fetch`, from the server's page list and bodies |

Alongside the page body, base records the storage XML the Markdown was generated from, the
block map used for surgical patching, a hash of everything you control in the file, the
version number, and a `sync_state` of `clean` or `conflicted`.

Deriving status is then a pure function of local hashes and two version numbers, which is
why `status` and `diff` are instant and work with no network:

- **local dirty** — the file's content hash differs from base's `markdown_hash`.
- **remote ahead** — remote's version is greater than base's version, and the page has not
  been deleted on the server.

```
                    remote ahead?
                       no        yes
                  ┌──────────┬──────────┐
 local dirty?  no │ unchanged│  behind  │
               yes│ modified │ diverged │
                  └──────────┴──────────┘
```

Two flags override that square: a page whose base `sync_state` is `conflicted` is always
`conflicted`, and a page the last fetch found missing from the server is always
`remote_deleted`.

## Page states

`confed status --json` reports one of these ten values per page.

| State | Short | What it means | What caused it |
|---|---|---|---|
| `unchanged` | ` ` | Local matches base, base matches remote. | Nothing to do. |
| `modified` | `M` | Edited locally; the server has not moved. | You edited the file, its title, labels or `parent_id`. `push` sends it. |
| `behind` | `B` | The server moved ahead; no local edits. | Someone else edited the page. `pull` fast-forwards the file. |
| `diverged` | `V` | Both sides moved. | You and someone else edited the same page. `pull` three-way merges. |
| `local_new` | `A` | A file with no `confed:` block. | You ran `confed new`, or wrote a file with just a `title:`. `push` creates the page. |
| `remote_new` | `R` | A page the last fetch found that has no local file. | Someone created a page, or you fetched without pulling. The `path` is empty because there is no file yet. |
| `local_deleted` | `D` | Base has a record but the file is gone. | You deleted the file, or ran `confed rm`. `push --allow-delete` deletes it on the server. |
| `remote_deleted` | `X` | The page vanished from the space listing. | Someone deleted or trashed it. `pull` removes the local file — unless you have local edits, in which case it stops. |
| `conflicted` | `C` | A merge left conflict markers in the file. | A `pull` that could not merge cleanly. `push` refuses the page until `confed resolve`. |
| `untracked` | `?` | The file names a page id confed has no base record for. | Usually a deleted or rebuilt `.state.db`. `fetch` repairs it. |

Two extra buckets appear in `status` output but are not page states:
`untracked_files` (Markdown files with no frontmatter — confed ignores them) and
`unreadable` (files that failed to parse, reported as warnings without aborting the scan).

`push` acts on `modified`, `local_new`, `local_deleted` and `diverged`. `pull` acts on
`behind`, `remote_new`, `remote_deleted` and `diverged`.

## What each command does

**`fetch`** lists the space, records every page's metadata into `remote_pages`, downloads
the body of any page whose version moved (or whose body it never had), and marks pages
that disappeared from the listing as deleted. Bodies are queued in `.state.db` before they
are downloaded, so an interrupted fetch resumes instead of restarting — the JSON result
reports `"resumed": true` when it did. Nothing on disk changes.

**`pull`** fetches first (unless `--no-fetch`), then plans and writes. Planning happens in
full before anything is written: if any page in scope would lose local work, nothing is
written at all and the command exits 7. Per page:

| Situation | Action |
|---|---|
| `remote_new` | create the file |
| `untracked` | block, because there is no base to compare against; `--force` overwrites |
| `behind` | overwrite from the server |
| `diverged` or `conflicted` | three-way merge (or, with `--no-merge`, block; with `--force`, overwrite) |
| `modified` | leave it alone — pull has nothing to add |
| `unchanged`, but rendered from different inputs | re-render from the same server content |
| `remote_deleted` | delete the file, unless it is locally dirty, in which case block |
| `local_deleted` | leave it deleted, unless `--force` recreates it |

An `untracked` page is a file whose `page_id` has no base record, which is what you get
after `.state.db` is deleted or rebuilt — including a fresh clone of a repository that
tracks the Markdown but not the state. confed has nothing to compare such a file against,
so it cannot tell whether the file holds work you never pushed, and `pull` stops with exit
code 7 rather than guessing. `pull --force` takes the server's copy. See
[troubleshooting.md](troubleshooting.md#untracked-pages-after-rebuilding-statedb).

Frontmatter is merged field by field: labels three-way as a set (additions from both sides
kept, deletions from either side honoured), and title by whichever side changed. If both
sides changed the title, yours wins and a warning is logged.

A page renamed on the server is *moved*, not duplicated: the new path is written and the
old file removed, and the move is reported in `result.moved`.

**`push`** builds a plan, refuses anything unsafe, then applies it. Creates run
parent-first so a child always has a parent to attach to; deletes run child-first so a
parent is never removed out from under its children. For an update, only the blocks whose
Markdown changed are regenerated — the rest of the storage XML is copied byte-for-byte
from base. After the server confirms, confed rewrites the file's `confed:` block with the
new version and advances base, so `status` is clean again immediately.

## The conflict workflow

Start from a page you have edited that someone else has also edited.

**1. See it.**

```console
$ confed status
Space DOCS
Last fetch 2026-08-16T09:12:44Z

Diverged (edited on both sides)
  Team Handbook/Onboarding.md  (base v7, server v9)

1 page to push, 1 page to pull
```

`confed status --json | jq -r '.result.pages[] | select(.state=="diverged") | .path'` is
the scriptable form.

**2. Merge.**

```console
$ confed pull
  merged   Team Handbook/Policies.md
  conflict Team Handbook/Onboarding.md

0 created, 0 updated, 1 merged, 0 deleted

1 page left conflict markers. Edit them, then run `confed resolve <page>`.
```

Exit code 4. Pages that merged cleanly are already done; only the conflicted one needs
you.

**3. Read the markers.** confed merges in diff3 style, so the file shows what the text
said *before* either side touched it, and labels the remote side with its version, author
and timestamp:

```markdown
Complete these before your first Monday.

<<<<<<< local
Ask your manager for VPN access on day one.
||||||| base
Ask IT for VPN access.
=======
Request VPN access through the service desk.
>>>>>>> remote (v9, edited by Alice Ng, 2026-08-14T11:20:31Z)

Then read the security policy.
```

`<<<<<<< local` is your working file, `||||||| base` is the common ancestor,
`>>>>>>> remote (…)` is the server. Everything outside the markers merged cleanly.

**4. Resolve.** Edit the file so no markers remain, then tell confed:

```console
$ confed resolve "Team Handbook/Onboarding.md"
  resolved Team Handbook/Onboarding.md

Run `confed push` to upload the resolved pages.
```

If you missed a marker, nothing is cleared and confed says exactly where:

```console
$ confed resolve "Team Handbook/Onboarding.md"
  unresolved Team Handbook/Onboarding.md — still has conflict markers on line(s) 12, 15, 19
```

Exit code 7. To take one side wholesale instead of editing, `confed resolve --ours <page>`
keeps your text and `--theirs` keeps the server's; both drop the markers and the base
section. `confed resolve --list` shows what is still outstanding.

**5. Push.**

```console
$ confed push -m "Merge VPN wording"
  updated  Team Handbook/Onboarding.md  (v9 -> v10)

1 page pushed.
```

The whole loop as an agent would run it:

```bash
confed status --json | jq -e '.result.clean' >/dev/null || confed pull --json
confed resolve --list --json | jq -e '.result.conflicted | length == 0' >/dev/null \
  || { echo "human needed"; exit 4; }
confed push --dry-run --json && confed push --json
```

## Optimistic version checking

Confluence updates are optimistic: you send the version number you expect the page to
become, and the server rejects the write if that is not the next one. confed layers a
local check on top so you almost never see the server's version of the error.

**Before sending anything**, `push` compares each page's base version with the last
fetched remote version. If remote is ahead, the page is not sent:

```json
{
  "skipped": [{
    "page_id": "163842",
    "path": "Team Handbook/Onboarding.md",
    "reason": "remote is at version 9 but this file is based on 7; run `confed pull`"
  }]
}
```

The command exits 4. Nothing was uploaded, so nothing was lost — the fix is always
`confed pull` (which merges) followed by `confed push`.

**When sending**, the update carries `version = base + 1`. If the server has moved since
your last fetch — a race confed cannot see — it answers 409 (or, on some Data Center
builds, a 400 whose message mentions the version), which confed maps to a conflict and
reports as a failed page, exit 8. Again: pull, then push.

There is deliberately no `--force-version`. The only way past a stale base is to look at
the other side's changes.

Two more refusals happen at plan time, for the same reason — confed will not write when it
cannot reason about what it is overwriting:

- **an unresolved conflict**: `"unresolved merge conflict; run `confed resolve` first"`.
- **hand-edited tool-managed frontmatter**: `"tool-managed frontmatter was edited
  (version); run `confed pull --force` on this page"`. If `version` in the file no longer
  matches `.state.db`, the optimistic check is meaningless.

## Safety properties

- **Base only ever advances to a state the server confirmed.** A local edit never moves
  the base, which is what keeps "modified" meaningful.
- **Every file write is atomic** — a temp file in the same directory, then a rename — so
  an interrupted `pull` or `push` can never truncate a page.
- **`pull` decides everything before writing anything.** A partial pull cannot leave half
  your workspace overwritten and half untouched.
- **A workspace lock** (`.confed.lock`, carrying the holder's pid) is held for the duration
  of every mutating command, so two confed processes cannot fight over one directory. A
  lock whose owner is dead is detected and removed automatically. It does not protect
  against your editor saving mid-push; that is what the plan-time hashing is for.
- **Deletions are opt-in twice.** `pull` will not delete a locally modified file, and
  `push` will not delete a server page without `--allow-delete`, so an accidental `rm -rf`
  cannot take a Confluence subtree with it.
- **Fetch failures are partial, not fatal.** A page that cannot be fetched is listed in
  `result.failed`, the rest of the space is still updated, and the command exits 8.

## Re-rendering

Two things can make an already-synced file out of date without either side changing: the
conversion rules improve, or somebody the page mentions becomes resolvable. confed records
a fingerprint of both against each page, and `pull` re-renders any page whose fingerprint
no longer matches — so an improvement reaches a whole workspace on the next pull instead
of arriving page by page as each one happens to change on the server.

Re-rendering only ever applies to pages with no local changes. A page you have edited is
left exactly as you left it, because rewriting it would show up as a change you did not
make.

## Starting over

`confed pull --reset` puts every tracked page back to what the server has, whatever state
it was in: local edits, half-finished merges, conflict markers, comment drafts and changed
attachments are all discarded, and locally deleted pages come back.

It follows the line `git reset --hard` draws. Pages confed tracks are restored; files that
exist only locally — a page you created but never pushed, or anything that is not a page —
are left exactly where they are. confed has no equivalent of `git clean`, so removing
those is up to you.

Because it destroys work, it reports every page whose local changes it discarded, and
`confed pull --reset --dry-run` lists them without touching anything:

```bash
confed pull --reset --dry-run    # what would be thrown away
confed pull --reset              # throw it away
```

## What a fetch actually costs

A page body at a given version never changes, so confed downloads it once and keeps it in
`.pages.db`, keyed by page and version. `fetch` then asks the server one question — which
versions exist — and takes everything else from the cache:

- **Nothing changed upstream.** One request, whatever the size of the space.
- **Some pages changed.** One request, plus a body, attachment list and comment list for
  each page whose version moved.
- **`.state.db` was rebuilt** — a fresh clone, or `confed init` over an existing tree.
  Still one request: every body is already cached, and the attachment and comment
  snapshots taken alongside each one are restored with it, leaving confed exactly as
  current as it was before the rebuild.

The cache keeps the last few versions of each page, so a page reverted on the server also
costs nothing. Deleting `.pages.db` is always safe; the next fetch downloads what it needs
again.
