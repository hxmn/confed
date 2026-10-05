//! The release notes, compiled into the binary.
//!
//! `confed version --changelog` answers from here rather than from a file on
//! disk or the network, so any binary can always explain the release it is —
//! which is what an agent needs when it finds a confed newer than the workspace
//! contract it was handed.

use serde::Serialize;

/// The changelog. A crate may only compile in files inside its own directory —
/// the published package holds nothing else — so this is a copy of the
/// repository's `CHANGELOG.md`, which a test keeps identical. `include_str!`
/// also makes cargo rebuild this crate whenever it changes.
const SOURCE: &str = include_str!("../CHANGELOG.md");

/// The version of this build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One `## [x.y.z] - date` section of the changelog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    /// `0.1.0`, or `Unreleased` for the section at the top.
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// The section body, heading excluded, trimmed of surrounding blank lines.
    pub notes: String,
}

impl Release {
    fn is_unreleased(&self) -> bool {
        self.version.eq_ignore_ascii_case("unreleased")
    }
}

/// Every release section, newest first, in the order the changelog lists them.
pub fn releases() -> Vec<Release> {
    parse(SOURCE)
}

/// The notes for one version, if the changelog records it.
pub fn for_version(version: &str) -> Option<Release> {
    releases().into_iter().find(|r| r.version == version)
}

/// Every release newer than `version`, newest first. An `Unreleased` section
/// counts as newer than everything, but only when it actually says something.
pub fn since(version: &str) -> Vec<Release> {
    newer_than(releases(), version)
}

fn newer_than(releases: Vec<Release>, version: &str) -> Vec<Release> {
    let Some(floor) = parse_version(version) else {
        return Vec::new();
    };
    releases
        .into_iter()
        .filter(|r| match parse_version(&r.version) {
            Some(v) => v > floor,
            None => r.is_unreleased() && !r.notes.is_empty(),
        })
        .collect()
}

/// Whether `s` looks like a version number, so `--since` can reject typos.
pub fn is_version(s: &str) -> bool {
    parse_version(s).is_some()
}

fn parse(source: &str) -> Vec<Release> {
    let mut releases = Vec::new();
    let mut current: Option<(String, Option<String>, Vec<&str>)> = None;

    for line in source.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            if let Some((version, date, body)) = current.take() {
                releases.push(finish(version, date, &body));
            }
            // Prose sections such as "## Versioning policy" are not releases.
            if let Some((version, date)) = parse_heading(heading) {
                current = Some((version, date, Vec::new()));
            }
            continue;
        }
        if let Some((_, _, body)) = current.as_mut() {
            body.push(line);
        }
    }

    if let Some((version, date, body)) = current {
        releases.push(finish(version, date, &body));
    }
    releases
}

fn finish(version: String, date: Option<String>, body: &[&str]) -> Release {
    // The comparison links Keep a Changelog collects at the bottom of the file
    // land inside the last section; they are markup, not release notes.
    let notes: Vec<&str> = body.iter().copied().filter(|l| !is_link_definition(l)).collect();
    Release { version, date, notes: notes.join("\n").trim().to_string() }
}

/// A trailing `[0.1.0]: https://…` reference definition.
fn is_link_definition(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with('[')
        && line.split_once("]: ").is_some_and(|(_, target)| !target.trim().is_empty())
}

/// `[0.1.0] - 2026-08-30`, `[Unreleased]`, or the same without the brackets.
fn parse_heading(heading: &str) -> Option<(String, Option<String>)> {
    let heading = heading.trim();
    let (version, tail) = match heading.strip_prefix('[') {
        Some(rest) => rest.split_once(']')?,
        None => heading.split_once(" - ").unwrap_or((heading, "")),
    };

    let version = version.trim();
    let is_release = version.eq_ignore_ascii_case("unreleased") || is_version(version);
    if !is_release {
        return None;
    }

    let date = tail.trim().trim_start_matches('-').trim();
    Some((version.to_string(), (!date.is_empty()).then(|| date.to_string())))
}

/// Enough of semver to order releases: pre-release and build metadata are
/// dropped, so `0.2.0-rc.1` and `0.2.0` compare equal.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;

    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    let patch = parts.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packaged copy must be the repository's changelog. Skipped where
    /// there is no repository around it — a crate unpacked from crates.io.
    #[test]
    fn the_packaged_changelog_is_the_repository_changelog() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../CHANGELOG.md");
        let Ok(repository) = std::fs::read_to_string(&root) else { return };
        assert!(
            repository == SOURCE,
            "crates/confed-cli/CHANGELOG.md is out of date: `cp CHANGELOG.md crates/confed-cli/` \
             (or `make changelog`) after editing the changelog"
        );
    }

    const SAMPLE: &str = "\
# Changelog

Preamble prose.

## Versioning policy

Not a release.

## [Unreleased]

## [0.2.0] - 2026-09-01

### Added

- A thing.

## [0.1.0] - 2026-08-30

### Added

- The first thing.

[0.1.0]: https://example.invalid/releases/tag/v0.1.0
";

    #[test]
    fn only_version_headings_become_releases() {
        let releases = parse(SAMPLE);
        let versions: Vec<&str> = releases.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, ["Unreleased", "0.2.0", "0.1.0"]);
    }

    #[test]
    fn a_section_carries_its_date_and_body_without_the_heading() {
        let release = parse(SAMPLE).into_iter().find(|r| r.version == "0.2.0").unwrap();
        assert_eq!(release.date.as_deref(), Some("2026-09-01"));
        assert_eq!(release.notes, "### Added\n\n- A thing.");

        let unreleased = parse(SAMPLE).into_iter().next().unwrap();
        assert_eq!(unreleased.date, None);
        assert!(unreleased.notes.is_empty(), "an empty section has no notes");

        let oldest = parse(SAMPLE).into_iter().last().unwrap();
        assert_eq!(oldest.notes, "### Added\n\n- The first thing.", "link definitions are markup");
    }

    #[test]
    fn versions_order_numerically_not_lexically() {
        assert!(parse_version("0.10.0") > parse_version("0.9.0"));
        assert_eq!(parse_version("1.2.3-rc.1"), parse_version("1.2.3"));
        assert_eq!(parse_version("v0.1"), Some((0, 1, 0)));
        assert_eq!(parse_version("0.1.2.3"), None);
        assert_eq!(parse_version("today"), None);
    }

    #[test]
    fn since_returns_what_an_upgrade_brought() {
        let newer: Vec<String> =
            newer_than(parse(SAMPLE), "0.1.0").into_iter().map(|r| r.version).collect();
        // The empty Unreleased section is not news.
        assert_eq!(newer, ["0.2.0"]);

        assert!(newer_than(parse(SAMPLE), "0.2.0").is_empty(), "nothing follows the newest entry");
    }

    #[test]
    fn since_an_unknown_version_is_empty_rather_than_everything() {
        assert!(since("not-a-version").is_empty());
        assert!(!is_version("not-a-version"));
        assert!(is_version("0.1.0"));
    }

    #[test]
    fn the_embedded_changelog_documents_this_build() {
        // Releasing means bumping the version *and* writing its notes; this
        // test is what makes forgetting either one a build failure.
        let release = for_version(VERSION).unwrap_or_else(|| {
            panic!("CHANGELOG.md has no `## [{VERSION}]` section for this build")
        });
        assert!(
            !release.notes.is_empty(),
            "the changelog section for {VERSION} is empty; describe the release"
        );
        assert!(release.date.is_some(), "the {VERSION} section needs a release date");
    }

    #[test]
    fn no_released_section_claims_to_be_newer_than_this_build() {
        let newer: Vec<String> =
            since(VERSION).into_iter().filter(|r| !r.is_unreleased()).map(|r| r.version).collect();
        assert!(
            newer.is_empty(),
            "CHANGELOG.md documents {newer:?} but Cargo.toml still says {VERSION}"
        );
    }
}
