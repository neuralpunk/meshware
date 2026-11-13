//! Node-to-node wire messages over QUIC.
//!
//! Connection lifecycle: the dialer opens one *handshake stream* and
//! exchanges `ConnHello`/`ConnHelloAck` once per connection. Every
//! subsequent bi-stream carries one `Request` frame followed by one or more
//! `Response` frames (streaming responses end with a terminal frame).

use serde::{Deserialize, Serialize};

use crate::content::Manifest;
use crate::gang::{GangOfferReply, GangTicket};
use crate::hosts::HostProbeReport;
use crate::objects::{ObjectManifest, ObjectRef};
use crate::parallel::{ChunkAssignment, ChunkReport, ParallelPlan};
use crate::{
    BundleId, ChunkHash, GangId, JobId, JobSpec, JobSummary, LivenessEntry, MeshId, NodeId,
    ObjectId, Op, OpEnvelope, Requirements, VersionVector,
};

/// Why this connection was opened. Non-members may only open `Join`
/// connections; everything else requires membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnPurpose {
    /// Normal mesh traffic between members.
    Member,
    /// A prospective node presenting an invite token.
    Join,
    /// Diagnostic reachability check (`meshnode ping`); allowed cross-mesh,
    /// but only Ping requests are served.
    Probe,
    /// LAN pairing (MASTERPLAN §5.2): a mesh member dials a candidate node
    /// that is waiting in `meshnode join --pair`. Allowed cross-mesh (the
    /// joiner has no mesh yet); only Pair* requests and Ping are served.
    Pair,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnHello {
    pub proto_major: u16,
    pub proto_minor: u16,
    pub purpose: ConnPurpose,
    pub node_id: NodeId,
    pub node_name: String,
    pub mesh_id: Option<MeshId>,
    pub mesh_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnHelloAck {
    pub proto_major: u16,
    pub proto_minor: u16,
    pub node_id: NodeId,
    pub node_name: String,
    pub mesh_id: Option<MeshId>,
    pub mesh_name: Option<String>,
    pub accepted: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Ping {
    pub nonce: u64,
    pub sent_at_micros: u64,
}

// ---------------------------------------------------------------------------
// Join
// ---------------------------------------------------------------------------

/// The invite token, decoded. Its display form is `mwj1<base32(postcard)>`.
/// Signed by an admin's *user* key; the founder (from genesis) is always a
/// valid signer, other admins are validated against replicated RBAC state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteToken {
    pub mesh_id: MeshId,
    pub invite_id: crate::InviteId,
    pub expires_at_micros: u64,
    pub uses: u32,
    pub tags: Vec<String>,
    /// Endpoints of nodes that can accept the join.
    pub bootstrap: Vec<String>,
    /// The admin user who minted this invite (the key the signature
    /// verifies against).
    pub created_by_user: crate::UserId,
    /// Signature by `created_by_user` over [`invite_signing_bytes`].
    pub signature: Vec<u8>,
}

pub fn invite_signing_bytes(t: &InviteToken) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(b"mw-invite-v1");
    buf.extend_from_slice(&t.mesh_id.0);
    buf.extend_from_slice(&t.invite_id.0);
    buf.extend_from_slice(&t.expires_at_micros.to_le_bytes());
    buf.extend_from_slice(&t.uses.to_le_bytes());
    for tag in &t.tags {
        buf.extend_from_slice(tag.as_bytes());
        buf.push(0);
    }
    for addr in &t.bootstrap {
        buf.extend_from_slice(addr.as_bytes());
        buf.push(0);
    }
    buf.extend_from_slice(&t.created_by_user.0);
    buf
}

const TOKEN_PREFIX: &str = "mwj1";

impl InviteToken {
    pub fn encode(&self) -> String {
        let bytes = postcard::to_stdvec(self).unwrap_or_default();
        format!(
            "{TOKEN_PREFIX}{}",
            crate::ids::ID_ENCODING.encode(&bytes).to_lowercase()
        )
    }

    pub fn decode(s: &str) -> Result<Self, crate::ProtoError> {
        let body = s
            .trim()
            .strip_prefix(TOKEN_PREFIX)
            .ok_or_else(|| crate::ProtoError::InvalidId("not an invite token".into()))?;
        let bytes = crate::ids::ID_ENCODING
            .decode(body.to_uppercase().as_bytes())
            .map_err(|_| crate::ProtoError::InvalidId("invite token corrupted".into()))?;
        Ok(postcard::from_bytes(&bytes)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub token: InviteToken,
    pub node_name: String,
    /// The joiner's listen port; the acceptor pairs it with the observed
    /// source IP to build a reachable candidate address.
    pub listen_port: u16,
    /// Extra explicit endpoints the joining node knows it is reachable on.
    pub addrs: Vec<String>,
    /// Extra tags from the joining node's own config.
    pub extra_tags: Vec<String>,
    /// The joiner's executor capacity (MASTERPLAN §11); refreshed by its
    /// boot-time MemberUpdate.
    pub slots: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinAccept {
    /// The genesis document (JSON bytes, exactly as stored on disk).
    pub genesis_json: Vec<u8>,
    /// Full op snapshot so the joiner starts converged.
    ///
    /// Served to a pre-1.7 joiner, this snapshot is cut at the first op the
    /// joiner could not decode, by the same rule and for the same reason as
    /// [`SyncResponse::vv`] (F0 §3.3).
    pub ops: Vec<Op>,
}

/// [`JoinAccept`] with length-delimited op bodies (protocol 1.7, F0).
///
/// A 1.7 node cannot serve the V1 snapshot faithfully once *any* op in its
/// log is one it only carries opaquely: rebuilding a `Vec<Op>` needs every
/// body decoded, and by construction it cannot decode that one. Handing a
/// joiner envelopes is the only shape that keeps a full snapshot full.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinAcceptV2 {
    pub genesis_json: Vec<u8>,
    pub ops: Vec<OpEnvelope>,
}

/// One instalment of a bootstrap snapshot (protocol 1.10).
///
/// [`JoinAcceptV2`] is this shape with `more` nailed to `false` and no way
/// to say otherwise, which is the whole bug: a snapshot is a frame, a frame
/// is capped, and an op-log is not. A chunk is the same payload cut to the
/// batch bound anti-entropy already uses, so the two paths share one
/// definition of "how much op fits in a message" and a mesh cannot grow
/// into a state where it can gossip but not admit.
///
/// The instalments are ordered and gapless per author — they come from the
/// same `missing_for` walk that serves sync — so the receiver can ingest as
/// they arrive or buffer and ingest once, and either way ends converged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChunk {
    /// The mesh's signed genesis. Sent on the **first** chunk and empty
    /// thereafter: it is the same bytes every time, and a joiner that
    /// cannot verify it has no reason to accept the ops behind it.
    pub genesis_json: Vec<u8>,
    pub ops: Vec<OpEnvelope>,
    /// More instalments follow. The chunk with `more == false` is the
    /// terminal one, and the only point at which the snapshot is complete
    /// enough to persist.
    pub more: bool,
}

// ---------------------------------------------------------------------------
// LAN pairing (MASTERPLAN §5.2)
// ---------------------------------------------------------------------------
//
// The AirDrop moment: a candidate node announces itself over mDNS and waits
// in `meshnode join --pair`; a mesh member (CLI `invite approve` or Director)
// dials it with purpose `Pair` and both sides derive the same 6-digit
// short-authentication-string, which both humans must confirm.
//
// The SAS is a commitment-based numeric comparison bound to the two node
// keys that TLS proved possession of on this very connection:
//
//   1. approver → PairInit   { commit = H(nonce_a) }        (commit first)
//   2. joiner   → PairAccept { nonce_j, join fields }
//   3. approver → PairReveal { nonce_a }                    (joiner checks commit)
//      code = H("mw-pair-sas-v1" ‖ approver_key ‖ joiner_key ‖ nonce_a ‖ nonce_j) mod 10⁶
//   4. approver → PairConfirm      (only after its human confirmed)
//      joiner   → PairDecision     (only after *its* human confirmed)
//   5. approver → PairComplete { genesis + ops }  (the normal JoinAccept)
//
// A MITM bridging two pairing connections shows each side a different code:
// it must commit its nonce on one leg before learning the other leg's, and
// it cannot present either victim's node key (TLS requires possession), so
// the two transcripts differ and the truncated hashes agree only with
// probability 10⁻⁶ per attempt. Mandatory confirmation on *both* sides is
// the anti-goal fence: there is no TOFU fallback (MASTERPLAN §5).

/// First frame from the approver: a commitment to its nonce, plus context
/// for the joiner's prompt ("Meshware `hyacinth` wants to pair").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairInit {
    /// BLAKE3 of `"mw-pair-commit-v1" ‖ nonce_a`.
    pub commitment: [u8; 32],
    pub mesh_name: String,
    pub approver_name: String,
}

/// The joiner's reply: its nonce plus everything the acceptor needs to
/// build the membership record after confirmation (mirrors [`JoinRequest`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairAccept {
    pub nonce: [u8; 32],
    pub node_name: String,
    /// The port the joiner will listen on once it runs `meshnode up`.
    pub listen_port: u16,
    pub addrs: Vec<String>,
    pub extra_tags: Vec<String>,
    /// The joiner's executor capacity (MASTERPLAN §11).
    pub slots: u32,
}

// ---------------------------------------------------------------------------
// Sync (anti-entropy)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    /// What the requester already has.
    pub vv: VersionVector,
    /// The requester's current liveness observations.
    pub liveness: Vec<LivenessEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    /// Ops the requester is missing (bounded batch; ask again if `more`).
    pub ops: Vec<Op>,
    pub more: bool,
    /// The responder's version vector, so the requester can push back what
    /// the responder is missing.
    ///
    /// Against a pre-1.7 requester this is **truncated** at the first op
    /// the requester could not decode (F0 §3.3): the peer is told it is
    /// caught up as far as it can actually be given, so it stops retrying
    /// a batch that can never land and converges on the subset of state it
    /// has code for.
    pub vv: VersionVector,
    pub liveness: Vec<LivenessEntry>,
}

// -- Sync V2 (protocol 1.7, F0) ---------------------------------------------
//
// Field-identical to the V1 pair except that ops carry length-delimited
// bodies. Deliberately *new structs* rather than a reshaped `SyncRequest`:
// these are embedded positionally inside `Request`/`Response`, and adding a
// field to something embedded positionally is the trap M9 §3.3 hit on
// `LivenessEntry` and F0 exists to stop repeating.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequestV2 {
    pub vv: VersionVector,
    pub liveness: Vec<LivenessEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponseV2 {
    /// Ops the requester is missing, bodies opaque (bounded batch).
    pub ops: Vec<OpEnvelope>,
    pub more: bool,
    pub vv: VersionVector,
    pub liveness: Vec<LivenessEntry>,
}

// ---------------------------------------------------------------------------
// Requests / responses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Ping(Ping),
    Join(JoinRequest),
    Sync(SyncRequest),
    /// Push ops the receiver is known to be missing (reverse leg of sync).
    PushOps {
        ops: Vec<Op>,
    },
    SubmitJob(JobSpec),
    CancelJob {
        job_id: JobId,
    },
    /// Stream a job's output from `offset` bytes into each log; if `follow`,
    /// keep streaming until the job ends.
    FollowJob {
        job_id: JobId,
        stdout_offset: u64,
        stderr_offset: u64,
        follow: bool,
    },
    /// Fetch a bundle's manifest from a holder.
    GetManifest {
        bundle_id: BundleId,
    },
    /// Fetch a single content chunk by hash.
    GetChunk {
        hash: ChunkHash,
    },
    // -- LAN pairing (purpose = Pair; approver is always the dialer) -------
    /// Round 1: commit to the approver nonce.
    PairInit(PairInit),
    /// Round 2: reveal the nonce; the joiner verifies the commitment and
    /// both sides can now derive + display the confirmation code.
    PairReveal {
        nonce: [u8; 32],
    },
    /// Round 3: the approver's human confirmed the code. The joiner answers
    /// with its own human's decision (this request blocks on a person).
    PairConfirm,
    /// Round 4: membership was authored; deliver genesis + ops so the
    /// joiner starts converged (same payload as a token join).
    PairComplete(Box<JoinAccept>),
    // -- WAN (protocol 1.5, MASTERPLAN M7) ----------------------------------
    //
    // postcard is positional: these variants are appended at the tail so a
    // 1.4 peer's decoder never sees them in a position it already assigned.
    // A 1.5 node sends them only to peers that negotiated minor >= 5.
    /// Reflexive address discovery: "what address do you see me from?"
    /// Answered in the transport from the connection's remote address,
    /// never over a relayed connection (the answer would be synthetic).
    Observe,
    /// Register with a relay-capable member (MASTERPLAN M7, DERP-style):
    /// the stream stays open for the life of the registration and carries
    /// [`RelayServerFrame`]s relay→client and [`RelayFrame`]s client→relay.
    RelayRegister,
    /// Open a relayed path to `target` (a node registered at this relay).
    /// The stream stays open; it carries opaque frames between the
    /// initiator and `target`. The relay never sees plaintext — the inner
    /// QUIC session is a normal key-pinned connection end to end.
    RelayOpen {
        target: NodeId,
    },
    /// Ask the relay to tell `target` (registered there) to dial us back
    /// on `candidates` — coordinated hole-punch. Answered with `PunchSent`.
    PunchRequest {
        target: NodeId,
        candidates: Vec<String>,
    },
    // -- Compute (protocol 1.6, MASTERPLAN M9) ------------------------------
    //
    // Appended at the tail for the same positional reason as the 1.5 block
    // above. Live pressure needs its own request pair rather than a field
    // on `LivenessEntry`: that struct is embedded *positionally* inside
    // `SyncRequest`/`SyncResponse`, so growing it would change the encoding
    // of every gossip message and a 1.5 peer would misparse all of them
    // (M9 §3.3). Adding a variant is safe where adding a field is not.
    /// "How loaded are you right now?" Answered with [`Response::Pressure`].
    /// Sent only to peers that negotiated minor >= `COMPUTE_MINOR`, and
    /// only on a connection the gossip tick already opened.
    Pressure,
    /// Owner → worker: "there is a parallel job you can help with; its
    /// chunks are mine to hand out." The worker answers `Ok` and starts
    /// pulling; it answers `Busy` if it has no room. Re-sent by an adopting
    /// owner after the original one dies, which is how workers re-home
    /// (M9 §5.4).
    ParallelOffer {
        plan: Box<ParallelPlan>,
        /// How many chunks the owner expects this worker to hold at once —
        /// its advertised capacity. The worker caps it by its own.
        lanes: u32,
    },
    /// Worker → owner: "give me the next chunk." Answered with
    /// [`Response::ChunkWork`]; `None` means the job has no work left,
    /// which is how a worker learns to stop.
    ///
    /// `completed` carries item indices this worker already finished and
    /// the owner may not know about — empty on every ordinary pull, and
    /// populated exactly once when a worker re-homes onto an owner that
    /// adopted the job after the original one died (M9 §5.4).
    ChunkPull {
        job: JobId,
        completed: Vec<u32>,
    },
    /// Worker → owner: one chunk finished, here is what every item did.
    ChunkDone(Box<ChunkReport>),
    /// Producer → directory node: "I now hold these objects." Batched
    /// per directory node, because a chunk of a thousand items produces a
    /// thousand objects and a round trip each would cost more than the
    /// work (M9 §5.5).
    ///
    /// The receiver records the *TLS-proven* sender as the holder; a node
    /// cannot announce that somebody else holds something.
    ObjectAnnounce {
        job: JobId,
        objects: Vec<(ObjectId, u64)>,
    },
    /// Anyone → directory node: "who holds this object?" Answered with
    /// [`Response::ObjectHolders`].
    ObjectLocate {
        object: ObjectId,
    },
    /// Fetcher → holder: the object's chunk list. The chunks themselves
    /// come back through the existing `GetChunk` path — a chunk hash is a
    /// global content address, so the fetch path does not care which class
    /// of object a chunk belongs to.
    GetObject {
        object: ObjectId,
    },
    // -- Forward-compatible op-log (protocol 1.7, F0) -----------------------
    //
    // Appended at the tail for the same positional reason as the 1.5 and
    // 1.6 blocks above. These are the V1 op-carrying messages with
    // length-delimited bodies; a node speaks V2 to peers negotiating
    // >= OPLOG_MINOR and V1 to everyone else, and both legs feed the same
    // `StateEngine::ingest`.
    /// Anti-entropy pull with opaque op bodies. Answered with
    /// [`Response::SyncV2`].
    SyncV2(SyncRequestV2),
    /// Reverse leg of [`Self::SyncV2`]: push ops the receiver is missing.
    PushOpsV2 {
        ops: Vec<OpEnvelope>,
    },
    /// Join with an opaque-body snapshot. Answered with
    /// [`Response::JoinAcceptV2`]. The joiner picks this over
    /// [`Self::Join`] once the handshake reports minor >= OPLOG_MINOR,
    /// which it always knows before its first request.
    JoinV2(Box<JoinRequest>),
    /// Terminal frame of a 1.7 pairing handshake — [`Self::PairComplete`]
    /// with an opaque-body snapshot.
    PairCompleteV2(Box<JoinAcceptV2>),
    // -- Inventory reachability (protocol 1.8, M12 §6.3 P4) ------------------
    /// **Retired: the host inventory is gone.** Was: "which of these hosts
    /// can you currently see?"
    ///
    /// Kept in place for the reason `ServiceConnect` is: postcard encodes a
    /// variant by index, and an older peer that still asks gets a readable
    /// `Error` rather than a dropped stream.
    HostProbes {
        hosts: Vec<String>,
    },
    // -- The fabric (protocol 1.9, M17 §3.3) --------------------------------
    //
    // Appended at the tail for the same positional reason as every block
    // above it, and served in the transport rather than forwarded to the
    // daemon as a `RequestEvent` — because the daemon has to be handed
    // *both* halves of the stream, which `RequestEvent`'s reply channel
    // cannot express. This is `RelayRegister`'s shape (R2) reached through
    // `set_punch_handler`'s idiom (R3), and it is the only other place in
    // the system where a peer keeps writing after the request frame.
    /// Upgrade this stream to a session: a shell, an exec, a file copy or
    /// one forwarded TCP connection. Thereafter the stream carries
    /// [`crate::SessionFrame`]s initiator→host and
    /// [`crate::SessionServerFrame`]s host→initiator until either end
    /// closes it.
    SessionOpen(Box<crate::SessionSpec>),
    // -- Chunked bootstrap (protocol 1.10) ----------------------------------
    //
    // Appended at the tail for the same positional reason as every block
    // above it. These replace a single unbounded frame with the pull loop
    // `SyncV2` has always used, in whichever direction the handshake
    // already put the client.
    /// Join with an instalment-delivered snapshot. Answered with
    /// [`Response::Snapshot`] carrying the *first* chunk; the joiner then
    /// asks for the rest with [`Self::JoinFetch`].
    ///
    /// Distinct from [`Self::JoinV2`] rather than a reshape of it, because
    /// the acceptor must know which shape to answer with before it commits
    /// the membership op — and the request variant is the only place that
    /// intent can live.
    JoinV3(Box<JoinRequest>),
    /// "Send me what I am still missing." The continuation leg of
    /// [`Self::JoinV3`], answered with [`Response::Snapshot`].
    ///
    /// Carries no token: by the time a joiner sends this the acceptor has
    /// already authored its `MemberAdd`, so the acceptor authorizes it the
    /// way it authorizes a member — anyone else asking on a join-purpose
    /// connection is not in the mesh and is refused. That is a stronger
    /// check than re-presenting the invite, which by then is spent.
    JoinFetch {
        /// What the joiner has accumulated so far.
        vv: VersionVector,
    },
    /// One instalment of a paired joiner's snapshot, answered with
    /// [`Response::Ok`]. The approver dials the joiner, so here the
    /// *sender* drives the loop and the chunk's `more` flag tells the
    /// joiner which frame completes the mesh.
    PairCompleteV3(Box<SnapshotChunk>),
    // -- Bundle prefetch (protocol 1.11, M24 §3.2) --------------------------
    //
    // Appended at the tail for the same positional reason as every block
    // above it.
    //
    // The one request in this enum that asks a peer to do something it was
    // already going to do. Bundle content is fetched lazily, on demand, by
    // whichever node needs to run something — the right default for a 200
    // MB environment nobody on `dc=west` will ever execute, and the reason
    // a first run is slow and unpredictable. This asks a node to fetch
    // *now*, before somebody is waiting on it.
    //
    // It is a scheduling decision and not a new trust path: the bundle is
    // addressed by digest, every chunk is verified on write, and a node
    // that never gets asked converges the moment it needs the content. So
    // it is idempotent by construction — sending it again is how the
    // asker polls for progress, and how a fetch already in flight is
    // *joined* rather than started twice.
    BundleFetch {
        bundle_id: BundleId,
    },
    /// "Act on this upgrade plan: fetch your own signed release and become
    /// the version it names."
    ///
    /// **No binary crosses this wire, and none ever will.** The node
    /// downloads from the release source it was configured with and
    /// verifies the minisign signature itself, exactly as
    /// `meshnode upgrade --self` does when a person runs it — this only
    /// says *when*. A path where one node hands another node an executable
    /// would be a lateral-movement primitive the design does not have, and
    /// no amount of signing makes that trade worth it when orchestration
    /// gets the same outcome (M24 §5).
    ///
    /// Carries an id and nothing else. The **receiving node** resolves it
    /// against its own replicated state: the plan's admin signature, that
    /// the signer is still an active admin *here*, that the plan has not
    /// expired or been cancelled, and that the selector the operator signed
    /// names this node. A compromised driver is a courier — it cannot
    /// upgrade a node the operator never named, or one whose plan was
    /// withdrawn a minute ago.
    ///
    /// Idempotent, like [`Self::BundleFetch`]: a node already running the
    /// target answers `done`, one already swapping answers `started`, and
    /// re-sending is how the driver polls a node through its restart.
    UpgradeSelf {
        plan_id: crate::UpgradeId,
    },
    // -- gang scheduling (protocol 1.12, C0-C9 §8.2) ------------------------
    //
    // Appended at the tail for the same positional reason as every block
    // above it.
    //
    // The first requests in this enum that ask a peer to *hold something
    // back*. Everything before them either moves state that already exists
    // or asks a node to do work it is free to refuse; these ask a node to
    // promise resources on behalf of a decision the asker has not made yet,
    // and the three of them are one two-phase transaction.
    //
    // There is no distributed cleanup path and that is deliberate. A lease
    // carries a TTL, so an origin that dies between offer and commit costs
    // the mesh one lease lifetime and nothing else: every holder reclaims
    // its own resources on its own clock, and the successor that adopts the
    // job re-drives the offer phase from the replicated plan.
    /// Origin → candidate: "hold `reqs` for gang `gang` for `ttl_secs`."
    /// Answered with [`Response::GangOffer`].
    ///
    /// The `ticket` travels because the *receiver* decides wound-wait, not
    /// the sender: a node holding a lease for a junior gang yields it when
    /// a senior gang's offer would otherwise be refused, and it can only
    /// know which is senior if it is told the asker's place in the order.
    /// The order itself is computable from replicated state, so a node that
    /// wanted to check a claimed ticket against the gang record can.
    GangOffer {
        gang: GangId,
        ticket: GangTicket,
        /// What **one** member needs. Per-member rather than aggregate so
        /// the receiver can charge the same figure the member job will
        /// declare when it arrives, and the two can never disagree.
        reqs: Requirements,
        /// How many of the gang's members this node is asked to hold. More
        /// than one under `pack`, which is the default precisely because
        /// packing a gang onto the fewest nodes minimises the inter-task
        /// network path — the thing a gang exists for.
        members: u32,
        /// Seconds. Bounded by the receiver at [`crate::gang::MAX_LEASE_TTL_SECS`]
        /// — a lease is a promise the *holder* keeps, so the holder caps how
        /// long it can be asked to keep it.
        ttl_secs: u64,
    },
    /// Origin → holder: "every candidate accepted; turn your lease into an
    /// admission." Answered with [`Response::Ok`], or an error if the lease
    /// expired first — in which case the origin releases the rest and
    /// retries, because a gang that started N-1 of N members is the failure
    /// this milestone exists to prevent.
    GangCommit {
        gang: GangId,
    },
    /// Origin → holder: "give it back." Sent to everyone who accepted when
    /// the offer phase fails, and by an adopting successor before it
    /// re-drives a gang whose leases may still be outstanding.
    ///
    /// Idempotent and unauthenticated beyond the connection's TLS-proven
    /// peer: releasing a lease the node does not hold is a no-op, and only
    /// the gang's own origin ever knows the gang id to release.
    GangRelease {
        gang: GangId,
    },
    // -- devices (protocol 1.13, C0-C9 §9) ---------------------------------
    //
    // The live half of the device model, and an *appended pair* rather than
    // a field on [`crate::PressureSnapshot`] — which is embedded in
    // `Response::Pressure`, so growing it would make every older peer
    // misparse every pressure reply. Fourth application of that rule in
    // this tree (§9.1).
    //
    // It carries the V1 snapshot inside it, so a 1.13 node asks once and
    // learns both halves; a peer below [`crate::ACCEL_MINOR`] is asked the
    // V1 question instead and its device pressure stays unknown, which
    // placement already treats as neutral.
    /// Ask a peer for live load *and* device telemetry. Answered with
    /// [`Response::PressureV2`].
    PressureV2,
    // -- action cache (protocol 1.14, C0-C9 §11) ---------------------------
    //
    // Appended at the tail, like every block above it, because postcard
    // encodes a variant by its index and a signed spec must decode the same
    // way on every node that has ever existed.
    /// Anyone → directory node: "has anybody already run this action?"
    /// Answered with [`Response::CacheEntry`].
    ///
    /// **The lookup a miss pays for.** One request to one of
    /// [`crate::cache::CACHE_REPLICAS`] nodes, ranked by the same
    /// rendezvous primitive the object directory uses — no broadcast, no
    /// index to consult, no coordinator to ask.
    CacheLookup {
        key: crate::ActionKey,
    },
    /// Producer → directory node: "I ran this action, here is what it
    /// produced." The receiver records it against the *TLS-proven* sender,
    /// so a node cannot publish an entry claiming somebody else computed
    /// it.
    ///
    /// Fire and forget: a lost publication costs a re-execution later, not
    /// correctness, exactly as a lost `ObjectAnnounce` does (M9 §10, Q3).
    CachePublish {
        entry: Box<crate::cache::CacheEntry>,
    },
    /// Live telemetry for **every node this peer knows about**, not just
    /// itself (`TELEMETRY_MINOR`).
    ///
    /// Appended at the tail, and answered with
    /// [`Response::Telemetry`]. The shape difference from
    /// [`Pressure`](Self::Pressure) is the whole point: pressure was a
    /// question about the peer, so a node only ever learned about the one
    /// partner a gossip tick happened to pick, and readings aged out
    /// faster than the rotation could refresh them on any mesh worth the
    /// name. This is a question about the *mesh*, so an answer carries
    /// what the peer has heard as well as what it measured, and a reading
    /// crosses the fleet the way liveness already does — epidemically, in
    /// log(n) rounds, for the same single round trip per tick.
    Telemetry,
    // -- M31 B3: reading history back (§14.2, §19 Q11) ----------------------
    /// Ask a peer for a page of its **local Parquet archive**
    /// (`ARCHIVE_MINOR`).
    ///
    /// The archive is one directory on one machine — usually Director's
    /// host or a designated box — so a node without one has the index
    /// window and nothing older. §19 Q11 was decided as *ask a peer*, and
    /// this is that ask.
    ///
    /// **Not a query interface.** The filter crosses as a `HistoryQuery`,
    /// the same shape the local index already takes, and the receiver
    /// builds its own statement from it. Accepting SQL would turn a history
    /// browse into a query engine somebody else drives.
    ArchivePage {
        query: Box<crate::HistoryQuery>,
    },
    /// Fetch one window of one job's exported output from the sink that
    /// holds it (`ARCHIVE_MINOR`).
    ///
    /// **The node that exported it is the node that fetches it** (§6): the
    /// same `run_on` election and the same `egress_allow` check as the
    /// write path. Director asks the mesh and receives the lines, so
    /// Director on a laptop with no route to the warehouse still works, and
    /// a read is governed exactly like a write.
    ReplayPayload {
        job_id: JobId,
        sink: String,
        key: String,
        /// Lines back from the end. Opening at the tail is the default
        /// because the reason anyone opens a six-month-old log is the thing
        /// that went wrong, and it is at the end.
        from_end: u64,
        limit: u64,
        /// Delegated to the destination's own index where it has one.
        search: String,
    },
    // -- wire authorization (protocol 1.19, S1) -----------------------------
    //
    // Appended at the tail, like every block above it.
    //
    /// Cancel a job, carrying the **canceller's own signed envelope**
    /// (`AUTHZ_MINOR`).
    ///
    /// [`Self::CancelJob`] carries a job id and nothing else, so an
    /// executor could only ever check that the node asking was a member of
    /// this mesh — the `require_cancel` rule that stops one person
    /// destroying another's running work lived entirely on the node the
    /// person happened to be talking to. This carries the `SignedIpc` that
    /// person signed, so the executor verifies the signature itself,
    /// resolves the user, and applies the same gate against its own view of
    /// who submitted the job.
    ///
    /// The envelope is the *whole* one, replayed onward rather than
    /// re-minted: a node cannot forge a cancel it was not asked for, and
    /// the executor's own nonce table makes the forward leg
    /// replay-protected independently of the leg that reached the first
    /// node.
    CancelJobV2 {
        env: Box<crate::SignedIpc>,
    },
    // -- Reachability (protocol 1.21, R4 P2) --------------------------------
    //
    // Appended at the tail for the same positional reason as every block
    // above it.
    //
    /// "Can you see these paths?" Asked of candidate nodes *before* work is
    /// offered to them, so a run over data on a share only some machines
    /// mount lands on the machines that mount it.
    ///
    /// **It is a scheduling hint and not an authorisation.** The answer says
    /// only whether a path exists and is readable to the node's own account;
    /// it grants nothing, and a node that answers "yes" is still subject to
    /// every sandbox rule when the job actually runs. Nothing here can widen
    /// what a job may reach — that is [`crate::sandbox::grantable`]'s job on
    /// the executing node, from the signed service definition.
    ///
    /// Cheap on purpose: a `stat`, no read, no directory walk. The origin
    /// asks about the *common prefix* of a run's paths rather than every
    /// item, because a mount is present or absent — it is not present for
    /// one file and absent for the next.
    CanReach {
        paths: Vec<String>,
        /// Commands the work needs on the machine that runs it, from the
        /// service's `#meshware: tools` declaration. Same round trip as the
        /// paths, because it is the same question — *can this machine do
        /// this work* — and a second probe would double the cost of asking.
        tools: Vec<String>,
    },
    /// What these instances are using right now (Phase B, E3). The node
    /// measures each job it runs and answers; a job it does not run gets no
    /// row. Member-only: resource use is information about the machine.
    InstanceStats {
        jobs: Vec<crate::JobId>,
    },
    /// **Retired: the mesh proxy is gone.** Was: open a connection to one
    /// registered instance this node runs, as a stream hand-off.
    ///
    /// Nothing sends it. It stays, at the tail, for two reasons: postcard
    /// encodes a variant by index, so a variant appended later must not
    /// inherit this one's number and be decoded from an old node's proxy
    /// frame; and an old node that does send it gets a readable `Error`
    /// rather than a dropped stream. Its instances are reached the way any
    /// client reaches them now — at the node's address and the port it was
    /// given.
    ServiceConnect {
        service: String,
        instance: crate::JobId,
        label: String,
    },
    // -- SEC-15, appended at the tail -----------------------------------------
    /// [`Request::FollowJob`], carrying the person's own signed `JobLogs`
    /// request. The executor answers the legacy request only for the node
    /// that dispatched the job; anyone else's read of a job's output has to
    /// show whose it is, and that person has to be allowed to read.
    FollowJobV2 {
        job_id: JobId,
        stdout_offset: u64,
        stderr_offset: u64,
        follow: bool,
        env: Box<crate::SignedIpc>,
    },
    // -- SEC-18, appended at the tail -----------------------------------------
    /// [`Request::CachePublish`], with what the entry was computed from. A
    /// directory replica re-derives the key from it and checks that the
    /// publisher is the node that ran the work; the legacy request, which
    /// carried a bare claim, is refused.
    CachePublishV2 {
        entry: Box<crate::CacheEntry>,
        proof: Box<crate::cache::CacheProof>,
    },
    // -- 1.35, appended at the tail -------------------------------------------
    /// [`Request::Telemetry`], answered with readings that name their device
    /// ([`Response::TelemetryV2`]). A new pair rather than a field on
    /// [`crate::DevicePressureV1`], which older peers decode by position.
    TelemetryV2,
}

impl Request {
    /// The lowest protocol minor that understands this request. A node
    /// never sends a request to a peer whose negotiated minor is below it:
    /// the peer could not decode the frame and would simply drop the
    /// stream (a 1.4 dialer also never `accept_bi`s, so a reverse request
    /// would hang forever).
    pub fn min_minor(&self) -> u16 {
        match self {
            Request::Observe
            | Request::RelayRegister
            | Request::RelayOpen { .. }
            | Request::PunchRequest { .. } => crate::WAN_MINOR,
            Request::Pressure
            | Request::ParallelOffer { .. }
            | Request::ChunkPull { .. }
            | Request::ChunkDone(_)
            | Request::ObjectAnnounce { .. }
            | Request::ObjectLocate { .. }
            | Request::GetObject { .. } => crate::COMPUTE_MINOR,
            Request::SyncV2(_)
            | Request::PushOpsV2 { .. }
            | Request::JoinV2(_)
            | Request::PairCompleteV2(_) => crate::OPLOG_MINOR,
            Request::HostProbes { .. } => crate::INVENTORY_MINOR,
            Request::ArchivePage { .. } | Request::ReplayPayload { .. } => crate::ARCHIVE_MINOR,
            Request::SessionOpen(_) => crate::SESSION_MINOR,
            Request::JoinV3(_) | Request::JoinFetch { .. } | Request::PairCompleteV3(_) => {
                crate::BOOTSTRAP_MINOR
            }
            Request::BundleFetch { .. } | Request::UpgradeSelf { .. } => crate::FLEET_MINOR,
            Request::GangOffer { .. }
            | Request::GangCommit { .. }
            | Request::GangRelease { .. } => crate::CLUSTER_MINOR,
            Request::PressureV2 => crate::ACCEL_MINOR,
            Request::Telemetry => crate::TELEMETRY_MINOR,
            Request::CacheLookup { .. } | Request::CachePublish { .. } => crate::CACHE_MINOR,
            Request::CancelJobV2 { .. } => crate::AUTHZ_MINOR,
            Request::CanReach { .. } => crate::REACH_MINOR,
            Request::InstanceStats { .. } => crate::INSTANCE_MINOR,
            Request::ServiceConnect { .. } => crate::PROXY_MINOR,
            Request::FollowJobV2 { .. } => crate::LOGS_MINOR,
            Request::CachePublishV2 { .. } => crate::CACHE_PROOF_MINOR,
            Request::TelemetryV2 => crate::DEVICE_KEY_MINOR,
            _ => 0,
        }
    }

    /// A short, stable name for logs and refusals. Not the wire encoding —
    /// postcard encodes a variant by index — just something a person reading
    /// a log line can match to this enum.
    pub fn name(&self) -> &'static str {
        match self {
            Request::Ping(_) => "Ping",
            Request::Join(_) => "Join",
            Request::Sync(_) => "Sync",
            Request::PushOps { .. } => "PushOps",
            Request::SubmitJob(_) => "SubmitJob",
            Request::CancelJob { .. } => "CancelJob",
            Request::CanReach { .. } => "CanReach",
            Request::InstanceStats { .. } => "InstanceStats",
            Request::ServiceConnect { .. } => "ServiceConnect",
            Request::FollowJob { .. } => "FollowJob",
            Request::FollowJobV2 { .. } => "FollowJobV2",
            Request::CachePublishV2 { .. } => "CachePublishV2",
            Request::TelemetryV2 => "TelemetryV2",
            Request::GetManifest { .. } => "GetManifest",
            Request::GetChunk { .. } => "GetChunk",
            Request::PairInit(_) => "PairInit",
            Request::PairReveal { .. } => "PairReveal",
            Request::PairConfirm => "PairConfirm",
            Request::PairComplete(_) => "PairComplete",
            Request::Observe => "Observe",
            Request::RelayRegister => "RelayRegister",
            Request::RelayOpen { .. } => "RelayOpen",
            Request::PunchRequest { .. } => "PunchRequest",
            Request::Pressure => "Pressure",
            Request::ParallelOffer { .. } => "ParallelOffer",
            Request::ChunkPull { .. } => "ChunkPull",
            Request::ChunkDone(_) => "ChunkDone",
            Request::ObjectAnnounce { .. } => "ObjectAnnounce",
            Request::ObjectLocate { .. } => "ObjectLocate",
            Request::GetObject { .. } => "GetObject",
            Request::SyncV2(_) => "SyncV2",
            Request::PushOpsV2 { .. } => "PushOpsV2",
            Request::JoinV2(_) => "JoinV2",
            Request::PairCompleteV2(_) => "PairCompleteV2",
            Request::HostProbes { .. } => "HostProbes",
            Request::SessionOpen(_) => "SessionOpen",
            Request::JoinV3(_) => "JoinV3",
            Request::JoinFetch { .. } => "JoinFetch",
            Request::PairCompleteV3(_) => "PairCompleteV3",
            Request::BundleFetch { .. } => "BundleFetch",
            Request::UpgradeSelf { .. } => "UpgradeSelf",
            Request::GangOffer { .. } => "GangOffer",
            Request::GangCommit { .. } => "GangCommit",
            Request::GangRelease { .. } => "GangRelease",
            Request::PressureV2 => "PressureV2",
            Request::CacheLookup { .. } => "CacheLookup",
            Request::CachePublish { .. } => "CachePublish",
            Request::Telemetry => "Telemetry",
            Request::ArchivePage { .. } => "ArchivePage",
            Request::ReplayPayload { .. } => "ReplayPayload",
            Request::CancelJobV2 { .. } => "CancelJobV2",
        }
    }

    /// Whether serving this request requires the peer to already be an
    /// **active member of this mesh**.
    ///
    /// Admission (`ConnPurpose`) deliberately lets an unknown peer connect:
    /// a node whose `MemberAdd` is still in flight would otherwise be locked
    /// out of the very gossip that would admit it. That is the right call at
    /// the connection level and the wrong one at the request level — the
    /// mesh id a peer must present to get that far is *not a secret* (a
    /// `Probe` connection is answered to anyone and the ack carries it
    /// verbatim, which is what `meshnode ping --same-mesh` reads).
    ///
    /// So membership is checked here instead, per request. `false` is the
    /// short list: the handshakes that a *non*-member is by definition
    /// running, plus the requests the transport answers before dispatch.
    /// Everything else — replicated state, job logs, archive pages, content,
    /// cancellation, telemetry — is member-only.
    ///
    /// Matched exhaustively on purpose: a new variant must be classified
    /// rather than inherit a default.
    pub fn requires_membership(&self) -> bool {
        match self {
            // Liveness. Answering a ping to a stranger is the point of a
            // ping, and it discloses nothing the handshake did not.
            Request::Ping(_)
            // Joining. The token *is* the authorization, and by definition
            // the asker is not a member yet.
            | Request::Join(_)
            | Request::JoinV2(_)
            | Request::JoinV3(_)
            // Continuation of a join: checked in the handler against
            // membership the acceptor has just authored, which is stronger
            // than re-presenting an invite that is already spent.
            | Request::JoinFetch { .. }
            // LAN pairing. Refused at admission for members, and the
            // candidate side has no membership to check.
            | Request::PairInit(_)
            | Request::PairReveal { .. }
            | Request::PairConfirm
            | Request::PairComplete(_)
            | Request::PairCompleteV2(_)
            | Request::PairCompleteV3(_)
            // Answered in the transport (WAN) or in the relay hub; they
            // never reach the daemon's dispatch. Listed as not requiring
            // membership so the transport keeps deciding, not this.
            | Request::Observe
            | Request::RelayRegister
            | Request::RelayOpen { .. }
            | Request::PunchRequest { .. }
            // Anti-entropy, which is the one place the mid-flight problem
            // is real: a node admitted a second ago holds the op that
            // admits it and needs to hand it to a peer that has not heard
            // yet. Both legs are handled rather than refused —
            //
            //   * a **push** is safe from anyone, because the merge path is
            //     the gate: every op carries its author's signature, the
            //     author must already be a member, and per-author sequences
            //     must be gapless. A stranger can push nothing that a
            //     member did not already author and sign.
            //   * a **pull** from a non-member is served an *introduction*
            //     rather than a batch — our version vector and no ops (see
            //     `handle_wire`), which is exactly enough for the peer to
            //     push us what we lack and nothing at all of the log it
            //     asked for.
            //
            // So these two are classified here as "not a hard refusal" and
            // the daemon does the rest.
            | Request::Sync(_)
            | Request::SyncV2(_)
            | Request::PushOps { .. }
            | Request::PushOpsV2 { .. } => false,
            // Everything else.
            Request::SubmitJob(_)
            | Request::CancelJob { .. }
            | Request::CancelJobV2 { .. }
            | Request::FollowJob { .. }
            | Request::FollowJobV2 { .. }
            | Request::GetManifest { .. }
            | Request::GetChunk { .. }
            | Request::Pressure
            | Request::PressureV2
            | Request::Telemetry
            | Request::TelemetryV2
            | Request::HostProbes { .. }
            | Request::ParallelOffer { .. }
            | Request::ChunkPull { .. }
            | Request::ChunkDone(_)
            | Request::ObjectAnnounce { .. }
            | Request::ObjectLocate { .. }
            | Request::GetObject { .. }
            | Request::SessionOpen(_)
            | Request::BundleFetch { .. }
            | Request::UpgradeSelf { .. }
            | Request::GangOffer { .. }
            | Request::GangCommit { .. }
            | Request::GangRelease { .. }
            | Request::CacheLookup { .. }
            | Request::CachePublish { .. }
            | Request::CachePublishV2 { .. }
            | Request::ArchivePage { .. }
            | Request::ReplayPayload { .. }
            // Whether a path exists on this machine is information about
            // the machine, and a stranger has no business asking.
            | Request::CanReach { .. }
            // What a machine's instances are using is information about the
            // machine, like whether a path exists on it.
            | Request::InstanceStats { .. }
            // A registered instance is reachable by the mesh, not by
            // whoever can open a connection to a node.
            | Request::ServiceConnect { .. } => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Pong(Ping),
    JoinAccept(Box<JoinAccept>),
    Sync(SyncResponse),
    Ok,
    JobAccepted {
        job_id: JobId,
        executor: NodeId,
    },
    /// Definitive backpressure refusal (MASTERPLAN §10.2): the executor's
    /// slots and pending queue are both full. The submitter spills to the
    /// next-best candidate. Only this response triggers spill-over — an
    /// ambiguous failure (timeout) never does, so a job cannot double-run.
    Busy {
        running: u32,
        queued: u32,
    },
    /// One chunk of job output. `stream`: 1 = stdout, 2 = stderr.
    LogChunk {
        stream: u8,
        data: Vec<u8>,
    },
    /// Terminal frame of a FollowJob stream.
    JobEnd {
        summary: JobSummary,
    },
    /// A bundle manifest (answer to GetManifest).
    Manifest(Box<Manifest>),
    /// One content chunk's bytes (answer to GetChunk). Empty = not held.
    Chunk {
        data: Vec<u8>,
    },
    Error {
        message: String,
    },
    // -- LAN pairing --------------------------------------------------------
    /// Answer to PairInit: the joiner's nonce + join fields.
    PairAccept(PairAccept),
    /// Answer to PairReveal: commitment verified, code displayable.
    PairReady,
    /// Answer to PairConfirm: whether the joiner's human confirmed too.
    PairDecision {
        accepted: bool,
    },
    // -- WAN (protocol 1.5) -------------------------------------------------
    /// Answer to `Observe`: the address this connection appears to come
    /// from, as `ip:port`.
    Observed {
        addr: String,
    },
    /// Answer to `RelayRegister` / `RelayOpen`: accepted; frames follow on
    /// this stream.
    RelayReady,
    /// Answer to `PunchRequest`: the punch was forwarded to the target.
    PunchSent,
    // -- Compute (protocol 1.6) ---------------------------------------------
    /// Answer to `Pressure`: how loaded the responder is right now.
    Pressure(crate::capacity::PressureSnapshot),
    /// Answer to `ChunkPull`: the next chunk, or `None` when the job is
    /// finished or unknown here (a worker that gets `None` stops pulling).
    ChunkWork(Option<Box<ChunkAssignment>>),
    /// Answer to `ChunkPull`: nothing right now, but the job is not over —
    /// work is outstanding elsewhere and may yet come back to this worker,
    /// as a requeued orphan or as a copy racing a straggler (M9 §5.6).
    /// The worker waits and asks again; it must not retire on this.
    ChunkWait,
    /// Answer to `ObjectLocate`: every node this directory knows holds the
    /// object, best-first. Empty means the directory has no entry — which
    /// is not the same as "the object does not exist", and the caller's
    /// answer to both is to re-run the task that made it (M9 §10, Q3).
    ObjectHolders {
        holders: Vec<ObjectRef>,
    },
    /// Answer to `GetObject`: the chunk list, or `None` if not held here.
    Object(Option<Box<ObjectManifest>>),
    // -- action cache (protocol 1.14, C0-C9 §11) ---------------------------
    /// Answer to [`Request::CacheLookup`]: the entry, or `None`.
    ///
    /// `None` is an ordinary answer and the common one on a cold mesh. It
    /// means *this directory node has no entry* — not that the action has
    /// never run — and the caller's response to both is the same: run it.
    CacheEntry(Option<Box<crate::cache::CacheEntry>>),
    // -- Forward-compatible op-log (protocol 1.7, F0) -----------------------
    /// Answer to [`Request::SyncV2`].
    SyncV2(SyncResponseV2),
    /// Answer to [`Request::JoinV2`].
    JoinAcceptV2(Box<JoinAcceptV2>),
    // -- Inventory reachability (protocol 1.8, M12 §6.3 P4) ------------------
    /// **Retired** with [`Request::HostProbes`]; nothing on this build
    /// sends it.
    HostProbes(Box<HostProbeReport>),
    // -- Chunked bootstrap (protocol 1.10) ----------------------------------
    /// One instalment of a bootstrap snapshot: the answer to both
    /// [`Request::JoinV3`] and [`Request::JoinFetch`].
    Snapshot(Box<SnapshotChunk>),
    // -- Bundle prefetch (protocol 1.11, M24 §3.2) --------------------------
    /// Answer to [`Request::BundleFetch`]: how far this node has got.
    BundleProgress(BundleProgress),
    /// Answer to [`Request::UpgradeSelf`].
    UpgradeState(UpgradeState),
    // -- gang scheduling (protocol 1.12, C0-C9 §8.2) ------------------------
    /// Answer to [`Request::GangOffer`]: the lease, or the reason there is
    /// none. A refusal that names the senior gang holding the resources is
    /// what makes `meshnode gangs show` able to answer "why is mine not
    /// running" with something other than "it isn't".
    GangOffer(GangOfferReply),
    // -- devices (protocol 1.13, C0-C9 §9) ---------------------------------
    /// Answer to [`Request::PressureV2`]: the V1 snapshot plus per-device
    /// telemetry. Every device field is optional because every probe can
    /// fail, and an absent field is *unknown* — which is neutral, never
    /// disqualifying, the same rule the rest of the capacity model follows.
    PressureV2(Box<crate::device::PressureV2>),
    /// Answer to [`Request::Telemetry`]: what this peer measured about
    /// itself, plus every reading it holds that is still fresh. The
    /// requester merges freshest-wins by HLC, so a slow path and a fast
    /// path carrying the same node's reading converge rather than fight.
    Telemetry(Vec<crate::TelemetryEntry>),
    // -- M31 B3: reading history back ---------------------------------------
    /// Answer to [`Request::ArchivePage`]: rows out of the peer's Parquet,
    /// already projected into the shapes a History list draws.
    ///
    /// An **empty page is an ordinary answer** and the common one — a node
    /// with no archive has no older history, which is a fact rather than a
    /// failure (A2's known-absence rule, one tier down).
    ArchivePage(Box<ArchivePageReply>),
    /// Answer to [`Request::ReplayPayload`]: one window of one job's
    /// output, oldest-first within the window.
    ///
    /// `status` carries the destination's own words when it would not
    /// answer, and is empty on success — because *"output is in
    /// `warehouse`; it did not answer: 503"* sends an operator to the right
    /// machine and "unavailable" sends them to the wrong one.
    PayloadWindow(Box<PayloadWindowReply>),
    /// Answer to [`Request::CanReach`]: one verdict per path asked about,
    /// in the order they were asked.
    ///
    /// A bare bool per path and nothing else. *Why* a path is unreachable —
    /// not mounted, not readable by this account, a typo — is a question for
    /// the person reading the run, and the honest answer is at the item that
    /// tried. Guessing here would produce a confident wrong reason.
    Reachable {
        ok: Vec<bool>,
        /// One verdict per tool asked about, in order.
        tools_ok: Vec<bool>,
    },
    /// Answer to [`Request::InstanceStats`]: one row per job the node runs.
    InstanceStats(Vec<crate::InstanceStat>),
    // -- SEC-18, appended at the tail -----------------------------------------
    /// Answer to a [`Request::CachePublishV2`] for a `map` item that came
    /// without its plan, from a replica that has not verified that plan
    /// yet: send it again with the plan.
    CachePlanNeeded,
    // -- 1.35, appended at the tail -------------------------------------------
    /// Answer to [`Request::TelemetryV2`]: [`Response::Telemetry`], with
    /// every device reading naming its vendor and kind — or saying it
    /// cannot, for a reading this peer heard from a pre-1.35 node.
    TelemetryV2(Vec<crate::TelemetryEntryV2>),
}

/// One page of a peer's archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivePageReply {
    pub jobs: Vec<crate::HistoryJobRow>,
    /// True when the responder actually has an archive. False means "I have
    /// nothing older", which the asker renders rather than retries.
    pub has_archive: bool,
}

/// One window of a replayed payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadWindowReply {
    pub lines: Vec<String>,
    pub more_before: bool,
    pub status: String,
}

/// What one node has done about an upgrade request (M24 §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeState {
    /// The version this node is running *right now*.
    pub version: String,
    /// It is already the target. The rollout moves on.
    pub done: bool,
    /// A swap is in flight here. The node will drain and restart on its
    /// own; the driver waits for it to come back.
    pub started: bool,
    /// Set when this node declined to act, with the reason a person reads.
    ///
    /// A refusal is an *answer*, not a transport failure, which is why it
    /// rides here rather than in [`Response::Error`]: the driver has to
    /// tell "this node said no" apart from "this node did not reply", and
    /// only the node can say which of its own conditions failed.
    pub refused: Option<String>,
    /// Could asking again ever change the answer?
    ///
    /// The distinction that keeps a rollout honest about time. A plan that
    /// has not gossiped to this node *yet* is worth another round trip
    /// seconds from now; a node whose build cannot decode the request, or
    /// whose plan does not name it, will still refuse in ten minutes — and
    /// retrying until a deadline turns something knowable in one round
    /// trip into a ten-minute stare at a spinner.
    pub retry: bool,
}

/// How much of a bundle one node holds (M24 §3.2).
///
/// Deliberately a *reading*, not a promise. A node reports what is on its
/// disk right now; the fetch that fills the gap runs on its own and
/// survives the asker hanging up, exactly as a drain does. Two consecutive
/// readings are what progress is — there is no per-node fetch state
/// machine anywhere, because there is nothing to keep consistent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BundleProgress {
    /// Distinct chunks already present locally.
    pub have_chunks: u32,
    /// Distinct chunks the manifest names. Zero while the manifest itself
    /// has not landed — a node that cannot yet see the shape of the work
    /// says so rather than reporting a fraction of an unknown.
    pub total_chunks: u32,
    pub have_bytes: u64,
    pub total_bytes: u64,
    /// Manifest and every chunk present.
    pub complete: bool,
    /// Set when the fetch gave up. The node keeps the chunks it did get,
    /// and asking again resumes from there.
    pub failed: bool,
}

// ---------------------------------------------------------------------------
// Relay data plane (protocol 1.5)
// ---------------------------------------------------------------------------
//
// Frames travel over long-lived bi-streams on the *outer* relay connection
// (never QUIC datagrams: an inner QUIC Initial must be >= 1200 bytes and
// the outer datagram budget at the minimum path MTU is ~1160, so
// QUIC-in-QUIC over datagrams cannot even complete a handshake). Streams
// give reliable, flow-controlled delivery; the inner session still treats
// the path as lossy UDP (the relay drops on a full queue).

/// Client → relay: an opaque payload for `peer`. On a registration stream
/// `peer` is whoever the payload is for; on a `RelayOpen` stream it must be
/// the opened target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayFrame {
    pub peer: NodeId,
    pub payload: Vec<u8>,
}

/// Relay → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayServerFrame {
    /// An opaque payload from `from` (the relay stamps the TLS-proven
    /// sender; clients never trust a source claimed inside the payload).
    Data { from: NodeId, payload: Vec<u8> },
    /// `from` asked the relay to have this node dial it back on
    /// `candidates` (coordinated punch). Only delivered on registration
    /// streams.
    Punch {
        from: NodeId,
        candidates: Vec<String>,
    },
}
