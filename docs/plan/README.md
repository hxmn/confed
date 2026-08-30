# confed — Implementation Plan (master)

Design docs: [`docs/design/`](../design/01-architecture.md). This plan turns them into
nine phases. Every task is a checkbox with acceptance criteria (**Accept:**); check them
off as you go — this file + phase files are the cross-session progress tracker.

## Phase graph

```
01-foundation ──► 02-api-clients ──► 04-sync-engine ──► 05-commands ──► 06-tui
        │                                  ▲                  │
        └────────► 03-conversion ──────────┘                  ├─► 07-comments
                                                              └─► 08-agent-docs ─► 09-testing-release
```

| Phase | File | Status | Milestone (each ends working & testable) |
|---|---|---|---|
| 01 Foundation | [01-foundation.md](01-foundation.md) | ✅ done | `confed whoami --json` scaffolding-level: workspace builds, config resolution, exit codes, DBs, keyring |
| 02 API clients | [02-api-clients.md](02-api-clients.md) | ✅ done | `whoami`/`spaces`/`search`/`doctor` work against real Cloud + DC and wiremock |
| 03 Conversion | [03-conversion.md](03-conversion.md) | ✅ done | `confed-convert` round-trips corpus; block-map patching proven by tests |
| 04 Sync engine | [04-sync-engine.md](04-sync-engine.md) | ✅ done | `fetch`/`pull`/`status`/`diff`/`resolve` end-to-end against mock server |
| 05 Commands | [05-commands.md](05-commands.md) | ✅ done | `push` + full CRUD command set; the tool is daily-usable headless |
| 06 TUI | [06-tui.md](06-tui.md) | ✅ done | `confed tui` browser + conflict resolver |
| 07 Comments | [07-comments.md](07-comments.md) | ✅ done | sidecar + `comment` commands, inline read/re-anchor, Cloud inline create |
| 08 Agent docs | [08-agent-docs.md](08-agent-docs.md) | ✅ done | generated CLAUDE.md/AGENTS.md, JSON schemas, `docs/` user docs |
| 09 Testing & release | [09-testing-release.md](09-testing-release.md) | partial | CI matrix, e2e suite, packaged v0.1.0 |
| 10 Inline marks | [10-inline-marks.md](10-inline-marks.md) | ✅ done (mkdocs/TUI highlighting pending) | inline threads shown in the page body as a stripped mark layer; `new` marks push as inline comments ([design 06](../design/06-inline-comment-marks.md)) |

Ordering notes: 02 and 03 are parallelizable after 01. 07 and 08 are parallelizable
after 05. Comment *snapshot fetching* lands in 04; all comment UX is 07. 10 follows 07 and touches
03's converter and 04's engine.

## Conventions for executing this plan

- Work top-to-bottom within a phase unless a task says otherwise; tasks are sized ≤ 1 day.
- A phase is done when its **Milestone check** (last section of each file) passes.
- Every code task includes its tests in the same checkbox (no separate "add tests later").
- Update this README's table row with ✅ when a phase milestone passes.
- Open questions Q1–Q10 in [design 05](../design/05-open-questions.md) use the
  recommended defaults unless the owner overrides them; record overrides in the design doc.

## Status as of the first implementation pass

Phases 01–05 are complete and committed: the four-crate workspace, both API
clients, the converter with block-level patching, the sync engine, and the
command set. 375 tests pass, clippy is clean, and `confed` runs end to end
against the stateful mock server for both Confluence flavors.

Phase 07 (comments) shipped its data path — the `comments.md` sidecar format,
its parser, comment snapshots in `.state.db`, draft posting on push, and the
`confed comment` subcommands. What is left there is inline-comment re-anchoring
after edits, which is the hard half.

Phase 09 has CI (`.github/workflows/ci.yml`, matrix across Linux/macOS/Windows
plus MSRV and an advisory audit) but not packaging or a release.

Not started: nothing else — but see each phase file's "Notes on what shipped"
section for the specific items inside a phase that were deferred, and
`docs/design/05-open-questions.md` for the decisions still open.
