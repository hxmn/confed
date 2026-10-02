# 06 — Inline comments in the page body

Status: implemented (plan 10); `mkdocs`/TUI highlighting and open question 1 remain.
Supersedes the "sidecar only" half of design 03 §4; the sidecar itself, the
`confed comment` commands and the re-anchoring engine all stay.

## 1. Problem

Inline comments are invisible where they matter. A reader of `Onboarding.md` cannot
tell that "first week checklist" has an open thread; they have to know to open
`.Onboarding/comments.md` and match `anchor="…"` back to the prose by hand. Adding one
means typing the exact anchor text into a sidecar marker or a `--anchor` flag, and it
only works when that text is unique on the page.

This document puts inline comments *in the Markdown*, at the span they belong to, and
lets a new comment be added by wrapping text — without giving up the three guarantees
the rest of confed is built on: untouched blocks round-trip byte for byte, a comment
never makes a page look edited, and nothing confed writes can leak into a Confluence
page.

## 2. What the user sees

```markdown
Complete your <!--c 77120 Alice Ng: Link the checklist template?-->first week
checklist<!--/c 77120--> before Friday, then ask <!--c 77304 Bob Lee: Still true?
(+2)-->IT for a laptop<!--/c 77304-->.
```

- `<!--c ID preview-->` opens a span, `<!--/c ID-->` closes it. The span is the text
  Confluence highlights.
- The preview is the author and the first line of the root comment, cut at 60
  characters, plus `(+N)` when the thread has replies. It is informational: confed never
  parses it back, and editing it changes nothing.
- Only **open** threads are shown. Resolved and orphaned comments stay in the sidecar
  (`resolved=true`, `orphaned=true`), as today.
- Everything else about a thread — replies, dates, full bodies, resolving — lives in
  `.<page>/comments.md`, unchanged. The body mark is a pointer into it: `confed comment
  reply 77120 -m …`, or `<!-- confed:resolve id=77120 -->` in the sidecar.

To add a comment, wrap the text and say what you want to say:

```markdown
The <!--c new Is this still the right team?-->platform team<!--/c new--> owns it.
```

`confed push` creates the comment, and the mark is rewritten with its real id. A body
that needs more than one line, or a span that overlaps another draft, goes through the
sidecar's `<!-- confed:new anchor="…" -->` form, which keeps working.

Marks are HTML comments, so any Markdown renderer — GitHub, `confed mkdocs`, an editor
preview — shows the page exactly as before. The raw text, which is what editors and
agents read, shows the threads in place.

## 3. The rule that makes it safe: marks are a layer, not content

Design 03 §4 rejected in-body markers because (a) they make every commented paragraph
look modified to the block differ, (b) editors and agents mangle invisible comments,
(c) they can break CommonMark inline parsing, and (d) push would have to strip them
perfectly or leak them into the page. All four fall to one rule:

> **Every reader of a page body strips the mark layer first.** The stripped body is
> what gets hashed, diffed, merged, patched and uploaded. Marks are re-applied from the
> database on the way back out.

Concretely, `confed-core` gains one module, `marks`, with two functions:

```rust
/// Split a body into its content and the marks that were in it.
pub fn strip(body: &str) -> (String, Vec<Mark>);
/// Put marks back, given byte offsets into the stripped body.
pub fn apply(body: &str, placed: &[PlacedMark]) -> String;

pub struct Mark {
    pub id: MarkId,            // Comment("77120") | New
    pub start: usize,          // byte offsets into the *stripped* body
    pub end: Option<usize>,    // None: opener with no closer
    pub text: String,          // what the span currently contains
    pub draft_body: Option<String>, // the preview text of a `new` mark
    pub line: usize,
}
```

`strip` is called in exactly one place, `frontmatter::PageFile::parse`, so the body that
`worktree`, `status`, `diff`, `merge` and `push` see is always clean. That single choke
point is what answers (a) and (d): the block differ compares clean text against clean
text, so a mark never turns an untouched paragraph into a regenerated one; and the
converter never sees a `c` mark as inline HTML, so nothing can pass through to storage
verbatim. `content_hash` hashes the clean body, so a page whose only change is a new
mark stays `Clean` — the draft is reported as comment work, not page work (§7).

(b) is answered by not trusting the layer: the database, not the file, is the record of
where a comment is. A mangled or deleted mark costs nothing — the next pull re-places it
from the stored anchor, using the existing text-search engine as the fallback (§5).

(c) is answered by *where* marks are emitted, which is the converter's job (§4).

## 4. Rendering: the converter places marks at DOM boundaries

Storage already tells us exactly where every inline comment is:

```xml
<p>Complete your <ac:inline-comment-marker ac:ref="9f2c-1">first week
checklist</ac:inline-comment-marker> before Friday.</p>
```

and both APIs hand back that ref (`properties.inlineMarkerRef` on Cloud v2,
`extensions.inlineProperties.markerRef` on Data Center v1). Today the converter renders
the marker's children and drops the wrapper. Instead, `ConvertOptions` gains

```rust
/// Inline comments to mark in the body, keyed by Confluence's marker ref.
pub inline_marks: HashMap<String, InlineMark>,   // ref → { id, preview }
```

and `to_markdown` emits `<!--c ID preview-->` before the marker's children and
`<!--/c ID-->` after them when the ref is in the map. A ref that is not — resolved,
orphaned, or a comment confed has not fetched — is dropped exactly as now. Placement at
DOM level rather than by text search means repeated text, edited spans and adjacent
comments are all exact, and no fuzzy matching happens on the pull path at all.

Rules that keep the output valid CommonMark:

- **Atomic constructs are wrapped, not entered.** A marker inside `<code>`, an image or
  an autolink moves outward to enclose the whole construct; an HTML comment inside a
  code span would be literal text.
- **Fragments merge.** Confluence splits a marker that crosses an inline boundary into
  several elements with the same ref. Fragments in one block become one span from the
  first open to the last close. Fragments in different blocks become one span per block
  with the same id; the preview appears on the first only.
- **Preserved blocks and code are untouched.** A marker inside a ```` ```confluence ````
  fence or a code macro stays in the fence as raw storage — lossless, just not rendered
  as a mark. The comment is still listed in the sidecar.
- **Preview is sanitized.** Whitespace collapsed, `--` replaced with `–`, `>` never
  first, so the marker is a well-formed CommonMark HTML comment.

Two mechanics keep that exact. First, a commented block is rendered twice — once
canonically, once with a private-use sentinel character wherever a marker opens or
closes — and the sentinel offsets are mapped onto the canonical text by character
alignment. Every decision about the text's *shape* (escaping a leading `-`, pushing
whitespace out of emphasis) is therefore made on the mark-free text, and a block with
marks strips back to exactly the block without them. Second, the placed result is
re-parsed and compared to the canonical parse; a mark that changes the structure is
nudged outside the emphasis delimiters it touches, and dropped for that block if that
does not help.

One CommonMark rule shapes placement: a line whose content begins with `<!--` is an
HTML *block*, not inline HTML. So a mark never opens a line. At a continuation line the
opener moves to the end of the previous line (before a hard-break backslash); at a
block's first line it steps one character in, past emphasis delimiters, or past a whole
code span or image. `apply` does this repair and it is idempotent, so a `new` mark a
user wrote at a line start is repaired the same way before push parses the body. The
stored anchor is not affected: when a mark starts a character or two late, the anchor
text is kept as long as it still fits around the mark.

Because a mark is inline and contains no newline, `md_span` in the block map does not
move. `BlockEntry.hash` is computed on the block's *stripped* text, so a block map built
with marks equals one built without.

The base Markdown that `build_storage` regenerates for patching uses the same options,
so it carries the same marks; both sides are stripped before alignment, so the marks
cancel out even if the comment set changed between pull and push.

## 5. Reading: the file is authoritative for where a live comment sits

When a mutating command (`pull`, `push`, `comment …`) reads a page file, the marks it
finds refresh the stored anchor: `anchor.text` becomes the span's current text, context
is recomputed. Editing *inside* a commented span — the case the fuzzy matcher handles
least well — becomes exact, because the marks moved with the text.

The re-anchoring engine (`reanchor.rs`) keeps its rules and becomes the fallback tier,
used when the file has no mark for a live comment:

| Situation | How the comment is placed |
|---|---|
| Storage has the marker, file is being (re)written from storage | DOM placement (§4) |
| File has the mark | its position, refreshed into the DB |
| Mark missing (mangled, deleted, or the page was merged) | `reanchor` by text + context; orphan if not found |
| Storage has no marker for a live comment (some Cloud pages edited in the new editor) | `reanchor` by text; the mark is rendered from that offset |

The fallback writes a mark only where the anchor text sits verbatim (exact, unique or
context-picked match, never fuzzy), and `apply` refuses any span with an edge inside a
fenced block, a code span or an HTML tag. A comment whose marker is in a preserved
```` ```confluence ```` block is therefore found by text but never marked there: it
stays in the sidecar, and the fence stays byte-identical to storage.

The layer is (re)applied whenever confed writes a page file, and additionally on every
pull for pages it did not otherwise rewrite, when the set of open inline comments
changed: read → strip → apply → write only if the bytes differ. A comment resolved on
the server disappears from the body on the next pull; one added on the server appears.
Content is never touched by this pass, so a `Modified` page stays exactly as modified.

**Conflicted pages carry no marks.** A file with conflict hunks has two candidate texts
for the same span; placing marks would be a guess. `confed resolve` re-applies the layer
once the hunks are gone.

## 6. Pushing: existing comments survive an edit, new ones are created in place

`push` reads the file (stripped body + marks) and:

1. **Body.** The block patcher aligns stripped blocks. Unchanged blocks copy their
   original storage bytes — markers included, as today. For a *regenerated* block the
   generator now re-inserts `<ac:inline-comment-marker ac:ref="…">` around each span
   whose mark has a known id, looking the ref up in the `comments` table. A mark that
   crosses an inline element is closed before the element ends and reopened after it,
   the same fragmenting Confluence itself does, so the XML stays well-formed. Today an
   edit anywhere in a paragraph drops its markers and the server orphans the thread;
   this closes that gap. `new` marks are not written into storage — Confluence inserts
   those markers itself when the comment is created.
2. **Drafts.** Every `new` mark becomes an inline-comment create, after the body update
   so the text exists in the new version. `textSelectionMatchCount` and
   `…MatchIndex` are computed from the span's position: occurrences of the plain span
   text in the blocks before it plus its index within its block. This lifts the current
   "anchor text must be unique on the page" restriction. Sidecar drafts with `anchor=`
   are posted the same way, with the unique-match rule they have now.
3. **Rewrite.** Each posted `new` mark is rewritten in the file with its real id, so a
   push that fails halfway is safe to re-run without double-posting. The sidecar gets
   the new entry as it does today.
4. **Marker freshness.** Creating an inline comment changes the server's storage body —
   a new marker appears — without, on Cloud, a page version bump. `.pages.db` keys bodies
   by version, so the base would keep a body without that marker, and a later push of an
   unrelated edit would copy the stale bytes and orphan the comment. After posting,
   confed re-downloads the body for that version and refreshes base storage and
   `storage.xml`. This is a latent defect today, independent of marks. Markers added
   by *other* people on a page whose version did not change are not detected: comments
   on unchanged pages come from the cache, so there is nothing to compare against
   without one extra request per page (the same limitation the sidecar has today).

Validation, before anything is uploaded (exit 7, pointing at the line):

- a `new` mark with no closer, or a closer with no opener;
- a `new` span inside a preserved fence or code block (comment on it from the sidecar
  instead — Confluence cannot anchor to macro internals either);
- an empty draft body.

A malformed mark with a *numeric* id is a warning, never an error: the comment is still
in the database and is re-placed on the next pull.

Data Center's public API has no inline create, so DC goes through the plugin API the
page view uses (`rest/inlinecomments/1.0`, undocumented; captures of 9.5.4 in
`crates/confed-api/tests/fixtures/dc-inline`). Three things differ from Cloud:

- **The selection is the server's.** `originalSelection`, `matchIndex` and `numMatches`
  are checked against the server's own text extraction (HTTP 412 on a mismatch). confed
  computes them from the page's current storage (`confed_convert::selection`): text
  nodes with entities decoded, `&nbsp;` kept as U+00A0 (a typed space matches it, and
  the page's own character is sent), macro parameters and code excluded, no match
  across blocks. A draft whose paragraph has unpushed edits that cannot be pushed first
  is refused with exit 7 rather than sent.
- **Creating a comment saves a page version.** After each post confed fetches the page;
  if the new version is the base plus one and, with the new comment's marker taken out,
  reads the same as the base, it is adopted — base storage and version, the file's
  `confed.version`, and (for an untouched file) the body, re-rendered so file and base
  agree. Anything else is somebody else's edit and is left for `pull`.
- **A draft at a paragraph's start** sits one character in (§4); push takes the
  comment to cover the whole first word (`marks::intended_start`).

## 7. Command surface

| Command | Change |
|---|---|
| `pull` | renders marks; refreshes the layer on unrewritten pages; `--no-comments` also skips marks |
| `push` | §6; `--json` `comments_added[]` gains `anchor` and `line` |
| `status` | per page: `comment_drafts` (body + sidecar) and `orphaned_comments`; human output shows `+1 comment draft` beside the state — closes the open item in plan 07 |
| `diff` | unaffected: both sides are stripped |
| `comment list` | entries gain `placed: bool` and `line`; `--inline` shows `L12` beside the anchor |
| `comment add --anchor TEXT` | writes a body mark instead of a sidecar draft; on several occurrences lists them with line numbers (exit 7) and accepts `--occurrence N`; `--sidecar` keeps the old behavior |
| `comment resolve` | unchanged; the mark leaves the body on the next pull |
| `resolve` | re-applies the layer to the resolved file |
| `mkdocs` / `export` | optional `--comments highlight`: render spans as `<mark title="preview">` so the rendered site shows threads too |
| `tui` | highlight marked spans in the page view; jump between them — what plan 06 promised |

Configuration: `comments.marks = full | ids | off` in the workspace config (`full` is the
default; `ids` renders `<!--c 77120-->…<!--/c 77120-->` with no preview for people who
find prose noisy). `off` restores the current behavior entirely; the sidecar and
`comment` commands do not depend on the layer.

The generated `CLAUDE.md` / `AGENTS.md` gain a paragraph: what a `c` mark is, that
deleting one changes nothing, how to add a `new` one, and that replies and resolutions
still go through the sidecar. That is the workflow this is for: an agent pulls, reads
the thread in place, fixes the text, replies and resolves in the sidecar, pushes.

## 8. Mark grammar

```
open    := "<!--" ws? "c" ws id ( ws preview )? ws? "-->"
close   := "<!--" ws? "/c" ws id ws? "-->"
id      := DIGIT+ | "new"
preview := text without "-->"; for id = "new" it is the draft body
```

- Closers carry the id so overlapping comments are unambiguous
  (`<!--c 1-->aa <!--c 2-->bb<!--/c 1--> cc<!--/c 2-->`). `new` closers match the
  nearest unclosed `new` opener; overlapping drafts use the sidecar.
- Whitespace inside the delimiters is ignored; the span's text begins immediately after
  `-->` and ends immediately before `<!--`.
- Any HTML comment that does not match the grammar is ordinary content and keeps
  today's behavior (passed through to storage as inline HTML). `c` followed by a number
  or `new` is specific enough that prose does not collide with it.
- The tag is one letter because it sits inside sentences; `confed:` stays the prefix
  for block-level markers (`confed:toc`, sidecar entries).
- Fenced code blocks and inline code spans are opaque: a mark-shaped comment inside
  them is literal text, left alone by `strip` and reported as an issue.

## 9. Alternatives considered

- **Footnotes** — `first week checklist[^c77120]` with definitions at the end of the
  file. Renders beautifully on GitHub, but marks only the end of a span, and the
  definitions look like content to an agent and to the differ unless they are stripped
  too — at which point they are just a worse layer.
- **CriticMarkup** — `{==text==}{>>comment<<}`. A real convention, but it has no id
  slot, is not CommonMark, and its braces leak into every rendered view.
- **Links into the sidecar** — `[first week checklist](.Onboarding/comments.md#c77120)`.
  Clickable everywhere, which is attractive, but links cannot nest, so any commented
  span containing a link cannot be expressed, and the converter would have to
  special-case a link destination.
- **Status quo plus tooling** — `confed comment show` with context, TUI highlighting.
  Does not help the raw-text reader, who is the primary audience.

HTML comments with an id are the only option that is invisible when rendered, legible
when raw, nestable inside any inline construct, and cheap to recognize with certainty.

## 10. Testing

- **Converter golden:** fixture 30 with an `inline_marks` map renders marks; without it
  the snapshot is unchanged. New fixtures: marker inside `<code>`, inside a link, across
  `<strong>` (fragments), across two blocks, inside a preserved macro.
- **Layer properties:** `strip(apply(clean, marks)) == (clean, marks)` for generated
  inputs; block hashes are identical with and without the layer; `strip` never changes
  a body that has no marks.
- **Generator:** a mark crossing `<em>` produces two well-formed fragments with the same
  ref; `new` marks produce no XML; marks inside fences are rejected with the line.
- **Core scenarios (both flavors):** pull renders marks and status is `Clean`; adding a
  `new` mark leaves the page `Clean` with `comment_drafts: 1`; push creates the comment
  with the computed match index and rewrites the mark; editing a commented paragraph
  and pushing keeps the marker in the uploaded storage; a comment resolved remotely
  leaves the body on pull; a deleted mark is re-placed on pull; a conflicted page has no
  marks and gets them back on `resolve`; a DC `new` mark posts and the version it
  saves is adopted.
- **Freshness:** a comment created on the server without a version bump refreshes the
  base body, and a subsequent unrelated push still carries the marker.

## 11. Open questions

1. **Does Cloud honor `ac:inline-comment-marker` in a storage-format PUT?** The legacy
   editor relied on it; the new editor stores ADF and converts. If the server drops
   re-inserted markers on regenerated blocks, the comment is orphaned — exactly today's
   behavior, so the design degrades to the status quo rather than below it. Verify on a
   real tenant before claiming §6 step 1 in the docs.
2. **Does creating an inline comment bump the page version on either flavor?** The
   freshness rule in §6 step 4 does not depend on the answer, but the cost of the extra
   download does.
3. **Default `full` vs `ids`.** `full` is proposed because the preview is what makes an
   agent able to act without opening the sidecar; the objection is prose noise.
4. **Should removing a mark mean anything?** Proposed: no, ever. An editor that strips
   HTML comments must not resolve anyone's thread. Resolution stays explicit.
