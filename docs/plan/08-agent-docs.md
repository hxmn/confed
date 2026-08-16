# Phase 08 — Agent Contracts & Documentation

Depends on: 05 (commands stable enough to document). Parallelizable with 06/07.
Milestone: `init` generates CLAUDE.md/AGENTS.md; JSON schemas published & validated in
CI; `docs/` user documentation complete.

## Generated agent contracts

- [x] Template engine for `CLAUDE.md` + `AGENTS.md` (same content, both filenames), rendered at `init` (and `confed doctor --fix` regenerates if missing/stale via embedded content-hash comment). Sections: what this directory is (space, base URL, flavor); frontmatter contract (writable vs managed table from design 02 §2, "never edit `confed:` block / `.state.db` / `.confed.lock`"); command cheat-sheet with `--json` examples for status/diff/pull/push/comment; conflict workflow (status→pull→resolve→push with exit-code table); comment conventions (sidecar format, `confed:new`); do/don't list.
  - **Accept:** golden rendered file for a fixture binding; stale-detection test; `--no-agent-docs` skips.
  - Implemented in `crates/confed/src/commands/agent_docs.rs`, with unit tests covering
    content, both flavors, every exit code, and identical output for both filenames.
    `crates/confed/tests/cli.rs` asserts `init` writes both files and that they name the
    space.
  - **Remaining:** there is no embedded content-hash comment, so `doctor --fix`
    regenerates only when a file is *missing*, never when it is stale. No stale-detection
    test exists because there is nothing to detect yet.
- [~] Agent-safety affordances audit: verify every documented agent flow works non-interactively (`--json` everywhere, exit codes deterministic, no hidden prompts) — scripted as a test that runs each cheat-sheet example against wiremock.
  - **Accept:** cheat-sheet examples are extracted from the template and executed in CI (docs can't rot).
  - `crates/confed/tests/cli.rs` runs `init`, `fetch`, `pull`, `status`, `diff`, `push
    --dry-run`, `push`, and `whoami` with `--json` against a wiremock Data Center, and
    pins the documented exit codes 0, 2, 3, 7 and 10, plus the "no arguments" and
    `--help` paths.
  - **Remaining:** the examples are written in the test rather than extracted from the
    `agent_docs.rs` template, so the two can still drift. `comment` and `log` are not
    covered end to end.

## JSON schemas

- [x] Write JSON Schema files (`docs/reference/json/<command>.schema.json` + envelope schema), schema_version=1.
  - **Accept:** CI test validates every `--json` snapshot fixture from phases 04–07 against its schema; unknown-field policy documented (additive = non-breaking).
  - `docs/reference/json/` holds `envelope.schema.json` plus `status`, `diff`, `pull`,
    `push`, `fetch`, `init`, `whoami`, `doctor`, `search`, `log`, `clone`, `resolve` and
    `tui`.
  - Validation is against *live* output rather than stored snapshots:
    `crates/confed/tests/schema/mod.rs` implements the JSON Schema subset the documents
    use (`$ref`, `type`, `required`, `properties`, `items`, `enum`, `const`, `minimum`,
    `maximum`, `anyOf`, `additionalProperties: false`) with no new dependency, and every
    `--json` run in `cli.rs` is checked against the envelope schema and its command's
    result schema.
  - The additive-is-non-breaking policy is stated in `envelope.schema.json` and in
    `docs/commands.md`; the validator enforces it by allowing unknown instance fields
    unless a schema says `additionalProperties: false`.

## User documentation (`docs/`)

- [x] `docs/quickstart.md` — install, clone-or-init, edit, push in 5 minutes; separate Cloud and DC tracks.
  - **Accept:** commands copy-paste-verified against wiremock demo script.
  - The DC track's `init` → `pull` → edit → `status` → `diff` → `push --dry-run` → `push`
    sequence is exactly what `crates/confed/tests/cli.rs` executes.
- [x] `docs/auth.md` — Cloud API token walkthrough, DC PAT walkthrough, keyring vs sqlite fallback, CI usage via `CONFED_TOKEN`, security notes (0600, no-log guarantee).
  - **Accept:** reviewed against implementation; env var table matches `--help`.
  - The env-var table is transcribed from `crates/confed/src/cli.rs`; the 0600 property
    and the "no token in stdout or stderr" property are asserted by `cli.rs`.
- [x] `docs/reference/commands.md` — generated from clap (`--help` dump) + hand-written examples; exit-code table; env precedence.
  - **Accept:** CI check that generated portion is current (`confed docs-dump` hidden cmd diffed).
  - Written as **`docs/commands.md`** (top level, next to the other user docs) rather than
    under `reference/`, which now holds only the JSON schemas.
  - **Remaining:** no `confed docs-dump` hidden command and no generated section, so the
    flag tables are hand-maintained and can drift from clap. A "Known gaps" section at the
    end lists every flag that parses but does nothing and every design-04 feature that has
    not shipped.
- [x] `docs/format.md` — frontmatter spec, slug rules, hierarchy/sidecar layout, preserved `confluence` fences, accepted lossiness (from design 02/03, kept as the single user-facing source).
  - **Accept:** cross-links resolve; examples validated by frontmatter parser test.
  - The frontmatter example is the shape `crates/confed-core/src/frontmatter.rs` parses in
    its own tests; the lossiness table is cross-checked against
    `crates/confed-convert/README.md`.
- [x] `docs/sync.md` + `docs/troubleshooting.md` — the three-snapshot model, conflict walkthrough with real marker output, doctor guide, common errors keyed by exit code, rate-limit and large-space guidance.
  - **Accept:** each troubleshooting entry names the exit code and a reproduction.
  - `docs/troubleshooting.md` has one section per exit code 1–10 plus situation entries for
    a stale base, tampered frontmatter, a held lock, no keyring, rate limits, large spaces,
    ignored files, and an `untracked` tree after a rebuilt `.state.db`.
- [x] `docs/README.md` — what confed is, the base/local/remote mental model, and a table of contents for everything above.

## Milestone check

- [ ] Fresh-eyes run: someone (or an agent) follows quickstart against a demo instance using only `docs/`, no source access; friction items filed and fixed.
  - Not attempted: it needs a real Confluence instance, which this environment does not
    have.

## Defects found while documenting

Reported, not fixed, because they live in `crates/*/src/`:

1. ~~**`pull` overwrites an `untracked` page without the dirty guard.**~~ **Fixed.** A file
   whose `page_id` has no base record (deleted or rebuilt `.state.db`) was written over
   from the server silently, losing unpushed edits. `pull` now stops with exit 7 and
   `--force` is the explicit way through. Covered by
   `pull_refuses_to_overwrite_an_untracked_file` in `crates/confed/tests/cli.rs` and
   `pull_refuses_to_overwrite_a_page_it_has_no_record_of` in the core scenarios.
2. **`confed diff --base` is accepted but never read**, so it silently produces the
   ordinary base-vs-local diff instead of the three-way view design 04 describes.
3. **`-y` / `--yes` is accepted but never read.** Nothing prompts for confirmation except
   `push --interactive`, which ignores it.
4. **The "world-readable `.session.db`" error suggests a fix that cannot work.** Its hint
   offers `confed doctor --fix`, but `doctor` opens the session store itself and fails
   with the same exit 7 before any fix runs. `doctor --fix`'s help text also advertises
   fixing "permissions", which it never does.
