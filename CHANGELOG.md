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

[Unreleased]: https://github.com/hxmn/confed/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/hxmn/confed/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/hxmn/confed/releases/tag/v0.1.0
