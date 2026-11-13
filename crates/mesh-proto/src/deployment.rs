//! C2 — deployments: work that is supposed to stay up (C0–C9 §6).
//!
//! A [`Deployment`] is a **replicated, user-signed entity of its own**, in
//! the exact shape [`crate::Schedule`] has and for the same reasons: it
//! outlives the node that created it many times over, it is owned by
//! whichever live node rendezvous-hashing picks for its *name* rather than
//! by its origin, and its authority is re-derived per instance from this
//! record's own signature rather than carried in a stored envelope. The
//! word "service" was already a versioned *definition* in this codebase
//! (`content::ServiceVersion`), so the running, replicated thing is a
//! deployment: *N instances of service version V should be running on nodes
//! matching selector S* (§6.2).
//!
//! # An instance is a job
//!
//! Nothing in this file describes a process. A running instance is an
//! ordinary job record with a **derived** id ([`crate::JobId::derive_instance`]),
//! which means streaming, cancel, history, artifacts, sandboxing, secrets
//! and adoption all work with no new code — the reconciler's entire job is
//! to notice that the count of non-terminal instances is not the desired
//! count and submit or cancel accordingly (§6.2). That is why this milestone
//! is small, and why this module is types and arithmetic rather than a
//! supervisor.
//!
//! # Why every field here rides F0
//!
//! `DeploymentPut`/`DeploymentRemove`/`DeploymentHealthPut` are appended
//! op-body variants, and `Op.body` is length-delimited bytes the signature
//! already covers — so a node on an older minor stores, verifies, counts
//! and forwards a deployment op of a kind it has never heard of rather than
//! wedging sync (§2.1, F0). C2 therefore needs no protocol bump.

use serde::{Deserialize, Serialize};

use crate::{JobId, NodeId, UserId};

/// Job environment key naming the deployment an instance belongs to.
///
/// The executor reads it to know a job is a supervised instance — which
/// deployment's health check to run, and which deployment/generation to
/// publish [`InstanceHealth`] under. Engine-owned, set on the instance
/// spec, never authored by a service definition.
pub const DEPLOY_ENV: &str = "MESHWARE_DEPLOYMENT";
/// Job environment key carrying the instance's generation, as a decimal
/// string.
pub const DEPLOY_GEN_ENV: &str = "MESHWARE_DEPLOYMENT_GENERATION";
/// Job environment key carrying the instance's slot label (a replica index
/// or a node name), for legibility in logs and the process table.
pub const DEPLOY_SLOT_ENV: &str = "MESHWARE_DEPLOYMENT_SLOT";

// ---------------------------------------------------------------------------
// Desired count
// ---------------------------------------------------------------------------

/// How many instances a deployment wants running.
///
/// An enum rather than a signed integer with a magic value, so `system` —
/// one instance per matching node, which is how the mesh hosts its own
/// relays, collectors and exporters (§6.2) — is a variant the type checker
/// enforces the two readings of, not a `-1` a reader has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Count {
    /// Exactly `n` instances, placed by the ordinary scheduler across the
    /// nodes the selector matches.
    Fixed(u32),
    /// One instance on **every** node the selector matches, tracking the
    /// set as nodes join and leave. There is no replica index: the node
    /// *is* the identity.
    System,
}

impl Count {
    pub fn as_display(&self) -> String {
        match self {
            Count::Fixed(n) => n.to_string(),
            Count::System => "system".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Restart policy (C2 P2)
// ---------------------------------------------------------------------------

/// Exponential backoff with a failure budget (§6.2).
///
/// A supervisor that restarts forever turns one broken service into a
/// mesh-wide fork bomb, so the budget is not optional: exceed
/// `budget_failures` failures inside `budget_window_secs` and the
/// deployment goes `degraded`, raises an event, and **stops restarting**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartPolicy {
    /// First retry delay, in seconds. Doubles each consecutive failure.
    pub backoff_min_secs: u64,
    /// The ceiling the doubling saturates at.
    pub backoff_max_secs: u64,
    /// How many instance failures inside the window trip `degraded`.
    pub budget_failures: u32,
    /// The rolling window the budget is counted over, in seconds.
    pub budget_window_secs: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        // 1s..5m backoff, 5 failures in 10m — the sketch's defaults (§6.3).
        RestartPolicy {
            backoff_min_secs: 1,
            backoff_max_secs: 300,
            budget_failures: 5,
            budget_window_secs: 600,
        }
    }
}

impl RestartPolicy {
    /// The delay before the `attempt`-th restart (0-based): `min · 2^attempt`
    /// clamped to `max`. Saturating throughout — a service that has failed
    /// forty times does not overflow the schedule, it sits at the ceiling.
    pub fn backoff_secs(&self, attempt: u32) -> u64 {
        let shifted = self
            .backoff_min_secs
            .checked_shl(attempt.min(63))
            .unwrap_or(u64::MAX);
        shifted.clamp(
            self.backoff_min_secs,
            self.backoff_max_secs.max(self.backoff_min_secs),
        )
    }
}

// ---------------------------------------------------------------------------
// Health checks (C2 P3)
// ---------------------------------------------------------------------------

/// How an instance's health is judged (§6.2).
///
/// Evaluated **on the executing node**, never from a central prober: there
/// is no central anything, and a check run from elsewhere measures the
/// network more than the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthKind {
    /// Run `command` in the instance's runtime; exit 0 is healthy.
    Exec,
    /// Open a TCP connection to `127.0.0.1:port`; a completed handshake is
    /// healthy.
    Tcp,
    /// GET `http://127.0.0.1:port{path}`; a 2xx is healthy.
    Http,
}

impl HealthKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            HealthKind::Exec => "exec",
            HealthKind::Tcp => "tcp",
            HealthKind::Http => "http",
        }
    }

    pub fn parse(s: &str) -> Option<HealthKind> {
        match s {
            "exec" => Some(HealthKind::Exec),
            "tcp" => Some(HealthKind::Tcp),
            "http" => Some(HealthKind::Http),
            _ => None,
        }
    }
}

/// A health check definition. Fields not relevant to `kind` are ignored
/// (an `http` check reads `path`+`port`; a `tcp` check reads `port`; an
/// `exec` check reads `command`) — validated at parse time so a nonsense
/// combination is refused where a human is watching, not on the executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCheck {
    pub kind: HealthKind,
    /// `exec` only: the command line, run through the instance's runtime.
    pub command: String,
    /// `http` only: the request path, e.g. `/healthz`.
    pub path: String,
    /// `tcp`/`http`: the loopback port to reach the service on.
    pub port: u16,
    /// How often the check runs.
    pub interval_secs: u64,
    /// How long after an instance starts before failures count. A service
    /// that takes 20s to warm up must not be killed at second 10.
    pub grace_secs: u64,
    /// How long one probe may take before it counts as a failure.
    pub timeout_secs: u64,
    /// Consecutive failures (past the grace) before the instance is
    /// declared unhealthy and eligible for replacement.
    pub failures_to_unhealthy: u32,
}

impl Default for HealthCheck {
    fn default() -> Self {
        HealthCheck {
            kind: HealthKind::Http,
            command: String::new(),
            path: "/healthz".to_string(),
            port: 0,
            interval_secs: 10,
            grace_secs: 30,
            timeout_secs: 5,
            failures_to_unhealthy: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Update strategy (C2 P4)
// ---------------------------------------------------------------------------

/// How a version change rolls out (§6.2).
///
/// Only `Rolling` exists, and it is an enum rather than a bool so a future
/// `Canary`/`BlueGreen` is an appended variant, not a format migration —
/// the same reason [`crate::Trigger`] is an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateStrategy {
    Rolling,
}

impl UpdateStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            UpdateStrategy::Rolling => "rolling",
        }
    }
}

/// The rolling-update policy: how many instances may be down at once, and
/// whether the next batch waits on the new instances passing health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePolicy {
    pub strategy: UpdateStrategy,
    /// The most instances that may be simultaneously unavailable during a
    /// roll. `1` rolls forty instances one at a time.
    pub max_unavailable: u32,
    /// Gate each batch on the new instances reporting healthy. With no
    /// health check defined this degrades to "gate on running", which is
    /// the honest best a deployment without a probe can offer.
    pub gate_on_health: bool,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        UpdatePolicy {
            strategy: UpdateStrategy::Rolling,
            max_unavailable: 1,
            gate_on_health: true,
        }
    }
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// A named, replicated declaration that N instances of a service version
/// should be running on nodes matching a selector (§6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    /// Unique. The handle every command, audit entry and instance
    /// correlation uses.
    pub name: String,
    /// The service definition to run. Always a *pinned* version — a
    /// deployment names `collector@7`, never a floating head, because
    /// "what is supposed to stay up" cannot be a moving target and a
    /// rollout is an explicit re-sign of this record, not a head bump
    /// happening underneath it.
    pub service: String,
    pub service_version: u32,
    /// The desired instance count.
    pub count: Count,
    /// The node selector, in the grammar [`crate::content`] callers already
    /// use (`tag=edge`, `name=anvil|name=edge-7`, `fact.os_id=debian`).
    /// Stored as the **string**, parsed at each use site, so a new selector
    /// form is free and degrades closed on an old node (M18 §4.7).
    pub targets: String,
    pub restart: RestartPolicy,
    /// `None` when the deployment declares no health check — instances are
    /// then judged live purely by whether their job is non-terminal.
    pub health: Option<HealthCheck>,
    pub update: UpdatePolicy,
    /// Bumped every time `service_version` changes. Instances derive their
    /// job id from it ([`crate::JobId::derive_instance`]), so a rollout
    /// produces a fresh set of ids the reconciler rolls toward one batch at
    /// a time, and a rollback (generation up, version back) is just another
    /// forward roll to an older, immutable version.
    pub generation: u32,
    /// Inputs handed to the service, validated against its contract at
    /// create time — the point of a deployment is that nobody is watching
    /// when an instance restarts, so a schema error has to surface while
    /// somebody is.
    pub inputs: Vec<(String, String)>,
    /// A paused deployment stays in state, keeps its instances' history,
    /// and reconciles nothing — it neither starts nor stops instances.
    /// Pausing is an edit, so it re-signs the record.
    pub paused: bool,
    pub description: String,
    /// The user whose authority every instance of this deployment carries.
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`],
    /// verified at merge time on every node **and again on every executor
    /// at every instance start** (the deployment analogue of a schedule
    /// fire, §6.2).
    pub signature: Vec<u8>,
}

impl Deployment {
    /// The bytes the creator's signature covers: the whole record with the
    /// signature blanked. Everything is covered, `paused`, `generation` and
    /// `service_version` included — a rollout and a pause are both edits
    /// only the owner may make, and re-signing is what makes that provable.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-deployment-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// `service` as a human writes it: `collector@7`.
    pub fn service_ref(&self) -> String {
        format!("{}@{}", self.service, self.service_version)
    }
}

/// Signing bytes for a [`crate::OpBody::DeploymentRemove`]. Distinct domain
/// tag from the record, so a `Put` signature can never be replayed as a
/// `Remove`.
pub fn deployment_remove_signing_bytes(name: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-deployment-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&by.0);
    buf
}

/// Deployment names share the rules host and schedule names use: something
/// a person types on a command line and reads back in a table.
pub fn is_valid_deployment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

// ---------------------------------------------------------------------------
// Instance health, replicated per node (C2 P3)
// ---------------------------------------------------------------------------

/// One instance's health as its executor sees it.
///
/// The executor is the only node that can judge health (§6.2), so it is the
/// author. The reconciler — which may be a different node — reads this to
/// decide whether an unhealthy instance needs replacing and whether a
/// rolling batch may advance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceHealth {
    pub job_id: JobId,
    pub deployment: String,
    /// The generation this instance belongs to, so a reconciler gating a
    /// roll can tell a healthy *new* instance from a healthy *old* one.
    pub generation: u32,
    /// True once the instance has passed a check (or, with no check
    /// defined, once its job is running past the grace). False while it is
    /// starting, or after `failures_to_unhealthy` consecutive failures.
    pub healthy: bool,
    /// When this state was entered — how "unhealthy for 30s" is answered
    /// without a second timestamp stream.
    pub since_micros: u64,
    /// A short human reason, e.g. `http 503` or `starting`.
    pub detail: String,
}

/// Every deployment instance one node currently runs, with its health.
///
/// **One record per node, LWW, republished only when the set or a health
/// bit changes** — exactly the shape [`crate::NodeReservations`] has, and
/// for the same reason: what a reconciler needs is "which of my instances
/// are healthy", the node already knows its own answer, and one op per node
/// that compacts to one live op per node keeps this off the per-job write
/// path C0 exists to bound. An instance that ends is dropped from the vec
/// on the next publish, so the record is self-trimming rather than
/// grow-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInstanceHealth {
    pub node: NodeId,
    pub instances: Vec<InstanceHealth>,
    /// When the node last recomputed this. Guards the LWW merge and lets a
    /// reader discount a node that has gone silent.
    pub published_at_micros: u64,
}

// ---------------------------------------------------------------------------
// Row / view types for the CLI and Director
// ---------------------------------------------------------------------------

/// One deployment, as a list or detail view sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRow {
    pub name: String,
    /// `collector@7`, as written.
    pub service: String,
    pub count: String,
    pub targets: String,
    pub generation: u32,
    /// `active`, `paused`, `degraded`, or `updating`.
    pub status: String,
    pub desired: u32,
    pub running: u32,
    pub healthy: u32,
    pub created_by_username: String,
    pub created_at_micros: u64,
    /// The record as YAML, present on a detail view so an editor and
    /// `--output yaml` render from one fetch — the `ScheduleRow::source`
    /// convention.
    pub source: Option<String>,
}

/// One instance in a deployment's status view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentInstanceRow {
    pub job_id: JobId,
    /// The replica index for a fixed-count deployment; the node name for a
    /// `system` one.
    pub slot: String,
    pub node: String,
    pub generation: u32,
    /// `queued`, `running`, `succeeded`, `failed`, `cancelled`, `timed-out`.
    pub state: String,
    /// `healthy`, `unhealthy`, `starting`, or `unknown` (no check / no
    /// report yet).
    pub health: String,
    pub health_detail: String,
    pub started_at_micros: Option<u64>,
}

/// The answer to `deployments status <name>`: desired vs running vs healthy,
/// the roll's progress if one is in flight, and the per-instance breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentStatusView {
    pub name: String,
    pub service: String,
    pub count: String,
    pub targets: String,
    pub generation: u32,
    pub status: String,
    pub desired: u32,
    pub running: u32,
    pub healthy: u32,
    /// The node currently elected to reconcile this deployment, if any is
    /// live — the legibility answer to "who is watching this".
    pub reconciler: Option<String>,
    /// Instances still on a prior generation, when a roll is in flight.
    pub updating_from: Option<u32>,
    /// A one-line reason when `status` is `degraded`.
    pub degraded_reason: Option<String>,
    pub instances: Vec<DeploymentInstanceRow>,
    /// Set while the canary set is up and still proving itself (N1 P3):
    /// new instances are running beside the old ones and nothing has been
    /// cut over. A reader needs this to tell "half rolled" from "rolling
    /// carefully on purpose", which look identical in the counts.
    pub canarying: bool,
    /// The canaries have settled and hold for `deployment promote` (Phase
    /// B, E2). Always alongside `canarying`.
    pub awaiting_promotion: bool,
    /// Set when the roll has made no progress for `progress_deadline`
    /// (N1 P3). Distinct from `degraded_reason`: a stalled roll may have
    /// nothing crashing at all.
    pub stalled_reason: Option<String>,
    /// Live members that would reconcile this deployment **without** its
    /// policy, because they are below `DEPLOY_POLICY_MINOR` (N1 P3/P4).
    ///
    /// Empty unless the deployment actually has a policy to lose: a mesh
    /// full of old nodes and no canary anywhere is not a problem, and a
    /// warning that fires on every mixed mesh is a warning nobody reads.
    /// Non-empty means the reconciler election could land on one of these
    /// and roll the whole thing at once.
    pub policy_blind: Vec<String>,
    /// The port labels this deployment's instances ask for (N2 P2).
    ///
    /// **What was declared, not what was allocated.** A dynamic port is
    /// decided on the executor at spawn, and `JobSummary` is an op body
    /// that cannot grow to carry it back — so the number an instance
    /// actually bound is node-local truth until N3's service registry
    /// replicates it. `meshnode ports` shows one node's ledger in full;
    /// this column says what to expect.
    ///
    /// Saying "http, metrics" and meaning "asked for" is honest. Showing an
    /// address column that was filled in for local instances and blank for
    /// remote ones would not be.
    pub port_labels: Vec<String>,
    /// Why nothing can be placed, when nothing can be (N1 P2).
    ///
    /// `Some` only when the mesh has **no** feasible node for this
    /// deployment right now — not merely fewer than `count`, because three
    /// replicas are perfectly happy to share one node. A deployment that
    /// is simply mid-roll carries `None`, so a reader can treat any value
    /// here as a real refusal rather than as progress.
    pub placement: Option<crate::PlacementRefusal>,
    /// Instances whose node this node does not currently hear as Alive
    /// (Phase A, U4).
    ///
    /// Their rows read `unreachable` rather than the health their node last
    /// published, because that health is the one fact about them that can
    /// no longer be updated. Appended, like every field added to a view.
    pub unreachable: u32,
    /// [`availability`] for this deployment: what happens when a node
    /// running one of its instances goes away (Phase A, U6).
    pub availability: String,
    /// The runtime its instances run under — `container`, `bash`, … — so a
    /// front end offers a shell *inside* an instance only where there is a
    /// container to open one in (Phase B, E3). Empty when the service
    /// version is not published here.
    pub runtime: String,
}

/// The answer to a dry-run `DeploymentPut` — what the record parsed to and
/// what it would do, so a mistake surfaces before anything is published.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentCheckView {
    pub name: String,
    pub service: String,
    pub count: String,
    pub targets: String,
    /// Nodes the selector matches right now — a `count` bigger than this,
    /// or a selector matching nothing, is worth seeing before publishing.
    pub matching_nodes: u32,
    pub health: Option<String>,
    pub issues: Vec<String>,
    /// The same refusal `status` carries, answered before publishing
    /// rather than after (N1 P2). A dry run that says "matches 7 nodes"
    /// and then places nothing because all seven are 128M short was
    /// telling the truth and still sending somebody into a wall.
    pub placement: Option<crate::PlacementRefusal>,
    /// [`availability`] for the policy this file produces — said before
    /// publishing, including the defaults nobody typed (Phase A, U6).
    pub availability: String,
    /// What publishing this would change, field by field, against what is
    /// published now (Phase B, E5). Empty on a first publish and when
    /// nothing changed; `first_publish` and `rolls` say which.
    pub changes: Vec<PlanChange>,
    /// Publishing this starts a new generation: every instance is replaced,
    /// in batches, under the rollout policy. False for a change that only
    /// adjusts how the *next* roll behaves, or changes nothing.
    pub rolls: bool,
    /// Nothing by this name is published yet.
    pub first_publish: bool,
}

/// One field a dry run found changed (Phase B, E5) — a line of Nomad's
/// `job plan` diff, for a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanChange {
    /// Dotted path in the file: `health.path`, `network.port.http.to`.
    pub field: String,
    /// As published; empty when the field is new.
    pub from: String,
    /// As in this file; empty when the field is removed.
    pub to: String,
    /// [`PLAN_ROLLS`], [`PLAN_KNOB`] or [`PLAN_RECORD`].
    pub effect: String,
}

/// A change that replaces every instance (Phase B, E5).
pub const PLAN_ROLLS: &str = "rolls";
/// A change to *how* rolls happen: applies to the next one and restarts
/// nothing (Phase B, E5).
pub const PLAN_KNOB: &str = "rollout knob";
/// A change to the record alone — a description, a pause (Phase B, E5).
pub const PLAN_RECORD: &str = "record only";
/// A change to a fixed count: instances are added or retired at the same
/// generation, and none that keeps running restarts (Phase B, E3).
pub const PLAN_SCALE: &str = "scale";
/// A change to where instances may go and which machines they prefer
/// (Phase C): applies to the next placement, and moves nothing running.
pub const PLAN_PLACEMENT: &str = "placement";

/// Live resource use of one running instance, as its executor measured it
/// just now (Phase B, E3) — `deployment status --stats`, and the instance
/// panel's sparklines.
///
/// **A measurement, never replicated.** It travels only as the answer to a
/// question (`Request::InstanceStats`): a number that changes every second
/// has no business in the op log (`facts.rs` — properties, not
/// measurements).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceStat {
    pub job: JobId,
    /// Percent of one core, so two busy cores read 200.
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
    /// The ceiling the instance runs under, when there is one.
    pub mem_limit_bytes: Option<u64>,
    /// Where the numbers came from: `container` (the engine's own stats),
    /// `cgroup` (the job's whole process tree) or `process` (its main process
    /// only, so a lower bound). Empty when nothing could be measured.
    pub source: String,
}

/// One incarnation of an instance slot, oldest first (Phase B, U1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncarnationRow {
    pub job_id: JobId,
    pub generation: u32,
    pub incarnation: u32,
    pub node: String,
    pub state: String,
    pub created_at_micros: u64,
    pub started_at_micros: Option<u64>,
    pub finished_at_micros: Option<u64>,
    pub exit_code: Option<i32>,
}

/// One instance slot up close (Phase B, E3 and U1) — `deployment instance`,
/// and Director's instance panel: every incarnation it has had, its health
/// now, and what it is using.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentInstanceDetail {
    pub deployment: String,
    pub slot: String,
    pub generation: u32,
    pub runtime: String,
    /// The current generation's live incarnation, when there is one.
    pub current: Option<DeploymentInstanceRow>,
    pub incarnations: Vec<IncarnationRow>,
    /// `None` when nothing is running to measure, or its node could not be
    /// asked.
    pub stats: Option<InstanceStat>,
}

/// One instance's live use, for `deployment status --stats` (Phase B, E3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatRow {
    pub slot: String,
    pub node: String,
    pub stat: Option<InstanceStat>,
}

/// One machine in the placement map (Phase C, U8) — `meshnode deployment
/// placement` and Director's Placement panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementNodeRow {
    pub node: crate::NodeId,
    pub name: String,
    /// `alive`, `suspect` or `dead`, as the node answering judges it.
    pub liveness: String,
    /// What the machine advertises; 0 when it advertises nothing.
    pub cores: u32,
    pub mem_mb: u64,
    /// Takes new work: not `no-execute`, not held back by an admin, not
    /// draining.
    pub eligible: bool,
    pub draining: bool,
    /// Draining because it is shutting down, rather than being emptied.
    pub leaving: bool,
    /// When a drain stops whatever is left, in unix seconds. `None` while
    /// draining is no deadline.
    pub drain_deadline_secs: Option<u64>,
    pub instances: Vec<PlacementInstanceRow>,
}

/// One deployment instance, on the machine running it (Phase C, U8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementInstanceRow {
    pub deployment: String,
    pub slot: String,
    pub generation: u32,
    pub state: String,
    /// `healthy`, `unhealthy` or `unknown`.
    pub health: String,
    /// What the instance asked for; 0 when it asked for nothing.
    pub cpus: u32,
    pub mem_mb: u64,
}

// ---------------------------------------------------------------------------
// Phase B, E1 — a deployment's versions, and names for the ones worth keeping
// ---------------------------------------------------------------------------

/// How many versions of each deployment every node keeps (Phase B, E1).
///
/// A version is a generation. Compaction holds the newest record per
/// generation for twice this many generations per author, and state shows
/// the newest ten of those; the slack is what lets every node show the
/// same ten however the publishes were spread across daemons.
pub const DEPLOYMENT_HISTORY_KEEP: usize = 10;

/// Everything published at one generation (Phase B, E1): the record and the
/// side-car beside it.
///
/// **Both halves, or a rollback restores half a file.** Ports, volumes,
/// resources and the operator's environment live in the policy's
/// `instance_env`, not in the deployment, so a version that kept only the
/// record would bring back an old image with today's ports. No signature of
/// its own: each half carries the one it was published with, and those are
/// what a node checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentVersion {
    pub deployment: Deployment,
    pub policy: DeploymentPolicy,
}

/// A name on one version of a deployment, which keeps it (Phase B, E1).
///
/// **It carries the version itself**, not a generation number. A tag has to
/// outlive the ten versions a deployment keeps — that is what it is for —
/// and a node that joins after the tagged generation was compacted away
/// would otherwise hold a name for nothing. `set_by`'s signature is what
/// authorizes it; a rollback to it is re-signed by whoever rolls back.
///
/// LWW per `(deployment, tag)` by HLC. `version: None` removes the tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentTag {
    pub deployment: String,
    pub tag: String,
    pub version: Option<DeploymentVersion>,
    pub description: String,
    pub set_by: UserId,
    pub set_at_micros: u64,
    pub signature: Vec<u8>,
}

impl DeploymentTag {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-deployment-tag-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// A tag is a word somebody types: letters, digits, `.`, `-` and `_`, up to
/// 64 of them.
pub fn is_valid_tag_name(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// One version in `deployment history` (Phase B, E1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentVersionRow {
    pub generation: u32,
    /// As published at this generation — the latest publish of it, when a
    /// scale or a pause re-published it without rolling.
    pub version: DeploymentVersion,
    pub published_by_username: String,
    pub published_at_micros: u64,
    pub tags: Vec<String>,
    pub current: bool,
    /// Older than the ten kept, and listed only because a tag names it.
    pub kept_by_tag: bool,
    /// Against the next older version listed; empty for the oldest.
    pub changes: Vec<PlanChange>,
    /// Rendered as `show --output toml` renders the current one.
    pub source: String,
}

/// One tag, with the exact version it names (Phase B, E1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentTagRow {
    pub tag: String,
    pub generation: u32,
    pub description: String,
    pub set_by_username: String,
    pub set_at_micros: u64,
    pub version: DeploymentVersion,
}

/// The answer to `deployment history` (Phase B, E1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentHistoryView {
    pub deployment: String,
    /// Newest first.
    pub versions: Vec<DeploymentVersionRow>,
    pub tags: Vec<DeploymentTagRow>,
}

// ---------------------------------------------------------------------------
// Phase B, E2 — promotion by a person, for a deployment that asks for it
// ---------------------------------------------------------------------------

/// The `update.auto_promote` knob.
pub const KNOB_AUTO_PROMOTE: &str = "auto_promote";

/// Per-deployment settings that arrived after [`DeploymentPolicy`] stopped
/// being able to grow (Phase B, E2).
///
/// **Pairs, so the next setting needs no op of its own.** The policy is a
/// positional record under a signature, and every field it will ever have
/// was fixed when it shipped; a node that does not know a key here simply
/// carries it. Published only by a whole file — a scale, a pause or a
/// rollback leaves the knobs as they are.
///
/// LWW per deployment by HLC, signed by a user who may edit the deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentKnobs {
    pub deployment: String,
    /// One entry per key. Empty is every default.
    pub entries: Vec<(String, String)>,
    pub set_by: UserId,
    pub set_at_micros: u64,
    pub signature: Vec<u8>,
}

impl Default for DeploymentKnobs {
    fn default() -> Self {
        DeploymentKnobs {
            deployment: String::new(),
            entries: Vec::new(),
            set_by: UserId([0u8; 32]),
            set_at_micros: 0,
            signature: Vec::new(),
        }
    }
}

impl DeploymentKnobs {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"mw-deployment-knobs-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Canaries cut over once they settle — true unless a file said
    /// `auto_promote = false`.
    pub fn auto_promote(&self) -> bool {
        self.get(KNOB_AUTO_PROMOTE) != Some("false")
    }
}

/// A person's go-ahead for one generation's canaries (Phase B, E2).
///
/// **It names the generation**, so it cannot carry over to the next roll: a
/// canary set nobody has looked at is not promoted by one somebody did.
/// LWW per deployment by HLC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentPromotion {
    pub deployment: String,
    pub generation: u32,
    pub by: UserId,
    pub at_micros: u64,
    pub signature: Vec<u8>,
}

impl DeploymentPromotion {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(b"mw-deployment-promote-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

// ---------------------------------------------------------------------------
// N1 P3/P4 — the rollout and partition policy, carried beside the deployment
// ---------------------------------------------------------------------------

/// What to do with an instance whose node came back (N1 P4).
///
/// An enum rather than a pair of bools because the four answers are
/// genuinely disjoint and a reader should not have to work out what
/// `keep_original && best_score` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reconcile {
    /// The instance that was there before the partition wins. The default,
    /// and the conservative answer: it has been serving all along from its
    /// own side, and its state is the state somebody has been talking to.
    KeepOriginal,
    /// The replacement wins. The right answer when the far side was
    /// unreachable *to clients too*, so the original has been serving
    /// nobody.
    KeepReplacement,
    /// Whichever node the scheduler would pick today.
    BestScore,
    /// Whichever instance has been up longest — the crudest proxy for
    /// "has the most warm state", and the one Nomad's users actually ask
    /// for.
    LongestRunning,
}

impl Reconcile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reconcile::KeepOriginal => "keep_original",
            Reconcile::KeepReplacement => "keep_replacement",
            Reconcile::BestScore => "best_score",
            Reconcile::LongestRunning => "longest_running",
        }
    }

    pub fn parse(s: &str) -> Option<Reconcile> {
        match s.trim() {
            "keep_original" => Some(Reconcile::KeepOriginal),
            "keep_replacement" => Some(Reconcile::KeepReplacement),
            "best_score" => Some(Reconcile::BestScore),
            "longest_running" => Some(Reconcile::LongestRunning),
            _ => None,
        }
    }
}

/// The things a deployment declares that cannot live in the signed record:
/// how a rollout may proceed (N1 P3), what happens to work on the far side
/// of a partition (N1 P4), and which ports its instances need (N2 P2).
///
/// ## Why this is a record beside `Deployment` and not fields inside it
///
/// [`Deployment`] is signed, and postcard encodes positionally, so growing
/// it — or growing [`UpdatePolicy`], which sits in its middle — does two
/// unrecoverable things at once. Measured, not assumed:
///
/// - **Every existing signature breaks.** [`Deployment::signing_bytes`] is
///   postcard over the whole struct, so one more field changes the bytes
///   every deployment ever published was signed over.
/// - **Every existing record stops decoding.** A build with the new field
///   reading a record written without it fails with
///   `DeserializeUnexpectedEnd` — the deployment does not degrade, it
///   *disappears* from state on upgrade. In the other direction an older
///   node reads the new field's bytes as `generation`, which is worse than
///   an error because it is silent.
///
/// This is the argument [`crate::OpBody::ApprovalDecisionV2`] and
/// [`crate::OpBody::PolicyPresencePut`] already make about `ApprovalDecision`
/// and `Policy`, and this is its third application. A side-car record gives
/// the degradation the track actually asked for: an older node carries the
/// deployment exactly as it does today and rolls it the old way, which is
/// the door-left-open failure `doctor` reports by name.
///
/// Defaults are today's behaviour throughout, so a deployment with no
/// policy record behaves precisely as it did before this existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentPolicy {
    /// The deployment this attaches to. Need not exist yet — a policy
    /// naming no deployment governs nothing, the way a presence
    /// requirement naming no policy does.
    pub deployment: String,

    // -- P3: the rollout ---------------------------------------------------
    /// Place N new instances **without stopping any old ones**. The old
    /// set keeps serving until the canaries are promoted, so a bad version
    /// costs nothing but the capacity to run it.
    ///
    /// `canary == count` is blue/green, and needs no separate strategy:
    /// the whole new set comes up beside the whole old one.
    pub canary: u32,
    /// On rollout failure — a missed `progress_deadline` — point
    /// `service_version` back at the **newest earlier version whose own
    /// rollout never stalled** (Phase A, §2.5a). Two bad versions in a row
    /// no longer revert onto each other; when every earlier version stalled
    /// too, it goes one back and says so.
    ///
    /// Cheap here, and that is not luck: a service version is already
    /// immutable and already signed, so a revert is a pointer move rather
    /// than a rebuild.
    pub auto_revert: bool,
    /// How long the rollout may make **no progress** before it is failed.
    /// The clock resets on every instance that reaches healthy, so forty
    /// instances rolling one at a time never trip a deadline sized for
    /// one. `0` means no deadline. A file that sets `auto_revert` or
    /// `canary` and names none gets [`DEFAULT_PROGRESS_DEADLINE_SECS`] from
    /// the parser, because either one without a deadline cannot finish.
    pub progress_deadline_secs: u64,
    /// How long an instance must stay healthy before it counts as healthy.
    /// Catches the process that passes one probe and dies. `0` means the
    /// first passing probe counts, which is today's behaviour.
    pub min_healthy_secs: u64,

    // -- P4: the partition -------------------------------------------------
    /// How long an unreachable node's instances stay `unknown` before they
    /// are considered lost.
    ///
    /// **The field this whole product has been missing.** Meshware
    /// traverses CGNAT and survives partitions by design, and then gives
    /// the operator no say in what happens to work on the far side:
    /// liveness is mesh-wide constants, Suspect at 46 s and Dead at 51 s,
    /// and a deployment inherits them whether it is a web server or a
    /// four-hour render. `0` means **never**: the instance waits for its
    /// node however long it is gone (`lost_after: never` in a file). A
    /// fixed-count file with no `disconnect` block gets
    /// [`DEFAULT_LOST_AFTER_SECS`] from the parser instead — this comment
    /// used to say zero meant "lost as soon as the node is Dead", which is
    /// what Nomad's zero means and what this one never did.
    pub lost_after_secs: u64,
    /// Start a replacement while the original is still `unknown`, rather
    /// than waiting for `lost_after` to expire. Costs double capacity for
    /// the duration of the partition and buys back the availability.
    pub replace_unknown: bool,
    /// Which instance wins when the partition heals and both are running.
    pub reconcile: Reconcile,

    // -- the reserved environment every instance carries --------------------
    /// The `MESHWARE_TASK_*` entries this deployment's instances are
    /// started with: ports (N2), service registration and drain timings
    /// (N3), and whatever later phases declare.
    ///
    /// **A map, not a field per feature, and that is the point.** N2 added
    /// `ports: Vec<PortRequest>` here and N3 would have added three more
    /// keys beside it — at which point every phase grows a signed struct
    /// and the rule against doing that becomes a rule nobody can follow.
    ///
    /// Every one of those things is *already* a reserved job-environment
    /// entry by the time it reaches an executor: that is how
    /// `meshnode run --port`, a service's own `--env` and a deployment
    /// block all arrive at the same place. Carrying the entries themselves
    /// makes this struct stop growing — N4's container config and N5's
    /// volume claims are entries, not fields — and it makes a deployment's
    /// instances and an equivalent `meshnode run` provably identical,
    /// because they are the same list.
    ///
    /// Validated at parse time, where a bad value can name the block it
    /// came from. Nothing reads this that does not read the same keys off
    /// an ad-hoc job.
    pub instance_env: Vec<(String, String)>,

    // -- provenance --------------------------------------------------------
    pub set_by: UserId,
    pub set_at_micros: u64,
    pub signature: Vec<u8>,
}

impl Default for DeploymentPolicy {
    /// Exactly today's behaviour, so an absent record and a default record
    /// are the same deployment.
    fn default() -> Self {
        DeploymentPolicy {
            deployment: String::new(),
            canary: 0,
            auto_revert: false,
            progress_deadline_secs: 0,
            min_healthy_secs: 0,
            lost_after_secs: 0,
            replace_unknown: false,
            reconcile: Reconcile::KeepOriginal,
            instance_env: Vec::new(),
            set_by: UserId([0u8; 32]),
            set_at_micros: 0,
            signature: Vec::new(),
        }
    }
}

impl DeploymentPolicy {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"mw-deployment-policy-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// Does this say anything at all?
    ///
    /// A policy that asks for nothing is how one is lifted, and it has to
    /// read as "no policy" everywhere rather than as "a policy of zeroes".
    pub fn is_inert(&self) -> bool {
        self.canary == 0
            && !self.auto_revert
            && self.progress_deadline_secs == 0
            && self.min_healthy_secs == 0
            && self.lost_after_secs == 0
            && !self.replace_unknown
            && self.reconcile == Reconcile::KeepOriginal
            && self.instance_env.is_empty()
    }

    /// The ports this deployment declares, recovered from the environment
    /// it carries — the same parse an executor does, so the two cannot
    /// disagree about what was asked for.
    pub fn ports(&self) -> crate::ports::PortRequests {
        crate::ports::PortRequests::from_env(&self.instance_env)
    }

    /// One reserved entry, if it is set.
    pub fn env_value(&self, key: &str) -> Option<&str> {
        self.instance_env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// What a fixed-count deployment's `lost_after` is when its file says
/// nothing (Phase A, `docs/NOMAD-ENHANCEMENTS.md` §2.1).
///
/// Roughly the mesh's Dead verdict (51 s on a silent drop) plus a tick.
/// **Written into the policy by the parser, never assumed by the
/// reconciler.** The side-car's `0` still means "never": that record is
/// signed and already on the wire, and an older node reading a newer
/// record must not find its meaning changed underneath it. So a file with
/// no `disconnect` block and one saying `lost_after: 60s` produce the same
/// record, and a deployment published before this existed keeps waiting
/// for its nodes until it is next put.
///
/// Nomad's zero means the opposite — lost at once — which is why the file
/// spells the wait-forever case as a word rather than a number.
pub const DEFAULT_LOST_AFTER_SECS: u64 = 60;

/// What `progress_deadline` is when `auto_revert` or `canary` is set and
/// the file names none (Phase A, §2.4). Nomad's default.
///
/// Without one a stalled roll is never declared failed, so `auto_revert`
/// has nothing to fire on and a canary set waits for a promotion that may
/// never come — both silently, which is how a file asking for a revert
/// shipped a revert that could not happen.
pub const DEFAULT_PROGRESS_DEADLINE_SECS: u64 = 600;

/// The reserved key a deployment's `[deployment.migrate]` travels in
/// (Phase C, E9). In `instance_env`, beside the placement rules, and like them
/// outside what a roll compares.
pub const REQ_MIGRATE: &str = "MESHWARE_TASK_MIGRATE";

/// The reserved key a deployment's `priority` travels in (Phase C, E10).
pub const REQ_PRIORITY: &str = "MESHWARE_TASK_PRIORITY";

/// A deployment's priority when its file names none. Nomad's.
pub const DEFAULT_PRIORITY: u8 = 50;

/// How far above a preemptible job an instance's priority must be to take
/// its place (Phase C, E10). Nomad's margin: a difference of a few points is
/// a preference, not a reason to kill somebody's work.
pub const PREEMPT_MARGIN: u8 = 10;

/// Set by the reconciler on a second offer of an instance every machine
/// refused as busy: this machine may make room by preempting (Phase C, E10).
///
/// **A request, never the authority.** An executor honours it only for a
/// start whose envelope it has verified against the deployment record, and
/// reads the priority from its own copy of that deployment's policy — so a
/// job that sets this key on itself preempts nothing.
pub const REQ_MAY_PREEMPT: &str = "MESHWARE_TASK_MAY_PREEMPT";

/// A deployment's priority, off its instance environment: 1 to 100, and the
/// default for anything absent or out of range.
pub fn priority_of_env(env: &[(String, String)]) -> u8 {
    env.iter()
        .find(|(k, _)| k == REQ_PRIORITY)
        .and_then(|(_, v)| v.parse::<u8>().ok())
        .filter(|p| (1..=100).contains(p))
        .unwrap_or(DEFAULT_PRIORITY)
}

/// Did the reconciler offer this start allowed to preempt?
pub fn may_preempt(env: &[(String, String)]) -> bool {
    env.iter().any(|(k, v)| k == REQ_MAY_PREEMPT && v == "1")
}

/// How a deployment's instances leave a draining machine (Phase C, E9) —
/// Nomad's `migrate` block, with Nomad's defaults.
///
/// **A replacement first, then the original.** An instance on a machine
/// being drained stays up until one standing in for it elsewhere has been
/// healthy for `min_healthy_secs`; only then is it stopped. At most
/// `max_parallel` of one deployment's instances are moving at once. Before
/// this, a drain cancelled every instance on the machine in the same tick,
/// which for two instances on one machine was an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigratePolicy {
    pub max_parallel: u32,
    /// How long a replacement must be healthy — or, with no health check,
    /// running — before the original is stopped.
    pub min_healthy_secs: u64,
    /// A replacement not healthy by then stops holding one of the
    /// `max_parallel` places, so the next instance can start moving. The
    /// original it stood in for keeps serving until the drain's deadline.
    pub healthy_deadline_secs: u64,
}

impl Default for MigratePolicy {
    fn default() -> Self {
        MigratePolicy {
            max_parallel: 1,
            min_healthy_secs: 10,
            healthy_deadline_secs: 300,
        }
    }
}

impl MigratePolicy {
    /// The reserved entry, or nothing when every value is the default.
    pub fn to_env(&self) -> Vec<(String, String)> {
        if *self == MigratePolicy::default() {
            return Vec::new();
        }
        vec![(
            REQ_MIGRATE.to_string(),
            serde_json::to_string(self).unwrap_or_default(),
        )]
    }

    pub fn from_env(env: &[(String, String)]) -> MigratePolicy {
        env.iter()
            .find(|(k, _)| k == REQ_MIGRATE)
            .and_then(|(_, v)| serde_json::from_str(v).ok())
            .unwrap_or_default()
    }
}

/// The word a file uses for "no timer": `lost_after: never`,
/// `progress_deadline: never`. Rendered back the same way, so a deliberate
/// zero survives a round trip instead of re-parsing as the default.
pub const NEVER: &str = "never";

/// The one sentence about what happens when a node running an instance
/// goes away (Phase A, U6).
///
/// One function for the dry run, `deployment status` and Director, so the
/// three cannot word it differently. It is the most important sentence
/// about a service's availability and neither product used to say it.
pub fn availability(count: &Count, policy: &DeploymentPolicy) -> String {
    if matches!(count, Count::System) {
        return concat!(
            "One instance per matching node: a node that goes away takes its ",
            "instance with it, and a node that joins gets one."
        )
        .to_string();
    }
    if policy.replace_unknown {
        let winner = match policy.reconcile {
            Reconcile::KeepOriginal => "the original",
            Reconcile::KeepReplacement => "the replacement",
            Reconcile::BestScore => "the better-placed of the two",
            Reconcile::LongestRunning => "whichever has run longer",
        };
        return format!(
            concat!(
                "If a node running an instance becomes unreachable, a replacement ",
                "starts at once; when the node comes back, {} stays."
            ),
            winner
        );
    }
    if policy.lost_after_secs == 0 {
        return concat!(
            "If a node running an instance goes away, the instance waits for that ",
            "node to come back and is never replaced elsewhere (lost_after: never)."
        )
        .to_string();
    }
    format!(
        concat!(
            "If a node running an instance goes away, the instance is replaced on ",
            "another node after {} unreachable."
        ),
        duration_words(policy.lost_after_secs)
    )
}

/// `45 s`, `1 min`, `1 h 30 min` — for sentences a person reads, never for
/// a file that is parsed back.
pub fn duration_words(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h} h"));
    }
    if m > 0 {
        parts.push(format!("{m} min"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s} s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod availability_tests {
    use super::*;

    /// One sentence per shape of policy, and each one says what that
    /// policy actually does — including the default nobody typed.
    #[test]
    fn the_sentence_says_what_the_policy_does() {
        let fixed = Count::Fixed(2);
        let defaulted = DeploymentPolicy {
            lost_after_secs: DEFAULT_LOST_AFTER_SECS,
            ..Default::default()
        };
        assert!(
            availability(&fixed, &defaulted).contains("replaced on another node after 1 min"),
            "{}",
            availability(&fixed, &defaulted)
        );
        assert!(availability(&fixed, &DeploymentPolicy::default()).contains("never replaced"));
        let replace = DeploymentPolicy {
            replace_unknown: true,
            reconcile: Reconcile::KeepReplacement,
            ..Default::default()
        };
        assert!(availability(&fixed, &replace).contains("a replacement starts at once"));
        assert!(availability(&fixed, &replace).contains("the replacement stays"));
        assert!(
            availability(&Count::System, &defaulted).starts_with("One instance per matching node")
        );
    }

    #[test]
    fn durations_read_as_words() {
        assert_eq!(duration_words(0), "0 s");
        assert_eq!(duration_words(45), "45 s");
        assert_eq!(duration_words(60), "1 min");
        assert_eq!(duration_words(600), "10 min");
        assert_eq!(duration_words(5400), "1 h 30 min");
        assert_eq!(duration_words(3661), "1 h 1 min 1 s");
    }
}
