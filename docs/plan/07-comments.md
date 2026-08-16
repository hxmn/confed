# Phase 07 — Comments

Depends on: 05 (comment snapshots already land in `.state.db` during 04's fetch).
Parallelizable with 06/08.
Milestone: sidecar round-trip + `confed comment` command set; inline read + re-anchoring
everywhere; inline create/resolve on Cloud with capability gating on DC.

## Sidecar format

- [ ] Sidecar writer: `comments` table → `.<slug>/comments.md` per design 02 §5 (threaded indentation, `confed:comment` metadata comments, resolved markers, inline entries with anchor+context); generated during pull when a page has comments.
  - **Accept:** golden sidecar fixtures (threads, resolved, inline, orphaned); regeneration idempotent.
- [ ] Sidecar parser: read sidecar → structured model; recognize `confed:new` drafts (with optional `reply-to`), `resolved=true` additions; ignore (with warning) edits to existing bodies; tolerate hand-formatting noise.
  - **Accept:** parser round-trips writer output; draft/resolve extraction tests; malformed-marker → warning not error.

## Commands (sidecar-primary; CLI edits the sidecar, `--push` syncs)

- [ ] `confed comment list` (`--unresolved`, `--inline`; anchors shown with context; JSON per design 04).
  - **Accept:** JSON snapshot; human golden.
- [ ] `confed comment add` / `reply` (`-m`/`--editor`; writes `confed:new` entry; `--push` posts then rewrites entry with real id).
  - **Accept:** e2e wiremock both flavors; sidecar rewritten-in-place test; draft-without-push then later `confed push` picks it up.
- [ ] `confed comment resolve` (Cloud: API call + sidecar marker; DC: exit 9 with explanation).
  - **Accept:** capability-gate test both flavors.
- [ ] Comment sync in `push`/`pull`: pull refreshes sidecars (preserving local drafts by re-appending them); push posts drafts + resolutions; `--no-comments` opt-outs.
  - **Accept:** e2e: remote gains comment + local has draft → pull keeps draft below refreshed threads → push posts it.

## Inline comments

- [ ] Anchor capture on fetch: extract inline-comment markers/refs from Cloud v2 API and DC body `inlineProperties`; store `{text, context_before/after, marker_ref, block_hash}`.
  - **Accept:** fixture tests both flavors produce identical anchor records.
- [ ] Re-anchoring engine per design 03 §4: exact context match → unique text → fuzzy (`similar` ≥0.75) within mapped block → else `orphaned=true`; runs on pull and before push.
  - **Accept:** table-driven tests: moved paragraph, edited-inside-anchor, deleted anchor, duplicated text; orphan never mis-anchors (property test on perturbed corpus).
- [ ] `confed comment add --inline --anchor "<text>"` (Cloud): unique-match validation (exit 7 with candidate list otherwise), v2 inline-create payload.
  - **Accept:** e2e wiremock; ambiguous-anchor error golden; DC → exit 9.
- [ ] Orphan surfacing: `status` counts orphaned inline comments per page; `comment list` flags them; doctor check.
  - **Accept:** JSON snapshot includes orphan flags.

## Milestone check

- [ ] Scenario e2e (both flavors): pull page with threads+inline → local body edit moves anchor → pull re-anchors → add draft + resolve → push → sidecar shows server ids; DC path degrades exactly as documented.
