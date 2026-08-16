# 02 — Data Design

Status: draft for review · Schema version: 1

## 1. On-disk layout of a confed directory

```
docs-space/                        # bound to one Confluence space
├── .session.db                    # credentials fallback (0600, git-ignored)
├── .state.db                      # sync state (git-ignored, see §4)
├── .gitignore                     # generated/extended by init
├── CLAUDE.md · AGENTS.md          # generated agent contracts (see 08-agent-docs plan)
├── Team Handbook.md               # top-level page
├── Team Handbook/                 # its children live in a dir named after it
│   ├── Onboarding.md
│   ├── .Onboarding/               # hidden sidecar dir for "Onboarding.md"
│   │   ├── diagram.png            # attachments
│   │   └── comments.md            # footer + inline comment sidecar
│   └── Onboarding/…               # grandchildren
```

Rules:

- A page maps to `<slug>.md`; its children map into a sibling directory `<slug>/`.
- The hidden sidecar `.<slug>/` (same name, leading dot) sits next to the `.md` file and
  holds attachments plus `comments.md`. It exists only when the page has either.
- Page **position** among siblings is stored in state (and frontmatter), not encoded in
  filenames — directory listings are alphabetical; `status`/`pull` order by position.

### Slugs, collisions, renames

- Slug = title, NFC-normalized, with `/ \ : * ? " < > |`, control chars, and leading dots
  replaced by `-`; runs of whitespace collapsed to a single space; trimmed; truncated to
  **120 bytes** (UTF-8 boundary safe) so full paths stay under conservative OS limits.
- Collision (two sibling pages produce the same slug): the later-fetched page gets
  `<slug>~<last-6-of-page-id>.md`. Deterministic, stable across pulls.
- **The filename is never the identity.** `page_id` in frontmatter is the stable key. If
  the user renames a file, confed re-associates by `page_id` and treats it as a local
  slug preference (kept, not pushed). Title changes come from frontmatter `title`;
  `confed mv` does both (file rename + title change) in one step.

## 2. Markdown file format

```markdown
---
title: "Onboarding"            # WRITABLE — push renames the page
labels: [hr, onboarding]       # WRITABLE — push adds/removes labels to match
parent_id: "163841"            # WRITABLE — changing it moves the page on push
confed:                        # everything below is MANAGED — never hand-edit
  schema: 1
  page_id: "163842"
  space_key: DOCS
  version: 7                   # base version at last sync (optimistic-lock anchor)
  status: current              # current | archived | trashed(remote)
  position: 2
  created: 2025-11-02T09:14:00Z
  updated: 2026-08-01T16:40:00Z
  author: jdoe@example.com     # creator; last-modifier tracked in .state.db
  attachments:
    - { id: "att901", file: "diagram.png", size: 48213, sha256: "9f2c…" }
---
# Onboarding

First week checklist … ![Architecture](.Onboarding/diagram.png)
```

Contract (also emitted into `CLAUDE.md`/`AGENTS.md`):

| Field | Who writes it | Push behavior |
|---|---|---|
| `title`, `labels`, `parent_id` | user/agent | synced to server |
| everything under `confed:` | tool only | hand edits are **rejected** at push (exit 7) with the detected tampering listed; `confed pull --force <page>` repairs |
| body | user/agent | converted & synced |
| new file without `confed:` block but with `title` | user/agent (or `confed new`) | `push` creates the page (parent inferred from directory, overridable via `parent_id`) |

- Unknown top-level frontmatter keys are preserved verbatim (user metadata allowed).
- Attachment references in the body are **relative links into the sidecar dir**:
  `![alt](.Onboarding/diagram.png)`. On pull, `ac:image`/`ac:link` to attachments are
  rewritten to this form; on push they are rewritten back to storage-format references.
- A file placed in `.<slug>/` and referenced from the body (or added via `confed attach`)
  is uploaded on push; a changed sha256 uploads a new attachment version; removing the
  file *and* its references deletes the attachment on push (with confirmation unless
  `--yes`/non-interactive plan approval).

## 3. `.session.db` (SQLite, mode 0600)

Created with `umask`-independent explicit `chmod 0600`; refused (exit 7) if an existing
file is group/world-readable. **Preferred credential store is the OS keyring** via the
`keyring` crate (service `confed`, account `<base_url>|<username>`); the DB then stores
only a reference. SQLite fallback (headless Linux/CI without a Secret Service) stores the
token in `secret_value` and `doctor` warns about it. Secrets are never logged (newtype
with redacted `Debug`).

```sql
CREATE TABLE session (
  id               INTEGER PRIMARY KEY CHECK (id = 1),   -- single row
  base_url         TEXT NOT NULL,
  flavor           TEXT NOT NULL CHECK (flavor IN ('cloud','datacenter')),
  auth_method      TEXT NOT NULL CHECK (auth_method IN ('api_token','pat','basic')),
  username         TEXT,            -- Cloud: email; DC basic: username; PAT: NULL
  secret_backend   TEXT NOT NULL CHECK (secret_backend IN ('keyring','sqlite')),
  secret_value     TEXT,            -- only when secret_backend = 'sqlite'
  created_at       TEXT NOT NULL,   -- RFC 3339 UTC (all timestamps in both DBs)
  last_verified_at TEXT
);
```

## 4. `.state.db` (SQLite)

**Git-ignored — decision & rationale**: `.state.db` contains full storage-format bodies
(bulky, binary-ish, merge-hostile) and is a *derived cache of server state* that any
clone can rebuild with `confed init && confed fetch`. Committing it would cause guaranteed
git merge conflicts on every sync and bloat history. So `init` writes both `.session.db`
and `.state.db` (plus `.confed.lock`) into `.gitignore`. The tradeoff — a fresh git clone
must re-fetch before it can push — is acceptable and documented; `confed doctor` detects
the "files without state" situation and says exactly that.

`PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;` A `.confed.lock` advisory file lock
serializes mutating commands.

```sql
CREATE TABLE meta (                -- schema_version, space_key, space_id, base_url,
  key TEXT PRIMARY KEY,            -- flavor, last_fetch_at, fetch_cursor, …
  value TEXT NOT NULL
);

-- BASE: last state confirmed by the server (fetch materialized by pull, or push response)
CREATE TABLE pages (
  page_id        TEXT PRIMARY KEY,
  title          TEXT NOT NULL,
  slug           TEXT NOT NULL,     -- current filename stem (may differ from title's slug)
  local_path     TEXT NOT NULL,     -- relative, e.g. "Team Handbook/Onboarding.md"
  parent_id      TEXT,
  position       INTEGER,
  version        INTEGER NOT NULL,
  status         TEXT NOT NULL,
  labels         TEXT NOT NULL DEFAULT '[]',  -- JSON array
  author         TEXT,
  created_at     TEXT, updated_at TEXT,
  storage_body   BLOB NOT NULL,     -- zstd-compressed storage XHTML  ← the merge base
  storage_hash   TEXT NOT NULL,     -- sha256 of uncompressed body
  markdown_hash  TEXT NOT NULL,     -- sha256 of the .md file as written at sync
                                    --   (body + writable frontmatter, canonicalized)
  block_map      TEXT,              -- JSON: storage-block ⇄ md-block map (03-conversion)
  sync_state     TEXT NOT NULL DEFAULT 'clean'
                 CHECK (sync_state IN ('clean','conflicted')),
  synced_at      TEXT NOT NULL
);

-- REMOTE: last observed server state (written only by fetch; the "remote-tracking" copy)
CREATE TABLE remote_pages (
  page_id        TEXT PRIMARY KEY,
  title          TEXT NOT NULL,
  parent_id      TEXT,
  position       INTEGER,
  version        INTEGER NOT NULL,
  status         TEXT NOT NULL,
  labels         TEXT NOT NULL DEFAULT '[]',
  author         TEXT, created_at TEXT, updated_at TEXT,
  storage_body   BLOB,              -- NULL until body fetched (list vs body two-phase)
  storage_hash   TEXT,
  fetched_at     TEXT NOT NULL,
  deleted        INTEGER NOT NULL DEFAULT 0   -- 1 = gone/trashed on server
);

CREATE TABLE attachments (
  attachment_id  TEXT PRIMARY KEY,
  page_id        TEXT NOT NULL REFERENCES pages(page_id) ON DELETE CASCADE,
  filename       TEXT NOT NULL,
  media_type     TEXT,
  file_size      INTEGER,
  version        INTEGER NOT NULL,
  sha256         TEXT,              -- of last synced content
  downloaded     INTEGER NOT NULL DEFAULT 0,
  UNIQUE (page_id, filename)
);

CREATE TABLE comments (
  comment_id     TEXT PRIMARY KEY,  -- server id; local-new rows use 'new:<uuid>'
  page_id        TEXT NOT NULL,
  parent_comment_id TEXT,           -- reply threading
  kind           TEXT NOT NULL CHECK (kind IN ('footer','inline')),
  author         TEXT, created_at TEXT,
  body_storage   BLOB,
  body_markdown  TEXT NOT NULL,
  resolved       INTEGER NOT NULL DEFAULT 0,
  anchor         TEXT,              -- inline only: JSON {text, context_before,
                                    --   context_after, marker_ref, orphaned: bool}
  synced_at      TEXT
);

CREATE TABLE fetch_queue (          -- resumable pulls for large spaces
  page_id  TEXT PRIMARY KEY,
  needs    TEXT NOT NULL,           -- JSON: ["body","attachments","comments"]
  done     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE sync_log (             -- audit trail; `confed log --local`
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL, op TEXT NOT NULL, page_id TEXT,
  from_version INTEGER, to_version INTEGER,
  result TEXT NOT NULL, detail TEXT
);
```

Why `markdown_hash` matters: local-dirty detection is `sha256(canonicalize(file)) ≠
pages.markdown_hash` — no storage-format re-conversion needed for `status`, so `status`
is fast and offline. Canonicalization = frontmatter re-serialized in fixed key order +
body byte-exact, so YAML formatting noise doesn't produce false "modified".

## 5. Comment sidecar file — `.<slug>/comments.md`

Human- and agent-editable; the **only** write path besides `confed comment` (which edits
this same file, then optionally pushes). Format is Markdown with one HTML metadata
comment per entry (machine-parseable, invisible in rendered view):

```markdown
# Comments — Onboarding (page 163842)

<!-- confed:comment id=98211 author="Alice Ng" date=2026-07-30T10:02:00Z -->
Should this mention the VPN setup?

  <!-- confed:comment id=98230 reply-to=98211 author="Bob Le." date=2026-07-30T11:40:00Z -->
  Yes — adding it.

<!-- confed:comment id=98211 resolved=true -->        ← resolution status line

<!-- confed:inline id=77120 author="Bob K." date=2026-08-01T09:00:00Z
     anchor="first week checklist" context-before="…during your " orphaned=false -->
Link the checklist template here?

<!-- confed:new [reply-to=98211] -->
Text under a `confed:new` marker is pushed as a new comment, then rewritten in place
with its real id.
```

- Replies are indented under their parent (nesting mirrors `parent_comment_id`).
- Users edit **only**: text under a `confed:new` marker, `resolved=true` markers
  (Cloud), and their own unpushed drafts. Editing existing comment bodies is ignored on
  push (server API has no general "edit others' comments"; confed doesn't try).
- Inline entries carry the anchor text + context for re-anchoring (see 03-conversion §4).
