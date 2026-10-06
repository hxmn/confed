# confed

`confed` mirrors a Confluence space into a directory of Markdown files, lets you edit
them with whatever tools you already use, and moves the changes back to the server. It
works offline, it never guesses when two people have edited the same page, and every
command speaks JSON so an agent can drive it as easily as a person can.

## The mental model

If you know git, you already know confed. A confed workspace tracks three snapshots of
every page, and every command is a statement about the relationship between them.

| Snapshot | Where it lives | Who writes it |
|---|---|---|
| **base** | the `pages` table in `.state.db` | confed only, and only from a server response |
| **local** | the `.md` file in your working tree | you, your editor, your agent |
| **remote** | the `remote_pages` table in `.state.db` | `confed fetch`, from the server |

`fetch` refreshes *remote* and touches nothing else. `pull` moves *remote* into *local*
and advances *base*. `push` moves *local* to the server and advances *base* to what the
server confirmed. `status` and `diff` just compare the three, which is why they work on
a plane with no network.

The base is the pivot. It is the last state confed and the server agreed on, so
"modified" means local differs from base, "behind" means remote is ahead of base, and
"diverged" means both. That single distinction is what lets `pull` merge instead of
clobber and lets `push` refuse a write that would silently overwrite somebody else.

A page's identity is its Confluence `page_id`, recorded in the file's frontmatter — not
its filename. Rename a file, move it, restructure the tree: confed still knows which page
it is.

## What a workspace looks like

```
Team Handbook.md              a page
Team Handbook/                its child pages
Team Handbook/Onboarding.md   a child page
Team Handbook/.Onboarding/    that child's attachments and comments.md
.state.db                     base + remote snapshots (SQLite)
.session.db                   credentials, mode 0600
.confed.lock                  held while a mutating command runs
CLAUDE.md, AGENTS.md          the generated contract for coding agents
.gitignore                    extended to exclude the three files above
```

## Table of contents

| Document | What it covers |
|---|---|
| [quickstart.md](quickstart.md) | install, clone a space, edit, push — separate Cloud and Data Center tracks |
| [auth.md](auth.md) | Cloud API tokens, Data Center PATs, precedence, keyring vs `.session.db`, CI |
| [commands.md](commands.md) | every command, every flag, examples, and the exit-code table |
| [format.md](format.md) | the frontmatter contract, file layout, slugs, preserved blocks, accepted lossiness |
| [sync.md](sync.md) | the three-snapshot model, every page state, the conflict workflow |
| [troubleshooting.md](troubleshooting.md) | one entry per exit code, plus the situations that produce them |
| [reference/json/](reference/json/) | JSON Schemas for the `--json` envelope and each command's result |

The `CLAUDE.md` / `AGENTS.md` files that `confed init` writes into a workspace are the
contract for coding agents working *inside* that directory: what is safe to edit, what is
tool-managed, and which commands to run. They are generated per workspace (they name the
space and the server), so they are not duplicated here — read the copy in your own
workspace, or `crates/confed-cli/src/commands/agent_docs.rs` for the template. A space
can put rules of its own at the top of both files by naming a Confluence page:
[`confed config --set rules_page_id`](commands.md#rules-for-agents-rules_page_id).

## Design documents

The [`design/`](design/) directory holds the original design notes:
[architecture](design/01-architecture.md), [data design](design/02-data-design.md),
[conversion and conflicts](design/03-conversion-and-conflicts.md),
[command reference](design/04-command-reference.md),
[open questions](design/05-open-questions.md), and
[inline comment marks](design/06-inline-comment-marks.md). They describe intent, which is not always
what shipped. Where the two disagree, the documents in this directory describe the
implementation and say so explicitly.

## Reading the space as a website

`confed mkdocs` generates an MkDocs site over the pulled Markdown — see
[commands.md](commands.md#confed-mkdocs). It links to the pages rather than copying them,
so `confed pull` is all it takes to update the site.
