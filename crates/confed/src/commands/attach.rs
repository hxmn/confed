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
        let recorded = ws.state().page_attachments(&page_id)?;
        let mut human = String::new();
        for attachment in &recorded {
            let _ = writeln!(
                human,
                "{:<30} {:>10}  {}",
                attachment.filename,
                attachment.file_size.map(|s| s.to_string()).unwrap_or_default(),
                ctx.style.dim(&format!("v{}", attachment.version))
            );
        }
        if recorded.is_empty() {
            human.push_str("No attachments.\n");
        }
        return Ok(Output::new(
            json!({
                "page_id": page_id,
                "attachments": recorded.iter().map(|a| json!({
                    "id": a.attachment_id, "file": a.filename,
                    "size": a.file_size, "version": a.version,
                    "sha256": a.sha256, "downloaded": a.downloaded,
                })).collect::<Vec<_>>()
            }),
            human,
        ));
    }

    if let Some(filename) = &args.remove {
        let path = sidecar.join(filename);
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| ConfedError::io(format!("removing {filename}"), e))?;
        }
        return Ok(Output::new(
            json!({ "page_id": page_id, "removed": filename }),
            format!(
                "Removed {filename} locally.\n{}\n",
                ctx.style.dim("It is deleted on the server at the next push.")
            ),
        ));
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
