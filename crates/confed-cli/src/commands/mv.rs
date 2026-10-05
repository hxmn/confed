//! `confed mv` — rename, move, or reorder a page.

use crate::cli::MvArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::paths;
use confed_core::slug::slugify;
use confed_core::sync::{write_atomic, PushOptions};
use serde_json::json;

pub async fn run(ctx: &mut Context, args: &MvArgs) -> Result<Output> {
    let page_id = ctx.resolve_page(&args.source)?;
    let ws = ctx.workspace()?;
    let record = ws
        .state()
        .get_page(&page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("no page {}", args.source)))?;

    let old_path = record.local_path.clone();
    let mut title_changed = false;
    let mut parent_changed = false;
    let mut new_path = old_path.clone();

    if let Some(destination) = &args.destination {
        let destination = ctx.workspace_path(destination);
        let raw = destination.trim_end_matches(".md");
        let (dir, name) = match raw.rsplit_once('/') {
            Some((dir, name)) => (Some(dir.to_string()), name.to_string()),
            None => (None, raw.to_string()),
        };
        new_path = match &dir {
            Some(dir) => format!("{dir}/{}.md", slugify(&name)),
            None => format!("{}.md", slugify(&name)),
        };

        let old_dir = old_path.rsplit_once('/').map(|(d, _)| d.to_string());
        parent_changed = dir != old_dir;
        // Renaming the file renames the page unless told otherwise.
        title_changed = args.rename_title || name != record.title;

        let absolute_new = ws.absolute(&new_path);
        if absolute_new.exists() && new_path != old_path {
            return Err(ConfedError::state(format!("{new_path} already exists")));
        }

        if new_path != old_path {
            let absolute_old = ws.absolute(&old_path);
            if let Some(parent) = absolute_new.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ConfedError::io(format!("creating {}", parent.display()), e))?;
            }
            std::fs::rename(&absolute_old, &absolute_new)
                .map_err(|e| ConfedError::io(format!("moving {old_path}"), e))?;

            // The sidecar and the children directory follow the page.
            for (from, to) in [
                (paths::sidecar_for(&old_path), paths::sidecar_for(&new_path)),
                (paths::children_dir(&old_path), paths::children_dir(&new_path)),
            ] {
                let from_abs = ws.absolute(&from);
                if from_abs.is_dir() {
                    let _ = std::fs::rename(from_abs, ws.absolute(&to));
                }
            }
        }

        // Update the file's own frontmatter to match its new home.
        let content = std::fs::read_to_string(ws.absolute(&new_path))
            .map_err(|e| ConfedError::io(format!("reading {new_path}"), e))?;
        let mut file = confed_core::frontmatter::parse(&content, &new_path)?;
        if title_changed {
            file.frontmatter.title = name.clone();
        }
        if parent_changed {
            file.frontmatter.parent_id = match &dir {
                Some(dir) => ws.state().get_page_by_path(&format!("{dir}.md"))?.map(|p| p.page_id),
                None => None,
            };
        }
        write_atomic(&ws.absolute(&new_path), &file.render()?)?;

        let mut updated = record.clone();
        updated.local_path = new_path.clone();
        updated.slug = new_path
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".md"))
            .unwrap_or(&new_path)
            .to_string();
        ctx.workspace()?.state().upsert_page(&updated)?;
    }

    let reorder = args.before.is_some() || args.after.is_some() || args.position.is_some();
    if reorder {
        // Ordering lives on the server; record the intent for the next push.
        let position = match (&args.before, &args.after, args.position) {
            (_, _, Some(p)) => Some(p),
            (Some(sibling), _, _) | (_, Some(sibling), _) => ctx
                .workspace()?
                .state()
                .get_page(&ctx.resolve_page(sibling)?)?
                .and_then(|p| p.position),
            _ => None,
        };
        let mut updated = ctx.workspace()?.state().get_page(&page_id)?.unwrap_or(record.clone());
        updated.position = position;
        ctx.workspace()?.state().upsert_page(&updated)?;
    }

    let mut human = format!("Moved {old_path} -> {new_path}\n");
    let mut pushed = false;
    if args.push {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        engine
            .push(ws, &PushOptions { scope: vec![new_path.clone()], ..Default::default() })
            .await?;
        pushed = true;
        human.push_str("Applied on the server.\n");
    } else {
        human.push_str(&ctx.style.dim("Run `confed push` to apply it on the server.\n"));
    }

    Ok(Output::new(
        json!({
            "page_id": page_id,
            "from": old_path,
            "to": new_path,
            "title_changed": title_changed,
            "parent_changed": parent_changed,
            "pushed": pushed,
        }),
        human,
    ))
}
