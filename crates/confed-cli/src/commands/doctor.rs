//! `confed doctor` — check connectivity, credentials, and local state.
//!
//! Every check reports pass / warn / fail with a specific next step, and
//! `--fix` applies the ones that are safe to automate.

use crate::cli::DoctorArgs;
use crate::context::Context;
use crate::output::Output;
use confed_core::error::{ExitCode, Result};
use confed_core::session::{keyring_available, SecretBackend};
use confed_core::state::STATE_DB_FILENAME;
use confed_core::workspace::{self, Workspace};
use confed_core::worktree;
use serde::Serialize;
use serde_json::json;
use std::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    pub fix_applied: bool,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: CheckStatus::Pass, detail: detail.into(), fix_applied: false }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: CheckStatus::Warn, detail: detail.into(), fix_applied: false }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, status: CheckStatus::Fail, detail: detail.into(), fix_applied: false }
    }
    fn fixed(mut self) -> Self {
        self.fix_applied = true;
        self.status = CheckStatus::Pass;
        self
    }
}

pub async fn run(ctx: &mut Context, args: &DoctorArgs) -> Result<Output> {
    let mut checks = Vec::new();

    // --- local state ------------------------------------------------------
    if !ctx.has_workspace() {
        checks.push(Check::fail(
            "workspace",
            "no .state.db here; run `confed init` or `confed clone <space>`",
        ));
        return Ok(report(ctx, checks));
    }
    let root = ctx.workspace()?.root().to_path_buf();
    checks.push(Check::pass("workspace", format!("{}", root.display())));

    match ctx.workspace()?.state().integrity_check() {
        Ok(result) if result == "ok" => {
            checks.push(Check::pass("state database", "integrity check passed"))
        }
        Ok(other) => checks.push(Check::fail("state database", format!("corrupt: {other}"))),
        Err(e) => checks.push(Check::fail("state database", e.to_string())),
    }

    let schema = ctx.workspace()?.state().get_meta("schema_version")?.unwrap_or_default();
    checks.push(Check::pass("state schema", format!("version {schema}")));

    // --- .gitignore -------------------------------------------------------
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
    let missing: Vec<&str> = workspace::IGNORED_FILES
        .iter()
        .filter(|f| !gitignore.lines().any(|l| l.trim().trim_start_matches('/') == **f))
        .copied()
        .collect();
    if missing.is_empty() {
        checks.push(Check::pass("gitignore", "credentials and state are ignored"));
    } else {
        let detail = format!("not ignored: {}", missing.join(", "));
        checks.push(if args.fix {
            workspace::ensure_gitignore(&root)?;
            Check::warn("gitignore", detail).fixed()
        } else {
            Check::warn("gitignore", format!("{detail} (run with --fix)"))
        });
    }

    // --- credentials ------------------------------------------------------
    match ctx.session()? {
        None => checks.push(Check::fail("credentials", "no session stored; run `confed init`")),
        Some(session) => {
            checks.push(Check::pass(
                "credentials",
                format!("{} auth for {}", session.auth_method.as_str(), session.base_url),
            ));
            if session.secret_backend == SecretBackend::Sqlite && keyring_available() {
                // A keyring is here now, so the file store was a choice (or the
                // keyring appeared later) — not something to warn about.
                checks.push(Check::pass(
                    "credential store",
                    "the token is in .session.db (mode 0600), as chosen; \
                     `confed config --force-keychain` moves it to the OS keyring",
                ));
            } else if session.secret_backend == SecretBackend::Sqlite {
                checks.push(Check::warn(
                    "credential store",
                    "the token is in .session.db (mode 0600): there is no OS keyring on this \
                     machine; prefer CONFED_TOKEN in CI",
                ));
            } else {
                checks.push(Check::pass("credential store", "OS keyring"));
            }
        }
    }
    if keyring_available() {
        checks.push(Check::pass("keyring", "available"));
    } else {
        checks.push(Check::warn("keyring", "no OS keyring on this machine"));
    }

    // .session.db permissions are enforced on open; reaching here means they are fine.
    checks.push(Check::pass("session file permissions", "0600"));

    // --- working tree -----------------------------------------------------
    let scan = worktree::scan(ctx.workspace()?)?;
    let tampered: Vec<&str> =
        scan.pages.iter().filter(|p| !p.tampering.is_empty()).map(|p| p.path.as_str()).collect();
    if tampered.is_empty() {
        checks.push(Check::pass("frontmatter", "no tool-managed blocks were edited"));
    } else {
        checks.push(Check::fail(
            "frontmatter",
            format!(
                "tool-managed frontmatter was edited in: {} — repair with `confed pull --force <page>`",
                tampered.join(", ")
            ),
        ));
    }

    if scan.unreadable.is_empty() {
        checks.push(Check::pass("page files", format!("{} readable", scan.pages.len())));
    } else {
        checks.push(Check::fail(
            "page files",
            format!(
                "{} file(s) could not be parsed: {}",
                scan.unreadable.len(),
                scan.unreadable.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ")
            ),
        ));
    }

    match ctx.workspace()?.state().get_meta("last_fetch_at")? {
        Some(ts) => {
            let age = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|t| {
                    chrono::Utc::now()
                        .signed_duration_since(t.with_timezone(&chrono::Utc))
                        .num_days()
                })
                .unwrap_or(0);
            checks.push(if age >= 7 {
                Check::warn("last fetch", format!("{age} days ago; run `confed fetch`"))
            } else {
                Check::pass("last fetch", ts)
            });
        }
        None => checks.push(Check::warn("last fetch", "never fetched; run `confed fetch`")),
    }

    // --- agent docs -------------------------------------------------------
    use crate::commands::agent_docs;
    let (missing_docs, stale_docs) = agent_docs::audit(&root);
    if missing_docs.is_empty() && stale_docs.is_empty() {
        checks.push(Check::pass(
            "agent docs",
            format!("CLAUDE.md and AGENTS.md, written by confed {}", agent_docs::VERSION),
        ));
    } else {
        let mut problems = Vec::new();
        if !missing_docs.is_empty() {
            problems.push(format!("missing: {}", missing_docs.join(", ")));
        }
        for (name, stamped) in &stale_docs {
            // The contract describes a confed that is no longer the one in the
            // path, so an agent following it may be reading obsolete rules.
            problems.push(format!(
                "{name} was written by {}, this is confed {}",
                agent_docs::writer(stamped),
                agent_docs::VERSION
            ));
        }
        let detail = problems.join("; ");

        if args.fix {
            let base_url = ctx.workspace()?.base_url()?.unwrap_or_default();
            let flavor = ctx.workspace()?.flavor()?.unwrap_or(confed_api::Flavor::Cloud);
            let space = ctx.workspace()?.space_key().unwrap_or_default();
            let rules = agent_docs::current_rules(ctx.workspace()?).unwrap_or(None);
            agent_docs::write(&root, &base_url, flavor, &space, rules.as_ref())?;
            checks.push(Check::warn("agent docs", detail).fixed());
        } else {
            checks.push(Check::warn("agent docs", format!("{detail} (run with --fix)")));
        }
    }
    if let Some(check) = agent_rules_check(ctx.workspace()?, args.fix) {
        checks.push(check);
    }

    // --- orphan inline markers --------------------------------------------
    // Markers in a page that no comment claims, usually left behind when a
    // comment was deleted. Harmless, but they pile up unseen.
    let mut orphaned: Vec<String> = Vec::new();
    for page in ctx.workspace()?.state().all_pages()? {
        let found = confed_core::sync::orphan_markers(ctx.workspace()?, &page.page_id)?;
        if !found.is_empty() {
            orphaned.push(format!("{} ({})", page.local_path, found.len()));
        }
    }
    if orphaned.is_empty() {
        checks.push(Check::pass("inline markers", "every marker belongs to a comment"));
    } else {
        checks.push(Check::warn(
            "inline markers",
            format!(
                "markers with no comment in {}; `confed comment list <page>` shows them \
                 (remove them by editing the page in Confluence)",
                orphaned.join(", ")
            ),
        ));
    }

    // --- converter self-test ---------------------------------------------
    checks.push(converter_self_test());

    // --- server -----------------------------------------------------------
    match ctx.build_client() {
        Err(e) => checks.push(Check::fail("server", e.to_string())),
        Ok(client) => match client.whoami().await {
            Ok(user) => {
                checks.push(Check::pass(
                    "server",
                    format!("{} as {}", client.base_url(), user.display_name),
                ));
                let caps = client.capabilities();
                checks.push(Check::pass(
                    "capabilities",
                    format!(
                        "{}: inline comment create {}, resolve {}",
                        caps.flavor,
                        yes_no(caps.inline_comment_create),
                        yes_no(caps.comment_resolve)
                    ),
                ));
            }
            Err(e) => checks.push(Check::fail("server", e.to_string())),
        },
    }

    Ok(report(ctx, checks))
}

/// Whether `CLAUDE.md` and `AGENTS.md` carry the rules the workspace asks for.
/// `None` when it asks for none and the files carry none: nothing to report.
fn agent_rules_check(ws: &Workspace, fix: bool) -> Option<Check> {
    use crate::commands::agent_docs;
    const NAME: &str = "agent rules";

    let page_id = match ws.rules_page_id() {
        Ok(page_id) => page_id,
        Err(e) => return Some(Check::fail(NAME, e.to_string())),
    };
    let rules = match agent_docs::current_rules(ws) {
        Ok(rules) => rules,
        Err(e) => return Some(Check::fail(NAME, format!("could not read the rules page: {e}"))),
    };
    let drift = agent_docs::rules_drift(ws.root(), rules.as_ref());

    let detail = match (&page_id, &rules) {
        (None, _) if drift.is_empty() => return None,
        (None, _) => format!("rules are still in {}, and no rules page is set", drift.join(", ")),
        (Some(id), None) => {
            // Nothing to copy yet, so nothing `--fix` could do about it.
            let fetched = ws.state().get_remote(id).ok().flatten().is_some_and(|r| !r.deleted);
            return Some(Check::warn(
                NAME,
                if fetched {
                    format!("rules_page_id is {id}, which has not been pulled; run `confed pull`")
                } else {
                    format!(
                        "rules_page_id is {id}, and this workspace has no such page; choose \
                         another with `confed config --set rules_page_id`"
                    )
                },
            ));
        }
        (Some(id), Some(rules)) if drift.is_empty() => {
            return Some(Check::pass(
                NAME,
                format!(
                    "\"{}\" (page {id}, version {}) heads {}",
                    rules.title,
                    rules.version,
                    agent_docs::FILENAMES.join(" and ")
                ),
            ));
        }
        (Some(id), Some(rules)) => format!(
            "the rules in {} are not version {} of \"{}\" (page {id})",
            drift.join(", "),
            rules.version,
            rules.title
        ),
    };

    Some(if !fix {
        Check::warn(NAME, format!("{detail} (run with --fix)"))
    } else {
        match agent_docs::sync_rules(ws) {
            Ok(_) => Check::warn(NAME, detail).fixed(),
            Err(e) => Check::fail(NAME, format!("{detail}; rewriting them failed: {e}")),
        }
    })
}

/// Round-trip a small document so a broken converter is caught here rather than
/// during a push.
fn converter_self_test() -> Check {
    let sample = "<p>Hello <strong>world</strong></p><h2>Section</h2><ul><li>one</li></ul>";
    let opts = confed_converter::ConvertOptions::default();
    match confed_converter::storage_to_markdown(sample, &opts) {
        Err(e) => Check::fail("converter", format!("storage to Markdown failed: {e}")),
        Ok(converted) => {
            if !converted.markdown.contains("**world**") {
                return Check::fail("converter", "inline formatting was lost");
            }
            match confed_converter::markdown_to_storage(&converted.markdown, &opts) {
                Ok(storage) if storage.contains("<strong>") => {
                    Check::pass("converter", "round trip is clean")
                }
                Ok(_) => Check::fail("converter", "formatting was lost on the way back"),
                Err(e) => Check::fail("converter", format!("Markdown to storage failed: {e}")),
            }
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn report(ctx: &Context, checks: Vec<Check>) -> Output {
    let style = &ctx.style;
    let mut human = String::new();

    for check in &checks {
        let marker = match check.status {
            CheckStatus::Pass => style.green("ok  "),
            CheckStatus::Warn => style.yellow("warn"),
            CheckStatus::Fail => style.red("FAIL"),
        };
        let fixed = if check.fix_applied { style.dim(" (fixed)") } else { String::new() };
        let _ = writeln!(human, "{marker} {:<26} {}{}", check.name, check.detail, fixed);
    }

    let failures = checks.iter().filter(|c| c.status == CheckStatus::Fail).count();
    let warnings = checks.iter().filter(|c| c.status == CheckStatus::Warn).count();
    let _ =
        writeln!(human, "\n{} checks, {} failed, {} warnings", checks.len(), failures, warnings);

    let mut output = Output::new(
        json!({
            "checks": checks,
            "failed": failures,
            "warnings": warnings,
            "state_db": STATE_DB_FILENAME,
        }),
        human,
    );
    if failures > 0 {
        output.exit = ExitCode::Error;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_converter_self_test_passes_on_a_healthy_build() {
        let check = converter_self_test();
        assert_eq!(check.status, CheckStatus::Pass, "{}", check.detail);
    }
}
