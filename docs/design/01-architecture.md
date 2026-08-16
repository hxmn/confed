# 01 — Architecture Overview

Status: draft for review · Schema/design version: 1 · Date: 2026-08-16

`confed` is an offline-first Confluence editor: it mirrors a Confluence space into a
directory of Markdown files and moves changes between disk and server with a git-like
command model (`fetch` / `pull` / `push` / `diff` / `status`).

## 1. Crate layout (cargo workspace)

```
confed/
├── Cargo.toml                  # workspace
├── crates/
│   ├── confed-api/             # ConfluenceClient trait + Cloud/DC impls, HTTP stack
│   ├── confed-convert/         # storage-format (XHTML) ⇄ Markdown converter
│   ├── confed-core/            # domain model, state DBs, sync engine, config resolution
│   └── confed/                 # binary: clap CLI, output (human/JSON), TUI (ratatui)
└── docs/
```

Why a workspace and not one crate:

- **Testability**: `confed-convert` is pure (no IO) and gets heavy snapshot testing;
  `confed-api` is tested against `wiremock` in isolation; `confed-core` is tested with a
  mock `ConfluenceClient`. Clean crate boundaries enforce those seams.
- **Compile times**: the converter and the TUI are the two heavy dependency trees; they
  rebuild independently.
- **Dependency direction is one-way**: `confed` → `confed-core` → {`confed-api`,
  `confed-convert`}. `confed-api` and `confed-convert` do not know about each other or
  about SQLite.

### Module map

| Crate | Modules | Responsibility |
|---|---|---|
| `confed-api` | `client` (trait), `cloud`, `dc`, `auth`, `retry`, `paginate`, `types` | All HTTP; API-flavor abstraction |
| `confed-convert` | `storage_parse`, `to_markdown`, `to_storage`, `macros`, `blockmap`, `frontmatter` | Content conversion, macro preservation, block map |
| `confed-core` | `config`, `session`, `state`, `worktree`, `sync`, `merge`, `slug`, `comments`, `attachments` | State machine, DBs, filesystem mapping |
| `confed` | `cli`, `commands/*`, `output` (human+JSON envelope), `tui/*`, `prompt` | UX layer only; no business logic |

## 2. Client abstraction: Cloud vs Data Center

```rust
#[async_trait]
pub trait ConfluenceClient: Send + Sync {
    fn capabilities(&self) -> &Capabilities;

    async fn whoami(&self) -> Result<User>;
    async fn get_space(&self, key: &str) -> Result<Space>;
    fn list_spaces(&self) -> BoxStream<Result<Space>>;

    /// Streams every page in a space (id, title, version, parent, position, status),
    /// bodies NOT included — bodies are fetched individually by the worker pool.
    fn list_pages(&self, space: &SpaceId) -> BoxStream<Result<PageSummary>>;
    async fn get_page(&self, id: &PageId, body: BodyFormat) -> Result<Page>;
    async fn create_page(&self, new: &NewPage) -> Result<Page>;
    /// MUST send `version.number = expected_base + 1`; server rejects stale updates.
    async fn update_page(&self, id: &PageId, update: &PageUpdate) -> Result<Page>;
    async fn delete_page(&self, id: &PageId) -> Result<()>;
    async fn move_page(&self, id: &PageId, new_parent: &PageId, pos: Position) -> Result<()>;

    async fn get_labels(&self, id: &PageId) -> Result<Vec<Label>>;
    async fn add_label(&self, id: &PageId, label: &str) -> Result<()>;
    async fn remove_label(&self, id: &PageId, label: &str) -> Result<()>;

    fn list_attachments(&self, id: &PageId) -> BoxStream<Result<Attachment>>;
    async fn download_attachment(&self, a: &Attachment, dest: &Path) -> Result<()>;
    async fn upload_attachment(&self, id: &PageId, file: &Path, existing: Option<&AttachmentId>) -> Result<Attachment>;
    async fn delete_attachment(&self, id: &AttachmentId) -> Result<()>;

    fn list_footer_comments(&self, id: &PageId) -> BoxStream<Result<Comment>>;
    fn list_inline_comments(&self, id: &PageId) -> BoxStream<Result<InlineComment>>;
    async fn add_footer_comment(&self, id: &PageId, body: &Storage, reply_to: Option<&CommentId>) -> Result<Comment>;
    async fn add_inline_comment(&self, id: &PageId, anchor: &InlineAnchor, body: &Storage) -> Result<InlineComment>; // Cloud only
    async fn resolve_comment(&self, id: &CommentId) -> Result<()>;                                                  // Cloud only

    fn search_cql(&self, cql: &str) -> BoxStream<Result<SearchResult>>;
    async fn get_page_versions(&self, id: &PageId) -> Result<Vec<VersionInfo>>;
    async fn get_page_at_version(&self, id: &PageId, v: u32) -> Result<Page>;
}
```

Both implementations share one HTTP stack (`reqwest` + retry middleware) and differ in:

| Concern | `CloudClient` (REST v2) | `DcClient` (REST v1) |
|---|---|---|
| Base path | `/wiki/api/v2` (fallback to v1 for CQL search, labels ops) | `/rest/api` |
| Auth | Basic: email + API token | Bearer PAT (preferred) or Basic user+password |
| Pagination | Cursor (`Link` header / `_links.next`) | `start`/`limit` |
| Body formats | `storage`, `atlas_doc_format` (ADF) | `storage` only |
| Inline comments | Full API (list/create/resolve) | List via v1 comment expansion; **no create/resolve API** |
| Page properties | v2 content-properties API | v1 content properties |
| Space id | Numeric `space.id` required by v2 endpoints | Space key used directly |
| Flavor detection | `*.atlassian.net` host, or probe `GET /wiki/api/v2/spaces` | Probe `GET /rest/api/space` |

Capability gaps are surfaced through `Capabilities`:

```rust
pub struct Capabilities {
    pub flavor: Flavor,                 // Cloud | DataCenter
    pub inline_comment_create: bool,    // Cloud only
    pub comment_resolve: bool,          // Cloud only
    pub adf: bool,                      // Cloud only (we still sync in storage format; see 03)
    pub max_request_concurrency: usize, // default 4 (Cloud), 8 (DC)
}
```

Commands never branch on flavor directly; they branch on capabilities, and unsupported
operations fail with exit code `9 UNSUPPORTED` and a message naming the flavor gap.

**Format decision**: even on Cloud we fetch and push `storage` format, not ADF. One
converter, one merge base representation, and DC parity. ADF is revisited in
[05-open-questions](05-open-questions.md#q7).

### HTTP stack (shared)

- `tokio` + `reqwest` with a retry layer: exponential backoff with full jitter
  (250 ms · 2ⁿ, cap 30 s, max 6 attempts) on 429/502/503/504 and connect errors;
  always honor `Retry-After` when present; retry non-idempotent requests (POST/PUT)
  **only** on connect-before-send errors.
- Bounded worker pool: a `tokio::sync::Semaphore` with `--concurrency N`
  (`CONFED_CONCURRENCY`, default per capabilities) gates all in-flight requests.
- A process-wide token-bucket soft limiter keeps Cloud under ~5 req/s to avoid tripping
  429s in the first place; 429 responses shrink the bucket adaptively.
- `tracing` instruments every request (method, path, status, ms) at `debug`; secrets are
  wrapped in a `Secret<String>` newtype whose `Debug`/`Display` print `***`.

## 3. The sync model (three snapshots, like git)

For every page there are up to three bodies:

| Name | Lives in | Analogous to |
|---|---|---|
| **base** | `.state.db pages` — storage XHTML + rendered Markdown of the last *synced* version | git index / merge base |
| **local** ("ours") | the `.md` working file | working tree |
| **remote** ("theirs") | `.state.db remote_pages` — snapshot written by `fetch` | remote-tracking branch |

Per-page status is derived from hash comparison — `local_dirty = hash(local_md) ≠ base.markdown_hash`
(plus frontmatter-writable fields), `remote_ahead = remote.version > base.version`:

```
                         remote_ahead = false        remote_ahead = true
local_dirty = false      Unchanged                   Behind        (pull fast-forwards)
local_dirty = true       Modified      (push ok)     Diverged      (needs merge → push)
plus:  LocalNew (file without page_id) · RemoteNew (fetch found unknown page)
       LocalDeleted (file gone, base exists) · RemoteDeleted (fetch: page gone/trashed)
       Conflicted (merge produced conflict markers; recorded in state.db until resolved)
```

### Data flow per command

**init** — resolve base URL/credentials (precedence: flag → `CONFED_*` env → stored
session → TTY prompt) → detect flavor → `whoami()` to verify → store credentials
(keyring preferred, `.session.db` 0600 fallback) → select space → create `.state.db`,
`.gitignore`, `CLAUDE.md`, `AGENTS.md`. Touches no page content.

**fetch** — `list_pages` stream → compare `(id, version)` with `remote_pages` → fetch
changed/new bodies with the worker pool → upsert `remote_pages` (+ attachment metadata
+ comment snapshots) in one transaction per page → record deletions. Never touches
working files or `pages` (base). Resumable: the page list is written to a `fetch_queue`
table first; completed ids are marked; an interrupted fetch continues where it stopped.

**pull** = fetch (unless `--no-fetch`) + materialize:

```
for each page in scope:
  Behind, local clean      → write new .md (body + managed frontmatter), update base
  RemoteNew                → create file at slugged path
  RemoteDeleted, clean     → delete file + attachment dir, drop from base
  Diverged                 → 3-way merge base/local/remote (see 03-conversion);
                             clean merge → write merged file, update base to remote version;
                             conflict → write conflict markers, mark Conflicted, exit 4
  local dirty & would clobber, not mergeable or --no-merge
                           → STOP: list files, show highlighted diff, exit 7
                             (--force overwrites; --merge is the default assist mode)
```

**push** — plan first, then apply:

```
plan:  scan working tree → statuses → ordered ops
       (creates parent-first; moves; body/title/label updates; deletes child-first;
        attachment uploads before body update of the referencing page; comments last)
verify: for every op, base.version must equal remote_pages.version
        (stale → exit 4 CONFLICT, tell user to pull) — optimistic concurrency
apply: worker pool; update_page sends version = base+1; on per-page 409 → record
       conflict, continue others, exit 8 PARTIAL at the end
commit: on success update `pages` (base) with the server's response body/version,
        rewrite the file's managed frontmatter (new version), append sync_log
```

`--dry-run` stops after `verify` and prints/JSON-emits the plan with rendered diffs.

### Sync engine invariants

1. `pages` (base) is only ever updated to a state the *server confirmed* (fetch result or
   push response). Never from local edits alone.
2. Every state mutation per page is one SQLite transaction; a killed process leaves a
   consistent DB and an idempotently re-runnable command.
3. Working files are written atomically (temp file + rename) and never modified while a
   push is computing its plan (plan reads a snapshot of hashes).
4. `push` never uploads a page whose status is `Conflicted`.

## 4. Argument resolution (strict precedence)

One `ConfigResolver` in `confed-core` used by every command:

```
1. CLI flag                    --space DOCS
2. Environment                 CONFED_SPACE=DOCS      (CONFED_ + SCREAMING_SNAKE flag name)
3. Stored config               .state.db meta / .session.db
4. TTY prompt                  only if stdin is a TTY AND !--non-interactive AND !--json
   otherwise                   exit 2 (usage): "missing --space (or CONFED_SPACE); run
                               'confed init' or pass the flag"
```

`--json` implies `--non-interactive`. Every resolved value records its source; `confed
config --list` and `doctor` display it (`space = DOCS (from env CONFED_SPACE)`).

## 5. TUI

The TUI is a thin optional layer (all functionality exists headless):

- `confed tui` — space browser: tree of pages, status badges, preview pane; actions
  mapped to the same command implementations (pull/push/diff on selection).
- Merge assist: side-by-side 3-way conflict resolver invoked from `pull`/`resolve`.
- Interactive prompts (init space picker, `push --interactive` confirmations) use small
  ratatui widgets when TTY, plain line prompts as fallback.
