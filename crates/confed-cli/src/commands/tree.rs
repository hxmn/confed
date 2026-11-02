//! `confed tree` — the space's pages as the server nests them: only real
//! pages, each with its id and title, and its file when it has been pulled.
//! `confed instance tree` shows the same for any space of an instance.

use crate::cli::TreeArgs;
use crate::context::Context;
use crate::output::Output;
use confed_api::PageSummary;
use confed_core::error::{ConfedError, Result};
use confed_core::workspace::Workspace;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write;

/// One page of the tree.
#[derive(Clone, Debug)]
pub struct Node {
    pub page_id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub position: Option<i64>,
    pub version: u32,
    /// Its file, when the workspace has it.
    pub path: Option<String>,
}

/// The tree as the workspace's last fetch listed it.
pub fn from_state(ws: &Workspace) -> Result<Vec<Node>> {
    let paths: HashMap<String, String> =
        ws.state().all_pages()?.into_iter().map(|p| (p.page_id, p.local_path)).collect();
    Ok(ws
        .state()
        .all_remote()?
        .into_iter()
        .filter(|r| !r.deleted)
        .map(|r| Node {
            path: paths.get(&r.page_id).cloned(),
            page_id: r.page_id,
            title: r.title,
            parent_id: r.parent_id,
            position: r.position,
            version: r.version,
        })
        .collect())
}

/// The tree as the server lists it now, with the workspace's files beside it.
pub fn from_listing(ws: &Workspace, listed: &[PageSummary]) -> Result<Vec<Node>> {
    let paths: HashMap<String, String> =
        ws.state().all_pages()?.into_iter().map(|p| (p.page_id, p.local_path)).collect();
    Ok(listed
        .iter()
        .map(|p| Node {
            page_id: p.id.0.clone(),
            title: p.title.clone(),
            parent_id: p.parent_id.as_ref().map(|id| id.0.clone()),
            position: p.position,
            version: p.version,
            path: paths.get(&p.id.0).cloned(),
        })
        .collect())
}

/// The pages under `root` (the space's top-level pages when `None`), in
/// sibling order, parent first, with their depth: 0 for `root`'s children.
pub fn walk<'a>(
    nodes: &'a [Node],
    root: Option<&str>,
    max_depth: Option<usize>,
) -> Vec<(usize, &'a Node)> {
    let known: std::collections::HashSet<&str> = nodes.iter().map(|n| n.page_id.as_str()).collect();
    let mut children: HashMap<Option<&str>, Vec<&Node>> = HashMap::new();
    for node in nodes {
        // A parent the listing does not have (a restricted page) puts its
        // children at the top, where they can still be seen.
        let parent = node.parent_id.as_deref().filter(|p| known.contains(p));
        children.entry(parent).or_default().push(node);
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));
    }
    let mut out = Vec::new();
    let mut stack: Vec<(usize, &Node)> =
        children.get(&root).map(|l| l.iter().rev().map(|n| (0, *n)).collect()).unwrap_or_default();
    while let Some((depth, node)) = stack.pop() {
        out.push((depth, node));
        if max_depth.is_some_and(|max| depth + 1 >= max) {
            continue;
        }
        if let Some(kids) = children.get(&Some(node.page_id.as_str())) {
            stack.extend(kids.iter().rev().map(|k| (depth + 1, *k)));
        }
    }
    out
}

/// The tree under `root`, as a result and as text.
pub fn render(
    ctx: &Context,
    nodes: &[Node],
    root: Option<&str>,
    depth: Option<usize>,
    space: &str,
) -> Result<Output> {
    if let Some(root) = root {
        if !nodes.iter().any(|n| n.page_id == root) {
            return Err(ConfedError::NotFound(format!(
                "page {root} is not in space {space}'s listing; `confed fetch` first, or pass \
                 --remote"
            )));
        }
    }
    let style = &ctx.style;
    let mut human = String::new();
    if let Some(node) = root.and_then(|r| nodes.iter().find(|n| n.page_id == r)) {
        let _ = writeln!(human, "{}  {}", node.title, style.dim(&node.page_id));
    }
    let rows = walk(nodes, root, depth);
    let mut pages = Vec::new();
    for (depth, node) in &rows {
        let indent = "  ".repeat(*depth + usize::from(root.is_some()));
        let local = if node.path.is_some() { "" } else { "  (not pulled)" };
        let _ = writeln!(
            human,
            "{indent}{}  {}{}",
            node.title,
            style.dim(&node.page_id),
            style.dim(local)
        );
        pages.push(json!({
            "page_id": node.page_id,
            "title": node.title,
            "parent_id": node.parent_id,
            "depth": depth,
            "version": node.version,
            "local": node.path.is_some(),
            "path": node.path,
        }));
    }
    Ok(Output::new(json!({ "space": space, "root": root, "pages": pages }), human))
}

pub fn run(ctx: &mut Context, args: &TreeArgs) -> Result<Output> {
    let root = args.page.as_deref().map(|p| ctx.resolve_page(p)).transpose()?;
    let ws = ctx.workspace()?;
    let nodes = from_state(ws)?;
    render(ctx, &nodes, root.as_deref(), args.depth, &ws.space_key()?)
}

pub async fn run_remote(ctx: &mut Context, args: &TreeArgs) -> Result<Output> {
    let root = args.page.as_deref().map(|p| ctx.resolve_page(p)).transpose()?;
    let client = ctx.build_client()?;
    let ws = ctx.workspace()?;
    let space = confed_api::SpaceId { key: ws.space_key()?, numeric: ws.space_numeric_id()? };
    let listed = client.list_pages(&space).await?;
    let nodes = from_listing(ws, &listed)?;
    render(ctx, &nodes, root.as_deref(), args.depth, &space.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, title: &str, parent: Option<&str>, position: i64) -> Node {
        Node {
            page_id: id.into(),
            title: title.into(),
            parent_id: parent.map(str::to_string),
            position: Some(position),
            version: 1,
            path: None,
        }
    }

    #[test]
    fn pages_come_parent_first_in_sibling_order_to_the_depth_asked() {
        let nodes = vec![
            node("1001", "Team Handbook", None, 0),
            node("1004", "Policies", Some("1001"), 2),
            node("1002", "Onboarding", Some("1001"), 1),
            node("1003", "Week One", Some("1002"), 0),
        ];
        let ids = |rows: Vec<(usize, &Node)>| {
            rows.iter().map(|(d, n)| format!("{d}:{}", n.page_id)).collect::<Vec<_>>()
        };
        assert_eq!(ids(walk(&nodes, None, None)), ["0:1001", "1:1002", "2:1003", "1:1004"]);
        assert_eq!(ids(walk(&nodes, Some("1001"), Some(1))), ["0:1002", "0:1004"]);
        assert_eq!(ids(walk(&nodes, Some("1003"), None)), Vec::<String>::new());
    }
}
