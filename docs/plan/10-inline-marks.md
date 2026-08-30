# Phase 10 — Inline comment marks in the page body

Depends on: 07 (sidecar, comment snapshots, re-anchoring). Design: [design 06](../design/06-inline-comment-marks.md).
Milestone: open inline threads visible at their span in every pulled page; a `new` mark
pushes as an inline comment; no page ever shows `Modified` because of a mark.

## The layer

- [x] `confed-core::marks`: `strip(body) -> (clean, Vec<Mark>)` and `apply(clean, &[PlacedMark]) -> body` per design 06 §3/§8; grammar with numeric and `new` ids, id-carrying closers, stack matching for `new`, malformed numeric marks tolerated with a warning.
  - **Accept:** property tests `strip(apply(clean, marks)) == (clean, marks)`; `strip` is identity on mark-free bodies; overlapping numeric spans round-trip; unterminated `new` reported with its line.
- [x] `PageFile::parse` strips the layer; `body` is always clean, `marks` is a new field; `content_hash` unchanged in signature and therefore mark-blind.
  - **Accept:** a page differing from base only by marks is `Clean`; existing `worktree` tests pass untouched.

## Converter

- [x] `ConvertOptions.inline_marks: HashMap<ref, InlineMark{id, preview}>`; `to_markdown` emits `<!--c ID preview-->…<!--/c ID-->` around `ac:inline-comment-marker` children when the ref is known, drops the wrapper otherwise; wrap-outward for `<code>`/image/autolink; fragment merging within a block, one span per block across blocks; preview sanitizer.
  - **Accept:** fixture 30 renders marks with a map and the existing snapshot without one; new golden fixtures for code, link, `<strong>` fragments, two-block span, marker inside a preserved macro (fence unchanged).
- [x] Block map hashes are computed on stripped block text in both `to_markdown` and `to_storage::patch`.
  - **Accept:** block-map snapshot for fixture 30 is byte-identical with and without `inline_marks`; patching a body that gained a mark in an untouched paragraph copies the original storage bytes.
- [x] `to_storage`: a `c` mark with a numeric id becomes `<ac:inline-comment-marker ac:ref="…">` (ref supplied through `ConvertOptions`), fragmented at inline element boundaries; `new` marks emit nothing; marks inside fences/code rejected with the line (exit 7 upstream).
  - **Accept:** well-formedness check on every generated fixture; a span crossing `<em>` yields two fragments with the same ref; a `c` mark never appears verbatim in generated storage (invariant test over the corpus).

## Sync engine

- [x] Pull renders marks (options built from the `comments` table: open, non-orphaned inline comments only) and refreshes the layer on pages it did not rewrite when the open-comment set changed; writes only if bytes differ; conflicted pages get no marks; `resolve` re-applies.
  - **Accept:** scenarios both flavors: marks appear on pull; remote resolve removes them; remote add inserts them; a `Modified` page keeps its edits through a layer refresh; conflicted page has none, `resolve` restores them.
- [x] Marks refresh stored anchors on read by mutating commands; `reanchor` becomes the fallback for live comments without a mark.
  - **Accept:** edit-inside-span scenario updates `anchor.text` exactly; deleted mark is re-placed by text on the next pull; `reanchor` tests untouched.
- [x] Push: re-insert markers in regenerated blocks; post `new` marks after the body update with computed `textSelectionMatchCount/Index`; rewrite `new` → id in the file; validation errors (exit 7) for unterminated/empty/in-fence drafts; DC → exit 9.
  - **Accept:** wiremock both flavors; uploaded storage for an edited commented paragraph contains the marker; match index correct with repeated text; re-running a half-failed push posts nothing twice.
- [x] Marker freshness: after posting inline comments, re-download the body for that version and refresh base storage + `storage.xml`. (Fetch-time detection of markers added by *others* on an unchanged page is not done: comments on unchanged pages come from the cache today, so there is nothing to compare against without an extra request per page.)
  - **Accept:** scenario: server-side comment without version bump → next fetch refreshes base → unrelated push still carries the marker. This fixes a latent orphaning defect independent of marks; note it in the CHANGELOG.

## Commands and docs

- [x] `status`: `comment_drafts` and `orphaned_comments` per page (JSON + human); body and sidecar drafts share one collection in `plan_push`.
- [x] `comment list`: `placed`, `line`; `comment add --anchor` writes a body mark, `--occurrence N`, `--sidecar` for the old behavior; ambiguous → exit 7 listing occurrences with lines.
- [x] Config `comments.marks = full | ids | off`; `pull --no-comments` covers marks.
- [ ] `mkdocs`/`export --comments highlight` renders spans as `<mark title="…">`; TUI highlights spans and jumps between them.
- [x] Docs: `format.md` (a "Marks" section under Comments), `commands.md`, `sync.md` (layer is stripped before status/diff/merge), `troubleshooting.md` (exit 7 cases), generated `CLAUDE.md`/`AGENTS.md`, JSON schemas for the new fields; design 03 §4 points at design 06.

### Notes on what shipped

The layer lives in `confed-convert::marks` (grammar, `strip`, `apply`, sentinel
placement) so the converter can hash on stripped text; `confed-core` reads it
through `MarkdownFile` and syncs it in `sync::sync_marks`. Two things the design
did not anticipate: CommonMark reads a line whose content starts with `<!--` as an
HTML *block*, so a marker never opens a line — it steps one character in, or moves
to the end of the previous line — and the converter renders a commented block twice
(canonical and with sentinels) and aligns the two, so escaping and whitespace
decisions are always made on the mark-free text. Still open: the `mkdocs`/`export`
highlight rendering and TUI highlighting.

## Milestone check

- [x] Scenario e2e (both flavors): pull page with two inline threads → marks in body, status clean → edit inside one span, wrap a new span with `new` → status shows 1 draft, page `Modified` only because of the text edit → push → uploaded storage keeps the edited marker, new comment created at the right occurrence, file rewritten with its id → remote user resolves the first → pull removes its mark, keeps the second → DC path renders marks and refuses `new` with exit 9.
- [ ] Open question 1 in design 06 answered against a real Cloud tenant and recorded there.
