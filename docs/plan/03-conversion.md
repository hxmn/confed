# Phase 03 — Conversion (storage ⇄ Markdown)

Depends on: 01. Parallel with: 02. Enables: 04.
Milestone: corpus round-trips with byte-identical unchanged blocks; golden fixture suite
for every mapped element; fuzz/property tests green.

## Parsing & model

- [x] Storage parser: `quick-xml` lenient reader → block tree (top-level blocks: p, h1–6, lists, task-lists, tables, code/admonition macros, images, layouts, unknown-macro, raw-passthrough); keeps exact source byte spans per block.
  - **Accept:** parser survives the corpus (below) without panic; spans reproduce original bytes exactly (`&src[span] == block.raw`).
- [x] Fixture corpus: `crates/confed-convert/fixtures/` — ≥25 real-world storage docs covering every element in design 03 §2 table + nasty cases (nested macros, layouts, CDATA in code blocks, emoticons, colspan tables, page/user/external links, images w/ attrs).
  - **Accept:** corpus checked in with provenance notes; loader iterates all in tests.

## Storage → Markdown

- [x] Renderer for direct elements (headings, emphasis, lists, task lists, hr, br, inline HTML for u/sub/sup) via `comrak` AST → CommonMark+GFM.
  - **Accept:** golden `storage.xml → expected.md` fixtures (insta snapshots) per element.
- [x] Macro mappings: code→fence(lang), info/note/warning/tip/panel→GitHub alerts (title as bold first line), status→`**[TEXT]**`, expand→`<details>`, toc→marker comment.
  - **Accept:** golden fixtures each; unmapped params force fallback to preserved block (test).
- [x] Tables: simple→GFM; complex (rowspan/colspan/nested blocks) → preserved block; detection function unit-tested on edge cases.
  - **Accept:** golden fixtures both paths.
- [x] Links & images: attachment `ac:image`→`![](.<slug>/file)`; same-space page links→relative `.md` path (resolver injected as trait, DB-backed later); cross-space/user/external→absolute URLs.
  - **Accept:** golden fixtures; unresolvable page link falls back to absolute URL (test).
- [x] Preserved blocks: unknown macros/layouts → ```` ```confluence ```` fence with exact original bytes.
  - **Accept:** fence body byte-equals source subtree for all corpus unknowns.
- [x] Block map emission: ordered `{storage_span, md_span, kind, hash}` JSON per design 03 §2.
  - **Accept:** invariants tested — spans cover whole doc, non-overlapping, monotonic; serde round-trip.

## Markdown → Storage (block patching)

- [x] Markdown block parser + base-block alignment (LCS over block hashes) per design 03 §3.
  - **Accept:** unit tests: unchanged doc → all aligned; insert/delete/edit/move cases classified correctly.
- [x] Storage generation for changed/new blocks (inverse of every mapping above), incl. `confluence` fence → verbatim bytes + XML well-formedness validation (invalid → `ConfedError::State` naming the block/line).
  - **Accept:** golden `md → storage` fixtures; invalid-fence test.
- [x] Patch assembler: unchanged blocks emit original base bytes; output validated well-formed.
  - **Accept:** **key property test (proptest):** for random corpus doc + random subset of blocks perturbed, all *unperturbed* blocks byte-identical in output; full no-edit round-trip storage→md→storage byte-identical modulo insignificant whitespace (documented normalization).
- [ ] Frontmatter module: serialize/parse per design 02 §2; canonicalization for `markdown_hash`; managed-block tamper detection (diff of `confed:` keys vs state).
  - **Accept:** round-trip + tamper-detection tests; unknown user keys preserved.

## Hardening

- [ ] Fuzz target (`cargo-fuzz`) on the storage parser; run 1h locally, fix crashes.
  - **Accept:** no crashes/oom in 1h run; target wired for CI nightly (09).
- [ ] `confed convert` hidden debug command: `--to-md < in.xml` / `--to-storage < in.md` for manual testing and doctor's converter self-test.
  - **Accept:** used by at least one integration test.

## Milestone check

- [ ] All golden + property + fuzz-smoke tests green; a README table in `confed-convert` documents mapped elements & accepted lossiness matching design 03.
  - Golden (35 fixtures × 2 snapshots), property and invariant suites are green;
    `crates/confed-convert/README.md` documents the mapping and lossiness.
    Fuzz-smoke is the only part outstanding.

### Notes on what is still open

- **Frontmatter module** — belongs with the on-disk layout (design 02 §2); not
  built here so the converter stays free of file-format concerns.
- **Fuzz target** — needs a `fuzz/` crate at the workspace root. The parser is
  already total over the corpus and never slices on a non-char boundary
  (regression covered in `dom::tests`), but that is not a substitute.
- **`confed convert` debug command** — lives in `crates/confed`.
