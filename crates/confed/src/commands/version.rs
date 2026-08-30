//! `confed version` — what this binary is, and what changed in it.
//!
//! Needs neither a workspace nor credentials: an agent that finds an unfamiliar
//! confed must be able to ask what it is before doing anything else.

use crate::changelog::{self, Release};
use crate::cli::VersionArgs;
use crate::output::{Output, JSON_SCHEMA_VERSION};
use confed_core::error::{ConfedError, Result};
use serde_json::json;
use std::fmt::Write as _;

pub fn run(args: &VersionArgs) -> Result<Output> {
    let releases = selected_releases(args)?;

    let result = json!({
        "version": changelog::VERSION,
        "json_schema": JSON_SCHEMA_VERSION,
        "state_schema": confed_core::state::SCHEMA_VERSION,
        "changelog": releases,
    });

    let mut human = format!("confed {}\n", changelog::VERSION);
    let _ = writeln!(human, "json envelope schema {JSON_SCHEMA_VERSION}");
    let _ = writeln!(human, ".state.db schema {}", confed_core::state::SCHEMA_VERSION);

    if args.changelog {
        for release in &releases {
            let _ = write!(human, "\n{}\n", heading(release));
            if !release.notes.is_empty() {
                let _ = writeln!(human, "\n{}", release.notes);
            }
        }
    }

    let mut output = Output::new(result, human);
    if args.changelog && releases.is_empty() {
        output = output.warn(match &args.since {
            Some(since) => format!("nothing newer than {since} is recorded in this build"),
            None => format!("this build's changelog has no entry for {}", changelog::VERSION),
        });
    }
    Ok(output)
}

/// The notes for this build, or every release after the `--since` version.
fn selected_releases(args: &VersionArgs) -> Result<Vec<Release>> {
    if !args.changelog {
        return Ok(Vec::new());
    }
    match &args.since {
        Some(since) => {
            if !changelog::is_version(since) {
                return Err(ConfedError::usage(format!(
                    "`--since {since}` is not a version number, e.g. `--since 0.1.0`"
                )));
            }
            Ok(changelog::since(since))
        }
        None => Ok(changelog::for_version(changelog::VERSION).into_iter().collect()),
    }
}

fn heading(release: &Release) -> String {
    match &release.date {
        Some(date) => format!("## {} — {date}", release.version),
        None => format!("## {}", release.version),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(changelog: bool, since: Option<&str>) -> VersionArgs {
        VersionArgs { changelog, since: since.map(str::to_string) }
    }

    #[test]
    fn the_bare_command_reports_all_three_versions() {
        let out = run(&args(false, None)).unwrap();
        assert_eq!(out.result["version"], json!(changelog::VERSION));
        assert_eq!(out.result["json_schema"], json!(JSON_SCHEMA_VERSION));
        assert_eq!(out.result["state_schema"], json!(confed_core::state::SCHEMA_VERSION));
        assert!(out.human.starts_with(&format!("confed {}", changelog::VERSION)));
        assert!(out.result["changelog"].as_array().unwrap().is_empty());
    }

    #[test]
    fn changelog_prints_the_notes_for_this_build() {
        let out = run(&args(true, None)).unwrap();
        let entries = out.result["changelog"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "one section: the one being built");
        assert_eq!(entries[0]["version"], json!(changelog::VERSION));
        assert!(
            out.human.contains(&format!("## {}", changelog::VERSION)),
            "the release heading is missing from:\n{}",
            out.human
        );
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn since_the_current_version_reports_nothing_rather_than_failing() {
        let out = run(&args(true, Some(changelog::VERSION))).unwrap();
        assert!(out.result["changelog"].as_array().unwrap().is_empty());
        assert_eq!(out.exit, confed_core::ExitCode::Ok);
        assert_eq!(out.warnings.len(), 1, "an empty answer is explained");
    }

    #[test]
    fn since_something_that_is_not_a_version_is_a_usage_error() {
        let err = run(&args(true, Some("yesterday"))).unwrap_err();
        assert_eq!(err.exit_code(), confed_core::ExitCode::Usage);
    }

    #[test]
    fn since_the_beginning_includes_this_build() {
        let out = run(&args(true, Some("0.0.0"))).unwrap();
        let versions: Vec<&str> = out.result["changelog"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["version"].as_str().unwrap())
            .collect();
        assert!(versions.contains(&changelog::VERSION), "{versions:?}");
    }
}
