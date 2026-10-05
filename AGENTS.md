# Working on confed

Rules for anyone — human or agent — changing this repository.

## Never commit personal, company or project information

This repository is public. confed is developed and tested against real Confluence
instances, and bug reports, logs and captures from them carry real data. None of it
may reach the repository — not in code, tests, fixtures, snapshots, docs,
`CHANGELOG.md`, commit messages, or branch and tag names.

Never commit, from any real instance:

- **People:** names, usernames, user keys, account ids, emails, avatars or profile
  URLs.
- **Organisations and servers:** company or product names, hostnames, URLs, IP
  addresses, internal service names, server build numbers or paths that identify an
  installation (a public product version such as "Data Center 9.5" is fine).
- **Content:** space keys, page titles, folder or file names, page and comment text,
  table contents, labels.
- **Identifiers:** page, comment, attachment and version ids, inline-comment marker
  refs, request ids.
- **Secrets:** tokens, passwords, cookies, session ids, XSRF tokens — even expired ones.

Use stand-ins instead:

| Kind | Use |
|---|---|
| Hosts | `wiki.example.test`, `wiki.corp`, `example.atlassian.net`, `*.example.test` |
| People | Alice Ng, Bob Lee, Carol, Dana — invented, never a colleague's name |
| Emails | `…@example.com` |
| Spaces | `DOCS`, `ENG` |
| Ids | small numbers (`1001`), or `9000000xxxxx` when the length matters |
| Titles and text | generic words — "Team Handbook", "Onboarding", "Фраза 1." |

### Turning a report from a real instance into a test

Reproduce the **shape** of the problem, not its content. If the bug depends on
Cyrillic, a non-breaking space, a long title or a raw table, keep that property with
neutral words; do not paste the user's page. A test name or comment says what is being
tested, never whose page it came from.

### Captures and logs

HAR files, DevTools copies and `--log` output are never committed as they are. To keep
a capture as a fixture, keep only the request and response bodies a test needs, delete
every header and cookie, and replace every value from the list above. Put a README
beside it saying what it is and that it was sanitized.

### Before every commit

Read the staged diff (`git diff --cached`) for anything that came from a real
instance: hostnames, names, ids, titles, non-placeholder emails. When in doubt, replace
it.

### If something slips through

Fixing it in a new commit is not enough: the old commit still carries it. Stop, tell
the maintainer, and rewrite history (`git filter-repo --replace-text`) before anything
else is pushed.

## Releasing

Every change that reaches the binary is a release:

1. Bump `[workspace.package] version` in `Cargo.toml` and the `confed-*` path
   dependency versions beside it (semver; before 1.0 a breaking change bumps the minor).
2. Add a dated `## [x.y.z] - YYYY-MM-DD` section to `CHANGELOG.md`, and its link at the
   bottom.
3. `make changelog` — the `confed-cli` crate compiles in its own copy,
   `crates/confed-cli/CHANGELOG.md`, because a published crate can only contain files from
   its own directory. A test fails if the copy is stale.
4. `make ci`, commit, tag `vX.Y.Z`, push the commit and the tag.

### crates.io

Eight crates are published, each after the crates it depends on: `confed-api`,
`confed-converter`, `confed-dc`, `confed-cloud`, `confed-core`, `confed-cli`,
`confed-tui`, `confed`. `cargo publish --workspace` works out that order itself. `cargo publish --workspace --dry-run` builds each from its
own package exactly as crates.io will; run it before a release that changes packaging.

- **First release (by hand):** `cargo login` with a crates.io API token (scopes
  `publish-new`, `publish-update`), then `cargo publish --workspace` from the tagged
  commit.
- **After that (automatic):** for each of the eight crates, add a trusted publisher on
  crates.io (crate → Settings → Trusted Publishing: repository `hxmn/confed`, workflow
  `release.yml`), then set the repository variable `PUBLISH_CRATES` to `true`. From
  then on, pushing a `vX.Y.Z` tag publishes through `.github/workflows/release.yml`,
  with no stored token.

A published version can never be changed or deleted — only yanked.
