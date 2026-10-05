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
