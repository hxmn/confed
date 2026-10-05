# confed-api

The Confluence client contract of [confed](https://github.com/hxmn/confed) — the
Confluence editor that keeps a space as Markdown files.

`ConfluenceClient` is the trait both clients implement:
[`confed-cloud`](https://crates.io/crates/confed-cloud) over REST v2 and
[`confed-dc`](https://crates.io/crates/confed-dc) over REST v1. This crate holds what
they share: the types, errors and HTTP transport (retries, rate limiting), the REST v1
wire formats, and `MockClient`, a stateful in-memory Confluence for tests.

This crate is developed for confed and its API follows confed's needs; it is published
so `confed` can be installed from crates.io. Licensed under MIT or Apache-2.0.
