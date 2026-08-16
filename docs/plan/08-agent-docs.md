# Phase 08 — Agent Contracts & Documentation

Depends on: 05 (commands stable enough to document). Parallelizable with 06/07.
Milestone: `init` generates CLAUDE.md/AGENTS.md; JSON schemas published & validated in
CI; `docs/` user documentation complete.

## Generated agent contracts

- [ ] Template engine for `CLAUDE.md` + `AGENTS.md` (same content, both filenames), rendered at `init` (and `confed doctor --fix` regenerates if missing/stale via embedded content-hash comment). Sections: what this directory is (space, base URL, flavor); frontmatter contract (writable vs managed table from design 02 §2, "never edit `confed:` block / `.state.db` / `.confed.lock`"); command cheat-sheet with `--json` examples for status/diff/pull/push/comment; conflict workflow (status→pull→resolve→push with exit-code table); comment conventions (sidecar format, `confed:new`); do/don't list.
  - **Accept:** golden rendered file for a fixture binding; stale-detection test; `--no-agent-docs` skips.
- [ ] Agent-safety affordances audit: verify every documented agent flow works non-interactively (`--json` everywhere, exit codes deterministic, no hidden prompts) — scripted as a test that runs each cheat-sheet example against wiremock.
  - **Accept:** cheat-sheet examples are extracted from the template and executed in CI (docs can't rot).

## JSON schemas

- [ ] Write JSON Schema files (`docs/reference/json/<command>.schema.json` + envelope schema), schema_version=1.
  - **Accept:** CI test validates every `--json` snapshot fixture from phases 04–07 against its schema; unknown-field policy documented (additive = non-breaking).

## User documentation (`docs/`)

- [ ] `docs/quickstart.md` — install, clone-or-init, edit, push in 5 minutes; separate Cloud and DC tracks.
  - **Accept:** commands copy-paste-verified against wiremock demo script.
- [ ] `docs/auth.md` — Cloud API token walkthrough, DC PAT walkthrough, keyring vs sqlite fallback, CI usage via `CONFED_TOKEN`, security notes (0600, no-log guarantee).
  - **Accept:** reviewed against implementation; env var table matches `--help`.
- [ ] `docs/reference/commands.md` — generated from clap (`--help` dump) + hand-written examples; exit-code table; env precedence.
  - **Accept:** CI check that generated portion is current (`confed docs-dump` hidden cmd diffed).
- [ ] `docs/format.md` — frontmatter spec, slug rules, hierarchy/sidecar layout, preserved `confluence` fences, accepted lossiness (from design 02/03, kept as the single user-facing source).
  - **Accept:** cross-links resolve; examples validated by frontmatter parser test.
- [ ] `docs/sync.md` + `docs/troubleshooting.md` — the three-snapshot model, conflict walkthrough with real marker output, doctor guide, common errors keyed by exit code, rate-limit and large-space guidance.
  - **Accept:** each troubleshooting entry names the exit code and a reproduction.

## Milestone check

- [ ] Fresh-eyes run: someone (or an agent) follows quickstart against a demo instance using only `docs/`, no source access; friction items filed and fixed.
