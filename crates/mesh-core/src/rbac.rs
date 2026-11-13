//! RBAC policy evaluation (MASTERPLAN §7.2).
//!
//! Enforcement happens on the serving/executing node against its local
//! materialized policy — there is no policy server to be down. The same
//! functions back both API-time checks (daemon) and merge-time checks
//! (state engine), differing only in the evaluation instant.

use std::collections::BTreeSet;

use mesh_proto::{Grant, MemberInfo, Role, UserId};

use crate::membership::Selector;
use crate::state::MeshState;

/// Roles that may read basic mesh state (everything except secrets/audit).
pub const READ: &[Role] = &[
    Role::Admin,
    Role::Operator,
    Role::Developer,
    Role::Auditor,
    Role::Viewer,
];
/// Roles that may submit jobs. Cancelling is scoped by *whose* job it is —
/// that rule lives in [`require_cancel`], not in a flat role list.
pub const RUNNERS: &[Role] = &[Role::Admin, Role::Operator, Role::Developer];
/// Roles that may cancel **any** job, including force-cancelling one whose
/// executor is gone. Operators are the day-2 crew — a mesh where only
/// admins can stop a runaway job is a mesh that pages an admin for it.
pub const CANCEL_ANY: &[Role] = &[Role::Admin, Role::Operator];
/// Roles that may manage content: push bundles, manage services/contracts.
pub const CONTENT: &[Role] = &[Role::Admin, Role::Developer];
/// Roles that may list secret names/metadata (never values).
pub const SECRET_READ: &[Role] = &[Role::Admin, Role::Developer, Role::Auditor];
/// Roles that may query the audit log.
pub const AUDIT_READ: &[Role] = &[Role::Admin, Role::Auditor];
/// Users, grants, invites, node revocation, secrets management.
pub const ADMIN_ONLY: &[Role] = &[Role::Admin];
/// Roles that may publish a fact collector (M18 §4.5).
///
/// The same list as [`RUNNERS`], and that is the argument for it: deciding
/// what a node reports about itself is operational data, not governance,
/// and anyone who could publish a collector could already run the command
/// it runs.
pub const COLLECTORS: &[Role] = &[Role::Admin, Role::Operator, Role::Developer];
/// Roles that may manage schedules (M8 §3.4).
///
/// [`RUNNERS`] again, and for the same reason [`COLLECTORS`] is: a schedule
/// grants no new power. Every fire of it carries its creator's authority
/// and nothing more. A schedule is a person saying "do the thing I could
/// already do, at 03:00" — so the bar is the bar for doing the thing.
pub const SCHEDULES: &[Role] = &[Role::Admin, Role::Operator, Role::Developer];

/// Roles that may manage deployments (C2 §6.2).
///
/// The same door schedules and services use, for the same reason: a
/// deployment grants no new power. Every instance it starts carries its
/// creator's authority and nothing more, runs a service version the creator
/// could already run by hand, and is gated by the same change-control
/// policies a typed submission hits. A deployment is a person saying "keep
/// running the thing I could already run" — so the bar is the bar for
/// running it once.
pub const DEPLOYMENTS: &[Role] = &[Role::Admin, Role::Operator, Role::Developer];

/// Is this user allowed to act at all? The founder (from genesis) is the
/// trust anchor and is always active; everyone else must be a known,
/// non-revoked user.
pub fn is_user_active(state: &MeshState, founder: &[u8; 32], user: &UserId) -> bool {
    if user.0 == *founder {
        return true;
    }
    state.users.get(user).is_some_and(|u| u.revoked.is_none())
}

/// Is this principal allowed to act **at `at_micros`**?
///
/// [`is_user_active`] answers "is this key known and not revoked". This
/// adds the machine-principal clock (E1-P4): a [`ServiceAccount`] past its
/// `expires_at_micros` cannot act at all, whatever grants it still holds.
///
/// **Two separate expiries, deliberately.** A `Grant` expiring leaves a
/// live key with no roles — recoverable by granting it again, which is
/// what you want when a person's on-call window lapses. An *account*
/// expiring is terminal for that credential: re-granting does nothing,
/// because the principal itself is out of time. That is the property a
/// short-lived CI credential needs, and the reason the expiry lives on the
/// account rather than only on its grants.
pub fn is_principal_active(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    at_micros: u64,
) -> bool {
    if !is_user_active(state, founder, user) {
        return false;
    }
    !state
        .service_accounts
        .get(user)
        .is_some_and(|a| a.expired_at(at_micros))
}

/// Person or machine — a projection over state, never a stored field.
///
/// Keeping this derived is what let E1-P4 ship without adding a field to
/// [`mesh_proto::AuditEntry`], whose postcard encoding is already signed
/// into every audit op ever written.
pub fn actor_type(state: &MeshState, user: &UserId) -> mesh_proto::ActorType {
    if state.service_accounts.contains_key(user) {
        mesh_proto::ActorType::Machine
    } else {
        mesh_proto::ActorType::Person
    }
}

/// May this principal hold `role`?
///
/// A service account may not be an admin (see [`mesh_proto::ServiceAccount`]).
/// Checked at creation *and* at merge, because a check that only runs on the
/// node a person typed on is a check a modified client walks around.
pub fn grant_is_permitted(
    state: &MeshState,
    user: &UserId,
    role: Role,
) -> std::result::Result<(), String> {
    if role == Role::Admin && state.service_accounts.contains_key(user) {
        return Err(format!(
            "{} is a service account, and a service account may not hold the admin role. \
             An admin key can mint users and grants, so a leaked pipeline credential holding \
             it is permanent mesh compromise. Grant a narrower role, or use a person's key \
             and own that decision.",
            state
                .service_accounts
                .get(user)
                .map(|a| a.name.as_str())
                .unwrap_or("this principal")
        ));
    }
    Ok(())
}

/// The user's active grants at `at_micros` (revocation checked as of now —
/// revocation is not retroactive, it stops future evaluation).
pub fn active_grants<'a>(state: &'a MeshState, user: &UserId, at_micros: u64) -> Vec<&'a Grant> {
    state
        .grants
        .values()
        .filter(|g| !g.revoked && g.grant.user_id == *user)
        .filter(|g| g.grant.expires_at_micros.is_none_or(|e| at_micros <= e))
        .map(|g| &g.grant)
        .collect()
}

/// Effective roles at a point in time. The founder holds an implicit,
/// unscoped admin role.
pub fn roles_at(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    at_micros: u64,
) -> BTreeSet<Role> {
    // `is_principal_active`, not `is_user_active`: an expired service
    // account holds no roles at all (E1-P4). One choke point rather than a
    // check at each caller — every authorization path in the daemon and
    // every merge-time check reaches this function, so a credential that
    // has run out cannot act anywhere, including on a node that has not
    // heard from an admin since.
    if !is_principal_active(state, founder, user, at_micros) {
        return BTreeSet::new();
    }
    let mut roles: BTreeSet<Role> = active_grants(state, user, at_micros)
        .iter()
        .map(|g| g.role)
        .collect();
    if user.0 == *founder {
        roles.insert(Role::Admin);
    }
    roles
}

pub fn has_role(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    allowed: &[Role],
    at_micros: u64,
) -> bool {
    let roles = roles_at(state, founder, user, at_micros);
    allowed.iter().any(|r| roles.contains(r))
}

/// API-time check with a human-readable denial.
pub fn require_role(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    allowed: &[Role],
    at_micros: u64,
) -> Result<(), String> {
    if has_role(state, founder, user, allowed, at_micros) {
        return Ok(());
    }
    // An expired machine credential is its own answer. Left to fall
    // through, it would report as "requires one of: developer" — advice to
    // grant a role to a principal that can no longer act however it is
    // granted, which sends an operator to fix the wrong thing.
    if let Some(account) = state.service_accounts.get(user)
        && account.expired_at(at_micros)
    {
        return Err(format!(
            "service account {} expired at {} — an expired account cannot act however it is \
             granted. Create a replacement with `meshnode serviceaccounts create {} …` and \
             give the new credential to whatever was using this one",
            account.name,
            crate::localtime::describe_instant(account.expires_at_micros.unwrap_or(0)),
            account.name,
        ));
    }
    if !is_user_active(state, founder, user) {
        // Revoked and never-added are *different problems with different
        // answers*, and collapsing them sent people to `users add` for a
        // key that can never be added again. Worse, the caller is often
        // not a person at all: a task talking back to the mesh signs with
        // the executing node's own `identity/user.key`, so "an admin can
        // add you" was advice addressed to a machine.
        return Err(match state.users.get(user) {
            Some(_) => format!(
                "user {} was revoked, and revocation is permanent — this key \
                 cannot be re-enrolled. Generate a new identity on that \
                 machine and enrol it with `meshnode enrol`",
                user.short()
            ),
            None => format!(
                "unknown user {} — whoever holds this key can ask for it with \
                 `meshnode enrol`, or an admin can add it directly with \
                 `meshnode users add <name> --key <mwu1…> --role developer`",
                user.short()
            ),
        });
    }
    let need: Vec<&str> = allowed.iter().map(|r| r.as_str()).collect();
    Err(format!("requires one of: {}", need.join(", ")))
}

/// May `user` cancel the job submitted by `submitter` — and if not, *why
/// not and who can*?
///
/// Cancel is the one RUNNERS verb whose target belongs to somebody, so a
/// flat role list is the wrong shape for it:
///
/// - **admin, operator**: any job, forced or not. Full power is the point —
///   a stuck job is an operational problem, and the people who operate the
///   mesh end it.
/// - **developer**: their own jobs, by the ordinary route. Not other
///   people's — a cancel is the one way one developer's slip can destroy
///   another's running work. And not *forced*: a force overrides the
///   executor's record, which is a statement about the mesh's history, not
///   about one's own job.
/// - everyone else: nothing, told in terms of what would work.
///
/// One function for the daemon gate and the refusal prose, so who-may and
/// the message saying so can never drift apart. The op-apply side enforces
/// the same rule from [`CANCEL_ANY`] — a force op is admin/operator
/// authority by construction, checked again on every peer.
pub fn require_cancel(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    submitter: &UserId,
    submitter_name: Option<&str>,
    force: bool,
    at_micros: u64,
) -> Result<(), String> {
    if !is_user_active(state, founder, user) {
        // The unknown-key and revoked-key guidance lives in `require_role`;
        // both answers are the same regardless of whose job it is.
        return require_role(state, founder, user, RUNNERS, at_micros);
    }
    let roles = roles_at(state, founder, user, at_micros);
    if CANCEL_ANY.iter().any(|r| roles.contains(r)) {
        return Ok(());
    }
    if roles.contains(&Role::Developer) {
        if force {
            return Err(
                "force-cancel rewrites the job's record without its executor's say-so and \
                 needs operator or admin — ask one to force it"
                    .into(),
            );
        }
        if submitter == user {
            return Ok(());
        }
        let owner = submitter_name
            .map(str::to_string)
            .unwrap_or_else(|| submitter.short());
        return Err(format!(
            "developers may cancel only their own jobs, and this one was submitted by \
             {owner} — an operator or admin can cancel any job"
        ));
    }
    let held = roles
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let held = if held.is_empty() {
        "no role".to_string()
    } else {
        format!("only: {held}")
    };
    Err(format!(
        "cancelling jobs needs developer (your own), operator or admin (any job) — \
         you hold {held}"
    ))
}

/// May `user` run work on `member`? A grant authorizes a run when its role
/// can run, its node selector matches the target, and its service glob
/// matches the service name (ad-hoc commands need an unrestricted `*`
/// services scope — governance is available, not mandatory, but scoping is
/// real when granted).
pub fn can_run_on(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    member: &MemberInfo,
    service: Option<&str>,
    at_micros: u64,
) -> bool {
    if !is_user_active(state, founder, user) {
        return false;
    }
    if user.0 == *founder {
        return true;
    }
    active_grants(state, user, at_micros)
        .iter()
        .filter(|g| matches!(g.role, Role::Admin | Role::Operator | Role::Developer))
        .any(|g| {
            Selector::parse(&g.nodes).matches_in(state, member)
                && glob_match(&g.services, service.unwrap_or(""))
        })
}

/// May `user` create/rollback/remove the service named `name`?
pub fn can_manage_service(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    name: &str,
    at_micros: u64,
) -> bool {
    if !is_user_active(state, founder, user) {
        return false;
    }
    if user.0 == *founder {
        return true;
    }
    active_grants(state, user, at_micros)
        .iter()
        .filter(|g| matches!(g.role, Role::Admin | Role::Developer))
        .any(|g| glob_match(&g.services, name))
}

/// May `user` publish/rollback/remove the workflow named `name`?
///
/// Deliberately the same grant scope as services (`Grant::services`): a
/// workflow is a content entity, a grant that says "you own `deploy-*`"
/// should cover the workflow that deploys as well as the service it calls,
/// and inventing a second glob field would change `Grant`'s encoding and
/// invalidate every grant ever signed (§2.4).
pub fn can_manage_workflow(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    name: &str,
    at_micros: u64,
) -> bool {
    can_manage_service(state, founder, user, name, at_micros)
}

/// Display name for a user: their username if known, short id otherwise.
pub fn username_of(state: &MeshState, founder: &[u8; 32], user: &UserId) -> String {
    if let Some(u) = state.users.get(user) {
        return u.record.username.clone();
    }
    if user.0 == *founder {
        return "founder".to_string();
    }
    user.short()
}

/// Anchored `*`-wildcard glob. `*` matches any (possibly empty) substring;
/// there are no character classes — service names are simple identifiers.
///
/// **One implementation** (S-Misc): `mesh_proto::glob_match`, re-exported
/// rather than wrapped so `rbac::glob_match` still reads as the rule it is.
/// A glob that decides authorization (`Grant::services`) must not be two
/// functions that could ever answer differently.
pub use mesh_proto::glob_match;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_semantics() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("deploy-*", "deploy-web"));
        assert!(!glob_match("deploy-*", "restart-web"));
        assert!(!glob_match("deploy-*", ""));
        assert!(glob_match("a*b*c", "a-x-b-y-c"));
        assert!(!glob_match("a*b*c", "acb"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }
}
