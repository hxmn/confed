# 03 — Conversion & Conflict Strategy

Status: draft for review

## 1. Principles

1. **The merge base is storage format, kept verbatim** in `.state.db` (`pages.storage_body`).
   Markdown is a *view*; storage XHTML is the *truth of record* for what the server had.
2. **Never destroy what we can't represent.** Anything the converter doesn't fully
   understand is preserved losslessly in a fenced block and re-emitted byte-identically.
3. **Only regenerate what changed.** Push patches storage format at block granularity;
   untouched blocks are copied from the base verbatim (no accidental churn, no lossy
   re-round-trip of content the user never edited).

## 2. Storage → Markdown (pull)

Pipeline: `quick-xml` parse (storage is XML-ish XHTML with `ac:`/`ri:` namespaces; parser
runs in lenient mode) → intermediate block tree → render CommonMark+GFM via `comrak`.

| Storage element | Markdown |
|---|---|
| `p, h1–h6, strong, em, u, del, sub/sup, br, hr` | direct equivalents (`u`,`sub`,`sup` kept as inline HTML) |
| `ul / ol / li`, task lists (`ac:task-list`) | `- ` / `1. ` / `- [ ]` `- [x]` |
| `ac:structured-macro name=code` | fenced code block with language |
| `ac:structured-macro info/note/warning/tip/panel` | GitHub alert blockquote `> [!NOTE]` etc.; title kept as bold first line |
| `table` (simple: no rowspan/colspan/nested block content) | GFM table |
| `table` (complex) | preserved block (below) |
| `ac:image` → attachment | `![alt](.<slug>/file.png)` (width/other attrs → preserved-block if present) |
| `ac:link` to page in this space | relative link to its `.md` path |
| `ac:link` to other space / user / external `a` | absolute Confluence URL |
| `ac:emoticon` | unicode emoji |
| `ac:structured-macro toc, status, expand(simple)` | mapped best-effort (toc → omitted marker comment, status → `**[STATUS]**`, expand → `<details>`) |
| **everything else** (jira, drawio, layouts `ac:layout`, page-properties, unknown macros) | preserved block |

### Preserved blocks (lossless macro passthrough)

````markdown
```confluence
<ac:structured-macro ac:name="jira" ac:macro-id="7f1a…">
  <ac:parameter ac:name="key">PROJ-142</ac:parameter>
</ac:structured-macro>
```
````

The fence body is the **exact original bytes** of that storage subtree. On push it is
re-emitted verbatim (after XML well-formedness validation — a corrupted edit fails the
push with a pointer to the block, exit 7). Agents/users may edit inside if they know
storage format; may delete the whole block to delete the element; must not edit the
`ac:macro-id`.

### Block map

During conversion, confed records `pages.block_map`: an ordered list of
`{storage_span: [byte_start, byte_end], md_span: [line_start, line_end], kind, hash}`
pairs at the top-block level (paragraph, heading, list, table, macro…). This powers
block-level patching on push and anchor re-location for inline comments.

## 3. Markdown → Storage (push): block-level patching

```
1. Parse local .md → md block sequence.  Parse base .md (regenerable from base storage
   + block_map) → base md block sequence.
2. Align sequences (LCS on block hashes, i.e. a block-level diff).
3. For each aligned-unchanged block  → emit the ORIGINAL storage bytes from the base.
   For each inserted/modified block  → generate storage XML from the Markdown block.
   Deleted blocks → omitted.
4. Concatenate → new storage body. Validate XML well-formedness.
```

**Accepted, documented lossiness** (only within *modified* blocks): unmapped inline
styling (colors, fonts), exotic inline macros inside an edited paragraph, table styling
attributes on an edited table. Editing a paragraph rewrites that paragraph "the Markdown
way". `push --dry-run` renders the storage-level diff so this is visible before upload.
Never lossy: blocks the user didn't touch, and preserved `confluence` fences.

## 4. Inline comments: chosen approach & re-anchoring

**Decision: sidecar with anchor text + context (option 2), not invisible in-body markers.**

> **Revised by [design 06](06-inline-comment-marks.md):** open inline threads are now
> also shown in the body as a stripped *mark layer* (`<!--c ID …-->…<!--/c ID-->`),
> placed from the storage marker rather than by text search. The sidecar, the anchor
> record and the re-anchoring rules below all stay; re-anchoring becomes the fallback.

Why not markers (`<!-- confed:inline … -->` wrapping the span): (a) they corrupt the
block-level diff — every marker makes an untouched paragraph look modified, defeating §3;
(b) editors, formatters, and LLM agents mangle or delete invisible HTML comments;
(c) markers inside inline formatting break CommonMark parsing in edge cases; (d) push
would have to strip them perfectly every time — one failure leaks garbage into the page.
The sidecar costs "you don't see comments while reading the page body" — mitigated by
`confed comment list <page>` showing anchors with context, and the TUI highlighting them.

Anchor record (in `comments.anchor` and the sidecar):
`{ text, context_before (32 chars), context_after (32 chars), marker_ref, block_hash }`
where `marker_ref` is Confluence's own inline-marker reference (Cloud provides it; DC v1
exposes the marker inside the stored body).

Re-anchoring after local edits (best-effort, in order): exact `context+text` match →
unique `text` match → fuzzy match (`similar` ratio ≥ 0.75) within the same mapped block.
Failing all: the comment is marked `orphaned=true` in sidecar and DB — never silently
dropped, and **never deleted server-side by confed**; Confluence itself orphans inline
comments whose anchor text disappears after a body push, which matches our model.

Creating, replying to and resolving inline comments uses the v2 API on Cloud and, on DC,
the undocumented `rest/inlinecomments/1.0` plugin API the page view uses (captures in
`crates/confed-api/tests/fixtures/dc-inline`). DC saves a page version for every new
inline comment; confed adopts it when the marker is the only change. Resolving a footer
comment on DC exits `9 UNSUPPORTED`.

## 5. 3-way merge (`Diverged` pages)

Inputs: **base** = base-version Markdown (regenerated from stored storage body — always
available offline), **ours** = local file, **theirs** = remote version rendered to
Markdown by the same converter. All three are the same representation, so a line-based
3-way merge is meaningful.

- Engine: `diffy` (3-way merge with conflict markers); `similar` renders display diffs.
- Frontmatter merges field-wise: `labels` = three-way set merge (union of additions minus
  deletions); `title`/`parent_id` = take whichever side changed, conflict if both did.
- Body: line-level 3-way merge. Clean → merged file written, base advanced to remote
  version, page stays `Modified` (local edits still pending push). Conflict → git-style
  markers written:

  ```
  <<<<<<< local
  our line
  ||||||| base
  original line
  =======
  their line
  >>>>>>> remote (v9, edited by Alice Ng, 2026-08-14)
  ```

  page marked `Conflicted` (exit 4). `push` refuses conflicted pages. Resolution:
  edit the file and run `confed resolve <page>` (verifies no markers remain, advances
  base to the remote version) — or `confed resolve --ours|--theirs <page>`, or the TUI
  side-by-side resolver. A preserved `confluence` fence that conflicts is never
  auto-merged — always a conflict, because merging raw XML lines can corrupt it.

## 6. Round-trip testing strategy (summary; details in plan 03/09)

- **Corpus snapshot tests** (`insta`): real-world storage documents → md → storage; the
  unchanged-document round trip must be byte-identical *at block level* (§3 guarantees
  it structurally; tests enforce it).
- **Property test**: for random subsets of blocks marked "edited", unedited blocks are
  byte-identical in output.
- **Golden pairs**: curated `storage.xml ⇄ expected.md` fixtures for every mapped
  element; CI fails on unreviewed snapshot changes.


## 7. Where the implementation diverged from this design

**Renderability taint.** A block is classified not only by its shape but by
whether it can be expressed in Markdown without loss. An unknown macro inside a
paragraph, a code macro with `collapse=true`, an image with a width attribute, a
table with `colspan` — any of these demotes the whole top-level block to a
verbatim `confluence` fence. Half a paragraph cannot be preserved, so the unit
of preservation has to be the whole block.

**Adjacent lists alternate bullets.** Two Markdown lists using the same bullet
character are a single list, which would fuse two storage blocks and invalidate
the block map. Sibling lists therefore alternate `-`/`*` and `1.`/`1)`.

**`status` renders as escaped `**[X]**`.** The design proposed `**[STATUS]**`,
but that is not a fixed point: the next pull escapes the brackets, and the file
looks edited when nothing changed.

**`info`, `note` and `panel` all render as `> [!NOTE]`.** GitHub alerts have no
second informational keyword, so an edited admonition returns to the server as
`info`.

**One genuine limitation.** Deleting a block that sat between two lists of the
same type makes those lists adjacent, so Markdown fuses them and both are
regenerated. The test suite asserts this boundary rather than papering over it.
