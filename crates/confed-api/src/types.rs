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
    /// Creating inline comments through the API. On Data Center this is the
    /// private `rest/inlinecomments/1.0` plugin API, and creating one adds a
    /// page version.
    pub inline_comment_create: bool,
    /// Resolving comments through the API (on Data Center, inline threads only).
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
            inline_comment_create: true,
            comment_resolve: true,
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
    /// `current` or `archived`, when the server says.
    #[serde(default)]
    pub status: Option<String>,
}

/// How a page's markup identifies a person. Which one appears depends on the
/// flavor and on how old the content is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserReference {
    /// Cloud, and the only identifier Cloud exposes.
    AccountId(String),
    /// Data Center's opaque key, what `ri:userkey` carries.
    UserKey(String),
    /// Older Data Center content.
    Username(String),
}

impl UserReference {
    /// The value as it appears in the markup, which is also how confed keys its
    /// cache of resolved people.
    pub fn value(&self) -> &str {
        match self {
            UserReference::AccountId(v)
            | UserReference::UserKey(v)
            | UserReference::Username(v) => v,
        }
    }

    /// The `ri:user` attribute this reference came from, without the prefix.
    pub fn attribute(&self) -> &'static str {
        match self {
            UserReference::AccountId(_) => "account-id",
            UserReference::UserKey(_) => "userkey",
            UserReference::Username(_) => "username",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub account_id: Option<String>,
    /// Data Center's stable user key — what a mention in storage carries.
    #[serde(default)]
    pub user_key: Option<String>,
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

/// Where [`crate::ConfluenceClient::move_page_to`] puts a page, relative to its
/// target: as the target's last child, or as its sibling before or after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MovePosition {
    Append,
    Before,
    After,
}

impl MovePosition {
    /// The path segment the move endpoint takes.
    pub fn as_str(self) -> &'static str {
        match self {
            MovePosition::Append => "append",
            MovePosition::Before => "before",
            MovePosition::After => "after",
        }
    }
}

/// What a capability probe found out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Probe {
    /// The server answered the way a server with the API does.
    Available,
    /// Data Center without the REST move endpoint: confed falls back to the
    /// web UI's move action, which is not a documented API.
    WebAction,
    /// Data Center without the REST move endpoint, whose web move action
    /// refuses scripted requests: its XSRF check passes neither
    /// `X-Atlassian-Token: no-check` nor a session token.
    WebActionRefused,
    /// The server answered the way a server without the API does.
    Unavailable,
    /// The answer said neither: a proxy, an outage, or an unfamiliar server.
    Unknown,
}

impl Probe {
    pub fn as_str(self) -> &'static str {
        match self {
            Probe::Available => "available",
            Probe::WebAction => "web_action",
            Probe::WebActionRefused => "web_action_refused",
            Probe::Unavailable => "unavailable",
            Probe::Unknown => "unknown",
        }
    }
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
    /// When creating: which occurrence of `text` on the page is meant, and how
    /// many there are. `None` means the text is unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_count: Option<usize>,
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

/// Where something a page carries beside its body — its comments, or its
/// attachments — changed recently, as far as the server can say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentActivity {
    /// The pages with one added or edited in the window.
    Pages(Vec<PageId>),
    /// More changes than the server will list: any page may be among them.
    Unbounded,
}

/// [`ContentActivity`], by the name it had while comments were all it described.
pub type CommentActivity = ContentActivity;

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
    /// The current version, when the server was asked to expand it.
    pub version: Option<u32>,
    /// Who last changed the page, when the server was asked to expand it.
    pub author: Option<String>,
    /// When it last changed. Search reports this even without an expansion.
    pub when: Option<String>,
}

/// A space for [`crate::ConfluenceClient::create_space`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSpace {
    pub key: String,
    pub name: String,
    /// Plain text: the only representation both flavors take.
    pub description: Option<String>,
    /// Visible to its creator only.
    pub private: bool,
}

/// Why a server refused to create a space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceRefusal {
    /// A space has the key already — current, archived or personal.
    KeyTaken,
    /// The server does not take the key as a space key.
    InvalidKey,
    /// The account may not create spaces.
    NoPermission,
}

impl SpaceRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            SpaceRefusal::KeyTaken => "key_taken",
            SpaceRefusal::InvalidKey => "invalid_key",
            SpaceRefusal::NoPermission => "no_permission",
        }
    }
}

/// A user for [`crate::ConfluenceClient::create_user`]. Without a password
/// the server mails the person a link to set one, which `notify` asks for.
#[derive(Clone, Debug)]
pub struct NewUser {
    pub username: String,
    pub full_name: String,
    pub email: String,
    pub password: Option<crate::secret::Secret>,
    pub notify: bool,
}

/// The profile fields an administrator may change; `None` leaves one alone.
#[derive(Clone, Debug, Default)]
pub struct UserUpdate {
    pub full_name: Option<String>,
    pub email: Option<String>,
}

/// A user group. Data Center knows a group by its name; Cloud also gives it an
/// id, which its write endpoints take.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub id: Option<String>,
}

/// Who a space permission or role is for, as the caller names them: a user by
/// username on Data Center and by account id on Cloud, a group by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    User(String),
    Group(String),
    Anonymous,
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Subject::User(user) => write!(f, "user {user}"),
            Subject::Group(group) => write!(f, "group {group}"),
            Subject::Anonymous => f.write_str("anonymous users"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    User,
    Group,
    Anonymous,
    /// Anything else a server reports (Cloud's access classes).
    Other,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::User => "user",
            PrincipalKind::Group => "group",
            PrincipalKind::Anonymous => "anonymous",
            PrincipalKind::Other => "other",
        }
    }
}

/// Who a permission or role is for, as the server reports it: an id (the
/// userkey or account id, or a Cloud group id; a Data Center group's name), a
/// name to show when one is known, and for a Data Center user the username,
/// which is how a [`Subject`] names them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub id: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self { kind: PrincipalKind::Anonymous, id: None, name: None, username: None }
    }

    /// The name when known, else the id.
    pub fn label(&self) -> String {
        match (&self.name, &self.id) {
            (Some(name), _) => name.clone(),
            (None, Some(id)) => id.clone(),
            (None, None) => self.kind.as_str().to_string(),
        }
    }
}

/// One thing a space permission allows: an operation on a kind of content,
/// written `target:key` (`space:read`, `page:create`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SpaceOperation {
    pub target: String,
    pub key: String,
}

/// The operations Data Center's space permission API takes (9.1+).
pub const DC_SPACE_OPERATIONS: &[(&str, &str)] = &[
    ("space", "read"),
    ("space", "administer"),
    ("space", "export"),
    ("space", "restrict"),
    ("space", "delete_own"),
    ("space", "delete_mail"),
    ("page", "create"),
    ("page", "delete"),
    ("blogpost", "create"),
    ("blogpost", "delete"),
    ("comment", "create"),
    ("comment", "delete"),
    ("attachment", "create"),
    ("attachment", "delete"),
];

/// The operations Cloud's v1 space permission API takes. `space:delete` is
/// "delete own", `space:restrict_content` "add/delete restrictions".
pub const CLOUD_SPACE_OPERATIONS: &[(&str, &str)] = &[
    ("space", "read"),
    ("space", "administer"),
    ("space", "export"),
    ("space", "restrict_content"),
    ("space", "delete"),
    ("page", "create"),
    ("page", "delete"),
    ("page", "archive"),
    ("blogpost", "create"),
    ("blogpost", "delete"),
    ("comment", "create"),
    ("comment", "delete"),
    ("attachment", "create"),
    ("attachment", "delete"),
];

/// Text that is not `target:key`, as [`SpaceOperation::parse`] takes it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not an operation: write it as target:key, e.g. space:read")]
pub struct InvalidOperation(pub String);

impl SpaceOperation {
    pub fn new(target: impl Into<String>, key: impl Into<String>) -> Self {
        Self { target: target.into(), key: key.into() }
    }

    /// `target:key`, as the command line writes it.
    pub fn parse(text: &str) -> Result<Self, InvalidOperation> {
        match text.trim().split_once(':') {
            Some((target, key)) if !target.trim().is_empty() && !key.trim().is_empty() => {
                Ok(Self::new(target.trim().to_ascii_lowercase(), key.trim().to_ascii_lowercase()))
            }
            _ => Err(InvalidOperation(text.to_string())),
        }
    }

    /// The operations `flavor`'s space permission API accepts.
    pub fn valid_for(flavor: Flavor) -> &'static [(&'static str, &'static str)] {
        match flavor {
            Flavor::Cloud => CLOUD_SPACE_OPERATIONS,
            Flavor::DataCenter => DC_SPACE_OPERATIONS,
        }
    }

    pub fn is_valid_for(&self, flavor: Flavor) -> bool {
        Self::valid_for(flavor).iter().any(|(t, k)| *t == self.target && *k == self.key)
    }
}

impl fmt::Display for SpaceOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.target, self.key)
    }
}

/// One permission granted in a space. `id` is Cloud's, which revoking it takes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpacePermission {
    pub id: Option<String>,
    pub principal: Principal,
    pub operation: SpaceOperation,
}

/// How a site grants space access. Data Center, and Cloud before roles, grant
/// permissions one by one; a Cloud site on roles assigns a role instead, and
/// one in transition takes both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleMode {
    Permissions,
    Transition,
    Roles,
}

impl RoleMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleMode::Permissions => "permissions",
            RoleMode::Transition => "transition",
            RoleMode::Roles => "roles",
        }
    }

    pub fn grants_permissions(self) -> bool {
        matches!(self, RoleMode::Permissions | RoleMode::Transition)
    }

    pub fn assigns_roles(self) -> bool {
        matches!(self, RoleMode::Roles | RoleMode::Transition)
    }
}

/// A role a Cloud space can assign: a named set of permissions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpaceRole {
    pub id: String,
    pub name: String,
    /// `system` or `custom`.
    pub kind: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoleAssignment {
    pub principal: Principal,
    pub role_id: String,
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
        assert!(dc.inline_comment_create, "through DC's inline-comment plugin API");
        assert!(dc.comment_resolve, "inline threads, through the same API");
        assert!(!dc.adf);
        assert!(Capabilities::cloud().inline_comment_create);
    }

    #[test]
    fn operations_are_written_target_colon_key() {
        let op = SpaceOperation::parse(" Page:Create ").unwrap();
        assert_eq!(op, SpaceOperation::new("page", "create"));
        assert_eq!(op.to_string(), "page:create");
        assert!(SpaceOperation::parse("read").is_err());
        assert!(SpaceOperation::parse("space:").is_err());
    }

    #[test]
    fn each_flavor_accepts_its_own_operations() {
        let restrict = SpaceOperation::new("space", "restrict");
        assert!(restrict.is_valid_for(Flavor::DataCenter));
        assert!(!restrict.is_valid_for(Flavor::Cloud));
        let archive = SpaceOperation::new("page", "archive");
        assert!(archive.is_valid_for(Flavor::Cloud));
        assert!(!archive.is_valid_for(Flavor::DataCenter));
    }
}
