# Phase 06 — TUI

Depends on: 05. Parallelizable with 07/08.
Milestone: `confed tui` browses the space, shows diffs, resolves conflicts, and drives
pull/push — all reusing headless command implementations.

- [x] TUI shell: `ratatui` + `crossterm` app scaffold — event loop, terminal guard (restore on panic), theming, keybinding map, help overlay (`?`); `confed tui` requires TTY (exit 2 otherwise).
  - **Accept:** launches/quits cleanly; panic restores terminal (test via induced panic).
- [x] Page tree view: hierarchy from state DB with status badges (M/A/D/B/V/C), position-ordered; search-as-you-type filter; lazy expand.
  - **Accept:** golden-frame tests (`ratatui` TestBackend) for tree rendering incl. badges.
  - Expansion is eager (whole tree built per refresh, rows flattened lazily) with
    per-node collapse, `E`/`C` for all; a space large enough to need lazy loading
    would need `scan` to page too.
- [x] Preview & diff panes: Markdown preview of selection; diff pane = base⇄local, `r` toggles remote diff; hunk navigation.
  - **Accept:** TestBackend goldens; large-file scroll perf sanity (10k-line page).
- [x] Actions wired to core ops: pull selection, push selection (opens plan preview first), open in browser, fetch; long ops run async with progress bar, cancellable (Esc).
  - **Accept:** integration test with MockClient: push from TUI produces same state as CLI push.
  - Esc cancellation stops at the sync engine's next await point (`select!` on a
    oneshot); whatever it already committed stays committed.
- [x] Conflict resolver: 3-pane (ours/base/theirs) with per-hunk take-ours/take-theirs/edit; writes resolution + calls `resolve` core op.
  - **Accept:** scenario test: conflicted page → resolve all hunks → state clean; goldens for the 3-pane layout.
  - Per-hunk take-ours/take-theirs only; there is no in-TUI text editor, so "edit"
    still means leaving the TUI and editing the file.
- [ ] Interactive prompt widgets reused by headless commands when TTY: init space picker, push `--interactive` confirmations, pull clobber warning.
  - **Accept:** prompts render via the same widget layer; non-TTY paths unaffected (regression tests from 01 still green).

## Milestone check

- [ ] Manual script in PR: browse, edit externally, see status change (watcher or `R` refresh), diff, push, resolve a staged conflict — recorded (asciinema) and linked.
