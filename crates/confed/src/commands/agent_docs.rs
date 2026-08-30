//! Generates `CLAUDE.md` and `AGENTS.md` — the contract that tells a coding
//! agent what this directory is and which parts of it are safe to edit.

use confed_api::Flavor;
use confed_core::error::{ConfedError, Result};
use std::path::Path;

pub const FILENAMES: &[&str] = &["CLAUDE.md", "AGENTS.md"];

/// The confed that generates these files, stamped into them so a later run can
/// tell whether the contract on disk still describes the binary in the path.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const STAMP_PREFIX: &str = "<!-- confed:agent-docs version=";

/// Write both agent contract files. Returns the filenames written.
pub fn write(dir: &Path, base_url: &str, flavor: Flavor, space_key: &str) -> Result<Vec<String>> {
    let content = render(base_url, flavor, space_key);
    let mut written = Vec::new();
    for name in FILENAMES {
        let path = dir.join(name);
        std::fs::write(&path, &content)
            .map_err(|e| ConfedError::io(format!("writing {}", path.display()), e))?;
        written.push((*name).to_string());
    }
    Ok(written)
}

/// The confed version stamped into a generated contract, if it has one. Files
/// from before the stamp existed, or hand-written ones, report `None`.
pub fn generated_version(content: &str) -> Option<&str> {
    let stamp = content.lines().next()?.trim().strip_prefix(STAMP_PREFIX)?;
    Some(stamp.strip_suffix("-->")?.trim())
}

/// What is wrong with the contract files in `dir`: the ones that are missing,
/// and the ones an older or newer confed wrote.
pub fn audit(dir: &Path) -> (Vec<&'static str>, Vec<(&'static str, String)>) {
    let mut missing = Vec::new();
    let mut stale = Vec::new();

    for name in FILENAMES {
        match std::fs::read_to_string(dir.join(name)) {
            Err(_) => missing.push(*name),
            Ok(content) => {
                let stamped = generated_version(&content).unwrap_or("an earlier confed");
                if stamped != VERSION {
                    stale.push((*name, stamped.to_string()));
                }
            }
        }
    }
    (missing, stale)
}

pub fn render(base_url: &str, flavor: Flavor, space_key: &str) -> String {
    let flavor_note = match flavor {
        Flavor::Cloud => "Confluence Cloud (REST v2). Inline comments can be created and resolved.",
        Flavor::DataCenter => {
            "Confluence Data Center (REST v1). Creating and resolving inline comments is \
             not supported by the server API — those commands exit with code 9."
        }
    };

    format!(
        r#"{STAMP_PREFIX}{VERSION} -->
# confed workspace

This directory mirrors the Confluence space **{space_key}** at {base_url} as Markdown
files. It is managed by `confed`, an offline-first Confluence editor with a git-like
command model. Server: {flavor_note}

These rules describe **confed {VERSION}**. Before relying on them, check that the
binary in your path is still that one — see [When confed is upgraded](#when-confed-is-upgraded).

## Layout

```
Team Handbook.md          a page
Team Handbook/            its child pages
Team Handbook/.Onboarding/  sidecar for "Onboarding.md": storage.xml, comments.md, attachments
.state.db                 sync state (SQLite) — never edit or delete
.session.db               credentials (mode 0600) — never read, never commit
```

A page's children live in a directory named after it; its attachments and comments
live in a hidden sidecar directory `.<page name>/` next to the file.

## Frontmatter contract

```yaml
---
title: "Onboarding"        # YOU MAY EDIT — renames the page on push
labels: [hr, onboarding]   # YOU MAY EDIT — synced on push
parent_id: "163841"        # YOU MAY EDIT — moves the page on push
confed:                    # TOOL-MANAGED — DO NOT EDIT ANY OF THIS
  schema: 1
  page_id: "163842"        # the stable identity of the page
  version: 7               # base version for optimistic locking
  ...
---
```

- **Never edit anything under `confed:`.** `push` detects hand edits and refuses the
  page (exit 7). If a block is broken, repair it with `confed pull --force <page>`.
- **`page_id` is the identity, not the filename.** Renaming a file is safe; confed
  re-associates by `page_id`.
- A new file with only `title:` in its frontmatter (no `confed:` block) is created on
  the server by the next `push`. `confed new <path>` scaffolds one correctly.
- Unknown top-level frontmatter keys are preserved, so team metadata is safe to add.

## Body rules

- Write normal Markdown (CommonMark + GFM tables, task lists, GitHub alerts).
- Fenced ```` ```confluence ```` blocks contain raw Confluence storage XML for macros
  confed does not model, indented for reading. One you do not touch is re-uploaded
  byte-for-byte; one you edit is uploaded as the fence reads. Edit them only if you know
  the storage format; deleting the whole block deletes the macro. Never touch
  `ac:macro-id`.
- Attachments are referenced relative to the sidecar: `![alt](.Onboarding/diagram.png)`.
- `.<page>/storage.xml` is the body as Confluence stores it, indented, refreshed on every
  sync. Read it to see what a conversion produced; editing it does nothing, because pushes
  are built from the Markdown.
- Only blocks you actually change are regenerated on push, so untouched content is
  never reformatted.

## Commands (every one supports `--json`)

```bash
confed status --json                  # what changed, locally and on the server
confed diff "Handbook/**"             # base vs local
confed diff --remote --exit-code      # exit 10 if the server has moved
confed fetch --json                   # refresh remote state, touch no files
confed pull --json                    # write remote changes into files (merges)
confed push --dry-run --json          # exactly what would be uploaded
confed push -m "reason" --json        # upload
confed comment list <page> --json     # read discussion
confed log <page> --json              # server version history
confed version --json                 # this build, its schemas, its changelog
```

`--json` implies `--non-interactive`: confed never prompts and never hangs. Missing
values fail immediately with exit code 2 and a message naming the flag and the
`CONFED_*` environment variable.

## When confed is upgraded

This file was generated by confed {VERSION} and is not updated automatically. The
binary can be upgraded under you — by a package manager, a container image, or a
teammate — and a newer one may add commands or change behaviour this file does not
describe. So before a session that will edit or push anything:

```bash
confed version --json | jq -r '.result.version'   # what you are actually running
```

If that is not `{VERSION}`, read what changed and refresh this contract:

```bash
confed version --changelog --since {VERSION}      # every release after this file
confed doctor --fix                               # rewrite CLAUDE.md and AGENTS.md
```

`confed doctor` reports the same drift as a warning on the "agent docs" check, so
running it first also answers the question. The release notes are compiled into the
binary: reading them needs no network access and no repository checkout.

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | success | — |
| 1 | internal error | report it |
| 2 | usage / missing value | pass the flag or set the env var named in the error |
| 3 | auth failure | credentials are wrong or expired |
| 4 | conflict | run `confed pull`, resolve, then push |
| 5 | network | retry later |
| 6 | not found | check the page path or id |
| 7 | local state problem | dirty files, tampered frontmatter, lock held |
| 8 | partial success | inspect `result` and `errors` in the JSON |
| 9 | unsupported on this server | a Cloud-only feature on Data Center |
| 10 | differences exist | only from `--exit-code` |

## Conflict workflow

1. `confed status --json` — look for `"state": "diverged"` or `"conflicted"`.
2. `confed pull` — confed does a three-way merge (base, yours, theirs).
3. Clean merge: done. Otherwise the file contains conflict markers:
   `<<<<<<< local` / `||||||| base` / `>>>>>>> remote (v9, …)`.
4. Edit the file so no markers remain, then `confed resolve <page>`.
5. `confed push`.

Never push while a page is conflicted — confed will refuse it anyway.

## Comments

Comments live in `.<page>/comments.md`. To add one, append:

```markdown
<!-- confed:new -->
Your comment text.
```

To reply, add `reply-to=<comment-id>` to that marker. To resolve (Cloud only):

```markdown
<!-- confed:resolve id=98211 -->
```

Then `confed push`. Do not edit the body of an existing comment — confed ignores it.

### Inline comments in the page body

An open inline thread is shown where it sits, as a pair of HTML comments:

```markdown
Complete your <!--c 77120 Alice Ng: Link the template?-->first week checklist<!--/c 77120--> today.
```

- The span between the markers is the commented text; the opener shows who
  said what. Read it, fix the text if that is what is being asked, then reply
  and resolve in the sidecar (`reply-to=77120`, `confed:resolve id=77120`).
- Marks are a layer, not content: `status` and `diff` ignore them, and deleting
  one changes nothing — the next pull puts it back. Never "resolve" a thread
  by removing its mark.
- To comment on some text yourself, wrap it (one line, no `--` inside):

  ```markdown
  The <!--c new Is this still the right team?-->platform team<!--/c new--> owns it.
  ```

  `confed push` creates the comment and rewrites the mark with its id. Or use
  `confed comment add <page> --anchor "platform team" -m "…"`. Cloud only.

## Do / don't

- **Do** check that `confed version` is still {VERSION}, the version this file
  describes, before you trust anything above.
- **Do** run `confed status` before and after editing.
- **Do** run `confed push --dry-run` before pushing; it shows the exact diff.
- **Do** treat exit code 4 as "pull first", never as "retry harder".
- **Don't** edit `.state.db` or `.session.db`, or read credentials out of them.
- **Don't** edit the `confed:` frontmatter block, especially `page_id` or `version`.
- **Don't** use `--force` or `--allow-delete` unless a human asked for it: they
  discard local work and delete server pages respectively.
- **Don't** commit `.state.db` or `.session.db` (already in `.gitignore`).
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_agent_files_are_written_with_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let written =
            write(dir.path(), "https://x.atlassian.net/wiki", Flavor::Cloud, "DOCS").unwrap();
        assert_eq!(written, ["CLAUDE.md", "AGENTS.md"]);

        let claude = std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap();
        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert_eq!(claude, agents);
        assert!(claude.contains("DOCS"));
    }

    #[test]
    fn the_contract_states_the_rules_that_matter() {
        let doc = render("https://wiki.corp", Flavor::DataCenter, "DOCS");
        for required in [
            "Never edit anything under `confed:`",
            "page_id",
            "--json",
            "exit code 2",
            "confed resolve",
            "confed:new",
            ".state.db",
        ] {
            assert!(doc.contains(required), "agent contract is missing: {required}");
        }
    }

    #[test]
    fn the_data_center_contract_names_its_capability_gap() {
        let dc = render("https://wiki.corp", Flavor::DataCenter, "DOCS");
        assert!(dc.contains("not supported by the server API"));

        let cloud = render("https://x.atlassian.net/wiki", Flavor::Cloud, "DOCS");
        assert!(cloud.contains("can be created and resolved"));
    }

    #[test]
    fn the_contract_says_which_confed_it_describes_and_how_to_recheck() {
        let doc = render("https://wiki.corp", Flavor::Cloud, "DOCS");
        assert_eq!(generated_version(&doc), Some(VERSION));
        for required in [
            "When confed is upgraded",
            "confed version --json",
            &format!("confed version --changelog --since {VERSION}"),
            "confed doctor --fix",
        ] {
            assert!(doc.contains(required), "the upgrade instructions are missing: {required}");
        }
    }

    #[test]
    fn an_unstamped_or_differently_stamped_file_is_reported_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(audit(dir.path()), (vec!["CLAUDE.md", "AGENTS.md"], vec![]));

        write(dir.path(), "https://x.atlassian.net/wiki", Flavor::Cloud, "DOCS").unwrap();
        assert_eq!(audit(dir.path()), (vec![], vec![]), "freshly written docs are current");

        std::fs::write(dir.path().join("AGENTS.md"), "<!-- confed:agent-docs version=0.0.1 -->\n")
            .unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# hand-written\n").unwrap();
        let (missing, stale) = audit(dir.path());
        assert!(missing.is_empty());
        assert_eq!(
            stale,
            vec![
                ("CLAUDE.md", "an earlier confed".to_string()),
                ("AGENTS.md", "0.0.1".to_string()),
            ]
        );
    }

    #[test]
    fn every_documented_exit_code_appears() {
        let doc = render("https://wiki.corp", Flavor::Cloud, "DOCS");
        for code in 0..=10 {
            assert!(doc.contains(&format!("| {code} |")), "exit code {code} is undocumented");
        }
    }
}
