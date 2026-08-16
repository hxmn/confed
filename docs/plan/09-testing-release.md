# Phase 09 — Testing Hardening & Release

Depends on: all previous phases (earlier phases carry their own tests; this phase is the
cross-cutting harness + shipping).
Milestone: v0.1.0 tagged with CI matrix green, e2e suite, packaged binaries.

## Test harness consolidation

- [ ] `MockConfluence` test-support crate: wraps wiremock with a *stateful* in-memory space (pages/versions/attachments/comments) speaking both API flavors, so scenario tests mutate state instead of hand-written fixtures; migrate phase 04/05/07 e2e tests onto it.
  - **Accept:** one scenario test runs identically against `flavor=cloud` and `flavor=dc` via parameterization; fixture duplication removed.
- [ ] Sync state-machine model test: proptest over random op sequences (local edit / remote edit / pull / push / conflict-resolve) asserting invariants — base only advances to server-confirmed states; no op loses un-pushed local content without `--force`; push never succeeds over a newer remote without `--force-version`.
  - **Accept:** 10k-case run green in CI (reduced case count on PRs, full nightly).
- [ ] Converter nightly fuzz (from 03) + corpus growth process documented (how to add a failing real-world page as a fixture, sanitized).
  - **Accept:** nightly workflow exists; corpus doc in `confed-convert/README`.
- [ ] Optional live-instance smoke suite (feature-gated, `CONFED_E2E_URL/TOKEN` env): init/pull/edit/push/rollback against a real Cloud sandbox and DC-in-Docker; manual/scheduled trigger only.
  - **Accept:** documented runbook; suite passes against both at least once, logged in PR.

## Quality gates & CI matrix

- [ ] CI matrix: linux/macos/windows × stable + MSRV; fmt, clippy -D warnings, tests, doc build; Windows path-length & case-insensitivity slug tests included.
  - **Accept:** matrix green; flaky-test policy documented.
- [ ] Coverage (llvm-cov) with floor on `confed-core` sync + `confed-convert` (≥80%); `cargo-deny` (licenses/advisories); `cargo-audit` scheduled.
  - **Accept:** gates enforced on PRs.
- [ ] Performance sanity benches: 1k-page fetch plan, status on 5k files, convert 1MB page — criterion baselines committed; regression check informational.
  - **Accept:** baselines recorded; status-on-5k under 1s target documented.

## Release

- [ ] Versioning & changelog: semver pre-1.0 policy (breaking JSON/schema changes bump minor + `confed.schema`); `CHANGELOG.md` kept manually; `confed --version --json` exposes all schema versions.
  - **Accept:** policy documented in CONTRIBUTING.
- [ ] Packaging with `cargo-dist`: binaries for linux (x86_64/aarch64, musl), macOS (universal), Windows; shell/pwsh installers; checksums.
  - **Accept:** dry-run release produces installable artifacts on all three OSes.
- [ ] Distribution extras: Homebrew tap formula, `cargo install confed`, container image (for CI agents); install docs updated.
  - **Accept:** each channel install-tested.
- [ ] v0.1.0 release: tag, artifacts, announcement notes (feature list, known lossiness, DC capability gaps).
  - **Accept:** clean-machine install → quickstart succeeds.

## Milestone check

- [ ] All phase milestones ✅ in `docs/plan/README.md`; v0.1.0 published; post-release doctor telemetry-free sanity checklist archived.
