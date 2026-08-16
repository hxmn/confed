# 05 — Open Questions & Risks

Each needs your input; every one has a recommended default that the plan assumes.
Numbered for reference (Q1…); risks follow.

## Decisions needing input

**Q1 — Is `.state.db` git-ignored?**
Recommended: **yes** (see design 02 §4: derived cache, merge-hostile, rebuildable via
`init` + `fetch`). Alternative: a committed lightweight manifest (`.confed/manifest.yml`
with page ids/versions, no bodies) so a fresh git clone knows the base versions without
network. Could be added later without breaking anything.

**Q2 — Hierarchy representation: `Parent.md` + sibling `Parent/` dir (recommended) vs
`Parent/index.md`.** The sibling model matches your attachment-dir example and keeps
leaf pages as plain files; the cost is that renaming a parent touches two paths (confed
`mv` handles both atomically).

**Q3 — Sync format on Cloud: storage vs ADF.**
Recommended: **storage everywhere** — one converter, one merge-base format, DC parity.
Risk: Atlassian is ADF-first on Cloud; some new elements may round-trip poorly through
their storage representation. Mitigation: `confed-convert` is format-agnostic at the
block-map layer, so an ADF backend can be added in a later major phase.

**Q4 — Admonition mapping: GitHub alerts (`> [!NOTE]`) — recommended — vs preserved
`confluence` fences.** Alerts are readable/editable everywhere but GitHub-specific;
fences are lossless but ugly for the most common macro. Configurable later if needed.

**Q5 — Comment primary mechanism: sidecar file (recommended) with CLI as accessor, vs
CLI-primary with sidecar as read-only view.** Sidecar-primary keeps everything
offline-first and agent-editable with plain file tools; the CLI guarantees well-formed
edits. Justification in design 02 §5.

**Q6 — Deletions on push: skipped unless `--allow-delete` (recommended) vs symmetric
with other changes.** Accidental `rm -rf` of a directory should not silently delete a
Confluence subtree. Cost: one extra flag when you really mean it.

**Q7 — Editing existing comment bodies**: out of scope for v1 (only add/reply/resolve).
Confluence permits editing your *own* comments; supporting it means tracking comment
body dirtiness. Recommended: defer.

**Q8 — Multi-space in one directory**: not supported in v1 (one dir = one space).
Recommended: keep; `clone` makes per-space dirs cheap.

**Q9 — Concurrent-writer protection**: advisory `.confed.lock` per directory
(recommended) — protects against two confed processes, not against editors writing
mid-push (mitigated by hashing at plan time and atomic temp-file writes).

**Q10 — Minimum supported DC version**: recommended 8.x+ (PAT support landed in 7.9;
8.x simplifies testing matrix). Confirm what your org runs.

## Top risks & mitigations

| # | Risk | Mitigation |
|---|---|---|
| R1 | **Converter fidelity** — storage format has a long tail of macros/layouts; a bad round-trip corrupts real pages | Block-level patching (design 03 §3) bounds damage to edited blocks; preserved fences for everything unknown; corpus snapshot tests from day one; `push --dry-run` shows the exact storage diff; `sync_log` + server version history make every push revertible (`confed log` → restore) |
| R2 | **DC inline-comment API gaps** (no create/resolve) | Capability gating + explicit exit 9; read-only sidecar on DC is still useful |
| R3 | **Cloud rate limits** on large-space clone (thousands of pages + attachments) | Bounded pool + adaptive token bucket + `Retry-After`; resumable `fetch_queue`; `--since` incremental fetch |
| R4 | **Re-anchoring wrongness** for inline comments after heavy edits | Conservative matching thresholds; orphan rather than mis-anchor; never delete server-side |
| R5 | **Keyring absent** (headless/CI) | Automatic sqlite fallback at 0600 + doctor warning; document `CONFED_TOKEN` as the CI-preferred path (no persistence) |
| R6 | **Path/slug edge cases** (unicode titles, case-insensitive filesystems, long paths on Windows) | Slug rules in design 02 §1; collision suffixing; CI matrix incl. Windows; property tests on slugger |
| R7 | **Schema evolution** of `.state.db` / frontmatter / JSON output | Explicit schema versions in all three; migrations table; JSON schema files versioned in docs |
