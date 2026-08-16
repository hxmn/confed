# Phase 01 — Foundation

Depends on: nothing. Enables: all other phases.
Milestone: workspace builds & is clippy-clean; `confed --help`, `confed config --list
--json`, and the config-resolution/exit-code/DB layers all work with tests.

## Workspace & tooling

- [ ] Create cargo workspace: `crates/confed`, `confed-core`, `confed-api`, `confed-convert`; shared lints (`clippy` pedantic-lite set), `rustfmt.toml`, MSRV pinned in `Cargo.toml`, deny-warnings in CI profile.
  - **Accept:** `cargo build --workspace && cargo clippy --workspace -- -D warnings` pass; deps flow only `confed → core → {api, convert}`.
- [ ] CI skeleton (GitHub Actions): fmt + clippy + test on linux; cache setup.
  - **Accept:** green run on a PR touching each crate.
- [ ] Error model: `ConfedError` enum in `confed-core` with variants mapped 1:1 to the exit-code table (design 04); `anyhow` only at the binary edge.
  - **Accept:** unit test asserts every variant → documented exit code; codes match design table.

## CLI skeleton & output layer

- [ ] `clap` (derive) skeleton: all commands from design 04 registered as stubs (`unimplemented` → exit 1 with "not yet implemented"); global flags (`--json`, `--non-interactive`, `--base-url`, `--token`, `--user`, `--space`, `--flavor`, `--concurrency`, `-C`, `-v/-q/--log`, `--yes`) parsed globally.
  - **Accept:** `confed --help` and `confed <cmd> --help` render for every command; `trycmd` golden tests for help output.
- [ ] Output layer: `Report` type rendering either human text or the JSON envelope (`confed.schema=1`, `ok`, `exit_code`, `result`, `errors[]`, `warnings[]`); `--json` implies `--non-interactive`; NDJSON `--stream` plumbing (used later).
  - **Accept:** snapshot tests of envelope for ok/error cases; envelope matches design 04 exactly.
- [ ] `tracing` init: stderr only, `CONFED_LOG`/`-v` control; `Secret<String>` newtype with redacted Debug/Display used for all credentials.
  - **Accept:** test proves a formatted request log containing a `Secret` renders `***`.

## Config resolution

- [ ] `ConfigResolver`: strict precedence flag → env (`CONFED_*`) → stored → TTY prompt; records source per value; TTY prompt gated on `stdin.is_terminal() && !non_interactive && !json`; missing value in non-interactive → exit 2 with actionable message naming flag + env var.
  - **Accept:** table-driven tests for all 4 sources × interactive/non-interactive; error message content asserted.
- [ ] `confed config` command: `--list` (values + sources, secrets masked), `--get/--set/--unset` against a `config` section in `.state.db meta`.
  - **Accept:** round-trip test; `--list --json` snapshot.

## Storage layer

- [ ] `.session.db` module: create with explicit `0600`; refuse group/world-readable existing file (exit 7); schema from design 02 §3; single-row enforcement.
  - **Accept:** unit tests incl. permission check (unix); schema matches design doc.
- [ ] Keyring integration: store/load/delete via `keyring` crate (`confed` / `<base_url>|<user>`); automatic fallback to sqlite backend when keyring unavailable, recorded in `secret_backend`.
  - **Accept:** mockable backend trait; tests for fallback path; manual smoke on Linux Secret Service.
- [ ] `.state.db` module: full schema from design 02 §4 (meta, pages, remote_pages, attachments, comments, fetch_queue, sync_log), WAL, foreign keys, `schema_version` in meta + migration runner (v1 = create).
  - **Accept:** `PRAGMA integrity_check` clean after create; migration runner test (v0→v1); zstd body compression round-trip test.
- [ ] Advisory lock: `.confed.lock` file lock acquired by mutating commands; friendly exit 7 when held (shows holder pid).
  - **Accept:** test with two processes.
- [ ] Slug module: sanitization, NFC, 120-byte truncation, collision suffix `~<id6>`, path assembly for the `Parent.md` + `Parent/` model.
  - **Accept:** property tests (proptest): output always filesystem-safe, deterministic, collision-free per sibling set; unicode/emoji/long-title cases.

## Milestone check

- [ ] `confed config --list --json` returns a valid envelope with sources; all storage/unit/property tests green; CI green.
