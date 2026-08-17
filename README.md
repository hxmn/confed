# confed

An offline-first Confluence editor. `confed` mirrors a Confluence space into
Markdown files on disk and moves changes between disk and server with a git-like
command model: `fetch`, `pull`, `push`, `diff`, `status`.

It works against **Confluence Cloud** (REST v2) and **Confluence Data Center**
(REST v1), and it is built to be driven by people and by coding agents equally:
every command speaks JSON, exit codes are deterministic, and nothing ever blocks
on a prompt when there is no terminal.

```bash
confed clone https://acme.atlassian.net/wiki/spaces/DOCS
cd DOCS
$EDITOR "Team Handbook/Onboarding.md"
confed status
confed push --dry-run     # exactly what would be uploaded
confed push -m "Clarify the first-week checklist"
```

## What makes it different

**Your edits do not get reformatted.** Confluence stores pages as XHTML with
macros, which does not map cleanly onto Markdown. Rather than regenerating the
whole document on every push, confed keeps a map from each storage block to the
Markdown lines it produced, and re-emits the original bytes for every block you
did not touch. Macros it cannot model are preserved verbatim inside
```` ```confluence ```` fences and round-trip byte for byte.

**Conflicts work like git.** confed keeps three snapshots per page — the base it
last synced, your working file, and what the server has now — so a divergence
gets a real three-way merge with familiar conflict markers, not a "last write
wins" surprise. Pushes carry the page version, so a stale write is refused by
the server rather than silently overwriting a colleague.

**It refuses to lose work.** `pull` stops before writing anything if that would
clobber local changes it cannot merge. `push` will not upload a page with
unresolved conflict markers, and will not delete a page on the server unless you
pass `--allow-delete`.

**Agents are a first-class user.** `confed init` writes `CLAUDE.md` and
`AGENTS.md` into the directory describing the frontmatter contract, the command
cheat-sheet, the conflict workflow, and the do-not-touch list. `--json` gives a
versioned envelope on every command.

## Layout on disk

```
DOCS/
├── Team Handbook.md            a page
├── Team Handbook/              its child pages
│   ├── Onboarding.md
│   └── .Onboarding/            attachments + comments.md for Onboarding.md
├── CLAUDE.md · AGENTS.md       generated agent contract
├── .state.db                   sync state (git-ignored)
└── .session.db                 credentials, mode 0600 (git-ignored)
```

Each page carries YAML frontmatter. `title`, `labels` and `parent_id` are yours
to edit and are synced on push; everything under `confed:` is tool-managed, and
push refuses a page whose managed block was hand-edited.

## Installing

```bash
cargo install --path crates/confed     # from a checkout
```

Requires Rust 1.85 or newer. There are no system dependencies: TLS, SQLite and
the OS keyring integration are all vendored or pure Rust.

## Documentation

- [`docs/quickstart.md`](docs/quickstart.md) — first sync in five minutes
- [`docs/auth.md`](docs/auth.md) — API tokens, PATs, and where secrets are kept
- [`docs/commands.md`](docs/commands.md) — every command, flag and exit code
- [`docs/format.md`](docs/format.md) — frontmatter, file layout, preserved macros
- [`docs/sync.md`](docs/sync.md) — the sync model and the conflict workflow
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — symptoms, causes, fixes

Design documents live in [`docs/design/`](docs/design/01-architecture.md) and the
implementation plan, with what shipped and what did not, in
[`docs/plan/`](docs/plan/README.md).

## Development

```bash
make            # list the available tasks
make test       # unit, wiremock and scenario tests
make lint       # clippy over every target, warnings denied
make ci         # everything the CI pipeline runs
```

The `Makefile` is a thin wrapper over cargo, so `cargo test --workspace` and
friends work equally well; `make ci` exists so a green local run means a green
pipeline.

The workspace is four crates: `confed-api` (both Confluence clients),
`confed-convert` (storage ⇄ Markdown), `confed-core` (state, merge, sync engine)
and `confed` (the CLI). Dependencies flow one way,
`confed → confed-core → {confed-api, confed-convert}`.

Sync behavior is covered end to end by
`crates/confed-core/tests/sync_scenarios.rs`, which runs every scenario against
a stateful in-memory server in both Cloud and Data Center modes.

## Status

Early, but complete enough to use. Both API clients, the converter, the sync
engine, the command set and the TUI are implemented and tested — 459 tests,
including scenarios that run end to end against a mock server in both Confluence
flavors. What is missing is packaging and a release, plus the smaller gaps each
phase file records.

See [`docs/plan/README.md`](docs/plan/README.md) for the state of each phase and
the defects the test suite caught along the way, and
[`docs/design/05-open-questions.md`](docs/design/05-open-questions.md) for the
decisions still open — the minimum supported Data Center version most of all.

## License

MIT or Apache-2.0, at your option.
