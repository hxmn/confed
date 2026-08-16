# confed-convert

Confluence storage format (XHTML + `ac:`/`ri:` macros) ⇄ Markdown, with
block-level patching on the way back.

Implements [design 03](../../docs/design/03-conversion-and-conflicts.md).

## The two guarantees

1. **Nothing is lost.** Anything the converter cannot model is preserved
   byte-for-byte inside a ` ```confluence ` fence and re-emitted unchanged.
2. **Only edited blocks are regenerated.** `markdown_to_storage_patched` diffs
   the new Markdown against the base at top-block granularity and copies the
   *original storage bytes* for every block whose Markdown is unchanged. The
   lossiness in the table below therefore only ever applies to a block the user
   actually edited.

Both are enforced by tests, not by convention:

| Test | Guarantee |
|---|---|
| `tests/invariants.rs` | block spans reproduce the source; spans + gaps reconstruct the document; the block map's line ranges survive a re-split of the Markdown |
| `tests/patching.rs` | identity patch reproduces the base byte-for-byte; editing / deleting / inserting any block leaves every other block's bytes untouched |
| `tests/property.rs` | the same, for random *subsets* of edited blocks (proptest, shrinking) |
| `tests/roundtrip.rs` | `confluence` fences survive md → storage byte-identically; rendering is a fixed point; a corrupted fence is refused |
| `tests/golden.rs` | insta snapshots of Markdown + block map for all 35 corpus fixtures |

## Mapped elements

### Storage → Markdown

| Storage | Markdown | Notes |
|---|---|---|
| `p` | paragraph | empty / `&nbsp;`-only paragraphs are dropped from the map; their bytes survive in the inter-block gap |
| `h1`–`h6` | `#`…`######` | |
| `strong`, `b` | `**…**` | |
| `em`, `i` | `*…*` | |
| `del`, `s`, `strike` | `~~…~~` | |
| `u`, `ins`, `sub`, `sup` | `<u>`, `<sub>`, `<sup>` | inline HTML, round-trips exactly |
| `code`, `tt` | `` `…` `` | backtick run widened as needed |
| `br` | `\` + newline (hard break) | |
| `hr` | `---` | |
| `ul` / `ol` / `li` | `- ` / `1. `, nested | `ol start` is honoured |
| `ac:task-list` | `- [ ]` / `- [x]` | `ac:task-id` is dropped (the server reassigns it) |
| `blockquote` | `> …` | |
| `table` (simple) | GFM table | first row becomes the header row |
| `ac:structured-macro name=code`/`noformat` | fenced block with `language` | |
| `info`, `note` | `> [!NOTE]` | |
| `tip` | `> [!TIP]` | |
| `warning` | `> [!WARNING]` | |
| `panel` | `> [!NOTE]` | a panel is a box, not a severity |
| …admonition `title` param | bold first paragraph inside the alert | |
| `expand` | `<details><summary>…</summary>…</details>` | |
| `toc` | `<!-- confed:toc -->` | |
| `status` | `**\[TITLE\]**` | brackets escaped so the form is a fixed point |
| `ac:image` → `ri:attachment` | `![alt](<attachment_dir>/file.png)` | angle-bracketed when the path has spaces |
| `ac:image` → `ri:url` | `![alt](url)` | |
| `ac:link` → `ri:page` in `page_links` | relative `.md` link | matched by content-id, then title, then `SPACE:title` |
| `ac:link` → `ri:page` elsewhere | `{base_url}/display/{SPACE}/{Title}` | |
| `ac:link` → `ri:user` | `{base_url}/display/~{account-id}` | |
| `ac:link` → `ri:attachment` | `<attachment_dir>/file` | |
| `ac:link` → `ri:url`, `a href` | plain Markdown link | |
| `ac:link ac:anchor` | `#anchor` appended | |
| `ac:emoticon` | unicode emoji | 22 names mapped |
| `ac:inline-comment-marker`, `span` | children only | wrapper dropped |
| `time datetime` | the date text | |
| **everything else** | ` ```confluence ` fence | see below |

### Markdown → Storage

The inverse of every row above, plus:

| Markdown | Storage |
|---|---|
| ` ```confluence ` fence | its body, verbatim (after an XML well-formedness check) |
| ` ```lang ` fence | `code` macro with a `language` parameter; `]]>` inside the body is split across CDATA sections |
| `> [!NOTE]` / `[!TIP]` / `[!WARNING]` | `info` / `tip` / `warning` macro |
| `> [!IMPORTANT]` / `[!CAUTION]` | `note` / `warning` (nearest Confluence equivalent) |
| a bold-only first paragraph in an alert | the macro's `title` parameter |
| `<details><summary>T</summary>…</details>` | `expand` macro |
| `<!-- confed:toc -->` | `toc` macro |
| image path under `attachment_dir` | `<ac:image><ri:attachment/></ac:image>` |
| link whose target is in `link_targets` | `<ac:link><ri:page/></ac:link>` |
| any other HTML block | passed through if well-formed, else escaped into a `<p>` |

## What falls back to a preserved fence

A top-level block becomes a verbatim ` ```confluence ` fence when it — or
anything inside it — cannot be said in Markdown without losing information:

- any macro that is not `code`, `noformat`, `info`, `note`, `warning`, `tip`,
  `panel`, `expand`, `toc`, `status` (jira, drawio, children, page-properties,
  third-party macros…)
- `ac:layout` and its sections and cells
- a `code` macro with a parameter other than `language` (`collapse`,
  `linenumbers`, `theme`) — the parameter would otherwise vanish
- an admonition or `expand` with a parameter other than `title` (`bgColor`,
  `borderStyle`)
- `ac:image` with any attribute other than `ac:alt` / `ac:title` (width, height,
  align, border, thumbnail)
- `ac:emoticon` whose name has no unicode mapping
- a table with `colspan` / `rowspan`, a nested table, or a cell containing block
  content (a list, two paragraphs, a macro)
- **a paragraph, list item or table cell containing any of the above** — the
  taint propagates up to the whole top-level block, because half a paragraph
  cannot be preserved
- XML comments and processing instructions at the top level

The fence body is the exact source bytes. Users and agents may edit inside it
if they know storage format, or delete the whole fence to delete the element;
a fence that stops being well-formed XML fails the push with
`ConvertError::InvalidPreservedBlock` naming the line, rather than uploading a
broken macro.

## Accepted lossiness

Per design 03 §3, all of this applies **only inside blocks the user edited** —
untouched blocks are copied byte-for-byte and preserved fences are never
re-serialised.

| Thing | What happens on edit | Why |
|---|---|---|
| `<span style="color:…">`, fonts, highlights | dropped, text kept | no Markdown spelling; too common to force every styled paragraph into a fence |
| `<b>`/`<i>`/`<s>` | normalised to `<strong>`/`<em>`/`<del>` | Markdown has one spelling |
| `&nbsp;` | becomes a plain space | invisible in Markdown, and it breaks every diff and hash |
| other named entities | expanded to the character (`&mdash;` → —) | unknown ones are left literal rather than dropped |
| `info` vs `note` | both render `> [!NOTE]`; an edited one comes back as `info` | GitHub has no second "informational" keyword |
| `panel` | renders `> [!NOTE]`; an edited one comes back as `info` | a panel has no severity |
| table styling, column widths, `<colgroup>` | dropped on an edited table | not expressible in GFM |
| table header row | the first row always becomes the GFM header | GFM requires one |
| `<p>` wrappers inside table cells | normalised (one `<p>` per cell on the way back) | |
| `ac:task-id` | dropped; the server reassigns | |
| `expand` body | blank lines removed | a blank line would end the HTML block and split one storage block into three Markdown blocks, breaking the block map |
| `toc` parameters | dropped | the marker comment only carries the fact that a TOC was there |
| whitespace / pretty-printing in the source | not reproduced for regenerated blocks | untouched blocks keep theirs exactly |

## Known edge cases

**Fusion.** Two adjacent Markdown lists with the same bullet are *one* list.
The renderer alternates `-` / `*` (and `1.` / `1)`) between neighbouring lists
so sibling storage lists stay distinct blocks. The one case this cannot cover:
if you **delete a block that sat between two lists of the same type**, the lists
become adjacent and genuinely fuse into one Markdown block, so both get
regenerated rather than copied. The result is correct, just not byte-identical.
`tests/patching.rs::deleting_a_block_removes_only_that_block` asserts exactly
this boundary.

**Stale block maps.** `patch` re-splits the base Markdown and checks it against
the stored map (block count, line ranges, and per-block hashes). Any
disagreement returns `ConvertError::StaleBlockMap` so the caller can fall back
to a full regeneration and warn, rather than patching against a map that no
longer describes the document.

**Escaping.** Text is escaped conservatively: `\ ` `` ` `` `*` `[` `]` `<`
always; `_` only when it is not inside a word (so `snake_case` stays readable);
`~` only when doubled; `&` only when it looks like an entity comrak would
decode. Line-leading `#`, `>`, `-`, `+`, `=` and `1.` are escaped so prose
cannot turn into syntax.

## Fixture corpus

35 storage documents in [`fixtures/`](fixtures/README.md), covering every row of
the mapping table plus nested macros, `ac:layout`, CDATA containing `]]`,
colspan tables, images with attributes, every link flavour, emoticons, task
lists, unknown third-party macros (jira, drawio), pretty-printed whitespace,
Markdown metacharacters and unicode. They are synthetic reconstructions of real
page shapes; no customer content is checked in.
