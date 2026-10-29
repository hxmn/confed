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
    if let Some(destination) = &args.destination {
        refuse_other_space(ctx, &args.source, destination)?;
    }
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

    // Order among siblings lives only on the server, and a push does not
    // carry it: it is made there, now, or not at all.
    let reorder = args.before.is_some() || args.after.is_some() || args.position.is_some();
    if reorder && !args.push {
        return Err(ConfedError::usage_with_hint(
            "the order of pages lives on the server, and `confed push` does not carry it",
            "add --push to reorder on the server now",
        ));
    }
    let sibling = if reorder { Some(reorder_target(ctx, &page_id, args)?) } else { None };
    let asked_to_reorder = sibling.is_some();

    let mut human = format!("Moved {old_path} -> {new_path}\n");
    let mut pushed = false;
    let mut reordered = false;
    let mut warnings = Vec::new();
    if args.push {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client.clone())?;
        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome = engine
            .push(ws, &PushOptions { scope: vec![new_path.clone()], ..Default::default() })
            .await?;
        if let Some(refused) = outcome.cross_space.first().or(outcome.skipped.first()) {
            return Err(ConfedError::state(format!("{}: {}", refused.path, refused.reason)));
        }
        if let Some(failed) = outcome.failed.first() {
            return Err(ConfedError::Other(format!("{}: {}", failed.page_id, failed.error)));
        }
        pushed = !outcome.pushed.is_empty() || !outcome.created.is_empty();
        if let Some(target) = sibling.flatten() {
            client
                .move_page(&confed_api::PageId::new(&page_id), &target.parent, target.position)
                .await?;
            reordered = true;
            if client.flavor() == confed_api::Flavor::DataCenter {
                warnings.push(
                    "Data Center does not report the order of pages, so confed cannot read it \
                     back; check it in the browser"
                        .to_string(),
                );
            }
        }
        human.push_str(match (pushed, reordered) {
            (_, true) => "Applied on the server, and reordered there.\n",
            (true, false) => "Applied on the server.\n",
            (false, false) if asked_to_reorder => "Already in that place; nothing to send.\n",
            (false, false) => "Nothing to send to the server.\n",
        });
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
            "reordered": reordered,
        }),
        human,
    )
    .warn_all(warnings))
}

/// Where a reorder puts the page: before or after a sibling, as the server
/// takes it. `None` when `--position` is past the last sibling and the page
/// is already last, or it has no siblings.
struct ReorderTarget {
    parent: confed_api::PageId,
    position: confed_api::Position,
}

fn reorder_target(ctx: &Context, page_id: &str, args: &MvArgs) -> Result<Option<ReorderTarget>> {
    let ws = ctx.workspace()?;
    let parent_of = |id: &str| -> Result<Option<String>> {
        Ok(ws.state().get_remote(id)?.and_then(|r| r.parent_id))
    };
    let numeric = |id: &str| {
        id.parse::<u64>().map_err(|_| {
            ConfedError::usage(format!("page id {id} is not a number the server takes"))
        })
    };
    let parent = |id: &str| -> Result<confed_api::PageId> {
        Ok(confed_api::PageId::new(parent_of(id)?.unwrap_or_default()))
    };
    if let Some(before) = &args.before {
        let sibling = ctx.resolve_page(before)?;
        return Ok(Some(ReorderTarget {
            parent: parent(&sibling)?,
            position: confed_api::Position::Before(numeric(&sibling)?),
        }));
    }
    if let Some(after) = &args.after {
        let sibling = ctx.resolve_page(after)?;
        return Ok(Some(ReorderTarget {
            parent: parent(&sibling)?,
            position: confed_api::Position::After(numeric(&sibling)?),
        }));
    }
    let Some(index) = args.position else { return Ok(None) };
    let own_parent = parent_of(page_id)?;
    let mut siblings: Vec<_> = ws
        .state()
        .all_remote()?
        .into_iter()
        .filter(|r| !r.deleted && r.parent_id == own_parent && r.page_id != page_id)
        .collect();
    siblings.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.title.cmp(&b.title)));
    let index = usize::try_from(index.max(0)).unwrap_or(0);
    let parent = confed_api::PageId::new(own_parent.unwrap_or_default());
    Ok(match (siblings.get(index), siblings.last()) {
        (Some(at), _) => Some(ReorderTarget {
            parent,
            position: confed_api::Position::Before(numeric(&at.page_id)?),
        }),
        (None, Some(last)) => Some(ReorderTarget {
            parent,
            position: confed_api::Position::After(numeric(&last.page_id)?),
        }),
        (None, None) => None,
    })
}

/// A destination inside another workspace of the same instance root is a
/// move between spaces, which only `confed instance mv` makes: say how, with
/// the paths as that command takes them, and touch nothing (exit 2).
fn refuse_other_space(ctx: &Context, source: &str, destination: &str) -> Result<()> {
    let Ok(instance) = ctx.instance() else { return Ok(()) };
    let ws = ctx.workspace()?;
    let own = ws.space_key()?;
    let (member, rest) = match crate::context::split_space_ref(destination) {
        Some((key, rest)) if !key.eq_ignore_ascii_case(&own) => match instance.member(key)? {
            Some(member) => (member, rest.to_string()),
            None => return Ok(()),
        },
        Some(_) => return Ok(()),
        None => {
            let mut absolute = std::path::PathBuf::new();
            for part in ctx.cwd.join(destination).components() {
                match part {
                    std::path::Component::ParentDir => {
                        absolute.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => absolute.push(other),
                }
            }
            let Some(member) = instance.member_for_path(&absolute)? else { return Ok(()) };
            if member.space_key.eq_ignore_ascii_case(&own) {
                return Ok(());
            }
            let dir = instance.member_dir(&member);
            let dir = dir.canonicalize().unwrap_or(dir);
            let parent =
                absolute.parent().map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
            let absolute = match (parent, absolute.file_name()) {
                (Some(parent), Some(name)) => parent.join(name),
                _ => absolute,
            };
            let rest = absolute
                .strip_prefix(&dir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            (member, rest)
        }
    };
    let from = match ctx.resolve_page(source) {
        Ok(id) => ws
            .state()
            .get_page(&id)?
            .map(|p| format!("{own}:{}", p.local_path))
            .unwrap_or_else(|| id.clone()),
        Err(_) => format!("{own}:{source}"),
    };
    let to = format!("{}:{rest}", member.space_key);
    Err(ConfedError::usage_with_hint(
        format!(
            "{destination} is in the workspace of space {}, not {own}: that is a move between \
             spaces",
            member.space_key
        ),
        format!(
            "from the instance root ({}), run: confed instance mv \"{from}\" \"{to}\" --dry-run",
            instance.root().display()
        ),
    ))
}
