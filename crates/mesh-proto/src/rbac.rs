//! Users, roles, grants, and audit records (MASTERPLAN §7).
//!
//! Every governance artifact carries a signature by the *user* who authored
//! it, verified at merge time on every node — a compromised node cannot
//! smuggle privilege changes into replicated state (§4.4). The signature
//! covers the postcard encoding of the artifact with its `signature` field
//! emptied, prefixed by a per-type domain tag.

use serde::{Deserialize, Serialize};

use crate::{AuditId, GrantId, NodeId, UserId};

/// Built-in roles (MASTERPLAN §7.2). Deliberately small but real; custom
/// role definitions are explicitly deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Role {
    /// Everything.
    Admin,
    /// Run/cancel jobs, drain nodes.
    Operator,
    /// Manage services/content/contracts, run.
    Developer,
    /// Read everything including the audit log, run nothing.
    Auditor,
    /// Read except secrets and audit.
    Viewer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Developer => "developer",
            Role::Auditor => "auditor",
            Role::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "admin" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            "developer" => Some(Role::Developer),
            "auditor" => Some(Role::Auditor),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

/// Domain-tagged signing bytes for a signable artifact: the postcard
/// encoding of the value (with signature cleared by the caller) behind a
/// per-type prefix.
pub(crate) fn tagged_bytes<T: Serialize>(domain: &[u8], value: &T) -> Vec<u8> {
    // **Encoded straight into the tagged buffer** (**M-17**). This built a
    // whole `Vec` with `to_stdvec` and then copied it in behind the domain
    // tag, so every signature and every *verification* allocated and copied
    // the encoded record twice over. `to_extend` appends in place.
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(domain);
    postcard::to_extend(value, buf).unwrap_or_default()
}

/// Glob match with `*` as the only wildcard: the rule a grant's service
/// scope and an identity provider's group mapping both use.
///
/// Lives in the protocol crate so that `mesh-core` and anything else
/// reading these records decide a match the same way — a second, subtly
/// different glob is the kind of divergence that only shows up as a grant
/// that did not apply.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// A mesh user: a key bound to a username (MASTERPLAN §7.1). Roles come
/// from separate [`Grant`]s so they can be scoped and expire independently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserRecord {
    pub user_id: UserId,
    pub username: String,
    pub created_at_micros: u64,
    /// The admin who added this user; their signature authorizes the add.
    pub added_by: UserId,
    /// Ed25519 signature by `added_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl UserRecord {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-user-add-v1", &unsigned)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserRevocation {
    pub user_id: UserId,
    pub reason: String,
    pub revoked_by: UserId,
    pub at_micros: u64,
    pub signature: Vec<u8>,
}

impl UserRevocation {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-user-revoke-v1", &unsigned)
    }
}

/// A scoped role grant (MASTERPLAN §7.2): `{user, role, selector, expiry?}`,
/// signed by an admin, replicated as state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Grant {
    pub grant_id: GrantId,
    pub user_id: UserId,
    pub role: Role,
    /// Node selector this grant is constrained to (`any` = all nodes).
    pub nodes: String,
    /// Service-name glob this grant is constrained to (`*` = all services,
    /// which is also what ad-hoc commands require).
    pub services: String,
    pub expires_at_micros: Option<u64>,
    pub created_at_micros: u64,
    pub granted_by: UserId,
    /// Ed25519 signature by `granted_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl Grant {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-grant-add-v1", &unsigned)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrantRevocation {
    pub grant_id: GrantId,
    pub revoked_by: UserId,
    pub at_micros: u64,
    pub signature: Vec<u8>,
}

impl GrantRevocation {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-grant-revoke-v1", &unsigned)
    }
}

/// Admin-authorized node expulsion (replaces M1's founder-node-only rule).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeRevocation {
    pub node_id: NodeId,
    pub reason: String,
    pub by: UserId,
    pub at_micros: u64,
    pub signature: Vec<u8>,
}

impl NodeRevocation {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-node-revoke-v1", &unsigned)
    }
}

/// A **machine principal** (E1-P4): a key that belongs to a system rather
/// than to a person.
///
/// ## Why this is a marker and not a new principal type
///
/// meshware's identity model is already the right one for machines — the
/// key *is* the identity, requests are signed with it, and roles attach to
/// it through scoped [`Grant`]s that already carry an expiry, a node
/// selector and a service glob. A CI runner needs none of that reinvented.
///
/// What it needed was three things a [`UserRecord`] alone cannot say:
///
/// 1. **That the principal is not a person.** An audit row reading
///    `github-ci` is ambiguous until something says whether that is a
///    human's account or a pipeline's. Actor type is then a *projection*
///    over state rather than a field on [`AuditEntry`] — which is what
///    lets this ship without touching a struct whose encoding is already
///    signed into thousands of existing ops.
/// 2. **An expiry on the principal itself**, not only on its grants. A
///    grant that lapses leaves a live key with no roles, which an admin
///    can re-grant by accident. An expired *account* cannot act at all,
///    and re-granting it does nothing.
/// 3. **A creation flow that mints the key**, so the credential handed to
///    a pipeline was never a person's and never lived in a person's
///    `identity/` directory.
///
/// ## The admin refusal
///
/// A service account **may not hold `Role::Admin`**, refused both at
/// creation and at merge. An admin key can mint users and grants, so a
/// leaked pipeline credential holding it is total and permanent mesh
/// compromise — and the entire reason to have a machine principal is that
/// it is narrower than a person's. This is the same shape as `upgrade
/// --on any` being refused: the dangerous thing is not made difficult, it
/// is made impossible, and the escape hatch is to use a human key and own
/// the decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceAccount {
    /// The same [`UserId`] its [`UserRecord`] carries — a service account
    /// *is* a user, plus this marker.
    pub user_id: UserId,
    /// Snapshot of the username, so an audit reader needs one lookup
    /// rather than two.
    pub name: String,
    /// Free text: which pipeline, which system, who to ask about it.
    pub description: String,
    pub created_at_micros: u64,
    /// The admin who created it; their signature authorizes it.
    pub created_by: UserId,
    /// When this principal stops being able to act **at all**.
    ///
    /// `None` is permitted and is not the default anywhere a person types:
    /// `meshnode serviceaccounts create` requires `--expires-in` or an
    /// explicit `--never-expires`, because a non-expiring machine
    /// credential is exactly the long-lived bearer token this feature
    /// exists to stop being.
    pub expires_at_micros: Option<u64>,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl ServiceAccount {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        tagged_bytes(b"mw-service-account-add-v1", &unsigned)
    }

    /// Has this principal's own clock run out at `at_micros`?
    pub fn expired_at(&self, at_micros: u64) -> bool {
        self.expires_at_micros.is_some_and(|e| at_micros > e)
    }
}

/// What kind of principal acted — a projection over state, never a stored
/// field (see [`ServiceAccount`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActorType {
    Person,
    Machine,
}

impl ActorType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActorType::Person => "person",
            ActorType::Machine => "machine",
        }
    }
}

/// One audit event (MASTERPLAN §7.4): who did what, where, and how it went.
/// Replicated grow-only; `request_sig` carries the user's original request
/// signature so an auditor can independently verify provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditEntry {
    pub audit_id: AuditId,
    pub at_micros: u64,
    pub user: UserId,
    /// Username snapshot at record time (usernames are immutable, but the
    /// snapshot keeps audit rows readable even for later-revoked users).
    pub username: String,
    /// Dotted action name: `job.run`, `job.cancel`, `service.create`,
    /// `secret.set`, `secret.resolve`, `user.add`, `grant.add`, …
    pub action: String,
    /// What the action touched: a job id, service name, secret name, …
    pub target: String,
    /// The node that served (or executed) the action.
    pub node: NodeId,
    /// `ok` or `denied: <reason>`.
    pub outcome: String,
    /// Human context (command preview, executor, …).
    pub detail: String,
    /// The user's signature from the request envelope that triggered this
    /// action; empty for node-internal events (e.g. secret resolution).
    pub request_sig: Vec<u8>,
}

/// An audit entry plus everything needed to **verify it offline** (M31 B4,
/// §19 Q1 decided).
///
/// ## Why the row alone is not enough
///
/// An audit log's value in a compliance conversation is non-repudiation,
/// and that does not live in the projection — it lives in the signature
/// over the op that recorded it. A WORM bucket full of flat rows proves
/// nobody edited *your copy*; it says nothing about what meshware actually
/// signed. Exporting the row and calling it an audit trail is the version
/// of this feature that fails the one conversation it exists for.
///
/// ## What makes this verifiable with no meshware at all
///
/// Three fields, and the trick is that the first one is already a key:
///
/// - [`Self::author`] is a [`NodeId`], and a `NodeId` **is** that node's
///   Ed25519 public key (`Keypair::node_id` returns its public bytes). The
///   verifier ships with the row.
/// - [`Self::signature`] is that node's signature.
/// - [`Self::signing_bytes`] is the exact byte string it covers, carried
///   verbatim rather than reconstructed.
///
/// That last choice is the one worth defending. The bytes *are* derivable —
/// they are `op_signing_bytes_raw(author, seq, hlc, postcard(body))` — and
/// deriving them would save about a hundred bytes a row. It would also mean
/// an auditor could check a signature only by owning a meshware build whose
/// framing had not changed since the row was written, which is exactly the
/// dependency a compliance archive exists to remove. Carrying the bytes
/// makes verification `ed25519_verify(author, signature, signing_bytes)` —
/// forty lines in any language, no meshware, no schema knowledge, for ever.
///
/// [`AuditEntry::request_sig`] is a *different* signature kept for a
/// different reason: it is the **user's**, over the IPC request that caused
/// the action. It cannot be verified from this row — the signed payload is
/// not carried — and it is exported anyway because it ties the recorded
/// action to a client envelope. **Two signatures, one verifiable, and the
/// difference is stated rather than blurred.**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedAuditRow {
    pub entry: AuditEntry,
    /// The node that authored the op recording this entry.
    ///
    /// Equal to `entry.node` in every row this mesh writes, and carried
    /// separately because the signature is verified against *this* one: a
    /// reader must be able to **check** that the two agree rather than
    /// assume it.
    pub author: NodeId,
    pub seq: u64,
    pub hlc: crate::Hlc,
    /// The authoring node's Ed25519 signature over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
    /// The exact bytes covered by [`Self::signature`].
    pub signing_bytes: Vec<u8>,
}

impl SignedAuditRow {
    /// Do the signed bytes actually encode *this* entry?
    ///
    /// Re-frames the op from the row and compares. Deliberately separate
    /// from checking the signature: a row whose `signing_bytes` were
    /// swapped for another genuine row's would pass a signature check and
    /// fail this one, and a verifier that conflated the two would report
    /// the wrong problem. This half needs meshware; the signature half
    /// needs only Ed25519.
    pub fn binds_its_entry(&self) -> bool {
        let Ok(body) = postcard::to_stdvec(&crate::OpBody::AuditAppend {
            entry: self.entry.clone(),
        }) else {
            return false;
        };
        crate::op_signing_bytes_raw(&self.author, self.seq, &self.hlc, &body) == self.signing_bytes
            && self.author == self.entry.node
    }
}

#[cfg(test)]
mod tagged_bytes_shape {
    use super::*;

    /// **M-17**: `tagged_bytes` switched from `to_stdvec` + copy to
    /// `to_extend` in place. That is an allocation change and must be a
    /// *byte-identical* one — these bytes are what signatures cover, so any
    /// difference would invalidate every record already signed and split
    /// the mesh between builds.
    ///
    /// Checked against the previous implementation, spelled out here.
    #[test]
    fn to_extend_matches_the_old_to_stdvec_form() {
        fn old<T: Serialize>(domain: &[u8], value: &T) -> Vec<u8> {
            let mut buf = Vec::with_capacity(128);
            buf.extend_from_slice(domain);
            buf.extend_from_slice(&postcard::to_stdvec(value).unwrap_or_default());
            buf
        }
        let user = UserRecord {
            user_id: crate::UserId([3u8; 32]),
            username: "ada".into(),
            created_at_micros: 1_700_000_000_000_000,
            added_by: crate::UserId([4u8; 32]),
            signature: vec![9u8; 64],
        };
        assert_eq!(
            tagged_bytes(b"mw-user-add-v1", &user),
            old(b"mw-user-add-v1", &user)
        );
        // And with the signature blanked, which is the form actually signed.
        let mut unsigned = user.clone();
        unsigned.signature = Vec::new();
        assert_eq!(
            tagged_bytes(b"mw-user-add-v1", &unsigned),
            old(b"mw-user-add-v1", &unsigned)
        );
        // An empty domain and an empty value, for the boundary.
        assert_eq!(tagged_bytes(b"", &0u8), old(b"", &0u8));
    }

    /// The bytes a signature covers must not depend on the *capacity* the
    /// buffer happened to start with — `to_extend` reallocating mid-write
    /// must produce the same output as one that never grows.
    #[test]
    fn a_value_larger_than_the_reserved_capacity_encodes_identically() {
        let big = UserRecord {
            user_id: crate::UserId([1u8; 32]),
            username: "x".repeat(4096),
            created_at_micros: 0,
            added_by: crate::UserId([2u8; 32]),
            signature: Vec::new(),
        };
        let a = tagged_bytes(b"mw-user-add-v1", &big);
        let mut expect = Vec::new();
        expect.extend_from_slice(b"mw-user-add-v1");
        expect.extend_from_slice(&postcard::to_stdvec(&big).expect("encode"));
        assert_eq!(a, expect);
    }
}
