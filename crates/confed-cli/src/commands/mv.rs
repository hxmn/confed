//! `confed mv` — rename, move, or reorder a page.

use crate::cli::MvArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::local::{self, LocalMove};
use confed_core::sync::PushOptions;
use serde_json::json;

/// The move on disk, and what it would reorder: everything `confed mv` does
/// before the server, with no credential needed.
struct Planned {
    page_id: String,
    moved: LocalMove,
    sibling: Option<Option<ReorderTarget>>,
}

fn move_locally(ctx: &mut Context, args: &MvArgs) -> Result<Planned> {
    if let Some(destination) = &args.destination {
        refuse_other_space(ctx, &args.source, destination)?;
    }
    let page_id = ctx.resolve_page(&args.source)?;

    // Order among siblings lives only on the server, and a push does not
    // carry it: it is made there, now, or not at all.
    let reorder = args.before.is_some() || args.after.is_some() || args.position.is_some();
    if reorder && !args.push {
        return Err(ConfedError::usage_with_hint(
            "the order of pages lives on the server, and `confed push` does not carry it",
            "add --push to reorder on the server now",
        ));
    }

    let ws = ctx.workspace()?;
    let moved = match &args.destination {
        Some(destination) => {
            local::move_page(ws, &page_id, &ctx.workspace_path(destination), args.rename_title)?
        }
        None => {
            let record = ws
                .state()
                .get_page(&page_id)?
                .ok_or_else(|| ConfedError::NotFound(format!("no page {}", args.source)))?;
            LocalMove {
                page_id: page_id.clone(),
                from: record.local_path.clone(),
                to: record.local_path,
                title_changed: false,
                parent_changed: false,
            }
        }
    };
    let sibling = if reorder { Some(reorder_target(ctx, &page_id, args)?) } else { None };
    Ok(Planned { page_id, moved, sibling })
}

fn result(planned: &Planned, pushed: bool, reordered: bool) -> serde_json::Value {
    let m = &planned.moved;
    json!({
        "page_id": planned.page_id,
        "from": m.from,
        "to": m.to,
        "title_changed": m.title_changed,
        "parent_changed": m.parent_changed,
        "pushed": pushed,
        "reordered": reordered,
    })
}

/// `confed mv` without `--push`: the files move, and the next push sends it.
pub fn run(ctx: &mut Context, args: &MvArgs) -> Result<Output> {
    let planned = move_locally(ctx, args)?;
    let human = format!(
        "Moved {} -> {}\n{}",
        planned.moved.from,
        planned.moved.to,
        ctx.style.dim("Run `confed push` to apply it on the server.\n")
    );
    Ok(Output::new(result(&planned, false, false), human))
}

/// `confed mv --push`: the same, then applied on the server.
pub async fn run_and_push(ctx: &mut Context, args: &MvArgs) -> Result<Output> {
    let planned = move_locally(ctx, args)?;
    let new_path = planned.moved.to.clone();
    let page_id = planned.page_id.clone();
    let asked_to_reorder = planned.sibling.is_some();

    let mut human = format!("Moved {} -> {new_path}\n", planned.moved.from);
    let mut warnings = Vec::new();
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
    let pushed = !outcome.pushed.is_empty() || !outcome.created.is_empty();
    let mut reordered = false;
    if let Some(Some(target)) = &planned.sibling {
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

    Ok(Output::new(result(&planned, pushed, reordered), human).warn_all(warnings))
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
