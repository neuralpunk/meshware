//! Local IPC schema (CLI / Director <-> daemon). Same framed postcard codec
//! as the wire. Every request travels inside a [`SignedIpc`] envelope,
//! signed by the caller's *user* key (MASTERPLAN §7.1: no passwords, no
//! bearer tokens). Most requests get exactly one response frame; `Run`,
//! `RunService` and `JobLogs` stream frames and terminate with `JobDone`.

use serde::{Deserialize, Serialize};

use crate::content::{Runtime, ServiceVersion};
use crate::facts::{FactCollector, FactCollectorRow, FactDiffRow, FactRow};
use crate::history::{HistoryJobPage, HistoryQuery, HistoryStats};
use crate::objects::{Capture, ObjectRef, ObjectRow};
use crate::oidc::OidcProvider;
use crate::ops::NodeQuarantine;
use crate::ops::{FanoutInfo, GitSource, HaltPolicy, JobPriority};
use crate::parallel::NodeProgress;
use crate::rbac::{
    AuditEntry, Grant, GrantRevocation, NodeRevocation, ServiceAccount, UserRecord, UserRevocation,
};
use crate::results::{ResultRow, ResultSet};
use crate::secrets::{SecretRecord, SecretRemoval, SecretScope};
// `RunState` and `StepState` outlive the workflow engine: the run stream
use crate::workflow::{RunState, StepState};
use crate::{
    BundleId, FanId, GrantId, InviteId, InvitePublic, JobId, JobState, Liveness, MeshId, NodeId,
    ObjectId, RunId, UserId,
};

/// A user-signed request envelope. The signature covers method + payload +
/// timestamp + nonce, so requests cannot be forged or replayed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignedIpc {
    pub user: UserId,
    pub at_micros: u64,
    pub nonce: u64,
    /// The postcard-encoded [`IpcRequest`].
    pub payload: Vec<u8>,
    /// Ed25519 signature by `user` over [`ipc_signing_bytes`].
    pub signature: Vec<u8>,
}

/// [`IpcRequest::SessionOpen`]'s postcard discriminant.
///
/// The IPC accept loop has to know whether an envelope opens a *session* —
/// which changes the framing of the whole connection — before it does
/// anything else. It answered that by deserialising the entire 186-variant
/// `IpcRequest`, and then the real dispatch deserialised it again, so every
/// payload was decoded twice; for the bulk variants (`data: Vec<u8>`, a
/// large `Submit` script) that doubled peak allocation, **before the
/// signature had been checked** (**M-16**).
///
/// A leading-varint peek answers the same question without allocating, the
/// way [`crate::op_body_index`] already does for ops. Pinned by a test
/// beside the enum.
/// Renumbered 124 → 95 when the workflow, schedule and drafting-agent
/// requests were removed — the last time any variant here moves. The
/// a client encoding requests by position from outside this build would see a
/// shift is a different request that decodes cleanly; `ipc-order/` pins
/// the order and the `ipc_order` tests hold it (DOC-8).
pub const IPCREQ_SESSION_OPEN_INDEX: u32 = 95;

/// The variant index of an encoded [`IpcRequest`], without decoding it.
///
/// postcard writes an enum's discriminant as a leading LEB128 varint, so
/// this reads at most five bytes. `None` for a truncated or malformed
/// prefix — which the caller should treat as "not the variant I asked
/// about" and let the real decode produce the error.
pub fn ipc_request_index(payload: &[u8]) -> Option<u32> {
    crate::ops::op_body_index(payload)
}

/// Every key meshware itself reads or writes in a job environment starts
/// with this. The origin adds and removes such keys after the user signed
/// (a cache key, an expanded inventory, an acting-for claim), so they are
/// the one part of a job's environment an executor does not compare with
/// the envelope (SEC-4).
pub const RESERVED_ENV_PREFIX: &str = "MESHWARE_";

/// Is `key` one of meshware's own?
pub fn is_reserved_env(key: &str) -> bool {
    key.starts_with(RESERVED_ENV_PREFIX)
}

/// Does `key` change what a process loads or runs before its own first
/// line? (SEC-4)
///
/// Refused in a job's environment whoever signed it. A person who wants a
/// preloaded library can say so in the command, where it is visible in
/// every preview and every audit row; an environment variable is where it
/// hides. `LD_LIBRARY_PATH` and the interpreter paths are deliberately not
/// here: they are ordinary configuration, and the signature check is what
/// keeps anyone but the user from setting them.
pub fn is_loader_env(key: &str) -> bool {
    matches!(
        key,
        "LD_PRELOAD"
            | "LD_AUDIT"
            | "LD_PROFILE"
            | "GLIBC_TUNABLES"
            | "DYLD_INSERT_LIBRARIES"
            | "BASH_ENV"
            | "SHELLOPTS"
            | "BASHOPTS"
    ) || key.starts_with("BASH_FUNC_")
}

/// Is `job_env` the environment `signed_env` asked for, give or take
/// meshware's own keys? Order does not matter; every other pair, and how
/// many times it appears, does (SEC-4).
pub fn env_matches_signed(job_env: &[(String, String)], signed_env: &[(String, String)]) -> bool {
    let theirs = |env: &[(String, String)]| {
        let mut pairs: Vec<(String, String)> = env
            .iter()
            .filter(|(k, _)| !is_reserved_env(k))
            .cloned()
            .collect();
        pairs.sort();
        pairs
    };
    theirs(job_env) == theirs(signed_env)
}

/// Who a signed request is for (SEC-12): one mesh, one node.
///
/// A `mw-ipc-v1` signature names neither, so an envelope captured on its
/// way to one node was good at every node of every mesh that knew the
/// signer for the rest of its five-minute window — and the HTTP API is
/// plain HTTP. A `mw-ipc-v2` signature covers both. `SignedIpc` does not
/// change shape: the audience is not carried, the verifier supplies the
/// ones it would accept and the signature says whether one of them is it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Audience {
    pub mesh: MeshId,
    pub node: NodeId,
}

/// The `mw-ipc-v2` preimage: [`ipc_signing_bytes`], bound to `audience`.
pub fn ipc_signing_bytes_for(
    audience: &Audience,
    user: &UserId,
    at_micros: u64,
    nonce: u64,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(112 + payload.len());
    buf.extend_from_slice(b"mw-ipc-v2");
    buf.extend_from_slice(&audience.mesh.0);
    buf.extend_from_slice(&audience.node.0);
    buf.extend_from_slice(&user.0);
    buf.extend_from_slice(&at_micros.to_le_bytes());
    buf.extend_from_slice(&nonce.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub fn ipc_signing_bytes(user: &UserId, at_micros: u64, nonce: u64, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + payload.len());
    buf.extend_from_slice(b"mw-ipc-v1");
    buf.extend_from_slice(&user.0);
    buf.extend_from_slice(&at_micros.to_le_bytes());
    buf.extend_from_slice(&nonce.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    Status,
    IdentityShow,
    /// Who does the daemon think I am, and what may I do?
    WhoAmI,
    /// Probe a raw address: handshake with whatever answers there and
    /// report who it turned out to be. For an endpoint that is not (yet)
    /// a member — a joiner, a host somebody typed in.
    PingPeer {
        addr: String,
    },
    /// Bootstrap info for minting an invite token client-side (the token is
    /// signed by the admin's user key, which only the client holds).
    InviteBootstrap,
    /// Publish the public record of a client-built invite.
    InvitePublish {
        public: InvitePublic,
    },
    Nodes,
    NodeRevoke {
        revocation: NodeRevocation,
    },
    // -- users / grants ----------------------------------------------------
    UserAdd {
        record: UserRecord,
    },
    Users,
    UserRevoke {
        revocation: UserRevocation,
    },
    GrantAdd {
        grant: Grant,
    },
    Grants,
    // -- machine principals (E1-P4) ----------------------------------------
    /// Publish the marker that makes an already-added user a **machine**.
    ///
    /// Deliberately its own request rather than a field on `UserAdd`, and
    /// deliberately not a compound "create everything" call: a service
    /// account is a `UserAdd`, this, and a `GrantAdd`, each independently
    /// signed and each landing in the audit log as the separate thing it
    /// is. A compound request would need its own copy of three
    /// authorization checks and would report as one audit row for three
    /// facts.
    ServiceAccountAdd {
        account: ServiceAccount,
    },
    /// Every machine principal, with the clock each one is on.
    ServiceAccounts,
    // -- identity-provider bridge (E1-P5) ----------------------------------
    /// Configure an identity provider this mesh will believe (admin).
    OidcProviderPut {
        provider: OidcProvider,
    },
    /// Stop believing one (admin).
    OidcProviderRemove {
        name: String,
        by: UserId,
        at_micros: u64,
        signature: Vec<u8>,
    },
    OidcProviders,
    // -- quarantine (E2.4) -------------------------------------------------
    /// Isolate a suspected node, or lift the isolation (admin).
    NodeQuarantine {
        quarantine: NodeQuarantine,
    },
    // -- key rotation (E2.3) -----------------------------------------------
    /// Bind a **second** key to an existing username and move that
    /// person's grants onto it, then revoke the old one.
    ///
    /// One request rather than four, because a rotation half-done is worse
    /// than not started: a person whose new key is bound but ungranted
    /// cannot work, and one whose old key is revoked before the new one is
    /// granted is locked out. The daemon does the four ops in an order
    /// that leaves the person able to act at every point.
    UserRotateKey {
        /// The username being rotated. Must exist and not be revoked.
        username: String,
        /// The record binding the new key, admin-signed.
        record: Box<UserRecord>,
        /// The grants to carry over, re-signed for the new key by the
        /// admin doing the rotation.
        grants: Vec<Grant>,
        /// The revocation of the old key, admin-signed.
        revocation: Box<UserRevocation>,
    },
    /// Run the whole device-code login on the daemon's behalf and verify
    /// the resulting token, publishing an assertion for `enrol` to carry.
    ///
    /// **The daemon does the network round trip, not the CLI**, for the
    /// same reason it does every other one: the node is the thing with the
    /// verified provider record, the key cache and the clock, and a client
    /// that fetched its own token would be asking the mesh to believe a
    /// string it was handed. The CLI's job is to print a code and wait.
    OidcLoginBegin {
        provider: String,
    },
    /// Poll the login started by [`IpcRequest::OidcLoginBegin`].
    OidcLoginPoll {
        provider: String,
        device_code: String,
        interval: u64,
    },
    GrantRevoke {
        revocation: GrantRevocation,
    },
    // -- secrets -------------------------------------------------------------
    /// Publish a client-sealed secret record (the daemon never sees the
    /// plaintext).
    SecretPublish {
        record: SecretRecord,
    },
    Secrets,
    SecretRemove {
        removal: SecretRemoval,
    },
    SecretProviders,
    // -- audit ---------------------------------------------------------------
    AuditQuery {
        /// Username or `mwu1…` id (prefix match on the id).
        user: Option<String>,
        /// Action prefix, e.g. `job.` or `secret.resolve`.
        action: Option<String>,
        since_micros: Option<u64>,
        limit: u32,
    },
    Run {
        command: String,
        on: Option<String>,
        env: Vec<(String, String)>,
        timeout_secs: Option<u64>,
        /// Fan out to matching nodes (MASTERPLAN §9.3): `None` = a single
        /// scheduler-picked target; `Some(0)` = every matching node;
        /// `Some(n)` = the n best-scored. Streamed responses switch to the
        /// Fanout*/FanChunk framing when this is set.
        fanout: Option<u32>,
        /// Queue priority on the executor (MASTERPLAN §10.2).
        priority: JobPriority,
        /// Fan-out only: at most this many sub-jobs in flight at once.
        max_parallel: Option<u32>,
        /// Fan-out only: stop dispatching once failures reach this threshold.
        halt_on_fail: Option<HaltPolicy>,
    },
    Jobs,
    JobShow {
        job_id: JobId,
    },
    JobCancel {
        job_id: JobId,
    },
    /// Streamed response: zero or more `LogChunk`, then `JobDone`.
    JobLogs {
        job_id: JobId,
        follow: bool,
    },
    /// Ingest a local directory into the content store as a bundle. The
    /// daemon reads the path from its own filesystem (same host), so it is
    /// served over the local socket only, never the node's own directory,
    /// and — for a job credential — only what that job could read (SEC-3).
    Push {
        path: String,
    },
    /// List bundles known to the mesh.
    Bundles,
    /// Register a client-built, client-signed service version. The daemon
    /// validates (bundle known, contract compiles, version is next) and
    /// authors the ops; `head_signature` moves the head to this version.
    ServiceCreate {
        version: Box<ServiceVersion>,
        head_signature: Vec<u8>,
    },
    Services,
    ServiceShow {
        name: String,
    },
    ServiceRollback {
        name: String,
        version: u32,
        by: UserId,
        signature: Vec<u8>,
    },
    ServiceRemove {
        name: String,
        by: UserId,
        signature: Vec<u8>,
    },
    /// Run a registered service, streaming output like `Run`.
    RunService {
        name: String,
        on: Option<String>,
        /// key=value inputs (validated against the service contract).
        inputs: Vec<(String, String)>,
        /// For ansible: render a mesh inventory from this selector.
        inventory: Option<String>,
        timeout_secs: Option<u64>,
        /// Fan out to matching nodes; see [`IpcRequest::Run::fanout`].
        fanout: Option<u32>,
        /// Queue priority on the executor (MASTERPLAN §10.2).
        priority: JobPriority,
        /// Fan-out only: at most this many sub-jobs in flight at once.
        max_parallel: Option<u32>,
        /// Fan-out only: stop dispatching once failures reach this threshold.
        halt_on_fail: Option<HaltPolicy>,
    },
    // -- LAN pairing (approver side; MASTERPLAN §5.2) -----------------------
    /// Dial a discovered candidate node and run the pairing exchange up to
    /// the code display. Streamed: `PairCode` (both humans now compare),
    /// then — after `PairDecide` — `PairDone` or `Error`. The single-use
    /// invite token rides along for provenance; it was built and signed
    /// client-side by the approving admin, like any invite.
    PairApprove {
        /// The candidate's pairing endpoint, from mDNS discovery.
        addr: String,
        /// Encoded single-use invite token (`mwj1…`).
        token: String,
    },
    /// The approving human's verdict on the displayed code.
    PairDecide {
        session: u64,
        accept: bool,
    },
    /// Update this node's own tags (MemberUpdate authored locally). Powers
    /// `meshnode nodes tag` and Director's contribute-compute toggle — the
    /// reserved `no-execute` tag is the capability the mesh sees (D6).
    TagsUpdate {
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Resolve a selector to the live placement candidates it would hit
    /// right now (Director's "this will hit N nodes" preview). Answered
    /// with `Nodes` rows.
    TargetPreview {
        selector: String,
    },
    // -- M5 scheduler visibility & Git sources ------------------------------
    /// Per-node load and queue depth (`meshnode top`, MASTERPLAN §10.2).
    Top,
    /// List registered Git sources.
    Sources,
    /// Register a client-signed Git source (MASTERPLAN §8.1).
    SourceAdd {
        source: GitSource,
    },
    /// Tombstone a Git source.
    SourceRemove {
        name: String,
        by: UserId,
        signature: Vec<u8>,
    },
    /// Sync a Git source right now on this node (manual trigger; the
    /// interval watcher is elected automatically).
    SourceSyncNow {
        name: String,
    },
    // -- M6 day-2 operations (MASTERPLAN §12) --------------------------------
    /// Gracefully drain this node: announce the `draining` state, stop
    /// accepting work, optionally re-dispatch still-queued jobs to other
    /// candidates, and finish what is running. Streamed: `DrainProgress`
    /// frames until idle, then `Ok` — after which the daemon exits cleanly.
    Drain {
        /// Re-dispatch queued (not-yet-running) jobs to other candidates
        /// before waiting (`meshnode drain --move`).
        move_pending: bool,
    },
    /// A drain with an end: refuse new work until `until_micros`, keep what
    /// is running, and rejoin on your own — a maintenance window rather
    /// than a shutdown (`meshnode drain --until 06:00`, `--for 2h`). The
    /// daemon does **not** exit. `None` lifts a timed drain early
    /// (`meshnode drain --lift`). Answered with `Notice`.
    DrainTimed {
        until_micros: Option<u64>,
        reason: String,
        /// Move queued jobs elsewhere first, as `Drain` does.
        move_pending: bool,
    },
    /// Write a logical backup (identity + genesis + full op-log) to `path`
    /// on the daemon's filesystem, mode 0600 (MASTERPLAN §4.5). Local socket
    /// only, never into the data directory, never for a job credential
    /// (SEC-3).
    Backup {
        path: String,
    },
    // -- bundle browsing (`meshnode bundles show|cat`, Director's file picker) --
    /// List the files in a bundle from its manifest. The manifest is fetched
    /// from a holder if this node does not have it yet (manifest only — no
    /// chunks), so any bundle known to the mesh can be browsed.
    BundleFiles {
        bundle_id: BundleId,
    },
    /// Read one regular file out of a bundle (the bundle is fetched if not
    /// local). Capped at [`BUNDLE_FILE_READ_MAX`] bytes — this is for
    /// contracts and scripts, not payloads.
    BundleFileRead {
        bundle_id: BundleId,
        path: String,
    },
    // -- parallel compute (MASTERPLAN M9 §5.4, §5.7) ------------------------
    /// Run `command` once per item across the mesh, streaming progress.
    /// This is `meshnode parallel`: the item list is expanded by the client
    /// (a file, a literal list, a range), the daemon owns the chunk queue,
    /// and workers pull chunks until the list is exhausted.
    ///
    /// Streams [`IpcResponse::ParallelStarted`], then `ParallelItem` per
    /// finished item interleaved with periodic `ParallelProgress`, and ends
    /// with `ParallelDone` — the established `Run`/`Drain` streaming shape.
    Parallel {
        /// Command template; `{}` is replaced by the item, GNU-parallel style.
        /// Ignored when `service` is set.
        command: String,
        /// A Library script to run once per item, instead of `command`.
        /// Everything Run can launch, this can launch per item.
        service: Option<String>,
        /// Contract inputs for `service`, validated on the origin exactly as
        /// `RunService` validates them.
        inputs: Vec<(String, String)>,
        /// Which input receives each item. `None` = append the item as a
        /// positional argument.
        item_input: Option<String>,
        /// The expanded input list.
        items: Vec<String>,
        /// Which nodes may take chunks; same selector idiom as `Run`.
        on: Option<String>,
        env: Vec<(String, String)>,
        /// Per-item timeout in seconds.
        timeout_secs: Option<u64>,
        /// Items per chunk. `None` = adaptive, targeting a few seconds of
        /// work per chunk (M9 §5.4) — the primary interface is never a raw
        /// chunk count.
        chunk: Option<u32>,
        /// Where each item's output goes (M9 §5.5). `Inline` is
        /// `meshnode parallel`; `Object` is `meshnode map`, and is what
        /// keeps intermediate data off the origin.
        capture: Capture,
        /// A merge command run once, after every item, over the map
        /// outputs (M9 §5.6, §6.3). Requires `capture: Object` — there is
        /// nothing to reduce over otherwise.
        reduce: Option<String>,
        /// Where the reduce runs. `None` = the node already holding the
        /// most output bytes, which is the pre-reduce §5.5 asks for: it
        /// moves the smallest amount of data.
        reduce_on: Option<String>,
    },
    // -- ephemeral objects (MASTERPLAN M9 §5.5) ------------------------------
    /// Every object this node holds (`meshnode objects`).
    Objects,
    /// Fetch one object's bytes, from wherever in the mesh it lives.
    /// Streams [`IpcResponse::ObjectChunk`] frames then `ObjectEnd`,
    /// because an object can be far larger than one IPC frame.
    ObjectGet {
        id: ObjectId,
        /// Nodes believed to hold it, from the item outcome that named it.
        /// Tried before the directory, so the common case costs no lookup.
        hints: Vec<NodeId>,
    },
    // -- local history index (M14 §2) ---------------------------------------
    //
    // These read `state/history.redb`, never the mesh. The answer is
    // whatever *this* node has seen, which is the honest answer a
    // decentralized system can give: there is no authority to ask for a
    // global one, and pretending otherwise would be the first step toward
    // needing one.
    // -- RETIRED 2026-09-20: the run index --------------------------------
    //
    // `meshnode history runs` searched workflow runs, and nothing has
    // recorded one since the workflow engine went in September — the ops
    // that fed it were deleted outright in protocol 2.0, so it could not
    // return a row even in principle. Payload-free tombstone, as change
    // control's are: `ipc_order` pins the list, and an older client that
    // still asks is told the feature is gone rather than handed a decode
    // error.
    /// Retired: the run index.
    HistoryRuns,
    /// Search indexed jobs, newest first.
    HistoryJobs(Box<HistoryQuery>),
    /// Counts and time bounds of what the local index holds.
    HistoryStatus,
    /// Entities this node is rate-limiting, and what it is holding back
    /// (M31 A3). A map read — cheap enough for `doctor` and a dashboard.
    Backpressure,
    /// Audit entries **with the signatures that make them verifiable**
    /// (M31 B4). What `meshware-archive ingest` pages to fill the archive's
    /// `audit` table, and what a WORM bucket ends up holding.
    ///
    /// Separate from [`IpcRequest::AuditQuery`], which answers *"what
    /// happened"* out of materialized state, because this one answers
    /// *"prove it"* and has to walk the op-log for the envelopes. Two
    /// questions, two costs, and merging them would make every `meshnode
    /// audit` pay for a scan it does not need.
    AuditRows {
        /// Exclusive: rows strictly newer than this. The ingest cursor.
        since_micros: u64,
        limit: u32,
    },
    /// One window of a job's output, read back from wherever a sink put it
    /// (M31 B3 §14.3).
    ///
    /// **The client names a job, never a sink and never a key.** The
    /// locator is looked up on the daemon from the job's own index row, so
    /// this cannot be pointed at an arbitrary row in somebody's warehouse —
    /// which is what taking a key from the caller would allow.
    ///
    /// Windowed from the end, because the reason anyone opens a six-month-
    /// old log is the thing that went wrong and it is at the bottom.
    HistoryReplay {
        job_id: JobId,
        /// Lines to skip back from the end. `0` is the tail, and the tail
        /// read is what counts as *opening* the payload — see the audit
        /// rule in the daemon's handler.
        from_end: u64,
        /// Lines wanted. Clamped by the daemon.
        limit: u64,
        /// Delegated to the destination where it has full text, capped and
        /// stated where it does not.
        search: String,
    },
    // -- RETIRED 2026-09-18: M11 change control ----------------------------
    //
    // Policies, approvals and touch-to-approve were removed. Each variant
    // stays as a payload-free name so that nothing after it renumbers
    // (`ipc_order`), and the daemon answers every one of them with a
    // refusal that says the feature is gone.
    /// Retired: change control.
    PolicyCreate,
    /// Retired: change control.
    Policies,
    /// Retired: change control.
    PolicyShow,
    /// Retired: change control.
    PolicyRemove,
    /// Retired: change control.
    PolicyCheck,
    /// Retired: change control.
    Approvals,
    /// Retired: change control.
    ApprovalShow,
    /// Retired: change control.
    ApprovalDecide,
    /// Retired: change control.
    ApprovalCancel,
    /// Resolve what a submission would do — targets, version, bundle, env
    /// keys, secret names, blast radius — dispatching nothing.
    ///
    /// A wrapper rather than a flag: wrapping the request the caller would
    /// otherwise have sent means the preview is of *that* submission, byte
    /// for byte, rather than of a second one assembled by preview-specific
    /// code. Accepts `Run`, `RunService` and `Parallel`.
    DryRun {
        request: Box<IpcRequest>,
    },
    /// Retired: change control. Was: run despite a policy refusal.
    ///
    /// **The payload stays**, unlike the other retired variants: a job's
    /// `JobSpec::auth` carries the request it was submitted with, and the
    /// executor re-decodes it — so a job submitted with `--break-glass`
    /// before the removal must still unwrap to the request inside. The
    /// daemon refuses a new one.
    BreakGlass {
        reason: String,
        request: Box<IpcRequest>,
    },
    // -- RETIRED 2026-09-18: M12 the host inventory ------------------------
    //
    // Payload-free names, for the reason the change-control block gives.
    // What is *not* free is adding a field to an existing variant — the
    // executor re-decodes the `IpcRequest` out of `JobSpec::auth` to
    // re-verify authorization, so a grown variant would break a mixed mesh.
    /// Retired: the host inventory.
    HostPut,
    /// Retired: the host inventory.
    HostImport,
    /// Retired: the host inventory.
    Hosts,
    /// Retired: the host inventory.
    HostShow,
    /// Retired: the host inventory.
    HostRemove,
    /// Retired: the host inventory.
    HostGroupPut,
    /// Retired: the host inventory.
    HostGroups,
    /// Retired: the host inventory.
    HostGroupRemove,
    /// Retired: the host inventory.
    HostCheck,
    /// Retired: the host inventory.
    HostInventory,
    /// Reach a **member**, by id. The daemon knows every address that node
    /// announced and which route currently reaches it, so it dials the way
    /// every other peer request does — candidates raced, relay included —
    /// instead of a frontend guessing one address out of the announced
    /// list. (That guess is wrong by default: the first entry is the
    /// peer's own loopback, which dials the caller's machine.)
    PingNode {
        node: NodeId,
    },
    // -- M18 facts (§4.9) ---------------------------------------------------
    /// What a subject is, per collector. Empty `subject` means this node.
    Facts {
        /// A node name, node id prefix, or host name. Empty = this node.
        subject: String,
        /// One collector, or every one that has published about it.
        collector: Option<String>,
    },
    /// Every subject whose facts satisfy a `fact.*`-style expression —
    /// `os_id=debian`, `kernel_release<6.1`, `gpu_model~=A100`.
    FactsQuery {
        /// The expression, with or without the `fact.` prefix.
        expression: String,
        /// One collector, or every one.
        collector: Option<String>,
    },
    /// What changed about a subject, and when (§4.8). Answered from this
    /// node's own history index.
    FactsDiff {
        subject: String,
        collector: Option<String>,
        since_micros: Option<u64>,
        limit: u32,
    },
    /// Publish a client-built, client-signed collector. Creating, editing,
    /// pausing and resuming all travel as this: a collector is edited by
    /// re-signing it, so there is no separate "set paused" verb that could
    /// change what a fleet reports without a signature behind it.
    FactCollectorPut {
        collector: Box<FactCollector>,
    },
    FactCollectors,
    /// Every fact **observation** this node's index holds, oldest first —
    /// not just the latest per subject.
    ///
    /// What the archive pass reads (§4.8). Distinct from `Facts`, which
    /// answers "what is this machine *now*"; five years of fleet change is
    /// a different question and it is the archive's, never the daemon's.
    FactsExport {
        since_micros: Option<u64>,
        limit: u32,
    },
    // -- M19 results (§5.3) -------------------------------------------------
    /// The rows of one fan-out, from this node's own index.
    ///
    /// Answered by whichever node owns or owned the group. Asked anywhere
    /// else it refuses **and names the owner** — the same honest answer
    /// `meshnode history` already gives, and for the same reason: there is
    /// no authority to ask for a global one, and pretending otherwise would
    /// be the first step toward needing one (M14, `ipc.rs` §421-427).
    Results {
        group: FanId,
        /// Only rows whose job did not succeed.
        failed_only: bool,
    },
    /// One parallel run as its owner recorded it — the request and every
    /// item's outcome — for `map --resume` and `results diff`. Node-local
    /// like `Results`: the owner has it and nobody else does.
    ParallelRecord {
        job_id: JobId,
    },
    /// Every result set this node owns, newest first — what the archive
    /// pass reads (§5.7).
    ResultsExport {
        since_micros: Option<u64>,
        limit: u32,
    },
    // -- M17 the fabric (§3.3) ----------------------------------------------
    //
    // A session has to solve the one-shot problem on *two* legs, not one
    // (R4): the wire leg is `Request::SessionOpen`, and this is the local
    // one. When the IPC loop decodes this variant it stops being a
    // request/response loop and hands the whole Unix stream to the session
    // machinery, which thereafter speaks [`crate::SessionFrame`] client→
    // daemon and [`crate::SessionServerFrame`] daemon→client — the same
    // two enums the wire leg carries, so the daemon is a splice and not a
    // translator.
    //
    // The envelope this request is signed into is the one forwarded to the
    // host node as `SessionSpec::auth`, unchanged: the host re-verifies the
    // user's own signature over the *user's own* statement of what they
    // asked for, and the origin daemon is not a party to it (§3.4).
    SessionOpen {
        /// The member to reach, by name or node id. Its own name opens the
        /// session locally, without a round trip.
        node: String,
        kind: crate::SessionKind,
        /// Why this terminal is being opened — a ticket, an incident, a
        /// sentence. Required, recorded in the audit entry, and covered by
        /// the signature, so it is the operator's own statement rather than
        /// something the origin node could invent for them.
        reason: String,
    },
    /// Live sessions this node is party to — hosted and initiated.
    Sessions,
    /// Close one live session by id, from either end.
    SessionClose {
        id: u64,
    },
    /// Publish a client-built, client-signed session policy (§3.4).
    SessionPolicyPut {
        policy: Box<crate::SessionPolicy>,
    },
    /// Every session policy this node holds.
    SessionPolicies,
    /// Retire one.
    SessionPolicyRemove {
        id: String,
        by: UserId,
        signature: Vec<u8>,
    },
    // -- M23 enrolment (§3.1, §3.2) -----------------------------------------
    //
    // The first three requests here are answered for a caller with **no
    // roles at all**, which is the whole point: today an unenrolled person
    // can sign requests and have every one of them denied, and that state
    // is indistinguishable from the product being broken. These are the
    // doors that state is allowed through.
    /// Put a client-built, client-signed request in the enrolment queue.
    ///
    /// Client-signed for the same reason a collector is: the daemon does
    /// not hold the user's key, and the signature is the only thing the
    /// request actually asserts. The daemon checks that the record names
    /// the key that signed the *envelope* — a caller may enrol themselves
    /// and nobody else.
    EnrolSubmit {
        request: Box<crate::EnrolmentRequest>,
    },
    /// Where the calling key stands: pending, declined, expired, enrolled.
    /// The blocking half of `meshnode enrol` polls this.
    EnrolStatus,
    /// The pending queue. Admin-only — a queue of who is asking to join is
    /// not something a viewer needs, and it is the shape of an org chart.
    Enrolments,
    /// Decline one request (admin-signed, client-signed like every other
    /// governance body).
    EnrolDecline {
        decline: Box<crate::EnrolmentDecline>,
    },
    // -- M24 definition distribution (§3.2, §3.1) ---------------------------
    /// Ask a set of nodes to fetch a bundle **now**, and stream what each
    /// one reports back. Streaming, like `Drain`.
    ///
    /// Definitions already replicate through the op-log; bundle *content*
    /// does not, and that lazy default is right for a 200 MB environment
    /// most of the fleet will never execute. This changes *when* each node
    /// pulls, and nothing else.
    BundlePrefetch {
        bundle_id: BundleId,
        /// Node selector (`any`, `tag=…`, `name=…`, `fact.…`).
        selector: String,
    },
    /// What this node's content store holds and what a reclaim would take
    /// back (M31 A2).
    ContentStatus,
    /// Reclaim the content store now, rather than waiting for the hourly
    /// sweep. `dry_run` measures and changes nothing.
    ContentPrune {
        dry_run: bool,
        /// Ignore the configured window and take everything unreferenced.
        /// The button an operator presses when the disk is full *now*.
        aggressive: bool,
    },
    /// Free space on every node the mesh can currently see (M31 A2).
    DiskReport,
    /// Everything this mesh distributes, what version it is on, and how
    /// many nodes have caught up (§3.1).
    Definitions,
    // -- C0 op-log compaction (C0-C9 §4) -----------------------------------
    /// What the replicated log costs on this node, and what a compaction
    /// pass would drop.
    StateShow,
    // -- C1 arbitration (C0-C9 §5) -----------------------------------------
    //
    // Quotas and the usage report were retired (2026-09-18); their verbs
    // stay as payload-free names so nothing renumbers. `Queue` is live.
    /// Retired: quotas and the usage report.
    QuotaPut,
    /// Retired: quotas and the usage report.
    QuotaRemove,
    /// Retired: quotas and the usage report.
    Quotas,
    /// Who is waiting on this node, behind whom, and why (C1 P4).
    Queue,
    /// Retired: quotas and the usage report.
    Usage,
    /// Retired: quotas and the usage report.
    UsageRows,
    /// Run a compaction pass on **this node's own** authored log now.
    /// `horizon` overrides `[state] compaction_horizon` for one pass.
    StateCompact {
        horizon_secs: Option<u64>,
        /// Report what would be dropped without dropping it.
        dry_run: bool,
    },
    /// Roll a signed release across a ring, one node at a time, with a
    /// health gate between each (§4). Streaming.
    ///
    /// **Orchestrated, never automatic**, and never the whole mesh at
    /// once. A mesh that upgrades itself is a mesh that can break itself at
    /// 3am with nobody in the loop, and the whole value proposition is that
    /// a person can point at a fleet and see what it is doing.
    ///
    /// `plan` is client-built and client-signed, like every other
    /// governance body: the daemon does not hold the user's key, and the
    /// signature is what each node re-verifies against its own state.
    /// Publishing the plan authorizes nothing on its own — a plan is inert
    /// until this request drives it.
    UpgradeFleet {
        plan: Box<crate::UpgradePlan>,
        /// Publish the plan as a **forced** one: nodes re-install even
        /// where the version already matches (`meshnode upgrade --force`).
        ///
        /// A sibling of the plan rather than a field of it, because that
        /// is what it is on the wire too — the flag is which op body
        /// carries the record, and a plan's own encoding is untouched. The
        /// caller must have signed
        /// [`UpgradePlan::signing_bytes_forced`][crate::UpgradePlan::signing_bytes_forced]
        /// with the same value, or every node drops the plan at merge.
        force: bool,
    },
    /// Withdraw a plan mid-rollout: nodes it has not reached stop being
    /// eligible.
    UpgradeCancel {
        plan_id: crate::UpgradeId,
        by: UserId,
        at_micros: u64,
        signature: Vec<u8>,
    },
    /// Live and recent upgrade plans, newest first.
    UpgradePlans,
    /// Publish a meshnode release into the mesh (§9). Client-built and
    /// client-signed; the bundle is pushed first, and this names it.
    ReleasePublish {
        release: Box<crate::ReleaseRecord>,
    },
    /// Trust a minisign public key for releases, mesh-wide (§9).
    /// Client-built and client-signed, like every governance body.
    ReleaseKeyAdd {
        key: Box<crate::ReleaseKey>,
    },
    /// Withdraw trust in a release key.
    ReleaseKeyRevoke {
        pubkey: String,
        by: UserId,
        at_micros: u64,
        signature: Vec<u8>,
    },
    /// Release keys this mesh trusts, and where this node's answer comes
    /// from.
    ReleaseKeys,
    /// Releases this mesh holds, newest version first.
    Releases,
    // -- C2 deployments (C0-C9 §6) -----------------------------------------
    /// Publish (or replace) a deployment. User-signed client-side, exactly
    /// as a schedule is: the daemon never holds the key that authorizes it.
    /// A pause, a rollout and a rollback are all this request with a
    /// re-signed record. `dry_run` reports what the record would do without
    /// publishing it.
    DeploymentPut {
        deployment: Box<crate::deployment::Deployment>,
        /// The rollout and partition policy that came out of the same file
        /// (N1 P3/P4). Two records on the wire, for the signature reason
        /// [`crate::DeploymentPolicy`] documents; one file to the person
        /// who wrote it, so they travel together from here.
        ///
        /// An inert policy authors no second op. Local IPC is same-build
        /// on both ends, so widening this variant costs nothing — the same
        /// note [`IPCREQ_SESSION_OPEN_INDEX`] makes about renumbering.
        policy: Box<crate::deployment::DeploymentPolicy>,
        dry_run: bool,
        /// The service this deployment names is being published by the
        /// same submission, from a `[deployment.container]` block.
        ///
        /// Only a `dry_run` needs to know: a real put publishes the service
        /// first, so by the time the deployment arrives the service is
        /// there. A rehearsal publishes nothing, and without this the check
        /// would report the very service it just said it would create as
        /// missing.
        inline_service: bool,
    },
    /// Every live deployment, with desired/running/healthy counts.
    Deployments,
    /// One deployment as a detail row, with its record as YAML.
    DeploymentShow {
        name: String,
    },
    /// Desired vs running vs healthy, the roll's progress, and the
    /// per-instance breakdown (§6.3).
    DeploymentStatus {
        name: String,
    },
    /// Withdraw a deployment. Its instances are cancelled by the reconciler.
    DeploymentRemove {
        name: String,
        by: UserId,
        signature: Vec<u8>,
    },
    // -- C4 gangs (C0-C9 §8.3) ---------------------------------------------
    /// Every gang the mesh knows about that has not closed, plus the most
    /// recent that have — reservations in flight, and who is senior to whom.
    /// Every accelerator in the mesh, or one node's (C5 P1).
    ///
    /// Answered from *replicated* state, so any node can answer for every
    /// node — which is the whole reason the inventory is an entity rather
    /// than a probe. The one field that is node-local is `held_by`: a
    /// device hold is not replicated (§8.2's rule, inherited), so it is
    /// filled in for the answering node and left `None` for the rest
    /// rather than guessed at.
    Devices {
        /// A node name or id prefix. `None` = the whole mesh.
        node: Option<String>,
    },
    Gangs {
        /// Include closed gangs. Off by default: the question this noun
        /// exists for is about what is holding resources *now*.
        all: bool,
    },
    /// One gang: its members, where they landed, and — when it is stuck —
    /// which gangs are ahead of it.
    GangShow {
        gang: crate::GangId,
    },
    /// The authorization a deployment instance's job carries, re-derived on
    /// the executor from the deployment record — never a request a client
    /// submits, exactly like `ScheduleFire`. Refused over the socket.
    DeploymentFire {
        deployment: String,
        generation: u32,
    },
    // -- C7 action cache (C0-C9 §11) ----------------------------------------
    /// Derive the action key for a submission and print every field that
    /// produced it, dispatching nothing (§11.3 P1).
    ///
    /// A wrapper around the request the caller would otherwise have sent,
    /// exactly like [`Self::DryRun`] and for exactly the same reason: the
    /// key explained must be the key of *that* submission, byte for byte,
    /// or the explanation is of a second, explain-shaped job that would
    /// never have run.
    ExplainKey {
        request: Box<IpcRequest>,
    },
    /// Hit rate, bytes saved and entry counts — for the mesh where the
    /// answering node can see it, and for this node's own shard where it
    /// cannot. Answered with [`IpcResponse::CacheStats`].
    CacheStats,
    /// Forget entries. Aimed at everything, one key, or one service.
    CachePurge {
        scope: crate::cache::PurgeScope,
    },
    // -- C8 legibility (C0-C9 §12) ------------------------------------------
    /// Why a job landed where it did, and what the cache had to say about
    /// it. Answered from the **local** index: the explanation is job-record
    /// data on the node that placed the job, which is why C8 costs no wire
    /// minor at all.
    JobExplain {
        job_id: crate::JobId,
    },
    /// The what-if planner: place this submission against the mesh as it is
    /// right now, without submitting it, and report when it would finish
    /// and whose work it would delay.
    ///
    /// Deliberately *not* `Run { dry_run }`: M11's dry run answers the
    /// governance question — targets, blast radius, verdict — and this one
    /// answers the capacity question. Folding them into one flag would give
    /// an operator one answer to two questions and no way to tell which one
    /// they got.
    Plan {
        /// Selector, exactly as `run --on` takes it.
        on: Option<String>,
        /// Service name, when planning a service.
        service: Option<String>,
        /// Job environment, carrying requirements and device requests the
        /// same way a submission does.
        env: Vec<(String, String)>,
        /// How many targets, for a fan-out. `None` = one.
        fanout: Option<u32>,
    },
    // -- M9 §6.3 -----------------------------------------------------------
    //
    // **Appended at the tail, and this is load-bearing.** A postcard enum is
    // a varint discriminant followed by the variant's fields, so inserting a
    // variant anywhere but the end renumbers every variant after it. That is
    // not merely a Python-SDK regeneration: an executor re-decodes the
    // submitter's `IpcRequest` out of the *signed* envelope to re-check
    // authorization, so a renumbered request means a 0.6 origin's job cannot
    // be read by a 0.5 executor — the mixed-version break M9 §5.3 and M12
    // §6.2 both take pains to avoid. New variants go here, at the bottom,
    // forever.
    /// One line for how much machine this mesh actually is
    /// (`meshnode totals`).
    ///
    /// Every number is already derivable from `Nodes` + `Top`, and that is
    /// precisely why this exists: the aggregate is the first question
    /// anybody asks about a mesh and the last one the surfaces answered,
    /// because answering it meant summing two tables by hand — and three
    /// surfaces were each about to sum them slightly differently.
    ///
    /// Answered with [`IpcResponse::Totals`].
    Totals,
    // -- RETIRED 2026-09-20: the mesh benchmark ---------------------------
    //
    // `meshnode bench` and Director's Benchmark screen were removed: the
    // score drove no operational decision that `top`, `/metrics`, `jobs
    // explain` and `plan` did not already answer, and it cost a crate.
    // The variant stays as a payload-free name so nothing after it
    // renumbers (`ipc_order`), and the daemon answers it with a refusal.
    /// Retired: the mesh benchmark.
    BenchRun,
    /// Forget every job that has already finished, mesh-wide.
    ///
    /// Appended rather than filed next to `JobCancel`, for the reason
    /// `OpBody` is append-only: a payload is postcard, a variant is its
    /// index, and a CLI one version behind the daemon must not have its
    /// requests read as some other verb.
    ///
    /// The caller signs the watermark it is asking for, the daemon
    /// authors [`crate::OpBody::JobsClear`] from it, and this node's own
    /// job records — specs and logs on disk — go with it. Answered with
    /// [`IpcResponse::Notice`].
    JobsClear {
        /// Wall clock the clear is issued at; jobs finished before it go.
        before_micros: u64,
        by: UserId,
        /// Signature by `by` over [`crate::jobs_clear_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Cancel a job even if its executor cannot be told (appended, like
    /// everything since F0).
    ///
    /// The daemon tries the ordinary route first — a reachable executor
    /// kills the process and reports its own terminal state, which is
    /// always the better record. Only when that route fails does it author
    /// [`crate::OpBody::JobCancelForce`] from the carried signature, so
    /// the summary converges to `cancelled` mesh-wide with nobody left to
    /// ask. Answered with [`IpcResponse::Notice`] saying which of the two
    /// happened.
    ///
    /// Gated in two steps, so one button can serve every role: the
    /// ordinary attempt is gated as an ordinary cancel (admins and
    /// operators any job, developers their own), and only the escalation —
    /// actually rewriting the record — demands operator or admin. A caller
    /// refused either step gets [`IpcResponse::Denied`] naming the rule
    /// and who can act.
    JobCancelForce {
        job_id: crate::JobId,
        by: UserId,
        /// Signature by `by` over [`crate::job_cancel_force_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Run a script carried in this request: the third way to say *what to
    /// run*, and the one the build farm needs (`crate::script`).
    ///
    /// [`Run`](Self::Run) takes a command string, [`RunService`](Self::RunService)
    /// names a published Library entry, and neither can carry a megabyte of
    /// preprocessed C. `JobPayload::Script` always could — a workflow step
    /// is one — it simply had no door outside a workflow.
    ///
    /// Streams exactly like `Run`: `JobStarted`, `LogChunk`s, an optional
    /// `Result` row, then `JobDone`. It is also **cacheable**, which is the
    /// whole point: an inline script's action key is the digest of its own
    /// source, so the same translation unit compiled anywhere in the mesh
    /// is one key and every repeat is a hit (C7 §11).
    ///
    /// Appended, and gated on [`crate::FARM_MINOR`]: an executor
    /// re-decodes this envelope to re-derive authority, and one that has
    /// never heard of the variant fails to parse rather than degrading.
    /// Placement filters on [`crate::script::SCRIPT_AUTH`] for that reason.
    RunScript {
        /// The script itself — runtime, source, requirements, inputs.
        script: Box<crate::content::InlineScript>,
        /// Target selector: `any` | `<name>` | `tag=<t>` | `id=<mw1…>`.
        on: Option<String>,
        /// Environment KEY=VALUE, including the reserved `MESHWARE_TASK_*`
        /// keys (`--cacheable`, resource requirements, capture keys).
        env: Vec<(String, String)>,
        timeout_secs: Option<u64>,
        priority: JobPriority,
    },
    /// Stream one regular file out of a bundle, in instalments (the bundle
    /// is fetched from holders first if this node does not hold it).
    ///
    /// The door [`BundleFileRead`](Self::BundleFileRead) cannot be: that
    /// one answers in a single frame, so it is bounded twice over — by
    /// [`BUNDLE_FILE_READ_MAX`] and, underneath it, by
    /// [`crate::MAX_FRAME_LEN`] — and a meshnode release binary is larger
    /// than both. This is for the payload case: the caller gets
    /// [`IpcResponse::BundleFileChunk`] frames in manifest order and then
    /// exactly one [`IpcResponse::BundleFileDone`], which is what makes a
    /// truncated stream tell itself apart from a short file.
    ///
    /// Appended at the tail, per the note on [`Totals`](Self::Totals).
    BundleFileFetch {
        bundle_id: BundleId,
        path: String,
    },
    /// Repair another member's recorded addresses (founder only).
    ///
    /// A node normally speaks only for itself: `MemberUpdate`'s merge rule
    /// admits an op about node N from N, or from the founder — and the
    /// founder exception exists for exactly one situation, which is this
    /// one. Every node re-announces its real address on every boot, so the
    /// mesh heals itself after any *single* machine changes address. It
    /// cannot heal after they *all* change at once: every recorded address
    /// is stale in every direction, nobody can make first contact, and the
    /// mesh is partitioned by arithmetic rather than by anything being
    /// down. Re-addressing a whole VLAN does this.
    ///
    /// One repaired record breaks the symmetry. The founder dials the node
    /// at the address the operator supplies, gossip syncs both ways, and
    /// the node's own boot-time announcement — which was always there,
    /// just unable to travel — reaches everyone. So this is a bootstrap
    /// nudge, not a source of truth: the subject overwrites it with its
    /// own account of itself as soon as the two can talk.
    ///
    /// Appended at the tail, per the note on [`Totals`](Self::Totals).
    NodeAddrRepair {
        node_id: NodeId,
        /// `host:port` entries, replacing what the mesh records. A
        /// hostname is resolved at dial time, so it survives the next
        /// renumbering; `relay:<id>` candidates are refused, because a
        /// relay route is something a node earns and announces, not
        /// something an operator asserts on its behalf.
        addrs: Vec<String>,
    },
    // -- M31 B1 sinks -------------------------------------------------------
    /// Create or replace a sink. Admin-signed client-side, exactly as a
    /// policy is: the node never holds the key that authorizes it.
    SinkPut {
        sink: Box<crate::sink::Sink>,
    },
    /// Retire one.
    SinkRemove {
        name: String,
        /// Signature by the caller over
        /// [`crate::sink::sink_remove_signing_bytes`].
        signature: Vec<u8>,
    },
    /// Every sink, with how far behind it is and what it last said.
    Sinks,
    /// POST one synthetic row to a destination and report what came back
    /// (M31 B2 item 8's Test button).
    ///
    /// **Takes the whole record rather than a name, so an *unsaved* form
    /// can be tested.** That is the requirement — an operator should learn
    /// about a wrong token in the dialog, not from a lag graph two hours
    /// later — and a name-keyed test could only run after the mistake had
    /// already been signed into the mesh.
    ///
    /// Nothing is authored: the record is used for its URL, its kind and
    /// its credential *reference*, and then discarded. It need not be
    /// signed, because it grants nothing.
    ///
    /// Runs on the node that receives the request rather than on the
    /// elected exporter, and that is deliberate: the operator is asking
    /// *"does this work"*, and the honest answer includes whether **this**
    /// machine can reach it. A test that silently succeeded from somewhere
    /// else would be the wrong reassurance.
    SinkTest {
        sink: Box<crate::sink::Sink>,
    },
    /// Release the restore latch on this node (§19 Q5): its data came out
    /// of a backup, so nothing has been exporting.
    ///
    /// Node-local by design — it is a statement about *this* machine's
    /// data, not a mesh-wide decision — so it is IPC rather than an op.
    SinksResume,
    // -- M31 A1 retention ---------------------------------------------------
    /// Publish (or replace) the mesh retention policy. Admin-signed
    /// client-side, exactly as a policy is: the node never holds the key
    /// that authorizes it.
    RetentionPut {
        policy: Box<crate::retention::RetentionPolicy>,
    },
    /// Withdraw it, returning the mesh to the shipped defaults.
    RetentionReset {
        at_micros: u64,
        /// Signature by the caller over
        /// [`crate::retention::retention_reset_signing_bytes`].
        signature: Vec<u8>,
    },
    /// The current policy, who signed it, and what it settles the log at.
    RetentionShow,
    // -- M31 P0 measurement -------------------------------------------------
    /// What the op-log is actually made of, by entity, and how fast it is
    /// growing.
    ///
    /// Separate from [`IpcRequest::StateShow`] rather than folded into it,
    /// because the two have different costs: `StateShow` walks this node's
    /// own authored ops, and this walks every op in the log. `state compact`
    /// asks for the first one on every pass and must not start paying for
    /// the second.
    ///
    /// Appended at the tail, per the note on [`IpcResponse::Totals`].
    OpCensus,
    // -- a second key for a person who already exists ----------------------
    /// Bind `record.user_id` to a username that is *already held* — the
    /// same person at a new machine, whose Director generated a new key.
    ///
    /// [`IpcRequest::UserAdd`] refuses a taken username, and should: two
    /// strangers must not share a name. This is the deliberate exception,
    /// and it is its own verb so the refusal in `UserAdd` stays absolute.
    /// The state model has always allowed it (a `UserAdd` op is per key,
    /// and the merge path never checked names); only the gate did not.
    /// Grants are per key too — the caller copies the ones the existing
    /// key holds, so "attach" means "the same person, the same access".
    ///
    /// Appended at the tail, per the note on [`IpcResponse::Totals`].
    UserAttachKey {
        record: UserRecord,
    },
    /// The owner's decision about whether a node may be given work
    /// (`OpBody::NodeExecutionSet`). Signed client-side by an admin, as
    /// every other authority-bearing record is.
    ///
    /// **Appended at the tail, and it has to be.** postcard encodes a
    /// variant by its index, and this enum is not private to two Rust
    /// binaries, and several indices in this file are peeked rather than
    /// decoded. Inserting in the middle renumbers
    /// everything after it, and the symptom is not a compile error but a
    /// live SDK sending `Push` and being told "Found a bool that wasn't
    /// 0 or 1".
    NodeContributeSet {
        decision: Box<crate::NodeExecution>,
    },
    /// The live job table, **filtered, counted and paged** (**H-1**).
    ///
    /// The bounded replacement for [`Jobs`](Self::Jobs), which has no
    /// arguments and therefore no ceiling: it builds a row for every job
    /// the mesh remembers — a `stat()` each, under the state read lock —
    /// and stops working entirely once the answer outgrows
    /// [`crate::MAX_FRAME_LEN`]. `Jobs` is kept because older
    /// clients send it, and it is now served as one page of this.
    ///
    /// Answered with [`IpcResponse::JobsPage`].
    ///
    /// Appended at the tail, per the note on
    /// [`NodeContributeSet`](Self::NodeContributeSet).
    JobsQuery {
        query: Box<JobsQuery>,
    },
    /// *"Has anything happened?"* — the question the protocol could not be
    /// asked (**H-8**).
    ///
    /// Answered with [`IpcResponse::StateVersions`]: one counter per
    /// [`crate::StateTopic`], bumped when an op of that topic applies.
    /// Cheap by construction — it builds no collection and takes no state
    /// lock — which is the whole point: a client can afford to ask this
    /// every two seconds precisely because it cannot afford to ask for
    /// every table.
    ///
    /// Gated as a plain read, like `Status`: it says how *many* things
    /// have changed, never what.
    ///
    /// Appended at the tail, per the note on
    /// [`NodeContributeSet`](Self::NodeContributeSet).
    StateVersion,
    // -- assigned tags -----------------------------------------------------
    // **Appended, like every variant since the index became load-bearing.**
    // postcard writes an enum's discriminant as its declaration position,
    // and `IPCREQ_SESSION_OPEN_INDEX` is a constant read off the wire
    // before anything is decoded — so a variant inserted in the middle of
    // this enum silently renumbers `SessionOpen` and every request after
    // it. New ones go here.
    /// Assign tags to **another** node, in the mesh
    /// (`OpBody::NodeTagsSet`). Signed client-side by an admin, like every
    /// other standing decision about a node.
    ///
    /// The assignment is total — it replaces whatever that node was
    /// assigned before, and an empty list clears it — so a caller doing
    /// add/remove reads `AssignedTags` first and sends the list it wants.
    /// That read answers with the assigned half *alone*, which is what a
    /// caller needs to compute the new list without having to subtract the
    /// node's own announcements by hand.
    NodeTags {
        assignment: crate::ops::NodeTags,
    },
    /// The tags an admin has assigned to one node — the assigned half
    /// alone, before it is unioned with what the node announces.
    AssignedTags {
        node_id: NodeId,
    },
    // -- RETIRED 2026-09-18: Y5 hardware presence (change control) ---------
    /// Retired: change control.
    PolicyPresencePut,
    /// Retired: change control.
    PolicyPresences,
    /// Retired: change control.
    PresenceReadiness,
    /// Retired: change control.
    UserTokenAdd,
    /// Retired: change control.
    UserTokens,
    /// Retired: change control.
    UserTokenRevoke,
    // -- appended at the tail (R3 P3) ---------------------------------------
    //
    // postcard encodes a variant by index and the IPC accept loop *peeks*
    // one of those indices without decoding (M-16), so a variant inserted
    // in the middle silently renumbers every one after it. This is the
    // same positional rule the peer wire follows: new things go last.
    /// "I have the results of this run; stop holding them for me."
    ///
    /// A finished run's output stops being on the fifteen-minute clock
    /// until it is collected. This is how it is told, by whichever surface
    /// did the collecting — `map --out` on the CLI, or Director's save.
    /// Idempotent: a run whose results were already released, or that never
    /// retained any, answers `Ok`.
    ResultsCollected {
        job: JobId,
    },
    // -- RETIRED 2026-09-18: Y5, the assertion's ride (change control) -----
    /// Retired: change control.
    ApprovalDecidePresent,
    // -- appended at the tail (Y2/Y3, hardware identity) --------------------
    /// Trust a certificate authority to attest hardware keys. Admin-signed.
    AttestRootAdd {
        root: Box<crate::attest::AttestRoot>,
    },
    /// Stop trusting one, by id.
    AttestRootRemove {
        id: String,
        by: UserId,
        at_micros: u64,
        signature: Vec<u8>,
    },
    /// Every authority this mesh trusts, and how many keys each vouches for.
    AttestRoots,
    /// "My key is on hardware, and here is the chain." Self-signed.
    UserHardwareAttest {
        attestation: Box<crate::attest::HardwareAttestation>,
    },
    /// Withdraw one. Signed by the owner or an admin.
    UserHardwareRevoke {
        revocation: Box<crate::attest::AttestRevocation>,
    },
    /// Publish a hardware co-signature over a release binary (Y6).
    ReleaseCosign {
        cosign: Box<crate::fleet::ReleaseCosignature>,
    },
    // -- appended at the tail (N1 P5, cron standalone) ----------------------
    //
    // The tail, for the reason every appended thing in this file gives, and
    // with one extra consequence worth naming: appending keeps
    // [`IPCREQ_SESSION_OPEN_INDEX`] where it is. Inserting anywhere before
    // it would shift a constant the accept loop reads off the wire before
    // decoding, and the pinned order with it.
    /// Publish (or replace) a schedule that fires a service. Client-signed.
    SchedulePut {
        schedule: Box<crate::schedule::ServiceSchedule>,
        /// Print what it would do without publishing anything.
        dry_run: bool,
    },
    /// Every live schedule, with its next and last occurrence.
    Schedules,
    /// One schedule as a detail row, with its record as TOML.
    ScheduleShow {
        name: String,
    },
    /// Retire one. Signed by the owner or an admin.
    ScheduleRemove {
        name: String,
        by: UserId,
        signature: Vec<u8>,
    },
    /// Every registered service, with healthy-of-total (N3 P1).
    ///
    /// Answered from local replicated state on whichever node is asked —
    /// every node holds the whole catalogue, so there is no round trip and
    /// no "ask the leader".
    Registry,
    /// One service's instances, with where each is and whether discovery
    /// would hand it out.
    RegistryShow {
        service: String,
    },
    /// What the DNS responder on this node is doing (N3 P3).
    DnsStatus,
    /// This node's volumes, and who is holding them (N5).
    ///
    /// Node-local by construction: the claim *is* the node's own lease
    /// table, and asking another node would be asking something that
    /// cannot know.
    Volumes {
        node: Option<String>,
    },
    /// One node's port ledger: its range, what it withholds, and what is
    /// held right now (N2 P1).
    ///
    /// Answered by the node asked, because that is the only node that
    /// knows: a dynamic port is decided at spawn and does not replicate
    /// until N3's registry exists.
    Ports {
        /// Which node, by name. Absent = the one this CLI talks to.
        node: Option<String>,
    },
    /// The authorization a scheduled job carries, re-derived on the
    /// executor from the schedule record — never a request a client
    /// submits, exactly like [`Self::DeploymentFire`]. Refused over the
    /// socket.
    ScheduleFire {
        schedule: String,
        /// The instant the expression **named**. Checked against the
        /// record's own trigger on the executor, so a job derived from an
        /// instant the schedule does not yield is refused however it was
        /// signed.
        occurrence_micros: u64,
    },
    // -- N5 P2 dynamic volumes, appended at the tail ------------------------
    //
    // At the tail because a variant's position *is* its encoding: inserting
    // beside `Volumes`, where they belong by subject, would renumber every
    // variant after it and make a new CLI and an old daemon disagree about
    // what was asked.
    /// Ask this node's volume plugin to provision a volume, and offer it
    /// from the moment it exists (N5 P2).
    VolumeCreate {
        name: String,
        /// What to ask the plugin for. 0 = the plugin decides.
        capacity_mb: u64,
        /// Plugin-specific parameters, validated before they leave.
        params: Vec<(String, String)>,
        /// Refuse write claims however a job asks, exactly as
        /// `read_only = true` on a `[[volume]]` block does.
        read_only: bool,
    },
    /// Remove a volume this node provisioned, and stop offering it.
    ///
    /// Only a provisioned one: a `[[volume]]` block is the operator's own
    /// line in their own file, and a verb that deleted it from under them
    /// would be a verb that edits `meshnode.toml`.
    VolumeRemove {
        name: String,
        /// Remove the record even when the plugin refuses, for a volume
        /// whose backing store is already gone.
        force: bool,
    },
    // -- N6 §12 mesh-wide load shedding, appended at the tail ---------------
    /// What is shed right now, and how much of the mesh will honour it.
    ShedStatus,
    /// Set the mesh-wide shed. Both switches clear lifts it.
    ///
    /// **Absolute, not a delta.** `submissions` and `reconcile` are the
    /// state to be in, not a change to apply: two admins flipping different
    /// halves from stale views would otherwise produce a record neither of
    /// them asked for, and the one thing this switch has to be is
    /// unambiguous.
    ShedSet {
        shed: Box<crate::shed::Shed>,
    },
    // -- T4 runtimes, appended at the tail ---------------------------------
    /// What each node can actually run (T4).
    ///
    /// Answered by the node asked, and it answers two different questions
    /// with two different confidences: **its own** probes in full, because
    /// it ran them; **every other node** from the `cap.rt` tag they
    /// announced, because a probe is node-local and a replicated diagnosis
    /// would be a diagnosis that was true when that node booted.
    ///
    /// The second half is what placement actually uses, so this table and
    /// the scheduler cannot disagree.
    Runtimes {
        /// Which node's own probes to ask for, by name. Absent = the one
        /// this client is talking to.
        node: Option<String>,
    },
    // -- Phase B E3/U1 instances, appended at the tail ---------------------
    /// One instance slot up close: every incarnation, health, live use.
    DeploymentInstance {
        name: String,
        slot: String,
    },
    /// Live use of every current instance of a deployment.
    DeploymentInstanceStats {
        name: String,
    },
    // -- Phase B E1 history, appended at the tail --------------------------
    /// The versions a deployment keeps, newest first, and its tags.
    DeploymentHistory {
        name: String,
    },
    /// Put or remove a name on one version. User-signed client-side, as a
    /// deployment is.
    DeploymentTag {
        tag: Box<crate::DeploymentTag>,
    },
    // -- Phase B E2 promotion, appended at the tail ------------------------
    /// A file's knobs, sent before its `DeploymentPut` so a hold is in place
    /// before the roll it holds. Authors nothing when they already match.
    DeploymentKnobs {
        knobs: Box<crate::DeploymentKnobs>,
    },
    /// Promote the canaries of the generation rolling now.
    DeploymentPromote {
        promotion: Box<crate::DeploymentPromotion>,
    },
    // -- Phase C E9, appended at the tail ------------------------------------
    /// `meshnode drain --deadline 30m`: a shutdown drain with a deadline.
    /// Streamed like [`IpcRequest::Drain`]. The node's deployment instances
    /// move under their `migrate` rules until `deadline_secs` from now, and at
    /// the deadline whatever is still running here — deployment instance or
    /// not — is stopped, so the drain ends. A plain `Drain` gives instances
    /// [`crate::DEFAULT_DRAIN_DEADLINE_SECS`] and stops nothing else.
    DrainDeadline {
        move_pending: bool,
        deadline_secs: u64,
    },
    // -- Phase C U8, appended at the tail ------------------------------------
    /// Every machine, with the deployment instances running on it and what
    /// each asked for (`meshnode deployment placement`, Director's Placement
    /// panel). Answered from replicated state, so any node draws the same map.
    PlacementMap,
    // -- SEC-5, appended at the tail ------------------------------------------
    /// Members whose ops this node is holding because they arrived dated
    /// too far ahead of its clock (`meshnode doctor`).
    ClockAhead,
    // -- SEC-17, appended at the tail -----------------------------------------
    /// Every invite this mesh knows about, and what is left of each
    /// (`meshnode invite ls`).
    Invites,
    /// Revoke an invite before it runs out on its own (`meshnode invite
    /// revoke`). Signed by an admin over
    /// [`crate::invite_revoke_signing_bytes`].
    InviteRevoke {
        invite_id: InviteId,
        by: UserId,
        signature: Vec<u8>,
    },
    // -- SEC-7, appended at the tail ------------------------------------------
    /// Set where a secret goes and who may use it (admin, signed).
    SecretScopeSet {
        scope: Box<SecretScope>,
    },
    /// Every secret's scope, as `meshnode secret ls` shows it.
    SecretScopes,
    /// The nodes a new value of `name` should be sealed for, by its scope
    /// — or by `nodes`, a scope about to be set — and this node's view of
    /// the mesh.
    SecretRecipients {
        name: String,
        nodes: Option<String>,
    },
    // -- Director as a view (2026-09), appended at the tail -------------------
    /// The whole live version of a service — its `env` and contract, which
    /// [`IpcRequest::ServiceShow`]'s row leaves out — for a client about to
    /// sign the next version from it. Director read this straight out of
    /// the node it used to embed; as a client of the machine's node it asks,
    /// and is held to what publishing that next version would need.
    ServiceCurrent {
        name: String,
    },
    // -- mesh reset, appended at the tail --------------------------------------
    /// Dissolve the mesh (`meshnode reset`). Founder only, signed over the
    /// mesh's id. Streamed: one [`IpcResponse::MeshReset`] once every
    /// member this node can reach has been handed the reset, and then this
    /// node resets itself.
    MeshReset {
        reset: crate::MeshReset,
    },
}

/// Everything known about one job's placement and its cache history
/// (C8 P1, C7 P2).
///
/// One view for both because they answer one operator question — *"what
/// happened to my job"* — and splitting it across two commands would make
/// the most common follow-up ("it was instant, why?") a second lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobExplainView {
    pub job: crate::JobId,
    pub cmd_preview: String,
    pub state: String,
    pub executor: crate::NodeId,
    pub executor_name: String,
    pub submitter_user: crate::UserId,
    pub created_at_micros: u64,
    pub duration_micros: Option<u64>,
    /// The scoring breakdown, when this node is the one that placed the
    /// job. `None` on any other node — the explanation is local data, and
    /// saying "ask the node that submitted it" is honest where inventing a
    /// re-scored answer against today's mesh would not be.
    pub placement: Option<crate::placement::PlacementExplain>,
    /// The node that holds the explanation, when this one does not.
    pub placement_on: Option<crate::NodeId>,
    /// The action key this job was dispatched under, if it was cacheable.
    pub action_key: Option<crate::ActionKey>,
    /// Set when this job's result came from the cache.
    pub cache_hit: Option<crate::cache::CacheHit>,
    /// The key's fields, for the *"why was this a miss"* diff.
    pub key_fields: Vec<crate::cache::KeyField>,
}

/// Largest file `IpcRequest::BundleFileRead` will return.
pub const BUNDLE_FILE_READ_MAX: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub version: String,
    pub proto_major: u16,
    pub proto_minor: u16,
    pub node_id: NodeId,
    pub node_name: String,
    pub mesh_id: Option<MeshId>,
    pub mesh_name: Option<String>,
    pub listen_addr: String,
    pub uptime_secs: u64,
    /// The fleet census, and **only** the fleet census: live, unrevoked,
    /// non-console members. The same rows `top_rows` and `totals` fold
    /// over, so "members alive" can be reconciled against the node list
    /// the operator is looking at. A console — somebody's Director beside
    /// a `meshnode` on the same box (`CONSOLE_TAG`) — is one person's
    /// window onto a machine the mesh already counts, never a machine of
    /// its own.
    pub members_alive: usize,
    pub members_total: usize,
    pub jobs_running: usize,
    /// Executor slots on this node (MASTERPLAN §11).
    pub slots: usize,
    /// Jobs waiting in this node's pending queue (MASTERPLAN §10.2).
    pub jobs_queued: usize,
    /// The node is draining: finishing accepted work, refusing new work
    /// (MASTERPLAN §12).
    pub draining: bool,
    /// How far the freshest peer clock observed through the HLC runs ahead
    /// of this node's wall clock, in microseconds. Large values mean some
    /// mesh member's clock is skewed — `meshnode doctor` flags it.
    pub hlc_wall_ahead_micros: u64,
    /// WAN reachability summary (MASTERPLAN M7). Appended last: IPC is
    /// same-host lockstep, and an older CLI decoding a newer daemon's
    /// status simply ignores trailing bytes.
    pub wan: Option<WanInfo>,
    /// Quiet hours, if this node has any. Appended for the same reason
    /// `wan` was.
    pub quiet: Option<QuietInfo>,
    /// The node named in genesis as this mesh's founder. Appended for the
    /// same reason `wan` was.
    ///
    /// Surfaced because the founder is the only node that can repair
    /// another's address record ([`IpcRequest::NodeAddrRepair`]), and until
    /// this existed the only way to find out which node that was, was to
    /// try the command somewhere and read the refusal.
    pub founder_node: NodeId,
    /// A timed drain in force (`meshnode drain --until`), appended for the
    /// same reason `wan` was. `None` when the node is not draining or is
    /// draining for good.
    pub drain: Option<DrainInfo>,
    /// How this node fills the mesh when it dispatches work (N6 §12),
    /// appended for the same reason `wan` was.
    ///
    /// Surfaced because it is **per node**: the policy decides where the
    /// node *sending* work puts it, so a mesh where half the nodes pack and
    /// half spread is a real state with no error message, and the only way
    /// to see it is to be able to read the answer off each node.
    pub placement: crate::placement::FitPolicy,
    /// What this node's own dispatches have cost (N6 §12), appended for the
    /// same reason `wan` was.
    ///
    /// Here as well as in `/metrics` because MASTERPLAN bet #3 calls
    /// dispatch overhead *the product*, and a number only a Prometheus can
    /// read is a number most meshes running this will never see.
    pub dispatch: Option<DispatchInfo>,
    /// What this node can run (T4), appended for the same reason `wan` was.
    ///
    /// One line on `status` rather than a table, because the table is
    /// `meshnode runtimes` and `status` is the thing people read first.
    /// What belongs here is the *shape* of the answer — which runtimes are
    /// ready — so that a surprise ("why is container missing?") sends
    /// somebody to the table, and no surprise costs them a second command.
    #[serde(default)]
    pub runtimes: Vec<crate::content::Runtime>,
    /// Why the op-log is refusing writes, if it is (ENG-4), and how many
    /// op merges have panicked and been contained (SEC-22).
    ///
    /// Here so `doctor` can ask about the store's integrity at all
    /// (ENG-26). The panic counter especially: containing a merge failure
    /// is the right behaviour and it means the *only* record that one
    /// happened was a counter nothing read, so a bug worth reporting
    /// looked exactly like a healthy node.
    #[serde(default)]
    pub store_failure: Option<String>,
    #[serde(default)]
    pub contained_panics: u64,
}

/// Dispatch latency on this node, as a one-line readout.
///
/// Bucket edges, never interpolations: "under 5ms" is an answer with
/// observations behind it and "3.7ms" would not be.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DispatchInfo {
    /// Dispatches this node has completed since it started.
    pub count: u64,
    /// The bucket the median landed in, in seconds.
    pub p50_secs: Option<f64>,
    /// The bucket p95 landed in, in seconds.
    pub p95_secs: Option<f64>,
}

/// A timed drain: when it ends and what it was for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DrainInfo {
    pub until_micros: u64,
    pub reason: String,
}

/// What `[limits] quiet` is doing on this node right now.
///
/// The *state*, not the configuration: `slots` above is what the operator
/// asked for and what the mesh has been told, and this is what the node
/// will actually start work in until `until`. Reporting both is the whole
/// point — a node running four jobs out of sixteen slots looks broken until
/// you can see that twelve of them are asleep on purpose.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuietInfo {
    /// Every configured window, as written: `08:00-23:00 @ 30%`.
    pub windows: String,
    /// The zone the windows are read in — `America/Chicago`, or `UTC` on a
    /// node with no tzdata. Named because "the window did not fire" and
    /// "the window fired in the wrong timezone" look identical otherwise.
    pub zone: String,
    /// Slots available right now. Equal to `slots` when no window applies.
    pub effective_slots: usize,
    /// The cap in force, as a percentage; `None` when no window applies.
    pub pct: Option<u8>,
    /// Local `HH:MM` at which the current state ends; `None` when no
    /// window applies and none is imminent.
    pub until: Option<String>,
}

/// WAN reachability as the daemon sees it (MASTERPLAN M7).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WanInfo {
    /// `unknown` | `public` | `natted` | `symmetric-nat`.
    pub reachability: String,
    /// The address peers see us from (cone NAT), when known.
    pub reflexive_addr: Option<String>,
    /// Operator-declared public endpoints (`[wan] advertise`).
    pub advertise: Vec<String>,
    /// This node relays for NATed members right now.
    pub relay_server: bool,
    /// Clients registered with this relay.
    pub relay_clients: u32,
    /// Relays this node is registered with (short ids).
    pub relays: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityInfo {
    pub node_id: NodeId,
    pub node_name: String,
    pub user_id: String,
    pub mesh_id: Option<MeshId>,
    pub mesh_name: Option<String>,
    pub data_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub peer_node_id: NodeId,
    pub peer_node_name: String,
    pub peer_mesh_id: Option<MeshId>,
    pub peer_mesh_name: Option<String>,
    pub same_mesh: bool,
    pub rtt_micros: u64,
    /// Which route answered: a socket address for a direct path, or
    /// `relay <node>` when the reply came back through a relay. A ping
    /// that cannot say *how* it got there cannot settle "is this peer
    /// reachable from here", which is the only question being asked.
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRow {
    pub node_id: NodeId,
    pub name: String,
    pub addrs: Vec<String>,
    /// User-facing tags only: runtime-managed `cap.*` tags are parsed out
    /// into [`Self::capacity`] rather than shown raw (M9 §5.1).
    pub tags: Vec<String>,
    /// Advertised hardware. `None` for a node that predates capacity
    /// announcements or whose probes all failed.
    pub capacity: Option<crate::Capacity>,
    pub liveness: Liveness,
    pub is_self: bool,
    pub revoked: bool,
    pub left: bool,
    pub joined_at_micros: u64,
    /// Set when this peer predates the forward-compatible op-log and is
    /// therefore being served less than the full op stream (F0 §3.3).
    /// A peer in this state is not failing — it is deliberately and
    /// stably behind, and it leaves the state by upgrading.
    pub stale: Option<StalePeer>,
    /// This node accepts interactive terminals (M17 §3.4) — its own
    /// statement about itself, read from the `cap.term` tag it announces.
    ///
    /// Appended at the tail: IPC is same-host lockstep and an older CLI
    /// decoding a newer daemon's row ignores trailing bytes, the property
    /// `results_m19.rs` pins with a test.
    pub accepts_terminals: bool,
    /// How far this peer trailed the local op log the last time it asked
    /// for one. `None` for this node itself, and for a peer that has not
    /// synced with us since the daemon started — absence of evidence, which
    /// the surface renders as "not yet" rather than as zero.
    ///
    /// Appended at the tail, per the note above.
    pub sync: Option<SyncProgress>,
    /// The meshnode build this peer announced (`cap.ver`), M24 §4.
    ///
    /// `None` for a node that predates version announcement — a real state
    /// and not a zero. A rollout reports it as unknown rather than
    /// assuming it is outdated (and upgrading it blind) or current (and
    /// skipping it forever).
    ///
    /// Appended at the tail, per the note above.
    pub version: Option<String>,
    /// Whether the mesh's owner allows work to be placed here
    /// (`OpBody::NodeExecutionSet`). `true` when nobody has decided, which
    /// is every node on a mesh that has never used the control.
    ///
    /// Distinct from the `no-execute` tag above it, which is the node's own
    /// statement about itself: this is the *owner's* answer to it.
    ///
    /// Appended at the tail, per the note above.
    pub contributes: bool,
    /// Isolated pending investigation (E2.4), with the reason. `None` is
    /// the ordinary case.
    pub quarantined: Option<String>,
    /// Why the owner is not sending work here. Empty when they are.
    ///
    /// Appended at the tail, per the note above.
    pub contribute_reason: String,
    /// Which of [`Self::tags`] an **admin assigned** rather than the node
    /// announcing them itself (`OpBody::NodeTagsSet`).
    ///
    /// A subset of `tags`, not a second list: the two are unioned before
    /// anything reads them, so a selector never has to care which half a
    /// tag came from. This is here for the surfaces that *do* care — the
    /// ones offering to remove a tag, which can only offer it for the half
    /// the mesh owns. A node's own `git-sync` is removed by editing that
    /// node's config; an assigned one is removed from anywhere.
    ///
    /// Appended at the tail, per the note above.
    pub assigned_tags: Vec<String>,
}

impl NodeRow {
    /// Should a newly sealed secret be wrapped for this node? (SEC-16)
    ///
    /// The one rule the CLI and Director both apply when they choose a
    /// secret's recipients, so the two cannot drift: not a revoked or
    /// departed machine, and not one quarantined pending investigation — a
    /// node isolated because it may be compromised is the last thing to
    /// hand a new credential to.
    pub fn receives_new_secrets(&self) -> bool {
        !self.revoked && !self.left && self.quarantined.is_none()
    }
}

/// How much of the local op log a peer had received at its last exchange
/// with this node.
///
/// Read against [`SyncProgress::total`]: a peer that is `behind: 0` has
/// everything this node had at `age_micros` ago. This is deliberately a
/// statement about *one* conversation between two nodes and not a
/// mesh-wide truth — a gossip mesh has no such vantage point, and a number
/// that implied one would be a lie.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SyncProgress {
    /// Ops in this node's log the peer had not received.
    pub behind: u64,
    /// Ops in this node's log at that moment — the denominator.
    pub total: u64,
    /// Authors the peer was behind on.
    pub authors: u32,
    /// How long ago the observation was made. A gossip round is seconds, so
    /// a reading minutes old means the peer has stopped asking.
    pub age_micros: u64,
}

/// Why a peer cannot receive everything this node has (F0 §3.3).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StalePeer {
    /// The protocol minor this peer negotiated.
    pub minor: u16,
    /// Authors whose sequence it can no longer follow.
    pub authors_blocked: u32,
    /// Ops in this node's log it can never be sent.
    pub ops_withheld: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRow {
    pub job_id: JobId,
    pub cmd_preview: String,
    pub runtime: String,
    pub submitter: NodeId,
    pub submitter_user: UserId,
    pub submitter_username: Option<String>,
    pub executor: NodeId,
    pub executor_name: Option<String>,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub created_at_micros: u64,
    pub finished_at_micros: Option<u64>,
    /// Set when this job is one target of a fan-out submission.
    pub fanout: Option<FanoutInfo>,
    /// Where this job's stdout and stderr are, and why you cannot see them
    /// (M31 A2). Resolved per read: it is a statement about the world right
    /// now, and caching it would mean answering about a machine that has
    /// since come back.
    #[serde(default = "available")]
    pub output: crate::OutputAvailability,
}

/// `serde(default)` for [`JobRow::output`], so a row written by an older
/// build reads as available rather than failing to decode.
fn available() -> crate::OutputAvailability {
    crate::OutputAvailability::Available
}

/// What to select out of the live job table (**H-1**).
///
/// [`IpcRequest::Jobs`] has no arguments, so every caller — `jobs ls`,
/// `jobs show <prefix>`, Director's two-second tick — asks for *every* job
/// the mesh still remembers, and the daemon pays a `stat()` and possibly a
/// redb read per row to build it, inside the lock arriving ops need. Past
/// roughly twenty thousand jobs the answer stops fitting in
/// [`crate::MAX_FRAME_LEN`] and the verb fails outright rather than
/// slowing down.
///
/// This is the same shape [`HistoryQuery`] already has for the *index*,
/// applied to the *live* table: narrow first, count once, and build rows
/// only for the page that is actually returned.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobsQuery {
    /// Resolve by id: the display form (`mwj1…`) or any leading slice of
    /// it, matched case-insensitively. **The reason `jobs show <prefix>`
    /// stops being O(all jobs)** — the daemon owns the table, so it is the
    /// thing that should be walking it.
    pub prefix: Option<String>,
    /// A state label (`queued`, `running`, `succeeded`, `failed`,
    /// `cancelled`, `timed-out`), matched case-insensitively against
    /// [`JobState::as_str`] exactly as [`HistoryQuery::state`] is. The
    /// pseudo-states `live` (queued or running) and `terminal` are also
    /// accepted, because they are what the UI's tabs actually mean.
    pub state: Option<String>,
    /// Only jobs submitted by this user.
    pub user: Option<UserId>,
    /// Only jobs whose executor is this node.
    pub node: Option<NodeId>,
    /// Only jobs of this fan-out group.
    pub fan: Option<FanId>,
    /// Inclusive lower bound on `created_at_micros`.
    pub since_micros: Option<u64>,
    /// Case-insensitive substring over the command preview.
    pub contains: Option<String>,
    /// Rows to return, newest first. 0 is read as
    /// [`JobsQuery::DEFAULT_LIMIT`].
    pub limit: u32,
    /// Rows to skip, for paging.
    pub offset: u32,
}

impl JobsQuery {
    pub const DEFAULT_LIMIT: u32 = 200;
    /// Hard ceiling per request, so one query cannot decide to materialize
    /// the whole table into an IPC frame — the failure mode this variant
    /// exists to remove.
    pub const MAX_LIMIT: u32 = 5000;
    /// *Every* match, ceiling and all.
    ///
    /// The one caller is the legacy [`IpcRequest::Jobs`], which is defined
    /// as "all jobs". Capping it here
    /// would not be a performance fix, it would be a silent truncation of
    /// a documented answer with no field on the response to admit it —
    /// so the old variant keeps its old meaning and only new callers are
    /// bounded.
    pub const UNBOUNDED: u32 = u32::MAX;

    /// The limit actually applied.
    pub fn effective_limit(&self) -> usize {
        match self.limit {
            0 => Self::DEFAULT_LIMIT as usize,
            Self::UNBOUNDED => usize::MAX,
            n => n.min(Self::MAX_LIMIT) as usize,
        }
    }

    /// A query that selects exactly one job, by full id.
    pub fn for_id(id: JobId) -> Self {
        Self {
            prefix: Some(id.to_display_string()),
            limit: 2,
            ..Self::default()
        }
    }
}

/// How many jobs are in each state, over the *whole* table rather than the
/// returned page.
///
/// Carried on the page because the tab counters above a paged list cannot
/// be derived from the page (**H-9**): computing them client-side is what
/// forced the frontend to hold every row. One pass on the daemon, under a
/// lock it already holds, answers all of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCounts {
    pub all: u64,
    pub queued: u64,
    pub running: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub timed_out: u64,
}

impl JobCounts {
    pub fn tally(&mut self, state: JobState) {
        self.all += 1;
        match state {
            JobState::Queued => self.queued += 1,
            JobState::Running => self.running += 1,
            JobState::Succeeded => self.succeeded += 1,
            JobState::Failed => self.failed += 1,
            JobState::Cancelled => self.cancelled += 1,
            JobState::TimedOut => self.timed_out += 1,
        }
    }

    /// Queued or running.
    pub fn live(&self) -> u64 {
        self.queued + self.running
    }

    /// Anything that ended badly, which is what the "failed" tab means.
    pub fn unsuccessful(&self) -> u64 {
        self.failed + self.cancelled + self.timed_out
    }
}

/// This node's change-signal counters, one per [`crate::StateTopic`]
/// (**H-8**).
///
/// A positional list rather than a map: the topics are a compile-time
/// list, so this is one integer each with no keys on the wire.
///
/// **A `Vec`, not a fixed array**, and that is a wire-compatibility
/// decision rather than a style one. postcard encodes a fixed array with
/// no length prefix, so adding a topic would silently reinterpret every
/// byte after it for a client one release behind — the same trap the
/// append-only rule on [`IpcRequest`] exists to avoid. A length-prefixed
/// list lets a reader take the counters it understands and ignore the
/// rest, which is what [`Self::get`] does.
///
/// **Counters are node-local and monotonic within one process.** They
/// count what this node has applied, not a mesh-wide clock, and they
/// restart at zero when the daemon does — so a client must treat *any*
/// difference as "changed" rather than assuming the number only goes up.
/// Comparing for inequality does that for free.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateVersions {
    /// One counter per [`crate::StateTopic`], in `StateTopic::ALL` order.
    /// Shorter than `ALL` when written by an older build; longer when
    /// written by a newer one. Both read correctly through [`Self::get`].
    pub counters: Vec<u64>,
}

impl StateVersions {
    pub fn get(&self, topic: crate::StateTopic) -> u64 {
        crate::StateTopic::ALL
            .iter()
            .position(|t| *t == topic)
            .and_then(|i| self.counters.get(i).copied())
            .unwrap_or(0)
    }

    /// Every topic this build knows whose counter differs from `other`.
    pub fn changed_since(&self, other: &StateVersions) -> Vec<crate::StateTopic> {
        crate::StateTopic::ALL
            .iter()
            .filter(|t| self.get(**t) != other.get(**t))
            .copied()
            .collect()
    }
}

/// One page of [`IpcRequest::JobsQuery`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobsPage {
    /// Newest first, at most [`JobsQuery::effective_limit`] of them.
    pub rows: Vec<JobRow>,
    /// How many rows match the filter, before `limit`/`offset`.
    pub total: u64,
    /// State tallies over the unfiltered table, for the counters a paged
    /// list cannot compute for itself.
    pub counts: JobCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRow {
    pub user_id: UserId,
    pub username: String,
    pub is_founder: bool,
    pub revoked: bool,
    pub roles: Vec<String>,
    pub created_at_micros: u64,
    /// This person's key is provably on hardware and cannot be exported
    /// (Y2/Y3). `None` for everyone who has not attested one — which is
    /// everybody, until somebody does.
    ///
    /// **The whole point of the feature is this field being present.** "We
    /// support YubiKeys" is a checkbox; a column that says *every admin key
    /// in this mesh was generated on genuine hardware, and here is the
    /// authority that vouched for it* is an audit finding that closes.
    pub hardware: Option<HardwareRow>,
}

/// A verified hardware attestation, as `meshnode users` prints it (Y2/Y3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareRow {
    /// `piv` | `openpgp` | `fido2`.
    pub kind: String,
    /// Where on the device, as the device named it: `9c`, `auth`, `sig`.
    pub slot: String,
    /// The trusted authority that vouched for the chain.
    pub root_label: String,
    /// The leaf certificate's subject, verbatim — on a YubiKey PIV
    /// attestation this carries the device serial.
    pub subject: String,
    pub attested_at_micros: u64,
    /// Leaf validity as the certificate states it. **Reported, never
    /// enforced**: a certificate expiring does not make the key it attested
    /// exportable, and a mesh that dropped a security property on a date
    /// nobody chose would be degrading silently.
    pub not_after_micros: u64,
    pub revoked: bool,
    pub revoked_reason: String,
}

/// One trusted attestation authority, as `meshnode users attest-root`
/// prints it (Y2/Y3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestRootRow {
    /// The digest of the certificate, which is what it is filed under.
    pub id: String,
    pub label: String,
    pub added_by: UserId,
    pub added_by_username: String,
    pub added_at_micros: u64,
    pub removed: bool,
    /// How many people's attestations this authority currently vouches for.
    pub attesting: u32,
}

/// One configured identity provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcProviderRow {
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub groups_claim: String,
    /// `group=role` pairs, rendered.
    pub group_roles: Vec<String>,
    pub configured_at_micros: u64,
}

/// One machine principal, as `meshnode serviceaccounts` prints it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceAccountRow {
    pub user_id: UserId,
    pub name: String,
    pub description: String,
    pub created_at_micros: u64,
    pub expires_at_micros: Option<u64>,
    /// Already past its own clock — it holds no roles whatever it was
    /// granted, and re-granting it will not help.
    pub expired: bool,
    /// The user record was revoked. Terminal and separate from `expired`:
    /// one is a clock running out, the other is an admin's decision.
    pub revoked: bool,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRow {
    pub grant_id: GrantId,
    pub user_id: UserId,
    pub username: String,
    /// The *holder* is revoked, so this grant confers nothing whatever its
    /// own state says (E2.3).
    ///
    /// Authorization was always right about this — `roles_at` checks the
    /// principal before it looks at a grant — but the table was not, and a
    /// revoked person's grant reading `active` in an access review is the
    /// kind of wrong that gets believed.
    pub holder_revoked: bool,
    pub role: String,
    pub nodes: String,
    pub services: String,
    pub expires_at_micros: Option<u64>,
    pub revoked: bool,
    pub expired: bool,
    pub created_at_micros: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRow {
    pub name: String,
    pub version: u32,
    pub created_by: UserId,
    pub created_by_name: String,
    pub created_at_micros: u64,
    /// How many nodes hold a wrap (can decrypt) vs total active members.
    pub wrapped_for: u32,
    pub members: u32,
    /// Tombstoned. Listed so clients can pick the next version correctly.
    pub removed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRow {
    pub name: String,
    pub plugin: String,
}

/// One node's saturation view (`meshnode top`, MASTERPLAN §10.2). Running
/// and queued counts come from replicated job summaries; slots from the
/// node's advertised capacity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopRow {
    pub node_id: NodeId,
    pub name: String,
    pub liveness: Liveness,
    pub is_self: bool,
    pub no_execute: bool,
    pub slots: u32,
    pub running: u32,
    pub queued: u32,
    /// Advertised hardware threads (M9 §5.1). 0 = not advertised.
    pub cores: u32,
    /// Live pressure (M9 §5.2), when a fresh reading is held for this node.
    /// `None` is unknown — a peer below the compute minor, a reading that
    /// aged out, or a node this one has not gossiped with recently.
    pub pressure: Option<crate::PressureSnapshot>,
    /// Cores held by gang leases on this node that no job is using yet
    /// (C4 P1) — reservations made on behalf of a decision that has not
    /// been made.
    ///
    /// **Known exactly for this node and inferred for every other**, and
    /// the row does not pretend otherwise. A lease is deliberately not
    /// replicated (§8.2), so a remote node's figure is computed from the
    /// gang *plans* that are: an uncommitted plan naming that node means a
    /// lease is outstanding there, unless the plan is older than the
    /// longest lease anyone will grant, in which case it has expired and
    /// nobody has to be told. That is an upper bound within one lease
    /// lifetime and exact outside it, which is the right shape for a column
    /// whose job is to explain why a node with free slots refused work.
    pub leased_cpus: u32,
    /// Reservations behind `leased_cpus`.
    pub leases: u32,
    /// Accelerators this node advertises, and how many of them are spoken
    /// for right now (C5 P5). Both zero on a mesh with no accelerators,
    /// which is what keeps the columns out of the way until there is
    /// something to put in them.
    pub devices: u32,
    /// **Exact for this node, and absent for every other**, for the same
    /// reason `leased_cpus` is a bound: a device hold is node-local and
    /// there is no honest way to read a remote node's ledger. A remote row
    /// leaves it `None` rather than guessing.
    pub devices_busy: Option<u32>,
    /// How busy the idlest advertised device is, in `0.0..=1.0`, from live
    /// telemetry. `None` is unknown — a peer below `ACCEL_MINOR`, or a
    /// reading that aged out — which is neutral, never idle and never busy.
    pub device_busy: Option<f64>,
    /// The live picture of the machine (`TELEMETRY_MINOR`): real CPU
    /// utilisation, memory and disk totals, uptime, runnable tasks.
    ///
    /// Appended at the tail, per the rule on `accepts_terminals`. `None`
    /// for a peer that predates it and for a reading that aged out — the
    /// same two reasons `pressure` is `None`, and the same treatment: a
    /// surface says "unknown", never "idle".
    pub system: Option<crate::SystemSnapshot>,
}

/// What this mesh adds up to (`meshnode totals`, M9 §6.3).
///
/// The aggregate of [`NodeRow::capacity`] over the live membership, plus
/// the live pressure readings that happen to be fresh. Nothing here is new
/// information — it is `Nodes` and `Top` summed — and the reason it is a
/// request rather than a client-side fold is that three surfaces were
/// each about to write the same summation slightly differently.
///
/// **Every total carries how many nodes it is a total *of*.** A mesh where
/// four of forty nodes have not advertised their memory has an honest
/// answer ("6.4 TB across 36 of 40") and a dishonest one ("6.4 TB"), and
/// the dishonest one is the one that erodes trust the first time somebody
/// checks it against the hardware.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Totals {
    /// Members that are neither revoked nor departed.
    pub nodes_known: u32,
    /// …of which gossip currently believes are alive.
    pub nodes_live: u32,
    /// …of which have stopped accepting work (`drain`).
    pub nodes_draining: u32,
    /// Hardware threads advertised by live nodes, and how many of them
    /// advertised any. `cores_from < nodes_live` means the figure is a
    /// floor, and the surfaces say so.
    pub cores: u64,
    pub cores_from: u32,
    /// Executor slots configured across live nodes — the mesh's real
    /// concurrency ceiling, which is a policy number rather than a
    /// hardware one and is routinely different from `cores`.
    pub slots: u32,
    /// Slots with a job on them right now.
    pub slots_busy: u32,
    /// Total RAM advertised, and by how many nodes.
    pub mem_mb: u64,
    pub mem_from: u32,
    /// RAM reported available by the freshest pressure readings held for
    /// live nodes, and how many nodes those readings cover. Absent
    /// readings are excluded from both numbers rather than counted as
    /// zero (M9 §5.2: a stale reading is unknown, never idle).
    pub mem_available_mb: u64,
    pub mem_available_from: u32,
    /// Data-dir filesystem size advertised, and by how many nodes.
    pub disk_mb: u64,
    pub disk_from: u32,
    /// Free space from the same pressure readings, on the same terms.
    pub disk_free_mb: u64,
    pub disk_free_from: u32,
    /// Accelerators advertised across the mesh (C5).
    pub devices: u32,
    /// Devices held by a job **on this node only**. A device hold is
    /// node-local and there is no honest way to read a remote ledger, so
    /// this is deliberately not a mesh-wide figure — same reasoning as
    /// [`TopRow::devices_busy`].
    pub devices_busy_here: u32,
    /// Jobs running and queued across live nodes, from the same readings.
    pub jobs_running: u32,
    pub jobs_queued: u32,
    /// Architectures present, most common first — the number that decides
    /// whether a bundle can run everywhere.
    pub arches: Vec<(String, u32)>,
    /// Mean load per core over the live nodes with a fresh reading, in
    /// `0.0..` where 1.0 is "as busy as it has cores for". `None` when no
    /// node has a fresh reading, which is a real state on a mesh that has
    /// just started.
    pub load_per_core: Option<f64>,
    /// Schedulable GPUs across the mesh, and the memory on them (L2 P1).
    ///
    /// A subset of [`Self::devices`], which counts every kind of
    /// accelerator: the strip says "3 GPUs (72 GB VRAM)" and a TPU in that
    /// sentence would be a wrong number rather than a rounded one.
    ///
    /// **Summed, unlike `cap.vram_mb`, which is the largest single card.**
    /// The two answer different questions and both are right: a filter asks
    /// *can one device hold this model* (largest), and a spec sheet asks
    /// *how much accelerator did I buy* (total). See
    /// [`crate::capacity::CAP_VRAM_MB`] for the other half.
    ///
    /// Appended at the tail, per the note on [`IpcRequest::Totals`]: IPC is
    /// same-host lockstep and an older CLI decoding a newer daemon's struct
    /// ignores trailing bytes.
    pub gpus: u32,
    pub vram_mb: u64,
    /// Live nodes that advertised **any** `cap.*` tag — the denominator of
    /// the strip's coverage line (D-L2.2).
    ///
    /// Not the same as `cores_from`: a node can advertise memory and fail
    /// its core probe, and a coverage line built from one field would call
    /// that node silent. Appended at the tail, per the note above.
    pub capacity_from: u32,
}

impl Totals {
    /// The mesh's spec sheet, in one line (L2 P1).
    ///
    /// `4 machines · 68 cores · 312 GB · 3 GPUs (72 GB VRAM) · 4.1 TB
    /// poolable disk · 2 jobs running`
    ///
    /// **One place, because three surfaces read it.** The CLI prints this
    /// string, Director's Mesh view renders it, and the shared snapshot
    /// (L2 P2) draws it into a PNG that leaves the building. Three
    /// summations phrased three ways is the failure `Totals` was made a
    /// request to avoid, and three *renderings* of one summation is the
    /// same failure one layer up.
    ///
    /// A figure nothing reported is absent rather than zero (D-L2.2): a
    /// mesh with no accelerators says nothing about GPUs instead of
    /// claiming it has none, which is the distinction between a probe that
    /// found nothing and a probe that did not run.
    pub fn strip(&self) -> String {
        let mut parts = vec![format!(
            "{} machine{}",
            self.nodes_live,
            if self.nodes_live == 1 { "" } else { "s" }
        )];
        if self.cores_from > 0 {
            parts.push(format!("{} cores", self.cores));
        }
        if self.mem_from > 0 {
            parts.push(size_mb(self.mem_mb));
        }
        if self.gpus > 0 {
            // Device memory is routinely unprobed on an integrated GPU, and
            // "(0 GB VRAM)" reads as a broken card rather than an unread
            // one.
            parts.push(match self.vram_mb {
                0 => format!("{} GPU{}", self.gpus, if self.gpus == 1 { "" } else { "s" }),
                mb => format!(
                    "{} GPU{} ({} VRAM)",
                    self.gpus,
                    if self.gpus == 1 { "" } else { "s" },
                    size_mb(mb)
                ),
            });
        }
        if self.disk_from > 0 {
            // "poolable disk", never "storage" (L2 §6.2). Disk pools across
            // a mesh and RAM does not, and a total labelled "storage"
            // invites exactly the inference this product refuses to let
            // anybody make.
            parts.push(format!("{} poolable disk", size_mb(self.disk_mb)));
        }
        if self.jobs_running > 0 {
            parts.push(format!("{} jobs running", self.jobs_running));
        }
        parts.join(" · ")
    }

    /// *"3 of 4 machines reporting"* — and `None` when every live machine
    /// is (D-L2.2).
    ///
    /// Shown whenever it is not 100%, and only then: a qualifier on every
    /// line trains people to stop reading qualifiers, and a total over
    /// three of four machines presenting as a total over four is the first
    /// lie the product would ever tell.
    pub fn coverage(&self) -> Option<String> {
        (self.capacity_from < self.nodes_live).then(|| {
            format!(
                "{} of {} machines reporting",
                self.capacity_from, self.nodes_live
            )
        })
    }
}

/// MB as `312 GB` / `4.1 TB` — the spec-sheet face, with the space and the
/// unit spelled out.
///
/// Deliberately not `format.rs`'s `gib()`, which renders `312G` for a
/// table column where the header carries the unit. This one appears inside
/// a sentence a stranger reads once, and `312G` in prose is a figure you
/// have to decode.
fn size_mb(mb: u64) -> String {
    let gb = mb as f64 / 1024.0;
    if gb >= 1024.0 {
        format!("{:.1} TB", gb / 1024.0)
    } else if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else {
        format!("{gb:.1} GB")
    }
}

/// One registered Git source (`meshnode sources ls`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRow {
    pub name: String,
    pub url: String,
    pub branch: String,
    pub interval_secs: u64,
    pub bump: Vec<String>,
    pub created_by_name: String,
    pub last_commit: Option<String>,
    pub last_bundle: Option<BundleId>,
    pub last_synced_at_micros: Option<u64>,
    /// The node currently elected to watch this source, if determinable.
    pub watcher: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmIInfo {
    pub user_id: UserId,
    pub username: Option<String>,
    pub registered: bool,
    pub is_founder: bool,
    pub roles: Vec<String>,
    pub grants: Vec<GrantRow>,
    /// The node that answered — "this node", in the sentence a person
    /// needs: *your key, on this machine, is not known to this mesh*.
    ///
    /// Appended at the tail: IPC is same-host lockstep and an older CLI
    /// decoding a newer daemon's struct ignores trailing bytes.
    pub node_name: String,
    /// The node that answered, by id. Carried because an enrolment
    /// request has to *name the node it was made from* and the caller
    /// making it has no roles yet — `whoami` is the one door open to them,
    /// so it is the one that has to tell them where they are standing.
    pub node_id: crate::NodeId,
    /// Where the calling key stands in the enrolment queue (M23). The
    /// difference between "ask an admin" and "you already did, two minutes
    /// ago, and the code is on your screen".
    pub enrolment: crate::EnrolmentState,
    /// The live enrolment code, while there is a request to compare.
    pub enrolment_code: Option<String>,
    /// True when this CLI is signing with the local daemon's own admin key
    /// — the founder's zero-setup path. Worth saying out loud, because a
    /// founder who never sees the unenrolled state has no intuition for
    /// what everyone else goes through.
    pub is_daemon_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRow {
    pub bundle_id: BundleId,
    pub total_size: u64,
    pub file_count: u32,
    pub chunk_count: u32,
    pub holders: usize,
    pub local: bool,
}

/// What one node's content store is holding (M31 A2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContentStatusView {
    /// The level this node is set to, and the window it resolved to after
    /// the mesh policy had its say. Both, because "balanced" and "7 days"
    /// are different answers when the mesh has capped the node.
    pub policy: String,
    pub window_days: u32,
    /// True when the mesh policy shortened what this node asked for — the
    /// one case where the screen and the config file disagree, and the
    /// reason has to be visible or it reads as a bug.
    pub capped_by_mesh: bool,
    pub bundles: u64,
    pub chunks: u64,
    pub bytes: u64,
    /// Bundles something still needs; never reclaimable at any level.
    pub protected: u64,
    pub reclaimable_bundles: u64,
    pub reclaimable_bytes: u64,
    pub max_bytes: u64,
    /// Filesystem holding the store.
    pub disk_free_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_floor_bytes: u64,
    pub disk_warn_bytes: u64,
    /// Under the floor after reclaiming: this node is refusing new content.
    pub disk_critical: bool,
}

/// What a reclaim took, or would take (M31 A2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContentPruneView {
    pub dry_run: bool,
    pub bundles_removed: u64,
    pub chunks_removed: u64,
    pub bytes_reclaimed: u64,
    pub bytes_remaining: u64,
    pub protected: u64,
    pub orphan_chunks: u64,
    pub orphan_bytes: u64,
}

/// One node's room, as the mesh currently sees it (M31 A2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskRow {
    pub node_id: NodeId,
    pub name: String,
    pub online: bool,
    pub free_bytes: u64,
    pub total_bytes: u64,
    /// `ok` | `warn` | `low` | `unknown`. A string because the set grows,
    /// and a node running an older build must render a verdict it has
    /// never heard of rather than fail to parse the row.
    pub state: String,
}

/// One kind of thing this mesh distributes (M24 §3.1).
///
/// The gap M24 names is not capability — every row here already replicates
/// through the op-log, automatically, within a gossip round or two.
/// The gap is that it is **invisible**: there was no surface that said
/// "here is everything this mesh distributes, here is what version of it
/// each node has, here is what to press to change it".
///
/// The catch-up count is not new machinery. A node's op-log position *is*
/// its definition version — a node that is caught up has every definition
/// by construction — so `caught_up` is the sync-lag reading the node work
/// already computes, filtered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionRow {
    /// Stable key: `workflows`, `services`, `collectors`, `hosts`, …
    pub kind: String,
    /// What it is, in one line.
    pub what: String,
    /// The op that publishes one.
    pub op: String,
    /// How many of this kind the mesh holds.
    pub count: u32,
    /// When one of them last changed.
    pub last_changed_micros: Option<u64>,
    /// The name of whatever changed last — "policy `prod-freeze`, 4m ago"
    /// is a sentence; "policies, 4m ago" is a shrug.
    pub last_changed_what: Option<String>,
    /// True when this kind *executes* on nodes without anybody pressing
    /// Run (§3.3) — the ones where widening a selector is a deployment.
    pub executes: bool,
}

/// One meshnode release living in the mesh (M24 §9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRow {
    pub version: String,
    pub bundle_id: BundleId,
    pub artifact: String,
    pub total_size: u64,
    /// How many nodes hold the bytes already. A release every node holds
    /// is a rollout that will not wait on a download — which is what
    /// `meshnode bundles prefetch` is for.
    pub holders: usize,
    /// This node holds it.
    pub local: bool,
    pub published_by: String,
    pub published_at_micros: u64,
    /// The architecture the artifact is for (`x86_64`, `aarch64`), read
    /// off its name; empty for a record that predates the naming. A
    /// version may be published once per architecture, so a fleet screen
    /// groups by version and lists these.
    ///
    /// Appended at the tail: IPC is same-host lockstep and an older CLI
    /// decoding a newer daemon's row ignores trailing bytes.
    pub arch: String,
    /// Hex of the binary's BLAKE3 digest — what both signatures are about,
    /// and what a co-signature is filed under (Y6).
    pub digest: String,
    /// The trusted key that co-signed this binary, if one has (Y6), and
    /// whether a human touched the device to do it.
    ///
    /// `None` where nothing has co-signed it. On a mesh that trusts an
    /// SSH-form release key that is not a gap in a list, it is a release
    /// nobody can install.
    pub cosigned_by: Option<String>,
    pub cosign_user_present: bool,
}

/// One upgrade plan, as a surface shows it (M24 §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradePlanRow {
    pub plan_id: crate::UpgradeId,
    pub selector: String,
    pub version: String,
    pub move_pending: bool,
    pub created_by: String,
    pub created_at_micros: u64,
    pub expires_at_micros: u64,
    pub cancelled: bool,
    /// True while the plan is still something a node would act on if asked.
    pub live: bool,
    /// Published as a forced plan: it re-installs even where the version
    /// already matches. Appended at the tail, per the note on
    /// [`IpcRequest::Totals`].
    pub force: bool,
}

/// One release key, as a surface shows it (M24 §9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseKeyRow {
    pub pubkey: String,
    pub label: String,
    pub added_by: String,
    pub added_at_micros: u64,
    pub revoked: bool,
    /// True when *this* node would actually use it: a node whose owner
    /// wrote `[release] pubkey` ignores the mesh's list, and a surface
    /// that did not say so would be describing a fleet it is not
    /// configuring.
    pub in_use_here: bool,
}

/// What this node verifies releases against (M24 §9) — the answer to "why
/// did that node refuse a release everyone else installed".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseKeyView {
    pub keys: Vec<ReleaseKeyRow>,
    /// This node's `[release] pubkey`, when its owner wrote one down. It
    /// overrides the mesh's list on this node alone.
    pub local_override: Option<String>,
    /// True when the key compiled into this build is the in-tree
    /// placeholder — the state a fleet is in before it has ever trusted a
    /// key, and the reason a first upgrade refuses.
    pub embedded_is_placeholder: bool,
}

/// One file of a bundle, as listed by `IpcRequest::BundleFiles`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleFileRow {
    /// Path relative to the bundle root, forward-slash separated.
    pub path: String,
    pub size: u64,
    /// Unix mode bits as stored in the manifest.
    pub mode: u32,
    /// Any execute bit set (what the `exec` runtime needs).
    pub executable: bool,
    /// Symlink target, for symlink entries.
    pub symlink: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRow {
    pub name: String,
    pub runtime: Runtime,
    pub version: u32,
    pub bundle_id: BundleId,
    pub working_dir: String,
    pub entrypoint: String,
    pub has_contract: bool,
    /// The contract's JSON Schema text, when one is attached — clients
    /// (Director) generate input forms from it (MASTERPLAN §8.3).
    pub contract: Option<String>,
    pub description: String,
    pub tags: Vec<String>,
    pub versions: Vec<u32>,
    /// The current version's static environment.
    ///
    /// Appended at the tail (IPC is same-host lockstep, and this struct is
    /// not signed). Carried because a client deciding whether an inline
    /// `[deployment.container]` already matches what is published has to
    /// compare the *command override* too, and that lives here as
    /// `MESHWARE_TASK_COMMAND` / `_ARGS` rather than in `entrypoint`.
    pub env: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Schedule rows (M8 §3.9)
// ---------------------------------------------------------------------------

/// Answer to a `DryRun`: the fully resolved submission, dispatched nowhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobPreview {
    /// `job.run` | `service.run` | `parallel`.
    pub kind: String,
    /// What would run, after resolution.
    pub action: String,
    pub service: Option<String>,
    pub version: Option<u32>,
    pub bundle_id: Option<BundleId>,
    pub runtime: Option<String>,
    pub selector: String,
    /// Where it would land, in scheduler order.
    pub targets: Vec<String>,
    /// Live nodes the selector matched that the caller's grants exclude —
    /// named rather than elided, because "it only hit 3 of 10" is the kind
    /// of surprise a preview exists to prevent.
    pub excluded: Vec<String>,
    /// Env keys only. Values may carry secret references, and a preview is
    /// a thing people paste into tickets.
    pub env_keys: Vec<String>,
    /// Secrets referenced, by name.
    pub secrets: Vec<String>,
    pub priority: String,
    pub timeout_secs: Option<u64>,
    /// Distinct nodes touched.
    pub blast_radius: u32,
}

// One reply per IPC round trip, never held in bulk, so boxing the large
// variants would buy nothing but an allocation per reply.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    Status(StatusInfo),
    Identity(IdentityInfo),
    WhoAmI(WhoAmIInfo),
    Ping(PingResult),
    InviteBootstrap {
        mesh_id: MeshId,
        node_id: NodeId,
        addrs: Vec<String>,
    },
    Nodes(Vec<NodeRow>),
    Users(Vec<UserRow>),
    ServiceAccounts(Vec<ServiceAccountRow>),
    OidcProviders(Vec<OidcProviderRow>),
    /// A login has begun; show the person the code.
    OidcLoginStarted {
        user_code: String,
        verification_uri: String,
        verification_uri_complete: String,
        device_code: String,
        interval: u64,
        expires_in: u64,
    },
    /// Still waiting for the person to approve.
    OidcLoginPending {
        interval: u64,
    },
    /// Verified. The assertion is published; `meshnode enrol` carries it.
    OidcLoginVerified {
        subject: String,
        email: String,
        display_name: String,
        groups: Vec<String>,
        suggested_role: Option<String>,
    },
    Grants(Vec<GrantRow>),
    Secrets(Vec<SecretRow>),
    Providers(Vec<ProviderRow>),
    Audit(Vec<AuditEntry>),
    Jobs(Vec<JobRow>),
    JobShow(JobRow),
    Pushed {
        bundle_id: BundleId,
        file_count: u32,
        chunk_count: u32,
        total_size: u64,
    },
    Bundles(Vec<BundleRow>),
    ContentStatus(Box<ContentStatusView>),
    ContentPrune(Box<ContentPruneView>),
    DiskReport(Vec<DiskRow>),
    Services(Vec<ServiceRow>),
    ServiceShow(ServiceRow),
    ServiceCreated {
        name: String,
        version: u32,
    },
    // -- runs (M10 P2) ------------------------------------------------------
    /// First frame of a run stream.
    RunStarted {
        run: RunId,
        workflow: String,
        version: u32,
        /// Step ids in definition order, main graph then compensation, so a
        /// client can lay out the run before anything has happened.
        steps: Vec<String>,
    },
    /// A step began. `targets` is what its selector resolved to — the
    /// answer `same-as:` pins later steps to.
    RunStepStarted {
        step: String,
        attempt: u32,
        job_id: Option<JobId>,
        group: Option<FanId>,
        targets: Vec<String>,
    },
    /// Output from one step. `stream`: 1 = stdout, 2 = stderr. Carries the
    /// step id so a client can prefix it without tracking job ids.
    RunChunk {
        step: String,
        stream: u8,
        data: Vec<u8>,
    },
    RunStepDone {
        step: String,
        state: StepState,
        exit_code: Option<i32>,
        /// Declared outputs this step published.
        outputs: Vec<(String, String)>,
        /// Why, when the state alone does not say it (a skip reason, a
        /// retry that ran out, a target nobody could place).
        detail: String,
    },
    /// Terminal frame of a run stream.
    RunDone {
        state: RunState,
        succeeded: u32,
        failed: u32,
        skipped: u32,
        detail: String,
    },
    /// Retired 2026-09-20 with [`IpcRequest::HistoryRuns`]. Tombstone.
    HistoryRuns,
    HistoryJobs(Box<HistoryJobPage>),
    HistoryStatus(Box<HistoryStats>),
    /// Answer to [`IpcRequest::Backpressure`].
    Backpressure(Box<BackpressureView>),
    /// Answer to [`IpcRequest::AuditRows`]: entries plus their proofs,
    /// oldest first.
    AuditRows(Vec<crate::rbac::SignedAuditRow>),
    /// Answer to [`IpcRequest::HistoryReplay`]: one window, oldest line
    /// first, plus whatever the read has to admit.
    PayloadWindow(Box<crate::PayloadWindowReply>),
    JobStarted {
        job_id: JobId,
        executor: NodeId,
        executor_name: String,
    },
    LogChunk {
        stream: u8,
        data: Vec<u8>,
    },
    JobDone {
        state: JobState,
        exit_code: Option<i32>,
    },
    Ok,
    /// The request was authenticated but the user's grants do not allow it.
    Denied {
        message: String,
    },
    Error {
        message: String,
    },
    // -- fan-out streaming (MASTERPLAN §9.3) --------------------------------
    /// First frame of a fanned-out run: the logical job + all its targets,
    /// in `index` order.
    FanoutStarted {
        group: FanId,
        targets: Vec<FanTarget>,
    },
    /// One chunk of one target's output. `stream`: 1 = stdout, 2 = stderr.
    FanChunk {
        index: u32,
        stream: u8,
        data: Vec<u8>,
    },
    /// One target finished.
    FanJobDone {
        index: u32,
        state: JobState,
        exit_code: Option<i32>,
    },
    /// Terminal frame of a fanned-out run. `skipped` counts targets never
    /// dispatched because `--halt-on-fail` tripped (MASTERPLAN §9.3).
    FanoutDone {
        succeeded: u32,
        failed: u32,
        skipped: u32,
    },
    // -- M5 scheduler visibility & Git sources ------------------------------
    Top(Vec<TopRow>),
    Sources(Vec<SourceRow>),
    /// Outcome of a `SourceSyncNow`.
    SourceSync {
        commit: String,
        bundle_id: BundleId,
        /// True when the commit matched the last sync (nothing ingested).
        unchanged: bool,
        /// Services auto-bumped, as (name, new version).
        bumped: Vec<(String, u32)>,
    },
    // -- LAN pairing --------------------------------------------------------
    /// The pairing exchange reached the code display: both humans now
    /// compare and confirm. Answer with `PairDecide { session, … }`.
    PairCode {
        session: u64,
        /// Six digits, zero-padded (display as `123 456`).
        code: String,
        joiner_name: String,
        joiner_id: NodeId,
    },
    /// Terminal frame of a successful pairing: the node is a member.
    PairDone {
        node_id: NodeId,
        node_name: String,
    },
    // -- M6 day-2 operations (MASTERPLAN §12) --------------------------------
    /// Periodic progress frame while a drain waits for accepted work to
    /// finish. The terminal frame of a drain is `Ok`.
    DrainProgress {
        running: u32,
        queued: u32,
        /// Queued jobs re-dispatched to other candidates (`--move`).
        moved: u32,
        /// Accelerators still held by work that has not finished (C5 P6).
        ///
        /// A drain is a wait, and "2 running" does not tell an operator
        /// whether the box can be powered down for the card swap they are
        /// draining it *for*. This does: a device is free when nothing
        /// holds it, and that is a different question from whether a slot
        /// is free.
        devices_held: u32,
    },
    /// A backup file was written (MASTERPLAN §4.5).
    BackupDone {
        path: String,
        ops: u64,
        bytes: u64,
    },
    // -- bundle browsing ------------------------------------------------------
    /// The files of a bundle (`IpcRequest::BundleFiles`), in manifest order.
    BundleFiles(Vec<BundleFileRow>),
    /// The bytes of one bundle file (`IpcRequest::BundleFileRead`).
    BundleFile {
        data: Vec<u8>,
    },
    // -- parallel compute (MASTERPLAN M9 §5.4) -------------------------------
    /// First frame of a parallel run: the job exists, has an item count,
    /// and — the part that stops a one-node run looking like a broken mesh
    /// — names the nodes that took it on, with the reason any node did not.
    ParallelStarted {
        job_id: JobId,
        total: u64,
        /// Nodes now pulling chunks, the owner first.
        workers: Vec<String>,
        /// `name: reason` for every node that was offered the job and did
        /// not take it.
        declined: Vec<String>,
        /// Roughly how long this will take, from what runs like it actually
        /// took (R5 P4). `None` when nothing comparable has ever run — the
        /// honest answer, and better than a confident number derived from
        /// one sample.
        ///
        /// A **range**, never a time. A preview that says "17 minutes" is
        /// wrong precisely often enough to be untrusted forever; one that
        /// says 12–25 is right and stays trusted (C8's rule, applied to the
        /// thing a person reads before they walk away from the screen).
        estimate: Option<crate::DurationRange>,
    },
    /// One item finished, wherever it ran.
    ParallelItem {
        item: String,
        /// `None` = the item could not be spawned, or timed out.
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        node: NodeId,
        node_name: String,
        /// Under `map`, where the output stayed. `stdout` is empty in that
        /// case for successful items: the bytes are in the object store on
        /// the node that produced them, and this is the reference (§5.5).
        output: Option<ObjectRef>,
        /// Outputs this item published (M19 §5.4). **Appended at the tail**
        /// of an existing variant, which is safe for an older CLI because
        /// `read_frame` is length-delimited and `postcard::from_bytes`
        /// ignores the unread tail — the same property `StatusInfo::wan`
        /// already relies on. That claim used to be a comment; M19 P2 turns
        /// it into an assertion in `results_m19.rs`, because the whole IPC
        /// skew story leans on it and nothing tested it.
        ///
        /// Empty unless the caller asked for capture, so an ordinary
        /// `parallel` run's frames are byte-identical to what they were.
        outputs: Vec<(String, String)>,
    },
    /// Periodic progress. `per_node` is the view that makes work stealing
    /// legible — a 32-core node visibly drawing twice the chunks of a
    /// 16-core one, with no operator input (M9 §6.3).
    ParallelProgress {
        total: u64,
        done: u64,
        failed: u64,
        /// Items handed out and not yet reported back.
        inflight: u64,
        /// Items per chunk right now (adaptive, M9 §5.4).
        chunk_items: u32,
        /// Chunks re-run elsewhere to beat a straggler (M9 §5.6). Shown so
        /// a duplicate reads as mitigation rather than as a fault.
        speculated: u32,
        per_node: Vec<NodeProgress>,
    },
    /// Terminal frame of a parallel run.
    ParallelDone {
        total: u64,
        succeeded: u64,
        failed: u64,
        micros: u64,
        speculated: u32,
        per_node: Vec<NodeProgress>,
        /// Why the owner stopped early, when `--halt-on-fail` tripped.
        /// Appended at the tail, per the rule on `ParallelItem::outputs`.
        halted: Option<String>,
    },
    /// The reduce step's result, sent just before `ParallelDone` when the
    /// job had one (M9 §5.6).
    ParallelReduced {
        /// Where it ran — the node that already held the most input, so
        /// the least data moved.
        node_name: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        /// The reduce's own output, if it produced one worth storing.
        output: Option<ObjectRef>,
        /// How many map outputs it was handed.
        inputs: u64,
    },
    // -- ephemeral objects (MASTERPLAN M9 §5.5) ------------------------------
    /// Answer to `IpcRequest::Objects`.
    Objects(Vec<ObjectRow>),
    /// One slice of a fetched object's bytes (`IpcRequest::ObjectGet`).
    ObjectChunk {
        data: Vec<u8>,
    },
    /// Terminal frame of an object fetch.
    ObjectEnd {
        size: u64,
    },
    // -- M11 change control (§5) --------------------------------------------
    /// Retired: change control.
    Policies,
    /// Retired: change control.
    PolicyShow,
    /// Retired: change control.
    PolicyGate,
    /// Retired: change control.
    Approvals,
    /// Retired: change control.
    ApprovalShow,
    /// Answer to a `DryRun`.
    JobPreview(Box<JobPreview>),
    /// Retired: change control.
    ApprovalPending,
    /// Retired: change control.
    ApprovalProgress,
    /// Retired: change control.
    PolicyDenied,
    // -- RETIRED 2026-09-18: M12 the host inventory ------------------------
    /// Retired: the host inventory.
    Hosts,
    /// Retired: the host inventory.
    HostShow,
    /// Retired: the host inventory.
    HostGroups,
    /// Retired: the host inventory.
    HostImported,
    /// Retired: the host inventory.
    HostChecks,
    /// Retired: the host inventory.
    HostInventory,
    // -- M18 facts (§4.9) ---------------------------------------------------
    Facts(Vec<FactRow>),
    FactCollectors(Vec<FactCollectorRow>),
    FactsDiff(Vec<FactDiffRow>),
    /// Answer to `FactsQuery`: the subjects that matched, with the value
    /// the expression was compared against so a reader can see *why*.
    FactsMatched {
        expression: String,
        /// `(subject name, the value that matched)`, in subject order.
        subjects: Vec<(String, String)>,
        /// Subjects that have facts but no such key. Reported rather than
        /// silently absent: "nine of your six hundred boxes do not say what
        /// kernel they run" is the answer somebody needs before patching.
        unknown: Vec<String>,
    },
    // -- M19 results (§5.4) -------------------------------------------------
    /// One target's answer, streamed just before its terminal frame when
    /// the caller asked for outputs.
    ///
    /// A *row*, not log text: `FanChunk` carries what the job printed and
    /// this carries what it answered. Sent for a single-target run as well
    /// as a fan-out, so `--format json` emits one array either way.
    Result(Box<ResultRow>),
    /// Terminal summary of an `--expect` assertion over a stream's rows.
    /// Sent after the last `Result` and before the run's own terminal
    /// frame; a non-empty `failed` is what makes the command exit non-zero.
    Expected {
        expression: String,
        passed: u32,
        failed: Vec<String>,
    },
    /// Answer to `IpcRequest::Results`.
    Results(Box<ResultSet>),
    /// Answer to `IpcRequest::ParallelRecord`.
    ParallelRecord(Box<crate::parallel::ParallelRecord>),
    /// Answer to `IpcRequest::ResultsExport`.
    ResultSets(Vec<ResultSet>),
    // -- M17 the fabric (§3.3) ----------------------------------------------
    /// Answer to `IpcRequest::Sessions`.
    Sessions(Vec<SessionRow>),
    /// Answer to `IpcRequest::SessionPolicies`.
    SessionPolicies(Vec<SessionPolicyRow>),
    // -- M23 enrolment (§3.2) -----------------------------------------------
    /// Answer to `IpcRequest::Enrolments`.
    Enrolments(Vec<crate::EnrolmentRow>),
    // -- M24 definition distribution (§3.1, §3.2) ---------------------------
    /// One node's reading during a prefetch, streamed as it arrives.
    PrefetchProgress {
        node: String,
        progress: crate::BundleProgress,
        /// Set when this node could not be asked at all — unreachable, or
        /// too old to understand the request. Not a failure of the bundle:
        /// it will fetch lazily when it needs the content, as it always
        /// did.
        unreachable: Option<String>,
    },
    /// Terminal frame of a prefetch: how it ended, per node.
    PrefetchDone {
        complete: u32,
        fetching: u32,
        skipped: u32,
    },
    /// Answer to `IpcRequest::Definitions`.
    Definitions(Vec<DefinitionRow>),
    /// Something the caller should know that did not stop the work. Rides
    /// a stream alongside the frames that carry it, and every client that
    /// does not recognise it may ignore it — which is why it says nothing
    /// load-bearing.
    Notice {
        message: String,
    },
    /// Retired: quotas and the usage report.
    QuotaDenied,
    /// Retired: quotas and the usage report.
    Quotas,
    /// Answer to `IpcRequest::Queue`.
    Queue(Vec<crate::quota::QueueRow>),
    /// Retired: quotas and the usage report.
    Usage,
    /// Retired: quotas and the usage report.
    UsageRows,
    /// Answer to `IpcRequest::StateShow` and `IpcRequest::StateCompact`
    /// (C0 §4.3) — the "ops before and after" a compaction pass reports.
    StateStats(StateStats),
    /// Answer to `IpcRequest::UpgradePlans`.
    UpgradePlans(Vec<UpgradePlanRow>),
    /// Answer to `IpcRequest::ReleaseKeys`.
    ReleaseKeys(ReleaseKeyView),
    /// Answer to `IpcRequest::Releases`.
    Releases(Vec<ReleaseRow>),
    /// One node's step of a fleet upgrade, streamed as it happens.
    UpgradeProgress {
        node: String,
        /// `asked` | `upgrading` | `back` | `already` | `failed` | `skipped`
        state: String,
        /// The sentence a person reads: what happened, or why it stopped.
        detail: String,
    },
    /// Terminal frame of a fleet upgrade.
    ///
    /// A partial upgrade is a **supported state**: F0's forward-compatible
    /// op-log exists precisely so a mesh mid-upgrade keeps working, and a
    /// node on an older minor is reported as `stale` rather than broken. So
    /// this frame reports where the rollout got to; it does not unwind.
    UpgradeDone {
        /// Nodes seen **back on the target, alive and caught up**. Never a
        /// node that was merely asked: a rollout that counts intentions as
        /// outcomes is a rollout nobody can trust.
        upgraded: u32,
        already: u32,
        /// The driving node started upgrading *itself*, last, and is on its
        /// way down. It is deliberately not counted as upgraded — it cannot
        /// verify itself, and the honest report is "confirm afterwards".
        driver_restarting: bool,
        /// Nodes never reached: the rollout stopped at the first failure,
        /// loudly, and these were behind it.
        remaining: u32,
        /// Set when the rollout stopped early. Naming the node, because
        /// "it failed" is not something anybody can act on.
        stopped_at: Option<String>,
    },
    /// Answer to `IpcRequest::EnrolStatus`.
    EnrolStatus {
        state: crate::EnrolmentState,
        /// The code to read aloud, while there is a live request.
        code: Option<String>,
        expires_at_micros: Option<u64>,
        /// Who said no, when they did.
        declined_by: Option<String>,
    },
    // -- C2 deployments (C0-C9 §6) -----------------------------------------
    /// Answer to `IpcRequest::Deployments`.
    Deployments(Vec<crate::deployment::DeploymentRow>),
    /// Answer to `IpcRequest::DeploymentShow`.
    DeploymentShow(Box<crate::deployment::DeploymentRow>),
    /// Answer to `IpcRequest::DeploymentStatus`.
    DeploymentStatus(Box<crate::deployment::DeploymentStatusView>),
    /// Answer to `IpcRequest::Schedules` (N1 P5).
    Schedules(Vec<crate::schedule::ServiceScheduleRow>),
    /// Answer to `IpcRequest::ScheduleShow`.
    ScheduleShow(Box<crate::schedule::ServiceScheduleRow>),
    /// Answer to a dry-run `IpcRequest::SchedulePut`.
    ScheduleCheck(Box<crate::schedule::ServiceScheduleCheck>),
    /// Answer to `IpcRequest::Ports` (N2 P1).
    Ports(Box<crate::ports::PortLedgerView>),
    /// Answer to `IpcRequest::Registry` (N3 P1).
    Registry(Vec<crate::registry::RegistryRow>),
    /// Answer to `IpcRequest::RegistryShow`.
    RegistryShow(Vec<crate::registry::RegistryInstanceRow>),
    /// Answer to `IpcRequest::DnsStatus` (N3 P3).
    DnsStatus(Box<crate::registry::DnsStatusView>),
    /// Answer to `IpcRequest::Volumes` (N5).
    Volumes(Vec<crate::volumes::VolumeRow>),
    /// Answer to a dry-run `IpcRequest::DeploymentPut`.
    DeploymentCheck(Box<crate::deployment::DeploymentCheckView>),
    // -- C4 gangs (C0-C9 §8) -----------------------------------------------
    /// Answer to `IpcRequest::Gangs`.
    Devices(Vec<crate::device::DeviceRow>),
    Gangs(Vec<crate::gang::GangRow>),
    // -- C7 action cache (C0-C9 §11) ----------------------------------------
    /// Answer to [`IpcRequest::CacheStats`].
    CacheStats(Box<crate::cache::CacheStats>),
    /// Answer to `run --explain-key`: the action key and every field that
    /// produced it. Terminal — nothing is submitted.
    CacheKey(Box<crate::cache::KeyExplain>),
    /// A streamed job was answered from the cache (§11.2: *a hit is visible
    /// or it is untrustworthy*). Sent before the log frames it stands in
    /// for, so a client can badge the output it is about to print rather
    /// than retroactively.
    CacheHit(Box<crate::cache::CacheHit>),
    /// A spot-check disagreed with the entry it was checking. The loudest
    /// frame in the protocol, and the only one whose existence is a bug
    /// report.
    CacheMismatch {
        key: crate::ActionKey,
        detail: String,
    },
    // -- C8 legibility (C0-C9 §12) ------------------------------------------
    /// Answer to [`IpcRequest::JobExplain`].
    JobExplain(Box<JobExplainView>),
    /// Answer to [`IpcRequest::Plan`].
    Plan(Box<crate::placement::PlanEstimate>),
    /// Answer to `IpcRequest::GangShow`.
    GangShow(Box<crate::gang::GangView>),
    /// A gang reached the point where every member started at once. Sent
    /// once, before the first log byte, because "it is held, waiting for
    /// the rest" and "it is running" are the two states a person watching a
    /// gang cares about and only the origin can tell them apart.
    GangStarted {
        gang: crate::GangId,
        /// Rank-ordered, so `members[i]` is rank `i` and the index on the
        /// `FanChunk` frames that follow is the member's rank. The gang
        /// reuses the fan-out's multiplexed log framing because it is the
        /// same problem — many jobs, one stream, attribution per line — and
        /// a second framing would be a second renderer to keep correct.
        members: Vec<FanTarget>,
        rounds: u32,
    },
    /// The offer phase is still running: `held` of `size` reservations are
    /// in hand, and this is round `round`. Emitted once per round so a
    /// `--follow` that is waiting shows *why* it is waiting.
    GangWaiting {
        gang: crate::GangId,
        held: u32,
        size: u32,
        round: u32,
        /// The senior gang the last refusal named, when there was one.
        behind: Option<crate::GangId>,
    },
    /// Terminal frame of a gang submission.
    GangDone {
        gang: crate::GangId,
        outcome: crate::gang::GangOutcome,
        succeeded: u32,
        failed: u32,
    },
    // -- M9 §6.3 -----------------------------------------------------------
    //
    // Appended at the tail, for the reason spelled out on
    // [`IpcRequest::Totals`]: a postcard discriminant is positional, and
    // renumbering responses breaks every client that has not been rebuilt
    // in lockstep — the pinned order first, since it carries its own copy of
    // this schema.
    /// Answer to [`IpcRequest::Totals`].
    Totals(Box<Totals>),
    /// Output from one target of a *fanned-out workflow step*, attributed
    /// by node name. The fan-out sibling of [`IpcResponse::RunChunk`]:
    /// a single-target step's output arrives as `RunChunk`, but a fan-out
    /// dispatches one job per node and a run watcher needs to know *whose*
    /// bytes these are — a benchmark screen showing "which kernel on which
    /// node" is the case this exists for.
    ///
    /// Streamed only for steps that opt in via the
    /// `MESHWARE_STREAM` env key (see `mesh-core`): following forty nodes'
    /// logs across the mesh is a real cost, and most fan-outs have no
    /// watcher who wants them live — their output is captured at the end
    /// through the ordinary results path either way.
    ///
    /// `stream`: 1 = stdout, 2 = stderr.
    RunFanChunk {
        step: String,
        node: String,
        stream: u8,
        data: Vec<u8>,
    },
    /// One instalment of an [`IpcRequest::BundleFileFetch`], in manifest
    /// order. Each frame is one content-store chunk, so no frame can
    /// approach [`crate::MAX_FRAME_LEN`] however large the file is.
    BundleFileChunk {
        data: Vec<u8>,
    },
    /// Terminal frame of an [`IpcRequest::BundleFileFetch`]: the size the
    /// manifest declared, which the caller compares against what it
    /// actually received. A stream that ends without this frame was cut
    /// short, and a caller that cannot tell those apart will happily write
    /// half a binary to disk and run it.
    BundleFileDone {
        size: u64,
    },
    /// Answer to [`IpcRequest::OpCensus`] (M31 P0). Boxed: the per-kind
    /// breakdown is a `Vec` of one row per entity in the mesh, and every
    /// other variant here would otherwise grow to match it.
    OpCensus(Box<crate::OpCensus>),
    /// Answer to [`IpcRequest::RetentionShow`] (M31 A1).
    Retention(Box<RetentionView>),
    /// Answer to [`IpcRequest::Sinks`] (M31 B1).
    Sinks(Vec<SinkRow>),
    /// Answer to [`IpcRequest::SinkTest`]. `status` is the destination's
    /// own words, verbatim — the whole point of pressing the button.
    SinkTest {
        ok: bool,
        status: String,
    },
    /// Sent in a log stream **instead of chunks**, before `JobDone`, when
    /// the bytes are not readable (M31 A2).
    ///
    /// A frame rather than an `Error`, and that is the whole point: an
    /// error ends the exchange and tells the reader nothing survived, while
    /// this says *which* of expired / unavailable / elsewhere happened and
    /// is still followed by a `JobDone` carrying the job's real state. The
    /// record is complete; only the payload is gone.
    LogsUnavailable {
        availability: crate::OutputAvailability,
    },
    /// Answer to [`IpcRequest::JobsQuery`] (**H-1**). Boxed for the reason
    /// [`OpCensus`](Self::OpCensus) is: it carries a `Vec` of rows and
    /// every other variant would otherwise be sized to match.
    JobsPage(Box<JobsPage>),
    /// Answer to [`IpcRequest::StateVersion`] (**H-8**).
    StateVersions(StateVersions),
    /// The admin-assigned half of one node's tags, and who last set it.
    ///
    /// Appended at the tail, for the reason `IpcRequest` gives: a variant's
    /// position *is* its encoding.
    AssignedTags {
        tags: Vec<String>,
        by: Option<UserId>,
        at_micros: u64,
    },
    // -- Y5 hardware presence, appended at the tail -------------------------
    /// Retired: change control.
    PolicyPresences,
    /// Retired: change control.
    PresenceReadiness,
    /// Retired: change control.
    UserTokens,
    // -- Y2/Y3 hardware identity, appended at the tail ----------------------
    AttestRoots(Vec<AttestRootRow>),
    // -- N5 P2 dynamic volumes, appended at the tail ------------------------
    /// Answer to `IpcRequest::VolumeCreate`: the volume now offered.
    VolumeCreated(crate::volumes::VolumeRow),
    /// Answer to `IpcRequest::VolumeRemove`. Carries a sentence rather than
    /// replying `Ok`, because a forced removal succeeded *and* left
    /// something behind, and the operator has to be told which they got.
    VolumeRemoved {
        message: String,
    },
    // -- N6 §12 mesh-wide load shedding, appended at the tail ---------------
    /// Answer to `IpcRequest::ShedStatus` and `ShedSet`.
    Shed(Box<ShedView>),
    // -- T4 runtimes, appended at the tail ---------------------------------
    /// Answer to [`IpcRequest::Runtimes`].
    Runtimes(Box<crate::runtimes::RuntimesView>),
    // -- Phase B E3/U1 instances, appended at the tail ---------------------
    /// Answer to [`IpcRequest::DeploymentInstance`].
    DeploymentInstance(Box<crate::DeploymentInstanceDetail>),
    /// Answer to [`IpcRequest::DeploymentInstanceStats`].
    DeploymentInstanceStats(Vec<crate::InstanceStatRow>),
    // -- Phase B E1 history, appended at the tail --------------------------
    /// Answer to [`IpcRequest::DeploymentHistory`].
    DeploymentHistory(Box<crate::DeploymentHistoryView>),
    // -- Phase C U8, appended at the tail ------------------------------------
    /// Answer to [`IpcRequest::PlacementMap`].
    PlacementMap(Vec<crate::PlacementNodeRow>),
    // -- SEC-5, appended at the tail ------------------------------------------
    /// Answer to [`IpcRequest::ClockAhead`].
    ClockAhead(Vec<ClockAheadRow>),
    // -- SEC-17, appended at the tail -----------------------------------------
    /// Answer to [`IpcRequest::Invites`].
    Invites(Vec<InviteRow>),
    // -- SEC-7, appended at the tail ------------------------------------------
    /// Answer to [`IpcRequest::SecretScopes`].
    SecretScopes(Vec<SecretScopeRow>),
    /// Answer to [`IpcRequest::SecretRecipients`].
    SecretRecipients(Vec<NodeId>),
    // -- Director as a view (2026-09), appended at the tail -------------------
    /// Answer to [`IpcRequest::ServiceCurrent`].
    ServiceCurrent(Box<ServiceVersion>),
    // -- mesh reset, appended at the tail --------------------------------------
    /// Answer to [`IpcRequest::MeshReset`]: who was handed the reset.
    MeshReset(Box<MeshResetReport>),
}

/// What `meshnode reset` could tell the rest of the mesh, peer by peer.
///
/// Every member but the node that published the reset, whatever became of
/// it: the founder is about to lose the only view of this mesh there is,
/// and a node left out of this list is a machine they would find out about
/// weeks later, still in a mesh that no longer exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshResetReport {
    pub mesh_name: String,
    pub peers: Vec<MeshResetPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshResetPeer {
    pub node_id: NodeId,
    pub name: String,
    pub outcome: MeshResetOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MeshResetOutcome {
    /// It holds the reset and will act on it.
    Delivered,
    /// It holds the reset, and its meshnode is below
    /// [`crate::MESH_RESET_MINOR`]: it will carry it and stay.
    TooOld,
    /// It could not be reached; the reason, as the transport gave it.
    Unreachable(String),
}

/// One secret's scope, for a person to read (SEC-7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretScopeRow {
    pub name: String,
    pub nodes: String,
    /// `username` where known, else the short id.
    pub users: Vec<String>,
    pub roles: Vec<String>,
    pub by: String,
    pub at_micros: u64,
}

/// One invite, as `meshnode invite ls` shows it (SEC-17). The token itself
/// never replicates; this is its public record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteRow {
    pub invite_id: InviteId,
    pub created_by_user: UserId,
    pub created_by_username: String,
    pub expires_at_micros: u64,
    pub uses: u32,
    pub used: u32,
    pub revoked: bool,
    pub tags: Vec<String>,
}

/// A member whose ops this node is holding because they were dated too far
/// ahead of its clock (SEC-5). Either that machine's clock is wrong or this
/// one's is; the row says how far apart they are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClockAheadRow {
    pub node_id: NodeId,
    pub name: String,
    /// How far ahead of this node's clock the latest held op was dated.
    pub ahead_micros: u64,
}

/// The mesh-wide shed, as an operator reads it (N6 §12).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShedView {
    pub submissions: bool,
    pub reconcile: bool,
    pub reason: String,
    /// Who last set it, and when. Absent when nobody ever has.
    pub by: Option<UserId>,
    pub by_name: Option<String>,
    pub at_micros: u64,
    /// How much of the mesh will actually honour it — the readout that
    /// matters, because this is the one capability gate that fails open.
    pub readiness: crate::shed::ShedReadiness,
}

/// One sink, as `meshnode sinks` shows it (M31 B1 item 9).
///
/// Reads like `meshnode cache stats`, and the columns are chosen against
/// L1–L5's rule that a number nobody can see is a number nobody trusts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SinkRow {
    pub name: String,
    pub kind: String,
    /// Where it goes, as an operator reads it: a URL, a path, a plugin.
    pub target: String,
    /// The destination's own database or index prefix, when it has one.
    ///
    /// Separate from `target` because they are different things and
    /// conflating them produced `CREATE TABLE https://host:8443.runs` the
    /// first time `sinks ddl` ran against a real record.
    #[serde(default)]
    pub database: String,
    pub tables: Vec<String>,
    pub enabled: bool,
    /// The selector that narrows which nodes may export this.
    ///
    /// On the row because a caller reconstructing a sink from a listing —
    /// `meshnode sinks test` does exactly that — otherwise rebuilds it with
    /// a blank `run_on` and silently widens the pool. For a storage probe
    /// that is not cosmetic: it would check the whole mesh and report on a
    /// sink nobody asked about.
    #[serde(default)]
    pub run_on: String,
    /// The node currently elected to export this, if any. Empty when no
    /// live member matches `run_on` — which is a real state and a common
    /// one, and the reason it is a column rather than an assumption.
    pub elected: String,
    /// Whether *this* node is the elected exporter.
    pub elected_here: bool,
    /// Whether the counters below are **this sink's** state or merely this
    /// node's copy of nothing.
    ///
    /// The cursor, the spool and the last status are local files under
    /// `<home>/sinks/<name>/` on whichever node exports. On every other
    /// node they read back as a pristine zero row — so a sink that has been
    /// failing every thirty seconds for an hour looks perfectly healthy
    /// from four machines out of five, and an operator who happens to ask
    /// the wrong one is told nothing is wrong.
    ///
    /// False means "these numbers are not this sink's; `elected` names the
    /// node that has them". A renderer must not draw them as a reading.
    #[serde(default)]
    pub status_is_local: bool,
    /// **Lag in the data's own clock**: the newest terminal row minus the
    /// cursor, in seconds.
    ///
    /// The data's clock rather than wall time since the last successful
    /// write, so it is comparable across sinks and still meaningful when
    /// nothing has run for an hour. A sink on an idle mesh is not behind.
    pub lag_secs: u64,
    pub sent: u64,
    pub spooled_bytes: u64,
    pub dropped: u64,
    /// The last status, **verbatim** — `503`, `connection refused`, the
    /// plugin's own stderr. "Sink unhealthy" sends an operator to the wrong
    /// machine; `503` sends them to the right one.
    pub last_status: String,
    pub last_at_micros: u64,
    /// This node's data came out of a backup, so nothing is exporting until
    /// an admin runs `meshnode sinks resume` (§19 Q5). On the row so the
    /// table says *why* a healthy-looking sink is not moving.
    pub paused_by_restore: bool,
    /// Payloads exported, and payloads skipped for being over the cap
    /// (M31 B2 item 5). The second number is what tells an operator their
    /// cap is doing something, and is otherwise invisible.
    #[serde(default)]
    pub payloads_sent: u64,
    #[serde(default)]
    pub payloads_too_large: u64,
    /// Days this sink holds deletion back, and records dropped anyway
    /// because that ceiling passed (§19 Q3).
    ///
    /// **`forced_gaps` is the number that must be said out loud.** Every
    /// one is a row that will never reach the destination, and an operator
    /// finding that out from a query six months later is the outcome the
    /// counter exists to prevent.
    #[serde(default)]
    pub hold_for_export_days: u32,
    #[serde(default)]
    pub forced_gaps: u64,
    /// What a person can get back out of this destination (§14.4). On the
    /// row rather than in a document, so it cannot drift from the code.
    pub archives: bool,
    pub browse_records: bool,
    pub replay_payloads: bool,
    pub created_by: String,
    pub created_at_micros: u64,
    pub description: String,
}

/// The retention policy as an operator reads it, with the projection that
/// makes it a decision rather than a knob (M31 A1 item 5).
///
/// *"This mesh writes 18 MB/day of surviving ops; with these settings the
/// log settles at ~1.6 GB"* — computed from P0's census and shown **before**
/// the policy is signed. A retention policy nobody can predict the effect of
/// is a retention policy nobody changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RetentionView {
    /// The live policy itself, carried so a client editing one window need
    /// not reconstruct the others from the rendered rows.
    ///
    /// The alternative — parsing `90d` back out of the display strings —
    /// is lossy in exactly the direction that matters: a floor rendered as
    /// `None` and a floor of zero are the same on screen and different in
    /// the record, and an edit that round-tripped through the display would
    /// silently rewrite windows nobody named.
    pub policy: crate::retention::RetentionPolicy,
    pub windows: Vec<RetentionRow>,
    /// Who signed the live policy and when, or `None` for the shipped
    /// defaults. The two are different sentences and only one of them means
    /// somebody decided.
    pub signed_by: Option<String>,
    pub signed_at_micros: u64,
    /// What the op-log holds now, and where it is heading.
    pub oplog_bytes: u64,
    /// Permanent bytes per day at the current rate — ops no window will
    /// ever retire. Growth that retention **cannot** fix, and the number
    /// that makes the audit trail's cost visible.
    pub permanent_bytes_per_day: u64,
    /// Bytes per day of closed records, which the `runs` window does retire.
    pub retirable_bytes_per_day: u64,
    /// Where the log settles under the current policy, or `None` when
    /// `runs` is `forever` — in which case it does not settle, and saying
    /// so is more useful than printing a number.
    pub settles_at_bytes: Option<u64>,
}

/// What this node is holding back, and why (M31 A3).
///
/// **Reported, never inferred.** A screen that showed a limit without
/// showing whether anything had hit it would be a setting; this is a
/// diagnosis, and it is only interesting when it is non-empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackpressureView {
    /// Ops a minute one entity may author here.
    pub limit_per_minute: u32,
    /// Entities that have been refused, worst first.
    pub entities: Vec<PressureRow>,
    /// Fact collectors held back for flapping (M18), carried alongside
    /// because they are the same question — *what is churning* — asked of
    /// the one entity kind that had an answer before A3 generalised it.
    pub flapping: Vec<String>,
}

/// One entity under pressure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PressureRow {
    /// The compaction key, as the log itself names the entity: `job:…`,
    /// `run:…`, `fact:…`, or `kind:…` for the grow-only ops that have no
    /// key of their own.
    pub entity: String,
    pub observed_per_minute: u32,
    pub refused_total: u64,
    /// Non-zero while auto-paused, and it expires by itself — a brake, not
    /// a tombstone.
    pub paused_until_micros: u64,
}

/// One table's window, resolved.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionRow {
    pub table: String,
    /// The mesh-wide window, as an operator types it: `90d`, `1y`,
    /// `forever`.
    pub window: String,
    /// The floor, when one is set.
    pub floor: Option<String>,
    /// What this node actually applies after its own config is taken into
    /// account — which may be *shorter*, never longer (Q8).
    pub effective_here: String,
    /// Whether this window is what bounds the replicated op-log, as opposed
    /// to local disk or a directory the operator owns: `runs`, and `audit`
    /// once it is finite (ENG-3).
    pub bounds_the_op_log: bool,
}

/// One session policy, as `sessions policy ls` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPolicyRow {
    pub id: String,
    pub description: String,
    pub users: String,
    pub nodes: String,
    /// `shell`, `shell-privileged`.
    pub allow: Vec<String>,
    pub expires_at_micros: Option<u64>,
    pub created_at_micros: u64,
    pub created_by_username: String,
    /// True when this policy covers the node answering the question — the
    /// answer to "why can I not get a prompt on *this* box".
    pub covers_this_node: bool,
}

/// One live session, from either end (M17 §3.5).
///
/// Not a `JobSummary` and deliberately not replicated: a session is live or
/// it is gone, and what survives it is the audit entry. This is the answer
/// to "what is open on this node right now", which a drain needs and an
/// operator wants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    /// Node-local and monotonic. A session is never named anywhere else, so
    /// it needs no global id and gets none.
    pub id: u64,
    /// `shell` | `exec` | `cp` | `forward`.
    pub kind: String,
    /// What it is doing, one line.
    pub detail: String,
    /// True when this node is the one running the process / holding the
    /// file / dialling the port; false when it is the initiator.
    pub hosting: bool,
    /// The node at the other end.
    pub peer: NodeId,
    pub peer_name: String,
    /// The human who opened it.
    pub user: UserId,
    pub username: String,
    /// Why the operator said they opened it.
    pub reason: String,
    /// The OS account the shell is running as on the host.
    pub os_user: String,
    pub opened_at_micros: u64,
    /// Last traffic in either direction — what the idle timeout measures.
    pub last_active_micros: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// One target of a fan-out submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanTarget {
    pub job_id: JobId,
    pub node_id: NodeId,
    pub node_name: String,
}

// ---------------------------------------------------------------------------
// Op-log figures (C0)
// ---------------------------------------------------------------------------

/// What the replicated log costs on one node, and what compacting it would
/// (or did) recover.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateStats {
    /// Ops in the log, every author included.
    pub ops_total: u64,
    /// Ops this node authored — the only ones it may ever compact.
    pub ops_mine: u64,
    /// Sequence numbers this node has already dropped and vouched for.
    pub pruned_mine: u64,
    /// Sequence numbers dropped across the whole mesh, as far as this node
    /// has heard. A peer that has not compacted contributes nothing.
    pub pruned_mesh: u64,
    /// What a pass would drop right now.
    pub droppable_now: u64,
    /// Superseded, but younger than the horizon — the "come back later"
    /// number.
    pub held_by_horizon: u64,
    pub horizon_secs: u64,
    /// Usage windows still read, and windows held in memory that have aged
    /// out (C0 P3). The second number is what the next pass releases.
    pub usage_live: u64,
    pub usage_expired: u64,
    pub bytes_on_disk: u64,
    /// Set on a `StateCompact` answer: what the pass actually dropped, and
    /// the seq of the checkpoint that vouches for it. Zero on `StateShow`
    /// and on a dry run.
    pub dropped: u64,
    pub checkpoint_seq: u64,
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// The op-log census (M31 P0)
// ---------------------------------------------------------------------------
//
// Here rather than in `mesh_core::compact` because it crosses IPC: `doctor`,
// `meshnode state show` and Director all read it out of the daemon. The
// *computation* stays in `compact.rs`, beside the compaction rules it has to
// agree with — a census that disagreed with what a pass would actually drop
// would be worse than no census.

/// One entity kind's contribution to the log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpKindCount {
    /// The `OpBody` variant name, or [`crate::UNKNOWN_OP_KIND`].
    pub kind: String,
    pub ops: u64,
    pub bytes: u64,
    /// Of those, how many would survive compaction indefinitely — the
    /// current value of a live entity, or an op that never compacts at all.
    ///
    /// **This is the number that predicts next year**, and reporting it
    /// beside the total is the whole point of the census. A single total
    /// hides the fact that C0 is working while the log still grows: a mesh
    /// doing ten thousand jobs a day writes thirty thousand `JobUpsert`
    /// ops and keeps ten thousand of them for ever, and only one of those
    /// two numbers is the wall.
    pub surviving: u64,
    /// Ops of this kind authored in the last the census window (a day), and
    /// how many of those survive. The growth rate, per entity.
    pub ops_last_day: u64,
    pub surviving_last_day: u64,
}

/// What is actually in the op-log, by entity (M31 P0).
///
/// Computed by scanning the log rather than by keeping counters, and that is
/// deliberate: a counter is a second source of truth that drifts, and this
/// is read by `doctor`, by `meshnode state show` and by a cached `/metrics`
/// gauge — never on a hot path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpCensus {
    pub ops_total: u64,
    /// Envelope bytes: body plus signature. Close to but not equal to
    /// [`OpCensus::disk_bytes`], which carries redb's own overhead — both are
    /// reported because the difference is the operator's, not ours.
    pub bytes_total: u64,
    pub disk_bytes: u64,
    /// Ops that no compaction pass would ever drop, at any horizon.
    pub surviving_total: u64,
    /// Ops authored in the last day, and the surviving share of them.
    pub ops_last_day: u64,
    pub surviving_last_day: u64,
    /// Bytes authored in the last day. The number `doctor` turns into
    /// "grows ~4 MB/day".
    pub bytes_last_day: u64,
    /// Oldest and newest HLC wall instants seen. Zero on an empty log.
    pub oldest_micros: u64,
    pub newest_micros: u64,
    /// Per entity, descending by op count.
    pub by_kind: Vec<OpKindCount>,
    /// Ops this build could not decode (F0). Counted rather than omitted,
    /// so the total still matches the disk.
    pub undecodable: u64,
}

impl OpCensus {
    /// Surviving bytes per day, extrapolated from the last day's ops.
    ///
    /// Surviving rather than total, because the total is what the log costs
    /// *this week* and the surviving share is what it costs for ever. An
    /// operator sizing a year of history wants the second one.
    pub fn surviving_bytes_per_day(&self) -> u64 {
        if self.ops_last_day == 0 {
            return 0;
        }
        // Mean envelope size over the window, times the surviving count.
        // Per-kind sizes differ by an order of magnitude, so this is an
        // estimate and is named as one wherever it is printed.
        let mean = self.bytes_last_day / self.ops_last_day.max(1);
        mean.saturating_mul(self.surviving_last_day)
    }

    /// Where the log would settle at a retention window of `days`, at the
    /// current surviving rate. `0` days means "never drops anything", and
    /// the honest answer there is that it does not settle.
    pub fn settles_at_bytes(&self, days: u64) -> Option<u64> {
        (days > 0).then(|| self.surviving_bytes_per_day().saturating_mul(days))
    }

    /// The kinds worth naming, largest first.
    pub fn top_kinds(&self, n: usize) -> &[OpKindCount] {
        &self.by_kind[..self.by_kind.len().min(n)]
    }
}

#[cfg(test)]
mod tests {
    use super::{env_matches_signed, is_loader_env};

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// SEC-4: a job's environment is what the user signed, plus or minus
    /// meshware's own keys — and nothing else.
    #[test]
    fn a_job_env_must_be_the_signed_one_give_or_take_reserved_keys() {
        let signed = pairs(&[("A", "1"), ("B", "2"), ("MESHWARE_TASK_CACHE", "1")]);
        // Reordered, reserved keys added and removed: the same request.
        let job = pairs(&[("MESHWARE_CACHE_KEY", "k"), ("B", "2"), ("A", "1")]);
        assert!(env_matches_signed(&job, &signed));
        // Anything else is not.
        for bad in [
            pairs(&[("A", "1"), ("B", "2"), ("LD_PRELOAD", "/tmp/x.so")]),
            pairs(&[("A", "1"), ("B", "3")]),
            pairs(&[("A", "1")]),
            pairs(&[("A", "1"), ("B", "2"), ("B", "2")]),
            pairs(&[("A", "1"), ("B", "2"), ("PATH", "/tmp/evil")]),
        ] {
            assert!(!env_matches_signed(&bad, &signed), "{bad:?}");
        }
    }

    #[test]
    fn loader_variables_are_named() {
        for key in [
            "LD_PRELOAD",
            "BASH_FUNC_ls%%",
            "BASH_ENV",
            "SHELLOPTS",
            "GLIBC_TUNABLES",
        ] {
            assert!(is_loader_env(key), "{key}");
        }
        // `ENV` is read by POSIX shells only when interactive, and is a
        // common application variable besides.
        for key in ["LD_LIBRARY_PATH", "PATH", "PYTHONPATH", "ENV", "MESHWARE_X"] {
            assert!(!is_loader_env(key), "{key}");
        }
    }

    use super::*;

    fn totals() -> Totals {
        Totals {
            nodes_known: 4,
            nodes_live: 4,
            nodes_draining: 0,
            cores: 68,
            cores_from: 4,
            slots: 68,
            slots_busy: 2,
            mem_mb: 319_488,
            mem_from: 4,
            mem_available_mb: 0,
            mem_available_from: 0,
            disk_mb: 4_299_161,
            disk_from: 4,
            disk_free_mb: 0,
            disk_free_from: 0,
            devices: 3,
            devices_busy_here: 0,
            jobs_running: 2,
            jobs_queued: 0,
            arches: Vec::new(),
            load_per_core: None,
            gpus: 3,
            vram_mb: 73_728,
            capacity_from: 4,
        }
    }

    /// L2 P1's exit demo, as a string comparison. The copy is the
    /// deliverable (§10.2), so it gets a test rather than a review.
    #[test]
    fn strip_is_the_spec_sheet() {
        assert_eq!(
            totals().strip(),
            "4 machines · 68 cores · 312 GB · 3 GPUs (72 GB VRAM) · 4.1 TB poolable disk · 2 jobs running"
        );
    }

    /// D-L2.2: a figure nothing reported is absent, never zero. A mesh with
    /// no accelerators says nothing about GPUs rather than claiming none.
    #[test]
    fn unknown_is_absent_not_zero() {
        let mut t = totals();
        t.gpus = 0;
        t.vram_mb = 0;
        t.mem_from = 0;
        t.disk_from = 0;
        t.jobs_running = 0;
        assert_eq!(t.strip(), "4 machines · 68 cores");
    }

    /// An integrated GPU routinely reports no memory. "(0 GB VRAM)" reads
    /// as a broken card; saying nothing reads as an unread one.
    #[test]
    fn a_gpu_with_no_readable_memory_says_only_the_count() {
        let mut t = totals();
        t.gpus = 1;
        t.vram_mb = 0;
        assert!(t.strip().contains("1 GPU ·"), "{}", t.strip());
    }

    #[test]
    fn one_machine_is_singular() {
        let mut t = totals();
        t.nodes_live = 1;
        assert!(t.strip().starts_with("1 machine · "), "{}", t.strip());
    }

    /// The coverage line appears exactly when the totals are a floor, and
    /// never otherwise — a qualifier on every line trains people to stop
    /// reading qualifiers.
    #[test]
    fn coverage_shows_only_when_it_is_not_everybody() {
        let mut t = totals();
        assert_eq!(t.coverage(), None);
        t.capacity_from = 3;
        assert_eq!(t.coverage().as_deref(), Some("3 of 4 machines reporting"));
    }

    #[test]
    fn sizes_read_as_prose() {
        assert_eq!(size_mb(512), "0.5 GB");
        assert_eq!(size_mb(8192), "8.0 GB");
        assert_eq!(size_mb(319_488), "312 GB");
        assert_eq!(size_mb(4_299_161), "4.1 TB");
    }
}

#[cfg(test)]
mod ipc_peek {
    use super::*;

    /// **M-16**: the accept loop peeks this discriminant instead of
    /// decoding the whole request, so a drift between the constant and the
    /// variant would route session opens down the request/response path —
    /// and a terminal would simply never work.
    #[test]
    fn session_open_index_matches_the_variant() {
        let encoded = postcard::to_stdvec(&IpcRequest::SessionOpen {
            node: String::new(),
            kind: crate::SessionKind::Shell {
                command: None,
                term: String::new(),
                cols: 0,
                rows: 0,
            },
            reason: String::new(),
        })
        .expect("encode");
        assert_eq!(ipc_request_index(&encoded), Some(IPCREQ_SESSION_OPEN_INDEX));
    }

    /// And it does not match anything else — a peek that said "yes" too
    /// often would hijack ordinary requests.
    #[test]
    fn other_requests_do_not_look_like_a_session_open() {
        for req in [IpcRequest::Status, IpcRequest::Jobs, IpcRequest::WhoAmI] {
            let encoded = postcard::to_stdvec(&req).expect("encode");
            assert_ne!(ipc_request_index(&encoded), Some(IPCREQ_SESSION_OPEN_INDEX));
        }
    }

    #[test]
    fn a_truncated_payload_is_not_a_session_open() {
        assert_ne!(ipc_request_index(&[]), Some(IPCREQ_SESSION_OPEN_INDEX));
    }
}

/// **Never renumber** (DOC-8).
///
/// The comments in this file have asked for new variants at the tail since
/// the session-open peek made the index load-bearing, and nothing held
/// anybody to it: the workflow excision renumbered `SessionOpen` from 124
/// to 95.
///
/// **The original reason was the Python SDK**, which encoded a request by
/// position from outside this build, so a shifted index was a *different
/// request* that decoded perfectly. The SDK went on 2026-09-20 and both
/// ends of this socket are one build again.
///
/// The pin stays, for the reason that outlived it: several indices in this
/// file are *peeked* rather than decoded — `IPCREQ_SESSION_OPEN_INDEX`
/// above is read from the first varint to route a frame before anything
/// has parsed it — and a constant that silently stops matching its variant
/// is a routing bug with no error message. Pinning the whole order is the
/// cheapest thing that catches that, and it is why the retired variants
/// below are tombstones rather than deletions.
#[cfg(test)]
mod ipc_order {
    /// The top-level variant names of `enum`, in declaration order, read
    /// from this file's own source.
    fn declared(name: &str) -> Vec<String> {
        let src = include_str!("ipc.rs");
        let open = format!("pub enum {name} {{");
        let mut out = Vec::new();
        let mut inside = false;
        for line in src.lines() {
            if !inside {
                inside = line.starts_with(&open);
                continue;
            }
            if line.starts_with('}') {
                break;
            }
            let Some(rest) = line.strip_prefix("    ") else {
                continue;
            };
            if rest.starts_with(|c: char| c.is_ascii_uppercase()) {
                let ident: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                out.push(ident);
            }
        }
        out
    }

    fn pinned(text: &str) -> Vec<String> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect()
    }

    fn check(name: &str, file: &str, pinned_text: &str) {
        let declared = declared(name);
        let pinned = pinned(pinned_text);
        assert!(!declared.is_empty(), "could not read `{name}` from ipc.rs");
        for (index, (was, is)) in pinned.iter().zip(&declared).enumerate() {
            assert_eq!(
                was, is,
                "`{name}` variant {index} was `{was}` and is now `{is}`. A variant \
                 never moves: leave a tombstone where one is retired, and add new \
                 ones at the end (ipc-order/{file})"
            );
        }
        assert!(
            declared.len() >= pinned.len(),
            "`{name}` lost its last variant(s) {:?} — leave tombstones instead (ipc-order/{file})",
            &pinned[declared.len()..]
        );
        assert_eq!(
            declared.len(),
            pinned.len(),
            "`{name}` has new variants {:?}: append them to ipc-order/{file}",
            &declared[pinned.len()..]
        );
    }

    #[test]
    fn requests_never_renumber() {
        check(
            "IpcRequest",
            "request.txt",
            include_str!("../ipc-order/request.txt"),
        );
    }

    #[test]
    fn responses_never_renumber() {
        check(
            "IpcResponse",
            "response.txt",
            include_str!("../ipc-order/response.txt"),
        );
    }
}
