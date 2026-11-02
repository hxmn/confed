//! Administration on Cloud: groups through v1 (`rest/api/group`, by id), and
//! space access either as v1 permissions or as v2 space roles, whichever the
//! site's role mode says. Users are not Confluence's to create on Cloud: an
//! Atlassian account belongs to the organization, and only the Organization
//! API (admin.atlassian.com) creates or removes one.

use crate::{encode, v2, CloudClient};
use confed_api::client::ConfluenceClient;
use confed_api::error::{ApiError, ApiResult};
use confed_api::paginate::{collect_cursor, collect_offset};
use confed_api::types::*;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

/// v1 group collections take at most 200 per request.
const GROUP_PAGE_SIZE: usize = 200;

/// Why a user cannot be managed with a Confluence credential on Cloud.
pub(crate) fn organization_only(flavor: Flavor, operation: &str) -> ApiError {
    ApiError::unsupported(
        flavor,
        format!(
            "{operation}: Atlassian accounts belong to the organization, so they are managed in \
             admin.atlassian.com or through the Organization API, not through Confluence"
        ),
    )
}

#[derive(Debug, Deserialize)]
struct GroupDto {
    name: String,
    #[serde(default)]
    id: Option<String>,
}

impl GroupDto {
    fn into_domain(self) -> Group {
        Group { name: self.name, id: self.id }
    }
}

#[derive(Debug, Default, Deserialize)]
struct PrincipalDto {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationDto {
    key: String,
    target_type: String,
}

#[derive(Debug, Deserialize)]
struct PermissionDto {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    principal: PrincipalDto,
    operation: OperationDto,
}

#[derive(Debug, Deserialize)]
struct RoleModeDto {
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RoleDto {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignmentPrincipalDto {
    #[serde(default)]
    principal_type: Option<String>,
    #[serde(default)]
    principal_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssignmentDto {
    #[serde(default)]
    principal: AssignmentPrincipalDto,
    #[serde(default)]
    role_id: Option<String>,
}

/// v1 names an anonymous user `anonymous`, as a user.
const ANONYMOUS: &str = "anonymous";

fn kind_of(raw: Option<&str>, id: Option<&str>) -> PrincipalKind {
    if id == Some(ANONYMOUS) {
        return PrincipalKind::Anonymous;
    }
    match raw.map(str::to_ascii_lowercase).as_deref() {
        Some("user") => PrincipalKind::User,
        Some("group") => PrincipalKind::Group,
        Some("anonymous") => PrincipalKind::Anonymous,
        _ => PrincipalKind::Other,
    }
}

/// Names for the ids a permission listing gives: groups from one listing,
/// people one lookup each, remembered.
struct Names {
    groups: HashMap<String, String>,
    users: HashMap<String, Option<String>>,
}

impl CloudClient {
    async fn group_id(&self, name: &str) -> ApiResult<String> {
        let found: Vec<GroupDto> = collect_offset(
            &self.http,
            "rest/api/group/picker",
            &[("query", name.to_string())],
            GROUP_PAGE_SIZE,
            None,
        )
        .await?;
        found
            .into_iter()
            .find(|g| g.name == name)
            .and_then(|g| g.id)
            .ok_or_else(|| ApiError::NotFound(format!("group {name}")))
    }

    async fn names(&self) -> ApiResult<Names> {
        let groups = self
            .admin_list_groups(None)
            .await?
            .into_iter()
            .filter_map(|g| Some((g.id?, g.name)))
            .collect();
        Ok(Names { groups, users: HashMap::new() })
    }

    async fn principal(
        &self,
        names: &mut Names,
        kind: PrincipalKind,
        id: Option<String>,
    ) -> Principal {
        let name = match (kind, &id) {
            (PrincipalKind::Group, Some(id)) => names.groups.get(id).cloned(),
            (PrincipalKind::User, Some(id)) => {
                if !names.users.contains_key(id) {
                    let found = self
                        .lookup_user(&UserReference::AccountId(id.clone()))
                        .await
                        .ok()
                        .map(|u| u.display_name);
                    names.users.insert(id.clone(), found);
                }
                names.users[id].clone()
            }
            _ => None,
        };
        match kind {
            PrincipalKind::Anonymous => Principal::anonymous(),
            _ => Principal { kind, id, name },
        }
    }

    /// `subject` as v1 permissions name it: a type and an identifier.
    async fn permission_subject(&self, subject: &Subject) -> ApiResult<(&'static str, String)> {
        Ok(match subject {
            Subject::User(account) => ("user", account.clone()),
            Subject::Group(name) => ("group", self.group_id(name).await?),
            Subject::Anonymous => ("user", ANONYMOUS.to_string()),
        })
    }

    async fn numeric(&self, space_key: &str) -> ApiResult<String> {
        self.numeric_space_id(&SpaceId::from_key(space_key)).await
    }

    pub(crate) async fn admin_user_groups(&self, account_id: &str) -> ApiResult<Vec<Group>> {
        let raw: Vec<GroupDto> = collect_offset(
            &self.http,
            "rest/api/user/memberof",
            &[("accountId", account_id.to_string())],
            GROUP_PAGE_SIZE,
            None,
        )
        .await?;
        Ok(raw.into_iter().map(GroupDto::into_domain).collect())
    }

    pub(crate) async fn admin_list_groups(&self, limit: Option<usize>) -> ApiResult<Vec<Group>> {
        let raw: Vec<GroupDto> =
            collect_offset(&self.http, "rest/api/group", &[], GROUP_PAGE_SIZE, limit).await?;
        Ok(raw.into_iter().map(GroupDto::into_domain).collect())
    }

    pub(crate) async fn admin_create_group(&self, name: &str) -> ApiResult<Group> {
        let created: GroupDto =
            self.http.post_json("rest/api/group", &json!({ "name": name })).await?;
        Ok(created.into_domain())
    }

    pub(crate) async fn admin_delete_group(&self, name: &str) -> ApiResult<()> {
        let id = self.group_id(name).await?;
        self.http.delete(&format!("rest/api/group/by-id?id={}", encode(&id))).await
    }

    pub(crate) async fn admin_group_members(
        &self,
        group: &str,
        limit: Option<usize>,
    ) -> ApiResult<Vec<User>> {
        let id = self.group_id(group).await?;
        let raw: Vec<v2::CurrentUser> = collect_offset(
            &self.http,
            &format!("rest/api/group/{}/membersByGroupId", encode(&id)),
            &[],
            GROUP_PAGE_SIZE,
            limit,
        )
        .await?;
        Ok(raw.into_iter().map(v2::CurrentUser::into_domain).collect())
    }

    pub(crate) async fn admin_membership(
        &self,
        group: &str,
        account_id: &str,
        add: bool,
    ) -> ApiResult<()> {
        let id = encode(&self.group_id(group).await?);
        if add {
            let _: serde_json::Value = self
                .http
                .post_json(
                    &format!("rest/api/group/userByGroupId?groupId={id}"),
                    &json!({ "accountId": account_id }),
                )
                .await?;
            Ok(())
        } else {
            self.http
                .delete(&format!(
                    "rest/api/group/userByGroupId?groupId={id}&accountId={}",
                    encode(account_id)
                ))
                .await
        }
    }

    /// Sites without role-based access control have no such endpoint, and
    /// grant permissions.
    pub(crate) async fn admin_role_mode(&self) -> ApiResult<RoleMode> {
        match self.http.get_json::<RoleModeDto>("api/v2/space-role-mode", &[]).await {
            Ok(answer) => Ok(match answer.mode.as_deref() {
                Some("ROLES") => RoleMode::Roles,
                Some("ROLES_TRANSITION") => RoleMode::Transition,
                _ => RoleMode::Permissions,
            }),
            Err(ApiError::NotFound(_)) => Ok(RoleMode::Permissions),
            Err(e) => Err(e),
        }
    }

    async fn raw_permissions(&self, space_key: &str) -> ApiResult<Vec<PermissionDto>> {
        let numeric = self.numeric(space_key).await?;
        collect_cursor(
            &self.http,
            &format!("api/v2/spaces/{numeric}/permissions"),
            &[("limit", "250".to_string())],
            None,
        )
        .await
    }

    pub(crate) async fn admin_space_permissions(
        &self,
        space_key: &str,
    ) -> ApiResult<Vec<SpacePermission>> {
        let raw = self.raw_permissions(space_key).await?;
        let mut names = self.names().await?;
        let mut out = Vec::with_capacity(raw.len());
        for permission in raw {
            let kind =
                kind_of(permission.principal.kind.as_deref(), permission.principal.id.as_deref());
            out.push(SpacePermission {
                id: permission.id,
                principal: self.principal(&mut names, kind, permission.principal.id).await,
                operation: SpaceOperation::new(
                    permission.operation.target_type,
                    permission.operation.key,
                ),
            });
        }
        Ok(out)
    }

    /// One v1 permission per operation: v1 grants them one at a time.
    pub(crate) async fn admin_grant(
        &self,
        space_key: &str,
        subject: &Subject,
        operations: &[SpaceOperation],
    ) -> ApiResult<()> {
        let (kind, identifier) = self.permission_subject(subject).await?;
        let path = format!("rest/api/space/{}/permission", encode(space_key));
        for op in operations {
            let body = json!({
                "subject": { "type": kind, "identifier": identifier },
                "operation": { "key": op.key, "target": op.target },
            });
            let _: serde_json::Value = self.http.post_json(&path, &body).await?;
        }
        Ok(())
    }

    /// Find every permission to take away first, so that one the subject does
    /// not have stops the revoke before anything is deleted.
    pub(crate) async fn admin_revoke(
        &self,
        space_key: &str,
        subject: &Subject,
        operations: &[SpaceOperation],
    ) -> ApiResult<()> {
        let (_, identifier) = self.permission_subject(subject).await?;
        let raw = self.raw_permissions(space_key).await?;
        let mut ids = Vec::new();
        for op in operations {
            let found = raw.iter().find(|p| {
                p.principal.id.as_deref() == Some(identifier.as_str())
                    && p.operation.key == op.key
                    && p.operation.target_type == op.target
            });
            match found.and_then(|p| p.id.clone()) {
                Some(id) => ids.push(id),
                None => {
                    return Err(ApiError::NotFound(format!(
                        "{subject} has no {op} permission in space {space_key}"
                    )))
                }
            }
        }
        for id in ids {
            self.http
                .delete(&format!("rest/api/space/{}/permission/{}", encode(space_key), encode(&id)))
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn admin_space_roles(&self, space_key: &str) -> ApiResult<Vec<SpaceRole>> {
        let numeric = self.numeric(space_key).await?;
        let raw: Vec<RoleDto> = collect_cursor(
            &self.http,
            "api/v2/space-roles",
            &[("space-id", numeric), ("limit", "250".to_string())],
            None,
        )
        .await?;
        Ok(raw
            .into_iter()
            .map(|r| SpaceRole {
                name: r.name.unwrap_or_else(|| r.id.clone()),
                id: r.id,
                kind: r.kind.map(|k| k.to_ascii_lowercase()),
                description: r.description,
            })
            .collect())
    }

    pub(crate) async fn admin_role_assignments(
        &self,
        space_key: &str,
    ) -> ApiResult<Vec<RoleAssignment>> {
        let numeric = self.numeric(space_key).await?;
        let raw: Vec<AssignmentDto> = collect_cursor(
            &self.http,
            &format!("api/v2/spaces/{numeric}/role-assignments"),
            &[("limit", "250".to_string())],
            None,
        )
        .await?;
        let mut names = self.names().await?;
        let mut out = Vec::with_capacity(raw.len());
        for assignment in raw {
            let Some(role_id) = assignment.role_id else { continue };
            let kind = kind_of(
                assignment.principal.principal_type.as_deref(),
                assignment.principal.principal_id.as_deref(),
            );
            let principal =
                self.principal(&mut names, kind, assignment.principal.principal_id).await;
            out.push(RoleAssignment { principal, role_id });
        }
        Ok(out)
    }

    /// The payload is an array: one entry, with no `roleId` to take a role away.
    pub(crate) async fn admin_set_role(
        &self,
        space_key: &str,
        subject: &Subject,
        role_id: Option<&str>,
    ) -> ApiResult<()> {
        let (kind, id) = match subject {
            Subject::User(account) => ("USER", account.clone()),
            Subject::Group(name) => ("GROUP", self.group_id(name).await?),
            Subject::Anonymous => {
                return Err(ApiError::unsupported(
                    self.flavor(),
                    "assign a space role to anonymous users (grant them permissions instead)",
                ))
            }
        };
        let mut entry = json!({ "principal": { "principalType": kind, "principalId": id } });
        if let Some(role) = role_id {
            entry["roleId"] = json!(role);
        }
        let numeric = self.numeric(space_key).await?;
        let _: serde_json::Value = self
            .http
            .post_json(&format!("api/v2/spaces/{numeric}/role-assignments"), &json!([entry]))
            .await?;
        Ok(())
    }
}
