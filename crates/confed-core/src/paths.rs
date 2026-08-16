//! Mapping the page hierarchy onto the filesystem, and links between files.

use crate::slug::{page_path, sidecar_dir_name, slugify, SlugAllocator};
use std::collections::HashMap;

/// The minimum a page needs for confed to decide where its file goes.
#[derive(Clone, Debug)]
pub struct PagePlacement {
    pub page_id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub position: Option<i64>,
}

/// Where a page's files live.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub slug: String,
    /// Workspace-relative path of the Markdown file.
    pub path: String,
    /// Workspace-relative path of the hidden sidecar directory.
    pub sidecar: String,
}

/// Assign a path to every page, honoring the hierarchy and resolving collisions.
///
/// `existing` maps page id → the slug already on disk, so a page that has been
/// pulled before keeps its filename even if a sibling would now sort first.
pub fn plan_paths(
    pages: &[PagePlacement],
    existing: &HashMap<String, String>,
) -> HashMap<String, Placement> {
    let by_id: HashMap<&str, &PagePlacement> =
        pages.iter().map(|p| (p.page_id.as_str(), p)).collect();

    // Group siblings so collisions are resolved per parent, not globally.
    let mut children: HashMap<Option<String>, Vec<&PagePlacement>> = HashMap::new();
    for page in pages {
        children.entry(page.parent_id.clone()).or_default().push(page);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.page_id.cmp(&b.page_id))
        });
    }

    let mut slugs: HashMap<String, String> = HashMap::new();
    for siblings in children.values() {
        let mut allocator = SlugAllocator::new();
        // Names already on disk win, so a re-pull never renames a stable file.
        for page in siblings {
            if let Some(slug) = existing.get(&page.page_id) {
                allocator.reserve(slug, &page.page_id);
                slugs.insert(page.page_id.clone(), slug.clone());
            }
        }
        for page in siblings {
            if slugs.contains_key(&page.page_id) {
                continue;
            }
            let slug = allocator.allocate(&page.title, &page.page_id);
            slugs.insert(page.page_id.clone(), slug);
        }
    }

    let mut out = HashMap::new();
    for page in pages {
        let Some(slug) = slugs.get(&page.page_id) else { continue };
        let ancestors = ancestor_slugs(page, &by_id, &slugs);
        let path = page_path(&ancestors, slug);
        let sidecar = {
            let mut parts = ancestors;
            parts.push(sidecar_dir_name(slug));
            parts.join("/")
        };
        out.insert(
            page.page_id.clone(),
            Placement { slug: slug.clone(), path, sidecar },
        );
    }
    out
}

/// Slugs of a page's ancestors, root first. Cycles and missing parents are
/// treated as "no parent" rather than looping forever.
fn ancestor_slugs(
    page: &PagePlacement,
    by_id: &HashMap<&str, &PagePlacement>,
    slugs: &HashMap<String, String>,
) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen = vec![page.page_id.clone()];
    let mut current = page.parent_id.clone();

    while let Some(parent_id) = current {
        if seen.contains(&parent_id) {
            break;
        }
        let Some(parent) = by_id.get(parent_id.as_str()) else { break };
        let slug = slugs
            .get(&parent_id)
            .cloned()
            .unwrap_or_else(|| slugify(&parent.title));
        chain.push(slug);
        seen.push(parent_id.clone());
        current = parent.parent_id.clone();
    }
    chain.reverse();
    chain
}

/// A Markdown link from one workspace file to another, e.g.
/// `Handbook/Onboarding.md` → `Handbook/Policies.md` is `Policies.md`.
pub fn relative_link(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = {
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        parts
    };
    let to_parts: Vec<&str> = to.split('/').collect();

    let common = from_dir
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut out: Vec<String> = std::iter::repeat_n("..".to_string(), from_dir.len() - common)
        .chain(to_parts[common..].iter().map(|s| (*s).to_string()))
        .collect();

    if out.is_empty() {
        out.push(to_parts.last().copied().unwrap_or_default().to_string());
    }
    out.join("/")
}

/// The sidecar directory for a page file: `a/b/Page.md` → `a/b/.Page`.
pub fn sidecar_for(path: &str) -> String {
    let stem = path.strip_suffix(".md").unwrap_or(path);
    match stem.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/.{name}"),
        None => format!(".{stem}"),
    }
}

/// The sidecar path as referenced from inside the page body (no directory part).
pub fn sidecar_ref(path: &str) -> String {
    let stem = path.strip_suffix(".md").unwrap_or(path);
    let name = stem.rsplit('/').next().unwrap_or(stem);
    format!(".{name}")
}

/// The directory a page's children live in: `a/Page.md` → `a/Page`.
pub fn children_dir(path: &str) -> String {
    path.strip_suffix(".md").unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str, title: &str, parent: Option<&str>) -> PagePlacement {
        PagePlacement {
            page_id: id.into(),
            title: title.into(),
            parent_id: parent.map(str::to_string),
            position: None,
        }
    }

    #[test]
    fn children_nest_under_a_directory_named_for_the_parent() {
        let pages = vec![
            page("1", "Team Handbook", None),
            page("2", "Onboarding", Some("1")),
            page("3", "Week One", Some("2")),
        ];
        let placed = plan_paths(&pages, &HashMap::new());

        assert_eq!(placed["1"].path, "Team Handbook.md");
        assert_eq!(placed["2"].path, "Team Handbook/Onboarding.md");
        assert_eq!(placed["3"].path, "Team Handbook/Onboarding/Week One.md");
        assert_eq!(placed["2"].sidecar, "Team Handbook/.Onboarding");
    }

    #[test]
    fn siblings_with_the_same_title_get_distinct_files() {
        let pages =
            vec![page("1", "Parent", None), page("2", "Plan", Some("1")), page("3", "Plan", Some("1"))];
        let placed = plan_paths(&pages, &HashMap::new());
        assert_ne!(placed["2"].path, placed["3"].path);
        assert!(placed["3"].path.contains('~'));
    }

    #[test]
    fn identical_titles_under_different_parents_do_not_collide() {
        let pages = vec![
            page("1", "A", None),
            page("2", "B", None),
            page("3", "Notes", Some("1")),
            page("4", "Notes", Some("2")),
        ];
        let placed = plan_paths(&pages, &HashMap::new());
        assert_eq!(placed["3"].path, "A/Notes.md");
        assert_eq!(placed["4"].path, "B/Notes.md");
    }

    #[test]
    fn a_filename_already_on_disk_is_kept() {
        let pages = vec![page("1", "Parent", None), page("2", "Renamed On Server", Some("1"))];
        let existing = [("2".to_string(), "Original Name".to_string())].into_iter().collect();
        let placed = plan_paths(&pages, &existing);
        assert_eq!(placed["2"].path, "Parent/Original Name.md");
    }

    #[test]
    fn a_missing_parent_places_the_page_at_the_root() {
        let pages = vec![page("2", "Orphan", Some("does-not-exist"))];
        let placed = plan_paths(&pages, &HashMap::new());
        assert_eq!(placed["2"].path, "Orphan.md");
    }

    #[test]
    fn a_parent_cycle_does_not_hang() {
        let pages = vec![page("1", "A", Some("2")), page("2", "B", Some("1"))];
        let placed = plan_paths(&pages, &HashMap::new());
        assert_eq!(placed.len(), 2, "both pages still get a path");
    }

    #[test]
    fn ordering_by_position_decides_who_keeps_the_plain_slug() {
        let mut first = page("9", "Plan", None);
        first.position = Some(1);
        let mut second = page("1", "Plan", None);
        second.position = Some(2);

        let placed = plan_paths(&[second, first], &HashMap::new());
        assert_eq!(placed["9"].path, "Plan.md", "position 1 wins the plain name");
        assert!(placed["1"].path.contains('~'));
    }

    #[test]
    fn relative_links_between_pages() {
        assert_eq!(relative_link("Handbook/Onboarding.md", "Handbook/Policies.md"), "Policies.md");
        assert_eq!(relative_link("Handbook/Onboarding.md", "Index.md"), "../Index.md");
        assert_eq!(relative_link("Index.md", "Handbook/Onboarding.md"), "Handbook/Onboarding.md");
        assert_eq!(
            relative_link("A/B/Deep.md", "A/C/Other.md"),
            "../C/Other.md"
        );
        assert_eq!(relative_link("Page.md", "Page.md"), "Page.md");
    }

    #[test]
    fn sidecar_paths_follow_the_page_file() {
        assert_eq!(sidecar_for("Handbook/Onboarding.md"), "Handbook/.Onboarding");
        assert_eq!(sidecar_for("Onboarding.md"), ".Onboarding");
        assert_eq!(sidecar_ref("Handbook/Onboarding.md"), ".Onboarding");
        assert_eq!(children_dir("Handbook/Onboarding.md"), "Handbook/Onboarding");
    }
}
