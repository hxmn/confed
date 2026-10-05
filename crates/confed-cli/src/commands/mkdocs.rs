//! `confed mkdocs` — scaffold an MkDocs site over the pulled Markdown.
//!
//! The workspace is already a documentation tree, so nothing is copied: `docs/`
//! is a directory of symlinks pointing back at the pages confed syncs, which
//! MkDocs follows. Editing a page or running `confed pull` updates the site with
//! no intermediate build step.
//!
//! The symlinks exist because MkDocs refuses a `docs_dir` that is not a child of
//! its config file, so pointing it straight at the workspace is not allowed.
//!
//! Two more details make the result read properly. Confluence's hierarchy puts a
//! page's children in a directory beside it (`Handbook.md` next to `Handbook/`),
//! which MkDocs would otherwise show as two unrelated entries, so the navigation
//! is generated from confed's own page tree with each parent as its section's
//! index. And attachments live in dot-directories, which MkDocs excludes by
//! default; since that default is a gitignore-style pattern, the generated
//! config negates it rather than reaching for a plugin.

use crate::cli::MkdocsArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::state::PageRecord;
use confed_core::sync::write_atomic;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

pub fn run(ctx: &mut Context, args: &MkdocsArgs) -> Result<Output> {
    let ws = ctx.workspace()?;
    let root = ws.root().to_path_buf();
    let space = ws.space_key().unwrap_or_else(|_| "Documentation".to_string());
    let site_name = args
        .site_name
        .clone()
        .or_else(|| ws.state().get_meta("space_name").ok().flatten())
        .unwrap_or_else(|| space.clone());
    let base_url = ws.base_url()?.unwrap_or_default();

    let pages = ws.state().all_pages()?;
    let nav = build_nav(&pages);

    let linked = link_docs_dir(&root)?;

    let files = [
        ("mkdocs.yml", mkdocs_yml(&site_name, &base_url, &nav)),
        ("pyproject.toml", pyproject_toml(&space)),
        ("Makefile", makefile()),
    ];

    let mut written = Vec::new();
    let mut skipped = Vec::new();
    for (name, content) in files {
        let path = root.join(name);
        if path.exists() && !args.force {
            skipped.push(name.to_string());
            continue;
        }
        write_atomic(&path, &content)?;
        written.push(name.to_string());
    }

    let ignored = ensure_gitignore(&root)?;

    let style = &ctx.style;
    let mut human = String::new();
    if written.is_empty() {
        let _ = writeln!(human, "Everything is already generated. Use --force to overwrite.");
    } else {
        let _ = writeln!(human, "Generated {}", written.join(", "));
    }
    if !skipped.is_empty() {
        let _ = writeln!(
            human,
            "{}",
            style.dim(&format!("Left alone (already present): {})", skipped.join(", ")))
        );
    }
    let _ = writeln!(human, "\n{}", style.bold("  make serve"));
    let _ = writeln!(
        human,
        "{}",
        style.dim(
            "  installs MkDocs with uv on first run and serves the space at\n  \
                   http://127.0.0.1:8000; `make build` writes a static site to site/."
        )
    );
    let _ = writeln!(
        human,
        "{}",
        style.dim("\nRe-run `confed mkdocs --force` after adding or moving pages to refresh\nthe navigation.")
    );

    Ok(Output::new(
        json!({
            "site_name": site_name,
            "generated": written,
            "skipped": skipped,
            "gitignore_added": ignored,
            "pages_in_nav": pages.len(),
            "linked": linked,
        }),
        human,
    ))
}

/// One entry in the generated navigation.
struct NavEntry {
    title: String,
    path: String,
    children: Vec<NavEntry>,
}

/// Build the navigation from confed's page tree.
///
/// Ordering follows the server: position first, then title, so the site reads
/// the way the space does.
fn build_nav(pages: &[PageRecord]) -> Vec<NavEntry> {
    let mut children: HashMap<Option<String>, Vec<&PageRecord>> = HashMap::new();
    for page in pages {
        children.entry(page.parent_id.clone()).or_default().push(page);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));
    }

    // A page whose parent is not in the workspace is shown at the top level,
    // rather than vanishing from the navigation.
    let known: Vec<&str> = pages.iter().map(|p| p.page_id.as_str()).collect();
    let mut roots: Vec<&PageRecord> = children
        .iter()
        .filter(|(parent, _)| parent.as_ref().is_none_or(|id| !known.contains(&id.as_str())))
        .flat_map(|(_, siblings)| siblings.iter().copied())
        .collect();
    roots.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));

    let mut nav: Vec<NavEntry> = roots.iter().map(|page| entry_for(page, &children, 0)).collect();

    // A cycle in the hierarchy leaves pages with no root to hang from. Showing
    // them at the top level is better than dropping them from the site.
    let mut reachable = Vec::new();
    collect_paths(&nav, &mut reachable);
    for page in pages {
        if !reachable.contains(&page.local_path) {
            nav.push(NavEntry {
                title: page.title.clone(),
                path: page.local_path.clone(),
                children: Vec::new(),
            });
        }
    }
    nav
}

fn entry_for(
    page: &PageRecord,
    children: &HashMap<Option<String>, Vec<&PageRecord>>,
    depth: usize,
) -> NavEntry {
    // A cycle in the hierarchy would otherwise recurse forever.
    let kids = if depth > 32 {
        Vec::new()
    } else {
        children
            .get(&Some(page.page_id.clone()))
            .map(|siblings| {
                siblings.iter().map(|child| entry_for(child, children, depth + 1)).collect()
            })
            .unwrap_or_default()
    };
    NavEntry { title: page.title.clone(), path: page.local_path.clone(), children: kids }
}

fn collect_paths(entries: &[NavEntry], out: &mut Vec<String>) {
    for entry in entries {
        out.push(entry.path.clone());
        collect_paths(&entry.children, out);
    }
}

fn render_nav(entries: &[NavEntry], indent: usize) -> String {
    let pad = "  ".repeat(indent + 1);
    let mut out = String::new();
    for entry in entries {
        if entry.children.is_empty() {
            let _ =
                writeln!(out, "{pad}- {}: {}", yaml_scalar(&entry.title), yaml_scalar(&entry.path));
        } else {
            // The parent page becomes the section's own landing page, which is
            // what `navigation.indexes` renders.
            let _ = writeln!(out, "{pad}- {}:", yaml_scalar(&entry.title));
            let _ = writeln!(out, "{pad}  - {}", yaml_scalar(&entry.path));
            out.push_str(&render_nav(&entry.children, indent + 1));
        }
    }
    out
}

/// Quote a value so YAML reads it as a string whatever it contains.
fn yaml_scalar(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn mkdocs_yml(site_name: &str, base_url: &str, nav: &[NavEntry]) -> String {
    let nav_yaml = if nav.is_empty() {
        // With nothing pulled yet, let MkDocs infer the tree.
        "# Nothing has been pulled yet; re-run `confed mkdocs --force` after a pull\n\
         # to generate the navigation from the page hierarchy.\n"
            .to_string()
    } else {
        format!("nav:\n{}", render_nav(nav, 0))
    };

    format!(
        r#"# Generated by `confed mkdocs`. Re-run it with --force to refresh the
# navigation after pages are added, moved or renamed.
site_name: {name}
{repo}
# `docs/` is a directory of symlinks back to the pages confed syncs, so MkDocs
# reads the real files: editing a page or running `confed pull` updates the site
# with nothing to rebuild. (MkDocs will not accept a docs_dir outside its own
# directory, which is why the symlinks exist.)
docs_dir: docs
site_dir: site

theme:
  name: material
  features:
    # A page whose children live beside it becomes that section's landing page,
    # which is how Confluence's hierarchy is shaped.
    - navigation.indexes
    - navigation.top
    - content.code.copy
    - search.highlight
  palette:
    - media: "(prefers-color-scheme: light)"
      scheme: default
      toggle: {{icon: material/brightness-7, name: Switch to dark mode}}
    - media: "(prefers-color-scheme: dark)"
      scheme: slate
      toggle: {{icon: material/brightness-4, name: Switch to light mode}}

markdown_extensions:
  - tables
  - attr_list
  - md_in_html
  - def_list
  - footnotes
  - admonition          # the GitHub alerts confed renders
  - pymdownx.details    # the <details> blocks confed renders
  - pymdownx.superfences
  - pymdownx.highlight
  - toc:
      permalink: true

plugins:
  - search

# MkDocs excludes dot-paths by default, which would drop every attachment: they
# live in a hidden directory beside each page. The default is a gitignore-style
# pattern, so it can simply be negated — later patterns win, which is how the
# sidecar files confed keeps for itself are kept out again.
exclude_docs: |
  CLAUDE.md
  AGENTS.md
  !.*/
  !.*/**
  .*/storage.xml
  .*/comments.md

{nav}"#,
        name = yaml_scalar(site_name),
        repo = if base_url.is_empty() {
            String::new()
        } else {
            format!("site_url: {}\n", yaml_scalar(base_url))
        },
        nav = nav_yaml,
    )
}

fn pyproject_toml(space: &str) -> String {
    format!(
        r#"# Generated by `confed mkdocs`. Dependencies are managed with uv:
#   uv sync      install them into .venv
#   uv run ...   run a command against them
[project]
name = "{name}-docs"
version = "0.0.0"
description = "MkDocs site for the {space} space, synced by confed"
requires-python = ">=3.9"
dependencies = [
    "mkdocs>=1.6",
    "mkdocs-material>=9.5",
]
"#,
        name = space.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
        space = space,
    )
}

fn makefile() -> String {
    r#"# Generated by `confed mkdocs`. Viewing this space as a site.
#
# uv installs MkDocs into .venv on demand, so there is nothing to set up first.

UV ?= uv
PORT ?= 8000

.DEFAULT_GOAL := help

## serve: preview the space at http://127.0.0.1:$(PORT), reloading on change
serve:
	$(UV) run mkdocs serve --dev-addr 127.0.0.1:$(PORT)

## build: write a static site to site/
build:
	$(UV) run mkdocs build --strict

## sync: pull the latest pages from Confluence, then refresh the navigation
sync:
	confed pull
	confed mkdocs --force

## nav: refresh the navigation after pages were added, moved or renamed
nav:
	confed mkdocs --force

## install: create .venv and install MkDocs without serving
install:
	$(UV) sync

## clean: remove the built site
clean:
	rm -rf site

## help: list the available targets
help:
	@echo "confed mkdocs — view this space as a site"
	@echo
	@sed -n 's/^## \([a-z-]*\): \(.*\)/  \1\t\2/p' $(MAKEFILE_LIST) | expand -t 12
	@echo
	@echo "Variables: PORT=$(PORT)  UV=$(UV)"

.PHONY: serve build sync nav install clean help
"#
    .to_string()
}

/// Build `docs/` as symlinks pointing back at the workspace.
///
/// Only top-level entries are linked: a page directory covers everything beneath
/// it, so new child pages appear on their own and only a new *top-level* page
/// needs `confed mkdocs --force` again.
fn link_docs_dir(root: &Path) -> Result<Vec<String>> {
    let docs = root.join("docs");
    // Rebuilt from scratch, so pages deleted since last time do not linger.
    if docs.exists() {
        std::fs::remove_dir_all(&docs)
            .map_err(|e| ConfedError::io(format!("clearing {}", docs.display()), e))?;
    }
    std::fs::create_dir_all(&docs)
        .map_err(|e| ConfedError::io(format!("creating {}", docs.display()), e))?;

    let entries = std::fs::read_dir(root)
        .map_err(|e| ConfedError::io(format!("reading {}", root.display()), e))?;

    let mut linked = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_page_content(&name, &entry.path()) {
            continue;
        }
        let target = Path::new("..").join(&name);
        symlink(&target, &docs.join(&name), entry.path().is_dir()).map_err(|e| {
            ConfedError::io(
                format!(
                    "linking {name} into docs/ (on Windows this needs Developer Mode                      or an elevated shell)"
                ),
                e,
            )
        })?;
        linked.push(name);
    }
    linked.sort();
    Ok(linked)
}

/// Everything that belongs in the site: pages, their child directories, and
/// their hidden sidecars, which carry attachments.
fn is_page_content(name: &str, path: &Path) -> bool {
    const NEVER: &[&str] = &[
        "docs",
        "site",
        ".venv",
        ".git",
        "CLAUDE.md",
        "AGENTS.md",
        "mkdocs.yml",
        "pyproject.toml",
        "Makefile",
        "uv.lock",
        ".gitignore",
    ];
    if NEVER.contains(&name) || name.ends_with(".db") || name.ends_with(".lock") {
        return false;
    }
    path.is_dir() || name.ends_with(".md")
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path, _is_dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path, is_dir: bool) -> std::io::Result<()> {
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Keep the generated build output and virtualenv out of git.
fn ensure_gitignore(root: &Path) -> Result<Vec<String>> {
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let wanted = ["site/", ".venv/", "uv.lock", "docs/"];

    let missing: Vec<String> = wanted
        .iter()
        .filter(|entry| !existing.lines().any(|line| line.trim() == **entry))
        .map(|entry| (*entry).to_string())
        .collect();
    if missing.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n# confed mkdocs: generated site and its virtualenv\n");
    for entry in &missing {
        out.push_str(entry);
        out.push('\n');
    }
    std::fs::write(&path, out)
        .map_err(|e| ConfedError::io(format!("writing {}", path.display()), e))?;
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use confed_core::state::{now, SyncState};

    fn page(id: &str, title: &str, path: &str, parent: Option<&str>, position: i64) -> PageRecord {
        PageRecord {
            page_id: id.into(),
            title: title.into(),
            slug: title.into(),
            local_path: path.into(),
            parent_id: parent.map(str::to_string),
            position: Some(position),
            version: 1,
            status: "current".into(),
            labels: vec![],
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: String::new(),
            storage_hash: String::new(),
            markdown_hash: String::new(),
            block_map: None,
            sync_state: SyncState::Clean,
            synced_at: now(),
            render_key: String::new(),
        }
    }

    #[test]
    fn a_parent_page_becomes_its_own_section_landing_page() {
        let pages = vec![
            page("1", "Team Handbook", "Team Handbook.md", None, 1),
            page("2", "Onboarding", "Team Handbook/Onboarding.md", Some("1"), 1),
        ];
        let nav = render_nav(&build_nav(&pages), 0);

        assert_eq!(
            nav,
            "  - \"Team Handbook\":\n    - \"Team Handbook.md\"\n    \
             - \"Onboarding\": \"Team Handbook/Onboarding.md\"\n"
        );
    }

    #[test]
    fn navigation_follows_the_order_the_space_uses() {
        let pages =
            vec![page("1", "Zebra", "Zebra.md", None, 1), page("2", "Alpha", "Alpha.md", None, 2)];
        let nav = render_nav(&build_nav(&pages), 0);
        assert!(
            nav.find("Zebra").unwrap() < nav.find("Alpha").unwrap(),
            "position wins over alphabetical order: {nav}"
        );
    }

    #[test]
    fn a_page_whose_parent_is_missing_still_appears() {
        let pages = vec![page("2", "Orphan", "Orphan.md", Some("gone"), 1)];
        let nav = render_nav(&build_nav(&pages), 0);
        assert!(nav.contains("Orphan"), "got {nav}");
    }

    #[test]
    fn a_hierarchy_cycle_does_not_hang() {
        let pages =
            vec![page("1", "A", "A.md", Some("2"), 1), page("2", "B", "B.md", Some("1"), 1)];
        let nav = render_nav(&build_nav(&pages), 0);
        assert!(!nav.is_empty(), "both pages still reachable");
    }

    #[test]
    fn titles_and_paths_are_quoted_so_yaml_cannot_misread_them() {
        let pages = vec![page("1", "Q3: Plan \"final\"", "Q3- Plan.md", None, 1)];
        let nav = render_nav(&build_nav(&pages), 0);
        assert!(nav.contains(r#""Q3: Plan \"final\"""#), "got {nav}");
    }

    #[test]
    fn the_config_names_the_things_the_workspace_needs() {
        let yml = mkdocs_yml("Docs", "https://wiki.corp", &[]);
        assert!(yml.contains("docs_dir: docs"), "MkDocs needs a child directory");
        assert!(yml.contains("navigation.indexes"), "parents are section landing pages");
        assert!(yml.contains("!.*/**"), "attachments must be un-excluded");
        assert!(yml.contains(".*/storage.xml"), "but confed's own sidecar files stay out");
        assert!(yml.contains("admonition"), "confed renders GitHub alerts");
        assert!(yml.contains("pymdownx.details"), "confed renders <details>");
        assert!(yml.contains("CLAUDE.md"), "agent docs are not pages");
    }

    #[test]
    fn the_makefile_and_project_drive_everything_through_uv() {
        let makefile = makefile();
        assert!(makefile.contains("$(UV) run mkdocs serve"));
        assert!(makefile.contains("$(UV) sync"));

        let project = pyproject_toml("DOCS");
        assert!(project.contains("mkdocs-material"));
        assert!(project.contains("name = \"docs-docs\""), "got {project}");
    }

    #[test]
    fn the_gitignore_gains_the_build_output_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), ".state.db\n").unwrap();

        let added = ensure_gitignore(dir.path()).unwrap();
        assert_eq!(added, ["site/", ".venv/", "uv.lock", "docs/"]);

        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".state.db"), "existing entries survive");
        assert!(ensure_gitignore(dir.path()).unwrap().is_empty(), "running again adds nothing");
    }
}
