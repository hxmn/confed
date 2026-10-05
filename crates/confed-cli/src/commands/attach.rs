//! `confed attach` — manage a page's attachments in its sidecar directory.

use crate::cli::AttachArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::attachments;
use confed_core::error::{ConfedError, Result};
use confed_core::paths;
use confed_core::sync::PushOptions;
use serde_json::json;
use std::fmt::Write;

pub async fn run(ctx: &mut Context, args: &AttachArgs) -> Result<Output> {
    let page_id = ctx.resolve_page(&args.page)?;
    let ws = ctx.workspace()?;
    let record = ws
        .state()
        .get_page(&page_id)?
        .ok_or_else(|| ConfedError::NotFound(format!("no page {}", args.page)))?;
    let sidecar_rel = paths::sidecar_for(&record.local_path);
    let sidecar = ws.absolute(&sidecar_rel);

    if args.list {
        return list(ctx, &page_id, args.remote).await;
    }

    if let Some(filename) = &args.remove {
        return remove(ctx, &page_id, &record.local_path, &sidecar, filename, args.push).await;
    }

    if args.files.is_empty() {
        return Err(ConfedError::usage("give at least one file to attach, or use --list"));
    }

    std::fs::create_dir_all(&sidecar)
        .map_err(|e| ConfedError::io(format!("creating {}", sidecar.display()), e))?;

    let mut attached = Vec::new();
    let mut human = String::new();
    for file in &args.files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| ConfedError::usage(format!("{} has no filename", file.display())))?;
        if attachments::is_partial(name) {
            return Err(ConfedError::usage_with_hint(
                format!("{name} is a confed scratch name, not an attachment"),
                "rename the file: push never uploads *.confed-part",
            ));
        }
        let dest = sidecar.join(name);
        std::fs::copy(file, &dest)
            .map_err(|e| ConfedError::io(format!("copying {}", file.display()), e))?;

        let sha = attachments::file_sha256(&dest)?;
        let size = attachments::file_size(&dest);
        let _ = writeln!(human, "  attached {sidecar_rel}/{name}");
        let _ = writeln!(
            human,
            "  {}",
            ctx.style.dim(&format!(
                "reference it as ![{name}]({}/{name})",
                paths::sidecar_ref(&record.local_path)
            ))
        );
        attached.push(json!({ "file": name, "size": size, "sha256": sha }));
    }

    let mut result = json!({ "page_id": page_id, "attached": attached });
    if args.push {
        let client = ctx.build_client()?;
        let engine = ctx.engine(client)?;
        let ws = ctx.workspace_mut()?;
        let _lock = ws.lock()?;
        let outcome = engine
            .push(
                ws,
                &PushOptions {
                    scope: vec![record.local_path.clone()],
                    with_attachments: true,
                    ..Default::default()
                },
            )
            .await?;
        result["push"] = serde_json::to_value(&outcome)?;
    }
    Ok(Output::new(result, human))
}

/// `--list`: from the local state by default, from the server with `--remote`.
///
/// The local answer is only as fresh as the last fetch, so it says so — a
/// stale listing that reads as authoritative hides server-side drift.
async fn list(ctx: &mut Context, page_id: &str, remote: bool) -> Result<Output> {
    let (rows, source) = if remote {
        let client = ctx.build_client()?;
        let listed = client.list_attachments(&confed_api::PageId::new(page_id)).await?;
        let rows = listed
            .iter()
            .map(|a| {
                (
                    a.filename.clone(),
                    a.file_size,
                    a.version,
                    json!({
                        "id": a.id.0, "file": a.filename, "size": a.file_size,
                        "version": a.version, "media_type": a.media_type,
                    }),
                )
            })
            .collect::<Vec<_>>();
        (rows, "server")
    } else {
        let recorded = ctx.workspace()?.state().page_attachments(page_id)?;
        let rows = recorded
            .iter()
            .map(|a| {
                (
                    a.filename.clone(),
                    a.file_size,
                    a.version,
                    json!({
                        "id": a.attachment_id, "file": a.filename, "size": a.file_size,
                        "version": a.version, "sha256": a.sha256, "downloaded": a.downloaded,
                    }),
                )
            })
            .collect::<Vec<_>>();
        (rows, "cache")
    };

    let last_fetch = ctx.workspace()?.state().get_meta("last_fetch_at")?;
    let mut human = String::new();
    for (filename, size, version, _) in &rows {
        let _ = writeln!(
            human,
            "{:<30} {:>10}  {}",
            filename,
            size.map(|s| s.to_string()).unwrap_or_default(),
            ctx.style.dim(&format!("v{version}"))
        );
    }
    if rows.is_empty() {
        human.push_str("No attachments.\n");
    }
    if !remote {
        let when = last_fetch.as_deref().unwrap_or("never");
        let _ = writeln!(
            human,
            "{}",
            ctx.style.dim(&format!(
                "(local state, last fetched {when} — use --remote to ask the server)"
            ))
        );
    }

    Ok(Output::new(
        json!({
            "page_id": page_id,
            "source": source,
            "last_fetch_at": last_fetch,
            "attachments": rows.into_iter().map(|(_, _, _, row)| row).collect::<Vec<_>>(),
        }),
        human,
    ))
}

/// `--rm`: remove the local file, and mean it about the server.
///
/// Reporting "removed" for an attachment that is still live is worse than
/// refusing, so this either deletes it (`--push`) or says plainly that the
/// deletion is only staged and what it takes to apply.
async fn remove(
    ctx: &mut Context,
    page_id: &str,
    local_path: &str,
    sidecar: &std::path::Path,
    filename: &str,
    push: bool,
) -> Result<Output> {
    let recorded = ctx.workspace()?.state().page_attachments(page_id)?;
    let on_server = recorded.iter().any(|a| a.filename == filename);
    let path = sidecar.join(filename);
    let local = path.exists();

    if !local && !on_server {
        return Err(ConfedError::NotFound(format!(
            "no attachment {filename} on this page; `confed attach <page> --list --remote` shows what is there"
        )));
    }
    if local {
        std::fs::remove_file(&path)
            .map_err(|e| ConfedError::io(format!("removing {filename}"), e))?;
    }

    // Never uploaded, so there is nothing on the server to chase.
    if !on_server {
        return Ok(Output::new(
            json!({
                "page_id": page_id, "file": filename,
                "removed_locally": true, "removed_on_server": false, "staged": false,
            }),
            format!("Removed {filename} locally. It was never uploaded.\n"),
        ));
    }

    if !push {
        return Ok(Output::new(
            json!({
                "page_id": page_id, "file": filename,
                "removed_locally": local, "removed_on_server": false, "staged": true,
            }),
            format!(
                "Removed {filename} locally; it is still on the server.\n{}\n",
                ctx.style
                    .dim("Run `confed push --allow-delete` to delete it, or re-run with --push.")
            ),
        ));
    }

    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let scope = vec![local_path.to_string()];
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    // `allow_attachment_delete`, not `allow_delete`: removing an attachment must
    // not become a way to delete the page it hangs off.
    let outcome = engine
        .push(
            ws,
            &PushOptions {
                scope,
                with_attachments: true,
                allow_attachment_delete: true,
                ..Default::default()
            },
        )
        .await?;

    let expected = format!("{}/{filename}", paths::sidecar_for(local_path));
    let deleted = outcome.attachments_deleted.contains(&expected);
    let human = if deleted {
        format!("Removed {filename} locally and on the server.\n")
    } else {
        format!("Removed {filename} locally, but the server delete did not happen.\n")
    };
    let mut output = Output::new(
        json!({
            "page_id": page_id, "file": filename,
            "removed_locally": local, "removed_on_server": deleted, "staged": !deleted,
            "push": serde_json::to_value(&outcome)?,
        }),
        human,
    );
    if !deleted {
        output = output.warn(format!("{filename} is still an attachment on page {page_id}"));
        output.exit = confed_core::error::ExitCode::Partial;
    }
    Ok(output)
}
