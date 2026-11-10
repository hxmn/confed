# Fuzz seeds

Starting inputs for the fuzz targets, so they begin from realistic input and
not from empty strings.

- `markdown/` — page files written for the purpose: the Markdown features
  confed converts, and the input behind every converter bug found so far
  (angle brackets in prose, lines of block tags, nested `<details>`, heading
  escapes, delimiter runs, line breaks). All text is invented.
- The `storage` target is seeded with the converter's fixture corpus,
  `../fixtures/*.xml`, which `make fuzz` and the fuzz workflow pass to it.

cargo-fuzz writes what it discovers to `../corpus/`, which is not committed.
