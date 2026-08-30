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
