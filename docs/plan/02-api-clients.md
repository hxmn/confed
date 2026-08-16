# Phase 02 — API Clients

Depends on: 01. Parallel with: 03. Enables: 04.
Milestone: `whoami`, `spaces`, `search`, `doctor`, `init` (auth part), `open` work
against wiremock mocks of *both* flavors and at least one real instance of each.

## Trait & shared HTTP stack

- [ ] Define `ConfluenceClient` trait + domain types (`Page`, `PageSummary`, `Attachment`, `Comment`, `InlineComment`, `Capabilities`, ids as newtypes) per design 01 §2.
  - **Accept:** compiles behind `#[async_trait]`; a `MockClient` (in-memory) implements it for core tests.
- [ ] HTTP stack: `reqwest` client factory (TLS, UA `confed/<ver>`, timeouts) + retry layer: exp backoff w/ full jitter (250ms·2ⁿ, cap 30s, 6 attempts) on 429/502/503/504/connect; honor `Retry-After` (seconds + http-date); POST/PUT retried only on connect-before-send.
  - **Accept:** wiremock tests: 429-with-Retry-After timing respected (tokio test clock), give-up → `ConfedError::Network`, POST not retried after send.
- [ ] Concurrency gate: semaphore sized by `--concurrency`/capabilities default + adaptive token bucket (shrink on 429, slow recovery).
  - **Accept:** test proving ≤N in-flight under 100 spawned calls; bucket shrinks on synthetic 429 burst.
- [ ] Unified pagination: `Paginator` adapters — cursor (`_links.next`) and start/limit — exposed as `BoxStream`; used by every list method.
  - **Accept:** wiremock multi-page fixtures for both styles stream all items exactly once, incl. empty and single-page cases.

## Cloud client (REST v2)

- [ ] Auth (email + API token Basic), flavor probe, `whoami`, `list_spaces`/`get_space` (key→numeric id mapping cached).
  - **Accept:** wiremock fixture tests; 401/403 → exit-3 error with hint.
- [ ] Pages: `list_pages` (v2 space pages, cursor), `get_page` (storage body), `create/update/delete/move_page`; update sends `version.number = base+1`, maps 409 → `ConfedError::Conflict`.
  - **Accept:** fixtures for each; stale-version 409 test; move via v2 (fallback v1 endpoint documented if v2 lacks reorder).
- [ ] Labels, attachments (list/download-stream-to-file/upload multipart/delete), CQL search (v1 `/wiki/rest/api/search` passthrough), version history + page-at-version.
  - **Accept:** fixture tests; large-download test streams (no full-buffer).
- [ ] Comments (Cloud): footer list/create/reply, inline list/create/resolve; fill `Capabilities { inline_comment_create: true, comment_resolve: true, adf: true }`.
  - **Accept:** fixture tests incl. inline anchor payload shape.

## Data Center client (REST v1)

- [ ] Auth (PAT Bearer preferred, Basic fallback), probe, `whoami` (`/rest/api/user/current`), spaces.
  - **Accept:** fixture tests; PAT + Basic both covered.
- [ ] Pages CRUD with start/limit pagination, `expand=body.storage,version,ancestors,metadata.labels`; update with version bump; move via `PUT` ancestors / `/pages/{id}/move` per DC version.
  - **Accept:** fixtures; stale-version conflict test; ancestor→parent_id mapping test.
- [ ] Labels, attachments, CQL search, version history (`/content/{id}/version` expand), page-at-version.
  - **Accept:** fixture tests.
- [ ] Comments (DC): footer list/create/reply via v1 content children; inline **list-only** via `extensions.inlineProperties` expansion; `Capabilities { inline_comment_create: false, comment_resolve: false, adf: false }`; unsupported calls return `ConfedError::Unsupported`.
  - **Accept:** fixture tests; calling `add_inline_comment` yields exit-9 mapping.

## Commands unlocked in this phase

- [ ] `confed init` (auth + space binding parts; agent-docs generation stubbed until 08): detect flavor (host heuristic + probes), verify via whoami, persist session (keyring→sqlite fallback), space selection (`--space` or TTY list picker), create `.state.db`, extend/create `.gitignore`.
  - **Accept:** e2e test against wiremock for both flavors, interactive + `--json` non-interactive paths; `.gitignore` idempotency test (no duplicate lines on re-init).
- [ ] `confed whoami`, `confed spaces`, `confed search`, `confed open` (URL construction per flavor, `--print`), per design 04.
  - **Accept:** JSON snapshots; `search` streams paginated results.
- [ ] `confed doctor` v1: connectivity, auth, flavor, keyring, session perms, state schema, gitignore checks (+`--fix` for perms/gitignore).
  - **Accept:** each check unit-tested via injected failures; JSON snapshot.

## Milestone check

- [ ] Manual smoke recorded in PR description: `init && whoami && spaces && search` against one real Cloud site and one real DC instance (or documented DC-in-docker), both flavors green in wiremock suite.
