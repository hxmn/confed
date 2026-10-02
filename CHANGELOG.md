# Changelog

All notable changes to confed are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Versioning policy

Before 1.0, breaking changes bump the minor version. Two things are versioned
separately from the crate and are part of the compatibility contract:

- **`confed.schema`** in `--json` output. Adding a field is not breaking;
  removing or renaming one bumps the schema and the minor version.
- **The `.state.db` schema.** A newer database is refused rather than migrated
  downward, with a message telling you to upgrade confed.

`confed version --json` reports all three, and `confed version --changelog`
prints the section below that describes the binary you are running: the release
notes are compiled into it, so no network access or checkout is needed.

## [Unreleased]

## [0.7.0] - 2026-10-02

### Fixed

- **`comment add --anchor` could lose the draft and report success.** Text inside a
  ```` ```confluence ```` block (a raw table, a macro) cannot carry a body mark — since
  0.4.1 confed never writes one into a fence — so the mark was dropped on write while
  the command answered as if it were queued. confed now checks the mark is in the file
  after writing it; when it is not, or cannot be (text that reads differently in the
  Markdown, a multi-line comment, which a one-line mark used to squash), the draft goes
  to `comments.md`, and the result says `"written_to": "sidecar"` with the reason.
- Data Center answered 500 to creating an inline comment (or reply) whose body has a
  mention or page link (`<ac:link>`). confed now creates it with those replaced by
  their text and puts the real body right after through the content API: one push,
  one comment, links and mentions intact, one notification. If the second step fails,
  the comment is kept and the warning names `confed comment edit <id>`.
- An error with an empty response body read as a dangling `…comments: `; it now says
  the server sent no body. Error bodies are logged at debug, request bodies (never
  credentials) at trace (`--log confed=trace`).

### Added

- Mentions in Markdown: `[@Name](user:<userkey>)`, or `user:account-id=…` on Cloud,
  `user:username=…`, become real mentions — in pages and in comments.
- Comment bodies are converted in their page's context, so mentions of known people
  (`[@Name](<profile url>)`) and page links (`[Title](Other.md)`) work in comments as
  they do in pages.
- `confed user search <name> [--limit N] --json`: people with their `username`,
  `userkey`, `account_id` and the `mention` to paste.
- The agent guide covers: drafts that fall back to the sidecar, checking every draft in
  `push --dry-run`, scoped pushes versus plain `push` in a shared workspace, the lock
  (wait and retry; confed clears a dead process's lock), mentions and page links, and
  Data Center's two-step create.

## [0.6.5] - 2026-10-02

### Fixed

- `push --dry-run` listed comment drafts on a page the last fetch found deleted as
  ordinary pending work, exit 0. It now reports them under `failed` with the message
  the real push gives, and exits 8.
- `pull` deleted a page that was gone from the server together with its unsent
  comment drafts, without a word — right after the push had pointed there. Unsent
  comment work now counts as local work: pull keeps such a page and refuses, saying
  how many drafts it has and that `confed pull --force` drops them. With `--force` (or
  `--reset`) the page is listed under `discarded`, as a deleted page with local edits
  now is too. The push's message says the same.
- Removing a page's last child (by `pull` or `rm`) left its empty folder behind;
  folders a removal empties are now removed too, up to the workspace root.

## [0.6.4] - 2026-10-02

### Fixed

- **`comment add/reply/resolve --push` pushed the whole workspace**, as `rm --push`
  did before 0.6.3: every edited page and every other page's comment drafts went out
  with the comment. They now send only the comment work of the pages they name — no
  page bodies, no attachments, nothing from other pages. The other `--push` shortcuts
  (`attach`, `new`, `mv`, `rm`) and the TUI were checked and already push only what
  they name.
- A comment swept up from another page was listed with `kind: null` as if it were on
  the page asked about; comment details now come from the comment's own page and
  include its `page_id`.
- `confed log <page>` answered 404 for every page on Data Center 9.5.4, which has no
  `rest/api/content/{id}/version`. confed now falls back to the experimental history
  endpoint and then to reading each version through the content endpoint, so a 404
  means only that the page does not exist — and `log` says so in those words.

## [0.6.3] - 2026-10-02

### Fixed

- **`rm --push` pushed the whole workspace.** It ran an unscoped push with deletions
  allowed: besides deleting the pages named, it uploaded every other edited page and
  deleted on the server any other page whose file was missing locally. It now pushes
  the pages named and nothing else.
- `rm --push` reported `server_deleted: false` for a page it had just deleted; it now
  says what the push did.
- Comment drafts on a page file the workspace no longer tracks (removed with
  `confed rm`, then the file put back) were left out of the push without a word,
  though the dry run listed them. They are now reported under `failed` — "the page
  no longer exists on the server", or "not tracked … `confed pull` picks the page up
  again" — the drafts stay, and the push exits 8.

### Added

- `confed rm --dry-run`: what would be removed, and deleted on the server with
  `--push`, without changing anything.
- A page id the workspace does not track still works where the server or the
  history can answer: `confed log <id>`, `confed log --local <id>`, and `confed
  comment list <id>`, which then reads the comments from the server and says so.

## [0.6.2] - 2026-10-02

### Fixed

- A reply to a reply in an inline thread showed up under the thread's first comment,
  with nothing saying why. Inline threads are one level deep — Data Center's web UI
  does the same — so confed now posts such a reply to the thread's root on purpose,
  and says so. Page-comment threads keep nesting.
- `comment reply --push` reported nothing about what it posted. It now returns
  `posted` (the ids), `replies` (`id`, `reply_to` as asked, `parent` as posted,
  `kind`) and the push's comment results, and prints each reply with its parent.

## [0.6.1] - 2026-10-02

### Fixed

- Pushing comment work for a page that was deleted on the server failed with exit 1,
  the server's raw `400 content.doesnt.exist` and `result: null`. confed now checks the
  page first: that page is reported under `failed` — "the page no longer exists on the
  server (deleted?); its comment drafts are kept — run `confed pull`" — the drafts
  stay, comments on other pages still go out, and the push exits 8. `comment add/
  reply/resolve --push` report it the same way. A `content.doesnt.exist` answer is
  now a not-found (exit 6) wherever it comes from.
- `comment list` on a page the last fetch found deleted warns that these are the
  comments confed last saw, not live ones.
- A mistyped page path got no suggestion when the typo was two swapped letters
  (`Tset.md`); a swap now counts as one edit, so `did you mean` finds `Test.md`.
- Replies in an inline thread were listed with an empty anchor marked `orphaned:
  true`. A reply has no anchor of its own; it is now listed without one, and the
  re-anchoring pass no longer touches replies.
- `doctor` said the token was in `.session.db` "because no OS keyring was
  available" while reporting a keyring as available. When a keyring is there, the
  file store was a choice, and `doctor` says so (with how to move to the keyring).
- The stale-guide warning read "written by confed an earlier confed".
- `comment rm` reports the replies deleted with each thread, as `replies_deleted`.

## [0.6.0] - 2026-10-02

### Added

- `confed comment edit <id> -m "…"` and `confed comment rm <id>…` change and delete
  posted comments on the server at once (rm asks first when interactive; `--yes`
  skips it; replies go with their thread). On Data Center both use the public REST
  v1 content endpoint.
- `confed comment reply` and `confed comment resolve` take several ids, and
  `confed comment resolve --all <page>` resolves every open thread on a page — on
  Data Center every open inline thread, listing page comments as skipped.
- A push reports `replies_added` and `comments_resolved` next to `comments_added`,
  and prints each one. **Changed:** `comments_added` now holds new threads only;
  replies moved to `replies_added`.
- `comment list --json` gives every comment `thread_resolved`, and a reply's
  `resolved` is now its thread's status: a reply has none of its own, and showing
  `false` in a resolved thread read as "still open". It also lists `orphan_markers`:
  inline markers in the page that no comment claims, such as those deleted comments
  leave behind on Data Center. `confed doctor` warns about them.
- Every command run in a workspace warns — in `--json`, in `warnings` — when
  `CLAUDE.md`/`AGENTS.md` were written by another confed, naming `confed doctor
  --fix`, which regenerates them. An upgrade no longer leaves an agent following an
  old guide unawares.
- The agent guide now documents comments in full: reading them, every `comment`
  command, the sidecar draft forms, the push report, the exit codes, the Data Center
  specifics, and how inline marks read — including that a comment on a paragraph's
  first word is written one character in (`Ф<!--c …-->раза 2.` is a comment on
  `Фраза 2.`) and must not be "fixed".

### Changed

- Page arguments are relative to the current directory, like git's: `confed comment
  add Test.md …` from `Handbook 1/` finds `Handbook 1/Test.md`. A path that names
  nothing there is still read from the workspace root, so existing scripts keep
  working, and globs are relative to the current directory. A miss says where it
  looked and suggests the closest page. This covers every command that takes a page
  or a path scope (`comment`, `attach`, `log`, `open`, `rm`, `mv`, `new`, `resolve`,
  `diff`, `push`, `pull`, `export`).

### Fixed

- Two comments on the same text produced crossed marks
  (`<!--c A--><!--c B-->text<!--/c A--><!--/c B-->`); they now nest, as in storage.
- The `--anchor` help and the docs said a Data Center inline comment always adds a
  page version. 9.5.4 wraps the marker in place, without one; confed handles both.

## [0.5.1] - 2026-10-02

### Fixed

- `push --dry-run` named the wrong text for an inline comment on the start of a
  paragraph: `add inline comment on "раза 2."` for `--anchor "Фраза 2."`. In the file
  the mark has to sit one character in (`Ф<!--c new t-->раза 2.<!--/c new-->`),
  because a line that starts with `<!--` is an HTML block to every Markdown renderer.
  Push already posted the whole `Фраза 2.`; the dry run read the mark literally. Both
  now go through one function, so what the dry run promises is what is posted.
- A comment draft in `comments.md` was listed by the dry run as `add comment (N chars)`
  even when it was inline. It now reads `add inline comment on "…", occurrence N
  (comments.md)`, and a reply `reply to <id>`. They were always posted as such.
- `status` showed an empty `path` for a page that is new on the server (`remote_new`);
  it now shows the file `pull` will write.
- `pull` reported a page deleted on the server before it was ever pulled as
  `deleted` with an empty path. There was never a file, so it is no longer reported.

## [0.5.0] - 2026-10-02

### Added

- Inline comments on Confluence Data Center: `confed comment add <page> --anchor
  "text" -m "…"`, `<!--c new …-->` marks in the page body and `confed:new anchor="…"`
  in `comments.md` now post there too, instead of exiting 9. So do replies to an
  inline thread (`reply-to=…`) and `confed:resolve` / `confed comment resolve` on one.
  Data Center's public API cannot do any of this, so confed uses the undocumented
  plugin API the page view itself calls (`rest/inlinecomments/1.0`), with request
  shapes taken from captures of DC 9.5.4, kept as test fixtures. It may change in any
  Data Center release: on a major other than 9 confed warns before the first call, and
  a server that answers 404/405 is reported as unsupported (exit 9) naming the step
  that failed. Footer comments still cannot be resolved on Data Center (exit 9).
- Creating an inline comment on Data Center saves a new page version. confed adopts
  it when the new comment's marker is the only change: the base, the file's
  `confed.version` and the body move to it, so the page stays `unchanged` and the next
  push neither conflicts nor drops the marker. If somebody else edited the page too,
  it is left for `pull` to merge.
- The selection an inline comment is created with is computed from the page's
  current storage, the way Confluence extracts text: entities decoded, `&nbsp;` kept as
  a non-breaking space (a typed space matches it, and the page's own character is
  sent), macro parameters and code excluded, no match across paragraphs or table
  cells. `--occurrence N` picks among repeats. Before anything is written or sent, an
  anchor that is not on the page exits 6, and one that appears more than once without
  `--occurrence` exits 2, listing where. A draft on a paragraph with edits the server
  does not have stops the push with exit 7, rather than being refused by the server.
- `comment add --push --json` reports what was created: `result.comments[]` with
  `id`, `kind`, `anchor` and Confluence's `marker_ref`.
- `whoami` shows the server's version (`server_version` in `--json`; Data Center
  only).
- `occurrence=N` on a `confed:new anchor="…"` sidecar draft, written by `comment add
  --sidecar --occurrence N`; it used to be lost.

### Fixed

- A draft in `comments.md` stayed there after it was posted, as did a
  `confed:resolve` request, so the next push posted the comment again. Each one now
  leaves the sidecar as soon as the server has it.
- A body draft for text at the start of a paragraph is written one character in
  (`W<!--c new …-->elcome`, because a line starting with `<!--` would be an HTML
  block). Push used to post the comment on `elcome`; it now covers `Welcome`.
- An inline comment's `--anchor` that was not on the page exited 7; it now exits 6
  (not found), and an ambiguous one exits 2 (say which), as for other commands.

## [0.4.1] - 2026-10-02

### Fixed

- `pull --reset` now leaves pages with inline comments clean. A comment whose
  marker sits in a block kept as raw storage (a ```` ```confluence ```` fence)
  is not shown in the body, but the fallback that places a comment by its text
  went looking for it anyway, found it in the raw XML, and wrote the mark
  there, sometimes in the middle of a word or a tag
  (`</a<!--/c 1-->c:inline-comment-marker>`). Inside a fence a mark is content,
  not a layer, so the page read as modified after every reset and a push would
  have uploaded invalid markup. Marks are now never written inside a fenced
  block, a code span or an HTML tag. The text fallback places a mark only where
  the comment's text sits verbatim: a fuzzy match has no reliable end, so a
  comment found only that way stays in the sidecar. A mark edge that falls on a
  line break is pulled back to its text, so it can no longer be pushed into the
  opening backticks of a fence that follows.
- `confed diff` and the TUI's diff pane no longer show open inline comments as
  changes. They compared the base, rendered *with* its marks, against the file
  body read *without* them, so every commented page showed a diff that `status`
  did not report. Both sides are compared without marks now, through one shared
  function. `diff --storage` also feeds push's patcher the marked body, as push
  does.
- The converter version is bumped, so pages left untouched re-render on the next
  pull. Pages already reported as modified because of a stray mark need one
  `confed pull --reset` (or `pull --force` on those pages).

## [0.4.0] - 2026-09-07

### Added

- `confed log` with no page now shows recent activity across the space this
  directory is bound to, instead of failing with "the following required
  arguments were not provided: <PAGE>". You rarely know which page a colleague
  touched — that is the thing you are running `log` to find out. It lists the
  most recently changed pages first with their version, author, timestamp and,
  for pages you have pulled, the file to edit. The ordering is the server's
  (`space = "KEY" and type = page order by lastmodified desc` through CQL), so
  `--limit` really is the N most recent and the whole view costs one request on
  a space of any size. `log --local` with no page widens the same way, over
  confed's own sync log, each entry naming its page.
- `log --local` no longer wakes the OS keyring. It answers from `.state.db`, so
  it now runs on the offline path alongside `status` and `diff` — useful when
  the credential store is locked, or there is no network.

### Changed

- CQL search asks the server to expand `content.version,content.space`, so a
  search result carries the version, author and last-modified time it used to
  drop. `confed search --json` is unchanged; the extra fields surface through
  `confed log`.

## [0.3.0] - 2026-09-04

### Fixed

- `push` no longer uploads confed's own partial downloads. A `pull` that died
  mid-stream left a `*.confed-part` file in the page's sidecar directory, and the
  next `push` treated it as a new attachment and uploaded it — 56 MB of scratch
  next to the complete file it was a partial copy of. The filter lives in the
  attachment scanner rather than the uploader, so `push --dry-run` does not offer
  one either. Three further defences: a failed download now deletes its own
  scratch file, partials are written as hidden `.<name>.confed-part` so a dotfile
  rule catches them too, and `pull` sweeps stale ones out of the sidecar.
- `attach --rm` no longer reports a removal it did not make. It deleted the local
  file, exited 0 with `{"removed": "<file>"}`, ignored `--push` entirely, and left
  the attachment on the server with no way to get rid of it through confed. It now
  deletes on the server with `--push`, and without it says the deletion is staged
  for `push --allow-delete`; either way `removed_on_server` and `staged` say which
  happened. Removing a name that is neither in the sidecar nor on the page is exit
  6 rather than a silent success. `--rm --push` cannot delete the page itself.
- `push --dry-run` reports the attachment work it would do. It listed only page
  bodies, so an upload arrived with no warning from the preview that was supposed
  to show it. The dry run and the real push now run off the same plan.
- An attachment deletion skipped for want of `--allow-delete` is reported under
  `result.skipped`, with the flag to pass. It was a `tracing::debug` line nobody
  would see.

### Added

- `attach --list --remote` asks the server. Plain `--list` reads local state, and
  now says so: the JSON carries `result.source` (`cache` or `server`) and
  `last_fetch_at`, and the human output names the last fetch. A stale listing that
  read as authoritative is what hid the `--rm` bug above.
- `push` reports attachments in its human output (`uploaded` and `unlinked` lines,
  and a count in the summary) instead of saying "Nothing to push." after uploading
  a file.
- `result.attachments_deleted` on `push`, alongside `attachments_uploaded`. A dry
  run also reports `result.comments_pending`.

## [0.2.0] - 2026-08-30

### Changed

- Confluence markup is now laid out across indented lines instead of the single
  line the API returns — both in each page's `.<page>/storage.xml` sidecar and
  inside ```` ```confluence ```` fences in the Markdown, where it is meant to be
  edited. A newline is only ever added between two block-level elements inside a
  container that lays its children out as blocks (`ac:structured-macro`,
  `ac:rich-text-body`, `ac:layout*`, `ac:task*`, tables, lists, `div`,
  `blockquote`). A paragraph, a heading, an `ac:link`, an `ac:parameter`, a table
  cell holding inline markup, a CDATA body: all keep their exact bytes, because a
  newline between two inline elements would become a rendered space. A fence you
  do not edit still goes back to Confluence byte-for-byte, because push copies
  the original storage for unchanged blocks; one you edit goes up as the fence
  reads.
- The converter version bumped to 4, so the next `pull` re-renders pages already
  in a workspace rather than waiting for each to change on the server.

## [0.1.0] - 2026-08-30

First release. Both API clients, the converter, the sync engine, the command set
and the TUI are implemented and covered end to end against a mock server in both
Confluence flavors.

### Added

- Confluence Cloud (REST v2) and Data Center (REST v1) clients behind one
  `ConfluenceClient` trait, with capability reporting so flavor gaps surface as
  exit code 9 instead of confusing failures.
- Shared HTTP stack: exponential backoff with jitter, `Retry-After` support,
  adaptive pacing that tightens on 429, and a bounded request pool.
- Storage-format ⇄ Markdown conversion with block-level patching: only the
  blocks you edited are regenerated, and unmodelled macros round-trip byte for
  byte inside ```` ```confluence ```` fences. Tables are laid out with aligned
  columns, and mentions render as named profile links.
- Sync engine with `fetch`, `pull` and `push` over a three-snapshot model
  (base, local, remote), three-way merge with git-style conflict markers, and
  optimistic version checks that refuse stale writes. `pull --reset` puts every
  tracked page back to the server's state, and page bodies are cached by version
  in `.pages.db` so a re-pull does not re-download them.
- Commands: `init`, `clone`, `fetch`, `pull`, `push`, `status`, `diff`,
  `resolve`, `new`, `mv`, `rm`, `attach`, `comment`, `log`, `open`, `search`,
  `spaces`, `whoami`, `config`, `doctor`, `export`, `mkdocs`, `version`,
  `completion`, and a full-screen `tui`.
- `confed version`, which reports the binary version, the JSON envelope schema
  and the `.state.db` schema. `--changelog` prints the release notes for the
  running build, and `--changelog --since <version>` prints everything an
  upgrade from that version brought.
- Each page's Confluence markup is kept in its sidecar (`.<page>/storage.xml`),
  so `diff --conf-format` shows exactly what a push would upload.
- Comment sidecars (`.<page>/comments.md`) with drafts, replies and resolution
  requests, plus inline-comment re-anchoring that flags anchors it cannot place
  rather than attaching them to the wrong text.
- Inline comments shown in the page body as marks (`<!--c ID preview-->…<!--/c ID-->`),
  placed from the server's own markers and stripped before anything is hashed,
  diffed, merged or uploaded. Wrap text with `<!--c new Your comment-->…<!--/c new-->`
  and `push` creates the comment at that occurrence (Cloud). `status` counts comment
  drafts and orphaned comments; `comment add --anchor` writes a body mark, with
  `--occurrence` for repeated text; `config --set comments.marks full|ids|off`.
- Editing a paragraph that carries an inline comment no longer orphans the
  thread: push writes the marker back into the regenerated block, and after
  posting an inline comment the base body is refreshed so a later push cannot
  copy stale bytes over the marker the server added.
- Generated `CLAUDE.md` and `AGENTS.md` agent contracts, stamped with the confed
  version that wrote them and telling an agent to read
  `confed version --changelog --since <that version>` when the binary has moved
  on; `confed doctor` reports the drift and `--fix` rewrites them.
- A versioned JSON envelope on every command, a documented exit-code table, and
  progress on `fetch`, `pull` and `push` that appears only when someone is
  watching (`--silent` turns it off).
- `confed mkdocs` generates an MkDocs site over the pulled Markdown, with
  attachment links mapped back to Confluence.
- Credentials in the OS keyring where available, falling back to a `0600`
  SQLite file, with secrets that cannot be printed. `confed config
  --no-keychain` / `--force-keychain` moves the credential between the two.

[Unreleased]: https://github.com/hxmn/confed/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/hxmn/confed/compare/v0.6.5...v0.7.0
[0.6.5]: https://github.com/hxmn/confed/compare/v0.6.4...v0.6.5
[0.6.4]: https://github.com/hxmn/confed/compare/v0.6.3...v0.6.4
[0.6.3]: https://github.com/hxmn/confed/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/hxmn/confed/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/hxmn/confed/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/hxmn/confed/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/hxmn/confed/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/hxmn/confed/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/hxmn/confed/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/hxmn/confed/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/hxmn/confed/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/hxmn/confed/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/hxmn/confed/releases/tag/v0.1.0
