# Phase 05 — Commands (push + full CRUD)

Depends on: 04. Enables: 06, 07, 08.
Milestone: the tool is daily-usable headless — push with dry-run/interactive, new/mv/rm,
attachments, clone, log, export; full JSON coverage.

## Push

- [ ] Push planner: statuses → ordered op list (creates parent-first, moves, updates, deletes child-first, attachment uploads before referencing body update); version verification (base == remote_pages; stale → exit 4 "pull first"); conflicted pages refused; deletions excluded unless `--allow-delete`.
  - **Accept:** planner unit tests: ordering on a synthetic tree incl. new-parent-with-new-child; stale-base blocks only affected pages; plan is pure (no IO).
- [ ] Body upload path: patch assembler (03) builds storage; `update_page` with `version = base+1` + `--message`; on success update base + rewrite managed frontmatter (`version`, `updated`); per-page tx; server 409 → record, continue, exit 8.
  - **Accept:** e2e wiremock both flavors: edit→push→remote body matches expected storage (byte-compare unchanged blocks); frontmatter version bumped; 409-mid-batch test.
- [ ] New pages / title / labels / moves: create from `title`-only frontmatter files (parent from dir, `parent_id` override); title rename; labels add/remove diff; move on `parent_id` change; position via `mv --before/--after` data.
  - **Accept:** e2e per op; created file gains full `confed:` block with server ids.
- [ ] Deletions (`--allow-delete`) + attachment sync (new file→upload, changed sha→new version, removed+unreferenced→delete w/ confirm) + body link rewrite md→storage refs.
  - **Accept:** e2e: attach→push→mock holds multipart; delete flows confirm/`--yes`.
- [ ] `--dry-run/--preview` (rendered md diff + storage diff per page, JSON plan) and `--interactive` (per-page y/n/q, TTY-only, exit 2 otherwise).
  - **Accept:** dry-run makes zero HTTP mutations (wiremock verify); JSON plan snapshot; interactive tested via pty harness or input abstraction.

## Local CRUD commands

- [ ] `confed new` (scaffold per design 04; `--template`, `--push`).
  - **Accept:** scaffold golden; push-create e2e.
- [ ] `confed mv` (file+title+parent+position; sidecar moved; `--push`).
  - **Accept:** rename/move/reorder tests incl. parent-dir restructure (page gains children dir).
- [ ] `confed rm` (tombstone + local delete; `--keep-local`, `--push`).
  - **Accept:** rm → status shows LocalDeleted → push --allow-delete e2e.
- [ ] `confed attach` (add/list/rm; frontmatter list maintenance).
  - **Accept:** JSON snapshots; sha256 change detection test.

## Remaining read commands

- [ ] `confed log` (server history + base marker; `--local` from sync_log).
  - **Accept:** fixtures both flavors; JSON snapshot.
- [ ] `confed clone` (init+pull; URL parsing for Cloud `/wiki/spaces/KEY` and DC `/display/KEY`).
  - **Accept:** e2e both URL forms; dir naming rules tested.
- [ ] `confed export --format html|storage` (storage → standalone HTML w/ inlined attachments; pdf deferred).
  - **Accept:** golden HTML for a corpus page; storage export byte-exact.
- [ ] `confed doctor` v2: add tampered-frontmatter, orphaned-files, stale-fetch, converter self-test checks; `completion` subcommand.
  - **Accept:** each new check has an induced-failure test; completions generate for bash/zsh/fish.

## Milestone check

- [ ] Full lifecycle e2e (both flavors, wiremock): clone → new → edit → attach → mv → push → remote-mutate → status/diff → pull-merge → resolve → push → rm → push --allow-delete. JSON snapshots for every step archived as fixtures.
