# Changelog

All notable changes to confed are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Versioning policy

Before 1.0, breaking changes bump the minor version. Two things are versioned
separately from the crate and are part of the compatibility contract:

- **`confed.schema`** in `--json` output. Adding a field is not breaking;
  removing or renaming one bumps the schema and the minor version.
- **The `.state.db` schema.** A newer database is refused rather than migrated
  downward, with a message telling you to upgrade confed.

`confed --version --json` reports all three.

## [Unreleased]

### Added

- Confluence Cloud (REST v2) and Data Center (REST v1) clients behind one
  `ConfluenceClient` trait, with capability reporting so flavor gaps surface as
  exit code 9 instead of confusing failures.
- Shared HTTP stack: exponential backoff with jitter, `Retry-After` support,
  adaptive pacing that tightens on 429, and a bounded request pool.
- Storage-format ⇄ Markdown conversion with block-level patching: only the
  blocks you edited are regenerated, and unmodelled macros round-trip byte for
  byte inside ```` ```confluence ```` fences.
- Sync engine with `fetch`, `pull` and `push` over a three-snapshot model
  (base, local, remote), three-way merge with git-style conflict markers, and
  optimistic version checks that refuse stale writes.
- Commands: `init`, `clone`, `fetch`, `pull`, `push`, `status`, `diff`,
  `resolve`, `new`, `mv`, `rm`, `attach`, `comment`, `log`, `open`, `search`,
  `spaces`, `whoami`, `config`, `doctor`, `export`, `completion`.
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
- Generated `CLAUDE.md` and `AGENTS.md` agent contracts, a versioned JSON
  envelope on every command, and a documented exit-code table.
- Credentials in the OS keyring where available, falling back to a `0600`
  SQLite file, with secrets that cannot be printed.
