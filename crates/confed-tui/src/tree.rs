//! The page tree: a hierarchy derived from a worktree scan plus the state DB.
//!
//! `worktree::scan` reports one flat status per page; parent links and sibling
//! order live in the state DB (base records, or the last fetched remote state for
//! pages that were never materialized). Building the tree is a pure function of
//! those three inputs so it can be tested without a workspace.

use confed_core::state::{PageRecord, RemotePage};
use confed_core::worktree::{PageState, Scan};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Node {
    pub page_id: Option<String>,
    /// Workspace-relative path; empty for a page that exists only on the server.
    pub path: String,
    pub title: String,
    pub state: PageState,
    pub position: Option<i64>,
    pub children: Vec<usize>,
    pub expanded: bool,
}

impl Node {
    /// What the tree shows when a page has no file yet.
    pub fn label(&self) -> &str {
        if self.title.is_empty() {
            &self.path
        } else {
            &self.title
        }
    }

    fn matches(&self, needle: &str) -> bool {
        self.title.to_lowercase().contains(needle) || self.path.to_lowercase().contains(needle)
    }

    /// Whether the title holds every one of `words`, which are lowercase.
    fn title_has(&self, words: &[String]) -> bool {
        let title = self.label().to_lowercase();
        words.iter().all(|word| title.contains(word))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
}

/// One visible line of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub node: usize,
    pub depth: usize,
}

impl Tree {
    pub fn build(scan: &Scan, base: &[PageRecord], remote: &[RemotePage]) -> Self {
        let base_by_id: HashMap<&str, &PageRecord> =
            base.iter().map(|r| (r.page_id.as_str(), r)).collect();
        let remote_by_id: HashMap<&str, &RemotePage> =
            remote.iter().map(|r| (r.page_id.as_str(), r)).collect();

        let mut nodes: Vec<Node> = Vec::with_capacity(scan.pages.len());
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut by_path: HashMap<String, usize> = HashMap::new();
        // Parent as recorded by confed, resolved to an index in a second pass.
        let mut parent_ids: Vec<Option<String>> = Vec::with_capacity(scan.pages.len());

        for page in &scan.pages {
            let record = page.page_id.as_deref().and_then(|id| base_by_id.get(id).copied());
            let remote_record =
                page.page_id.as_deref().and_then(|id| remote_by_id.get(id).copied());

            let parent = record
                .and_then(|r| r.parent_id.clone())
                .or_else(|| remote_record.and_then(|r| r.parent_id.clone()));
            let position =
                record.and_then(|r| r.position).or_else(|| remote_record.and_then(|r| r.position));

            let index = nodes.len();
            if let Some(id) = &page.page_id {
                by_id.insert(id.clone(), index);
            }
            if !page.path.is_empty() {
                by_path.insert(page.path.clone(), index);
            }
            parent_ids.push(parent);
            nodes.push(Node {
                page_id: page.page_id.clone(),
                path: page.path.clone(),
                title: page.title.clone(),
                state: page.state,
                position,
                children: Vec::new(),
                expanded: true,
            });
        }

        let mut roots = Vec::new();
        for index in 0..nodes.len() {
            let parent = match &parent_ids[index] {
                Some(id) => by_id.get(id.as_str()).copied(),
                // A page confed has no record of (a brand new file) is placed by
                // its path: `Handbook/Onboarding.md` sits under `Handbook.md`.
                None => parent_by_path(&nodes[index].path).and_then(|p| by_path.get(&p).copied()),
            };
            match parent {
                Some(parent) if parent != index => nodes[parent].children.push(index),
                // An unknown parent means the parent was not pulled: show it at
                // the top level rather than hiding the page.
                _ => roots.push(index),
            }
        }

        let mut tree = Self { nodes, roots };
        tree.sort();
        tree
    }

    /// Confluence order: explicit position first, then title, then path.
    fn sort(&mut self) {
        fn key(node: &Node) -> (i64, String, String) {
            (node.position.unwrap_or(i64::MAX), node.title.to_lowercase(), node.path.to_lowercase())
        }
        let keys: Vec<(i64, String, String)> = self.nodes.iter().map(key).collect();
        let by_key = |a: &usize, b: &usize| keys[*a].cmp(&keys[*b]);

        self.roots.sort_by(by_key);
        for index in 0..self.nodes.len() {
            let mut children = std::mem::take(&mut self.nodes[index].children);
            children.sort_by(by_key);
            self.nodes[index].children = children;
        }
    }

    /// The visible rows, honoring collapse state and the search filter.
    ///
    /// While a filter is active, collapse state is ignored: a match deep in the
    /// tree is useless if its ancestors are folded.
    pub fn rows(&self, filter: &str) -> Vec<Row> {
        let needle = filter.trim().to_lowercase();
        let mut rows = Vec::new();
        for &root in &self.roots {
            self.walk(root, 0, &needle, &mut rows);
        }
        rows
    }

    fn walk(&self, index: usize, depth: usize, needle: &str, rows: &mut Vec<Row>) {
        if !needle.is_empty() && !self.subtree_matches(index, needle) {
            return;
        }
        rows.push(Row { node: index, depth });
        if !needle.is_empty() || self.nodes[index].expanded {
            for &child in &self.nodes[index].children {
                self.walk(child, depth + 1, needle, rows);
            }
        }
    }

    fn subtree_matches(&self, index: usize, needle: &str) -> bool {
        self.nodes[index].matches(needle)
            || self.nodes[index].children.iter().any(|&c| self.subtree_matches(c, needle))
    }

    /// Pages whose title holds every word of `query`, in tree order.
    ///
    /// Unlike [`rows`](Self::rows) this is a flat list with no ancestors: it is
    /// a set of answers to choose from, so every row has to be one.
    pub fn search_titles(&self, query: &str) -> Vec<Row> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut rows = Vec::new();
        for &root in &self.roots {
            self.collect_titles(root, &words, &mut rows);
        }
        rows
    }

    fn collect_titles(&self, index: usize, words: &[String], rows: &mut Vec<Row>) {
        if self.nodes[index].title_has(words) {
            rows.push(Row { node: index, depth: 0 });
        }
        for &child in &self.nodes[index].children {
            self.collect_titles(child, words, rows);
        }
    }

    pub fn has_children(&self, index: usize) -> bool {
        !self.nodes[index].children.is_empty()
    }

    pub fn set_expanded(&mut self, index: usize, expanded: bool) {
        self.nodes[index].expanded = expanded;
    }

    pub fn set_all_expanded(&mut self, expanded: bool) {
        for node in &mut self.nodes {
            node.expanded = expanded;
        }
    }

    /// Restore collapse state across a refresh, keyed by page id then path.
    pub fn carry_over(&mut self, previous: &Tree) {
        let mut collapsed: Vec<&str> = Vec::new();
        for node in &previous.nodes {
            if !node.expanded {
                collapsed.push(node.page_id.as_deref().unwrap_or(node.path.as_str()));
            }
        }
        for node in &mut self.nodes {
            let key = node.page_id.as_deref().unwrap_or(node.path.as_str());
            if collapsed.contains(&key) {
                node.expanded = false;
            }
        }
    }
}

/// `Handbook/Onboarding.md` → `Handbook.md`.
fn parent_by_path(path: &str) -> Option<String> {
    let (dir, _) = path.rsplit_once('/')?;
    Some(format!("{dir}.md"))
}

/// A one-character badge, matching `confed status --short`.
pub fn badge(state: PageState) -> char {
    state.short_code()
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use confed_core::state::{now, SyncState};
    use confed_core::worktree::{FieldChanges, PageStatus};

    pub fn status(id: &str, path: &str, title: &str, state: PageState) -> PageStatus {
        PageStatus {
            page_id: Some(id.to_string()),
            path: path.to_string(),
            title: title.to_string(),
            state,
            base_version: Some(1),
            remote_version: Some(1),
            local_dirty: state.has_local_work(),
            remote_ahead: state.has_remote_work(),
            field_changes: FieldChanges::default(),
            moved_from: None,
            tampering: Vec::new(),
            comment_drafts: 0,
        }
    }

    pub fn record(id: &str, path: &str, parent: Option<&str>, position: Option<i64>) -> PageRecord {
        PageRecord {
            page_id: id.into(),
            title: path.trim_end_matches(".md").rsplit('/').next().unwrap_or(path).into(),
            slug: "s".into(),
            local_path: path.into(),
            parent_id: parent.map(str::to_string),
            position,
            version: 1,
            status: "current".into(),
            labels: Vec::new(),
            author: None,
            created_at: None,
            updated_at: None,
            storage_body: "<p>body</p>".into(),
            storage_hash: "h".into(),
            markdown_hash: "h".into(),
            block_map: None,
            sync_state: SyncState::Clean,
            synced_at: now(),
            render_key: String::new(),
        }
    }

    /// A file that has never been pushed: no page id at all.
    pub fn draft(path: &str, title: &str) -> PageStatus {
        PageStatus { page_id: None, ..status("draft", path, title, PageState::LocalNew) }
    }

    pub fn scan_of(pages: Vec<PageStatus>) -> Scan {
        Scan { pages, ..Default::default() }
    }

    /// A three-level space: Handbook → Onboarding → Week One, plus a sibling.
    pub fn sample() -> (Scan, Vec<PageRecord>) {
        let scan = scan_of(vec![
            status("1", "Handbook.md", "Handbook", PageState::Unchanged),
            status("2", "Handbook/Onboarding.md", "Onboarding", PageState::Modified),
            status("3", "Handbook/Onboarding/Week One.md", "Week One", PageState::Conflicted),
            status("4", "Runbook.md", "Runbook", PageState::Behind),
        ]);
        let records = vec![
            record("1", "Handbook.md", None, Some(0)),
            record("2", "Handbook/Onboarding.md", Some("1"), Some(0)),
            record("3", "Handbook/Onboarding/Week One.md", Some("2"), Some(0)),
            record("4", "Runbook.md", None, Some(1)),
        ];
        (scan, records)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn node_at(tree: &Tree, path: &str) -> usize {
        tree.nodes.iter().position(|n| n.path == path).expect("node")
    }

    fn labels(tree: &Tree, filter: &str) -> Vec<String> {
        tree.rows(filter)
            .iter()
            .map(|row| format!("{}{}", "  ".repeat(row.depth), tree.nodes[row.node].label()))
            .collect()
    }

    #[test]
    fn the_hierarchy_follows_parent_ids_from_the_state_db() {
        let (scan, records) = sample();
        let tree = Tree::build(&scan, &records, &[]);
        assert_eq!(labels(&tree, ""), ["Handbook", "  Onboarding", "    Week One", "Runbook"]);
    }

    #[test]
    fn siblings_are_ordered_by_position_then_title() {
        let scan = scan_of(vec![
            status("1", "Root.md", "Root", PageState::Unchanged),
            status("2", "Root/Zulu.md", "Zulu", PageState::Unchanged),
            status("3", "Root/Alpha.md", "Alpha", PageState::Unchanged),
            status("4", "Root/Mike.md", "Mike", PageState::Unchanged),
        ]);
        let records = vec![
            record("1", "Root.md", None, None),
            // Zulu is pinned first on the server, so it wins over alphabetical.
            record("2", "Root/Zulu.md", Some("1"), Some(0)),
            record("3", "Root/Alpha.md", Some("1"), None),
            record("4", "Root/Mike.md", Some("1"), None),
        ];
        let tree = Tree::build(&scan, &records, &[]);
        assert_eq!(labels(&tree, ""), ["Root", "  Zulu", "  Alpha", "  Mike"]);
    }

    #[test]
    fn collapsing_hides_descendants() {
        let (scan, records) = sample();
        let mut tree = Tree::build(&scan, &records, &[]);
        let handbook = node_at(&tree, "Handbook.md");
        tree.set_expanded(handbook, false);
        assert_eq!(labels(&tree, ""), ["Handbook", "Runbook"]);
        assert!(tree.has_children(handbook));
    }

    #[test]
    fn a_filter_shows_matches_with_their_ancestors_even_when_collapsed() {
        let (scan, records) = sample();
        let mut tree = Tree::build(&scan, &records, &[]);
        tree.set_all_expanded(false);
        assert_eq!(labels(&tree, "week"), ["Handbook", "  Onboarding", "    Week One"]);
        assert!(tree.rows("nothing here").is_empty());
    }

    #[test]
    fn a_title_search_lists_only_the_matching_pages() {
        let (scan, records) = sample();
        let mut tree = Tree::build(&scan, &records, &[]);
        tree.set_all_expanded(false);

        // No ancestors, and no match on the path: `Handbook/Onboarding.md` is
        // not a page called "handbook".
        assert_eq!(labels(&tree, "book"), ["Handbook", "  Onboarding", "    Week One", "Runbook"]);
        let found = |query: &str| -> Vec<&str> {
            tree.search_titles(query).iter().map(|row| tree.nodes[row.node].label()).collect()
        };
        assert_eq!(found("book"), ["Handbook", "Runbook"]);
        assert_eq!(found("WEEK"), ["Week One"], "case does not matter");
        assert_eq!(found("one  week"), ["Week One"], "every word, in any order");
        assert_eq!(found("week two"), [] as [&str; 0]);
        assert!(tree.search_titles("on").iter().all(|row| row.depth == 0), "a flat list");
    }

    #[test]
    fn a_new_local_file_is_placed_under_the_page_that_owns_its_directory() {
        let scan = scan_of(vec![
            status("1", "Handbook.md", "Handbook", PageState::Unchanged),
            draft("Handbook/Draft.md", "Draft"),
        ]);
        let records = vec![record("1", "Handbook.md", None, Some(0))];
        let tree = Tree::build(&scan, &records, &[]);
        assert_eq!(labels(&tree, ""), ["Handbook", "  Draft"]);
    }

    #[test]
    fn a_page_whose_parent_was_never_pulled_stays_visible_at_the_top() {
        let scan = scan_of(vec![status("2", "Orphan.md", "Orphan", PageState::Unchanged)]);
        let records = vec![record("2", "Orphan.md", Some("999"), Some(0))];
        let tree = Tree::build(&scan, &records, &[]);
        assert_eq!(labels(&tree, ""), ["Orphan"]);
    }

    #[test]
    fn collapse_state_survives_a_refresh() {
        let (scan, records) = sample();
        let mut old = Tree::build(&scan, &records, &[]);
        let handbook = node_at(&old, "Handbook.md");
        old.set_expanded(handbook, false);

        let mut fresh = Tree::build(&scan, &records, &[]);
        fresh.carry_over(&old);
        assert_eq!(labels(&fresh, ""), ["Handbook", "Runbook"]);
    }

    #[test]
    fn badges_match_the_short_status_codes() {
        assert_eq!(badge(PageState::Modified), 'M');
        assert_eq!(badge(PageState::Conflicted), 'C');
        assert_eq!(badge(PageState::Unchanged), ' ');
    }
}
