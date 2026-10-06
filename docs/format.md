# File format

What a confed workspace contains, what you may edit in it, and exactly what is lost in
translation between Confluence storage format and Markdown.

- [Frontmatter](#frontmatter)
- [Layout](#layout)
- [Slugs and collisions](#slugs-and-collisions)
- [The body](#the-body)
- [Preserved `confluence` blocks](#preserved-confluence-blocks)
- [Attachments](#attachments)
- [Comment sidecars](#comment-sidecars)
- [Accepted lossiness](#accepted-lossiness)

## Frontmatter

Every page file starts with a YAML frontmatter block. It has two halves: keys you own, and
a `confed:` block the tool owns.

```yaml
---
title: Onboarding            # you own this
labels:                      # you own this
  - hr
  - onboarding
parent_id: '163841'          # you own this
team: platform               # your own key: preserved untouched
confed:                      # tool-managed — do not edit
  schema: 1
  page_id: '163842'
  space_key: DOCS
  version: 7
  status: current
  position: 3
  created: '2026-01-04T09:00:00Z'
  updated: '2026-08-14T11:20:31Z'
  author: Alice Ng
  attachments:
    - id: att901
      file: diagram.png
      size: 48213
      sha256: 6f1c…
---

# Onboarding

First week checklist.
```

### Writable keys

| Key | Type | Effect on `push` |
|---|---|---|
| `title` | string, **required** | Renames the page on the server. |
| `labels` | list of strings | Labels are added and removed to match. Order and duplicates are ignored. |
| `parent_id` | string (a page id) | Moves the page under that parent. |
| *anything else* | any YAML | Preserved verbatim, in its original order. confed never sends it anywhere. |

`title` is the only required key. A file without it fails to parse with exit 7 and a hint
telling you to add it or run `confed pull --force` on the page. A file with no frontmatter
at all is not a confed page: it is reported under `untracked_files` in `confed status` and
otherwise ignored, so a hand-written `README.md` can live in the workspace safely.

### The tool-managed `confed:` block

| Field | Meaning |
|---|---|
| `schema` | Frontmatter contract version. Currently 1. |
| `page_id` | The page's identity on the server. This, not the filename, is what confed tracks. |
| `space_key` | The space the page belongs to. |
| `version` | **The base version**: the server version this file was last synced with. Optimistic locking depends on it. |
| `status` | `current` or `archived`, as reported by the server. |
| `position` | The page's position among its siblings, when the server exposes one. |
| `created`, `updated`, `author` | Server metadata, for reading. |
| `attachments` | The attachment manifest: `id`, `file`, and optionally `size` and `sha256`. |

**Do not edit any of it.** confed compares `page_id`, `space_key`, `version` and `schema`
against `.state.db` on every scan; a mismatch is reported as tampering, `confed status`
lists the page under "Tool-managed frontmatter was edited", and `push` refuses that page
with a `skipped` entry rather than uploading against a version it cannot trust. The repair
is `confed pull --force <page>`, which rebuilds the block from the server.

A file with **no** `confed:` block is a new page. `push` creates it on the server and then
writes the block in. `confed new` scaffolds exactly this.

### What counts as a local change

confed hashes the parts of the file you control: `title` (trimmed), `labels` (sorted and
deduplicated), `parent_id`, your extra keys, and the body (with `\r\n` normalized and
trailing whitespace ignored). The `confed:` block is deliberately excluded, so the version
bump `push` writes does not make the file look dirty a second later, and reordering labels
or reflowing YAML quoting is not a change.

## Layout

A page is a `.md` file. Its children live in a directory named after it, and its
attachments and comments live in a hidden sidecar directory beside it.

```
Team Handbook.md                       page 1001
Team Handbook/                         children of 1001
Team Handbook/Onboarding.md            page 1002
Team Handbook/Onboarding/              children of 1002
Team Handbook/Onboarding/Week One.md   page 1003
Team Handbook/.Onboarding/             sidecar for Onboarding.md
Team Handbook/.Onboarding/storage.xml  its body as Confluence stores it
Team Handbook/.Onboarding/comments.md  its comment thread
Team Handbook/.Onboarding/diagram.png  one of its attachments
```

The rules, precisely:

- The file for a page is `<ancestor slugs…>/<slug>.md`, with ancestors root-first.
- The sidecar for `a/b/Page.md` is `a/b/.Page`. Inside the page body it is referenced as
  `.Page` — a plain relative path from the file's own directory.
- Scanning skips every directory whose name starts with `.`, which is what keeps sidecars,
  `.git` and confed's own state out of the page scan.
- `CLAUDE.md` and `AGENTS.md` are generated files, not pages, and are skipped by name.
- Inside a sidecar, everything is an attachment except `storage.xml`, `comments.md`,
  dotfiles, and `*.confed-part` — confed's own scratch name for a download in flight,
  which `pull` sweeps and `push` never uploads.

confed's own files at the workspace root:

| File | Contents |
|---|---|
| `.state.db` | SQLite: the base snapshot (`pages`), the remote snapshot (`remote_pages`), attachments, comments, the resumable fetch queue, and a sync log. |
| `.session.db` | SQLite, mode 0600: base URL, flavor, auth method, username, and the token if no OS keyring was available. |
| `.pages.db` | SQLite: page bodies confed has already downloaded, keyed by version, plus the attachment and comment snapshots taken with them. A cache — deleting it costs bandwidth, not correctness. |
| `.confed.lock` | Present only while a mutating command runs; carries the pid so a stale one can be cleaned up. |

All four are added to `.gitignore` by `confed init`. `.state.db` is excluded deliberately
as well as `.session.db`: it holds full page bodies, is rebuilt by `init` + `fetch`, and
would conflict on every sync if committed.

## Slugs and collisions

A page title becomes a filename component like this:

1. Normalize to Unicode NFC. Accents, CJK and emoji all survive.
2. Collapse every run of whitespace (including newlines and tabs) into one space.
3. Replace `/ \ : * ? " < > |` and control characters with `-`.
4. Trim, then strip leading dots (a leading dot would hide the file and collide with the
   sidecar convention) and trailing dots and spaces (invalid on Windows).
5. Truncate to 120 bytes, on a character boundary.
6. An empty result becomes `untitled`. A Windows device name — `CON`, `NUL`, `COM1`… —
   gets `-page` appended.

Examples: `Q3/Q4 Plan` → `Q3-Q4 Plan`, `  Team   Handbook \n` → `Team Handbook`,
`.hidden` → `hidden`, `CON` → `CON-page`, `Café Menü` → `Café Menü`.

**Collisions are resolved per parent, not globally.** Two pages called `Notes` under
different parents are simply `A/Notes.md` and `B/Notes.md`. Two siblings with the same
title are ordered by server position, then title, then page id; the first keeps the plain
slug and the others get `~` plus the last six characters of their page id — `Plan.md` and
`Plan~163842.md`. That suffix is derived from the page id, so it is stable across
re-pulls, and the check is case-insensitive because macOS and Windows filesystems are.

**A filename already on disk wins.** When a page has been pulled before, its existing slug
is reserved before any new ones are allocated, so renaming a page on the server does not
rename your file, and a new sibling can never steal a name that is already in use. Because
identity lives in `page_id`, you may rename or move files yourself; `confed status` reports
the move (`moved_from`) and everything keeps working.

## The body

Everything after the frontmatter is CommonMark with GFM tables, task lists and GitHub
alerts. The full mapping in both directions is tabulated in
[`crates/confed-converter/README.md`](../crates/confed-converter/README.md); the short version:

| Confluence | Markdown |
|---|---|
| headings, bold, italic, strikethrough, inline code, rules, lists, block quotes | the obvious Markdown |
| `u`, `ins`, `sub`, `sup` | inline HTML, round-trips exactly |
| `ac:task-list` | `- [ ]` / `- [x]` |
| simple tables | GFM tables (the first row becomes the header) |
| `code` / `noformat` macros | fenced code blocks with a language |
| `info`, `note`, `panel` | `> [!NOTE]`; `tip` → `> [!TIP]`; `warning` → `> [!WARNING]` |
| `expand` | `<details><summary>…</summary>…</details>` |
| `toc` | `<!-- confed:toc -->` |
| `status` | `**\[TITLE\]**` |
| images and links to attachments | `![alt](.Page/file.png)` |
| links to pages in this workspace | relative `.md` links |
| links to pages elsewhere, users | absolute URLs built from the base URL |
| `ac:emoticon` | the Unicode emoji (22 names mapped) |
| **anything else** | a preserved ` ```confluence ` block |

## Preserved `confluence` blocks

Anything the converter cannot express in Markdown without losing information is wrapped in
a fenced block. Confluence stores a macro as one long line; confed lays it out across lines
so it can be read and edited, and that layout is the *only* difference from what the server
holds — a newline goes between two block-level elements inside a container that lays its
children out as blocks, and nowhere else. Never inside a paragraph, an `ac:link`, an
`ac:parameter`, a CDATA body, or a table cell holding inline markup, where the newline
would be a rendered space:

````markdown
```confluence
<ac:structured-macro ac:name="jira" ac:macro-id="9a1f…">
  <ac:parameter ac:name="key">OPS-4711</ac:parameter>
</ac:structured-macro>
```
````

That covers third-party and unmodelled macros (jira, drawio, children, page-properties),
`ac:layout` and its sections, a `code` macro with a parameter other than `language`, an
admonition or `expand` with a parameter other than `title`, an `ac:image` with sizing or
alignment attributes, an emoticon with no Unicode mapping, tables with `colspan`/`rowspan`
or block content in a cell, and top-level XML comments. The taint propagates upward: a
paragraph, list item or table cell containing one of those turns the whole top-level block
into a fence, because half a paragraph cannot be preserved.

A fence you do not touch goes back to Confluence as the exact bytes it came down as: push
copies the original storage for every unchanged block, so the layout never reaches the
server. A fence you *do* edit goes up as the fence reads, indentation and all.

You may edit inside a fence if you know the storage format, and you may delete a whole
fence to delete the element. Two rules:

- **Keep it well-formed XML.** A fence that stops parsing fails the push with a
  `ConvertError::InvalidPreservedBlock` naming the line, rather than uploading a broken
  macro.
- **Never touch `ac:macro-id`.** It is the server's handle for that macro instance.

## Attachments

Attachments live in the page's sidecar directory and are referenced from the body with an
ordinary relative Markdown path:

```markdown
![Network diagram](.Onboarding/diagram.png)

See the [runbook PDF](.Onboarding/failover.pdf).
```

On the way to the server, a link or image whose target is inside the sidecar becomes an
`<ac:image><ri:attachment/></ac:image>` or an `<ac:link><ri:attachment/></ac:link>`. Paths
containing spaces are written with angle brackets — `![alt](<.Onboarding/my file.png>)`.

The `confed:` block's `attachments` list is the manifest confed compares against: `id` is
the server's attachment id, `file` the filename in the sidecar, and `sha256` the hash of
what was last downloaded or uploaded. Drop a new file into the sidecar and `push` uploads
it; change a file and `push` uploads a new version of the same attachment rather than a
duplicate. `confed attach` is the convenient front end for both.

The list follows the server. `pull` rewrites it — and nothing else in the file — when a
file was attached to the page, replaced or deleted in Confluence, whether or not the page
itself changed, and downloads or removes the copies in the sidecar to match. An entry
without a `sha256` is a file the server has that has not been downloaded here yet. See
[sync.md](sync.md#attachments-change-without-their-page).

## Comment sidecars

## The Confluence markup copy

Every page keeps `storage.xml` in its sidecar: the body as Confluence stores it, laid out
across indented lines rather than left as the single line the API returns, refreshed by
`pull` and again by `push`. `pull` writes it for every page in
scope, including ones it had no other reason to touch, so a workspace synced by an older
confed is filled in by the next pull and a deleted copy comes back. A copy that is already
correct is left alone rather than rewritten. It is what the Markdown was
rendered from and what a push patches, so it is the file to read when a conversion looks
wrong — and `confed diff --conf-format` diffs at that level.

It is a copy, not an input. Pushes are built from the Markdown and the block map, so
editing `storage.xml` changes nothing and the next sync overwrites it. To change what
Confluence stores, edit the Markdown, or edit the ```` ```confluence ```` fence inside it
for a macro confed does not model.

confed never uploads it as an attachment, and deleting a page removes it along with the
rest of the sidecar.

## Mentions

Confluence writes a mention as an opaque id:

```xml
<ac:link><ri:user ri:userkey="6cb6d404f61e0043d34f805b8eca16d6"/></ac:link>
```

confed resolves that id against the server, caches the answer, and renders the person's
name linked to their profile:

```markdown
Ask [@Alice Ng](https://wiki.corp/display/~alice.ng) about it.
```

The profile URL is the one the site actually uses: `…/display/~username` on Data Center —
the tilde-prefixed *username*, not the opaque key — and `…/people/<account id>` on Cloud.
Editing a paragraph containing a mention sends it back as a mention, not as an ordinary
link.

Somebody confed cannot resolve — a deleted account, or one it lacks permission to see —
is never linked to a guessed URL. That block keeps its original markup in a
```` ```confluence ```` fence instead, and the next pull that can resolve the person
renders it properly.

## Comments

Comments live in `.<page>/comments.md`. Each entry is introduced by an HTML comment
carrying its metadata, so the file renders cleanly in any Markdown viewer while staying
machine-parseable, and replies are indented under their parent.

```markdown
# Comments — Onboarding (page 163842)

<!-- confed:comment id=98211 author="Bob Lee" date=2026-07-30T10:00:00Z -->
Should this mention the VPN setup?

  <!-- confed:comment id=98230 reply-to=98211 author="Alice Ng" date=2026-07-30T10:01:00Z -->
  Yes — adding it.

<!-- confed:inline id=77120 author="Alice Ng" date=2026-07-30T10:02:00Z anchor="first week checklist" resolved=true -->
Link the checklist template?
```

To write, add markers of your own and run `confed push`:

```markdown
<!-- confed:new -->
Reviewed for Q3.

<!-- confed:new reply-to=98211 -->
Agreed, I will add it.

<!-- confed:resolve id=98211 -->
```

- `confed:new` is a draft: it has no `id` yet, and `push` posts it and gives it one. An
  empty draft marker is ignored rather than posting a blank comment.
- Adding `anchor="…"` to a `confed:new` marker makes it an inline comment;
  `occurrence=N` (1-based) picks one when the text appears more than once. A posted
  draft, and a handled `confed:resolve`, leave the sidecar, so a retried push never
  posts twice.
- `confed:resolve id=…` asks the server to resolve a thread. On Data Center only inline
  threads can be resolved; a footer comment exits 9.
- Editing the body of an existing `confed:comment` does nothing — confed does not update
  comments in place. Reply instead.
- Unpushed drafts survive a `confed pull`: the sidecar is rewritten from the server, and
  your drafts are appended back underneath.

### Inline comments in the page body

Open inline threads are also shown *in the page*, at the text they are about, as a pair
of HTML comments — the mark layer of [design 06](design/06-inline-comment-marks.md):

```markdown
Complete your <!--c 77120 Alice Ng: Link the checklist template?-->first week
checklist<!--/c 77120--> before Friday.
```

- `<!--c ID preview-->` opens the span and `<!--/c ID-->` closes it. The preview is the
  author, the first line of the comment and `(+N)` replies; it is informational and is
  never parsed back. Only open threads are shown; resolved and orphaned ones stay in the
  sidecar.
- Marks are a **layer, not content**. confed strips them before it hashes, diffs, merges
  or uploads a body, so a page is never `Modified` because somebody commented on it, an
  untouched paragraph is never regenerated because it carries a mark, and no mark can
  leak into Confluence. Deleting a mark changes nothing: the next pull puts it back from
  what the server knows. Resolution stays explicit (`confed:resolve` in the sidecar).
- A mark cannot open at the very start of a line — CommonMark would read `<!--` there as
  an HTML block — so confed starts it one character in when it has to. The comment's
  real anchor is unaffected.
- To comment on some text, wrap it with a `new` mark whose preview is your comment:

  ```markdown
  The <!--c new Is this still the right team?-->platform team<!--/c new--> owns it.
  ```

  `confed push` creates the comment and rewrites the mark with its id. On Data Center
  the server wraps the text in a marker, in place (9.5.4) or as a new page version;
  confed takes that change in, so the page stays unchanged.
  Confluence checks the text against its own copy of the page: a draft on a paragraph
  with unpushed edits that cannot be pushed first stops the push with exit 7. The draft is one line and may not contain `--`; a longer body, or a
  span inside a code block, goes through the sidecar's `confed:new anchor="…"` form.
  A `new` mark that is not closed, wraps nothing, or has no text fails the push with
  exit 7 and the line number, before anything is uploaded.
- Editing the text inside a span, or the paragraph around it, keeps the thread attached:
  push writes the marker back into the regenerated paragraph.
- `confed config --set comments.marks ids` writes marks without the preview, and `off`
  keeps page bodies free of marks entirely; `pull --no-comments` skips them for one run.

## Accepted lossiness

Two guarantees bound all of this, and both are enforced by tests rather than by
convention:

1. **Nothing is lost.** Anything unmodellable is preserved in a ` ```confluence ` fence,
   which holds the source subtree with only ignorable whitespace laid out between its
   block-level elements.
2. **Only edited blocks are regenerated.** Push diffs your Markdown against the base at
   top-level-block granularity and copies the *original storage bytes* for every block
   whose Markdown is unchanged.

So the table below applies **only inside a block you actually edited**. An untouched
paragraph keeps its original bytes, styling and all.

| Thing | What happens when you edit its block | Why |
|---|---|---|
| `<span style="color:…">`, fonts, highlights | dropped; the text is kept | no Markdown spelling, and too common to force every styled paragraph into a fence |
| `<b>`, `<i>`, `<s>` | normalized to `<strong>`, `<em>`, `<del>` | Markdown has one spelling |
| `&nbsp;` | becomes a plain space | invisible in Markdown, and it breaks every diff and hash |
| other named entities | expanded to the character (`&mdash;` → —) | unknown ones are left literal rather than dropped |
| `info` vs `note`, and `panel` | all render `> [!NOTE]`; an edited one comes back as `info` | GitHub alerts have no second informational keyword, and a panel has no severity |
| table styling, column widths, `<colgroup>` | dropped | not expressible in GFM |
| the table header row | the first row always becomes the GFM header | GFM requires one |
| `<p>` wrappers in table cells | normalized to one `<p>` per cell | |
| `ac:task-id` | dropped; the server reassigns it | |
| blank lines inside an `expand` body | removed | a blank line would split one storage block into three Markdown blocks and break the block map |
| `toc` parameters | dropped | the marker comment only records that a TOC was there |
| source pretty-printing and whitespace | not reproduced for regenerated blocks | untouched blocks keep theirs exactly |

Two edge cases worth knowing:

**Adjacent lists can fuse.** Two Markdown lists with the same bullet are one list. The
renderer alternates `-`/`*` and `1.`/`1)` between neighbouring lists so sibling storage
lists stay distinct — but if you *delete a block that sat between two lists of the same
type*, they become adjacent and genuinely fuse, so both are regenerated instead of copied.
The result is correct, just not byte-identical.

**A stale block map falls back to a full regeneration.** Push re-splits the base Markdown
and checks it against the stored block map (count, line ranges, per-block hashes). If they
disagree — usually because `.state.db` was rebuilt independently of the file — confed
regenerates the whole body and logs a warning, rather than patching against a map that no
longer describes the document. Blocks you never touched may be reformatted in that one
case.
