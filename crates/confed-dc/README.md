# confed-dc

The Confluence Data Center client of [confed](https://github.com/hxmn/confed): pages,
labels, attachments, page comments, CQL search, users and version history over REST v1,
and inline comments — create, reply, resolve — through the inline-comment API the Data
Center page view itself uses, which Atlassian does not document. Request shapes come
from sanitized captures kept as test fixtures.

This crate is developed for confed and its API follows confed's needs; it is published
so `confed` can be installed from crates.io. Licensed under MIT or Apache-2.0.
