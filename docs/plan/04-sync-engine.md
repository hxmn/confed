# Phase 04 — Sync Engine

Depends on: 02 (clients), 03 (converter). Enables: 05.
Milestone: `fetch` / `pull` / `status` / `diff` / `resolve` fully working end-to-end
against the wiremock server for both flavors, including merges, conflicts, and resume.

## Status computation

- [ ] Worktree scanner: walk directory, parse frontmatter, canonical-hash files, join with `pages`/`remote_pages` → per-page `SyncStatus` per the state table in design 01 §3 (incl. LocalNew/RemoteNew/LocalDeleted/RemoteDeleted/Conflicted).
  - **Accept:** table-driven tests covering all 4×2 hash/version combinations + the six special states; renamed-file re-association by page_id test.
- [ ] `confed status`: human (grouped, colored, stale-fetch warning), `--short` porcelain, `--json`, `--exit-code`, `--fetch`.
  - **Accept:** JSON snapshot; porcelain golden; offline run touches no network.

## Fetch

- [ ] Fetch planner: `list_pages` stream → compare `(id, version)` vs `remote_pages` → write `fetch_queue` (needs body/attachments-meta/comments), mark server-side deletions.
  - **Accept:** wiremock test: only changed pages queued; deletion recorded with `deleted=1`.
- [ ] Fetch executor: worker pool drains queue (bodies, attachment metadata, comment snapshots → `comments` table), one tx per page, `--since` narrowing, progress line/`--stream` events.
  - **Accept:** e2e wiremock test 200-page space; kill-and-rerun test resumes without refetching done pages; partial-failure → exit 8 with failed list.

## Materialization (pull)

- [ ] Writer: page → path via slug module; atomic temp+rename writes; create/rename/delete files + sidecar dirs; hierarchy dirs for parents; attachment download into `.<slug>/`; base (`pages`) updated per-page transactionally with `block_map` + `markdown_hash`.
  - **Accept:** e2e: empty dir + mock space → tree matches expected fixture exactly (files, sidecars, frontmatter); re-pull is a no-op.
- [ ] Pull scoping: positional globs, `--page`, `--label`, `--cql` (server-side where possible, else filter fetched set).
  - **Accept:** tests per scope kind; out-of-scope dirty files untouched.
- [ ] Safety gate: classify each in-scope page (fast-forward / merge / clobber); on non-mergeable clobber or `--no-merge`: stop **before any write**, print highlighted diffs of would-be-lost changes, exit 7; `--force` path; `--dry-run` report.
  - **Accept:** tests: dirty+remote-changed stops with diff; `--force` overwrites; nothing written when stopping (fs snapshot compare).
- [ ] Remote deletions on pull: clean local → delete file+sidecar; dirty local → keep file, warn, mark conflict-like state (exit 4 semantics documented).
  - **Accept:** both paths tested.

## 3-way merge & conflicts

- [ ] Merge engine: regenerate base-md from stored storage+block_map; `diffy` 3-way (base/ours/theirs-md); frontmatter field-wise merge (labels set-merge; title/parent one-side-wins, both-sides→conflict); `confluence` fences never auto-merged (always conflict on overlap).
  - **Accept:** unit tests: clean merge advances base & keeps local dirt; each conflict kind produces markers exactly as design 03 §5 (golden); labels set-merge cases.
- [ ] Conflict lifecycle: `Conflicted` persisted in `pages.sync_state`; `status`/`diff` surface it; `confed resolve [--ours|--theirs|--list]` verifies markers gone, advances base.
  - **Accept:** e2e: diverge → pull (exit 4) → hand-edit → resolve → status shows Modified → (push in phase 05).

## Diff

- [ ] `confed diff`: base⇄local default; `--remote` (uses `remote_pages`, with `--fetch`); `--base` 3-pane text output; `--storage` (via patch assembler); `--stat`, `--name-only`, `--exit-code`; `similar`-rendered colored hunks; JSON hunks per design 04.
  - **Accept:** golden human output; JSON snapshot; exit-code 10 behavior test.

## Milestone check

- [ ] Scenario suite (wiremock, both flavors): clone-like init+fetch+pull → local edit → remote edit (mock mutates) → status shows Diverged → pull merges / conflicts → resolve. All green in CI.
