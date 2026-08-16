//! `confed new` — scaffold a page file that `push` will create on the server.

use crate::cli::NewArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ConfedError, Result};
use confed_core::frontmatter::{Frontmatter, MarkdownFile};
use confed_core::slug::slugify;
use confed_core::sync::write_atomic;
use serde_json::json;

pub fn run(ctx: &mut Context, args: &NewArgs) -> Result<Output> {
    let (path, file) = scaffold(ctx, args)?;
    let ws = ctx.workspace()?;
    let absolute = ws.absolute(&path);

    if absolute.exists() {
        return Err(ConfedError::state_with_hint(
            format!("{path} already exists"),
            "pick another name, or edit the existing file",
        ));
    }
    write_atomic(&absolute, &file.render()?)?;

    let parent = file.frontmatter.parent_id.clone();
    let human = format!(
        "Created {path}\n{}\n",
        ctx.style.dim("It will be created on the server by the next `confed push`.")
    );
    Ok(Output::new(
        json!({
            "created_file": path,
            "title": file.frontmatter.title,
            "would_create_under": { "parent_id": parent },
        }),
        human,
    ))
}

pub async fn run_and_push(ctx: &mut Context, args: &NewArgs) -> Result<Output> {
    let created = run(ctx, args)?;
    if !args.push {
        return Ok(created);
    }

    let path = created.result["created_file"].as_str().unwrap_or_default().to_string();
    let client = ctx.build_client()?;
    let engine = ctx.engine(client)?;
    let ws = ctx.workspace_mut()?;
    let _lock = ws.lock()?;
    let outcome = engine
        .push(
            ws,
            &confed_core::sync::PushOptions { scope: vec![path], ..Default::default() },
        )
        .await?;

    let human = format!(
        "{}{}",
        created.human,
        outcome
            .created
            .first()
            .map(|c| format!("Created on the server as page {}\n", c.page_id))
            .unwrap_or_default()
    );
    Ok(Output::new(
        json!({ "created": created.result, "push": serde_json::to_value(&outcome)? }),
        human,
    ))
}

/// Build the file without writing it, so tests can inspect the scaffold.
fn scaffold(ctx: &Context, args: &NewArgs) -> Result<(String, MarkdownFile)> {
    let ws = ctx.workspace()?;

    let raw = args.path.trim_start_matches("./").trim_end_matches(".md");
    let (dir, name) = match raw.rsplit_once('/') {
        Some((dir, name)) => (Some(dir.to_string()), name.to_string()),
        None => (None, raw.to_string()),
    };
    if name.is_empty() {
        return Err(ConfedError::usage("a page needs a name"));
    }

    let title = args.title.clone().unwrap_or_else(|| name.clone());
    let slug = slugify(&name);
    let path = match &dir {
        Some(dir) => format!("{dir}/{slug}.md"),
        None => format!("{slug}.md"),
    };

    // A page inside `A/B/` is a child of the page at `A/B.md`.
    let parent_id = match &dir {
        Some(dir) => ws.state().get_page_by_path(&format!("{dir}.md"))?.map(|p| p.page_id),
        None => None,
    };

    let body = match &args.template {
        Some(template) => std::fs::read_to_string(template)
            .map_err(|e| ConfedError::io(format!("reading {}", template.display()), e))?,
        None => format!("# {title}\n\n"),
    };

    Ok((
        path,
        MarkdownFile::new(
            Frontmatter {
                title,
                labels: args.labels.clone(),
                parent_id,
                // No `confed:` block: that is what marks the page as new.
                managed: None,
                extra: Default::default(),
            },
            body,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use confed_core::workspace::Workspace;

    fn context(dir: &std::path::Path) -> Context {
        let ws = Workspace::create(dir).unwrap();
        ws.state().set_meta("space_key", "DOCS").unwrap();
        let mut ctx = Context::build(crate::cli::GlobalArgs {
            non_interactive: true,
            directory: Some(dir.to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        ctx.set_workspace(ws);
        ctx
    }

    fn args(path: &str) -> NewArgs {
        NewArgs {
            path: path.into(),
            title: None,
            labels: vec![],
            template: None,
            push: false,
        }
    }

    #[test]
    fn a_scaffolded_page_has_no_managed_block_so_push_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());

        let (path, file) = scaffold(&ctx, &args("Runbooks/Database Failover")).unwrap();
        assert_eq!(path, "Runbooks/Database Failover.md");
        assert_eq!(file.frontmatter.title, "Database Failover");
        assert!(file.frontmatter.is_new());
        assert!(file.body.starts_with("# Database Failover"));
    }

    #[test]
    fn the_title_flag_wins_over_the_filename() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let mut a = args("Notes/2026-08-16");
        a.title = Some("Weekly Sync".into());
        a.labels = vec!["notes".into()];

        let (_, file) = scaffold(&ctx, &a).unwrap();
        assert_eq!(file.frontmatter.title, "Weekly Sync");
        assert_eq!(file.frontmatter.labels, ["notes"]);
    }

    #[test]
    fn unsafe_characters_in_the_name_are_slugged() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (path, file) = scaffold(&ctx, &args("Q3/Q4: Plan?")).unwrap();
        assert_eq!(path, "Q3/Q4- Plan-.md");
        assert_eq!(file.frontmatter.title, "Q4: Plan?", "the title keeps the real characters");
    }

    #[test]
    fn a_trailing_md_extension_is_not_doubled() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let (path, _) = scaffold(&ctx, &args("Notes.md")).unwrap();
        assert_eq!(path, "Notes.md");
    }

    #[test]
    fn writing_over_an_existing_page_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        run(&mut ctx, &args("Page")).unwrap();

        let err = run(&mut ctx, &args("Page")).unwrap_err();
        assert_eq!(err.exit_code(), confed_core::ExitCode::State);
    }

    #[test]
    fn the_scaffold_round_trips_through_the_parser() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        run(&mut ctx, &args("Page")).unwrap();

        let content = std::fs::read_to_string(dir.path().join("Page.md")).unwrap();
        let parsed = confed_core::frontmatter::parse(&content, "Page.md").unwrap();
        assert_eq!(parsed.frontmatter.title, "Page");
        assert!(parsed.frontmatter.is_new());
    }
}
