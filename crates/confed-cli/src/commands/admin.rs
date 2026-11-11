//! `confed admin` — users, groups and space permissions.
//!
//! Every write takes `--dry-run`, which checks what it can by reading (the
//! operations, the site's role mode, the role named) and then says what it
//! would do without doing it. Deleting a user or a group and revoking ask first
//! on a terminal, as `comment rm` does.
//!
//! No workspace is needed. At an instance root the credential is the
//! instance's, read as `confed instance` reads it; users and groups belong to
//! the whole site, and a `perms` command names its space with `KEY:` or
//! `--space`.

use crate::cli::{AdminCommand, AdminGroupCommand, AdminPermsCommand, AdminUserCommand};
use crate::context::Context;
use crate::output::{ErrorEntry, Output};
use crate::pageref::{self, UrlRef};
use confed_api::{
    AdminClient, ConfluenceClient, Group, NewUser, Principal, PrincipalKind, RoleAssignment,
    RoleMode, Secret, SpaceOperation, SpacePermission, Subject, User, UserUpdate,
};
use confed_core::config;
use confed_core::error::{ConfedError, ExitCode, Result};
use serde_json::{json, Value};
use std::fmt::Write;
use std::sync::Arc;

/// The synchronous half: read the credential before the runtime starts.
///
/// At an instance root there is no workspace to read one from, so it is the
/// instance's, found as `confed instance` finds it: the root's shared one, else
/// the first workspace's own. A base URL given by flag or environment names
/// another server, and takes the ordinary path.
pub fn prepare(ctx: &mut Context) -> Result<()> {
    let elsewhere = ctx.resolver.lookup(&config::BASE_URL, ctx.global.base_url.as_deref());
    if !ctx.has_workspace() && ctx.instance_root().is_some() && elsewhere.is_none() {
        let connection = super::instance::connection(ctx, &ctx.instance()?)?;
        let client = connection.client(ctx)?;
        ctx.set_client(client);
        return Ok(());
    }
    ctx.preload_credentials()
}

pub async fn run(ctx: &mut Context, command: &AdminCommand) -> Result<Output> {
    let action = command.action();
    match command {
        AdminCommand::User(c) => user(ctx, action, c).await,
        AdminCommand::Group(c) => group(ctx, action, c).await,
        AdminCommand::Perms(c) => perms(ctx, action, c).await,
    }
}

// ----------------------------------------------------------------- users ---

async fn user(ctx: &mut Context, action: &str, command: &AdminUserCommand) -> Result<Output> {
    match command {
        AdminUserCommand::Create {
            username,
            full_name,
            email,
            password_stdin,
            notify,
            dry_run,
        } => {
            let client = ctx.build_client()?;
            let password = if *notify {
                None
            } else if *password_stdin {
                Some(read_password_line()?)
            } else if ctx.is_interactive() && !*dry_run {
                Some(Secret::new(crate::prompt::read_secret(&format!(
                    "Password for {username}: "
                ))?))
            } else if *dry_run {
                None
            } else {
                return Err(ConfedError::usage_with_hint(
                    format!("no password for {username}"),
                    "pass --password-stdin and the password on stdin, or --notify to e-mail a link to set one",
                ));
            };
            let new = NewUser {
                username: username.clone(),
                full_name: full_name.clone(),
                email: email.clone(),
                password,
                notify: *notify,
            };
            if *dry_run {
                return Ok(planned(
                    action,
                    vec![format!("create user {username} ({full_name}, {email})")],
                ));
            }
            let created = admin(&client)?.create_user(&new).await?;
            let mut human = format!("Created user {username}.\n");
            if *notify {
                let _ = writeln!(human, "{email} gets a link to set a password.");
            }
            Ok(Output::new(
                json!({ "action": action, "dry_run": false, "user": user_json(&created) }),
                human,
            ))
        }
        AdminUserCommand::Update { username, full_name, email, dry_run } => {
            let client = ctx.build_client()?;
            let mut changes = Vec::new();
            if let Some(name) = full_name {
                changes.push(format!("full name to {name}"));
            }
            if let Some(address) = email {
                changes.push(format!("e-mail to {address}"));
            }
            let sentence = format!("change {username}'s {}", changes.join(" and "));
            if *dry_run {
                return Ok(planned(action, vec![sentence]));
            }
            let update = UserUpdate { full_name: full_name.clone(), email: email.clone() };
            admin(&client)?.update_user(username, &update).await?;
            Ok(done(action, json!({ "username": username }), format!("Updated {username}.\n")))
        }
        AdminUserCommand::Disable { username, dry_run }
        | AdminUserCommand::Enable { username, dry_run } => {
            let enable = matches!(command, AdminUserCommand::Enable { .. });
            let verb = if enable { "enable" } else { "disable" };
            let client = ctx.build_client()?;
            if *dry_run {
                return Ok(planned(action, vec![format!("{verb} user {username}")]));
            }
            admin(&client)?.set_user_enabled(username, enable).await?;
            Ok(done(
                action,
                json!({ "username": username }),
                format!("{}d {username}.\n", capitalize(verb)),
            ))
        }
        AdminUserCommand::Delete { username, dry_run } => {
            let client = ctx.build_client()?;
            if *dry_run {
                return Ok(planned(action, vec![format!("delete user {username}")]));
            }
            if !confirmed(ctx, &format!("Delete user {username}?"))? {
                return Ok(cancelled(action));
            }
            admin(&client)?.delete_user(username).await?;
            Ok(done(action, json!({ "username": username }), format!("Deleted user {username}.\n")))
        }
        AdminUserCommand::Groups { user } => {
            let client = ctx.build_client()?;
            let groups = admin(&client)?.user_groups(user).await?;
            let mut human = String::new();
            for g in &groups {
                let _ = writeln!(human, "{}", g.name);
            }
            if groups.is_empty() {
                let _ = writeln!(human, "{user} is in no group.");
            }
            Ok(Output::new(
                json!({ "action": action, "user": user, "groups": groups.iter().map(group_json).collect::<Vec<_>>() }),
                human,
            ))
        }
    }
}

fn read_password_line() -> Result<Secret> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| ConfedError::io("reading the password from stdin", e))?;
    let password = line.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        return Err(ConfedError::usage("--password-stdin read an empty password"));
    }
    Ok(Secret::new(password))
}

// ---------------------------------------------------------------- groups ---

async fn group(ctx: &mut Context, action: &str, command: &AdminGroupCommand) -> Result<Output> {
    let client = ctx.build_client()?;
    match command {
        AdminGroupCommand::List { matching, limit } => {
            // A match is looked for in every group, then cut to the limit.
            let fetch = if matching.is_some() { None } else { *limit };
            let mut groups = admin(&client)?.list_groups(fetch).await?;
            if let Some(text) = matching {
                let wanted = text.to_lowercase();
                groups.retain(|g| g.name.to_lowercase().contains(&wanted));
                groups.truncate(limit.unwrap_or(usize::MAX));
            }
            let mut human = String::new();
            for g in &groups {
                let _ = writeln!(human, "{}", g.name);
            }
            match (groups.is_empty(), matching) {
                (true, Some(text)) => {
                    let _ = writeln!(human, "No group's name contains `{text}`.");
                }
                (true, None) => human.push_str("No groups are visible to this account.\n"),
                _ => {}
            }
            Ok(Output::new(
                json!({ "action": action, "groups": groups.iter().map(group_json).collect::<Vec<_>>() }),
                human,
            ))
        }
        AdminGroupCommand::Create { name, dry_run } => {
            if *dry_run {
                return Ok(planned(action, vec![format!("create group {name}")]));
            }
            let created = admin(&client)?.create_group(name).await?;
            Ok(done(
                action,
                json!({ "group": group_json(&created) }),
                format!("Created group {name}.\n"),
            ))
        }
        AdminGroupCommand::Delete { name, dry_run } => {
            if *dry_run {
                return Ok(planned(action, vec![format!("delete group {name}")]));
            }
            if !confirmed(
                ctx,
                &format!("Delete group {name}? Its members stay, but lose what it grants."),
            )? {
                return Ok(cancelled(action));
            }
            admin(&client)?.delete_group(name).await?;
            Ok(done(
                action,
                json!({ "group": { "name": name } }),
                format!("Deleted group {name}.\n"),
            ))
        }
        AdminGroupCommand::Members { name, limit } => {
            let members = admin(&client)?.group_members(name, *limit).await?;
            let mut human = String::new();
            for m in &members {
                let _ = writeln!(
                    human,
                    "{:<28} {}",
                    m.display_name,
                    m.username.clone().or_else(|| m.account_id.clone()).unwrap_or_default()
                );
            }
            if members.is_empty() {
                let _ = writeln!(human, "{name} has no members.");
            }
            Ok(Output::new(
                json!({ "action": action, "group": name, "members": members.iter().map(user_json).collect::<Vec<_>>() }),
                human,
            ))
        }
        AdminGroupCommand::Add { group, users, dry_run }
        | AdminGroupCommand::Remove { group, users, dry_run } => {
            let add = matches!(command, AdminGroupCommand::Add { .. });
            let sentence = |u: &str| {
                if add {
                    format!("add {u} to {group}")
                } else {
                    format!("remove {u} from {group}")
                }
            };
            if *dry_run {
                return Ok(planned(action, users.iter().map(|u| sentence(u)).collect()));
            }
            membership(client, action, group, users, add).await
        }
    }
}

/// What became of a change to who is in a group.
struct Membership {
    /// The users added or removed.
    changed: Vec<String>,
    /// Adding: already in the group. Removing: not in it. Nothing was sent
    /// for them.
    unchanged: Vec<String>,
    /// Those that failed, with why; an unknown user is one of them.
    failed: Vec<(String, ConfedError)>,
    /// Who is in the group now, read back after the change.
    members: Vec<User>,
}

/// Whether a listed member is the user the command line named: by username
/// on Data Center, by account id on Cloud.
fn is_user(member: &User, named: &str) -> bool {
    member.username.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(named))
        || member.account_id.as_deref() == Some(named)
        || member.user_key.as_deref() == Some(named)
}

/// How a member is named on the command line.
fn user_arg(member: &User) -> Option<String> {
    member.username.clone().or_else(|| member.account_id.clone())
}

fn unknown_user(user: &str) -> ConfedError {
    ConfedError::refused(
        "unknown_user",
        ExitCode::NotFound,
        format!("no user {user}"),
        Some(
            "a user is a username on Data Center and an account id on Cloud; `confed user \
             search NAME` prints both"
                .into(),
        ),
    )
}

/// Add `users` to `group`, or remove them, one at a time, carrying on past a
/// failure. `before` is who is in the group, when the caller knows; reading it
/// here also says that a group does not exist before anything is sent.
async fn change_members(
    client: &Arc<dyn ConfluenceClient>,
    group: &str,
    users: &[String],
    add: bool,
    before: Option<Vec<User>>,
) -> Result<Membership> {
    let api = admin(client)?;
    let before = match before {
        Some(members) => members,
        None => api.group_members(group, None).await?,
    };
    let mut done = Membership {
        changed: Vec::new(),
        unchanged: Vec::new(),
        failed: Vec::new(),
        members: Vec::new(),
    };
    let mut seen: Vec<&str> = Vec::new();
    for user in users {
        if seen.iter().any(|s| s.eq_ignore_ascii_case(user)) {
            continue;
        }
        seen.push(user);
        let member = before.iter().any(|m| is_user(m, user));
        let outcome = match (add, member) {
            (true, true) | (false, false) => {
                // Nothing to send. Somebody who is not in the group may be
                // nobody at all, which only the server can say.
                let known = match (member, client.flavor()) {
                    (true, _) => Ok(()),
                    (false, confed_api::Flavor::Cloud) => client
                        .lookup_user(&confed_api::UserReference::AccountId(user.clone()))
                        .await
                        .map(drop),
                    (false, confed_api::Flavor::DataCenter) => client
                        .lookup_user(&confed_api::UserReference::Username(user.clone()))
                        .await
                        .map(drop),
                };
                match known {
                    Ok(()) => {
                        done.unchanged.push(user.clone());
                        continue;
                    }
                    Err(e) => Err(e),
                }
            }
            (true, false) => api.add_group_member(group, user).await,
            (false, true) => api.remove_group_member(group, user).await,
        };
        match outcome {
            Ok(()) => done.changed.push(user.clone()),
            // The group is there: what was not found is the user.
            Err(confed_api::ApiError::NotFound(_)) => {
                done.failed.push((user.clone(), unknown_user(user)));
            }
            Err(e) => done.failed.push((user.clone(), e.into())),
        }
    }
    done.members = api.group_members(group, None).await?;
    Ok(done)
}

impl Membership {
    /// The users that failed because the server knows nobody by that name.
    fn unknown(&self) -> Vec<&str> {
        self.failed
            .iter()
            .filter(|(_, e)| e.kind() == Some("unknown_user"))
            .map(|(user, _)| user.as_str())
            .collect()
    }

    /// The fields every membership change reports, beside its own.
    fn report(&self, result: &mut Value, add: bool) {
        result["users"] = json!(self.changed);
        result[if add { "already_members" } else { "not_members" }] = json!(self.unchanged);
        result["unknown_users"] = json!(self.unknown());
        result["members"] = self.members.iter().map(user_json).collect();
    }

    fn describe(&self, human: &mut String, group: &str, add: bool) {
        for user in &self.changed {
            let _ = writeln!(human, "{} {user}", if add { "added  " } else { "removed" });
        }
        for user in &self.unchanged {
            let why = if add { "already a member" } else { "not a member" };
            let _ = writeln!(human, "kept    {user} ({why})");
        }
        for (user, e) in &self.failed {
            let _ = writeln!(human, "failed  {user}: {e}");
        }
        let names: Vec<String> = self.members.iter().filter_map(user_arg).collect();
        let _ = match names.len() {
            0 => writeln!(human, "{group} has no members."),
            n => writeln!(
                human,
                "{group} has {}: {}",
                crate::output::plural(n, "member", "members"),
                names.join(", ")
            ),
        };
    }

    /// The exit code and the errors of a change some of which failed: a
    /// partial success when anybody went through, else the first failure's.
    fn finish(self, mut output: Output) -> Output {
        let Some(first) = self.failed.first().map(|(_, e)| e.exit_code()) else { return output };
        let nobody = self.changed.is_empty() && self.unchanged.is_empty();
        output.exit = if nobody { first } else { ExitCode::Partial };
        output.with_errors(self.failed.iter().map(|(_, e)| ErrorEntry::from_error(e)).collect())
    }
}

/// `group add` and `group remove`.
async fn membership(
    client: Arc<dyn ConfluenceClient>,
    action: &str,
    group: &str,
    users: &[String],
    add: bool,
) -> Result<Output> {
    let mut done = change_members(&client, group, users, add, None).await?;
    // One user, and it failed: that is the command's own failure.
    if users.len() == 1 && !done.failed.is_empty() {
        return Err(done.failed.remove(0).1);
    }
    let mut result = json!({ "action": action, "dry_run": false, "group": group });
    done.report(&mut result, add);
    let mut human = String::new();
    done.describe(&mut human, group, add);
    Ok(done.finish(Output::new(result, human)))
}

// ----------------------------------------------------------- permissions ---

async fn perms(ctx: &mut Context, action: &str, command: &AdminPermsCommand) -> Result<Output> {
    let (subject, target) = match command {
        AdminPermsCommand::List { target, .. } | AdminPermsCommand::Roles { target } => {
            ("", target.as_deref())
        }
        AdminPermsCommand::Grant { subject, target, .. }
        | AdminPermsCommand::Revoke { subject, target, .. } => {
            subject_and_space(subject, target.as_deref())
        }
    };
    let space = perms_space(ctx, target)?;
    let client = ctx.build_client()?;
    let mode = admin(&client)?.space_role_mode().await?;
    match command {
        AdminPermsCommand::List { only, compact, .. } => {
            let only = only.as_deref().map(parse_subject).transpose()?;
            let mut permissions = if mode.grants_permissions() {
                admin(&client)?.space_permissions(&space).await?
            } else {
                Vec::new()
            };
            let (mut assignments, roles) = if mode.assigns_roles() {
                (
                    admin(&client)?.space_role_assignments(&space).await?,
                    admin(&client)?.space_roles(&space).await?,
                )
            } else {
                (Vec::new(), Vec::new())
            };
            if let Some(subject) = &only {
                permissions.retain(|p| is_subject(&p.principal, subject));
                assignments.retain(|a| is_subject(&a.principal, subject));
            }
            let role_name = |id: &str| roles.iter().find(|r| r.id == id).map(|r| r.name.clone());
            let held = holdings(&permissions, &assignments);

            let mut human = String::new();
            for h in &held {
                let mut what = list_operations(&h.operations).replace(", ", " ");
                if let Some(id) = &h.role {
                    let role = role_name(id).unwrap_or_else(|| id.clone());
                    let _ = write!(what, "{}role {role}", if what.is_empty() { "" } else { "  " });
                }
                let _ = writeln!(human, "{:<32} {what}", subject_label(&h.principal));
            }
            match (held.is_empty(), &only) {
                (true, Some(subject)) => {
                    let _ = writeln!(human, "Nothing is granted to {subject} in {space}.");
                }
                (true, None) => {
                    let _ = writeln!(human, "Nothing is granted in {space}.");
                }
                _ => {}
            }

            let mut result = json!({ "action": action, "space": space, "mode": mode.as_str() });
            if let Some(subject) = &only {
                result["subject"] = json!(subject_arg_of(subject));
            }
            if *compact {
                result["subjects"] = held
                    .iter()
                    .map(|h| holding_json(h, h.role.as_deref().and_then(role_name)))
                    .collect();
            } else {
                result["permissions"] = permissions
                    .iter()
                    .map(|p| {
                        json!({
                            "subject": principal_json(&p.principal),
                            "operation": p.operation.to_string(),
                            "id": p.id,
                        })
                    })
                    .collect();
                result["assignments"] = assignments
                    .iter()
                    .map(|a| {
                        json!({
                            "subject": principal_json(&a.principal),
                            "role_id": a.role_id,
                            "role": role_name(&a.role_id),
                        })
                    })
                    .collect();
            }
            Ok(Output::new(result, human))
        }
        AdminPermsCommand::Roles { .. } => {
            require_roles(mode, "list space roles")?;
            let roles = admin(&client)?.space_roles(&space).await?;
            let mut human = String::new();
            for r in &roles {
                let _ =
                    writeln!(human, "{:<20} {}", r.name, r.description.clone().unwrap_or_default());
            }
            Ok(Output::new(
                json!({
                    "action": action,
                    "space": space,
                    "roles": roles.iter().map(|r| json!({
                        "id": r.id, "name": r.name, "type": r.kind, "description": r.description,
                    })).collect::<Vec<_>>(),
                }),
                human,
            ))
        }
        AdminPermsCommand::Grant { ops, role, dry_run, .. } => {
            let who = parse_subject(subject)?;
            if let Some(role) = role {
                require_roles(mode, "assign a space role")?;
                let roles = admin(&client)?.space_roles(&space).await?;
                let found = roles
                    .iter()
                    .find(|r| r.id == *role || r.name.eq_ignore_ascii_case(role))
                    .ok_or_else(|| {
                        ConfedError::usage_with_hint(
                            format!("{space} has no role {role}"),
                            format!(
                                "the roles are: {}",
                                roles
                                    .iter()
                                    .map(|r| r.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        )
                    })?;
                let sentence = format!("give {who} the {} role in {space}", found.name);
                if *dry_run {
                    return Ok(planned(action, vec![sentence]));
                }
                admin(&client)?.set_space_role(&space, &who, Some(&found.id)).await?;
                return Ok(done(
                    action,
                    json!({ "space": space, "subject": subject, "role": found.name, "role_id": found.id }),
                    format!("Gave {who} the {} role in {space}.\n", found.name),
                ));
            }
            let operations = parse_operations(client.as_ref(), ops)?;
            require_permissions(mode, "grant space permissions")?;
            let listed = list_operations(&operations);
            if *dry_run {
                return Ok(planned(action, vec![format!("grant {who} {listed} in {space}")]));
            }
            admin(&client)?.grant_space_permissions(&space, &who, &operations).await?;
            let held = held_now(&client, &space, &who).await?;
            Ok(done(
                action,
                json!({
                    "space": space,
                    "subject": subject,
                    "operations": operations_json(&operations),
                    "held": operations_json(&held),
                }),
                format!("Granted {who} {listed} in {space}.\n{}", holds_line(&who, &space, &held)),
            ))
        }
        AdminPermsCommand::Revoke { ops, role, dry_run, .. } => {
            let who = parse_subject(subject)?;
            if *role {
                require_roles(mode, "take a space role away")?;
                if *dry_run {
                    return Ok(planned(action, vec![format!("take {who}'s role in {space} away")]));
                }
                if !confirmed(ctx, &format!("Take {who}'s role in {space} away?"))? {
                    return Ok(cancelled(action));
                }
                admin(&client)?.set_space_role(&space, &who, None).await?;
                return Ok(done(
                    action,
                    json!({ "space": space, "subject": subject, "role": Value::Null }),
                    format!("Took {who}'s role in {space} away.\n"),
                ));
            }
            let operations = parse_operations(client.as_ref(), ops)?;
            require_permissions(mode, "revoke space permissions")?;
            let listed = list_operations(&operations);
            if *dry_run {
                return Ok(planned(action, vec![format!("revoke {listed} from {who} in {space}")]));
            }
            if !confirmed(ctx, &format!("Revoke {listed} from {who} in {space}?"))? {
                return Ok(cancelled(action));
            }
            admin(&client)?.revoke_space_permissions(&space, &who, &operations).await?;
            let held = held_now(&client, &space, &who).await?;
            Ok(done(
                action,
                json!({
                    "space": space,
                    "subject": subject,
                    "operations": operations_json(&operations),
                    "held": operations_json(&held),
                }),
                format!(
                    "Revoked {listed} from {who} in {space}.\n{}",
                    holds_line(&who, &space, &held)
                ),
            ))
        }
    }
}

/// The space a `perms` command is about: its `KEY:` target, else `--space`
/// (or `CONFED_SPACE`), else the workspace's. Nothing is guessed: at an
/// instance root, with every space to choose from, one must be named.
fn perms_space(ctx: &Context, target: Option<&str>) -> Result<String> {
    if let Some(target) = target {
        return space_target(target);
    }
    if let Some(found) = ctx.resolver.lookup(&config::SPACE, ctx.global.space_flag()) {
        return Ok(found.value);
    }
    if ctx.is_interactive() {
        return Ok(ctx.space_key()?.value);
    }
    let registered: Vec<String> = ctx
        .instance()
        .and_then(|i| i.enabled_members())
        .map(|members| members.into_iter().map(|m| m.space_key).collect())
        .unwrap_or_default();
    let mut hint = "name the space: `KEY:` after the command, or --space KEY".to_string();
    if !registered.is_empty() {
        let _ = write!(hint, " (registered here: {})", registered.join(", "));
    }
    Err(ConfedError::refused(
        "space_required",
        ExitCode::Usage,
        "this command is about one space, and none was named",
        Some(hint),
    ))
}

/// `DOCS:`, `DOCS` or a space URL → `DOCS`.
fn space_target(text: &str) -> Result<String> {
    let no_space = || {
        ConfedError::usage_with_hint(
            format!("`{text}` names no space"),
            "write the space as `KEY:` (DOCS:), or pass --space KEY",
        )
    };
    if pageref::is_url(text) {
        return match pageref::parse(text).map(|parsed| parsed.target) {
            Some(UrlRef::Space { space }) => Ok(space),
            _ => Err(no_space()),
        };
    }
    let key = text.strip_suffix(':').unwrap_or(text).trim();
    if key.is_empty() || key.contains([':', '/', ' ']) {
        return Err(no_space());
    }
    Ok(key.to_string())
}

/// `grant` and `revoke` take the subject and, optionally, the space as `KEY:`.
/// Written the other way round they mean the same: only one of the two can
/// name somebody.
fn subject_and_space<'a>(subject: &'a str, target: Option<&'a str>) -> (&'a str, Option<&'a str>) {
    match target {
        Some(other) if parse_subject(subject).is_err() && parse_subject(other).is_ok() => {
            (other, Some(subject))
        }
        _ => (subject, target),
    }
}

/// The administration half of `client`, or why it has none.
fn admin(client: &Arc<dyn ConfluenceClient>) -> Result<&dyn AdminClient> {
    client.admin().ok_or_else(|| {
        ConfedError::from(confed_api::ApiError::unsupported(client.flavor(), "administration"))
    })
}

/// `group:NAME`, `user:NAME` or `anonymous`.
fn parse_subject(text: &str) -> Result<Subject> {
    let hint =
        "name it group:NAME, user:NAME (username on Data Center, account id on Cloud) or anonymous";
    if text.eq_ignore_ascii_case("anonymous") {
        return Ok(Subject::Anonymous);
    }
    match text.split_once(':') {
        Some((kind, name)) if !name.is_empty() => match kind.to_ascii_lowercase().as_str() {
            "group" => Ok(Subject::Group(name.to_string())),
            "user" => Ok(Subject::User(name.to_string())),
            _ => Err(ConfedError::usage_with_hint(format!("`{text}` names nobody"), hint)),
        },
        _ => Err(ConfedError::usage_with_hint(format!("`{text}` names nobody"), hint)),
    }
}

/// Checked against what this flavor's API takes, before anything is sent.
fn parse_operations(client: &dyn ConfluenceClient, ops: &[String]) -> Result<Vec<SpaceOperation>> {
    let flavor = client.flavor();
    let valid = || {
        SpaceOperation::valid_for(flavor)
            .iter()
            .map(|(t, k)| format!("{t}:{k}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out: Vec<SpaceOperation> = Vec::new();
    for text in ops.iter().filter(|o| !o.trim().is_empty()) {
        let op = SpaceOperation::parse(text).map_err(|e| ConfedError::usage(e.to_string()))?;
        if !op.is_valid_for(flavor) {
            return Err(ConfedError::usage_with_hint(
                format!("{flavor} has no space permission {op}"),
                format!("the operations are: {}", valid()),
            ));
        }
        if !out.contains(&op) {
            out.push(op);
        }
    }
    if out.is_empty() {
        return Err(ConfedError::usage_with_hint(
            "no operation given",
            format!("pass --op, one of: {}", valid()),
        ));
    }
    Ok(out)
}

fn require_roles(mode: RoleMode, what: &str) -> Result<()> {
    if mode.assigns_roles() {
        return Ok(());
    }
    Err(ConfedError::Unsupported(format!(
        "{what}: this site grants space permissions, not roles; use --op instead of --role"
    )))
}

fn require_permissions(mode: RoleMode, what: &str) -> Result<()> {
    if mode.grants_permissions() {
        return Ok(());
    }
    Err(ConfedError::Unsupported(format!(
        "{what}: this site assigns space roles instead; use --role (see `confed admin perms roles`)"
    )))
}

fn list_operations(operations: &[SpaceOperation]) -> String {
    operations.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

fn operations_json(operations: &[SpaceOperation]) -> Vec<String> {
    operations.iter().map(ToString::to_string).collect()
}

/// A subject as the command line names it: `group:NAME`, `user:NAME`,
/// `anonymous`.
fn subject_arg_of(subject: &Subject) -> String {
    match subject {
        Subject::Group(name) => format!("group:{name}"),
        Subject::User(name) => format!("user:{name}"),
        Subject::Anonymous => "anonymous".to_string(),
    }
}

/// A listed principal as a SUBJECT argument names it, so that what a listing
/// prints can be given back to `grant`, `revoke` or `--subject`: a group by
/// name, a user by username on Data Center and by account id on Cloud. A
/// user the server no longer knows is left with the userkey it listed.
fn subject_arg(principal: &Principal) -> String {
    let first = |names: [&Option<String>; 3]| {
        names.into_iter().flatten().next().cloned().unwrap_or_default()
    };
    match principal.kind {
        PrincipalKind::Anonymous => "anonymous".to_string(),
        PrincipalKind::Group => {
            format!("group:{}", first([&principal.name, &principal.id, &None]))
        }
        PrincipalKind::User => {
            format!("user:{}", first([&principal.username, &principal.id, &principal.name]))
        }
        PrincipalKind::Other => format!("other:{}", first([&principal.id, &principal.name, &None])),
    }
}

/// [`subject_arg`], with a person's name beside it for a reader.
fn subject_label(principal: &Principal) -> String {
    let arg = subject_arg(principal);
    match (&principal.kind, &principal.name) {
        (PrincipalKind::User, Some(name)) if !arg.ends_with(&format!(":{name}")) => {
            format!("{arg} ({name})")
        }
        _ => arg,
    }
}

/// Whether a listed principal is the subject the command line named.
fn is_subject(principal: &Principal, subject: &Subject) -> bool {
    let is = |held: &Option<String>, wanted: &str| held.as_deref() == Some(wanted);
    match subject {
        Subject::Anonymous => principal.kind == PrincipalKind::Anonymous,
        Subject::Group(name) => {
            principal.kind == PrincipalKind::Group
                && (is(&principal.name, name) || is(&principal.id, name))
        }
        Subject::User(user) => {
            principal.kind == PrincipalKind::User
                && (principal.username.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(user))
                    || is(&principal.id, user))
        }
    }
}

/// The operations `subject` holds in `space`, read from the server: after a
/// change, what the change came to.
async fn held_now(
    client: &Arc<dyn ConfluenceClient>,
    space: &str,
    subject: &Subject,
) -> Result<Vec<SpaceOperation>> {
    let permissions = admin(client)?.space_permissions(space).await?;
    Ok(operations_of(&permissions, subject))
}

/// What `subject` holds among `permissions`, each operation once.
fn operations_of(permissions: &[SpacePermission], subject: &Subject) -> Vec<SpaceOperation> {
    let mut held: Vec<SpaceOperation> = Vec::new();
    for permission in permissions.iter().filter(|p| is_subject(&p.principal, subject)) {
        if !held.contains(&permission.operation) {
            held.push(permission.operation.clone());
        }
    }
    held
}

fn holds_line(who: &Subject, space: &str, held: &[SpaceOperation]) -> String {
    match held.is_empty() {
        true => format!("Nothing is held by {who} in {space} now.\n"),
        false => format!("Now held by {who} in {space}: {}\n", list_operations(held)),
    }
}

/// What one subject holds in a space: its operations, in the order the
/// server lists them, and its role on a site that assigns them.
struct Holding {
    principal: Principal,
    operations: Vec<SpaceOperation>,
    role: Option<String>,
}

/// A space's permissions and role assignments, one entry per subject.
fn holdings(permissions: &[SpacePermission], assignments: &[RoleAssignment]) -> Vec<Holding> {
    fn entry<'a>(held: &'a mut Vec<Holding>, principal: &Principal) -> &'a mut Holding {
        let same = |h: &Holding| {
            h.principal.kind == principal.kind
                && h.principal.id == principal.id
                && (principal.id.is_some() || h.principal.name == principal.name)
        };
        match held.iter().position(same) {
            Some(at) => &mut held[at],
            None => {
                held.push(Holding {
                    principal: principal.clone(),
                    operations: Vec::new(),
                    role: None,
                });
                held.last_mut().expect("pushed above")
            }
        }
    }
    let mut held = Vec::new();
    for permission in permissions {
        let operations = &mut entry(&mut held, &permission.principal).operations;
        if !operations.contains(&permission.operation) {
            operations.push(permission.operation.clone());
        }
    }
    for assignment in assignments {
        entry(&mut held, &assignment.principal).role = Some(assignment.role_id.clone());
    }
    held
}

/// One subject of `perms list --compact`.
fn holding_json(holding: &Holding, role: Option<String>) -> Value {
    let principal = &holding.principal;
    let mut entry = json!({
        "subject": subject_arg(principal),
        "type": principal.kind.as_str(),
        "id": principal.id,
        "name": principal.name,
        "operations": operations_json(&holding.operations),
    });
    if let Some(username) = &principal.username {
        entry["username"] = json!(username);
    }
    if let Some(id) = &holding.role {
        entry["role_id"] = json!(id);
        entry["role"] = json!(role);
    }
    entry
}

// --------------------------------------------------------------- helpers ---

/// Ask on a terminal unless --yes; without one, go ahead, as `comment rm` does.
fn confirmed(ctx: &Context, question: &str) -> Result<bool> {
    if ctx.global.yes || !ctx.is_interactive() {
        return Ok(true);
    }
    crate::prompt::confirm(question, false)
}

fn planned(action: &str, steps: Vec<String>) -> Output {
    let mut human = String::from("Would:\n");
    for step in &steps {
        let _ = writeln!(human, "  {step}");
    }
    human.push_str("Nothing was sent (--dry-run).\n");
    Output::new(json!({ "action": action, "dry_run": true, "planned": steps }), human)
}

fn done(action: &str, fields: Value, human: String) -> Output {
    let mut result = json!({ "action": action, "dry_run": false });
    if let (Some(map), Value::Object(extra)) = (result.as_object_mut(), fields) {
        map.extend(extra);
    }
    Output::new(result, human)
}

fn cancelled(action: &str) -> Output {
    Output::new(
        json!({ "action": action, "dry_run": false, "cancelled": true }),
        "Cancelled: nothing was changed.\n".to_string(),
    )
}

fn user_json(user: &User) -> Value {
    json!({
        "display_name": user.display_name,
        "username": user.username,
        "userkey": user.user_key,
        "account_id": user.account_id,
        "email": user.email,
    })
}

fn group_json(group: &Group) -> Value {
    json!({ "name": group.name, "id": group.id })
}

fn principal_json(principal: &Principal) -> Value {
    let mut subject =
        json!({ "type": principal.kind.as_str(), "id": principal.id, "name": principal.name });
    if let Some(username) = &principal.username {
        subject["username"] = json!(username);
    }
    subject
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_is_named_by_key_or_url() {
        assert_eq!(space_target("DOCS:").unwrap(), "DOCS");
        assert_eq!(space_target("DOCS").unwrap(), "DOCS");
        assert_eq!(space_target("~alice:").unwrap(), "~alice");
        assert_eq!(space_target("https://wiki.example.test/display/ENG").unwrap(), "ENG");
        for bad in [":", "DOCS:Team Handbook.md", "group:writers", ""] {
            assert_eq!(space_target(bad).unwrap_err().exit_code(), ExitCode::Usage, "{bad}");
        }
    }

    #[test]
    fn a_subject_and_a_space_are_told_apart_in_either_order() {
        assert_eq!(subject_and_space("group:writers", None), ("group:writers", None));
        assert_eq!(
            subject_and_space("group:writers", Some("DOCS:")),
            ("group:writers", Some("DOCS:"))
        );
        assert_eq!(
            subject_and_space("DOCS:", Some("group:writers")),
            ("group:writers", Some("DOCS:"))
        );
        assert_eq!(subject_and_space("anonymous", Some("DOCS")), ("anonymous", Some("DOCS")));
    }
}
