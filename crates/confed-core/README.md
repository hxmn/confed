# confed-core

The sync engine of [confed](https://github.com/hxmn/confed) — the Confluence editor
that keeps a space as Markdown files: workspace state in SQLite, page files and their
frontmatter, three-way merge, inline comment marks, and `fetch`, `pull` and `push`
against Confluence Cloud and Data Center.

This crate is developed for confed and its API follows confed's needs; it is published
so `confed` can be installed from crates.io. Licensed under MIT or Apache-2.0.
