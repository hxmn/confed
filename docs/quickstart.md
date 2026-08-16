# Quickstart

Five minutes from nothing to an edited, pushed page. Pick the track that matches your
Confluence: [Cloud](#track-a--confluence-cloud) or
[Data Center / Server](#track-b--confluence-data-center-or-server). Everything after the
tracks is identical.

## Install from source

confed needs a Rust toolchain of 1.85 or newer. It has no system dependencies: TLS is
`rustls`, SQLite is bundled, and the keyring backend is pure Rust, so there are no
headers to install first.

```bash
git clone https://github.com/hxmn/confed
cd confed
cargo install --path crates/confed
confed --version
```

`cargo install` puts the binary in `~/.cargo/bin`, which is on your `PATH` if you
installed Rust with `rustup`. To build without installing, `cargo build --release` leaves
it at `target/release/confed`.

Optional, but pleasant: `confed completion zsh > ~/.zfunc/_confed` (also `bash`, `fish`,
`powershell`, `elvish`).

## Track A — Confluence Cloud

Create an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>, then
clone a space. The base URL includes the `/wiki` context path.

```bash
export CONFED_TOKEN='<your API token>'

confed clone https://acme.atlassian.net/wiki/spaces/DOCS --user you@example.com
cd DOCS
```

`clone` takes the base URL and the space key straight out of a space URL, so you can copy
one out of your browser. If you would rather be explicit:

```bash
confed clone DOCS ./docs \
  --base-url https://acme.atlassian.net/wiki \
  --user you@example.com
```

The e-mail address is not optional on Cloud: the API token is used as an HTTP Basic
password, and the username half is your Atlassian account e-mail. If you leave `--user`
off in a terminal, confed asks for it; in a script it exits 2 and tells you which flag
and environment variable to set.

## Track B — Confluence Data Center or Server

Create a Personal Access Token from your profile menu → **Personal Access Tokens** →
**Create token**. The base URL is the Confluence *context root*, which on many installs
is not the host root — `https://wiki.corp.example.com/confluence` is typical.

```bash
export CONFED_TOKEN='<your personal access token>'

confed clone https://wiki.corp.example.com/confluence/display/DOCS
cd DOCS
```

A PAT authenticates on its own, so no `--user` is needed. If your instance predates PATs
and you must use a username and password, pass `--user <username>` and put the password in
`CONFED_TOKEN`; confed then uses HTTP Basic instead of a bearer token.

confed detects Cloud from the hostname (`*.atlassian.net`) and otherwise probes the API
roots. Behind a proxy that answers everything, or on a host name that lies, skip the probe
with `--flavor dc` (or `--flavor cloud`).

## The everyday loop

`confed clone` is `confed init` followed by `confed pull`. If the directory already
exists, do those two steps yourself:

```bash
confed init --base-url https://wiki.corp.example.com/confluence --space DOCS
confed pull
```

Either way you now have a tree of Markdown files. Edit one with anything:

```bash
$EDITOR "Team Handbook/Onboarding.md"
```

Ask what changed. `status` is offline — it compares the file against the base snapshot
recorded at the last sync, and against the remote snapshot recorded at the last fetch.

```bash
confed status
```

```text
Space DOCS
Last fetch 2026-08-16T09:12:44Z

Modified locally
  Team Handbook/Onboarding.md

1 page to push, 0 pages to pull
```

Look at the change, then at exactly what would be uploaded:

```bash
confed diff "Team Handbook/Onboarding.md"
confed push --dry-run
```

`--dry-run` makes no request that changes anything on the server. It prints the pages,
the version each one would move from and to, and which aspects change (`body`, `title`,
`labels`, `parent`). When that looks right:

```bash
confed push -m "Add the VPN step"
```

The `-m` message becomes the version comment in Confluence's page history. After a push,
confed rewrites the file's tool-managed frontmatter with the new version, so `status` is
clean again immediately.

## Keeping up with other people

```bash
confed fetch     # refresh confed's view of the server; writes no files
confed status    # see what is behind, diverged, new, or deleted
confed pull      # write the server's changes into your files
```

`pull` fast-forwards pages you have not touched and three-way merges pages you have. If a
merge cannot be resolved automatically it leaves conflict markers in the file and exits 4;
see [sync.md](sync.md#the-conflict-workflow) for the walkthrough.

## Driving it from a script or an agent

Add `--json` to any command. It prints one JSON envelope on stdout, sends every log line
and warning to stderr, and never prompts — a missing value fails immediately with exit 2
instead of waiting for input that will not arrive.

```bash
export CONFED_TOKEN="$CONFLUENCE_TOKEN"

confed status --json | jq -r '.result.pages[] | select(.state != "unchanged") | .path'
confed push --dry-run --json | jq '.result.pushed'
confed push --json
```

Check exit codes rather than parsing prose: 0 is success, 4 means "pull first", 10 is
only ever produced by `--exit-code`. The full table is in
[commands.md](commands.md#exit-codes), and each code has a troubleshooting entry in
[troubleshooting.md](troubleshooting.md).

## Where to go next

- [auth.md](auth.md) — how credentials are stored and what to do in CI.
- [format.md](format.md) — what you may and may not edit in a page file.
- [commands.md](commands.md) — the rest of the commands: `new`, `mv`, `rm`, `attach`,
  `comment`, `log`, `search`, `export`, `doctor`.
