# Security policy

confed stores Confluence credentials and writes to a wiki in its user's name, so a flaw
in it can expose a token or change pages nobody meant to change. Please report one
privately, not in a public issue.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: the **Security** tab of this repository,
then **Report a vulnerability**. The report reaches the maintainer only. Include what you
did, what happened, the version (`confed version`), and the Confluence flavor (Cloud or
Data Center) — but no real tokens, hostnames, page content or names from your instance;
a reproduction against a stand-in such as `wiki.example.test` is enough.

You can expect an acknowledgement within a week, and to hear what will be done once the
report is understood. A fix is released as a new version, and the changelog describes it
once that version is out — not before.

## Supported versions

Fixes go into the next release of the latest minor version. confed is before 1.0, so
upgrade to the newest release to receive them; older versions are not patched.

## Scope

In scope: how confed handles credentials (`.session.db`, the OS keyring, `CONFED_TOKEN`),
what it sends where, and anything that makes it write to the server, or to files outside
the workspace, other than as asked. Bugs in Confluence itself belong with Atlassian.
