# Conversion fixture corpus

Storage-format documents used by the golden snapshot tests
(`tests/golden.rs`) and by the block-patch property test
(`tests/roundtrip.rs`).

## Provenance

These are **synthetic reconstructions**, hand-written to match the storage
format Confluence Cloud and Data Center actually emit (element names, attribute
spellings, `ac:schema-version`, `ac:macro-id`, CDATA usage, the `<p>`-wrapped
table cells the editor produces). No customer content is checked in. Each file
is shaped after a real page type — a runbook, a service page, a release
checklist — so the corpus exercises realistic *combinations*, not just isolated
elements.

## What each file is for

| File | Covers |
|---|---|
| `01-basic-prose.xml` | headings + paragraphs + inline emphasis |
| `02-headings-all-levels.xml` | `h1`–`h6` |
| `03-inline-formatting.xml` | `strong`/`em`/`del`/`u`/`sub`/`sup`/`code`, `br`, entities, styled `span` |
| `04-lists-nested.xml` | `ul`/`ol`, three levels deep, `<p>`-wrapped items |
| `05-adjacent-lists.xml` | sibling lists that must not fuse into one Markdown block |
| `06-task-list.xml` | `ac:task-list` with mixed status and inline markup |
| `07-code-macros.xml` | `code` macro with/without language, with inert params |
| `08-code-cdata-edge.xml` | `]]` inside CDATA, and a body containing ``` fences |
| `09-code-collapsed.xml` | `code` macro with unmappable params → preserved |
| `10-admonitions.xml` | `info`/`note`/`warning`/`tip`/`panel`, with and without titles |
| `11-admonition-styled.xml` | panel with `bgColor`/`borderStyle` → preserved |
| `12-admonition-nested-list.xml` | list + code macro inside a `rich-text-body` |
| `13-simple-table.xml` | plain GFM-expressible table |
| `14-table-inline-content.xml` | `<p>`-wrapped cells, literal `|`, links in cells |
| `15-table-colspan.xml` | `colspan`/`rowspan` → preserved |
| `16-table-block-content.xml` | list inside a cell → preserved |
| `17-images.xml` | attachment images, filename with a space, external `ri:url` |
| `18-image-attributes.xml` | `ac:width`/`ac:align` → preserved |
| `19-links.xml` | page/other-space/anchored/user/attachment/external/rich-body links |
| `20-emoticons.xml` | mapped emoticons and an unmapped one |
| `21-expand.xml` | `expand` with and without a title |
| `22-toc-and-status.xml` | `toc` marker comment, inline `status` lozenge |
| `23-unknown-macros.xml` | jira, drawio, children → preserved |
| `24-layout.xml` | `ac:layout` sections and cells → preserved |
| `25-page-properties.xml` | `details` (page properties) macro → preserved |
| `26-nested-macros.xml` | macros inside macros, one of them unknown |
| `27-blockquote-and-rules.xml` | `blockquote`, `hr`, multi-paragraph quotes |
| `28-whitespace-heavy.xml` | pretty-printed source, empty and `&nbsp;` paragraphs |
| `29-mixed-real-page.xml` | a realistic service page using most of the above |
| `30-inline-comment-marker.xml` | `ac:inline-comment-marker` spans |
| `31-html-comments.xml` | XML comments between blocks |
| `32-markdown-metachars.xml` | text that would become Markdown syntax if unescaped |
| `33-deep-list-content.xml` | multi-paragraph and macro-bearing list items |
| `34-empty-and-degenerate.xml` | empty elements, empty CDATA, empty table |
| `35-unicode.xml` | emoji, CJK, RTL, combining marks, numeric refs |
