//! The administration half of the client contract: users, groups and space
//! permissions. Users are named by username on Data Center and by account id
//! on Cloud; groups by name. Every call needs an administrator's credential.
//!
//! A trait of its own, so that the sync engine's contract does not carry
//! eighteen methods it never calls, and a client — the mock, a future one —
//! need not implement any of them: [`ConfluenceClient::admin`] says whether a
//! client has this half at all. Each method defaults to "unsupported on this
//! flavor", for an implementation that has only some.

use crate::client::ConfluenceClient;
use crate::error::ApiResult;
use crate::types::*;
use async_trait::async_trait;

#[async_trait]
pub trait AdminClient: ConfluenceClient {
    /// Create a user in the server's own directory.
    async fn create_user(&self, user: &NewUser) -> ApiResult<User> {
        let _ = user;
        Err(crate::error::ApiError::unsupported(self.flavor(), "create a user"))
    }

    async fn update_user(&self, username: &str, update: &UserUpdate) -> ApiResult<()> {
        let _ = (username, update);
        Err(crate::error::ApiError::unsupported(self.flavor(), "change a user's profile"))
    }

    /// Disable a user (they keep their content and can no longer sign in), or
    /// enable them again.
    async fn set_user_enabled(&self, username: &str, enabled: bool) -> ApiResult<()> {
        let _ = (username, enabled);
        Err(crate::error::ApiError::unsupported(self.flavor(), "disable or enable a user"))
    }

    async fn delete_user(&self, username: &str) -> ApiResult<()> {
        let _ = username;
        Err(crate::error::ApiError::unsupported(self.flavor(), "delete a user"))
    }

    /// The groups `user` belongs to.
    async fn user_groups(&self, user: &str) -> ApiResult<Vec<Group>> {
        let _ = user;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list a user's groups"))
    }

    async fn list_groups(&self, limit: Option<usize>) -> ApiResult<Vec<Group>> {
        let _ = limit;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list groups"))
    }

    async fn create_group(&self, name: &str) -> ApiResult<Group> {
        let _ = name;
        Err(crate::error::ApiError::unsupported(self.flavor(), "create a group"))
    }

    async fn delete_group(&self, name: &str) -> ApiResult<()> {
        let _ = name;
        Err(crate::error::ApiError::unsupported(self.flavor(), "delete a group"))
    }

    async fn group_members(&self, group: &str, limit: Option<usize>) -> ApiResult<Vec<User>> {
        let _ = (group, limit);
        Err(crate::error::ApiError::unsupported(self.flavor(), "list a group's members"))
    }

    async fn add_group_member(&self, group: &str, user: &str) -> ApiResult<()> {
        let _ = (group, user);
        Err(crate::error::ApiError::unsupported(self.flavor(), "add a user to a group"))
    }

    async fn remove_group_member(&self, group: &str, user: &str) -> ApiResult<()> {
        let _ = (group, user);
        Err(crate::error::ApiError::unsupported(self.flavor(), "remove a user from a group"))
    }

    /// Whether the site grants space access as permissions, as roles, or both.
    async fn space_role_mode(&self) -> ApiResult<RoleMode> {
        Ok(RoleMode::Permissions)
    }

    /// Every permission granted in the space.
    async fn space_permissions(&self, space_key: &str) -> ApiResult<Vec<SpacePermission>> {
        let _ = space_key;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list space permissions"))
    }

    /// Grant `operations` in the space, leaving the subject's others alone.
    async fn grant_space_permissions(
        &self,
        space_key: &str,
        subject: &Subject,
        operations: &[SpaceOperation],
    ) -> ApiResult<()> {
        let _ = (space_key, subject, operations);
        Err(crate::error::ApiError::unsupported(self.flavor(), "grant space permissions"))
    }

    /// Revoke `operations` in the space, leaving the subject's others alone.
    async fn revoke_space_permissions(
        &self,
        space_key: &str,
        subject: &Subject,
        operations: &[SpaceOperation],
    ) -> ApiResult<()> {
        let _ = (space_key, subject, operations);
        Err(crate::error::ApiError::unsupported(self.flavor(), "revoke space permissions"))
    }

    /// The roles a space can assign.
    async fn space_roles(&self, space_key: &str) -> ApiResult<Vec<SpaceRole>> {
        let _ = space_key;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list space roles"))
    }

    async fn space_role_assignments(&self, space_key: &str) -> ApiResult<Vec<RoleAssignment>> {
        let _ = space_key;
        Err(crate::error::ApiError::unsupported(self.flavor(), "list space role assignments"))
    }

    /// Assign `role_id` to the subject in the space, or with `None` take the
    /// subject's role away.
    async fn set_space_role(
        &self,
        space_key: &str,
        subject: &Subject,
        role_id: Option<&str>,
    ) -> ApiResult<()> {
        let _ = (space_key, subject, role_id);
        Err(crate::error::ApiError::unsupported(self.flavor(), "assign a space role"))
    }
}
