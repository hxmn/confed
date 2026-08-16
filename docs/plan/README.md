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

| Phase | File | Depends on | Milestone (each ends working & testable) |
|---|---|---|---|
| 01 Foundation | [01-foundation.md](01-foundation.md) | — | `confed whoami --json` scaffolding-level: workspace builds, config resolution, exit codes, DBs, keyring |
| 02 API clients | [02-api-clients.md](02-api-clients.md) | 01 | `whoami`/`spaces`/`search`/`doctor` work against real Cloud + DC and wiremock |
| 03 Conversion | [03-conversion.md](03-conversion.md) | 01 (parallel with 02) | `confed-convert` round-trips corpus; block-map patching proven by tests |
| 04 Sync engine | [04-sync-engine.md](04-sync-engine.md) | 02, 03 | `fetch`/`pull`/`status`/`diff`/`resolve` end-to-end against mock server |
| 05 Commands | [05-commands.md](05-commands.md) | 04 | `push` + full CRUD command set; the tool is daily-usable headless |
| 06 TUI | [06-tui.md](06-tui.md) | 05 | `confed tui` browser + conflict resolver |
| 07 Comments | [07-comments.md](07-comments.md) | 05 (04 for snapshots) | sidecar + `comment` commands, inline read/re-anchor, Cloud inline create |
| 08 Agent docs | [08-agent-docs.md](08-agent-docs.md) | 05 | generated CLAUDE.md/AGENTS.md, JSON schemas, `docs/` user docs |
| 09 Testing & release | [09-testing-release.md](09-testing-release.md) | all | CI matrix, e2e suite, packaged v0.1.0 |

Ordering notes: 02 and 03 are parallelizable after 01. 07 and 08 are parallelizable
after 05. Comment *snapshot fetching* lands in 04; all comment UX is 07.

## Conventions for executing this plan

- Work top-to-bottom within a phase unless a task says otherwise; tasks are sized ≤ 1 day.
- A phase is done when its **Milestone check** (last section of each file) passes.
- Every code task includes its tests in the same checkbox (no separate "add tests later").
- Update this README's table row with ✅ when a phase milestone passes.
- Open questions Q1–Q10 in [design 05](../design/05-open-questions.md) use the
  recommended defaults unless the owner overrides them; record overrides in the design doc.
