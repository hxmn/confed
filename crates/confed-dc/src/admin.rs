//! Administration on Data Center: users and groups (`rest/api/admin/*`, 8.4+),
//! group membership, and space permissions (`rest/api/space/{key}/permissions`,
//! 9.1+). Shapes are from Atlassian's 10.1 OpenAPI description; the space
//! permission listing is read the way a 9.1 server answers it, where a user is
//! known only by userkey.

use crate::{encode_path, DcClient, PAGE_SIZE};
use confed_api::client::ConfluenceClient;
use confed_api::error::{ApiError, ApiResult};
use confed_api::paginate::collect_offset;
use confed_api::types::*;
use confed_api::wire::v1;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

/// The release that brought the user and group administration API.
pub(crate) const ADMIN_USERS_SINCE: (u32, u32) = (8, 4);
/// The release that brought the space permission API.
pub(crate) const SPACE_PERMISSIONS_SINCE: (u32, u32) = (9, 1);

/// `rest/api/group` and `rest/api/user/memberof` list groups as `{name}`.
#[derive(Debug, Deserialize)]
struct GroupDto {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationDto {
    target_type: String,
    operation_key: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubjectDto {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    user_key: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PermissionDto {
    operation: OperationDto,
    #[serde(default)]
    subject: Option<SubjectDto>,
}

/// `9.5.4` → `(9, 5)`.
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

impl DcClient {
    /// Refuse, before sending anything, an operation this server's release is
    /// known to lack. A server that does not say its version is tried.
    pub(crate) async fn require_version(
        &self,
        since: (u32, u32),
        operation: &str,
    ) -> ApiResult<()> {
        let Some(version) = self.cached_version().await else { return Ok(()) };
        match major_minor(&version) {
            Some(found) if found < since => Err(ApiError::unsupported(
                self.flavor(),
                format!(
                    "{operation}: needs Data Center {}.{} or later, and this server is {version}",
                    since.0, since.1
                ),
            )),
            _ => Ok(()),
        }
    }

    async fn user_key(&self, username: &str) -> ApiResult<String> {
        let user = self.lookup_user(&UserReference::Username(username.to_string())).await?;
        user.user_key.ok_or_else(|| ApiError::NotFound(format!("user {username} has no userkey")))
    }

    /// The path segment of a space permission endpoint that names `subject`.
    async fn subject_path(&self, subject: &Subject) -> ApiResult<String> {
        Ok(match subject {
            Subject::User(username) => {
                format!("user/{}", encode_path(&self.user_key(username).await?))
            }
            Subject::Group(name) => format!("group/{}", encode_path(name)),
            Subject::Anonymous => "anonymous".to_string(),
        })
    }

    pub(crate) async fn admin_create_user(&self, user: &NewUser) -> ApiResult<User> {
        self.require_version(ADMIN_USERS_SINCE, "create a user").await?;
        let mut body = json!({
            "userName": user.username,
            "fullName": user.full_name,
            "email": user.email,
            "notifyViaEmail": user.notify,
        });
        if let Some(password) = &user.password {
            body["password"] = json!(password.expose());
        }
        let created: serde_json::Value =
            self.http.post_json_sensitive("rest/api/admin/user", &body).await?;
        Ok(User {
            account_id: None,
            user_key: created["userKey"].as_str().map(str::to_string),
            username: Some(user.username.clone()),
            display_name: user.full_name.clone(),
            email: Some(user.email.clone()),
        })
    }

    pub(crate) async fn admin_update_user(
        &self,
        username: &str,
        update: &UserUpdate,
    ) -> ApiResult<()> {
        self.require_version(ADMIN_USERS_SINCE, "change a user's profile").await?;
        let mut body = serde_json::Map::new();
        if let Some(name) = &update.full_name {
            body.insert("fullName".into(), json!(name));
        }
        if let Some(email) = &update.email {
            body.insert("email".into(), json!(email));
        }
        let _: serde_json::Value = self
            .http
            .put_json(&format!("rest/api/admin/user/{}", encode_path(username)), &body)
            .await?;
        Ok(())
    }

    pub(crate) async fn admin_set_user_enabled(
        &self,
        username: &str,
        enabled: bool,
    ) -> ApiResult<()> {
        self.require_version(ADMIN_USERS_SINCE, "disable or enable a user").await?;
        let action = if enabled { "enable" } else { "disable" };
        self.http
            .put_empty(&format!("rest/api/admin/user/{}/{action}", encode_path(username)))
            .await
    }

    /// Deleting a user is a background task; follow it to the end.
    pub(crate) async fn admin_delete_user(&self, username: &str) -> ApiResult<()> {
        self.require_version(ADMIN_USERS_SINCE, "delete a user").await?;
        let answer: serde_json::Value = self
            .http
            .delete_json(&format!("rest/api/admin/user/{}", encode_path(username)))
            .await?;
        let task = match &answer["id"] {
            serde_json::Value::String(id) if !id.is_empty() => Some(id.clone()),
            serde_json::Value::Number(id) => Some(id.to_string()),
            _ => None,
        };
        match task {
            Some(task) => self.follow_long_task(&task, "user deletion").await,
            None => Ok(()),
        }
    }

    pub(crate) async fn admin_user_groups(&self, username: &str) -> ApiResult<Vec<Group>> {
        let raw: Vec<GroupDto> = collect_offset(
            &self.http,
            "rest/api/user/memberof",
            &[("username", username.to_string())],
            PAGE_SIZE,
            None,
        )
        .await?;
        Ok(raw.into_iter().map(|g| Group { name: g.name, id: None }).collect())
    }

    pub(crate) async fn admin_list_groups(&self, limit: Option<usize>) -> ApiResult<Vec<Group>> {
        let raw: Vec<GroupDto> =
            collect_offset(&self.http, "rest/api/group", &[], PAGE_SIZE, limit).await?;
        Ok(raw.into_iter().map(|g| Group { name: g.name, id: None }).collect())
    }

    pub(crate) async fn admin_create_group(&self, name: &str) -> ApiResult<Group> {
        self.require_version(ADMIN_USERS_SINCE, "create a group").await?;
        let created: Option<GroupDto> =
            self.http.post_json("rest/api/admin/group", &json!({ "name": name })).await?;
        Ok(Group { name: created.map(|g| g.name).unwrap_or_else(|| name.to_string()), id: None })
    }

    pub(crate) async fn admin_delete_group(&self, name: &str) -> ApiResult<()> {
        self.require_version(ADMIN_USERS_SINCE, "delete a group").await?;
        self.http.delete(&format!("rest/api/admin/group/{}", encode_path(name))).await
    }

    pub(crate) async fn admin_group_members(
        &self,
        group: &str,
        limit: Option<usize>,
    ) -> ApiResult<Vec<User>> {
        let raw: Vec<v1::User> = collect_offset(
            &self.http,
            &format!("rest/api/group/{}/member", encode_path(group)),
            &[],
            PAGE_SIZE,
            limit,
        )
        .await?;
        Ok(raw.into_iter().map(v1::User::into_domain).collect())
    }

    pub(crate) async fn admin_membership(
        &self,
        group: &str,
        username: &str,
        add: bool,
    ) -> ApiResult<()> {
        let operation = if add { "add a user to a group" } else { "remove a user from a group" };
        self.require_version(ADMIN_USERS_SINCE, operation).await?;
        let path = format!("rest/api/user/{}/group/{}", encode_path(username), encode_path(group));
        if add {
            self.http.put_empty(&path).await
        } else {
            self.http.delete(&path).await
        }
    }

    pub(crate) async fn admin_space_permissions(
        &self,
        space_key: &str,
    ) -> ApiResult<Vec<SpacePermission>> {
        self.require_version(SPACE_PERMISSIONS_SINCE, "list space permissions").await?;
        let raw: Vec<PermissionDto> = self
            .http
            .get_json(&format!("rest/api/space/{}/permissions", encode_path(space_key)), &[])
            .await?;
        // A 9.1 server names a user only by userkey: look each one up once.
        let mut names: HashMap<String, Option<String>> = HashMap::new();
        let mut out = Vec::with_capacity(raw.len());
        for permission in raw {
            let subject = permission.subject.unwrap_or_default();
            let principal = match subject.kind.as_deref() {
                Some("group") => Principal {
                    kind: PrincipalKind::Group,
                    id: subject.name.clone(),
                    name: subject.name,
                },
                Some("user") | Some("known") => {
                    let id = subject.user_key.or(subject.username.clone());
                    let mut name = subject.display_name.or(subject.username);
                    if let (None, Some(key)) = (&name, &id) {
                        if !names.contains_key(key) {
                            let found = self
                                .lookup_user(&UserReference::UserKey(key.clone()))
                                .await
                                .ok()
                                .map(|u| u.display_name);
                            names.insert(key.clone(), found);
                        }
                        name = names[key].clone();
                    }
                    Principal { kind: PrincipalKind::User, id, name }
                }
                None | Some("anonymous") => Principal::anonymous(),
                Some(_) => Principal { kind: PrincipalKind::Other, id: subject.name, name: None },
            };
            out.push(SpacePermission {
                id: None,
                principal,
                operation: SpaceOperation::new(
                    permission.operation.target_type,
                    permission.operation.operation_key,
                ),
            });
        }
        Ok(out)
    }

    pub(crate) async fn admin_change_space_permissions(
        &self,
        space_key: &str,
        subject: &Subject,
        operations: &[SpaceOperation],
        grant: bool,
    ) -> ApiResult<()> {
        let operation = if grant { "grant space permissions" } else { "revoke space permissions" };
        self.require_version(SPACE_PERMISSIONS_SINCE, operation).await?;
        let body: Vec<serde_json::Value> = operations
            .iter()
            .map(|op| json!({ "targetType": op.target, "operationKey": op.key }))
            .collect();
        let path = format!(
            "rest/api/space/{}/permissions/{}/{}",
            encode_path(space_key),
            self.subject_path(subject).await?,
            if grant { "grant" } else { "revoke" }
        );
        let _: serde_json::Value = self.http.put_json(&path, &body).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_major_and_minor() {
        assert_eq!(major_minor("9.5.4"), Some((9, 5)));
        assert_eq!(major_minor("10"), Some((10, 0)));
        assert_eq!(major_minor("not a version"), None);
        assert!(major_minor("8.9.2").unwrap() < SPACE_PERMISSIONS_SINCE);
        assert!(major_minor("10.0.1").unwrap() >= SPACE_PERMISSIONS_SINCE);
    }
}
