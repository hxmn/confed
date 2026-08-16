//! Flavor-independent domain types. Both the Cloud (v2) and Data Center (v1)
//! clients map their wire formats onto these.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }
    };
}

id_newtype!(PageId);
id_newtype!(AttachmentId);
id_newtype!(CommentId);

/// Confluence flavor. Cloud speaks REST v2, Data Center speaks REST v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Flavor {
    Cloud,
    DataCenter,
}

impl Flavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Flavor::Cloud => "cloud",
            Flavor::DataCenter => "datacenter",
        }
    }
}

impl fmt::Display for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Flavor {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "cloud" => Ok(Flavor::Cloud),
            "dc" | "datacenter" | "data-center" | "server" => Ok(Flavor::DataCenter),
            other => Err(format!("unknown flavor `{other}` (expected `cloud` or `dc`)")),
        }
    }
}

/// What the connected instance can do. Commands branch on capabilities, never on
/// [`Flavor`] directly, so gaps are reported uniformly (exit code 9).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    pub flavor: Flavor,
    /// Creating inline comments through the API (Cloud only).
    pub inline_comment_create: bool,
    /// Resolving comments through the API (Cloud only).
    pub comment_resolve: bool,
    /// Atlassian Document Format bodies available (Cloud only; confed still syncs storage).
    pub adf: bool,
    /// Default in-flight request ceiling for this flavor.
    pub max_request_concurrency: usize,
}

impl Capabilities {
    pub fn cloud() -> Self {
        Self {
            flavor: Flavor::Cloud,
            inline_comment_create: true,
            comment_resolve: true,
            adf: true,
            max_request_concurrency: 4,
        }
    }

    pub fn data_center() -> Self {
        Self {
            flavor: Flavor::DataCenter,
            inline_comment_create: false,
            comment_resolve: false,
            adf: false,
            max_request_concurrency: 8,
        }
    }
}

/// A space identity. v2 Cloud endpoints need the numeric id; v1 DC uses the key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceId {
    pub key: String,
    pub numeric: Option<String>,
}

impl SpaceId {
    pub fn from_key(key: impl Into<String>) -> Self {
        Self { key: key.into(), numeric: None }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Space {
    pub id: SpaceId,
    pub name: String,
    pub kind: Option<String>,
    pub homepage_id: Option<PageId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub account_id: Option<String>,
    pub username: Option<String>,
    pub display_name: String,
    pub email: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageStatus {
    Current,
    Archived,
    Draft,
    Trashed,
    Deleted,
}

impl PageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PageStatus::Current => "current",
            PageStatus::Archived => "archived",
            PageStatus::Draft => "draft",
            PageStatus::Trashed => "trashed",
            PageStatus::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "archived" => PageStatus::Archived,
            "draft" => PageStatus::Draft,
            "trashed" => PageStatus::Trashed,
            "deleted" => PageStatus::Deleted,
            _ => PageStatus::Current,
        }
    }
}

/// Everything about a page except its body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageSummary {
    pub id: PageId,
    pub title: String,
    pub space_key: String,
    pub parent_id: Option<PageId>,
    /// Sibling ordering when the server exposes it.
    pub position: Option<i64>,
    pub version: u32,
    pub status: PageStatus,
    pub labels: Vec<String>,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    #[serde(flatten)]
    pub summary: PageSummary,
    /// Confluence storage format (XHTML). The merge base of record.
    pub body_storage: String,
}

#[derive(Clone, Debug, Default)]
pub struct NewPage {
    pub space: SpaceId,
    pub title: String,
    pub parent_id: Option<PageId>,
    pub body_storage: String,
    pub labels: Vec<String>,
}

/// An optimistic-concurrency update: `version` is the *new* version number,
/// which the server accepts only if it is exactly `base + 1`.
#[derive(Clone, Debug)]
pub struct PageUpdate {
    pub title: String,
    pub body_storage: Option<String>,
    pub version: u32,
    pub parent_id: Option<PageId>,
    pub status: Option<PageStatus>,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Append,
    Before(u64),
    After(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyFormat {
    Storage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub page_id: PageId,
    pub filename: String,
    pub media_type: Option<String>,
    pub file_size: Option<u64>,
    pub version: u32,
    /// Server-relative or absolute download path.
    pub download_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommentKind {
    Footer,
    Inline,
}

/// Where an inline comment attaches in the page text.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InlineAnchor {
    /// The highlighted text itself.
    pub text: String,
    #[serde(default)]
    pub context_before: String,
    #[serde(default)]
    pub context_after: String,
    /// Confluence's own marker reference, when the API exposes it.
    #[serde(default)]
    pub marker_ref: Option<String>,
    /// True when the anchor text can no longer be found in the body.
    #[serde(default)]
    pub orphaned: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub page_id: PageId,
    pub parent_comment_id: Option<CommentId>,
    pub kind: CommentKind,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub body_storage: String,
    pub resolved: bool,
    /// Present for [`CommentKind::Inline`].
    pub anchor: Option<InlineAnchor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionInfo {
    pub number: u32,
    pub author: Option<String>,
    pub when: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub page_id: PageId,
    pub title: String,
    pub space_key: Option<String>,
    pub url: String,
    pub excerpt: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_parsing_accepts_common_spellings() {
        for s in ["cloud", "CLOUD"] {
            assert_eq!(s.parse::<Flavor>().unwrap(), Flavor::Cloud);
        }
        for s in ["dc", "datacenter", "data-center", "server"] {
            assert_eq!(s.parse::<Flavor>().unwrap(), Flavor::DataCenter);
        }
        assert!("sharepoint".parse::<Flavor>().is_err());
    }

    #[test]
    fn capability_gaps_match_the_design() {
        let dc = Capabilities::data_center();
        assert!(!dc.inline_comment_create, "DC has no inline comment create API");
        assert!(!dc.comment_resolve, "DC has no comment resolve API");
        assert!(Capabilities::cloud().inline_comment_create);
    }
}
