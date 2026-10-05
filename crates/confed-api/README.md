# confed-api

Confluence Cloud (REST v2) and Data Center (REST v1) clients behind one trait,
`ConfluenceClient`, used by [confed](https://github.com/hxmn/confed) — the
Confluence editor that keeps a space as Markdown files.

It covers pages, labels, attachments, page and inline comments (on Data Center through
the inline-comment API its page view uses), CQL search, users and version history,
with retries, rate limiting and a stateful in-memory mock for tests.

This crate is developed for confed and its API follows confed's needs; it is published
so `confed` can be installed from crates.io. Licensed under MIT or Apache-2.0.
