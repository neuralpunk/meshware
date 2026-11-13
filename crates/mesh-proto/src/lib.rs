//! meshware wire protocol.
//!
//! Everything that crosses a socket — QUIC streams between nodes or the local
//! IPC socket between CLI/Director and the daemon — is defined here, so that
//! every consumer (node, CLI, Director) shares one schema.
//!
//! Versioning contract (MASTERPLAN §6): every handshake carries
//! `(protocol_major, protocol_minor)`. Minor bumps are additive only; peers
//! negotiate the highest common minor. Nodes one *major* apart must refuse
//! cleanly with a reason, never hang.

pub mod acting;
pub mod attest;
pub mod availability;
pub mod cache;
pub mod capacity;
pub mod container;
pub mod content;
pub mod deployment;
pub mod device;
pub mod enrol;
pub mod facts;
pub mod fleet;
pub mod gang;
pub mod history;
pub mod hosts;
mod ids;
mod ipc;
pub mod objects;
pub mod oidc;
mod ops;
pub mod parallel;
pub mod placement;
pub mod ports;
pub mod quota;
pub mod rbac;
pub mod registry;
pub mod results;
pub mod retention;
pub mod runtimes;
pub mod schedule;
pub mod script;
pub mod secrets;
pub mod sessions;
/// Mesh-wide load shedding (N6 §12).
pub mod shed;
pub mod sink;
pub mod text;
pub mod version;
pub mod volumes;
mod wire;
pub mod workflow;

pub use attest::{
    AttestKind, AttestRevocation, AttestRoot, HardwareAttestation, attest_root_remove_signing_bytes,
};
pub use availability::{ExpiryReason, KnownAbsence, OutputAvailability};
pub use cache::{
    ActionSpec, CACHE_CONTEXT, CACHE_ENV_PREFIX, CACHE_KEY, CACHE_REPLICAS, CACHE_VERIFY,
    CACHEABLE_TAG, CacheEntry, CacheHit, CacheProof, CacheStatRow, CacheStats, CachedOutput,
    KeyExplain, KeyField, PurgeScope, REQ_CACHE, REQ_CACHE_INPUTS, REQ_CACHE_OUTPUTS, is_cache_env,
    is_cacheable, key_why,
};
pub use capacity::{
    CAP_GPUS, CAP_TAG_PREFIX, CAP_TERM, CAP_VER, CAP_VRAM_MB, Capacity, OUT_ENV_PREFIX, OUT_EXPECT,
    OUT_WANT, PressureSnapshot, REQ_BATCH, REQ_ENV_PREFIX, REQ_PREEMPTIBLE, REQ_RACE,
    REQ_SINGLETON, Requirements, STAGED_ITEMS, SystemSnapshot, TelemetryEntry, TelemetryEntryV2,
    accepts_terminals, assignable_tag, is_batched, is_capacity_tag, is_preemptible,
    is_reserved_env, is_reserved_input, race_of, term_tag, user_tags, version_of, version_tag,
};
pub use container::{REQ_ARGS, REQ_COMMAND, command_env, command_override};
pub use content::{
    ANSIBLE_CHECK, ANSIBLE_ENV_PREFIX, ANSIBLE_FORKS, ANSIBLE_LIMIT, ANSIBLE_TAGS, BundleMeta,
    FileEntry, InlineScript, MAX_ANSIBLE_FORKS, Manifest, MeshInventory, Runtime, ServiceExec,
    ServiceVersion, ansible_forks,
};
pub use deployment::{
    Count, DEFAULT_LOST_AFTER_SECS, DEFAULT_PROGRESS_DEADLINE_SECS, DEPLOY_ENV, DEPLOY_GEN_ENV,
    DEPLOY_SLOT_ENV, DEPLOYMENT_HISTORY_KEEP, Deployment, DeploymentCheckView,
    DeploymentHistoryView, DeploymentInstanceDetail, DeploymentInstanceRow, DeploymentPolicy,
    DeploymentRow, DeploymentStatusView, DeploymentTag, DeploymentTagRow, DeploymentVersion,
    DeploymentVersionRow, HealthCheck, HealthKind, IncarnationRow, InstanceHealth, InstanceStat,
    InstanceStatRow, NEVER, NodeInstanceHealth, PLAN_KNOB, PLAN_PLACEMENT, PLAN_RECORD, PLAN_ROLLS,
    PLAN_SCALE, PlacementInstanceRow, PlacementNodeRow, PlanChange, Reconcile, RestartPolicy,
    UpdatePolicy, UpdateStrategy, availability, deployment_remove_signing_bytes, duration_words,
    is_valid_deployment_name, is_valid_tag_name,
};
pub use device::{
    CUDA_VISIBLE, DEVICE_COUNT, DEVICE_ENV_PREFIX, DEVICE_INDICES, DEVICE_UUIDS, Device,
    DeviceHealth, DeviceInventory, DeviceKey, DeviceKind, DevicePressure, DevicePressureV1,
    DeviceRequest, DeviceRow, HIP_VISIBLE, OrdinalSpace, PressureV2, REQ_DEVICES, ROCR_VISIBLE,
    allocation_env, empty_allocation_env, is_device_env, reading_for,
};
pub use enrol::{
    EnrolmentDecline, EnrolmentRequest, EnrolmentRow, EnrolmentState, VerifiedIdentity,
    enrolment_code,
};
pub use facts::{
    FACT_COLLECTOR_ENV, FACT_ENV_PREFIX, FACT_SELECTOR_PREFIX, FACT_SUBJECT_ENV, FLAP_WINDOW_SECS,
    FactChange, FactCollector, FactCollectorRow, FactDiffRow, FactRow, FactSubject,
    FactSubjectKind, MAX_FACT_DOC, SYSTEM_COLLECTOR, SYSTEM_INTERVAL_SECS,
};
pub use fleet::{
    DEFAULT_PLAN_TTL_SECS, MAX_BATCH, MAX_PLAN_TTL_SECS, ReleaseCosignature, ReleaseKey,
    ReleaseRecord, UpgradePlan, release_key_revoke_signing_bytes, upgrade_cancel_signing_bytes,
};
pub use gang::{
    DEFAULT_LEASE_TTL_SECS, GANG_ENV_PREFIX, GANG_ID, GANG_LEADER, GANG_NODES, GANG_RANK,
    GANG_SIZE, GangMember, GangMemberView, GangOfferReply, GangOutcome, GangPlacement,
    GangPlanRecord, GangRefusal, GangRow, GangTicket, GangView, LeaseView, MAX_GANG_ATTEMPTS,
    MAX_LEASE_TTL_SECS, MAX_OFFER_ROUNDS, OFFER_DEADLINE_SECS, REQ_GANG, REQ_GANG_PLACEMENT,
    REQ_GANG_RESTART, gang_size_of, is_gang_env,
};
pub use history::{HistoryJobPage, HistoryJobRow, HistoryQuery, HistoryStats, TierReport};
pub use hosts::{INV_ENV_PREFIX, is_inventory_env};
pub use ids::{
    ActionKey, ApprovalId, AuditId, BundleId, ChunkHash, FanId, GangId, GrantId, InviteId, JobId,
    LeaseId, MeshId, NodeId, ObjectId, RunId, UpgradeId, UserId, hex32, parse_hex32,
};
pub use ipc::*;
pub use objects::{Capture, ObjectManifest, ObjectRef, ObjectRow};
pub use oidc::{GroupRole, JwtAlg, OidcAssertion, OidcProvider, nonce_for_key};
pub use ops::*;
pub use parallel::{ChunkAssignment, ChunkReport, ItemOutcome, NodeProgress, ParallelPlan};
pub use placement::{
    Affinity, CandidateScore, DelayedPrincipal, DistinctProperty, DurationRange,
    EXPLAIN_CANDIDATES, Exclusion, FitPolicy, MAX_AFFINITIES, PlacementExplain, PlacementRefusal,
    PlacementSpec, PlanEstimate, REQ_PLACEMENT, ScoreTerm, Spread, is_valid_attribute, term_gloss,
};
pub use ports::{
    ADDR_ENV_PREFIX, CAP_PORTS, IP_ENV_PREFIX, MAX_PORTS_PER_JOB, PORT_ENV_PREFIX, PortAllocation,
    PortHeldView, PortLedgerView, PortRange, PortRequest, PortRequests, REQ_HEALTH_PORT, REQ_PORTS,
};
pub use quota::*;
pub use rbac::{
    ActorType, AuditEntry, Grant, GrantRevocation, NodeRevocation, Role, ServiceAccount,
    SignedAuditRow, UserRecord, UserRevocation, glob_match,
};
pub use registry::{
    DEFAULT_DNS_PORT, DNS_SUFFIX, DnsStatusView, Health, MAX_SHUTDOWN_DELAY_SECS,
    NodeRegistrations, REQ_KILL_TIMEOUT, REQ_SERVICE, REQ_SERVICE_PORT, REQ_SERVICE_TAGS,
    REQ_SHUTDOWN_DELAY, Registration, RegistryInstanceRow, RegistryRow,
};
pub use results::{ResultRow, ResultSet};
pub use retention::{
    AUDIT_FLOOR_DAYS, ContentPolicy, DEFAULT_ARCHIVE_DAYS, DEFAULT_CONTENT_DAYS,
    DEFAULT_FACTS_DAYS, DEFAULT_JOB_OUTPUT_DAYS, DEFAULT_RUNS_DAYS, FOREVER, RetentionPolicy,
    RetentionTable, RetentionWindow, retention_reset_signing_bytes,
};
pub use schedule::{
    CatchUp, DEFAULT_GRACE_SECS, MIN_INTERVAL_SECS, Overlap, SCHEDULE_ENV, SCHEDULE_OCCURRENCE_ENV,
    Schedule, ServiceSchedule, ServiceScheduleCheck, ServiceScheduleRow, Trigger, WF_SCHEDULE,
    is_valid_schedule_name, schedule_remove_signing_bytes, service_schedule_remove_signing_bytes,
};
pub use secrets::{SecretRecord, SecretRemoval, SecretScope, SecretWrap};
pub use sessions::{
    DEFAULT_MAX_SESSIONS_PER_PEER, MAX_SESSION_FRAME, MAX_SESSION_PATH, MAX_SESSION_REASON,
    SESSION_CHUNK, STREAM_STDERR, STREAM_STDOUT, SessionEnd, SessionFrame, SessionKind,
    SessionPermission, SessionPolicy, SessionServerFrame, SessionSignal, SessionSpec,
    session_policy_remove_signing_bytes,
};
pub use sink::{
    MAX_HOLD_FOR_EXPORT_DAYS, ReplayState, Sink, SinkCapabilities, SinkKind, SinkOnFailure,
    SinkPayloads, SinkRecords, SinkTables, is_secret_reference, is_valid_sink_name,
    sink_remove_signing_bytes,
};
pub use version::{version_cmp, version_key};
pub use volumes::{
    CAP_VOLUMES, MAX_VOLUMES_PER_JOB, PLUGIN_CAPACITY_ENV, PLUGIN_CONFIG_ENV,
    PLUGIN_CONTRACT_VERSION, PLUGIN_NAME_ENV, PLUGIN_OPS, PLUGIN_PARAMS_ENV, PLUGIN_PATH_ENV,
    PluginCreated, REQ_VOLUMES, SOURCE_STATIC, VOLUME_ENV_PREFIX, Volume, VolumeRequest,
    VolumeRequests, VolumeRow,
};
pub use wire::*;
pub use workflow::{
    AnsiblePlay, Lane, MAX_DRAFT_SOURCE, PlannedStep, RetryPolicy, RunState, RunStep,
    SAME_AS_PREFIX, StepAction, StepEdge, StepFanout, StepLayout, StepState, WF_ATTEMPT,
    WF_ENV_PREFIX, WF_RUN, WF_STEP, WorkflowDraft, WorkflowLayout, WorkflowRunPlan,
    WorkflowRunRecord, WorkflowStep, WorkflowVersion, same_as_target, workflow_head_signing_bytes,
    workflow_remove_signing_bytes,
};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// **2 since 2026-09-20.** Thirty retired `OpBody` variants — the workflow
/// engine, its runs and drafts, the old schedule trigger, the drafting
/// agent, change control, touch-to-approve, the host inventory and quotas —
/// were deleted rather than kept as inert names, and postcard encodes an
/// enum by variant index. Everything after the first deletion therefore
/// renumbered, so a 1.x op decodes here as a different kind entirely and
/// its signature no longer verifies.
///
/// **This is the one break the major version exists for.** A 1.x node and a
/// 2.x node do not interoperate and must not try: the handshake refuses on
/// the major, which is the check that makes a silent misread impossible.
/// A data directory written by 1.x is refused with the same reasoning, and
/// the changelog says what to do (`meshnode reset`, or a fresh data dir).
///
/// It was spent deliberately and once, before publication, because a
/// retired variant cannot be removed at any later time for free: every
/// mesh in existence would have to be reset to collect the saving.
pub const PROTOCOL_MAJOR: u16 = 2;
/// Minor 4 = M5: scored placement + capacity (`MemberInfo::slots`,
/// `Response::Busy`), queue priorities (`JobSpec::priority`), fan-out
/// controls and origin-failure adoption (Fanout* ops), and Git sources
/// (Source* ops). Pre-1.0 note: M5 reshaped `MemberInfo`, `JobSpec`, and
/// `ServiceVersion`, so M4 data dirs and mixed meshes are not carried
/// forward — the wire freezes at 1.0, not before.
///
/// Minor 5 = M7 (WAN): **additive only**. New `Request` variants appended
/// at the tail (`Observe`, `RelayRegister`, `RelayOpen`, `PunchRequest`)
/// with matching `Response` variants (`Observed`, `RelayReady`,
/// `PunchSent`), the relay data-plane frames ([`RelayFrame`],
/// [`RelayServerFrame`]), the reserved [`RELAY_TAG`], and the
/// `relay:<node-id>` candidate scheme inside the existing
/// `MemberInfo::addrs: Vec<String>` (signature-covered op bodies are not
/// reshaped). 1.4 ↔ 1.5 meshes interoperate: a 1.5 node only sends the new
/// requests to peers that negotiated minor >= 5, and a 1.4 node skips
/// `relay:` candidates as unparseable (a relay-only member therefore looks
/// dead to it — documented degradation). M6 data dirs carry forward.
///
/// Minor 6 = M9 (parallel compute): **additive only**, and appended at the
/// enum tails in three instalments as the milestone was built —
/// [`Request::Pressure`] / [`Response::Pressure`] for live load (P2,
/// §5.2); `ParallelOffer` / `ChunkPull` / `ChunkDone` with
/// `Response::ChunkWork`, because §5.4's design has workers *pull* and a
/// worker cannot pull without a request to send (P4); and
/// `ObjectAnnounce` / `ObjectLocate` / `GetObject` with
/// `Response::ObjectHolders` / `Response::Object` for the ephemeral data
/// plane (P6, §5.5). Extending a *released* minor's variant set would be
/// the very break §3.4 rules out; 1.6 has not shipped, so the version
/// moves exactly once across all of M9.
///
/// Static capacity needs no wire change at all: it rides reserved `cap.*`
/// tags on the existing `MemberUpdate` (M9 §5.1). No new `OpBody` variant
/// is introduced, deliberately: ops travel as a `Vec<Op>` inside
/// `SyncResponse` and postcard decodes that vector atomically, so one
/// unknown variant index would fail a peer's *entire* sync response and
/// leave it permanently behind (M9 §3.4). Objects stay out of the op-log
/// for the same reason at a thousand times the volume, which is why they
/// are located through a rendezvous-hashed directory instead (§5.5).
/// 1.5 ↔ 1.6 meshes interoperate: every 1.6 request is gated on the
/// negotiated minor, unknown pressure is neutral in placement, and a 1.5
/// peer simply never takes part in a parallel job. M7 data dirs carry
/// forward.
///
/// Minor 7 = F0 (forward-compatible op-log): **additive only**, and the
/// change that lets every later milestone add replicated entity kinds at
/// all. Ops travel and are stored as [`OpEnvelope`], whose body is the
/// postcard bytes the signature already covered, left length-delimited —
/// so a node can verify, persist and forward an op it cannot decode
/// instead of failing the whole batch that carried it. Four appended
/// `Request` variants (`SyncV2`, `PushOpsV2`, `JoinV2`, `PairCompleteV2`)
/// and two appended `Response` variants (`SyncV2`, `JoinAcceptV2`) carry
/// the new shape; the V1 messages stay exactly as they were.
///
/// 1.6 ↔ 1.7 meshes interoperate, at a stated cost. A 1.7 node serving a
/// 1.6 peer drops ops the peer has no variant for **and truncates the
/// version vector it advertises** to just before each such op, so the peer
/// believes it is caught up as far as it can see, stops retrying, and is
/// stably behind rather than wedged (§3.3). It is behind only on entity
/// kinds it could not render anyway; `meshnode nodes` marks it
/// `stale (pre-1.7)` and `doctor` says how many ops it cannot receive.
/// M9 data dirs carry forward — the op-log is re-encoded in one pass on
/// first open, with the previous file kept as `oplog.redb.pre-f0`.
///
/// Minor 8 = M12 (inventory): **additive only**, and smaller than it
/// sounds. Host and group *records* are four new `OpBody` variants, which
/// cost nothing on the wire because F0 already made an unknown op body
/// carryable. The rendered inventory reaches the executor through reserved
/// `MESHWARE_INV_*` job environment keys — the `MESHWARE_TASK_*` idiom
/// (M9 §5.3) — so neither `JobSpec` nor `ServiceExec` grows a field.
/// The single new wire pair is `Request::HostProbes` /
/// `Response::HostProbes`, gated exactly as `Pressure` is, because host
/// reachability is an observation and not a fact worth replicating
/// (M12 §6.2).
///
/// 1.7 ↔ 1.8 meshes interoperate, with one gate that is enforced rather
/// than hoped for: a 1.7 executor has no code to materialize an inventory
/// from the environment, and running an Ansible play with *no* inventory
/// silently targets an implicit localhost. So the origin refuses to place
/// an inventory-carrying job on a pre-1.8 node instead of placing it and
/// hoping. Everything else degrades quietly: a 1.7 peer is never asked for
/// probes, and its vantage points read as unknown, which placement already
/// treats as neutral.
///
/// Minor 10 = chunked bootstrap: **additive only**, and a fix rather than a
/// feature. A joining node was handed the whole op-log in one frame
/// (`JoinAcceptV2`, `PairCompleteV2`), which is a design that works right
/// up until the mesh has more history than [`MAX_FRAME_LEN`] — and then
/// stops working permanently, because an op-log only grows. The encoder
/// refuses the frame, the join fails, and no amount of retrying helps: the
/// mesh has become one that cannot admit a node.
///
/// So the snapshot is delivered the way anti-entropy already delivers ops —
/// in bounded instalments, the receiver asking again while `more`. Three
/// appended `Request` variants (`JoinV3`, `JoinFetch`, `PairCompleteV3`)
/// and one appended `Response` variant (`Snapshot`) carry
/// [`SnapshotChunk`], and the loop runs in whichever direction the flow
/// already had a client: a token joiner *pulls* from the acceptor, and a
/// pairing approver *pushes* to the joiner, because pairing dials the other
/// way round.
///
/// 1.9 ↔ 1.10 meshes interoperate on the pre-existing terms: a peer below
/// this minor gets the single-frame V2 snapshot exactly as before, which is
/// correct whenever it fits and now fails with a message naming the version
/// to upgrade instead of a bare codec error. The same batch bound
/// (`store::SYNC_BATCH_BYTES`) is applied to ordinary sync, where the
/// count-only bound had the identical latent overflow. M17 data dirs carry
/// forward; nothing on disk changes.
///
/// Minor 12 = C4 (gang scheduling): **additive only**, three appended
/// `Request` variants (`GangOffer`, `GangCommit`, `GangRelease`) and one
/// appended `Response` variant (`GangOffer`). It is the first wire change
/// in the cluster direction, and the whole of it — the gang *record* is an
/// op body, which F0 already made carryable, and the runtime environment
/// rides reserved `MESHWARE_GANG_*` job env keys, which change no encoding
/// at all (C0-C9 §14.1).
///
/// The pair exists because nothing in the tree could hold a resource on
/// behalf of a decision not yet made: `admits()` is local and per-task, and
/// two large `parallel` jobs could each acquire half the mesh and neither
/// finish. An offer is a *soft* reservation with a TTL, and the TTL is the
/// failure model — an origin that dies between offer and commit costs the
/// mesh one lease lifetime and no distributed cleanup path, because there
/// is nothing to clean up.
///
/// 1.11 ↔ 1.12 meshes interoperate on terms the origin enforces rather than
/// hopes for. A pre-1.12 node cannot hold a lease — it has no code for the
/// request and would drop the stream — so a gang **never offers to one**:
/// such a node is filtered out of the candidate set before ranking, and a
/// mesh with too few 1.12 nodes refuses the gang by name and version
/// instead of placing part of it. That is the [`INVENTORY_MINOR`] rule
/// (refuse rather than half-do it) rather than the [`SESSION_MINOR`] one,
/// and for the same reason: a gang that started on six of eight nodes is
/// the exact failure this milestone exists to prevent. Ordinary jobs,
/// including on pre-1.12 peers, are unaffected — a mesh that never submits
/// a gang never sends one of these requests. M24 data dirs carry forward.
/// Minor 13 = C5 (devices): **additive only**, and one appended pair —
/// `PressureV2` request and response — which is the entire wire cost of the
/// accelerator model.
///
/// Everything else in C5 rides shapes that already exist. The device
/// *inventory* is an op body (`DeviceInventoryPut`), which F0 already made
/// carryable by a node that has never heard of it; the coarse selector
/// filter is two more `cap.*` tags on the existing `MemberUpdate`; the
/// device *request* is a reserved `MESHWARE_TASK_DEVICES` job environment
/// key; and the allocation reaches the task as `CUDA_VISIBLE_DEVICES` and
/// its vendor siblings, which are not meshware's encoding at all.
///
/// The pair exists because live device telemetry could not be added to
/// [`capacity::PressureSnapshot`]: that struct is embedded in
/// `Response::Pressure`, so a new field would make every older peer
/// misparse every pressure reply. This is the fourth time that rule has
/// been applied in this codebase and it will not be the last (C0-C9 §9.1).
///
/// 1.12 ↔ 1.13 meshes interoperate with no gate at all, which is the
/// difference between this minor and [`CLUSTER_MINOR`]. A pre-1.13 peer is
/// asked the V1 pressure question instead of the V2 one and its device
/// telemetry stays unknown — neutral in placement, exactly like an unknown
/// RTT. It can still *hold* devices and still *advertise* them, because
/// neither the inventory nor the allocation needs the new minor; a 1.12
/// node simply carries the inventory op opaquely and never renders it.
/// C4 data dirs carry forward; nothing on disk changes.
///
/// Minor 19 = S1 (wire authorization): one appended request,
/// [`Request::CancelJobV2`], which carries the canceller's own signed
/// envelope so the **executor** can apply `require_cancel` rather than
/// taking a peer's word for it. Additive and ungated: a pre-1.19 peer is
/// sent the V1 request instead, exactly as before.
/// Minor 20 = Y5 (hardware presence): one appended op,
/// [`OpBody::PolicyPresencePut`], letting a change-control policy require a
/// FIDO2 touch on what it governs.
///
/// **A capability gate, and this one has teeth.** Most gates here degrade
/// into "that peer does less"; this one degrades into "that peer does not
/// enforce a control an admin wrote." A pre-1.20 node carries the op
/// opaquely — correct for convergence, and it means a privileged action
/// routed through that node skips the touch. Presence is therefore only
/// sound once every member is on 1.20, which `meshnode policy presence`
/// reports rather than leaving somebody to assume.
/// Minor 22 = Y5 completed (the assertion's ride): one appended op,
/// [`OpBody::ApprovalDecisionV2`], an approve/deny that carries the FIDO2
/// touch that authorized it. Until this minor the mesh could *state* a
/// presence requirement and *verify* an assertion, and nothing carried one
/// from the person who touched the key to the nodes that check it.
///
/// **The gate points the other way from 1.20's, and that is the good
/// direction.** A pre-1.20 node under-enforces: it carries the requirement
/// opaquely and lets a privileged action past without a touch. A pre-1.22
/// node under-*counts*: it carries the touched decision opaquely, so the
/// approval it cannot read simply does not add to the tally and the gate
/// stays shut. One failure opens a door; the other leaves it closed.
/// Presence readiness reported against this minor for
/// exactly that reason — both halves have to be present for the feature to
/// behave the way the screen says it does.
/// Minor 30 = SEC-1 (acting-for, proven): no new variant anywhere. An
/// acting-for claim gains two reserved job env keys, `MESHWARE_ACT_PROOF`
/// and `MESHWARE_ACT_PROOF_SIG`, carrying the user-signed envelope the
/// claim stands on ([`acting`]). A 1.30 verifier refuses a claim without
/// one, because without one any member could speak for any user.
///
/// **This minor closes a door rather than opening one**, so it has no gate
/// to wait on. A 1.30 node mints both signatures, and a pre-1.30 verifier
/// reads the one it knows and behaves exactly as before. What a mixed mesh
/// loses is the other direction: a job running on a pre-1.30 node can no
/// longer act for its submitter on a 1.30 node, and says so by name.
/// Minor 31 = SEC-12 (envelopes with an audience): no new variant. A
/// request may be signed under `mw-ipc-v2` ([`ipc::ipc_signing_bytes_for`]),
/// which binds it to one mesh and one node, and a request over the HTTP
/// API must be. Everything signed at a terminal stays `mw-ipc-v1`.
///
/// **What a mixed mesh loses:** work submitted over the HTTP API carries a
/// v2 envelope bound to the node that took it, and a pre-1.31 executor
/// cannot verify one — it refuses the job as an invalid signature. Work
/// submitted any other way is unaffected in both directions.
/// Minor 32 = SEC-15: one appended request, [`wire::Request::FollowJobV2`],
/// a log read that carries the reader's own signed `JobLogs` request. An
/// executor now answers the legacy `FollowJob` only for the node that
/// dispatched the job (or owns its fan-out or gang), so a pre-1.32 node
/// can no longer read the output of a job it did not dispatch from a 1.32
/// executor — which is exactly the read that used to need no permission.
/// Minor 33 = SEC-18: one appended request, [`wire::Request::CachePublishV2`],
/// and its one appended answer, [`wire::Response::CachePlanNeeded`]. A cache
/// entry now travels with the job spec or `map` plan it was computed from,
/// and a directory replica indexes it only if the publisher ran that work
/// and the work derives that key. **What a mixed mesh loses:** a pre-1.33
/// node's entries are not indexed by a 1.33 replica, and a 1.33 node
/// publishes nothing to a pre-1.33 one — a miss, which the cache already
/// treats as free.
/// Minor 34 = SEC-7: one appended op kind, [`ops::OpBody::SecretScopeSet`],
/// and local requests to set and read it. **What a mixed mesh loses:** a
/// pre-1.34 node carries a scope without reading it, so it goes on healing
/// a scoped secret for every member and resolving it for anyone — scope a
/// secret once the nodes that hold it are upgraded.
/// Minor 35 = device readings that name their device: one appended
/// request, [`wire::Request::TelemetryV2`], and its appended answer,
/// [`wire::Response::TelemetryV2`]. A reading used to carry only the
/// vendor's ordinal, and on a box with an NVIDIA card and an AMD integrated
/// GPU — both ordinal 0 — each card was shown, exported and placed on with
/// the other's load. **What a mixed mesh loses:** a pre-1.35 peer is asked
/// the old question and its readings arrive unnamed, so they are used only
/// where the ordinal names one card on that machine and are unknown on a
/// mixed-vendor box, which placement treats as neutral.
/// Minor 36 = mesh reset: one appended op kind, [`ops::OpBody::MeshReset`],
/// and the local request that publishes it. **What a mixed mesh loses:** a
/// pre-1.36 node carries the reset without acting on it, so it stays in
/// the old mesh after everyone else has left, and has to be adopted into
/// the new one with `--force`.
/// **Does not restart at major 2**, and that is deliberate. Every feature
/// gate below is a `peer_minor >= N` comparison with N as high as 36, and
/// those gates are still read — between two 2.x nodes as much as they ever
/// were. Resetting this to 0 would make a node decide it was too old for
/// its own features, which is the bug the `const { assert!(PROTOCOL_MINOR
/// >= …) }` checks in this file's tests exist to catch, and which they did.
///
/// A 1.x peer is refused on the *major*; the minor keeps counting.
pub const PROTOCOL_MINOR: u16 = 36;

/// Mesh reset. Below this a node carries [`ops::OpBody::MeshReset`] and
/// does not act on it.
pub const MESH_RESET_MINOR: u16 = 36;

/// Device readings with their vendor and kind. Below this a peer is asked
/// [`wire::Request::Telemetry`], whose readings carry the bare ordinal.
pub const DEVICE_KEY_MINOR: u16 = 35;

/// Secret scopes (SEC-7).
pub const SECRET_SCOPE_MINOR: u16 = 34;

/// A cache entry with its proof (SEC-18). Below this a replica is not sent
/// the entry at all.
pub const CACHE_PROOF_MINOR: u16 = 33;

/// A log read with its reader's envelope (SEC-15). Below this a peer is sent
/// the legacy `FollowJob`.
pub const LOGS_MINOR: u16 = 32;

/// A terminal inside a container instance, `SessionKind::Instance`, and the
/// per-instance live stats request that arrives with it (Phase B, E3). A
/// node below this cannot decode either, so the origin refuses by name
/// rather than letting the far node drop the stream.
pub const INSTANCE_MINOR: u16 = 27;

/// Deployment history and tags (Phase B, E1). A node below this neither
/// records a version when its daemon publishes one nor reads a tag — it
/// carries both opaquely — so on a mixed mesh `deployment history` can come
/// up short for publishes that went through an older node.
pub const HISTORY_MINOR: u16 = 28;

/// Manual promotion (Phase B, E2). A node below this reconciles a deployment
/// that set `auto_promote = false` as though it had not — its canaries cut
/// over the moment they settle — so `status` names every member that would,
/// under the policy-blind warning.
pub const PROMOTE_MINOR: u16 = 28;

/// The mesh proxy (DNS hardening), **retired**. `Request::ServiceConnect`
/// was the stream a node's proxy opened to the node running an instance. No
/// build sends it any more, and this build answers one from an older node
/// with an error, which that node's proxy treats like any refused instance.
/// Kept so the variant keeps its minor and its postcard index.
pub const PROXY_MINOR: u16 = 29;

pub use deployment::{
    DEFAULT_PRIORITY, DeploymentKnobs, DeploymentPromotion, KNOB_AUTO_PROMOTE, MigratePolicy,
    PREEMPT_MARGIN, REQ_MAY_PREEMPT, REQ_MIGRATE, REQ_PRIORITY, may_preempt, priority_of_env,
};

/// First minor that announces which runtimes a node can serve (T3):
/// the `cap.rt` tag, and the [`IpcRequest::Runtimes`] pair beside it.
///
/// **A gate that leaves a door open, deliberately, and in the direction
/// that costs nothing.** A node below this announces no `cap.rt`, which
/// [`runtimes::from_tags`](crate::runtimes::from_tags) reads as *unknown*
/// rather than *none* — so it stays a candidate for every kind of work,
/// exactly as it was before this existed.
///
/// The alternative was the `cap.ports` rule, where an absent tag is a
/// statement. It is wrong here for one reason: a pre-1.26 node may have a
/// `container_plugin` configured and be running containers perfectly well,
/// so excluding it would mean **upgrading one node silently shrinks the
/// mesh's container capacity** — a deployment shedding replicas with
/// nothing anywhere saying why. `meshnode runtimes` names every node in
/// that state, so the ambiguity is visible rather than silently carried.
pub const RUNTIME_MINOR: u16 = 26;

/// First minor that honours a mesh-wide load shed (N6 §12):
/// [`OpBody::ShedPut`].
///
/// **The gate that fails open, and the only one.** Every other capability
/// minor in this product leaves an older node doing *less* than it was
/// asked: no registry, no rollout policy, no presence requirement. This one
/// leaves it doing *more* — a node that cannot read the record keeps taking
/// submissions and keeps reconciling, which is precisely what the operator
/// flipped the switch to stop.
///
/// So the readout is load-bearing rather than decorative:
/// [`shed::ShedReadiness`](crate::shed::ShedReadiness) names every member
/// that will ignore it, `meshnode shed status` prints that list, and
/// `doctor` reports it. An operator shedding load on a half-upgraded mesh
/// has to be told which half they actually shed.
pub const SHED_MINOR: u16 = 25;

/// First minor that applies a deployment's rollout and partition policy
/// (N1 P3/P4): [`OpBody::DeploymentPolicyPut`].
///
/// **A door left open, deliberately.** A node below this carries the policy
/// op opaquely and reconciles the deployment exactly as it does today — no
/// canary, no progress deadline, no `lost_after`, no auto-revert. The
/// deployment itself is a separate, unchanged op, so it stays fully visible
/// and fully reconcilable there; what an old node loses is the *care* with
/// which a roll is done, not the roll.
///
/// That is the right direction for this feature and the wrong one for some
/// others, which is why this is its own constant: the failure here is
/// "rolled faster than you asked", not "ran somewhere it should not have".
/// It is still a failure — a mesh that elects an old node to reconcile a
/// canary deployment gets no canary and no warning from the roll itself —
/// so `doctor` reports every member behind it by name.
pub const DEPLOY_POLICY_MINOR: u16 = 23;

/// First minor that publishes and reads the service registry (N3 P1):
/// [`OpBody::RegistryPut`].
///
/// **A capability gate of the mildest kind, and not a safety one.** A node
/// below this registers nothing, so its instances are absent from discovery
/// and from `<service>.mesh` — they run exactly as they always did and
/// simply cannot be found by name. Nothing degrades wrongly; a resolve just
/// reaches fewer instances, which is the same answer a mesh gets for a
/// service that has not started yet.
///
/// The failure worth naming is the *quiet* one: a rolling upgrade through
/// this minor makes a service's instance count appear to shrink in DNS
/// while nothing is actually down. `doctor` reports every member below it
/// for exactly that reason.
pub const REGISTRY_MINOR: u16 = 24;

/// First minor that can be asked whether it can see a path (R4 P2).
///
/// The question a mixed mesh needs answered before it schedules: *"can you
/// open this?"* Data lives on a share that three of your six machines mount,
/// and the other three have no way to say so — they accept the work, open the
/// path, and fail. Multiplied across four hundred items that is four hundred
/// identical errors and a run that took the long way to tell you about a
/// missing mount.
///
/// Degrades the safe way. A node below this minor is never *asked*, and an
/// unasked node is never excluded — it is offered work exactly as it is
/// today, and a genuinely unreachable path still fails at the item with the
/// message it always had. Filtering only ever narrows candidates that
/// answered "no", which is why a silent old node cannot cause a run to find
/// nowhere to go.
pub const REACH_MINOR: u16 = 21;

/// First minor that can serve its archive and replay a payload (M31 B3).
///
/// **A capability gate, not a safety one.** A pre-1.18 peer simply has no
/// older history to offer and cannot fetch a payload back — so the asker
/// skips it and says what it found, which is the same sentence it says for
/// a node that has no archive at all. Nothing degrades wrongly; a browse
/// just reaches fewer machines.
pub const ARCHIVE_MINOR: u16 = 18;

/// First minor that understands sinks (M31 B1): `OpBody::SinkPut` and
/// `OpBody::SinkRemove`.
///
/// **A gate on *election*, not on carriage.** F0 means a pre-1.17 node
/// stores, verifies, counts and forwards a sink op like any other — so a
/// mixed mesh is fine and nothing wedges. What the minor is for is the
/// exporter election: a sink must never elect a node that has no code to
/// export it, because that node would win the rendezvous, do nothing, and
/// the sink would silently stop while `meshnode sinks` showed a healthy
/// elected firer. Silence that looks like health is the failure mode worth
/// spending a minor on.
pub const SINK_MINOR: u16 = 17;

/// First minor that understands the WAN additions (reverse streams,
/// `Observe`, relay, punch). A node sends those only to peers that
/// negotiated at least this minor.
pub const WAN_MINOR: u16 = 5;

/// First minor that understands the compute additions (live pressure).
/// A node asks a peer for pressure only at or above this minor; below it,
/// the peer's pressure stays unknown, which placement treats as neutral.
pub const COMPUTE_MINOR: u16 = 6;

/// First minor that understands length-delimited op bodies (F0). Below it
/// a peer gets the V1 `Vec<Op>` messages and the downgrade rule: ops it
/// has no variant for are withheld, and the version vector it is told is
/// truncated to match, so it converges on the subset it can decode instead
/// of retrying a batch that can never land.
pub const OPLOG_MINOR: u16 = 7;

/// First minor that understands the inventory additions (M12): the
/// `HostProbes` request/response pair, and — the part that is a *safety*
/// gate rather than a feature gate — materializing a rendered inventory out
/// of the reserved `MESHWARE_INV_*` job environment.
///
/// A job whose environment carries an inventory is never placed below this
/// minor. The reason is specific: an older executor would ignore the keys,
/// pass no `-i` to `ansible-playbook`, and run the play against an implicit
/// localhost. "Configures the wrong machine, silently" is the one
/// degradation that is worse than refusing, so this is the one place M12
/// refuses.
pub const INVENTORY_MINOR: u16 = 8;

/// First minor that understands sessions (M17): `Request::SessionOpen` and
/// the two frame enums it upgrades the stream to.
///
/// A feature gate, not a safety gate. A pre-1.9 peer cannot be shelled
/// into, copied to, or forwarded through, and the refusal names the
/// version rather than hanging — a 1.8 node would fail to decode the
/// request and simply drop the stream, which from the initiator's end is
/// indistinguishable from a hang.
pub const SESSION_MINOR: u16 = 9;

/// First minor that receives a bootstrap snapshot in bounded instalments
/// rather than one frame.
///
/// Below it a joiner gets the single-frame `JoinAcceptV2` /
/// `PairCompleteV2`, which is what it has code to read. That is a real
/// ceiling and not a degradation the joiner can shrug off — past
/// [`MAX_FRAME_LEN`] of history the join cannot complete at all — so the
/// acceptor checks the encoded size before it tries, and refuses with the
/// version to upgrade to rather than a codec error from three layers down.
pub const BOOTSTRAP_MINOR: u16 = 10;

/// First minor that understands the fleet additions of M24:
/// [`Request::BundleFetch`] ("fetch this bundle now, and tell me how far
/// you have got") and [`Request::UpgradeSelf`] ("fetch your own signed
/// release and swap yourself").
///
/// Both are scheduling gates, and the mildest kind in this list.
///
/// Prefetching changes nothing about what is authoritative: the bundle is
/// already content-addressed, already published, already resolvable, and a
/// node that is never asked still converges the moment it needs the
/// content. A peer below this minor is reported as *not pushed to* rather
/// than as failed, and it fetches lazily exactly as it always did.
///
/// `UpgradeSelf` carries no binary — it asks a node to fetch and verify
/// its own signed release, which it could already do by hand. A peer below
/// this minor is reported as needing `meshnode upgrade --self` run on it,
/// which is exactly what it needed before this existed.
pub const FLEET_MINOR: u16 = 11;

/// First minor that understands the gang additions of C4: the
/// `GangOffer` / `GangCommit` / `GangRelease` requests and the lease table
/// behind them.
///
/// A **safety** gate, not a feature gate, and the second one in this list
/// after [`INVENTORY_MINOR`]. A node below this minor cannot hold a soft
/// reservation, so offering to it would mean believing a resource was held
/// when nothing was holding it — and an all-or-nothing admission that is
/// wrong about one of its N nodes has admitted nothing at all while
/// starting work on N-1 of them. So gang candidates are filtered to this
/// minor before placement runs, and a mesh with fewer than N such nodes is
/// told to upgrade rather than handed a partial gang.
///
/// Nothing else in C4 is gated: the gang record is an op body (F0 carries
/// it opaquely on an older node), and the runtime environment is job env.
pub const CLUSTER_MINOR: u16 = 12;

/// First minor that understands the device additions of C5: the
/// [`wire::Request::PressureV2`] / [`wire::Response::PressureV2`] pair.
///
/// A **feature** gate, and the mildest kind. A peer below this minor is
/// asked the V1 pressure question, answers it exactly as it always did, and
/// its per-device telemetry reads as unknown — which the scheduler already
/// treats as neutral rather than as idle or as busy. Nothing about the
/// device model needs the gate: the inventory is an op body (F0 carries it
/// opaquely), the filter is a tag, the request is job environment, and the
/// allocation is `CUDA_VISIBLE_DEVICES`.
///
/// So a 1.12 GPU box in a 1.13 mesh advertises its cards, is selected for
/// GPU work, leases its devices and isolates them. What it does not do is
/// report which of them is idle — and the placement term that would have
/// used that simply does not fire.
pub const ACCEL_MINOR: u16 = 13;

/// First minor that understands the action cache of C7: the
/// [`wire::Request::CacheLookup`] / [`wire::Request::CachePublish`] pair and
/// [`wire::Response::CacheEntry`].
///
/// A **feature** gate, and the mildest kind there is — milder even than
/// [`ACCEL_MINOR`], because a peer below it is not asked a different
/// question, it is simply not asked. The cache directory is a rendezvous
/// ranking over the live members; a member that cannot answer a lookup is
/// skipped in the ranking, so a 1.13 node in a 1.14 mesh runs every job it
/// is given and indexes none of them, which is exactly how the whole mesh
/// behaved a version ago.
///
/// Nothing else in C7 is gated. The key is a BLAKE3 digest computed
/// identically on every node from data that already travels; the entry is
/// directory state and never an op; the outputs are ordinary M9 objects
/// fetched through the existing `GetObject` path; and `cacheable` is a
/// service tag and a reserved `MESHWARE_TASK_CACHE` environment key.
///
/// **A miss must cost nothing**, which is the reason the gate is a skip
/// rather than a negotiation: a mesh with no 1.14 members does one
/// rendezvous ranking, finds nobody, and dispatches — no round trip, no
/// timeout, no wait.
pub const CACHE_MINOR: u16 = 14;

/// First minor that understands [`IpcRequest::RunScript`] — a job whose
/// body is a script carried in the request rather than a Library entry or
/// an ad-hoc command. What the build farm submits (`meshnode cc`).
///
/// A **hard** gate, and the strictest kind in this file, because the thing
/// an older executor cannot do is not "answer a new question" — it is
/// *decode the job it was handed*. Every executor re-derives authority from
/// the user's signed envelope inside the spec (§7.2), and that means
/// `postcard::from_bytes::<IpcRequest>` over a payload whose variant it has
/// never heard of. It does not degrade; it fails to parse, and the job dies
/// on arrival with a message about malformed authorization that says
/// nothing about the real cause.
///
/// So placement filters these jobs to peers at or above this minor, and
/// [`crate::script::requires_script_support`] is the marker it filters on —
/// the same shape as the 1.8 inventory gate, and for the same reason:
/// refusing to place is a sentence an operator can act on, and placing
/// blind is a failure they cannot read.
///
/// Nothing *else* about a script job is new. `JobPayload::Script` has
/// travelled since M10 — a workflow step is one — and a 1.14 executor runs
/// those happily; it is only the request shape in the signed envelope that
/// changed.
pub const FARM_MINOR: u16 = 15;

/// First minor that answers [`crate::wire::Request::Telemetry`] — live
/// system telemetry, and the first pressure signal that *spreads*.
///
/// Two things change at this minor, and they are the same change.
///
/// **What is measured.** `Response::Pressure` reports `load1` and PSI, and
/// on a 16-core machine with every core pegged those read 0.20-per-core and
/// 2.6% — both indistinguishable from idle, for twelve seconds and counting.
/// `load1` averages the run queue over a minute; PSI `some` counts tasks
/// *waiting* for a CPU, which is nothing when the thread count matches the
/// core count. [`crate::SystemSnapshot::cpu_pct`] measures utilisation, from
/// `/proc/stat` deltas, and moves immediately.
///
/// **How far it travels.** `Request::Pressure` asks one peer about itself,
/// once per gossip tick, and gossip picks one partner per tick — so a node
/// learns about each peer roughly every *n* ticks while readings expire
/// after thirty seconds. Past about thirty nodes every reading is always
/// stale, which the M9 comment recorded as deliberate and which is really
/// just the pull model's ceiling. `Request::Telemetry` answers with
/// everything the peer knows, so readings spread epidemically like liveness
/// does: log(n) rounds to reach the mesh, at the same one round trip per
/// tick it already spent.
///
/// A peer below this minor is still asked the question it has code for, and
/// its reading still arrives — first-hand only, as before.
pub const TELEMETRY_MINOR: u16 = 16;

/// First minor that accepts a **user-authorized** cancel over the wire
/// ([`Request::CancelJobV2`]).
///
/// The V1 [`Request::CancelJob`] carries a job id and nothing else, so the
/// executor could only ever check that the *node* asking was a member —
/// which meant the `require_cancel` rule ("a developer stops their own
/// work, not somebody else's") lived entirely on the node the person
/// happened to be talking to. V2 forwards the person's own signed envelope
/// so the executor re-verifies the signature, resolves the user, and
/// applies the same gate against its own view of the job.
///
/// **A capability gate, not a safety one.** A pre-1.19 executor is sent the
/// V1 request, because the alternative is a cancel that silently does
/// nothing during a rolling upgrade. The V1 arm is still served for that
/// reason and for gang teardown, which is one node telling another to stop
/// a job it placed there itself and has no person behind it.
pub const AUTHZ_MINOR: u16 = 19;

/// ALPN identifier for mesh QUIC connections.
pub const ALPN: &[u8] = b"mw/1";

/// Reserved node tag: a member carrying it participates fully in the mesh
/// (state, submission, streaming) but is never a placement candidate — the
/// `no-execute` capability behind Director's contribute-compute setting
/// (MASTERPLAN D6).
pub const NO_EXECUTE_TAG: &str = "no-execute";

/// Reserved node tag: nodes carrying it are preferred candidates for
/// watching Git sources (MASTERPLAN §8.1). When no live member carries it,
/// the watcher is elected among all live executors.
pub const GIT_SYNC_TAG: &str = "git-sync";

/// Reserved node tag: the node is draining (MASTERPLAN §12) — it finishes
/// what it accepted but is never a placement candidate. Announced via an
/// ordinary `MemberUpdate` so drain needs no wire-schema change; cleared by
/// the node's boot-time re-announcement (a restarted node is not draining).
pub const DRAINING_TAG: &str = "draining";

/// Node tag prefix: this node is being drained, and its deployment instances
/// must be off it by `drain.by=<unix seconds>` — or `drain.by=never` (Phase C,
/// E9).
///
/// **Announced or assigned.** A node running `meshnode drain` announces it
/// beside `draining`, because it knows when it began; an admin assigns it to
/// another node with `meshnode nodes drain`, which moves that node's instances
/// off without stopping it — Nomad's drain, where `draining` is meshware's
/// shutdown. Either way it is an ordinary tag, so it costs no wire change and
/// an older node carries it without reading it.
pub const DRAIN_BY_PREFIX: &str = "drain.by=";

/// Node tag: a drain leaves `count = "system"` instances where they are
/// (Phase C, E9 — Nomad's `-ignore-system`). Only meaningful beside
/// [`DRAIN_BY_PREFIX`].
pub const DRAIN_KEEP_SYSTEM_TAG: &str = "drain.keep-system";

/// How long a drain gives instances to move when nobody said (Phase C, E9).
/// Nomad's default drain deadline.
pub const DEFAULT_DRAIN_DEADLINE_SECS: u64 = 3600;

/// What a node's tags say about draining it (Phase C, E9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeDrain {
    /// When whatever is left is cancelled. `None` is `never`; a node that
    /// announces `draining` with no `drain.by` — every build before this —
    /// reads as a deadline already passed, which is what its drain has
    /// always meant: move everything now.
    pub deadline_micros: Option<u64>,
    /// `count = "system"` instances stay.
    pub keep_system: bool,
    /// The node is shutting down (`draining`), not merely being emptied. A
    /// shutdown with nowhere for an instance to go does not wait for a place
    /// to appear.
    pub leaving: bool,
}

/// Read a node's drain state off its tags. `None` when it is not draining.
pub fn drain_of<S: AsRef<str>>(tags: &[S]) -> Option<NodeDrain> {
    let leaving = tags.iter().any(|t| t.as_ref() == DRAINING_TAG);
    let by = tags
        .iter()
        .find_map(|t| t.as_ref().strip_prefix(DRAIN_BY_PREFIX));
    if !leaving && by.is_none() {
        return None;
    }
    let deadline_micros = match by {
        // A number nobody could read is not a reason to wait for ever.
        Some("never") => None,
        Some(value) => Some(value.parse::<u64>().unwrap_or(0).saturating_mul(1_000_000)),
        None => Some(0),
    };
    Some(NodeDrain {
        deadline_micros,
        keep_system: tags.iter().any(|t| t.as_ref() == DRAIN_KEEP_SYSTEM_TAG),
        leaving,
    })
}

/// The tags that state a drain: `drain.by=…`, and `drain.keep-system` when
/// asked. `deadline_micros: None` is `never`.
pub fn drain_tags(deadline_micros: Option<u64>, keep_system: bool) -> Vec<String> {
    let mut tags = vec![match deadline_micros {
        Some(at) => format!("{DRAIN_BY_PREFIX}{}", at / 1_000_000),
        None => format!("{DRAIN_BY_PREFIX}never"),
    }];
    if keep_system {
        tags.push(DRAIN_KEEP_SYSTEM_TAG.to_string());
    }
    tags
}

/// Is this one of the tags [`drain_tags`] writes?
pub fn is_drain_tag(tag: &str) -> bool {
    tag.starts_with(DRAIN_BY_PREFIX) || tag == DRAIN_KEEP_SYSTEM_TAG
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    #[test]
    fn a_drain_reads_back_what_its_tags_wrote() {
        assert_eq!(drain_of::<&str>(&["edge"]), None);
        let at = 1_700_000_000_000_000;
        let tags = drain_tags(Some(at), true);
        assert_eq!(
            drain_of(&tags),
            Some(NodeDrain {
                deadline_micros: Some(at),
                keep_system: true,
                leaving: false
            })
        );
        let never = drain_tags(None, false);
        assert_eq!(drain_of(&never).and_then(|d| d.deadline_micros), None);
        assert!(tags.iter().all(|t| is_drain_tag(t)));
    }

    /// Every node before Phase C announces `draining` alone. That drain has
    /// always meant "move everything now", and it still does.
    #[test]
    fn draining_with_no_deadline_is_a_deadline_already_passed() {
        let drain = drain_of(&[DRAINING_TAG]).expect("draining");
        assert_eq!(drain.deadline_micros, Some(0));
        assert!(drain.leaving);
        let mut announced = vec![DRAINING_TAG.to_string()];
        announced.extend(drain_tags(Some(5_000_000), false));
        assert_eq!(
            drain_of(&announced).and_then(|d| d.deadline_micros),
            Some(5_000_000)
        );
    }

    /// Phase C, E10: the default queues as every instance always has, and
    /// the two scales meet where the margin needs them to.
    #[test]
    fn a_priority_reads_onto_the_queue_classes_and_back() {
        assert_eq!(
            JobPriority::from_score(DEFAULT_PRIORITY),
            JobPriority::Normal
        );
        assert_eq!(JobPriority::from_score(1), JobPriority::Low);
        assert_eq!(JobPriority::from_score(80), JobPriority::High);
        assert_eq!(priority_of_env(&[]), DEFAULT_PRIORITY);
        for (value, read) in [("80", 80), ("0", 50), ("101", 50), ("x", 50)] {
            assert_eq!(
                priority_of_env(&[(REQ_PRIORITY.into(), value.into())]),
                read,
                "{value}"
            );
        }
        // A low-priority preemptible job is within reach of a deployment at
        // 35; a normal one needs 60.
        assert!(JobPriority::Low.score() + PREEMPT_MARGIN <= 35);
        assert!(JobPriority::Normal.score() + PREEMPT_MARGIN > 59);
        assert!(!may_preempt(&[(REQ_MAY_PREEMPT.into(), "0".into())]));
    }

    #[test]
    fn migrate_defaults_say_nothing_and_the_rest_round_trips() {
        assert!(MigratePolicy::default().to_env().is_empty());
        let m = MigratePolicy {
            max_parallel: 2,
            min_healthy_secs: 30,
            healthy_deadline_secs: 600,
        };
        assert_eq!(MigratePolicy::from_env(&m.to_env()), m);
    }
}

/// Reserved node tag: this node runs jobs, but nobody gets a terminal on it
/// (M17 §3.4).
///
/// One notch narrower than [`NO_EXECUTE_TAG`], and the distinction is the
/// point: `no-execute` says *do not place work here*, which is a capacity
/// statement; `no-shell` says *this is not a machine people get prompts on*,
/// which is a policy statement about a box that is otherwise a perfectly
/// good executor. Production is usually exactly that.
///
/// Replicated like any tag, so an admin carves out a node or a whole group
/// from anywhere rather than editing a TOML file on six hundred machines —
/// and the node's own `[sessions] accept = "off"` still wins over it, because
/// the machine's owner has the last word about their own machine.
pub const NO_SHELL_TAG: &str = "no-shell";

/// Reserved node tag (MASTERPLAN M7): the member relays traffic for NATed
/// peers. Announced automatically when `[wan] relay = "on"` and the node
/// has verified it is publicly reachable; NATed members register with
/// relay-tagged members and announce `relay:<node-id>` candidates.
pub const RELAY_TAG: &str = "relay";

// Reserved tag *prefix* (MASTERPLAN M9 §5.1): runtime-managed hardware
// capacity, e.g. `cap.cores=32`. Unlike the exact-match reserved tags
// above these are `key=value` pairs — see [`capacity`], which owns
// `CAP_TAG_PREFIX` and the typed `Capacity` they parse into. Announced on
// the ordinary `MemberUpdate`, so capacity costs no wire change and older
// nodes simply ignore them.

/// Candidate-address scheme prefix for "reachable via this relay member".
pub const RELAY_CANDIDATE_PREFIX: &str = "relay:";

/// Build a `relay:<node-id>` candidate for `MemberInfo::addrs`.
pub fn relay_candidate(relay: &NodeId) -> String {
    format!("{RELAY_CANDIDATE_PREFIX}{}", relay.to_display_string())
}

/// Parse a `relay:<node-id>` candidate; `None` for anything else (a bare
/// `host:port` direct candidate, or garbage — both are left to the
/// transport's address resolver, which skips what it cannot parse).
pub fn parse_relay_candidate(candidate: &str) -> Option<NodeId> {
    let id = candidate.strip_prefix(RELAY_CANDIDATE_PREFIX)?;
    NodeId::parse(id).ok()
}

/// Hard cap on a single control frame. Content transfer will use its own
/// streaming path with chunking; control messages stay small.
pub const MAX_FRAME_LEN: u32 = 4 * 1024 * 1024;

/// Upper bound on one relayed datagram payload (MASTERPLAN M7). The inner
/// QUIC session's MTU discovery tops out below this; anything larger is
/// not a QUIC packet and is dropped by the relay.
pub const MAX_RELAY_FRAME: usize = 1500;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("frame too large: {0} bytes (max {MAX_FRAME_LEN})")]
    FrameTooLarge(u32),
    #[error("codec error: {0}")]
    Codec(#[from] postcard::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid identifier: {0}")]
    InvalidId(String),
}

/// A hybrid logical clock timestamp: wall-clock microseconds plus a logical
/// counter, totally ordered and immune to modest clock skew (MASTERPLAN §4.4).
/// The clock that *produces* these lives in mesh-core; the value travels here.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize, Default,
)]
pub struct Hlc {
    pub wall_micros: u64,
    pub logical: u32,
}

// ---------------------------------------------------------------------------
// Frame codec: u32 LE length prefix + postcard body
// ---------------------------------------------------------------------------

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<(), ProtoError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    // **Encoded with the length prefix reserved in front of it** (L-13).
    //
    // This wrote the four-byte header and the body as two separate
    // `write_all`s, which on a TCP or TLS stream is two syscalls — and, for
    // a small frame, a four-byte packet followed by the payload. Every
    // frame in the system goes through here: every gossip response, every
    // IPC reply, every log chunk.
    //
    // `to_extend` appends to a buffer that already holds the placeholder,
    // so the prefix is filled in afterwards and the whole frame leaves in
    // one write, with no second allocation to concatenate them.
    let mut frame = Vec::with_capacity(4 + 256);
    frame.extend_from_slice(&[0u8; 4]);
    let mut frame = postcard::to_extend(msg, frame)?;
    let len = u32::try_from(frame.len() - 4).map_err(|_| ProtoError::FrameTooLarge(u32::MAX))?;
    if len > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge(len));
    }
    frame[..4].copy_from_slice(&len.to_le_bytes());
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(r: &mut R) -> Result<T, ProtoError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_frame_limited(r, MAX_FRAME_LEN).await
}

/// [`read_frame`] with a tighter cap than [`MAX_FRAME_LEN`] — for a peer
/// that has not earned the full one (SEC-10).
pub async fn read_frame_limited<R, T>(r: &mut R, max_len: u32) -> Result<T, ProtoError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > max_len.min(MAX_FRAME_LEN) as usize {
        return Err(ProtoError::FrameTooLarge(len as u32));
    }
    // **Grown as the bytes arrive, not reserved on the sender's word**
    // (S-M3). A frame begins with "here is how big I am", and this used to
    // allocate that number before a single byte of body had shown up:
    // quinn's default allows 100 concurrent bidirectional streams per
    // connection, and the transport spawns a task per stream, so a peer
    // that opened them all and wrote only the four-byte prefix on each held
    // 400 MB having sent 400 bytes.
    //
    // The cap is the same and the fast path is the same shape — one
    // allocation for an ordinary frame, which is far below the chunk — but
    // the memory now tracks what was actually delivered.
    // Grown **on the heap**, one chunk at a time. Not a stack buffer: this
    // is an `async fn`, so any array declared here would live in its future
    // — and `read_frame` is awaited inside the transport, the IPC router and
    // every session, each of which is itself a large state machine. A 64 KiB
    // array here cost megabytes of stack across the daemon.
    let mut body: Vec<u8> = Vec::new();
    while body.len() < len {
        let start = body.len();
        let want = (len - start).min(READ_CHUNK);
        body.resize(start + want, 0);
        r.read_exact(&mut body[start..]).await?;
    }
    Ok(postcard::from_bytes(&body)?)
}

/// How much of a frame body is claimed at a time (S-M3).
///
/// Big enough that an ordinary frame — a sync batch, a manifest, a log
/// window — is one or two reads, and small enough that a peer which
/// declares four megabytes and sends nothing has cost 64 KB.
const READ_CHUNK: usize = 64 * 1024;

/// Negotiate protocol compatibility. Returns the common minor on success.
pub fn negotiate(local: (u16, u16), remote: (u16, u16)) -> Result<u16, String> {
    if local.0 != remote.0 {
        return Err(format!(
            "protocol major mismatch: local {}.{}, remote {}.{}",
            local.0, local.1, remote.0, remote.1
        ));
    }
    Ok(local.1.min(remote.1))
}

pub fn now_micros() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S-M3: a frame's declared length is a claim, not a reservation.
    ///
    /// `read_frame` used to `vec![0; len]` before a byte of body arrived, so
    /// a peer that opened quinn's hundred streams and wrote only the
    /// four-byte prefix on each held 400 MB having sent 400 bytes. The
    /// buffer now grows with what is actually delivered — and grows on the
    /// heap, because this is an `async fn` and a stack array here lives in
    /// every future that awaits it.
    #[tokio::test]
    async fn a_frame_body_is_read_rather_than_reserved() {
        // A declared 4 MiB with nothing behind it ends as an error at the
        // first short read, not as four megabytes of zeroes.
        let mut starved: &[u8] = &MAX_FRAME_LEN.to_le_bytes();
        let refused: Result<Ping, _> = read_frame(&mut starved).await;
        assert!(refused.is_err(), "a frame with no body cannot decode");

        // Over the cap is still refused on the length alone.
        let mut huge: &[u8] = &(MAX_FRAME_LEN + 1).to_le_bytes();
        let refused: Result<Ping, _> = read_frame(&mut huge).await;
        assert!(matches!(refused, Err(ProtoError::FrameTooLarge(_))));

        // And a real frame still round-trips, including one larger than the
        // read chunk so the growing path is the one under test.
        let big = vec![7u8; READ_CHUNK * 2 + 11];
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &big).await.expect("write");
        let mut cursor: &[u8] = &buf;
        let back: Vec<u8> = read_frame(&mut cursor).await.expect("read");
        assert_eq!(back, big);
    }

    #[test]
    fn node_id_roundtrip() {
        let id = NodeId([7u8; 32]);
        let s = id.to_display_string();
        assert!(s.starts_with("mw1"));
        let back = NodeId::parse(&s).expect("parse own display form");
        assert_eq!(id, back);
    }

    #[test]
    fn job_id_roundtrip_and_ordering() {
        let a = JobId::generate();
        let b = JobId::generate();
        let s = a.to_display_string();
        assert!(s.starts_with("job-"));
        assert_eq!(JobId::parse(&s).expect("parse"), a);
        assert_ne!(a, b);
    }

    #[test]
    fn relay_candidate_roundtrip() {
        let id = NodeId([9u8; 32]);
        let c = relay_candidate(&id);
        assert!(c.starts_with("relay:mw1"));
        assert_eq!(parse_relay_candidate(&c), Some(id));
        assert_eq!(parse_relay_candidate("10.0.0.1:7777"), None);
        assert_eq!(parse_relay_candidate("relay:garbage"), None);
        // A 1.4 resolver sees `relay:<id>`: "host:port" split gives a
        // non-numeric port → unresolvable → skipped, never a panic.
        assert!(
            c.rsplit_once(':')
                .is_some_and(|(_, p)| p.parse::<u16>().is_err())
        );
    }

    #[test]
    fn wan_requests_gate_on_minor() {
        assert_eq!(Request::Observe.min_minor(), WAN_MINOR);
        assert_eq!(Request::RelayRegister.min_minor(), WAN_MINOR);
        assert_eq!(
            Request::Ping(Ping {
                nonce: 1,
                sent_at_micros: 0
            })
            .min_minor(),
            0
        );
    }

    #[test]
    fn gang_requests_gate_on_the_cluster_minor() {
        // The safety gate of C4: a node below this minor cannot hold a
        // lease, so it must never be sent one of these. A regression here
        // would let a gang believe a reservation existed on a node that has
        // no code for reservations — and then start N-1 of N members.
        let gang = GangId([7u8; 16]);
        let ticket = GangTicket::new(JobPriority::Normal, Hlc::default(), gang);
        assert_eq!(
            Request::GangOffer {
                gang,
                ticket,
                reqs: Requirements::default(),
                members: 1,
                ttl_secs: 10,
            }
            .min_minor(),
            CLUSTER_MINOR
        );
        assert_eq!(Request::GangCommit { gang }.min_minor(), CLUSTER_MINOR);
        assert_eq!(Request::GangRelease { gang }.min_minor(), CLUSTER_MINOR);
        // …and the mesh advertises a minor that can actually serve them.
        const { assert!(PROTOCOL_MINOR >= CLUSTER_MINOR) };
    }

    #[test]
    fn cache_requests_gate_on_the_cache_minor() {
        // Milder than the gang gate: a peer below this minor is skipped in
        // the directory ranking rather than refused, so the failure mode of
        // getting this wrong is a decode error on an old node rather than a
        // half-started gang. It is still a gate, and it is still asserted.
        let key = ActionKey([3u8; 32]);
        assert_eq!(Request::CacheLookup { key }.min_minor(), CACHE_MINOR);
        let entry = cache::CacheEntry {
            key,
            exit_code: Some(0),
            stdout: None,
            stdout_len: 0,
            stderr: None,
            stderr_len: 0,
            outputs: Vec::new(),
            producer: NodeId([1u8; 32]),
            job: JobId([2u8; 16]),
            epoch: 0,
            hlc: Hlc::default(),
            at_micros: 0,
        };
        assert_eq!(
            Request::CachePublish {
                entry: Box::new(entry)
            }
            .min_minor(),
            CACHE_MINOR
        );
        const { assert!(PROTOCOL_MINOR >= CACHE_MINOR) };
        // The two 1.1x feature constants coexist rather than supersede each
        // other, which is the coexistence rule §14.1 exists to preserve.
        const { assert!(CACHE_MINOR > ACCEL_MINOR) };
    }

    /// T3's gate is the one that leaves a door **open**, and the shape of
    /// that is asserted here rather than only described above: a node below
    /// it announces no `cap.rt`, which reads as *unknown*, which stays a
    /// candidate. Get this backwards and upgrading one node shrinks the
    /// mesh.
    #[test]
    fn the_runtime_gate_leaves_an_old_node_able() {
        const { assert!(PROTOCOL_MINOR >= RUNTIME_MINOR) };
        // Coexists with the shed minor rather than superseding it.
        const { assert!(RUNTIME_MINOR > SHED_MINOR) };

        let old: [String; 1] = ["cap.cores=8".into()];
        assert_eq!(runtimes::from_tags(&old), None, "no tag at all");
        for r in runtimes::ALL {
            assert!(
                runtimes::tags_can_run(&old, r),
                "a pre-{RUNTIME_MINOR} node must stay a candidate for {}",
                r.as_str()
            );
        }
    }

    #[test]
    fn negotiate_rules() {
        assert_eq!(negotiate((1, 3), (1, 1)), Ok(1));
        assert_eq!(negotiate((1, 0), (1, 9)), Ok(0));
        assert!(negotiate((1, 0), (2, 0)).is_err());
    }

    #[test]
    fn hlc_ordering() {
        let a = Hlc {
            wall_micros: 10,
            logical: 5,
        };
        let b = Hlc {
            wall_micros: 10,
            logical: 6,
        };
        let c = Hlc {
            wall_micros: 11,
            logical: 0,
        };
        assert!(a < b && b < c);
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        let msg = Request::Ping(Ping {
            nonce: 42,
            sent_at_micros: 1234,
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.expect("write");
        let mut cursor = std::io::Cursor::new(buf);
        let back: Request = read_frame(&mut cursor).await.expect("read");
        match back {
            Request::Ping(p) => assert_eq!(p.nonce, 42),
            other => panic!("wrong message: {other:?}"),
        }
    }
}
