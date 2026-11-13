//! The node daemon: identity + genesis, state engine, transport, gossip,
//! job manager, and the request router that binds them (MASTERPLAN §3, §9,
//! §10.3). Every node runs this identical stack — there is no server role.
//!
//! Since M3, every frontend request arrives in a user-signed envelope
//! ([`SignedIpc`]); the daemon verifies the signature, replay-checks the
//! nonce, evaluates the user's grants against local materialized policy,
//! and audits the action (MASTERPLAN §7).

// -- `impl Node`, continued elsewhere ----------------------------------
//
// This file was 31,056 lines and `impl Node` was 27,994 of them, so a
// reader looking for one behaviour had to scroll past every other. Each
// module below is a continuation of that same `impl`, carved along a
// seam the code already had — a banner comment, or a run of methods
// that only call each other. Nothing moved between types and nothing
// changed behaviour; `use super::*` keeps one import list.
mod action_cache;
mod dispatch;
mod facts;
mod gangs;
mod ipc;
mod metrics;
// `work_stealing`, not `parallel`: `crate::parallel` is already in
// scope here, and a submodule of the same name shadows it.
mod schedules;
mod sinks;
mod storage;
mod upgrade;
mod wire;
mod work_stealing;

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{Instrument, debug, error, info, warn};

use crate::audit::{self, AuditFilter};
use crate::capacity;
use crate::config::NodeConfig;
use crate::content::ContentStore;
use crate::error::{CoreError, Result};
use crate::genesis::Genesis;
use crate::gossip::Gossip;
use crate::identity::{Identity, verify_signature};
use crate::jobs::{JobManager, LogEvent};
use crate::membership::{self, Selector};
use crate::objects::{ObjectDirectory, ObjectStore};
use crate::state::StateEngine;
use crate::throttle::Source;
use crate::transport::{AdmissionCheck, LocalInfo, RequestEvent, Transport};
use crate::wan::Wan;
use crate::{
    content_sync, gitsync, object_sync, outputs, pairing, parallel, rbac, scheduler, secrets,
    services,
};
use mesh_proto::{
    BundleId, BundleRow, ChunkAssignment, ConnPurpose, EnrolmentRequest, EnrolmentRow,
    EnrolmentState, FanId, FanTarget, FanoutControls, FanoutInfo, FanoutPlanRecord, GitSource,
    Grant, GrantId, GrantRow, IdentityInfo, InviteToken, IpcRequest, IpcResponse, JobId,
    JobPayload, JobPriority, JobRow, JobSpec, JobState, JobSummary, Liveness, MemberInfo, NodeId,
    NodeRow, OpBody, PingResult, PlannedTarget, ProviderRow, Request, Response, Role, SecretRow,
    ServiceRow, SignedIpc, SourceRow, StatusInfo, TopRow, UserId, UserRecord, UserRow, WhoAmIInfo,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum clock skew tolerated on request envelopes.
const AUTH_SKEW: Duration = Duration::from_secs(300);

/// How long `bundles prefetch` follows a fleet before it stops reporting
/// (M24 §3.2). The fetches themselves are not bounded by it — a node that
/// is still pulling when this expires keeps pulling, and the summary says
/// so rather than pretending the push failed.
/// How long a *staged input* pin is held without anyone releasing it
/// (ENG-19). The run that staged them unpins when it ends; this is only
/// the backstop for one that dies first, so it is longer than any run
/// should be rather than as short as possible.
const STAGED_INPUT_LEASE: Duration = Duration::from_secs(36 * 3600);
const PREFETCH_DEADLINE: Duration = Duration::from_secs(900);
/// How often each still-fetching node is re-read.
const PREFETCH_POLL: Duration = Duration::from_secs(2);

/// How long one node may make **no progress** before the rollout stops and
/// says so (M24 §4, ENG-26).
///
/// This was a flat ten-minute ceiling on the whole step — fetch, verify,
/// swap, drain, restart, catch up — and a drain is not bounded by it. A
/// node running an hour-long job drains for an hour, correctly, and the
/// rollout declared it failed at ten minutes and stopped, in the middle of
/// a fleet, over a node that was doing exactly what it was asked to.
///
/// So the clock is on *silence*, not on the step. It resets whenever the
/// node says something new, and whenever the work it is draining gets
/// smaller — which is the observation that separates a long drain from a
/// hang, and which this node can make from the replicated job records
/// without asking for anything the protocol does not already carry.
const UPGRADE_NODE_STALL: Duration = Duration::from_secs(600);

/// How long an upgrade's drain leaves this node's deployment instances
/// alone before they are moved elsewhere (ENG-26).
///
/// The window in which "this node is restarting" and "this node is gone"
/// look the same from outside, and the whole question is which one the
/// mesh should assume. A binary swap and restart is seconds; two minutes
/// is far past it, and far inside the point at which leaving a service
/// down would be the wrong call.
const UPGRADE_INSTANCE_GRACE: Duration = Duration::from_secs(120);

/// …and the ceiling on one node however well it is doing, because a
/// rollout that can wait forever is a rollout nobody is watching. Longer
/// than any job anybody should be running, and finite.
const UPGRADE_NODE_MAX: Duration = Duration::from_secs(12 * 3600);
/// How often a node mid-upgrade is re-asked.
const UPGRADE_POLL: Duration = Duration::from_secs(3);

/// The release key compiled into this build, and whether it is the in-tree
/// placeholder.
///
/// A fleet that has never trusted a key verifies against this, and the
/// placeholder verifies nothing — which is correct (a build that never had
/// a real key must not be upgradeable by anyone who can serve bytes) and
/// is also the single most confusing refusal in M24, because it arrives
/// months after install with no hint of what to do. Surfaces read this to
/// say so before it costs anybody a rollout.
const EMBEDDED_RELEASE_KEY: &str = include_str!("../../../../packaging/minisign.pub");

/// Whether [`EMBEDDED_RELEASE_KEY`] is the placeholder.
pub fn embedded_release_key_is_placeholder() -> bool {
    EMBEDDED_RELEASE_KEY
        .to_ascii_lowercase()
        .contains("placeholder")
}

/// How often a node that keeps giving the *same* answer says so anyway.
/// Long enough not to be noise, short enough that a stall is visible well
/// inside [`UPGRADE_NODE_STALL`] rather than at the end of it.
const UPGRADE_STILL_WAITING: Duration = Duration::from_secs(30);
/// Envelopes older than this are rejected at the executor even when the
/// origin accepted them (jobs dispatch immediately in M3).
const JOB_AUTH_WINDOW: Duration = Duration::from_secs(3600);
/// Handlers running at once for requests from peers that are not members
/// (SEC-10).
const MAX_STRANGER_HANDLERS: usize = 64;
/// How far past its TTL an acting-for deadline may sit before it is taken
/// as a node choosing its own licence rather than clock skew (SEC-1).
const ACT_CLAIM_SKEW_MICROS: u64 = 5 * 60 * 1_000_000;
/// The words an authorization refusal uses when it rests on state this node
/// may simply not have merged yet — which is worth one pull, not a failure.
const NOT_KNOWN_HERE: &str = "is not known here";

/// What an executor concluded about an acting-for claim (SEC-1).
enum ActingCheck {
    /// The claim stands.
    Proven,
    /// There is no claim at all.
    Absent,
    /// The claim rests on a job or plan this node has not merged.
    Unknown { what: String },
    /// The claim is refused, for this reason.
    Refused(String),
}

impl ActingCheck {
    /// As an authorization result for `what` ("job authorization
    /// envelope"), with the reason in the message. [`NOT_KNOWN_HERE`] marks
    /// the refusal a caller may retry after a pull.
    fn into_result(self, what: &str) -> Result<()> {
        match self {
            ActingCheck::Proven => Ok(()),
            ActingCheck::Absent => Err(CoreError::Auth(format!("{what} names a different user"))),
            ActingCheck::Unknown { what: missing } => Err(CoreError::Auth(format!(
                "{what} names a different user, and the claim that it acts for them \
                 rests on {missing}, which {NOT_KNOWN_HERE}"
            ))),
            ActingCheck::Refused(why) => Err(CoreError::Auth(format!(
                "{what} names a different user, and the claim that it acts for them \
                 was refused: {why}"
            ))),
        }
    }
}

/// How an envelope relates to the replicated plans that might carry it.
enum PlanBinding {
    /// A plan lists this job on this node, with this envelope.
    Listed,
    /// No plan carries the envelope, and none should.
    Unplanned,
    /// The job should be in a plan this node has not merged.
    Missing,
    /// A plan contradicts the claim.
    Refused(&'static str),
}
/// How often a node checks whether it can heal secret wraps for members
/// that joined after a secret was sealed.
const REWRAP_INTERVAL: Duration = Duration::from_secs(15);
/// How often a node checks whether any of its own ops have become eligible
/// to compact (C0 P2). Deliberately not configurable — see
/// [`Node::spawn_compaction_task`].
const COMPACTION_INTERVAL: Duration = Duration::from_secs(3600);

/// Widen a browse so the merge can do its own paging.
///
/// Both tiers are asked for `offset + limit` rows starting at zero, because
/// an offset applied to each tier separately skips rows the *other* tier was
/// about to supply — the classic distributed-paging error, and the reason
/// page depth here is capped rather than unbounded.
fn deepen(q: &mesh_proto::HistoryQuery) -> mesh_proto::HistoryQuery {
    mesh_proto::HistoryQuery {
        offset: 0,
        limit: (q.offset as usize)
            .saturating_add(q.effective_limit())
            .clamp(1, crate::archive_read::ARCHIVE_ROW_CAP as usize) as u32,
        ..q.clone()
    }
}

/// How many rows matched, when that is knowable — and `None` the rest of
/// the time, because a total that is really a floor is worse than no total.
/// A UI renders it as a count and a person reads it as one.
///
/// Three things all have to hold, and the middle one is the subtle one:
///
/// - the index knew its own total (its scan did not stop early);
/// - the index **returned** every row it counted. `HistoryQuery::limit`
///   caps what comes back but not what is counted, so a page of 2,000 with
///   a total of 40,000 is a normal answer — and the merge only deduplicates
///   against the rows it can see. Adding archive rows to a count that
///   already includes rows the merge never inspected would double-count
///   whatever overlaps;
/// - the archive did not fill its page, so its own cap is not doing the
///   talking.
fn exact_total(
    merged: usize,
    index_rows: usize,
    index_total: Option<u64>,
    archive_rows: usize,
    deep_limit: u32,
) -> Option<u64> {
    (index_total? == index_rows as u64 && archive_rows < deep_limit as usize)
        .then_some(merged as u64)
}

/// Turn raw archive JSON into the reply shape, dropping rows whose ids do
/// not parse rather than inventing an id for them.
fn rows_into_reply(rows: &[serde_json::Value], has_archive: bool) -> mesh_proto::ArchivePageReply {
    mesh_proto::ArchivePageReply {
        jobs: rows
            .iter()
            .filter_map(crate::archive_read::job_row_from)
            .collect(),
        has_archive,
    }
}

/// The host out of a URL, without a URL parser.
///
/// A dependency for this would be the wrong trade: what `egress_allow`
/// needs is the authority's host, the rules are three lines, and the one
/// case that matters — somebody putting credentials in a `user:pass@host`
/// URL — is handled by taking everything after the last `@` rather than
/// before it. A parser that got that backwards would allowlist the
/// username.
fn url_host(url: &str) -> String {
    let rest = url
        .trim()
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or_else(|| url.trim());
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // `user:pass@host:port` — the host is after the *last* `@`.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // `[::1]:8123` keeps its brackets off; `host:8123` loses its port.
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host_port.split(':').next().unwrap_or(host_port),
    };
    host.to_ascii_lowercase()
}

/// How often a sink export pass runs (M31 B1), unless
/// `[integrations] export_interval_secs` says otherwise.
///
/// Thirty seconds, matching `SinkRecords::every_secs`'s default. A pass
/// with nothing to export costs two index range scans and no writes, so an
/// idle mesh with a sink pays a scan every half minute — and a mesh with no
/// sinks pays a map read.
pub const SINK_TICK_SECS: u64 = 30;

/// Payloads considered in one pass.
///
/// Much smaller than the record batch, because each one is a whole job's
/// stdout rather than a few hundred bytes — a hundred of those is already a
/// large amount of work for one tick, and the tick comes round again in
/// thirty seconds.
const PAYLOAD_BATCH: u32 = 100;

/// How long a row must have been terminal before a sink sees it, unless
/// `[integrations] settle_secs` says otherwise.
///
/// Five minutes, the archive's `INGEST_LAG_MICROS` and for the archive's
/// reason: a run that finished on another node reaches this one whenever
/// gossip gets round to it, and exporting five minutes behind the present
/// makes "the row arrived late" a non-event rather than a permanent gap in
/// somebody's compliance store.
///
/// The knob exists because the figure is a property of *this* mesh's
/// convergence, not of the product: a single-node mesh has nothing to
/// converge with and can honestly use zero, and a mesh gossiping over a
/// satellite link may want more than five minutes.
pub const SINK_SETTLE_SECS: u64 = 5 * 60;

/// How often rate-limit episodes are turned into audit entries (M31 A3).
///
/// A minute. Long enough that the drain is free; short enough that an
/// operator watching a loop happen sees it in `meshnode audit` while they
/// are still looking.
const BACKPRESSURE_TICK: Duration = Duration::from_secs(60);

/// How long an entity may be quiet before the limiter forgets it.
///
/// An hour. Only entities with nothing on their record are forgotten, so
/// this bounds memory without losing a diagnosis.
const BACKPRESSURE_IDLE_SECS: u64 = 3_600;

/// How long an op-log census stays fresh (M31 P0).
///
/// Five minutes. The numbers it holds — what the log is made of, and how
/// fast it grows — move on the scale of hours, and computing one walks
/// every op in the log. Shorter would make a scrape expensive; longer would
/// make `doctor` report something an operator has already changed.
const CENSUS_TTL: Duration = Duration::from_secs(300);

/// How often a node with accelerators samples their live telemetry (C5 P5).
///
/// Ten seconds, which is roughly a gossip round: the figure exists so that
/// five GPU jobs distribute by real idleness rather than by job count, and a
/// placement decision made on a ten-second-old utilisation reading is the
/// same decision as one made on a fresh one. Sampling per second would fork
/// `nvidia-smi` 86,400 times a day to answer a question nobody asked.
const DEVICE_PRESSURE_INTERVAL: Duration = Duration::from_secs(10);

/// How many telemetry samples pass before the *static* inventory is
/// re-probed (C5 P6) — six, so a driver change is picked up within a minute.
///
/// The two cadences are deliberately different. Utilisation changes every
/// second; a card's model, UUID and memory size change when somebody opens
/// the machine. A minute is well inside the window in which a person who
/// just upgraded a driver would go looking for the result, and it is 1/6th
/// of the forks that sampling on the same clock would cost.
const DEVICE_REPROBE_TICKS: u32 = 6;
/// How often a node scans replicated fan-out plans for a dead owner it
/// should adopt (MASTERPLAN §10.3).
const ADOPTION_SCAN_INTERVAL: Duration = Duration::from_secs(3);
/// How often a headless fan-out driver re-reads replicated summaries.
const DRIVE_POLL: Duration = Duration::from_secs(1);
/// The shortest gap between two driver passes for one run (**M-13**).
///
/// The driver is woken by the `Jobs` change signal now rather than by a
/// clock, which is what removes the 0–1 s dead time at every step handoff —
/// a twenty-step chain was gaining about ten seconds of pure waiting. But
/// *any* job op anywhere bumps that topic, so on a busy mesh the wake could
/// arrive far more often than a run has anything to do. This floor
/// coalesces a burst into one pass and keeps the wake rate bounded, while
/// still being far below what a person would notice.
const DRIVE_FLOOR: Duration = Duration::from_millis(50);
/// Dispatch attempts per fan-out target before the driver gives up on it.
const DRIVE_MAX_ATTEMPTS: u32 = 5;

/// How long one submission may wait for the executor's answer (ENG-6).
/// Peer requests had no deadline at all, so a submission on a connection to
/// a peer that had just died waited out the transport's idle timeout.
const SUBMIT_DEADLINE: Duration = Duration::from_secs(15);

/// How long to keep asking the *same* executor after a submission went out
/// and no answer came back (ENG-6). Asking again is safe — an executor
/// acknowledges a job id it has already journaled — and it is the only way
/// to find out whether it took the job. Long enough for a node that is
/// restarting into a new release to come back.
const RESUBMIT_WINDOW: Duration = Duration::from_secs(20);

/// How long a fan-out keeps trying a target it cannot reach before counting
/// it failed (ENG-6). A fan-out's targets are fixed, so there is nowhere to
/// spill to; the case that matters is a node restarting during a rolling
/// upgrade, which is back in seconds.
const FANOUT_PATIENCE: Duration = Duration::from_secs(30);
/// Rendezvous context for fan-out successor election.
const SUCCESSOR_CONTEXT: &[u8] = b"mw-fanout-succ-v1";

/// Rendezvous context for workflow-run adoption. Distinct from the fan-out
/// How often the Git watcher evaluates due sources (MASTERPLAN §8.1).
const GIT_SCAN_INTERVAL: Duration = Duration::from_secs(5);
/// The deployment reconciler's cadence (C2 §6). The same 5s the git watcher
/// uses: fast enough that a killed instance is replaced
/// promptly, slow enough that a mesh at rest authors nothing.
const DEPLOYMENT_TICK: Duration = Duration::from_secs(5);
/// What one machine said when offered one deployment instance (Phase B
/// hardening). Three answers, because only two of them are safe to act on.
enum Offer {
    Accepted,
    /// Certainly not accepted — nothing journaled it — so the start may go
    /// to the next machine. `busy`: the machine is full rather than unable,
    /// which is the only refusal a preemption can answer (Phase C, E10).
    Refused {
        why: String,
        busy: bool,
    },
    /// Perhaps accepted: the request left and no answer came back. The start
    /// stays with this machine until its record arrives or the pin expires.
    Unknown(String),
}

/// How often the executor probes its deployment instances' health (C2 P3).
/// A little faster than the reconciler, so a freshly-healthy instance is
/// published before the reconciler's next pass wants to gate on it.
const DEPLOYMENT_HEALTH_TICK: Duration = Duration::from_secs(3);

/// How long a start that preempted a job waits for it to stop before asking
/// admission again (Phase C, E10). Bounded, because the reconciler is waiting
/// on the answer; a job that takes longer to die is answered `Busy`, and the
/// start is tried again later.
const PREEMPT_WAIT_SECS: u64 = 10;

/// Outcome of moving one queued job off a draining node (MASTERPLAN §12).
#[derive(Debug)]
enum MoveOutcome {
    /// A candidate accepted it (or verifiably journaled it already).
    Moved(String),
    /// Everyone refused cleanly — safe to keep it local.
    NobodyTook(String),
    /// A candidate may have journaled it and cannot confirm: the job is
    /// theirs now; re-running it here could double-execute (§10.3).
    Ambiguous(String),
}

/// How a submission is being made.
///
/// An enum rather than booleans on every call, because the modes are
/// mutually exclusive by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitMode {
    /// Execute.
    Normal,
    /// Resolve everything — targets, version, env keys, secret names —
    /// and dispatch nothing.
    DryRun,
    /// Resolve everything, derive the action key, dispatch nothing (C7 P1).
    ///
    /// A sibling of [`Self::DryRun`] rather than a flag on it, because the
    /// two answer different questions: dry run answers *"what would this
    /// touch"*, and this answers *"is this the same work as last time"*.
    /// Folding them together would give an operator one output for two
    /// questions.
    ExplainKey,
}

impl SubmitMode {
    fn is_dry_run(&self) -> bool {
        matches!(self, SubmitMode::DryRun)
    }

    /// Neither of the modes that dispatch anything.
    fn is_explain_key(&self) -> bool {
        matches!(self, SubmitMode::ExplainKey)
    }
}

/// The longest a critical timed loop may go without a round before the node
/// counts as wedged (ENG-1): no watchdog feed, and `/healthz` fails.
///
/// Generous on purpose. These are backstops for a loop that has *stopped*,
/// and a false alarm costs a restart. The scheduler's allowance is the
/// larger because its tick also runs the hourly retention sweeps, which
/// walk the job directory on the same task.
const GOSSIP_STALL_AFTER: Duration = Duration::from_secs(120);
const SCHEDULER_STALL_AFTER: Duration = Duration::from_secs(300);

/// How often a node that failed to write its op-log tries to reopen it
/// (ENG-4).
const STORE_RETRY_EVERY: Duration = Duration::from_secs(10);

/// Free space, in MiB, below which a failed op-log is not reopened (ENG-4).
const STORE_REOPEN_FREE_MB: u64 = 64;

/// What a client still asking about quotas or the usage report is told.
const QUOTAS_RETIRED: &str = "quotas and the usage report were removed from meshware, and this \
     node no longer answers for them. Fair share still orders a busy queue — `meshnode queue` \
     shows it";

/// What a client or an older peer still asking about the host inventory is
/// told.
const INVENTORY_RETIRED: &str =
    "the host inventory was removed from meshware, and this node no longer answers for it";

/// What a client still asking about change control is told.
const CHANGE_CONTROL_RETIRED: &str = "change control — policies, approvals, freeze windows and \
     touch-to-approve — was removed from meshware, and this node no longer answers for it";

/// What a client still asking to search workflow runs is told.
const RUN_INDEX_RETIRED: &str = "the run index was removed from meshware with the workflow \
     engine it recorded. `meshnode history jobs` searches this node's job history, and \
     `meshnode history results <group>` has a fan-out's rows";

/// What a client still asking for the mesh benchmark is told.
const BENCH_RETIRED: &str = "the mesh benchmark was removed from meshware. `meshnode top` shows \
     live load, `meshnode jobs explain` says why a job landed where it did, and `/metrics` has \
     the history";

/// Files under `work` matching any declared output glob (C7 P2).
///
/// Returns `(relative path, absolute path)` pairs, sorted, so two runs of
/// the same action publish their outputs in the same order and an entry is
/// comparable to another entry by inspection.
///
/// `*` matches across `/`, which is [`mesh_proto::device::glob_match`]'s
/// behaviour and is deliberately kept rather than reimplemented: one glob
/// dialect in the product beats two, and `out/*` meaning "everything under
/// out" is what somebody typing it expects anyway.
///
/// Symlinks are not followed and are not published. A cached output that
/// pointed outside the job directory would make a cache hit a way to read
/// the executor's disk.
fn collect_outputs(work: &std::path::Path, globs: &[String]) -> Vec<(String, std::path::PathBuf)> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(String, std::path::PathBuf)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                walk(root, &path, out);
            } else if meta.is_file()
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push((rel.to_string_lossy().replace('\\', "/"), path.clone()));
            }
        }
    }
    let mut found = Vec::new();
    walk(work, work, &mut found);
    found.retain(|(rel, _)| globs.iter().any(|g| mesh_proto::device::glob_match(g, rel)));
    found.sort();
    found
}

/// The live node. Cloneable handle shared by IPC, transport dispatch, and
/// embedders (Director).
#[derive(Clone)]
pub struct Node {
    inner: Arc<Inner>,
}

/// Why a replica did not index a cache entry (SEC-18).
enum CacheRefusal {
    /// A `map` item whose plan this replica has not verified: ask for it.
    PlanNeeded,
    Refused(String),
}

/// A `map` plan a cache replica has checked, kept so the plan — which
/// carries every item — is sent to it once rather than with every item.
struct VerifiedPlan {
    /// The plan with its item list moved into `items`.
    plan: mesh_proto::ParallelPlan,
    items: HashSet<String>,
}

/// The replica's verified plans, bounded by count and by items held.
#[derive(Default)]
struct VerifiedPlans {
    plans: HashMap<JobId, (Arc<VerifiedPlan>, Instant)>,
}

impl VerifiedPlans {
    const MAX_PLANS: usize = 32;
    const MAX_ITEMS: usize = 1_000_000;

    fn get(&mut self, job: &JobId) -> Option<Arc<VerifiedPlan>> {
        let (plan, used) = self.plans.get_mut(job)?;
        *used = Instant::now();
        Some(plan.clone())
    }

    fn insert(&mut self, mut plan: mesh_proto::ParallelPlan) -> Arc<VerifiedPlan> {
        let items: HashSet<String> = std::mem::take(&mut plan.items).into_iter().collect();
        let verified = Arc::new(VerifiedPlan { plan, items });
        self.plans
            .insert(verified.plan.job, (verified.clone(), Instant::now()));
        // Oldest first, until both bounds hold — the one just added last.
        loop {
            let held: usize = self.plans.values().map(|(p, _)| p.items.len()).sum();
            if self.plans.len() <= Self::MAX_PLANS && held <= Self::MAX_ITEMS
                || self.plans.len() == 1
            {
                break;
            }
            let oldest = self
                .plans
                .iter()
                .filter(|(job, _)| **job != verified.plan.job)
                .min_by_key(|(_, (_, used))| *used)
                .map(|(job, _)| *job);
            match oldest {
                Some(job) => {
                    self.plans.remove(&job);
                }
                None => break,
            }
        }
        verified
    }
}

/// How long a node waits before saying it cannot open a secret, and how
/// often it says it again (SEC-17).
const SECRET_WANT_AFTER: Duration = Duration::from_secs(60);
const SECRET_WANT_EVERY: Duration = Duration::from_secs(5 * 60);

/// Which record, and which of its wraps are this node's — what decides
/// whether an earlier "can I open it" still holds.
#[derive(Clone, PartialEq)]
struct SecretContent {
    version: u32,
    created_at_micros: u64,
    signature: Vec<u8>,
    /// A digest of the wraps addressed to this node; `None` if there are
    /// none.
    mine: Option<[u8; 32]>,
}

impl SecretContent {
    fn of(record: &mesh_proto::SecretRecord, me: NodeId) -> Self {
        let mut h = blake3::Hasher::new();
        let mut any = false;
        for w in record.wraps.iter().filter(|w| w.node_id == me) {
            any = true;
            h.update(&w.eph_pub);
            h.update(&w.nonce);
            h.update(&w.wrapped_key);
        }
        SecretContent {
            version: record.version,
            created_at_micros: record.created_at_micros,
            signature: record.signature.clone(),
            mine: any.then(|| *h.finalize().as_bytes()),
        }
    }
}

/// What this node last found out about opening one secret.
#[derive(Clone)]
struct SecretMemo {
    content: SecretContent,
    opens: bool,
    /// Since when `opens` has held, whatever the wraps did meanwhile.
    since: Instant,
    /// When this node last asked to be rewrapped.
    asked: Option<Instant>,
}

impl SecretMemo {
    fn new(content: SecretContent, opens: bool, now: Instant) -> Self {
        SecretMemo {
            content,
            opens,
            since: now,
            asked: None,
        }
    }
}

struct Inner {
    config: NodeConfig,
    identity: Identity,
    engine: Arc<StateEngine>,
    transport: Arc<Transport>,
    gossip: Arc<Gossip>,
    /// When a join last made this node ask its peers about each invite it
    /// had not heard of ([`Node::learn_invite`], SEC-17).
    invite_pull: Mutex<HashMap<mesh_proto::InviteId, Instant>>,
    /// Whether this node can open each secret, so the rewrap pass does not
    /// redo the crypto every fifteen seconds (SEC-17).
    secret_memo: Mutex<HashMap<String, SecretMemo>>,
    /// Holds the previous `/proc/stat` reading, because a utilisation
    /// percentage is a rate and one sample of a cumulative counter is not
    /// one. Lives here so every sampler in the process shares a baseline.
    cpu_sampler: capacity::CpuSampler,
    /// Same shape, same reason, for `/proc/net/dev`.
    net_sampler: capacity::NetSampler,
    jobs: Arc<JobManager>,
    /// Identity-provider signing keys, cached per JWKS URI (E1-P5).
    /// In memory only: a key set is public, cheap to refetch, and
    /// persisting it would let a node verify against keys the provider
    /// withdrew before it last restarted.
    jwks: Arc<crate::oidc::Jwks>,
    content: ContentStore,
    /// Set when this node is under its disk floor and reclaiming did not
    /// fix it (M31 A2). The one piece of A2 that is *not* advisory: while
    /// it is set, this node declines to pull new bundles, because the
    /// alternative is being the process that fills somebody's volume.
    disk_critical: AtomicBool,
    /// Ephemeral objects produced here (M9 §5.5). Separate from `content`
    /// in every way that matters: lifetime, index and residency. A restart
    /// forgets them, exactly as it forgets the chunk queue.
    objects: ObjectStore,
    /// This node's slice of the rendezvous-hashed object directory — the
    /// entries whose object ids happen to rank it first (M9 §5.5). Not a
    /// service and not a leader: every node runs one, and which one answers
    /// for a given object is arithmetic anyone can do.
    objdir: ObjectDirectory,
    /// This node's slice of the action-cache directory (C7 §11.2) — the
    /// entries whose action keys happen to rank it first. The same
    /// primitive `objdir` uses, under its own rendezvous context, and for
    /// the same reason: "has anybody already run this?" must cost one
    /// request to one node that arithmetic alone identified.
    actions: crate::cache::ActionCache,
    /// Placement breakdowns for jobs this node submitted, keyed by job id
    /// (C8 §12.2), until the history indexer picks them up.
    ///
    /// A staging buffer and not a store: the record's home is the local
    /// history index, but the explanation is produced at *dispatch* and the
    /// indexer runs on its own timer. Bounded by
    /// `EXPLAIN_PENDING_MAX` — an unbounded map on the submission path is
    /// the memory leak §12.2 refuses to ship the chart with.
    pending_explains: Mutex<Vec<(JobId, mesh_proto::PlacementExplain)>>,
    /// Jobs the cache publish pass has already looked at (C7 P2), so it is
    /// idempotent across ticks and reads each job's spec from disk at most
    /// once. Bounded by a watermark — see `mark_considered`.
    published_actions: Mutex<Considered>,
    /// `map` plans this node has verified as a cache replica (SEC-18).
    verified_plans: Mutex<VerifiedPlans>,
    /// What callers with no role may make this node do (SEC-19).
    anon: crate::throttle::AnonLimits,
    /// The local run/job query index (M14 §2). `None` when `[history]
    /// enabled = false`, or when the index could not be opened — a node
    /// whose index is broken still runs, joins, executes and converges,
    /// because nothing in that list needs it.
    history: Option<Arc<crate::history::HistoryStore>>,
    listen_addr: SocketAddr,
    started: Instant,
    /// Replay guard (S-M1): a two-bucket time wheel, bounded.
    nonces: Mutex<NonceWheel>,
    /// Open pairing approvals awaiting the human verdict (`PairDecide`),
    /// keyed by session id (MASTERPLAN §5.2).
    pair_sessions: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    pair_next: AtomicU64,
    /// Fan-out groups this node is currently driving (as origin or adopter)
    /// — the guard against double-driving one group (MASTERPLAN §10.3).
    active_drives: Mutex<HashSet<FanId>>,
    /// Gangs this node is currently driving, as origin or as adopter — the
    /// same guard as `active_drives`, for the same reason (C4 §8.2 P5).
    active_gangs: Mutex<HashSet<mesh_proto::GangId>>,
    /// Git sources this node is syncing right now (MASTERPLAN §8.1).
    active_syncs: Mutex<HashSet<String>>,
    /// The release tag this node is currently swapping itself to (M24 §4),
    /// if any. Memory only, and set for the seconds between spawning the
    /// upgrade and the daemon exiting — long enough that a driver polling
    /// this node is told "already going" rather than starting a second one.
    upgrading: Mutex<Option<String>>,
    /// Bundles this node is fetching because somebody asked it to, ahead
    /// of needing them (M24 §3.2). Memory only, and a *dedup set* rather
    /// than a queue: a second ask joins the fetch already in flight, which
    /// is what makes `BundleFetch` safe to use as the progress poll.
    prefetching: Mutex<HashSet<BundleId>>,
    /// Draining (MASTERPLAN §12): finishing accepted work, refusing new.
    draining: AtomicBool,
    /// A timed drain (`meshnode drain --until`): when it lifts and why it
    /// was asked for. `None` when not draining, or draining for good.
    drain_until: Mutex<Option<(u64, String)>>,
    /// Fans out to every background loop; flipping it stops them so the
    /// engine (and its store lock) can actually be released.
    shutdown_tx: watch::Sender<bool>,
    /// A completed drain asks the surrounding daemon loop to exit.
    exit_tx: watch::Sender<bool>,
    /// Set once this node has begun leaving a reset mesh, by whichever of
    /// the founder's request and the reset watch got there first.
    resetting: AtomicBool,
    /// The reset this node is leaving for, once its work has stopped:
    /// [`run`] moves the mesh's data aside after the node is down.
    left_mesh: Mutex<Option<mesh_proto::MeshReset>>,
    /// Long-lived background task handles, awaited/aborted by [`Node::stop`].
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Job-history retention, hot-reloadable (MASTERPLAN §9.1, §12).
    retention: Mutex<crate::config::Retention>,
    /// The last op-log census, and when it was taken (M31 P0).
    ///
    /// Cached because computing one walks every op in the log, and
    /// `/metrics` is scraped every few seconds by something that does not
    /// know that. The numbers it carries — what the log is made of, and how
    /// fast it grows — move on the scale of hours, so a stale one is not a
    /// wrong one.
    census: Mutex<Option<(Instant, mesh_proto::OpCensus)>>,
    /// The configuration as it is actually in effect (ENG-11): the boot
    /// config, with every reload's hot-reloadable sections applied.
    ///
    /// `SIGHUP` diffs against this and replaces it. It diffed against
    /// `config`, which is the boot config and never changes, so a setting
    /// changed and then changed back — `sandbox` auto → off → auto — read
    /// as "unchanged" the second time and stayed off. Readers of a
    /// reloadable setting read it here; `config` keeps what only a restart
    /// changes.
    live: parking_lot::RwLock<Arc<NodeConfig>>,
    /// Held for the whole of a reload, so each diffs against what the last
    /// one left — without holding `live` while probes and op writes run.
    reloading: Mutex<()>,
    /// Dispatch latency (N6 §12): submission in, accepted somewhere.
    ///
    /// MASTERPLAN bet #3 calls dispatch overhead "the product" and fifty-two
    /// metrics existed without measuring it, so the central claim was the
    /// one thing an operator could not check.
    dispatch: crate::latency::Histogram,
    /// How long this node takes to answer a local request (ENG-21). The
    /// meter for "is the daemon responsive", which every other gauge here
    /// assumes and none of them measured.
    ipc_latency: crate::latency::Histogram,
    /// Fan-out, parallel and gang runs this node owns (ENG-21). The
    /// counterpart to `jobs.meters()`, which counts what this node
    /// *executes*: the owner of a 200-target fan-out executes none of it,
    /// so without these the whole run is invisible on the one machine a
    /// person is looking at.
    groups: crate::observe::GroupMeters,
    /// Pack or spread (N6 §12), live.
    ///
    /// Held here rather than read from `config.compute` at each dispatch
    /// because this is the first `[compute]` field that genuinely reloads:
    /// the section's own documentation has claimed hot-reloadability since
    /// M9 and none of it was ever wired, which `apply_config` says out
    /// loud. Reading a stale boot value would make `SIGHUP` a lie here too.
    placement: Mutex<mesh_proto::FitPolicy>,
    /// Real bound address of the observability endpoint, when enabled.
    http_addr: Mutex<Option<SocketAddr>>,
    /// Where the E1-P6 API listener actually bound, when one was asked for.
    api_addr: Mutex<Option<SocketAddr>>,
    /// WAN reachability: reflexive discovery, relay roles, punch
    /// (MASTERPLAN M7).
    wan: Arc<Wan>,
    /// Parallel jobs this node owns, with their chunk queues (M9 §5.4).
    /// Memory only — a restart forgets them, exactly as it forgets liveness.
    parallel: parallel::ParallelRegistry,
    /// Parallel jobs this node is *working on* for someone else (M9 §5.4).
    working: parallel::WorkerRegistry,
    /// Chunk lanes running here across all parallel jobs. This is what
    /// bounds chunk concurrency, in place of the slot accounting an
    /// ordinary job would get.
    lanes: AtomicU32,
    /// Per-collector publish bookkeeping (M18 §4.6). Node-local: the guard
    /// bounds what *this* node authors, and a node that has just restarted
    /// starting from a clean budget is correct — it publishes once and then
    /// falls silent again.
    flap: Arc<crate::facts::FlapGuard>,
    /// Live sessions this node is party to, hosted and initiated (M17
    /// §3.5). Memory only, and deliberately: a session is live or it is
    /// gone, and what outlives it is the audit entry. Replicating it would
    /// put an hour of terminal output in an op-log that is never compacted.
    sessions: Arc<crate::sessions::Registry>,
    /// The elected reconciler's short memory of placing deployment
    /// instances (Phase B hardening) — see `deployments::PlacementMemory`.
    placements: Mutex<crate::deployments::PlacementMemory>,
    /// Whether the loops that make this a working node are alive and
    /// moving (ENG-1, ENG-2).
    vitals: Arc<crate::vitals::Vitals>,
    /// Submissions that did not land where they were first sent (ENG-6).
    dispatch_failures: DispatchFailures,
    /// Job ids being accepted here right now, one at a time each (ENG-6).
    accepting: Mutex<HashMap<JobId, Arc<tokio::sync::Mutex<()>>>>,
    /// The op-log's pulse (ENG-4): dropped when the store cannot even be
    /// reopened, which ends the daemon with a failure.
    store_pulse: Mutex<Option<crate::vitals::Pulse>>,
}

/// Why submissions did not land where they were sent, counted for
/// `/metrics` (ENG-6).
#[derive(Default)]
struct DispatchFailures {
    /// The executor could not be reached; nothing was delivered.
    unreachable: AtomicU64,
    /// The executor was full, or draining.
    busy: AtomicU64,
    /// Sent, and it could not be established whether the executor took it.
    undetermined: AtomicU64,
    /// The executor refused it for a reason trying again will not change.
    refused: AtomicU64,
    /// The executor's protocol is older than the request needs (ENG-24).
    /// Its own reason because the remedy is not "retry" but "upgrade that
    /// node", and a fleet mid-rollout should be able to see this fall to
    /// zero as it finishes.
    too_old: AtomicU64,
    /// Submissions that moved on to another candidate after one of the above.
    spilled: AtomicU64,
}

impl DispatchFailures {
    fn note(&self, e: &CoreError) {
        let counter = match e {
            CoreError::Unreachable(_) => &self.unreachable,
            CoreError::PeerTooOld(_) => &self.too_old,
            CoreError::Busy { .. } => &self.busy,
            e if is_undetermined(e) => &self.undetermined,
            _ => &self.refused,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// The name of an [`IpcRequest`] variant, for a span field (ENG-21).
///
/// Taken from `Debug`, which prints the variant name first, and stopped
/// there: the sink returns an error the moment the identifier ends, which
/// aborts the formatting run before it touches a field. So a `Run` whose
/// payload is a megabyte of script costs the four bytes of `"Run"` and
/// not a megabyte of formatting on a path that runs for every request.
///
/// The alternative was a hand-written `match` over a hundred-odd
/// variants, which is a hundred-odd chances to forget a line when one is
/// added, in return for nothing this does not give.
fn request_kind(req: &IpcRequest) -> String {
    use std::fmt::Write as _;

    struct FirstWord(String);
    impl std::fmt::Write for FirstWord {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            match s.find(|c: char| !c.is_alphanumeric() && c != '_') {
                Some(end) => {
                    self.0.push_str(&s[..end]);
                    // Anything after the name is payload: stop the whole
                    // `Debug` run rather than format and discard it.
                    Err(std::fmt::Error)
                }
                None => {
                    self.0.push_str(s);
                    Ok(())
                }
            }
        }
    }

    let mut sink = FirstWord(String::new());
    let _ = write!(sink, "{req:?}");
    sink.0
}

/// Which housekeeping walk [`Node::sweep_off_thread`] is running.
#[derive(Debug, Clone, Copy)]
enum Sweep {
    /// The content store: bundles, releases, source syncs.
    Content,
    /// Ephemeral objects and the directory that points at them.
    Objects,
    /// Job directories past the retention window.
    Retention,
}

/// A submission that went out and got no answer (ENG-6): the executor may
/// or may not have journaled it.
fn is_undetermined(e: &CoreError) -> bool {
    matches!(
        e,
        CoreError::Transport(_) | CoreError::Proto(_) | CoreError::Io(_) | CoreError::Tls(_)
    ) || matches!(e, CoreError::Job(m) if m.starts_with(UNDETERMINED))
}

/// How an undetermined outcome is reported once asking again ran out.
const UNDETERMINED: &str = "undetermined:";

/// A submission may move on to the next candidate: nothing ran here.
fn may_spill(e: &CoreError) -> bool {
    matches!(
        e,
        // Nothing was sent, so another candidate is safe — and where the
        // old node could not have decoded the request, a newer one can
        // (ENG-24). A mesh mid-rollout should place work on the nodes that
        // are ready rather than refuse it.
        CoreError::Unreachable(_) | CoreError::PeerTooOld(_) | CoreError::Busy { .. }
    )
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Node({})", self.inner.engine.node_id().short())
    }
}

impl Node {
    /// Bootstrap identity + genesis, open state, bind transport, start gossip.
    pub async fn start(config: NodeConfig) -> Result<Self> {
        // Adopt a pre-consolidation install before anything opens a file
        // under the home — the old layout put the CLI user key at the exact
        // path the identity directory now needs, and `ensure` is the only
        // code that knows how to unpick that safely.
        let layout = config.layout();
        for moved in layout.adopt_legacy()? {
            info!(%moved, "adopted data from the previous layout");
        }
        // And before anything could found a mesh: a machine whose only mesh
        // is the one Director used to run inside itself is *in* that mesh,
        // and a new genesis here would be a second one nobody asked for.
        for moved in layout.adopt_director_node()? {
            info!(%moved, "adopted the node Director used to run");
        }
        if let Some(note) = layout.director_node_left_behind() {
            warn!("{note}");
        }
        // A home whose mesh was reset waits to be adopted into a new one
        // (`crate::reset`). With no genesis, carrying on would found a mesh
        // here — and a supervisor restarting the node is not somebody
        // deciding to found one.
        if let Some(marker) = crate::reset::resume(&layout)? {
            return Err(CoreError::AwaitingAdoption(marker.awaiting()));
        }
        layout.ensure()?;
        let identity = Identity::load_or_generate(&config.data_dir)?;
        if identity.node_created {
            info!(node_id = %identity.node.node_id(), "generated new node identity");
        }

        // A sealed user key is not a reason to refuse to boot — a daemon has
        // nobody to ask for a passphrase, and under systemd there is no
        // terminal and no `$XDG_RUNTIME_DIR` to hold an unlock. It only
        // means this node cannot act as the founder, which is the ordinary
        // state of every node but one.
        if identity.user.is_none() {
            info!(
                "the user key here is sealed or absent; starting without a founder key \
                 (`meshnode identity status` on this home says which)"
            );
        }
        let (genesis, bootstrapped) = Genesis::load_or_bootstrap(
            &config.data_dir,
            &config.mesh_name,
            identity.user.as_ref(),
            identity.node.node_id(),
        )?;
        if bootstrapped {
            info!(mesh = %genesis.body.mesh_id, name = %genesis.body.mesh_name,
                "bootstrapped new mesh");
        }

        let engine = StateEngine::open(&config.data_dir, identity.node.clone(), genesis.clone())?;
        let me = identity.node.node_id();

        let local = LocalInfo {
            node_id: me,
            node_name: config.node_name.clone(),
            mesh_id: Some(genesis.body.mesh_id),
            mesh_name: Some(genesis.body.mesh_name.clone()),
        };

        let admission = make_admission(engine.clone());
        let (transport, events_rx) = Transport::bind(
            config.listen,
            identity.node.clone(),
            local,
            admission,
            config.bind_options(),
        )
        .await?;
        // The real bound address — authoritative over the configured one,
        // which may specify port 0 (kernel-assigned).
        let listen_addr = transport.local_addr()?;
        let base_addrs = advertised_addrs(
            config.listen.ip(),
            listen_addr.port(),
            &config.network.advertise_interfaces,
        );
        // Operator-declared public endpoints (`[wan] advertise`) are direct
        // candidates from the first announcement on (MASTERPLAN M7).
        let mut advertised = base_addrs.clone();
        for a in &config.wan.advertise {
            if !advertised.contains(a) {
                advertised.push(a.clone());
            }
        }

        // Probe accelerators once, here, and hand the same answer to both
        // consumers (C5 P2): the coarse `cap.gpus` filter tags on the
        // announcement below, and the replicated inventory the device task
        // publishes as soon as the node is up. Deriving the tag from the
        // inventory rather than probing twice is what stops the two from
        // disagreeing about how many cards this machine has.
        //
        // Off the async workers (ENG-26): the probes shell out to
        // `nvidia-smi` and `rocm-smi`, and on the box this milestone has
        // to survive — the one whose driver is wedged — each of them sits
        // in a polling loop for the full [`crate::devices::PROBE_TIMEOUT`].
        // Ten seconds of a runtime worker, on the boot path, for a
        // machine that may have no accelerator at all.
        let boot_devices = tokio::task::spawn_blocking(crate::devices::detect)
            .await
            .unwrap_or_default();
        if !boot_devices.is_empty() {
            info!(
                devices = boot_devices.len(),
                "accelerators detected: {}",
                boot_devices
                    .iter()
                    .map(mesh_proto::Device::summary)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // A node restored from a backup writes nothing of its own until a
        // peer has shown it its own history (ENG-13). With no other member
        // there is nobody to wait for: the backup is the only copy.
        if let Some(hold) = engine.restore_hold() {
            let others = engine.read(|s| {
                s.members
                    .keys()
                    .filter(|id| **id != me && s.is_active_member(id))
                    .count()
            });
            if others == 0 {
                engine.release_restore_hold("this mesh has no other member to wait for")?;
            } else {
                warn!(
                    "this node was {}: it writes nothing — not even its own address — until it \
                     has synced with one of its {others} peer(s)",
                    hold.what()
                );
            }
        }
        let held = engine.restore_hold().is_some();

        // Ensure this node is a member of its own mesh, advertising its real
        // address. The founder self-adds; a joined node refreshes its record.
        if held {
            // Made when the hold lifts; see `spawn_restore_watch`.
        } else if !engine.read(|s| s.is_active_member(&me)) {
            if me == genesis.body.founder_node {
                engine.author_op(OpBody::MemberAdd {
                    member: mesh_proto::MemberInfo {
                        node_id: me,
                        name: config.node_name.clone(),
                        addrs: advertised.clone(),
                        // No runtimes yet: the probes shell out and run
                        // off the boot path, re-announcing when they
                        // finish (T1 §5, landmine 5). An absent `cap.rt`
                        // is *unknown*, which is neutral — so the window
                        // between boot and the first probe leaves this node
                        // a candidate for everything, exactly as it was
                        // before this track.
                        tags: capacity::announced_tags_with_devices(&config, &boot_devices, &[]),
                        slots: config.limits.slots,
                        joined_at_micros: mesh_proto::now_micros(),
                    },
                    invite: None,
                })?;
            } else {
                warn!("node is not yet a member of its mesh; gossip will reconcile");
            }
        } else {
            // Refresh our advertised addresses/tags/capacity on every boot —
            // this is how a node that joined on an ephemeral port announces
            // its real listen address to the rest of the mesh.
            //
            // Tags are *merged* rather than replaced: the runtime-managed
            // ones are re-derived and everything else the mesh already
            // records for this node is kept. Replacing them wiped every tag
            // that did not come from this node's own config — which is all
            // of them for `meshnode nodes tag` and for a tag carried in a
            // join token — and it only did so on restart, so it read as
            // working right up until it did not (see `refreshed_tags`).
            //
            // What this node itself announced, not the member record: that
            // record is unioned with what admins assigned, and re-announcing
            // an assignment would make it this node's own statement — one
            // that an admin unassigning it could no longer withdraw.
            let recorded: Vec<String> = engine
                .read(|s| s.members.get(&me).map(|m| m.announced_tags.clone()))
                .unwrap_or_default();
            engine.author_op(OpBody::MemberUpdate {
                node_id: me,
                name: config.node_name.clone(),
                addrs: advertised.clone(),
                // Runtimes are re-announced by the probe task; see above.
                tags: capacity::refreshed_tags(&config, &boot_devices, &[], &recorded),
                slots: config.limits.slots,
            })?;
        }

        // Register the founding admin as a first-class user with an admin
        // grant, so `users ls`, `whoami` and audit rows show the founder
        // like anyone else. Merge-wise the founder is authorized by genesis
        // regardless — this is materialization, not privilege.
        // Skipped entirely when there is no openable user key. This is
        // materialization of a record genesis already authorizes, so a node
        // that cannot do it is not degraded — the founder is still the
        // founder, and the row appears the next time they run the CLI with
        // the key unlocked.
        if let Some(founder_key) = identity.user.as_ref()
            && founder_key.public_bytes() == genesis.body.founder_user
            && !held
        {
            let founder_uid = founder_key.user_id();
            if !engine.read(|s| s.users.contains_key(&founder_uid)) {
                let username = std::env::var("USER").unwrap_or_else(|_| "admin".into());
                let mut record = UserRecord {
                    user_id: founder_uid,
                    username,
                    created_at_micros: mesh_proto::now_micros(),
                    added_by: founder_uid,
                    signature: Vec::new(),
                };
                record.signature = founder_key.sign(&record.signing_bytes()).to_vec();
                engine.author_op(OpBody::UserAdd { record })?;
                let mut grant = Grant {
                    grant_id: GrantId::generate(),
                    user_id: founder_uid,
                    role: Role::Admin,
                    nodes: "any".into(),
                    services: "*".into(),
                    expires_at_micros: None,
                    created_at_micros: mesh_proto::now_micros(),
                    granted_by: founder_uid,
                    signature: Vec::new(),
                };
                grant.signature = founder_key.sign(&grant.signing_bytes()).to_vec();
                engine.author_op(OpBody::GrantAdd { grant })?;
            }
        }

        let (gossip, _changed_rx) = Gossip::new(engine.clone(), transport.clone());
        // The transport follows `relay:<id>` candidates through replicated
        // member records (MASTERPLAN M7).
        {
            let lookup_engine = engine.clone();
            transport.set_member_lookup(Arc::new(move |id: &NodeId| {
                lookup_engine.read(|s| {
                    s.members
                        .get(id)
                        .filter(|m| s.is_active_member(&m.info.node_id))
                        .map(|m| m.info.addrs.clone())
                })
            }));
        }
        let wan = Wan::new(
            engine.clone(),
            transport.clone(),
            gossip.clone(),
            listen_addr.port(),
            base_addrs,
            config.wan.clone(),
        );
        let content = ContentStore::open(&config.data_dir)?;
        let compute = config.compute;
        let cache_cfg = config.cache;
        let objects = ObjectStore::open(
            &config.data_dir,
            config.compute.object_ram_mb,
            config.compute.object_ttl,
        )?;
        let jobs = JobManager::new(
            engine.clone(),
            content.clone(),
            identity.node.clone(),
            config.secret_providers.clone(),
            &config.data_dir,
            config.limits,
            config.quiet.clone(),
            capacity::of(&config, &[]),
            config.runtime.sandbox.clone(),
        );

        jobs.set_devices(boot_devices);
        jobs.set_output_limit(config.disk.job_output_max_bytes());
        // N2 P1. The port range and the reserved list, plus the address a
        // job is told to advertise itself on — which is `advertise` where
        // one is configured and the listen address otherwise, because a
        // job behind NAT that advertised its listen address would be
        // publishing an address nothing can reach.
        jobs.set_network(
            config.network.clone(),
            advertise_host(&config),
            job_bind_host(&config),
        );
        jobs.set_container_plugin(config.runtime.container_plugin.clone());

        // Re-announce any bundles this node already holds on disk so a
        // restart re-establishes our holds in replicated state.
        reannounce_local_bundles(&engine, &content);

        // Derived data, opened best-effort: a node that cannot write its
        // index logs it once and carries on. Treating a cache as fatal would
        // trade the thing that must not stop for the thing that may.
        let history = if config.history.enabled {
            match crate::history::HistoryStore::open(&layout.history()) {
                Ok(store) => Some(Arc::new(store)),
                // **Rebuild rather than give up** (ENG-26). The index is
                // derived data: every run, job, audit row and fact in it
                // comes from the op-log, which is still on this disk. A
                // file that will not open is therefore a file to replace,
                // not a capability to switch off for the life of the
                // process — and switching it off meant `meshnode history`
                // fell back to a full scan on every query until somebody
                // noticed a line in a week-old journal.
                Err(e) => {
                    warn!("history index will not open ({e}); rebuilding it from the op-log");
                    let path = layout.history();
                    let _ = std::fs::remove_file(&path);
                    match crate::history::HistoryStore::open(&path) {
                        Ok(store) => {
                            info!("history index rebuilt");
                            Some(Arc::new(store))
                        }
                        Err(e) => {
                            warn!(
                                "history index unavailable even after a rebuild, queries will \
                                 fall back to a scan: {e}"
                            );
                            None
                        }
                    }
                }
            }
        } else {
            None
        };

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (exit_tx, _exit_rx) = watch::channel(false);
        let vitals = Arc::new(crate::vitals::Vitals::new());
        vitals.adopt(transport.accept_monitor());
        let store_pulse = vitals.register("op-log store", None);
        let retention = config.retention;
        let live = Arc::new(config.clone());
        let placement = config.compute.placement;
        let http_cfg = config.http_addr;
        let api_cfg = config.api_addr;
        let node = Node {
            inner: Arc::new(Inner {
                jwks: crate::oidc::Jwks::new(),
                invite_pull: Mutex::new(HashMap::new()),
                secret_memo: Mutex::new(HashMap::new()),
                config,
                identity,
                engine,
                transport,
                gossip: gossip.clone(),
                cpu_sampler: capacity::CpuSampler::new(),
                net_sampler: capacity::NetSampler::new(),
                jobs,
                content,
                disk_critical: AtomicBool::new(false),
                objects,
                objdir: ObjectDirectory::new(compute.object_ttl),
                actions: crate::cache::ActionCache::new(
                    cache_cfg.enabled,
                    cache_cfg.epoch,
                    cache_cfg.verify_rate,
                    cache_cfg.max_entries,
                ),
                pending_explains: Mutex::new(Vec::new()),
                published_actions: Mutex::new(Considered::default()),
                verified_plans: Mutex::new(VerifiedPlans::default()),
                anon: crate::throttle::AnonLimits::default(),
                history,
                listen_addr,
                started: Instant::now(),
                nonces: Mutex::new(NonceWheel::default()),
                pair_sessions: Mutex::new(HashMap::new()),
                pair_next: AtomicU64::new(1),
                active_drives: Mutex::new(HashSet::new()),
                active_gangs: Mutex::new(HashSet::new()),
                active_syncs: Mutex::new(HashSet::new()),
                prefetching: Mutex::new(HashSet::new()),
                upgrading: Mutex::new(None),
                draining: AtomicBool::new(false),
                drain_until: Mutex::new(None),
                shutdown_tx,
                exit_tx,
                resetting: AtomicBool::new(false),
                left_mesh: Mutex::new(None),
                tasks: Mutex::new(Vec::new()),
                retention: Mutex::new(retention),
                census: Mutex::new(None),
                live: parking_lot::RwLock::new(live),
                reloading: Mutex::new(()),
                dispatch: crate::latency::Histogram::default(),
                ipc_latency: crate::latency::Histogram::default(),
                placement: Mutex::new(placement),
                http_addr: Mutex::new(None),
                api_addr: Mutex::new(None),
                wan: wan.clone(),
                parallel: parallel::ParallelRegistry::default(),
                working: parallel::WorkerRegistry::default(),
                lanes: AtomicU32::new(0),
                flap: Arc::new(crate::facts::FlapGuard::new()),
                sessions: Arc::new(crate::sessions::Registry::new()),
                placements: Mutex::new(crate::deployments::PlacementMemory::default()),
                vitals,
                dispatch_failures: DispatchFailures::default(),
                groups: crate::observe::GroupMeters::default(),
                accepting: Mutex::new(HashMap::new()),
                store_pulse: Mutex::new(Some(store_pulse)),
            }),
        };

        // The fabric (M17 §3.3): the transport hands both halves of a
        // `SessionOpen` stream here. Installed with the same idiom as the
        // punch handler — the transport knows nothing about RBAC, reserved
        // tags or audit, and this closure is where all three live.
        //
        // **Weak, and it has to be.** The transport is owned by `Inner`, so
        // a handler holding a `Node` (which is an `Arc<Inner>`) closes a
        // reference cycle: `Inner → transport → handler → Inner`. Nothing
        // would ever drop, the redb store's exclusive lock would outlive
        // `stop()`, and a restart on the same data dir would fail with
        // "Database already open" — which is precisely how this was found.
        // `wan.rs` downgrades for the punch handler for the same reason.
        {
            let weak = Arc::downgrade(&node.inner);
            node.inner
                .transport
                .set_session_handler(Arc::new(move |peer, spec, send, recv| {
                    let Some(inner) = weak.upgrade() else {
                        // The node is shutting down; the stream closing is
                        // the initiator's answer.
                        return;
                    };
                    let node = Node { inner };
                    tokio::spawn(async move {
                        node.serve_wire_session(peer, spec, send, recv).await;
                    });
                }));
        }
        // Dispatch inbound wire requests.
        node.clone().spawn_dispatcher(events_rx);
        // T1. Find out what this node can actually run, then say so.
        //
        // Off the boot path, because the probes shell out — five processes,
        // one of them a container engine that may take a second to answer —
        // and a node's return to the mesh must not wait on them (landmine
        // 5). The window costs nothing: until `cap.rt` is announced the
        // node reads as *unknown*, which is neutral, so it is a candidate
        // for everything exactly as it was before this track. When the
        // probes land, the list narrows to the truth.
        //
        // This also installs the detected container engine, which is what
        // makes `runtime = "container"` work on a machine with podman or
        // docker and no configuration whatsoever.
        node.clone().spawn_runtime_probe();
        node.clone().spawn_restore_watch();
        node.clone().spawn_reset_watch();

        // Start gossip. A round takes one tick plus its syncs, each of which
        // is bounded by the dial and request deadlines; two minutes without
        // one is a loop that is stuck, not slow.
        let gossip_pulse = node
            .inner
            .vitals
            .register("gossip", Some(GOSSIP_STALL_AFTER));
        node.track(tokio::spawn(gossip.run(shutdown_rx, gossip_pulse)));
        // WAN reachability: reflexive discovery, relay roles (MASTERPLAN M7).
        node.track(tokio::spawn(wan.run(node.inner.shutdown_tx.subscribe())));

        // N3 P3. Every node holds the whole catalogue, so every node
        // answers `<service>.mesh` authoritatively with no round trip —
        // the property worth having, and the reason this is on by default
        // rather than a role some nodes take.
        if let Some(bind) = node.inner.config.network.dns_bind {
            let engine_for_dns = node.inner.engine.clone();
            let stop = node.inner.shutdown_tx.subscribe();
            // Phase A, §2.3: an instance whose node is Dead is not handed
            // out, whatever health its node last published.
            let liveness = node.clone();
            node.track(tokio::spawn(async move {
                crate::dns::serve(engine_for_dns, bind, stop, move |id| {
                    liveness.node_is_dead(id)
                })
                .await;
            }));
        } else if node.inner.config.network.dns {
            warn!(
                "DNS is enabled but [network] dns_bind is not a usable address — \
                 nothing will answer .{}",
                mesh_proto::DNS_SUFFIX
            );
        }
        // Announce ourselves eagerly: until our boot-time MemberUpdate (the
        // op carrying our real address) reaches a member, nobody can dial
        // us — a fresh joiner or restarted node would otherwise depend on
        // its own random gossip landing on a known-dialable peer. Push it
        // to a few members right away (D12: at 1,000 nodes this is what
        // keeps join → converged in seconds).
        node.clone().spawn_boot_announce();
        // Heal secret wraps for members that joined after sealing.
        node.clone().spawn_rewrap_task();
        node.clone().spawn_compaction_task();
        node.clone().spawn_usage_task();
        // Publish this node's accelerators, and re-publish them when the
        // hardware or a driver changes (C5 P2/P6).
        node.clone().spawn_device_task();
        // Adopt fan-outs whose owner died (MASTERPLAN §10.3) — including our
        // own unfinished ones after a restart.
        node.clone().spawn_adoption_task();
        // Watch registered Git sources when elected (MASTERPLAN §8.1).
        node.clone().spawn_git_task();
        // Describe this machine, and re-describe it when it changes
        // (M18 §4.5). Runs on every node, elected by nobody.
        node.clone().spawn_facts_task();
        // Reconcile deployments this node is elected for (C2 §6.2).
        node.clone().spawn_deployment_task();
        node.clone().spawn_deployment_health_task();
        // Enforce job-history retention (MASTERPLAN §9.1) and re-check the
        // memory admission gate so queued work is promoted when pressure
        // clears (MASTERPLAN §11).
        node.clone().spawn_maintenance_task();
        node.clone().spawn_sink_task();
        // Switch the per-entity op-rate limiter on (M31 A3). It is off in
        // `StateEngine::open` on purpose: that constructor is also how
        // offline tools hold a data dir, and a tool replaying a log must
        // never be rate limited by the config of whichever node's directory
        // it happens to be pointed at.
        node.inner
            .engine
            .rate_guard()
            .set_limit(node.inner.config.state.op_rate_per_minute);
        node.clone().spawn_backpressure_task();
        node.clone().spawn_history_task();
        // Separately, and unconditionally: the op-log flush and the action
        // cache do not depend on the query index existing (ENG-26).
        node.clone().spawn_durability_task();
        // Local observability endpoint (MASTERPLAN §12).
        if let Some(addr) = http_cfg {
            match crate::http::bind(addr).await {
                Ok((listener, local)) => {
                    *node.inner.http_addr.lock() = Some(local);
                    let handle = tokio::spawn(crate::http::serve(
                        listener,
                        node.clone(),
                        node.inner.shutdown_tx.subscribe(),
                    ));
                    node.track(handle);
                    info!(http = %local, "healthz/metrics endpoint up");
                }
                // Never fail the daemon over observability (a second node on
                // the same host races for the default port).
                Err(e) => warn!(addr = %addr, "cannot bind healthz/metrics endpoint: {e}"),
            }
        }
        // The HTTP API (E1-P6). Off unless an operator set `[api] listen`.
        //
        // **A hard failure, unlike the observability endpoint above.** That
        // one degrades to nothing because a second node racing for 7776 is
        // not worth refusing to start over. This one is a management
        // surface somebody deliberately asked for — starting without it,
        // quietly, would mean an operator's automation silently talks to
        // nothing.
        if let Some(addr) = api_cfg {
            match crate::api::bind(addr).await {
                Ok((listener, local)) => {
                    *node.inner.api_addr.lock() = Some(local);
                    let handle = tokio::spawn(crate::api::serve(
                        listener,
                        node.clone(),
                        node.inner.shutdown_tx.subscribe(),
                    ));
                    node.track(handle);
                    info!(api = %local, "http api up (signed envelopes only)");
                }
                Err(e) => {
                    return Err(CoreError::Config(format!(
                        "cannot bind the api listener on {addr}: {e}"
                    )));
                }
            }
        }

        info!(
            node = %node.inner.engine.node_id().short(),
            name = %node.inner.config.node_name,
            mesh = %genesis.body.mesh_name,
            listen = %listen_addr,
            "meshware node up"
        );
        Ok(node)
    }

    pub fn node_id(&self) -> NodeId {
        self.inner.engine.node_id()
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.inner.listen_addr
    }

    /// The configuration this node booted with. For anything a reload can
    /// change, see [`Self::live_config`].
    pub fn config(&self) -> &NodeConfig {
        &self.inner.config
    }

    /// The configuration as it is in effect now: the boot config with each
    /// reload's hot-reloadable sections applied (ENG-11).
    pub fn live_config(&self) -> Arc<NodeConfig> {
        self.inner.live.read().clone()
    }

    pub fn engine(&self) -> &Arc<StateEngine> {
        &self.inner.engine
    }

    pub fn transport(&self) -> &Arc<Transport> {
        &self.inner.transport
    }

    /// This node's job manager — and, through it, its lease table (C4 P1).
    ///
    /// Public because a lease is deliberately node-local: there is no
    /// replicated record to inspect and no request that reports one, so a
    /// test (or the Director's local view) that wants to know what this
    /// node is holding has to ask the node.
    pub fn jobs(&self) -> &Arc<JobManager> {
        &self.inner.jobs
    }

    pub fn gossip(&self) -> &Arc<Gossip> {
        &self.inner.gossip
    }

    /// WAN reachability state (MASTERPLAN M7).
    pub fn wan(&self) -> &Arc<Wan> {
        &self.inner.wan
    }

    /// Test-only access to the founder/user key for minting invites in
    /// `mesh-testkit`. Production callers use the IPC `InviteCreate` path.
    ///
    /// Panics when this node holds no openable user key. A test that founded
    /// a mesh always has one; a test that reaches this on a node without one
    /// is asking the wrong node.
    #[doc(hidden)]
    pub fn user_key_for_test(&self) -> &crate::identity::Keypair {
        self.inner
            .identity
            .user
            .as_ref()
            .expect("this node holds no openable user key")
    }

    /// Test-only access to the node key, so `mesh-testkit` can seal an op
    /// this build could not otherwise author — an op body from a future
    /// protocol version (F0). Nothing in production signs ops except
    /// [`StateEngine::author_op`].
    #[doc(hidden)]
    pub fn node_key_for_test(&self) -> &crate::identity::Keypair {
        &self.inner.identity.node
    }

    /// Test-only: exercise the receiving half of a fleet upgrade (M24 §4)
    /// without a second process. Production callers reach it through
    /// `Request::UpgradeSelf`, which is where the authorization it applies
    /// actually matters.
    #[doc(hidden)]
    pub fn serve_upgrade_self_for_test(&self, plan_id: mesh_proto::UpgradeId) -> Response {
        self.serve_upgrade_self(plan_id)
    }

    /// Test-only access to the job manager (retention sweeps in tests).
    #[doc(hidden)]
    /// Run one reclamation pass now, instead of waiting for the timer.
    pub async fn reclaim_pass_for_test(&self) {
        self.reclaim_pass().await;
    }

    pub fn jobs_for_test(&self) -> &Arc<JobManager> {
        &self.inner.jobs
    }

    /// Test-only: give this node a synthetic device inventory and publish
    /// it, exactly as a real probe would (C5 P2).
    ///
    /// Test-only because the real path shells out to `nvidia-smi`, and a CI
    /// runner has no GPU in it. Everything downstream of the probe — the
    /// entity, the filter tags, admission, allocation, isolation — is the
    /// production path, which is the half worth testing.
    #[doc(hidden)]
    pub fn advertise_devices_for_test(&self, devices: Vec<mesh_proto::Device>) {
        self.inner.jobs.set_devices(devices.clone());
        self.reannounce_device_tags(&devices);
        self.publish_device_inventory(devices);
    }

    /// Test-only: hand this node synthetic device telemetry, as a probe
    /// would (C5 P5).
    #[doc(hidden)]
    pub fn set_device_pressure_for_test(&self, pressure: Vec<mesh_proto::DevicePressure>) {
        self.inner.jobs.set_device_pressure(pressure);
    }

    /// Test-only: run the ephemeral-object sweep now rather than waiting
    /// out the maintenance tick (M9 §5.5).
    #[doc(hidden)]
    pub fn sweep_objects_for_test(&self) {
        self.sweep_objects();
    }

    /// Test-only: how many ephemeral objects this node is holding, and how
    /// many directory entries it indexes for others.
    #[doc(hidden)]
    pub fn object_counts_for_test(&self) -> (usize, usize) {
        (self.inner.objects.len(), self.inner.objdir.len())
    }

    /// Immediate shutdown of the transport (legacy path; prefer [`stop`]).
    pub fn shutdown(&self) {
        self.inner.vitals.stopping();
        let _ = self.inner.shutdown_tx.send(true);
        self.inner.transport.close();
    }

    /// Let work already accepted here finish before the node goes down
    /// (ENG-15). Returns what was still running when the grace ended.
    ///
    /// `systemctl restart` is the documented package-upgrade path, so the
    /// stop path is an upgrade path, and a stop that kills running jobs
    /// makes every upgrade an outage. This stops admitting new work — a
    /// peer dispatching here gets `Busy` and places the job elsewhere,
    /// exactly as it would for a full node — and then waits out the jobs
    /// this node already owns, bounded by `[shutdown] grace_secs`.
    ///
    /// Deliberately *not* a drain: nothing is announced mesh-wide and
    /// nothing is moved. A restart lasts seconds, and a membership change
    /// per restart would cost every peer more than it saves. `meshnode
    /// drain` is still how a node is emptied on purpose.
    ///
    /// The unit pairs this with `KillMode=mixed`, without which systemd
    /// signals every job process directly and there is nothing left to
    /// wait for.
    pub async fn shutdown_grace(&self) -> usize {
        let grace = self.inner.config.shutdown.grace;
        // Already draining: that path has its own deadline and has waited
        // for this work once. Waiting again only delays the exit.
        if grace.is_zero() || self.inner.draining.swap(true, Ordering::SeqCst) {
            return 0;
        }
        let running = self.inner.jobs.running_count();
        if running == 0 {
            return 0;
        }
        info!(
            jobs = running,
            grace_secs = grace.as_secs(),
            "not accepting new work; waiting for running jobs before stopping"
        );
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.inner.jobs.running_count() == 0 {
                info!("running jobs finished; stopping");
                return 0;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Left running, and about to be killed by the service manager.
        // Saying so is the difference between an operator reading the next
        // start's "orphaned by restart" lines as an explanation and reading
        // them as a bug.
        let left = self.inner.jobs.running_count();
        warn!(
            jobs = left,
            grace_secs = grace.as_secs(),
            "shutdown grace expired with jobs still running; they will be \
             marked failed on the next start"
        );
        left
    }

    /// Stop the node completely: signal every background loop, close the
    /// transport, and reap the tasks so the state store's exclusive lock is
    /// actually released (a restart on the same data dir depends on it).
    pub async fn stop(&self) {
        self.inner.vitals.stopping();
        let _ = self.inner.shutdown_tx.send(true);
        // Parallel chunk queues stop handing work out first (M9 §5.4): a
        // lane that pulls one more chunk on the way down does work whose
        // result this node will never record, and someone else will have
        // to redo.
        self.inner.parallel.cancel_all();
        self.inner.transport.close();
        let handles: Vec<_> = std::mem::take(&mut *self.inner.tasks.lock());
        for mut handle in handles {
            // Loops exit at their next yield; anything stuck gets aborted.
            // Aborts land on await points only, so no store transaction is
            // ever cut mid-write.
            if tokio::time::timeout(Duration::from_secs(3), &mut handle)
                .await
                .is_err()
            {
                handle.abort();
            }
        }
        // Last, once every writer has stopped: land anything written
        // without waiting for the disk (**H-3**). A clean shutdown should
        // never be the reason a peer's ops have to be fetched again — the
        // relaxed path is there to survive a crash, not to make an orderly
        // stop lossy.
        if let Err(e) = self.inner.engine.flush() {
            warn!("op-log flush on shutdown failed: {e}");
        }
    }

    /// Which of this node's critical loops are alive and moving (ENG-1,
    /// ENG-2).
    pub fn vitals(&self) -> &Arc<crate::vitals::Vitals> {
        &self.inner.vitals
    }

    fn track(&self, handle: tokio::task::JoinHandle<()>) {
        self.inner.tasks.lock().push(handle);
    }

    /// True once a drain has begun (MASTERPLAN §12).
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::SeqCst)
    }

    /// The reset this node has stopped to leave for, once it has.
    pub fn left_mesh(&self) -> Option<mesh_proto::MeshReset> {
        self.inner.left_mesh.lock().clone()
    }

    /// Subscribe to the "drain finished — exit the daemon" signal.
    pub fn exit_requested(&self) -> watch::Receiver<bool> {
        self.inner.exit_tx.subscribe()
    }

    /// Real bound address of the healthz/metrics endpoint, when it is up.
    pub fn http_addr(&self) -> Option<SocketAddr> {
        *self.inner.http_addr.lock()
    }

    /// Where the E1-P6 API listener bound, if one was configured.
    pub fn api_addr(&self) -> Option<SocketAddr> {
        *self.inner.api_addr.lock()
    }

    // -- request authentication (MASTERPLAN §7.1) ---------------------------

    /// Verify a signed request envelope: signature, freshness, replay.
    /// Returns the authenticated user. Whether that user may do anything is
    /// a separate policy question, answered per request.
    fn verify_envelope(&self, env: &SignedIpc) -> std::result::Result<UserId, String> {
        if <[u8; 64]>::try_from(env.signature.as_slice()).is_err() {
            return Err("malformed request signature".into());
        }
        // Unbound (`mw-ipc-v1`), or bound to this node of this mesh — never
        // to another (SEC-12).
        if self.envelope_binding(env).is_none() {
            return Err("invalid request signature".into());
        }
        let now = mesh_proto::now_micros();
        let skew = u64::try_from(AUTH_SKEW.as_micros()).unwrap_or(u64::MAX);
        if env.at_micros.abs_diff(now) > skew {
            return Err("request timestamp outside the freshness window".into());
        }
        let mut nonces = self.inner.nonces.lock();
        if !nonces.accept((env.user, env.nonce), now, skew) {
            return Err("replayed request nonce".into());
        }
        drop(nonces);
        // A task this node is running, calling back into the mesh it is
        // running in. It signs with the credential this node minted for
        // its job (`JobManager::mint_job_credential`), and that credential
        // speaks for the user who submitted the job — an authorization
        // this node verified out of `JobSpec::auth` before it ever agreed
        // to run the thing.
        //
        // Local to this node and to that job. There is no record of the
        // key anywhere, no peer is asked to honour it, and the same
        // envelope arriving at any other node is an unknown user. The
        // *effect* is entirely ordinary: from here on this request is the
        // submitter's, gated by the submitter's grants and audited under
        // the submitter's name, which is what a person running a workflow
        // means by "its steps are mine".
        Ok(self.inner.jobs.acting_user(&env.user).unwrap_or(env.user))
    }

    /// How `env` was signed, if validly for this node (SEC-12).
    fn envelope_binding(&self, env: &SignedIpc) -> Option<crate::identity::IpcBinding> {
        crate::identity::verify_ipc(env, self.mesh_id(), &[self.node_id()])
    }

    fn mesh_id(&self) -> mesh_proto::MeshId {
        self.inner.engine.genesis().body.mesh_id
    }

    /// Role gate for one request. On denial, audits the attempt (when the
    /// action is worth auditing) and returns the response to send.
    fn gate(
        &self,
        user: &UserId,
        allowed: &[Role],
        action: &str,
        target: &str,
        env: &SignedIpc,
        audit_denial: bool,
    ) -> std::result::Result<(), Box<IpcResponse>> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let verdict = self
            .inner
            .engine
            .read(|s| rbac::require_role(s, &founder, user, allowed, mesh_proto::now_micros()));
        match verdict {
            // An action worth auditing waits until this node can write its
            // audit trail again (SEC-6): carrying it out unrecorded would be
            // the one outcome the trail exists to prevent.
            Ok(()) if audit_denial => match self.inner.engine.write_failure() {
                None => Ok(()),
                Some(why) => Err(Box::new(IpcResponse::Error {
                    message: format!(
                        "this node cannot write to its log right now ({why}), so it is refusing \
                         {action} rather than doing it unrecorded"
                    ),
                })),
            },
            Ok(()) => Ok(()),
            Err(message) => {
                if audit_denial {
                    audit::record(
                        &self.inner.engine,
                        *user,
                        action,
                        target,
                        &format!("denied: {message}"),
                        "",
                        &env.signature,
                    );
                }
                Err(Box::new(IpcResponse::Denied { message }))
            }
        }
    }

    /// The cancel gate ([`rbac::require_cancel`]): admins and operators
    /// cancel anything, developers their own jobs, and a refusal names the
    /// rule and who *can* — "denied" without "who to ask" is a dead end
    /// with an audit entry.
    #[allow(clippy::too_many_arguments)]
    fn gate_cancel(
        &self,
        user: &UserId,
        submitter: &UserId,
        submitter_name: Option<&str>,
        force: bool,
        action: &str,
        target: &str,
        env: &SignedIpc,
    ) -> std::result::Result<(), Box<IpcResponse>> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let verdict = self.inner.engine.read(|s| {
            rbac::require_cancel(
                s,
                &founder,
                user,
                submitter,
                submitter_name,
                force,
                mesh_proto::now_micros(),
            )
        });
        match verdict {
            Ok(()) => Ok(()),
            Err(message) => {
                audit::record(
                    &self.inner.engine,
                    *user,
                    action,
                    target,
                    &format!("denied: {message}"),
                    "",
                    &env.signature,
                );
                Err(Box::new(IpcResponse::Denied { message }))
            }
        }
    }

    /// A job's submitter and their username, for the cancel gate and its
    /// refusal prose.
    fn job_submitter(&self, job_id: &mesh_proto::JobId) -> Option<(UserId, Option<String>)> {
        self.inner.engine.read(|s| {
            s.jobs.get(job_id).map(|j| {
                (
                    j.submitter_user,
                    s.users
                        .get(&j.submitter_user)
                        .map(|u| u.record.username.clone()),
                )
            })
        })
    }

    fn audit_ok(&self, user: &UserId, action: &str, target: &str, detail: &str, env: &SignedIpc) {
        audit::record(
            &self.inner.engine,
            *user,
            action,
            target,
            "ok",
            detail,
            &env.signature,
        );
    }

    /// Handle one signed request that produces a single response.
    ///
    /// The handler runs beside the reader (SEC-12). It used to be awaited
    /// first, into a sixteen-frame channel nobody read until it returned —
    /// so a streamed answer longer than that (a run's output, a fan-out)
    /// blocked it forever, and over the HTTP API every such call held one
    /// of the listener's permits until restart. A caller of this function
    /// gets the first frame; the rest of a streamed answer is drained, so
    /// the handler finishes, and the work goes on.
    pub async fn handle_signed(&self, env: SignedIpc) -> IpcResponse {
        self.handle_signed_from(env, Source::Local).await
    }

    async fn handle_signed_from(&self, env: SignedIpc, source: Source) -> IpcResponse {
        // Timed to the *first* response, which is what a caller waits on:
        // a streaming request is answered as its first frame arrives, and
        // measuring to the last one would measure the job, not the daemon
        // (ENG-21).
        let began = Instant::now();
        let (tx, mut rx) = mpsc::channel(16);
        let node = self.clone();
        tokio::spawn(async move { node.handle_signed_stream_from(env, tx, source).await });
        let first = rx.recv().await;
        self.inner.ipc_latency.record(began.elapsed());
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        first.unwrap_or(IpcResponse::Error {
            message: "no response produced".into(),
        })
    }

    /// Handle one signed request, streaming responses to `out`. Streaming
    /// requests (`Run`, `RunService`, `JobLogs`) emit many frames; everything
    /// else emits exactly one.
    pub async fn handle_signed_stream(&self, env: SignedIpc, out: mpsc::Sender<IpcResponse>) {
        self.handle_signed_stream_from(env, out, Source::Local)
            .await;
    }

    /// [`Self::handle_signed_stream`], knowing where the request came from
    /// — which only the requests open to anybody care about (SEC-19).
    ///
    /// **The correlation boundary** (ENG-21). One span per request, with a
    /// [`TraceId`] minted here, so every line logged while this request is
    /// handled — through the gate, the scheduler, two spawned tasks and
    /// whatever it dispatches — carries the same `trace=`. Before this,
    /// the way to find out what a node did for one `meshnode run` was to
    /// read the log and guess by timestamp.
    ///
    /// The id is node-local and deliberately so: across nodes the join is
    /// the job, group or gang id, which the plan already replicates and
    /// the executor already journals. A synthetic id pushed over the wire
    /// would need a protocol field to carry what those already say.
    ///
    /// [`TraceId`]: crate::observe::TraceId
    async fn handle_signed_stream_from(
        &self,
        env: SignedIpc,
        out: mpsc::Sender<IpcResponse>,
        source: Source,
    ) {
        let span = tracing::info_span!(
            "ipc",
            trace = %crate::observe::TraceId::new(),
            // Filled in once the envelope verifies and the payload
            // decodes, which is also when a refusal stops being anonymous.
            req = tracing::field::Empty,
            user = tracing::field::Empty,
        );
        self.handle_signed_stream_inner(env, out, source)
            .instrument(span)
            .await;
    }

    async fn handle_signed_stream_inner(
        &self,
        env: SignedIpc,
        out: mpsc::Sender<IpcResponse>,
        source: Source,
    ) {
        let user = match self.verify_envelope(&env) {
            Ok(u) => u,
            Err(message) => {
                let _ = out.send(IpcResponse::Error { message }).await;
                return;
            }
        };
        let req: IpcRequest = match postcard::from_bytes(&env.payload) {
            Ok(r) => r,
            Err(e) => {
                // **Name the likely cause** (ENG-24). A newer `meshnode`
                // against an older daemon sends a request variant this
                // build's enum does not have, and postcard reports that as
                // a decode error — so the operator got "malformed request
                // payload: …" for the ordinary mid-upgrade state of having
                // upgraded the CLI first. The envelope carries no version
                // to check against, and adding one to a local socket whose
                // two ends are normally the same build would be a wire
                // change to say what this sentence says.
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!(
                            "this daemon could not read the request ({e}).\n\n\
                             The usual cause is a `meshnode` CLI from a different release than \
                             the daemon it is talking to. This daemon is {VERSION} (protocol \
                             {}.{}); compare it with `meshnode --version`, and upgrade whichever \
                             is behind.",
                            mesh_proto::PROTOCOL_MAJOR,
                            mesh_proto::PROTOCOL_MINOR,
                        ),
                    })
                    .await;
                return;
            }
        };
        {
            let span = tracing::Span::current();
            span.record("req", request_kind(&req));
            span.record("user", tracing::field::display(user.short()));
        }
        if let Some(message) = self.credential_refusal(&env, &req) {
            let _ = out.send(IpcResponse::Denied { message }).await;
            return;
        }
        match req {
            // -- M10 runs, M11 change control -------------------------------
            //
            // Every execution path funnels through `dispatch_submission`, and
            // `--dry-run` / `--break-glass` are *wrappers around the request
            // the caller would otherwise have sent* rather than flags beside
            // it (M11 §5.2). That is what makes the preview a preview of the
            // real submission instead of a second, preview-shaped one that
            // can drift.
            IpcRequest::Run { .. }
            | IpcRequest::RunService { .. }
            | IpcRequest::RunScript { .. }
            | IpcRequest::Parallel { .. } => {
                self.dispatch_submission(user, env, req, SubmitMode::Normal, out)
                    .await;
            }
            IpcRequest::DryRun { request } => {
                self.dispatch_submission(user, env, *request, SubmitMode::DryRun, out)
                    .await;
            }
            IpcRequest::ExplainKey { request } => {
                self.dispatch_submission(user, env, *request, SubmitMode::ExplainKey, out)
                    .await;
            }
            // Retired with change control. Still decoded — a job submitted
            // with it before the removal carries it in its `auth` — but no
            // longer accepted.
            IpcRequest::BreakGlass { .. } => {
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!("--break-glass: {CHANGE_CONTROL_RETIRED}"),
                    })
                    .await;
            }
            IpcRequest::PairApprove { addr, token } => {
                if let Err(resp) =
                    self.gate(&user, rbac::ADMIN_ONLY, "node.pair", &addr, &env, true)
                {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.pair_approve_stream(user, env, addr, token, out).await;
            }
            // -- M24 §3.2 -----------------------------------------------------
            //
            // A content role, the same door pushing the bundle used. It is
            // not an admin act: prefetching cannot change what any node
            // will run, only when it already has the bytes to run it.
            IpcRequest::BundlePrefetch {
                bundle_id,
                selector,
            } => {
                if let Err(resp) = self.gate(
                    &user,
                    rbac::CONTENT,
                    "bundle.prefetch",
                    &bundle_id.to_display_string(),
                    &env,
                    true,
                ) {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.prefetch_stream(bundle_id, selector, out).await;
            }
            // Reading a bundle file is a read, exactly as
            // `BundleFileRead` is — this variant differs only in how many
            // frames the answer takes, and how many frames an answer takes
            // is not an authority question.
            IpcRequest::BundleFileFetch { bundle_id, path } => {
                if let Err(resp) = self.gate(&user, rbac::READ, "bundles", &path, &env, false) {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.stream_bundle_file(bundle_id, path, out).await;
            }
            // -- M24 §4 -------------------------------------------------------
            //
            // Admin-only, because changing what the mesh *is* is not an
            // operator's act — it is the one thing in M24 that is not
            // "definitions", and the whole document turns on keeping the
            // two apart.
            IpcRequest::UpgradeFleet { ref plan, force } => {
                let plan = plan.clone();
                if let Err(resp) = self.gate(
                    &user,
                    rbac::ADMIN_ONLY,
                    "node.upgrade",
                    &format!(
                        "{} -> {}{}",
                        plan.selector,
                        plan.version,
                        if force { " (forced)" } else { "" }
                    ),
                    &env,
                    true,
                ) {
                    let _ = out.send(*resp).await;
                    return;
                }
                if plan.created_by != user {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: "the upgrade plan is signed by a different user".into(),
                        })
                        .await;
                    return;
                }
                self.upgrade_stream(*plan, force, out).await;
            }
            IpcRequest::Drain { move_pending } => {
                // Operators drain nodes (MASTERPLAN §7.2 role table).
                if let Err(resp) = self.gate(
                    &user,
                    &[Role::Admin, Role::Operator],
                    "node.drain",
                    "",
                    &env,
                    true,
                ) {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.drain_stream(
                    user,
                    env,
                    move_pending,
                    mesh_proto::DEFAULT_DRAIN_DEADLINE_SECS,
                    false,
                    out,
                )
                .await;
            }
            // Phase C, E9: the same drain, with a deadline that stops what is
            // left when it passes.
            IpcRequest::DrainDeadline {
                move_pending,
                deadline_secs,
            } => {
                if let Err(resp) = self.gate(
                    &user,
                    &[Role::Admin, Role::Operator],
                    "node.drain",
                    "",
                    &env,
                    true,
                ) {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.drain_stream(user, env, move_pending, deadline_secs, true, out)
                    .await;
            }
            // Founder only, at this machine's socket, by a person: the gate
            // lets admins through and the founder is one, and the rest is
            // `reset_stream`'s to check. Every node checks the founder's
            // signature again when the op reaches it.
            IpcRequest::MeshReset { reset } => {
                if !matches!(source, Source::Local) {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: RESET_LOCAL_ONLY.into(),
                        })
                        .await;
                    return;
                }
                if let Err(resp) = self.gate(&user, rbac::ADMIN_ONLY, "mesh.reset", "", &env, true)
                {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.reset_stream(user, env, reset, out).await;
            }
            IpcRequest::ObjectGet { id, hints } => {
                if let Err(resp) = self.gate(&user, rbac::READ, "object.get", "", &env, false) {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.object_stream(id, hints, out).await;
            }
            IpcRequest::JobLogs { job_id, follow } => {
                if let Err(resp) = self.gate(&user, rbac::READ, "job.logs", "", &env, false) {
                    let _ = out.send(*resp).await;
                    return;
                }
                // M31 A2. Three outcomes, and the product used to collapse
                // two of them into the most damaging sentence it has.
                let me = self.node_id();
                let found = self.inner.engine.read(|s| {
                    s.jobs.get(&job_id).map(|j| {
                        (
                            j.executor,
                            j.state,
                            j.exit_code,
                            self.output_availability_in(s, j, me),
                        )
                    })
                });
                match found {
                    Some((exec, _, _, avail)) if avail.is_available() => {
                        // The person's own request goes along when there is
                        // a person — not a job credential this node mapped,
                        // which no executor could resolve.
                        let signed = (env.user == user).then_some(&env);
                        self.stream_job_logs_as(job_id, exec, follow, signed, out)
                            .await;
                    }
                    // The record is here and the payload is not. Say which
                    // of expired / unavailable happened, then still report
                    // the job's **real** terminal state — a successful job
                    // whose log aged out is a successful job.
                    Some((_, state, exit_code, availability)) => {
                        let _ = out
                            .send(IpcResponse::LogsUnavailable { availability })
                            .await;
                        let _ = out.send(IpcResponse::JobDone { state, exit_code }).await;
                    }
                    // Genuinely nothing: not in state, and the local index
                    // has no memory of it either. Only now is "no such job"
                    // the truth.
                    None => {
                        let known = self
                            .inner
                            .history
                            .as_ref()
                            .and_then(|h| h.job(&job_id).ok().flatten());
                        match known {
                            // A1 pruned the replicated record but this
                            // node's index still remembers the job. The
                            // record survives; only the payload is gone.
                            Some(row) => {
                                let _ = out
                                    .send(IpcResponse::LogsUnavailable {
                                        availability: mesh_proto::OutputAvailability::Expired {
                                            when_micros: row.finished_at_micros.unwrap_or(0),
                                            reason: mesh_proto::ExpiryReason::Retention,
                                        },
                                    })
                                    .await;
                                let _ = out
                                    .send(IpcResponse::JobDone {
                                        state: row.state,
                                        exit_code: row.exit_code,
                                    })
                                    .await;
                            }
                            None => {
                                let _ = out
                                    .send(IpcResponse::Error {
                                        message: format!(
                                            "no job {} in this mesh's memory. If it ran long \
                                             enough ago, the retention window dropped it \
                                             (`meshnode retention show`).",
                                            job_id.short()
                                        ),
                                    })
                                    .await;
                            }
                        }
                    }
                }
            }
            other => {
                let resp = self.handle_authed(user, &env, other, source).await;
                let _ = out.send(resp).await;
            }
        }
    }

    // -- wire request dispatch --------------------------------------------

    fn spawn_dispatcher(self, mut events_rx: mpsc::Receiver<RequestEvent>) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        // Requests from peers that are not members get a bounded number of
        // handlers (SEC-10). They are all quick — a join, a pairing step,
        // or a refusal — so a full budget means a flood, and the flood is
        // told "busy" instead of costing a task each. Members are bounded
        // by membership, and their streams (a followed log) can be long.
        let strangers = Arc::new(tokio::sync::Semaphore::new(MAX_STRANGER_HANDLERS));
        // Event-driven, so quiet is fine; ending is not (ENG-2).
        let pulse = self.inner.vitals.register("wire dispatch", None);
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                let _pulse = pulse;
                loop {
                    tokio::select! {
                        event = events_rx.recv() => match event {
                            Some(event) => {
                                let member = this
                                    .inner
                                    .engine
                                    .read(|s| s.is_active_member(&event.peer.node_id));
                                let permit = if member {
                                    None
                                } else {
                                    match strangers.clone().try_acquire_owned() {
                                        Ok(permit) => Some(permit),
                                        Err(_) => {
                                            let _ = event.reply.try_send(Response::Error {
                                                message: "busy".into(),
                                            });
                                            continue;
                                        }
                                    }
                                };
                                let node = this.clone();
                                tokio::spawn(async move {
                                    node.handle_wire(event).await;
                                    drop(permit);
                                });
                            }
                            None => return,
                        },
                        _ = shutdown.changed() => return,
                    }
                }
            }
        });
        self.track(handle);
    }

    // -- the action cache (C7 §11) ----------------------------------------

    /// Enforcement on the executing node (MASTERPLAN §7.2): verify the
    /// user's signed request envelope carried inside the job spec, and check
    /// the user's grants against *this node's* materialized policy. A
    /// compromised origin node cannot run work here as a user who never
    /// asked for it or may not target this node.
    fn verify_job_auth(&self, spec: &JobSpec) -> Result<()> {
        self.verify_job_auth_for(spec, self.node_id(), true)
    }

    /// [`Self::verify_job_auth`], for work that ran on `runner` — this node
    /// when it is about to run it, or the node that did when a cache replica
    /// checks an entry after the fact (SEC-18). `fresh` is the submission
    /// window, which only means something at submission: an entry is
    /// published after the job has finished.
    fn verify_job_auth_for(&self, spec: &JobSpec, runner: NodeId, fresh: bool) -> Result<()> {
        // Whoever signed it (SEC-4): see `mesh_proto::is_loader_env`.
        if let Some(key) = loader_key(&spec.env) {
            return Err(CoreError::Auth(format!(
                "job environment sets {key}, which is never accepted"
            )));
        }
        let env = &spec.auth;
        // Signed by the submitter, or by a per-job credential a live member
        // has signed a statement for (`mesh_proto::acting`). Nothing else.
        if env.user != spec.submitter_user {
            self.acting_check(&spec.env, env.user, spec.submitter_user)
                .into_result("job authorization envelope")?;
        }
        let sig: [u8; 64] = env
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| CoreError::Auth("malformed job authorization signature".into()))?;
        // Decoded before anything is trusted, because *which* verification
        // applies depends on what the envelope claims to be. Each branch
        // below verifies a real signature; decoding first only lets a
        // caller choose which check it must pass.
        let req: IpcRequest = postcard::from_bytes(&env.payload)
            .map_err(|_| CoreError::Auth("malformed job authorization payload".into()))?;

        // -- C2 §6.2: a deployment instance ------------------------------
        //
        // An instance is a long-lived job re-submitted by whichever node is
        // the elected reconciler, so it cannot carry a live user envelope
        // any more than a schedule fire can. It names the deployment and
        // carries the *deployment record's* signature; authority is
        // re-derived here from this node's own state — revoking the creator
        // or removing the deployment stops the next instance dead, with no
        // signature to recall.
        if let IpcRequest::DeploymentFire {
            deployment: ref name,
            generation,
        } = req
        {
            let deployment = self
                .inner
                .engine
                .read(|s| {
                    s.deployments
                        .get(name)
                        .filter(|e| !e.removed)
                        .map(|e| e.deployment.clone())
                })
                .ok_or_else(|| {
                    CoreError::Auth(format!(
                        "job cites deployment {name:?}, which this node does not have"
                    ))
                })?;
            if deployment.created_by != env.user {
                return Err(CoreError::Auth(
                    "deployment authorization names a different user".into(),
                ));
            }
            if !verify_signature(&env.user.0, &deployment.signing_bytes(), &sig) {
                return Err(CoreError::Auth(
                    "invalid deployment authorization signature".into(),
                ));
            }
            let founder = self.inner.engine.genesis().body.founder_user;
            let live = self.inner.engine.read(|s| {
                crate::deployments::creator_may_still_run(
                    s,
                    &founder,
                    &deployment,
                    mesh_proto::now_micros(),
                )
            });
            if !live {
                return Err(CoreError::Auth(format!(
                    "deployment {name:?} was authorized by {}, who may no longer run services",
                    deployment.created_by.short()
                )));
            }
            // Anti-replay without a freshness window: the envelope names a
            // generation, and the payload must be the service version that
            // generation of this deployment declares. A stolen instance
            // spec cannot be re-submitted to run a different service.
            if generation != deployment.generation {
                return Err(CoreError::Auth(format!(
                    "deployment {name:?} is at generation {}, not {generation}",
                    deployment.generation
                )));
            }
            match &spec.payload {
                JobPayload::Service(exec)
                    if exec.service_name == deployment.service
                        && exec.version == deployment.service_version => {}
                _ => {
                    return Err(CoreError::Auth(format!(
                        "instance job does not run {}",
                        deployment.service_ref()
                    )));
                }
            }
            return self.verify_job_grants_on(spec, Some(&deployment.service), runner);
        }

        // -- N1 P5: a scheduled job --------------------------------------
        //
        // The same shape as the deployment fire above. A schedule outlives
        // the session that created it many times over, so its jobs cannot
        // carry a live user envelope; they name the schedule and carry the
        // *record's* signature, and authority is re-derived here from this
        // node's own state. Revoking the creator, taking away their role,
        // or removing the schedule stops the next fire dead, with no
        // signature to recall and no envelope to expire.
        if let IpcRequest::ScheduleFire {
            schedule: ref name,
            occurrence_micros,
        } = req
        {
            let schedule = self
                .inner
                .engine
                .read(|s| {
                    s.service_schedules
                        .get(name)
                        .filter(|e| !e.removed)
                        .map(|e| e.schedule.clone())
                })
                .ok_or_else(|| {
                    CoreError::Auth(format!(
                        "job cites schedule {name:?}, which this node does not have"
                    ))
                })?;
            if schedule.created_by != env.user {
                return Err(CoreError::Auth(
                    "schedule authorization names a different user".into(),
                ));
            }
            if !verify_signature(&env.user.0, &schedule.signing_bytes(), &sig) {
                return Err(CoreError::Auth(
                    "invalid schedule authorization signature".into(),
                ));
            }
            // **Anti-replay, and the reason a scheduled job needs no
            // freshness window.** A signed schedule authorizes the instants
            // its own expression names and no others, so a compromised
            // origin holding one cannot turn a nightly backup into a
            // continuous one: every extra fire would have to cite an
            // occurrence the trigger does not yield.
            if !schedule.names_occurrence(occurrence_micros) {
                return Err(CoreError::Auth(format!(
                    "schedule {name:?} does not name the occurrence this job cites"
                )));
            }
            if mesh_proto::JobId::derive_occurrence(name, occurrence_micros) != spec.job_id {
                return Err(CoreError::Auth(
                    "scheduled job id is not derived from the occurrence it cites".into(),
                ));
            }
            let founder = self.inner.engine.genesis().body.founder_user;
            let live = self.inner.engine.read(|s| {
                crate::schedules::creator_may_still_run(
                    s,
                    &founder,
                    &schedule,
                    mesh_proto::now_micros(),
                )
            });
            if !live {
                return Err(CoreError::Auth(format!(
                    "schedule {name:?} was authorized by {}, who may no longer run services",
                    env.user.to_display_string()
                )));
            }
            if let JobPayload::Service(exec) = &spec.payload
                && exec.service_name != schedule.service
            {
                return Err(CoreError::Auth(format!(
                    "scheduled job does not run {}",
                    schedule.service_ref()
                )));
            }
            return self.verify_job_grants_on(spec, Some(&schedule.service), runner);
        }

        // Unbound, or bound to the origin that took the request (or to this
        // node, when it is that origin) — SEC-12.
        if crate::identity::verify_ipc(env, self.mesh_id(), &[spec.submitter, runner]).is_none() {
            return Err(CoreError::Auth(
                "invalid job authorization signature".into(),
            ));
        }
        let window = u64::try_from(JOB_AUTH_WINDOW.as_micros()).unwrap_or(u64::MAX);
        if fresh
            && env.at_micros.abs_diff(mesh_proto::now_micros()) > window
            && !self.plan_vouches(spec, runner)
        {
            return Err(CoreError::Auth("job authorization has expired".into()));
        }
        // M11's `--dry-run` and `--break-glass` wrap the request the user
        // would otherwise have sent, and it is the *inner* request that
        // describes the work. Unwrapping here rather than at the origin is
        // what keeps the executor's check meaningful: the executor verifies
        // the envelope it was handed, and an override changes who may
        // submit, never what the signature covers.
        let req = match req {
            IpcRequest::BreakGlass { request, .. }
            | IpcRequest::DryRun { request }
            | IpcRequest::ExplainKey { request } => *request,
            other => other,
        };
        // What the user signed about the *environment* and the *deadline*,
        // beside the code (SEC-4). The payload match below says which code
        // runs; these say how. Fan-out plans carry the envelope to every
        // member for an hour, so without them any member could replay a
        // user's command with `PATH` or `LD_PRELOAD` of its own choosing.
        const NO_ENV: &[(String, String)] = &[];
        let (signed_env, signed_timeout) = match &req {
            IpcRequest::Run {
                env, timeout_secs, ..
            }
            | IpcRequest::RunScript {
                env, timeout_secs, ..
            } => (env.as_slice(), *timeout_secs),
            // `RunService` has no environment: everything in the job's comes
            // from the service definition or is meshware's own.
            IpcRequest::RunService { timeout_secs, .. } => (NO_ENV, *timeout_secs),
            _ => (NO_ENV, None),
        };
        if !mesh_proto::env_matches_signed(&spec.env, signed_env) {
            return Err(CoreError::Auth(
                "job environment does not match the one the user signed".into(),
            ));
        }
        if spec.timeout_secs != signed_timeout {
            return Err(CoreError::Auth(
                "job timeout does not match the one the user signed".into(),
            ));
        }
        let service = match (&req, &spec.payload) {
            (IpcRequest::Run { command, .. }, JobPayload::Command(cmd)) if command == cmd => None,
            (IpcRequest::RunService { name, .. }, JobPayload::Service(exec))
                if *name == exec.service_name =>
            {
                Some(exec.service_name.clone())
            }
            // A script carried in the request (1.15, `mesh_proto::script`).
            // The comparison is the *whole* script — source, runtime,
            // requirements and inputs — not a name, because there is no
            // name: the body is the thing the user signed, so the body is
            // what has to match. That makes this the strictest arm here,
            // and it is the right strictness for the only payload class a
            // compromised origin could otherwise fill with anything.
            (IpcRequest::RunScript { script, .. }, JobPayload::Script(exec))
                if script.as_ref() == exec.as_ref() =>
            {
                None
            }
            _ => {
                return Err(CoreError::Auth(
                    "job authorization does not match the job payload".into(),
                ));
            }
        };
        self.verify_job_grants_on(spec, service.as_deref(), runner)
    }

    /// May this user run this work on **this** node? The last half of
    /// [`Self::verify_job_auth`], factored out because a scheduled job
    /// reaches it by a different route and must not reach a weaker version
    /// of it.
    /// Does a replicated fan-out or gang plan vouch for this sub-job, on
    /// `runner`, now (ENG-9)?
    ///
    /// A fan-out throttled by `--max-parallel`, or finished by a successor,
    /// dispatches its targets long after the user signed the request, and
    /// every target carries that one envelope. The hour a submission is
    /// fresh for then failed every target dispatched after minute sixty — on
    /// exactly the long runs across many machines the feature exists for.
    ///
    /// The plan is what the user's request became: replicated, authored by
    /// the origin, carrying the envelope. So an expired envelope is still
    /// good for a job the plan names, on the node the plan names, while the
    /// group is open and inside the runs window — and nothing else. The
    /// signature has already been checked; what the envelope says about the
    /// code, the environment and the deadline is still checked below; and
    /// whether the user may still run here is asked of today's grants, so
    /// revoking them stops the next target, as it does for a deployment.
    fn plan_vouches(&self, spec: &JobSpec, runner: NodeId) -> bool {
        let since = self.drivable_since();
        self.inner.engine.read(|s| {
            if let Some(info) = &spec.fanout {
                return s.fanouts.get(&info.group).is_some_and(|entry| {
                    let plan = &entry.plan;
                    entry.closed.is_none()
                        && plan.created_at_micros >= since
                        && plan.auth == spec.auth
                        && plan.submitter == spec.submitter
                        && plan.submitter_user == spec.submitter_user
                        && usize::try_from(info.index)
                            .ok()
                            .and_then(|i| plan.targets.get(i))
                            .is_some_and(|t| t.job_id == spec.job_id && t.node_id == runner)
                });
            }
            if let Some(gang) = crate::jobs::gang_of(&spec.env) {
                return s.gangs.get(&gang).is_some_and(|entry| {
                    let plan = &entry.plan;
                    entry.closed.is_none()
                        && plan.created_at_micros >= since
                        && plan.auth == spec.auth
                        && plan.submitter == spec.submitter
                        && plan.submitter_user == spec.submitter_user
                        && plan
                            .members
                            .iter()
                            .any(|m| m.job_id == spec.job_id && m.node_id == runner)
                });
            }
            false
        })
    }

    fn verify_job_grants_on(
        &self,
        spec: &JobSpec,
        service: Option<&str>,
        runner: NodeId,
    ) -> Result<()> {
        let me = runner;
        let founder = self.inner.engine.genesis().body.founder_user;
        let allowed = self.inner.engine.read(|s| {
            s.members.get(&me).is_some_and(|m| {
                rbac::can_run_on(
                    s,
                    &founder,
                    &spec.submitter_user,
                    &m.info,
                    service,
                    mesh_proto::now_micros(),
                )
            })
        });
        if !allowed {
            return Err(CoreError::Denied(format!(
                "user {} has no grant covering this node",
                spec.submitter_user.short()
            )));
        }
        Ok(())
    }

    /// Accept a job for local execution: verify the user's authorization,
    /// fetch its bundle if needed, then spawn. Bundle fetch is the M2
    /// replication path (MASTERPLAN §8.1). The submitter is always a
    /// candidate holder (robust against gossip lag).
    /// Accept a job here, one submission of each job id at a time (ENG-6).
    ///
    /// An origin that got no answer asks again, and reads `Busy` as proof
    /// that this node does not hold the job. That is only true once the
    /// first submission has finished: a draining node answers `Busy` to a
    /// job it has no record of, and the first submission may still be
    /// fetching its bundle, not yet journaled — so the origin would send the
    /// job elsewhere while this node went on to run it. Waiting for the
    /// first submission makes the second one's answer the truth.
    async fn accept_job(&self, spec: JobSpec) -> Result<()> {
        let job_id = spec.job_id;
        let gate = self
            .inner
            .accepting
            .lock()
            .entry(job_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let result = {
            let _one_at_a_time = gate.lock().await;
            self.accept_job_once(spec).await
        };
        let mut accepting = self.inner.accepting.lock();
        // The map's reference and ours: nobody else is waiting.
        if Arc::strong_count(&gate) <= 2 {
            accepting.remove(&job_id);
        }
        result
    }

    async fn accept_job_once(&self, spec: JobSpec) -> Result<()> {
        // A node that cannot write its op-log, or is below its disk floor,
        // takes nothing new either (ENG-4): `Busy`, so the job goes to a
        // node that can record it. Work already journaled still acks.
        if (self.inner.engine.store_failure().is_some()
            || self.inner.engine.restore_hold().is_some()
            || self.inner.disk_critical.load(Ordering::Relaxed))
            && !self.inner.jobs.has_record(&spec.job_id)
        {
            return Err(CoreError::Busy {
                running: u32::try_from(self.inner.jobs.running_count()).unwrap_or(u32::MAX),
                queued: u32::try_from(self.inner.jobs.queued_count()).unwrap_or(u32::MAX),
            });
        }
        // A draining node accepts nothing new (MASTERPLAN §12). Busy is the
        // definitive spill-over refusal, so origins that have not yet merged
        // our `draining` tag route the job to the next candidate instead of
        // failing it. Re-dispatch of already-journaled work still acks
        // below (idempotency, §10.3).
        //
        // …and so does a node an admin is draining from afar (Phase C, E9),
        // whose own flag is not set: the decision is the mesh's, in its tags.
        let me = self.node_id();
        let drained = self.inner.engine.read(|s| {
            s.members
                .get(&me)
                .is_some_and(|m| mesh_proto::drain_of(&m.info.tags).is_some())
        });
        if (self.inner.draining.load(Ordering::SeqCst) || drained)
            && !self.inner.jobs.has_record(&spec.job_id)
        {
            return Err(CoreError::Busy {
                running: u32::try_from(self.inner.jobs.running_count()).unwrap_or(u32::MAX),
                queued: u32::try_from(self.inner.jobs.queued_count()).unwrap_or(u32::MAX),
            });
        }
        // Defense in depth for the no-execute capability (D6): the scheduler
        // never picks such nodes, and they refuse work outright too.
        let me = self.node_id();
        let no_execute = self.inner.engine.read(|s| {
            s.members
                .get(&me)
                .is_some_and(|m| m.info.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG))
        });
        if no_execute {
            return Err(CoreError::Job(
                "this node carries the no-execute capability and does not run jobs".into(),
            ));
        }
        // Quarantine, here as well as in placement (SEC-16). Placement never
        // picks a quarantined node, but a job sent to it directly — by an
        // origin that has not merged the quarantine yet, or by one that
        // ignores it — arrived at this door and ran.
        if let Some(why) = self.quarantined_here() {
            return Err(CoreError::Job(why));
        }
        // …and the owner's own veto, checked here as well as in placement.
        // The scheduler will not pick a vetoed node, but a re-dispatch of
        // work planned before the decision, or an origin running an older
        // build, both arrive at this door — and the point of the decision
        // is that the fleet's owner is obeyed, not consulted.
        let vetoed = self.inner.engine.read(|s| {
            s.execution_refusal(&me).map(|d| d.reason.clone()).map(|r| {
                if r.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({r})")
                }
            })
        });
        if let Some(why) = vetoed {
            return Err(CoreError::Job(format!(
                "an administrator of this mesh has not allowed this node to run work{why}"
            )));
        }
        // An acting-for claim is checked against *this node's* copy of the
        // job and plan it cites (SEC-1), which the node vouching for it may
        // have published moments ago. Rather than let a converging mesh look
        // like an authorization failure, pull from the submitter and the
        // voucher once and ask again — the same "the submitter is always a
        // candidate holder" robustness the bundle path has.
        if let Err(e) = self.verify_job_auth(&spec) {
            if !matches!(&e, CoreError::Auth(m) if m.contains(NOT_KNOWN_HERE)) {
                return Err(e);
            }
            for peer in Self::acting_pull_targets(&spec.env, spec.submitter) {
                self.pull_state_from(peer).await;
            }
            self.verify_job_auth(&spec)?;
        }
        if let JobPayload::Service(exec) = &spec.payload {
            self.ensure_bundle(&exec.bundle_id, &[spec.submitter])
                .await?;
        }
        // Idempotent + capacity-aware (MASTERPLAN §10.2/§10.3): an already-
        // journaled job id is acknowledged, a saturated node answers Busy.
        match self.inner.jobs.accept(spec.clone()) {
            Err(CoreError::Busy { running, queued }) if mesh_proto::may_preempt(&spec.env) => {
                self.accept_by_preempting(spec, CoreError::Busy { running, queued })
                    .await
            }
            other => other,
        }
    }

    /// Why this node refuses work because it is quarantined, if it is
    /// (SEC-16).
    fn quarantined_here(&self) -> Option<String> {
        let me = self.node_id();
        self.inner.engine.read(|s| {
            s.quarantine(&me).map(|q| {
                format!(
                    "{} is quarantined and runs no work — {}",
                    self.inner.config.node_name, q.reason
                )
            })
        })
    }

    /// Make room for a deployment instance by preempting one `--preemptible`
    /// job, then ask admission again (Phase C, E10).
    ///
    /// **Everything that decides this is this node's own.** The start must be
    /// a deployment instance — its envelope already checked by
    /// `verify_job_auth` against the deployment record — and the priority is
    /// read from this node's copy of that deployment's policy, not from the
    /// spec. The victim must have opted in with `--preemptible`, must not be
    /// another deployment's instance, and must sit at least
    /// [`mesh_proto::PREEMPT_MARGIN`] below; it is requeued, as it is for share
    /// reclamation. One job per start, and `Busy` when nothing qualifies.
    ///
    /// Best effort by construction: the slot a preemption frees can be taken
    /// by work already queued here before this asks again, and the start is
    /// then refused like any other and tried again later.
    async fn accept_by_preempting(&self, spec: JobSpec, busy: CoreError) -> Result<()> {
        let Ok(IpcRequest::DeploymentFire { deployment, .. }) =
            postcard::from_bytes::<IpcRequest>(&spec.auth.payload)
        else {
            return Err(busy);
        };
        let priority = self.inner.engine.read(|s| {
            mesh_proto::priority_of_env(
                &crate::deployments::policy_for(s, &deployment).instance_env,
            )
        });
        if priority <= mesh_proto::DEFAULT_PRIORITY {
            return Err(busy);
        }
        let jobs = self.inner.jobs.clone();
        let victim = jobs
            .preemptible_running()
            .into_iter()
            .filter_map(|(id, owner)| {
                let victim = jobs.spec_of(&id)?;
                if matches!(
                    postcard::from_bytes::<IpcRequest>(&victim.auth.payload),
                    Ok(IpcRequest::DeploymentFire { .. })
                ) {
                    return None;
                }
                let score = victim.priority.score();
                (score.saturating_add(mesh_proto::PREEMPT_MARGIN) <= priority)
                    .then_some((score, id, owner))
            })
            .min_by_key(|(score, id, _)| (*score, *id));
        let Some((score, victim_job, owner)) = victim else {
            return Err(busy);
        };
        // Requeue: the owner volunteered the job for a move, not for a
        // deletion, and finishing elsewhere is kinder than losing the work.
        let mode = mesh_proto::PreemptMode::Requeue;
        let detail = format!(
            "priority {score}; made room for deployment {deployment} at priority {priority}"
        );
        audit::record(
            &self.inner.engine,
            owner,
            "preemption",
            &victim_job.to_display_string(),
            mode.as_str(),
            &detail,
            &[],
        );
        info!(job = %victim_job.short(), mode = mode.as_str(), detail, "preempting");
        self.preempt_job(victim_job, mode);
        let deadline = Instant::now() + Duration::from_secs(PREEMPT_WAIT_SECS);
        while jobs.is_running(&victim_job) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        jobs.accept(spec)
    }

    /// One anti-entropy round with a peer, on demand. Gossip converges on
    /// its own schedule; this is for the moments where waiting for it would
    /// turn a timing question into an error.
    async fn pull_state_from(&self, peer: NodeId) {
        let Some(member) = self
            .inner
            .engine
            .read(|s| s.members.get(&peer).map(|m| m.info.clone()))
        else {
            return;
        };
        if let Err(e) = self.inner.gossip.sync_with(&member).await {
            debug!(peer = %peer.short(), "on-demand sync failed: {e}");
        }
    }

    /// Ensure a bundle is present locally, fetching from holders and recording
    /// our hold on success.
    ///
    /// Refuses outright when this node is under its disk floor and
    /// reclaiming did not clear it (M31 A2). A refusal here surfaces as the
    /// job or deployment failing to start with a message naming the disk,
    /// which is a bad afternoon — and strictly better than the alternative,
    /// where the fetch succeeds, the volume fills, and every other service
    /// on the machine fails with something that names anything but the
    /// cause. A bundle already present is never refused: the guard is about
    /// *adding* bytes, and a node that could not read what it already holds
    /// would be broken for no benefit.
    async fn ensure_bundle(&self, bundle_id: &BundleId, hints: &[NodeId]) -> Result<()> {
        // An image-only service has no bundle to fetch. `nginx:alpine`
        // carries its own filesystem; the mesh has nothing to ship.
        if bundle_id.is_none() {
            return Ok(());
        }
        if self.inner.content.has_bundle(bundle_id) {
            self.inner.content.touch(bundle_id);
            return Ok(());
        }
        if self.disk_critical() {
            return Err(CoreError::Store(format!(
                "not enough disk to fetch bundle {} — this node is below its \
                 [disk] min_free floor and has already reclaimed what it could. \
                 Free space on the data dir, or lower `min_free_mb`",
                bundle_id.short()
            )));
        }
        content_sync::ensure_bundle(
            &self.inner.engine,
            &self.inner.transport,
            &self.inner.content,
            self.node_id(),
            bundle_id,
            hints,
            &|id| self.inner.gossip.liveness_of(&id) == Liveness::Alive,
        )
        .await?;
        if let Ok(Some(manifest)) = self.inner.content.load_manifest(bundle_id) {
            let _ = services::record_hold_from_manifest(
                &self.inner.engine,
                &self.inner.content,
                &manifest,
            );
        }
        // Fetching it is using it: the window starts now, not at whatever
        // mtime the manifest landed with.
        self.inner.content.touch(bundle_id);
        Ok(())
    }

    /// The manifest of a bundle known to the mesh, fetched from a holder
    /// (manifest only) when this node does not hold it.
    async fn bundle_manifest(&self, bundle_id: &BundleId) -> Result<mesh_proto::Manifest> {
        content_sync::ensure_manifest(
            &self.inner.engine,
            &self.inner.transport,
            &self.inner.content,
            self.node_id(),
            bundle_id,
        )
        .await
    }

    /// Read one regular file out of a bundle, fetching the bundle first if
    /// needed. Bounded by `BUNDLE_FILE_READ_MAX`.
    async fn read_bundle_file(&self, bundle_id: &BundleId, path: &str) -> Result<Vec<u8>> {
        let manifest = self.bundle_manifest(bundle_id).await?;
        let entry = manifest
            .files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| CoreError::Store(format!("no such file in bundle: {path}")))?;
        if entry.symlink.is_some() {
            return Err(CoreError::Store(format!("{path} is a symlink")));
        }
        if entry.size > mesh_proto::BUNDLE_FILE_READ_MAX {
            return Err(CoreError::Store(format!(
                "{path} is {} bytes; bundle file reads are capped at {} bytes",
                entry.size,
                mesh_proto::BUNDLE_FILE_READ_MAX
            )));
        }
        self.ensure_bundle(bundle_id, &[]).await?;
        let mut data = Vec::with_capacity(entry.size as usize);
        for hash in &entry.chunks {
            match self.inner.content.read_chunk(hash)? {
                Some(chunk) => data.extend_from_slice(&chunk),
                None => {
                    return Err(CoreError::Store(format!(
                        "chunk missing for {path} after fetch"
                    )));
                }
            }
        }
        Ok(data)
    }

    /// Stream one regular file out of a bundle, chunk by chunk, fetching
    /// the bundle first if needed. Unbounded by design — this is the door
    /// a release binary comes through, and [`read_bundle_file`] cannot be
    /// it, because a single frame cannot hold twenty-five megabytes.
    ///
    /// Each frame is one content-store chunk, so memory here stays flat
    /// however large the file is, and the terminal `BundleFileDone` frame
    /// is what lets the caller tell a finished stream from a cut one.
    ///
    /// [`read_bundle_file`]: Self::read_bundle_file
    async fn stream_bundle_file(
        &self,
        bundle_id: BundleId,
        path: String,
        out: mpsc::Sender<IpcResponse>,
    ) {
        macro_rules! fail {
            ($($arg:tt)*) => {{
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!($($arg)*),
                    })
                    .await;
                return;
            }};
        }
        let manifest = match self.bundle_manifest(&bundle_id).await {
            Ok(m) => m,
            Err(e) => fail!("{e}"),
        };
        let Some(entry) = manifest.files.iter().find(|f| f.path == path).cloned() else {
            fail!("no such file in bundle: {path}")
        };
        if entry.symlink.is_some() {
            fail!("{path} is a symlink")
        }
        if let Err(e) = self.ensure_bundle(&bundle_id, &[]).await {
            fail!("{e}")
        }
        for hash in &entry.chunks {
            let chunk = match self.inner.content.read_chunk(hash) {
                Ok(Some(c)) => c,
                Ok(None) => fail!("chunk missing for {path} after fetch"),
                Err(e) => fail!("reading {path}: {e}"),
            };
            // A caller that hung up mid-file is not an error to report to
            // the caller that hung up.
            if out
                .send(IpcResponse::BundleFileChunk { data: chunk })
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = out
            .send(IpcResponse::BundleFileDone { size: entry.size })
            .await;
    }

    // -- IPC (local CLI / Director) ---------------------------------------

    /// Per-node saturation rows for `meshnode top` (MASTERPLAN §10.2):
    /// running/queued from replicated job summaries, capacity from the
    /// member record.
    fn top_rows(&self) -> Vec<TopRow> {
        let me = self.node_id();
        // Our own pressure is read directly; peers' comes from whatever the
        // gossip tick last collected (M9 §5.2).
        let own_system = self.system();
        let own = self.pressure();
        // Our own leases are known exactly; every other node's are inferred
        // from the replicated gang plans (C4 P1). Leases are node-local by
        // design, so this is an upper bound within one lease lifetime and
        // exact outside it — see `TopRow::leased_cpus` for why that is the
        // right shape for this column rather than a reason to replicate
        // them.
        let own_leases = self.inner.jobs.leases();
        let inferred = self.inferred_leases();
        let own_device_holds = self.inner.jobs.device_holders();
        let own_device_pressure = self.inner.jobs.device_pressure();
        let mut rows = self.inner.engine.read(|s| {
            // **One pass over the job table for the whole answer** (M-4).
            //
            // This used to be two `s.jobs.values()` scans *per member*, so
            // `top` cost members × jobs — 500 × 50 000 = 25 million
            // iterations per call, under the state read lock, twice a
            // second while a Director window was open. The table is the
            // same table for every row; folding it once and looking the
            // answer up is the same arithmetic without the multiplier.
            let mut load: HashMap<NodeId, (u32, u32)> = HashMap::new();
            for j in s.jobs.values() {
                match j.state {
                    JobState::Running => {
                        load.entry(j.executor).or_default().0 += 1;
                    }
                    JobState::Queued => {
                        load.entry(j.executor).or_default().1 += 1;
                    }
                    _ => {}
                }
            }
            s.members
                .values()
                .filter(|m| !m.left && !s.revoked.contains_key(&m.info.node_id))
                // Consoles are somebody's window, not a machine. They are
                // excluded *here* rather than at each surface because this
                .map(|m| {
                    let id = m.info.node_id;
                    let (running, queued) = load.get(&id).copied().unwrap_or((0, 0));
                    TopRow {
                        node_id: id,
                        name: m.info.name.clone(),
                        liveness: self.inner.gossip.liveness_of(&id),
                        is_self: id == me,
                        no_execute: m.info.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG),
                        slots: m.info.slots,
                        running,
                        queued,
                        cores: mesh_proto::Capacity::from_tags(&m.info.tags).map_or(0, |c| c.cores),
                        pressure: if id == me {
                            Some(own.clone())
                        } else {
                            self.inner.gossip.pressure_of(&id)
                        },
                        leased_cpus: if id == me {
                            own_leases.iter().map(|l| l.cpus).sum()
                        } else {
                            inferred.get(&id).map_or(0, |(cpus, _)| *cpus)
                        },
                        leases: if id == me {
                            u32::try_from(own_leases.len()).unwrap_or(u32::MAX)
                        } else {
                            inferred.get(&id).map_or(0, |(_, n)| *n)
                        },
                        // Devices come from replicated state for every
                        // node, holds only for this one (C5 P5). The
                        // asymmetry is the same one `leased_cpus` states:
                        // a hold is node-local and there is no honest way
                        // to read a remote node's ledger, so a remote row
                        // says `None` rather than guessing.
                        devices: s
                            .devices
                            .get(&id)
                            .map(|inv| u32::try_from(inv.healthy().count()).unwrap_or(u32::MAX))
                            .unwrap_or(0),
                        devices_busy: (id == me)
                            .then(|| u32::try_from(own_device_holds.len()).unwrap_or(u32::MAX)),
                        // The idlest *schedulable* device. A card the
                        // driver has taken out of service reads 0% busy
                        // and is not a place work can go, so counting it
                        // would report a saturated box as idle.
                        device_busy: s.devices.get(&id).and_then(|inv| {
                            let telemetry = if id == me {
                                own_device_pressure.clone()
                            } else {
                                self.inner.gossip.device_pressure_of(&id)
                            };
                            inv.healthy()
                                .filter_map(|d| {
                                    mesh_proto::reading_for(&telemetry, d, &inv.devices)
                                })
                                .filter_map(mesh_proto::DevicePressure::busy_fraction)
                                .min_by(f64::total_cmp)
                        }),
                        system: if id == me {
                            Some(own_system.clone())
                        } else {
                            self.inner.gossip.system_of(&id)
                        },
                    }
                })
                .collect::<Vec<_>>()
        });
        rows.sort_by_key(|r| (!r.is_self, r.name.clone(), r.node_id));
        rows
    }

    /// Sum the mesh into one answer (`meshnode totals`, M9 §6.3).
    ///
    /// A fold over [`Self::top_rows`] for everything live, plus the
    /// capacity tags for the hardware that does not change. It is
    /// deliberately not a new measurement: two surfaces summing the same
    /// two tables slightly differently is how a dashboard and a CLI end up
    /// disagreeing about how big the mesh is, and the fix for that is one
    /// summation rather than a convention.
    ///
    /// **Dead nodes contribute nothing and unknown values are not zero.**
    /// A node gossip believes is dead still has its cores, but they are not
    /// available to anyone, so counting them would make the headline number
    /// a statement about the hardware somebody once owned. A node that has
    /// never advertised its memory is excluded from `mem_mb` *and* from
    /// `mem_from`, so the pair reads "6.4 TB across 36 of 40" instead of
    /// silently averaging in a zero.
    fn totals(&self) -> mesh_proto::Totals {
        let rows = self.top_rows();
        // Capacity lives in the members' tags rather than on a `TopRow`,
        // which carries only the cores the scheduler scores against.
        let caps: std::collections::HashMap<NodeId, mesh_proto::Capacity> =
            self.inner.engine.read(|s| {
                s.members
                    .values()
                    .filter(|m| !m.left && !s.revoked.contains_key(&m.info.node_id))
                    .filter_map(|m| {
                        mesh_proto::Capacity::from_tags(&m.info.tags).map(|c| (m.info.node_id, c))
                    })
                    .collect()
            });
        // GPUs and their memory, from the replicated inventories (L2 P1).
        // `TopRow::devices` counts every schedulable accelerator; the strip
        // says "3 GPUs (72 GB VRAM)" and needs the kind and the memory,
        // neither of which survives that count. Schedulable only, on the
        // same terms: a card the driver has taken out of service is not
        // capacity anybody can spend.
        let gpu: std::collections::HashMap<NodeId, (u32, u64)> = self.inner.engine.read(|s| {
            s.devices
                .iter()
                .map(|(id, inv)| {
                    let cards = inv
                        .healthy()
                        .filter(|d| d.kind == mesh_proto::DeviceKind::Gpu);
                    let mut count = 0_u32;
                    let mut mem = 0_u64;
                    for d in cards {
                        count = count.saturating_add(1);
                        mem = mem.saturating_add(d.mem_mb);
                    }
                    (*id, (count, mem))
                })
                .collect()
        });

        let mut t = mesh_proto::Totals {
            nodes_known: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            nodes_live: 0,
            nodes_draining: 0,
            cores: 0,
            cores_from: 0,
            slots: 0,
            slots_busy: 0,
            mem_mb: 0,
            mem_from: 0,
            mem_available_mb: 0,
            mem_available_from: 0,
            disk_mb: 0,
            disk_from: 0,
            disk_free_mb: 0,
            disk_free_from: 0,
            devices: 0,
            devices_busy_here: 0,
            jobs_running: 0,
            jobs_queued: 0,
            arches: Vec::new(),
            load_per_core: None,
            gpus: 0,
            vram_mb: 0,
            capacity_from: 0,
        };
        let mut arches: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        let mut load_sum = 0.0_f64;
        let mut load_from = 0_u32;

        for row in &rows {
            // Dead is dead: its capacity is real and unreachable, and a
            // total that includes it answers a question nobody asked.
            if matches!(self.inner.gossip.liveness_of(&row.node_id), Liveness::Dead) {
                continue;
            }
            t.nodes_live += 1;
            if row.no_execute {
                t.nodes_draining += 1;
            }
            t.slots = t.slots.saturating_add(row.slots);
            t.slots_busy = t.slots_busy.saturating_add(row.running);
            t.devices = t.devices.saturating_add(row.devices);
            t.devices_busy_here = t
                .devices_busy_here
                .saturating_add(row.devices_busy.unwrap_or(0));
            if row.cores > 0 {
                t.cores = t.cores.saturating_add(u64::from(row.cores));
                t.cores_from += 1;
            }
            if let Some((cards, mem)) = gpu.get(&row.node_id) {
                t.gpus = t.gpus.saturating_add(*cards);
                t.vram_mb = t.vram_mb.saturating_add(*mem);
            }
            if let Some(cap) = caps.get(&row.node_id) {
                // The coverage denominator (D-L2.2): any `cap.*` tag at all
                // counts as reporting. A node that advertised memory and
                // failed its core probe has spoken, and a coverage line
                // built from one field alone would call it silent.
                t.capacity_from += 1;
                if cap.mem_mb > 0 {
                    t.mem_mb = t.mem_mb.saturating_add(cap.mem_mb);
                    t.mem_from += 1;
                }
                if cap.disk_mb > 0 {
                    t.disk_mb = t.disk_mb.saturating_add(cap.disk_mb);
                    t.disk_from += 1;
                }
                if !cap.arch.is_empty() {
                    *arches.entry(cap.arch.clone()).or_default() += 1;
                }
            }
            // Pressure is the only live half, and it is routinely absent —
            // a peer below the compute minor, or a reading that aged out.
            // Absent contributes to neither the sum nor its denominator.
            if let Some(p) = &row.pressure {
                t.jobs_running = t.jobs_running.saturating_add(p.running);
                t.jobs_queued = t.jobs_queued.saturating_add(p.queued);
                if let Some(mb) = p.mem_available_mb {
                    t.mem_available_mb = t.mem_available_mb.saturating_add(mb);
                    t.mem_available_from += 1;
                }
                if let Some(mb) = p.disk_free_mb {
                    t.disk_free_mb = t.disk_free_mb.saturating_add(mb);
                    t.disk_free_from += 1;
                }
                if let Some(per_core) = p.load_per_core(row.cores) {
                    load_sum += per_core;
                    load_from += 1;
                }
            }
        }

        t.arches = arches.into_iter().collect();
        // Most common first: the question this answers is "can a bundle
        // built for one architecture run everywhere", and the majority
        // arch is the one that decides it.
        t.arches.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if load_from > 0 {
            t.load_per_core = Some(load_sum / f64::from(load_from));
        }
        t
    }

    fn source_rows(&self) -> Vec<SourceRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        self.inner.engine.read(|s| {
            let mut rows: Vec<SourceRow> = s
                .sources
                .values()
                .filter(|e| !e.removed)
                .map(|e| {
                    let watcher = gitsync::elected_watcher(s, &e.source.name, |id| {
                        self.inner.gossip.liveness_of(id)
                    })
                    .and_then(|id| s.members.get(&id).map(|m| m.info.name.clone()));
                    SourceRow {
                        name: e.source.name.clone(),
                        url: e.source.url.clone(),
                        branch: e.source.branch.clone(),
                        interval_secs: e.source.interval_secs,
                        bump: e.source.bump.clone(),
                        created_by_name: rbac::username_of(s, &founder, &e.source.created_by),
                        last_commit: e.last.as_ref().map(|l| l.commit.clone()),
                        last_bundle: e.last.as_ref().map(|l| l.bundle_id),
                        last_synced_at_micros: e.last.as_ref().map(|l| l.at.wall_micros),
                        watcher,
                    }
                })
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            rows
        })
    }

    /// Approver side of LAN pairing over a streaming IPC request: dial the
    /// candidate, surface the code (`PairCode`), wait for `PairDecide`, and
    /// finish the join (MASTERPLAN §5.2). The invite token was built and
    /// signed client-side by the approving admin, like any invite.
    async fn pair_approve_stream(
        &self,
        user: UserId,
        env: SignedIpc,
        addr: String,
        token: String,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let fail = |message: String| IpcResponse::Error { message };
        let token = match InviteToken::decode(&token) {
            Ok(t) => t,
            Err(_) => {
                let _ = out.send(fail("invite token is malformed".into())).await;
                return;
            }
        };
        if token.created_by_user != user {
            let _ = out
                .send(fail(
                    "the pairing invite was signed by a different user".into(),
                ))
                .await;
            return;
        }
        let sock = match crate::transport::resolve(&addr).await {
            Ok(s) => s,
            Err(e) => {
                let _ = out.send(fail(e.to_string())).await;
                return;
            }
        };

        // Session bookkeeping: the code event carries a session id the
        // frontend answers with PairDecide.
        let session = self.inner.pair_next.fetch_add(1, Ordering::Relaxed);
        let (decision_tx, decision_rx) = oneshot::channel();
        self.inner.pair_sessions.lock().insert(session, decision_tx);

        let (events_tx, mut events_rx) = mpsc::channel(4);
        let forward_out = out.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(ev) = events_rx.recv().await {
                match ev {
                    pairing::PairEvent::Code {
                        code,
                        joiner_id,
                        joiner_name,
                    } => {
                        let _ = forward_out
                            .send(IpcResponse::PairCode {
                                session,
                                code,
                                joiner_name,
                                joiner_id,
                            })
                            .await;
                    }
                }
            }
        });

        let result = pairing::pair_approve(
            &self.inner.transport,
            &self.inner.engine,
            sock,
            token,
            events_tx,
            decision_rx,
        )
        .await;
        self.inner.pair_sessions.lock().remove(&session);
        let _ = forwarder.await;

        match result {
            Ok(outcome) => {
                self.audit_ok(
                    &user,
                    "node.pair",
                    &outcome.node_id.to_display_string(),
                    &outcome.node_name,
                    &env,
                );
                let _ = out
                    .send(IpcResponse::PairDone {
                        node_id: outcome.node_id,
                        node_name: outcome.node_name,
                    })
                    .await;
            }
            Err(e) => {
                let _ = out.send(fail(e.to_string())).await;
            }
        }
    }

    // -- drain (MASTERPLAN §12) ---------------------------------------------

    /// Graceful drain: announce `draining` mesh-wide, stop accepting work,
    /// optionally re-dispatch still-queued jobs (`--move`), wait for
    /// everything accepted here (and every fan-out this node is driving) to
    /// finish, then ask the daemon loop to exit cleanly. Running jobs are
    /// never killed — that is the whole point.
    /// Drain and exit **on this node's own authority**, because the binary
    /// on disk is no longer the one running (M24 §4).
    ///
    /// The upgrade process cannot do this for us. It runs as the daemon's
    /// user and signs IPC with whatever CLI identity that user has, which
    /// on every node except the founder's is a key the mesh has never
    /// heard of — so `meshnode upgrade --self` installs the binary and is
    /// then refused its own drain. The authorization for this restart was
    /// established when the plan was verified; asking a second time, as
    /// somebody else, was the mistake.
    async fn drain_for_upgrade(&self, target: &str) {
        if self.inner.draining.swap(true, Ordering::SeqCst) {
            return; // already going down
        }
        // **Not a deadline of now** (ENG-26). A deadline already passed
        // tells every deployment to move its instances off this node
        // immediately — so a rolling upgrade stopped and restarted every
        // service on every node, twice: once away, once back, for a binary
        // swap that takes seconds. Nobody asked for that, and for a
        // single-replica service it is a real outage per node upgraded.
        //
        // A short deadline instead. Long enough that an ordinary upgrade
        // restart is over before anything migrates — the node comes back
        // and its instances were never disturbed — and short enough that a
        // node which does *not* come back has its work moved rather than
        // left waiting on it. What does not fit in the window still moves,
        // which is what the deadline has always meant.
        self.announce_draining(Some(
            mesh_proto::now_micros() + UPGRADE_INSTANCE_GRACE.as_micros() as u64,
        ));
        info!(
            target,
            "upgrade installed — draining to restart on the new binary"
        );
        let closed = self.inner.sessions.close_hosted(&format!(
            "{} is restarting into meshnode {target}",
            self.inner.config.node_name
        ));
        if closed > 0 {
            info!(sessions = closed, "upgrade drain closed live sessions");
        }
        // Waits for accepted work, exactly as an operator's drain does: an
        // upgrade is not a reason to kill a job somebody is waiting on.
        loop {
            let running = self.inner.jobs.running_count();
            let queued = self.inner.jobs.queued_count();
            let drives = self.inner.active_drives.lock().len();
            if running == 0 && queued == 0 && drives == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // Best-effort final push so peers hold our last summaries — and,
        // more to the point here, our *old* version tag, so the mesh sees
        // a clean transition rather than a gap.
        let me = self.node_id();
        let peers: Vec<MemberInfo> = self.inner.engine.read(|s| {
            s.members
                .values()
                .filter(|m| s.is_active_member(&m.info.node_id) && m.info.node_id != me)
                .filter(|m| {
                    matches!(
                        self.inner.gossip.liveness_of(&m.info.node_id),
                        Liveness::Alive
                    )
                })
                .map(|m| m.info.clone())
                .take(3)
                .collect()
        });
        for peer in peers {
            let _ = self.inner.gossip.sync_with(&peer).await;
        }
        info!("drain complete — exiting so the supervisor starts {target}");
        let _ = self.inner.exit_tx.send(true);
    }

    /// `force` is `--deadline`: at the deadline, whatever still runs here is
    /// stopped so the drain ends. Without it only deployment instances are
    /// bound by the deadline — they move — and other work is waited for, as a
    /// drain always has.
    async fn drain_stream(
        &self,
        user: UserId,
        env: SignedIpc,
        move_pending: bool,
        deadline_secs: u64,
        force: bool,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let deadline =
            mesh_proto::now_micros().saturating_add(deadline_secs.saturating_mul(1_000_000));
        let first_drain = !self.inner.draining.swap(true, Ordering::SeqCst);
        if first_drain {
            self.announce_draining(Some(deadline));
        }
        self.audit_ok(
            &user,
            "node.drain",
            &self.node_id().to_display_string(),
            if move_pending { "move" } else { "" },
            &env,
        );
        // **A drain does not wait for sessions** (M17 §3.9). It waits for
        // running and queued jobs and owned fan-out drives; a human at a
        // prompt could hold that open forever, and the opposite behaviour
        // is the tempting one — it turns `meshnode drain` into a command
        // that sometimes never returns. So live sessions are closed with a
        // message naming this node and the reason, and their operators find
        // out rather than wondering.
        let closed = self.inner.sessions.close_hosted(&format!(
            "{} is draining and closed this session",
            self.inner.config.node_name
        ));
        if closed > 0 {
            info!(sessions = closed, "drain closed live sessions");
        }
        info!(move_pending, "drain started");

        let mut moved = 0u32;
        if move_pending {
            moved = self.move_queued().await;
        }

        // The client hanging up never stops the drain — like fan-out
        // ownership, the node finishes what it started.
        let mut forced = false;
        loop {
            if force && !forced && mesh_proto::now_micros() >= deadline {
                forced = true;
                let left = self.inner.jobs.active_ids();
                warn!(
                    jobs = left.len(),
                    "drain deadline reached — stopping what is still here"
                );
                for id in left {
                    if let Err(e) = self.inner.jobs.cancel(&id) {
                        warn!(job = %id.short(), "drain deadline cancel failed: {e}");
                    }
                }
            }
            let running = u32::try_from(self.inner.jobs.running_count()).unwrap_or(u32::MAX);
            let queued = u32::try_from(self.inner.jobs.queued_count()).unwrap_or(u32::MAX);
            let drives = self.inner.active_drives.lock().len();
            let _ = out
                .send(IpcResponse::DrainProgress {
                    running,
                    queued,
                    moved,
                    devices_held: u32::try_from(self.inner.jobs.device_holders().len())
                        .unwrap_or(u32::MAX),
                })
                .await;
            if running == 0 && queued == 0 && drives == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Best-effort final push so peers hold our last job summaries
        // before the transport goes away.
        let me = self.node_id();
        let peers: Vec<MemberInfo> = self.inner.engine.read(|s| {
            s.members
                .values()
                .filter(|m| s.is_active_member(&m.info.node_id) && m.info.node_id != me)
                .filter(|m| {
                    matches!(
                        self.inner.gossip.liveness_of(&m.info.node_id),
                        Liveness::Alive
                    )
                })
                .map(|m| m.info.clone())
                .take(3)
                .collect()
        });
        for peer in peers {
            let _ = self.inner.gossip.sync_with(&peer).await;
        }

        info!("drain complete — node is idle");
        let _ = out.send(IpcResponse::Ok).await;
        let _ = self.inner.exit_tx.send(true);
    }

    /// Announce the reserved `draining` tag via an ordinary MemberUpdate.
    /// The boot-time re-announcement clears it: a restarted node is not
    /// draining.
    ///
    /// **With the deadline its instances have to move by** (Phase C, E9):
    /// `drain.by=<unix seconds>`, which the reconciler reads. A node that
    /// announced only `draining` had every instance cancelled in one tick.
    ///
    /// From the node's own announcements, not the member record: that record
    /// is unioned with what admins assigned, and announcing it back would
    /// make an assignment this node's own statement.
    fn announce_draining(&self, deadline_micros: Option<u64>) {
        let me = self.node_id();
        let Some((info, announced)) = self.inner.engine.read(|s| {
            s.members
                .get(&me)
                .map(|m| (m.info.clone(), m.announced_tags.clone()))
        }) else {
            return;
        };
        let mut tags: Vec<String> = announced
            .into_iter()
            .filter(|t| !mesh_proto::is_drain_tag(t))
            .collect();
        if !tags.iter().any(|t| t == mesh_proto::DRAINING_TAG) {
            tags.push(mesh_proto::DRAINING_TAG.to_string());
        }
        tags.extend(mesh_proto::drain_tags(deadline_micros, false));
        if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
            node_id: me,
            name: info.name,
            addrs: info.addrs,
            tags,
            slots: info.slots,
        }) {
            warn!("cannot announce draining state: {e}");
        }
    }

    /// Re-dispatch every still-queued job to the best other candidate
    /// (`drain --move`). Placement follows the same rules as submission:
    /// selector-free candidates, grant-filtered for the submitting user,
    /// ranked by the scheduler; only a definitive Busy spills onward. Jobs
    /// nobody can take are re-queued locally and the drain waits for them.
    async fn move_queued(&self) -> u32 {
        let specs = self.inner.jobs.take_queued();
        let mut moved = 0u32;
        for spec in specs {
            let job_id = spec.job_id;
            match self.redispatch(spec.clone()).await {
                MoveOutcome::Moved(target) => {
                    moved += 1;
                    info!(job = %job_id.short(), to = %target, "moved queued job");
                }
                MoveOutcome::NobodyTook(reason) => {
                    warn!(job = %job_id.short(),
                        "cannot move queued job ({reason}); keeping it here — drain waits for it");
                    if let Err(e) = self.inner.jobs.accept(spec) {
                        error!(job = %job_id.short(), "re-queue after failed move: {e}");
                    }
                }
                // The invariant is never-duplicate-execution (§10.3): when a
                // target may have journaled the job but cannot confirm, the
                // job belongs to it — running it here too could double-run.
                MoveOutcome::Ambiguous(target) => {
                    moved += 1;
                    warn!(job = %job_id.short(), target = %target,
                        "dispatch outcome ambiguous — the job is left with that node \
                         (it will run there or surface as failed; it will not run twice)");
                }
            }
        }
        moved
    }

    /// Find a new home for one job. Double-execution safety (§10.3): after
    /// an ambiguous submit failure we only continue if the target verifiably
    /// never journaled the job; if that cannot be established, the job is
    /// treated as placed there (its journal — if any — owns it).
    async fn redispatch(&self, spec: JobSpec) -> MoveOutcome {
        let me = self.node_id();
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        let service = match &spec.payload {
            JobPayload::Service(exec) => Some(exec.service_name.clone()),
            // An inline script names no Library entry and needs no bundle:
            // it carries its own body (§4.7).
            JobPayload::Command(_) | JobPayload::Script(_) => None,
        };
        let bundle = match &spec.payload {
            JobPayload::Service(exec) => Some(exec.bundle_id),
            JobPayload::Command(_) | JobPayload::Script(_) => None,
        };
        // The user's placement intent travels in the signed request; a
        // moved job must still land inside its original selector.
        let selector = postcard::from_bytes::<IpcRequest>(&spec.auth.payload)
            .ok()
            .and_then(|req| match req {
                IpcRequest::Run { on, .. } | IpcRequest::RunService { on, .. } => on,
                _ => None,
            })
            .map(|on| Selector::parse(&on))
            .unwrap_or(Selector::Any);
        let pressure_of = self.pressure_lookup();
        let ranked: Vec<MemberInfo> = self.inner.engine.read(|s| {
            let candidates: Vec<MemberInfo> = membership::resolve_feasible_for_env(
                s,
                &selector,
                me,
                |id| self.inner.gossip.liveness_of(id),
                // T3: the job's environment **plus its runtime**, so a
                // container job is never dispatched to a node with no
                // engine and a python job never to one with no python3.
                &spec.env_for_placement(),
                &pressure_of,
            )
            .into_iter()
            .filter(|m| m.node_id != me)
            .filter(|m| {
                rbac::can_run_on(
                    s,
                    &founder,
                    &spec.submitter_user,
                    m,
                    service.as_deref(),
                    now,
                )
            })
            .collect();
            scheduler::rank_with_pressure(
                s,
                candidates,
                bundle.as_ref(),
                me,
                |id| self.inner.transport.rtt_micros(id),
                &pressure_of,
                self.placement(),
            )
        });
        if ranked.is_empty() {
            return MoveOutcome::NobodyTook("no other candidate can run it".into());
        }
        for target in ranked {
            match self.submit_remote(target.node_id, spec.clone()).await {
                Ok(()) => return MoveOutcome::Moved(target.name),
                // Neither took it: nothing was delivered (ENG-6).
                Err(CoreError::Busy { .. } | CoreError::Unreachable(_)) => continue,
                Err(e) => {
                    // Ambiguous: the target may or may not have journaled
                    // the job. Probe; only a definitive "not here" lets us
                    // keep looking.
                    match self.has_remote_job(target.node_id, spec.job_id).await {
                        Some(false) => continue,
                        Some(true) => return MoveOutcome::Moved(target.name),
                        None => {
                            warn!(target = %target.name, "ambiguous dispatch: {e}");
                            return MoveOutcome::Ambiguous(target.name);
                        }
                    }
                }
            }
        }
        MoveOutcome::NobodyTook("every candidate refused".into())
    }

    /// Stream a job's output to a peer that is allowed to read it.
    async fn serve_follow(
        &self,
        job_id: mesh_proto::JobId,
        stdout_offset: u64,
        stderr_offset: u64,
        follow: bool,
        reply: mpsc::Sender<Response>,
    ) {
        if !self.inner.jobs.has_record(&job_id) {
            let _ = reply
                .send(Response::Error {
                    message: format!("no such job here: {}", job_id.short()),
                })
                .await;
            return;
        }
        let mut rx = self
            .inner
            .jobs
            .follow(job_id, stdout_offset, stderr_offset, follow);
        while let Some(ev) = rx.recv().await {
            let frame = match ev {
                LogEvent::Chunk { stream, data } => Response::LogChunk { stream, data },
                // `Gone` and `Done` are the same frame here, and that is not
                // an oversight: this handler already answers from the
                // **replicated summary** rather than from the status file,
                // so a record that has left this disk still reports the
                // job's real outcome. It is the shape the IPC path had to be
                // taught (M31 A2).
                LogEvent::Done { .. } | LogEvent::Gone => {
                    match self.inner.engine.read(|s| s.jobs.get(&job_id).cloned()) {
                        Some(summary) => Response::JobEnd { summary },
                        None => Response::Error {
                            message: "job ended but summary missing".into(),
                        },
                    }
                }
            };
            let is_end = matches!(frame, Response::JobEnd { .. } | Response::Error { .. });
            if reply.send(frame).await.is_err() || is_end {
                break;
            }
        }
    }

    /// Did `peer` dispatch `job` here, or does it own the fan-out or gang
    /// the job belongs to? The nodes that may act on a job without a
    /// person's signature in hand (SEC-15).
    fn dispatched_by(&self, peer: NodeId, job: &mesh_proto::JobId) -> bool {
        if self
            .inner
            .jobs
            .spec_of(job)
            .is_some_and(|spec| spec.submitter == peer)
        {
            return true;
        }
        self.inner.engine.read(|s| {
            let fan = s
                .jobs
                .get(job)
                .and_then(|j| j.fanout)
                .and_then(|f| s.fanouts.get(&f.group))
                .is_some_and(|e| e.plan.submitter == peer || e.adopters.contains(&peer));
            fan || s.gangs.values().any(|g| {
                g.plan.members.iter().any(|m| m.job_id == *job)
                    && (g.plan.submitter == peer || g.adopters.contains(&peer))
            })
        })
    }

    /// [`Self::dispatched_by`], after one pull from `peer` when the job
    /// belongs to a fan-out or a gang: an adopter records its adoption a
    /// moment before it starts following the members, and this node may not
    /// have merged that yet.
    async fn dispatched_by_or_learn(&self, peer: NodeId, job: &mesh_proto::JobId) -> bool {
        if self.dispatched_by(peer, job) {
            return true;
        }
        let planned = self.inner.engine.read(|s| {
            s.jobs.get(job).is_some_and(|j| j.fanout.is_some())
                || s.gangs
                    .values()
                    .any(|g| g.plan.members.iter().any(|m| m.job_id == *job))
        });
        if !planned {
            return false;
        }
        self.pull_state_from(peer).await;
        self.dispatched_by(peer, job)
    }

    /// May the legacy `CancelJob` from `peer` stop `job`? The nodes above,
    /// and — for a deployment instance — the deployment's elected
    /// reconciler, which retires instances it did not start (SEC-15).
    fn may_cancel_unsigned(&self, peer: NodeId, job: &mesh_proto::JobId) -> bool {
        if self.dispatched_by(peer, job) {
            return true;
        }
        let Some(spec) = self.inner.jobs.spec_of(job) else {
            return false;
        };
        let Ok(IpcRequest::DeploymentFire { deployment, .. }) =
            postcard::from_bytes::<IpcRequest>(&spec.auth.payload)
        else {
            return false;
        };
        let gossip = self.inner.gossip.clone();
        self.inner.engine.read(|s| {
            crate::deployments::elected_reconciler(s, &deployment, |id| gossip.liveness_of(id))
                == Some(peer)
        })
    }

    /// Is `env` a person's own, fresh request to read `job`'s output, and
    /// may that person read it here? (SEC-15)
    fn verify_log_read(
        &self,
        env: &SignedIpc,
        job: &mesh_proto::JobId,
        courier: NodeId,
    ) -> std::result::Result<(), String> {
        if crate::identity::verify_ipc(env, self.mesh_id(), &[self.node_id(), courier]).is_none() {
            return Err("invalid log-read signature".into());
        }
        let now = mesh_proto::now_micros();
        let skew = u64::try_from(AUTH_SKEW.as_micros()).unwrap_or(u64::MAX);
        if env.at_micros.abs_diff(now) > skew {
            return Err("log-read request outside the freshness window".into());
        }
        match postcard::from_bytes::<IpcRequest>(&env.payload) {
            Ok(IpcRequest::JobLogs { job_id, .. }) if job_id == *job => {}
            _ => return Err("the signed request is not a read of this job's output".into()),
        }
        if !self
            .inner
            .nonces
            .lock()
            .accept((env.user, env.nonce), now, skew)
        {
            return Err("replayed log-read request".into());
        }
        let founder = self.inner.engine.genesis().body.founder_user;
        self.inner
            .engine
            .read(|s| rbac::require_role(s, &founder, &env.user, rbac::READ, now))
    }

    /// Does `executor` hold a journal for `job_id`? `None` = unreachable.
    async fn has_remote_job(&self, executor: NodeId, job_id: mesh_proto::JobId) -> Option<bool> {
        let conn = self.remote_conn(executor).await.ok()?;
        let req = Request::FollowJob {
            job_id,
            stdout_offset: 0,
            stderr_offset: 0,
            follow: false,
        };
        let mut stream = self
            .inner
            .transport
            .request_stream(&conn, &req)
            .await
            .ok()?;
        match stream.next().await {
            Ok(Some(Response::Error { message })) if message.contains("no such job") => Some(false),
            Ok(Some(_)) => Some(true),
            _ => None,
        }
    }

    /// Service-glob scoping check for content management (grants can be
    /// limited to service-name patterns).
    fn gate_service_scope(
        &self,
        user: &UserId,
        name: &str,
        action: &str,
        env: &SignedIpc,
    ) -> std::result::Result<(), Box<IpcResponse>> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let allowed = self
            .inner
            .engine
            .read(|s| rbac::can_manage_service(s, &founder, user, name, mesh_proto::now_micros()));
        if allowed {
            Ok(())
        } else {
            let message = format!("your grants do not cover the service {name:?}");
            audit::record(
                &self.inner.engine,
                *user,
                action,
                name,
                &format!("denied: {message}"),
                "",
                &env.signature,
            );
            Err(Box::new(IpcResponse::Denied { message }))
        }
    }

    fn whoami(&self, user: &UserId) -> WhoAmIInfo {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            let registered = s.users.contains_key(user) || user.0 == founder;
            let username = s.users.get(user).map(|u| u.record.username.clone());
            let roles = rbac::roles_at(s, &founder, user, now)
                .iter()
                .map(|r| r.as_str().to_string())
                .collect();
            let grants = self
                .grant_rows_from(s)
                .into_iter()
                .filter(|g| g.user_id == *user)
                .collect();
            WhoAmIInfo {
                user_id: *user,
                username,
                registered,
                is_founder: user.0 == founder,
                roles,
                grants,
                node_name: self.inner.config.node_name.clone(),
                node_id: self.node_id(),
                enrolment: enrolment_state_in(s, user, registered, now),
                // Only while there is still something to compare. An
                // approved person's old code is not a fact about them any
                // more, and showing it invites reading it out.
                enrolment_code: (!registered)
                    .then(|| s.enrolments.get(user))
                    .flatten()
                    .filter(|e| e.is_pending_at(now))
                    .map(|e| e.request.code()),
                // With no openable user key on this node, no key is "the
                // daemon's own" — which is the right answer, not a missing
                // one: there is nothing here to be the same as.
                is_daemon_key: self
                    .inner
                    .identity
                    .user
                    .as_ref()
                    .is_some_and(|k| *user == k.user_id()),
            }
        })
    }

    /// Where one key stands in the enrolment queue, and what to show about
    /// it (M23 §3.2).
    fn enrolment_status(
        &self,
        user: &UserId,
    ) -> (EnrolmentState, Option<String>, Option<u64>, Option<String>) {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            let registered = s.users.contains_key(user) || user.0 == founder;
            let state = enrolment_state_in(s, user, registered, now);
            let entry = (!registered).then(|| s.enrolments.get(user)).flatten();
            (
                state,
                entry.map(|e| e.request.code()),
                entry.map(|e| e.request.expires_at()),
                entry
                    .and_then(|e| e.declined.as_ref())
                    .map(|(by, _, _)| rbac::username_of(s, &founder, by)),
            )
        })
    }

    /// The pending queue, newest ask first — an admin reads a queue from
    /// the top, and the newest ask is the one somebody is waiting on.
    fn enrolment_rows(&self) -> Vec<EnrolmentRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            let mut rows: Vec<EnrolmentRow> = s
                .enrolments
                .values()
                // Expired requests are not shown, ever: a queue that keeps
                // what nobody acted on is a queue that stops being read.
                .filter(|e| !e.request.is_expired_at(now))
                .filter(|e| !s.users.contains_key(&e.request.user_id))
                .map(|e| EnrolmentRow {
                    user_id: e.request.user_id,
                    suggested_username: e.request.suggested_username.clone(),
                    from_node: s
                        .members
                        .get(&e.request.from_node)
                        .map(|m| m.info.name.clone())
                        .unwrap_or_else(|| e.request.from_node.short()),
                    code: e.request.code(),
                    requested_at_micros: e.request.created_at_micros,
                    expires_at_micros: e.request.expires_at(),
                    declined: e.is_declined(),
                    declined_by: e
                        .declined
                        .as_ref()
                        .map(|(by, _, _)| rbac::username_of(s, &founder, by)),
                    // E1-P5: what a provider proved, when the person ran
                    // `meshnode login` before asking. Absent is the normal
                    // case and stays normal — SSO is optional.
                    verified: s.oidc_assertions.get(&e.request.user_id).map(|(a, _)| {
                        mesh_proto::VerifiedIdentity {
                            provider: a.provider.clone(),
                            subject: a.subject.clone(),
                            email: a.email.clone(),
                            display_name: a.display_name.clone(),
                            groups: a.groups.clone(),
                            suggested_role: a.suggested_role.map(|r| r.as_str().to_string()),
                            verified_by: s
                                .members
                                .get(&a.verified_by)
                                .map(|m| m.info.name.clone())
                                .unwrap_or_else(|| a.verified_by.short()),
                            fresh: a.is_fresh(now),
                        }
                    }),
                })
                .collect();
            rows.sort_by(|a, b| {
                b.requested_at_micros
                    .cmp(&a.requested_at_micros)
                    .then_with(|| a.suggested_username.cmp(&b.suggested_username))
            });
            rows
        })
    }

    /// Author one enrolment request on behalf of the key that signed the
    /// envelope (M23 §3.1).
    fn ipc_enrol_submit(
        &self,
        user: &UserId,
        request: EnrolmentRequest,
        env: &SignedIpc,
        source: Source,
    ) -> IpcResponse {
        // A caller may enrol themselves and nobody else. The envelope's
        // signature is what proves who is asking; the record has to name
        // the same key, or an admin's queue could be filled with requests
        // in other people's names.
        if request.user_id != *user {
            return IpcResponse::Error {
                message: "an enrolment request must name the key that signed it".into(),
            };
        }
        if request.from_node != self.node_id() {
            return IpcResponse::Error {
                message: "an enrolment request must name the node it was made from".into(),
            };
        }
        let founder = self.inner.engine.genesis().body.founder_user;
        // "Already a user" is true of a *revoked* key too, and answering a
        // revoked key with it is the least useful sentence available: it
        // reads as "you are fine" to somebody every request is failing for.
        // Revocation is terminal, so the honest answer is that this key is
        // finished and a new one is the way forward.
        if let Some(message) = self.inner.engine.read(|s| {
            if user.0 == founder {
                return Some("you are already a user on this mesh".to_string());
            }
            match s.users.get(user) {
                Some(u) if u.revoked.is_some() => Some(format!(
                    "{} was revoked, and revocation is permanent — this key cannot \
                     be re-enrolled. Delete this machine's identity/user.key to \
                     generate a new one, then run `meshnode enrol` again",
                    user.short()
                )),
                Some(_) => Some("you are already a user on this mesh".to_string()),
                None => None,
            }
        }) {
            return IpcResponse::Error { message };
        }
        // Anybody with a key can get this far, and a key costs nothing to
        // make (SEC-19). A new request is refused when the queue is already
        // longer than anybody reads, or this caller's address, or the node
        // as a whole, is asking faster than people enrol. A request already
        // in the queue may be renewed whatever the queue's length.
        let now = mesh_proto::now_micros();
        let (queued, pending) = self.inner.engine.read(|s| {
            (
                s.enrolments.get(user).is_some_and(|e| e.is_pending_at(now)),
                s.enrolments
                    .values()
                    .filter(|e| e.is_pending_at(now))
                    .count(),
            )
        });
        if !queued && pending >= crate::throttle::MAX_PENDING_ENROLMENTS {
            return IpcResponse::Error {
                message: format!(
                    "{pending} enrolment requests are already waiting for an admin — try again \
                     once they have been answered"
                ),
            };
        }
        if !self.inner.anon.enrol_source.admit(&source.key())
            || !self.inner.anon.enrol_all.admit("all")
        {
            return IpcResponse::Error {
                message: format!(
                    "{}enrolment requests — try again in a minute",
                    crate::throttle::TOO_MANY
                ),
            };
        }
        match self.inner.engine.author_op(OpBody::EnrolRequest {
            request: Box::new(request),
        }) {
            Ok(_) => {
                // Audited without a role check, which is the only place in
                // this file that happens: the record of *who asked* is
                // exactly as load-bearing as the record of who approved.
                self.audit_ok(user, "enrol.request", &user.to_display_string(), "", env);
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        }
    }

    fn user_rows(&self) -> Vec<UserRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            let mut rows: Vec<UserRow> = s
                .users
                .values()
                .map(|u| UserRow {
                    user_id: u.record.user_id,
                    username: u.record.username.clone(),
                    is_founder: u.record.user_id.0 == founder,
                    revoked: u.revoked.is_some(),
                    roles: rbac::roles_at(s, &founder, &u.record.user_id, now)
                        .iter()
                        .map(|r| r.as_str().to_string())
                        .collect(),
                    created_at_micros: u.record.created_at_micros,
                    // Y2/Y3. Read from the verified attestation, never from
                    // a claim a client made: the whole value of this column
                    // is that the mesh checked the chain itself.
                    hardware: s.attestations.get(&u.record.user_id).map(|a| {
                        mesh_proto::HardwareRow {
                            kind: a.attestation.kind.as_str().to_string(),
                            slot: a.attestation.slot.clone(),
                            root_label: a.attested.root_label.clone(),
                            subject: a.attested.subject.clone(),
                            attested_at_micros: a.attestation.added_at_micros,
                            not_after_micros: a.attested.not_after_micros,
                            revoked: a.revoked,
                            revoked_reason: a.revoked_reason.clone(),
                        }
                    }),
                })
                .collect();
            rows.sort_by(|a, b| a.username.cmp(&b.username));
            rows
        })
    }

    /// Every attestation authority this mesh trusts (Y2/Y3).
    fn attest_root_rows(&self) -> Vec<mesh_proto::AttestRootRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::AttestRootRow> = s
                .attest_roots
                .iter()
                .map(|(id, e)| mesh_proto::AttestRootRow {
                    id: id.clone(),
                    label: e.root.label.clone(),
                    added_by: e.root.added_by,
                    added_by_username: rbac::username_of(s, &founder, &e.root.added_by),
                    added_at_micros: e.root.added_at_micros,
                    removed: e.removed,
                    // Counted rather than stored, so removing a root shows
                    // immediately what stops being vouched for.
                    attesting: u32::try_from(
                        s.attestations
                            .values()
                            .filter(|a| !a.revoked && a.attested.root_label == e.root.label)
                            .count(),
                    )
                    .unwrap_or(u32::MAX),
                })
                .collect();
            rows.sort_by(|a, b| a.label.cmp(&b.label));
            rows
        })
    }

    fn service_account_rows(&self) -> Vec<mesh_proto::ServiceAccountRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::ServiceAccountRow> = s
                .service_accounts
                .values()
                .map(|a| mesh_proto::ServiceAccountRow {
                    user_id: a.user_id,
                    name: a.name.clone(),
                    description: a.description.clone(),
                    created_at_micros: a.created_at_micros,
                    expires_at_micros: a.expires_at_micros,
                    expired: a.expired_at(now),
                    revoked: s.users.get(&a.user_id).is_some_and(|u| u.revoked.is_some()),
                    // Reported as of *now*, so an expired account shows the
                    // empty set it actually has rather than the roles it
                    // was granted — `roles_at` is the same function the
                    // authorization path calls, which is the point.
                    roles: rbac::roles_at(s, &founder, &a.user_id, now)
                        .iter()
                        .map(|r| r.as_str().to_string())
                        .collect(),
                })
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            rows
        })
    }

    fn grant_rows(&self) -> Vec<GrantRow> {
        self.inner.engine.read(|s| self.grant_rows_from(s))
    }

    fn grant_rows_from(&self, s: &crate::state::MeshState) -> Vec<GrantRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        let mut rows: Vec<GrantRow> = s
            .grants
            .values()
            .map(|g| GrantRow {
                grant_id: g.grant.grant_id,
                user_id: g.grant.user_id,
                username: rbac::username_of(s, &founder, &g.grant.user_id),
                holder_revoked: s
                    .users
                    .get(&g.grant.user_id)
                    .is_some_and(|u| u.revoked.is_some()),
                role: g.grant.role.as_str().to_string(),
                nodes: g.grant.nodes.clone(),
                services: g.grant.services.clone(),
                expires_at_micros: g.grant.expires_at_micros,
                revoked: g.revoked,
                expired: g.grant.expires_at_micros.is_some_and(|e| now > e),
                created_at_micros: g.grant.created_at_micros,
            })
            .collect();
        rows.sort_by_key(|r| r.created_at_micros);
        rows
    }

    fn secret_rows(&self) -> Vec<SecretRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        self.inner.engine.read(|s| {
            let members = u32::try_from(
                s.members
                    .values()
                    .filter(|m| s.is_active_member(&m.info.node_id))
                    .count(),
            )
            .unwrap_or(u32::MAX);
            let mut rows: Vec<SecretRow> = s
                .secrets
                .values()
                .map(|e| SecretRow {
                    name: e.record.name.clone(),
                    version: e.record.version,
                    created_by: e.record.created_by,
                    created_by_name: rbac::username_of(s, &founder, &e.record.created_by),
                    created_at_micros: e.record.created_at_micros,
                    // Recipients, not wraps: a node can hold several
                    // (SEC-17).
                    wrapped_for: u32::try_from(
                        e.record
                            .wraps
                            .iter()
                            .map(|w| w.node_id)
                            .collect::<HashSet<_>>()
                            .len(),
                    )
                    .unwrap_or(u32::MAX),
                    members,
                    removed: e.removed,
                })
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            rows
        })
    }

    /// Extend secret wraps for active members that joined after sealing —
    /// any node that can unwrap heals the others, no user needed. Runs
    /// periodically in the background; public so tests and embedders can
    /// trigger it eagerly.
    pub fn rewrap_secrets(&self) {
        let me = self.node_id();
        let gossip = self.inner.gossip.clone();
        let mut memo = self.inner.secret_memo.lock().clone();
        struct Pass {
            record: Option<mesh_proto::SecretRecord>,
            name: String,
            version: u32,
            content: SecretContent,
            /// Recipients this node might heal, with how many times each
            /// has asked (SEC-17).
            targets: Vec<(NodeId, u32)>,
            /// This node's standing statement for the record.
            mine: crate::state::HealStatement,
        }
        let (work, candidates, quarantined, live): (Vec<Pass>, Vec<NodeId>, bool, HashSet<String>) =
            self.inner.engine.read(|s| {
                // **The active-member list, once** (**M-6**). This ran every
                // fifteen seconds and rebuilt the list inside the per-secret
                // closure, then asked `wraps.iter().any(..)` per member — so
                // 200 secrets against 1 000 members was 200 membership walks
                // and 2 × 10⁸ comparisons per pass, under the state read lock,
                // to discover that nothing had changed.
                // A quarantined node is not healed (SEC-16): it may be the
                // machine somebody is investigating.
                let active: Vec<NodeId> = s
                    .members
                    .values()
                    .filter(|m| s.is_active_member(&m.info.node_id))
                    .filter(|m| !s.is_quarantined(&m.info.node_id))
                    .map(|m| m.info.node_id)
                    .collect();
                let candidates: Vec<NodeId> = active
                    .iter()
                    .copied()
                    .filter(|n| *n == me || matches!(gossip.liveness_of(n), Liveness::Alive))
                    .collect();
                let live = s
                    .secrets
                    .values()
                    .filter(|e| !e.removed)
                    .map(|e| e.record.name.clone())
                    .collect();
                // Who a secret is for (SEC-7): the default set once, a
                // scoped secret's own set where it has one.
                let recipients_of = |name: &str| -> Vec<NodeId> {
                    s.members
                        .values()
                        .filter(|m| secrets::is_recipient(s, name, &m.info))
                        .map(|m| m.info.node_id)
                        .collect()
                };
                let unscoped = recipients_of("");
                let work = s
                    .secrets
                    .values()
                    .filter(|e| !e.removed)
                    .filter_map(|e| {
                        let content = SecretContent::of(&e.record, me);
                        let mine = e.heals.get(&me).cloned().unwrap_or_default();
                        // A set per secret rather than a linear scan per
                        // (secret, member) pair: `wraps` is as long as the
                        // membership, so the inner `any` was the second
                        // factor in the square.
                        let wrapped: HashSet<NodeId> =
                            e.record.wraps.iter().map(|w| w.node_id).collect();
                        let scoped = s
                            .secret_scopes
                            .get(&e.record.name)
                            .is_some_and(|sc| !sc.nodes.trim().is_empty());
                        let for_this = if scoped {
                            recipients_of(&e.record.name)
                        } else {
                            unscoped.clone()
                        };
                        let targets: Vec<(NodeId, u32)> = for_this
                            .iter()
                            .copied()
                            .filter(|t| *t != me && !mine.wraps.contains_key(t))
                            .filter_map(|t| match e.wants(&t) {
                                0 => (!wrapped.contains(&t)).then_some((t, 0)),
                                round => Some((t, round)),
                            })
                            .collect();
                        let opens = memo
                            .get(&e.record.name)
                            .filter(|m| m.content == content)
                            .map(|m| m.opens);
                        // The record is cloned only when there is crypto
                        // to do: to find out whether this node can open it,
                        // or to heal somebody with it.
                        let need_record = content.mine.is_some()
                            && (opens.is_none() || (opens == Some(true) && !targets.is_empty()));
                        // A node that can open it and is still asking has
                        // a retraction to make.
                        let need_pass = need_record || opens != Some(true) || mine.wants > 0;
                        need_pass.then(|| Pass {
                            record: need_record.then(|| e.record.clone()),
                            name: e.record.name.clone(),
                            version: e.record.version,
                            content,
                            targets,
                            mine,
                        })
                    })
                    .collect();
                (work, candidates, s.is_quarantined(&me), live)
            });
        memo.retain(|name, _| live.contains(name));
        let now = Instant::now();
        for pass in work {
            let known = memo
                .get(&pass.name)
                .filter(|m| m.content == pass.content)
                .map(|m| m.opens);
            let opens = match (&pass.record, pass.content.mine, known) {
                (_, None, _) => false,
                (_, Some(_), Some(known)) => known,
                (Some(record), Some(_), None) => {
                    secrets::unwrap_data_key(record, &self.inner.identity.node).is_ok()
                }
                (None, Some(_), None) => false,
            };
            // The clock restarts only when the answer changes. Restarting
            // it whenever this node's wraps changed would let a member that
            // re-sends a junk wrap every half minute keep it from ever
            // asking.
            let entry = memo
                .entry(pass.name.clone())
                .and_modify(|m| {
                    if m.opens != opens {
                        *m = SecretMemo::new(pass.content.clone(), opens, now);
                    } else {
                        m.content = pass.content.clone();
                    }
                })
                .or_insert_with(|| SecretMemo::new(pass.content.clone(), opens, now));
            let mut wants = pass.mine.wants;
            let mut new_wraps = Vec::new();
            if opens {
                wants = 0;
                if let Some(record) = &pass.record {
                    let missing: Vec<NodeId> = pass
                        .targets
                        .iter()
                        .filter(|(t, round)| {
                            secrets::is_healer(
                                record,
                                t,
                                &me,
                                candidates.iter().copied().filter(|c| c != t),
                                *round,
                            )
                        })
                        .map(|(t, _)| *t)
                        .collect();
                    if !missing.is_empty() {
                        match secrets::rewrap_for(record, &self.inner.identity.node, &missing) {
                            Ok(wraps) => new_wraps = wraps,
                            Err(e) => warn!(secret = %record.name, "rewrap failed: {e}"),
                        }
                    }
                }
            } else {
                // Say so — after a minute, so a joiner's first heal has
                // time to arrive, and then every five while it lasts. Each
                // one widens who heals this node.
                let settled = now.duration_since(entry.since) >= SECRET_WANT_AFTER;
                let quiet = entry
                    .asked
                    .is_none_or(|at| now.duration_since(at) >= SECRET_WANT_EVERY);
                if settled && quiet && !quarantined {
                    entry.asked = Some(now);
                    wants = wants.saturating_add(1);
                }
            }
            if new_wraps.is_empty() && wants == pass.mine.wants {
                continue;
            }
            // The whole statement, every time: what this node already
            // provides, what it adds, and whether it is asking.
            let healed = new_wraps.len();
            let mut wraps: Vec<mesh_proto::SecretWrap> =
                pass.mine.wraps.into_values().chain(new_wraps).collect();
            wraps.push(secrets::statement_marker(me, wants));
            match self.inner.engine.author_op(OpBody::SecretRewrap {
                name: pass.name.clone(),
                version: pass.version,
                wraps,
            }) {
                Err(e) => warn!(secret = %pass.name, "rewrap op failed: {e}"),
                Ok(_) if healed > 0 => {
                    info!(secret = %pass.name, nodes = healed, "rewrapped secret");
                }
                Ok(_) if wants > pass.mine.wants => {
                    info!(secret = %pass.name, "cannot open secret; asked to be rewrapped");
                }
                Ok(_) => {}
            }
        }
        *self.inner.secret_memo.lock() = memo;
    }

    /// Boot-time announcement: sync with a handful of dialable members so
    /// our address op spreads immediately (and we pull their state). Retries
    /// briefly; a single success is enough — gossip does the rest.
    fn spawn_boot_announce(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                let me = this.node_id();
                for _attempt in 0u32..10 {
                    let targets: Vec<MemberInfo> = this.inner.engine.read(|s| {
                        s.members
                            .values()
                            .filter(|m| s.is_active_member(&m.info.node_id))
                            .filter(|m| m.info.node_id != me && !m.info.addrs.is_empty())
                            .map(|m| m.info.clone())
                            .take(3)
                            .collect()
                    });
                    if targets.is_empty() {
                        return; // founding a fresh mesh: nobody to tell yet
                    }
                    let mut any = false;
                    for target in targets {
                        if this.inner.gossip.sync_with(&target).await.is_ok() {
                            any = true;
                        }
                    }
                    if any {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = shutdown.changed() => return,
                    }
                }
            }
        });
        self.track(handle);
    }

    /// Compact this node's own op-log on a timer (C0 P2).
    ///
    /// The interval is not a setting. The horizon is the knob — an op is
    /// eligible when it is superseded *and* older than
    /// `[state] compaction_horizon` — and how often a node checks whether
    /// anything has become eligible changes nothing an operator can
    /// observe except how promptly the disk follows. Two knobs would be one
    /// too many here for the same reason §4.2 gives for retention.
    ///
    /// A pass with nothing to do costs one range scan of this node's own
    /// ops and no writes at all, so an idle mesh pays an hourly walk of a
    /// log that is not growing.
    fn spawn_compaction_task(self) {
        if !self.inner.config.state.compaction_enabled {
            tracing::info!("op-log compaction is off (`[state] compaction_enabled = false`)");
            return;
        }
        let horizon = self.inner.config.state.horizon_secs();
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(COMPACTION_INTERVAL) => {}
                        _ = shutdown.changed() => return,
                    }
                    // Release aged-out usage windows from memory first: it
                    // is a lock and a retain, and it is worth doing even on
                    // a tick where nothing is eligible to leave the log.
                    let released = this.inner.engine.expire_usage(mesh_proto::now_micros());
                    if released > 0 {
                        tracing::debug!(released, "released usage windows past the decay horizon");
                    }
                    // Refresh the census on the same tick (M31 P0). It walks
                    // the whole log, so it belongs on a schedule rather than
                    // on whichever scrape happens to find the cache cold —
                    // and this tick is already paying for a walk of half of
                    // it.
                    {
                        let this = this.clone();
                        let _ = tokio::task::spawn_blocking(move || this.op_census()).await;
                    }
                    let engine = this.inner.engine.clone();
                    // A pass takes the store's write lock to reclaim, so it
                    // runs on the blocking pool rather than stalling a
                    // reactor thread that gossip is sharing.
                    let done =
                        tokio::task::spawn_blocking(move || engine.compact_self(horizon)).await;
                    match done {
                        Ok(Ok((0, _))) => {}
                        Ok(Ok((dropped, seq))) => {
                            tracing::info!(dropped, checkpoint_seq = seq, "op-log compacted");
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "op-log compaction pass failed");
                        }
                        Err(e) => tracing::warn!(error = %e, "op-log compaction task panicked"),
                    }
                    // Then everyone else's log, as far as their own
                    // checkpoints say (ENG-3). The same switch governs it:
                    // a node told not to compact keeps everything.
                    let engine = this.inner.engine.clone();
                    match tokio::task::spawn_blocking(move || engine.prune_peer_logs()).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "pruning peers' compacted ops failed");
                        }
                        Err(e) => tracing::warn!(error = %e, "peer pruning task panicked"),
                    }
                }
            }
        });
        self.track(handle);
    }

    /// Turn rate-limit episodes into audit entries, and forget entities
    /// that have gone quiet (M31 A3 item 4).
    ///
    /// **A rate limit that silently drops is indistinguishable from a bug.**
    /// One that refuses, names itself, and leaves a record is a control —
    /// so the episode reaches `meshnode audit` and the operator finds it
    /// there rather than by correlating a graph with a hunch.
    ///
    /// One entry per *episode*, never per refused op: `AuditAppend` is
    /// `Grow` and never compacts, so per-op auditing would let a runaway
    /// loop turn a log-growth problem into a permanent one — the control
    /// becoming the incident. The entry carries the count, so nothing is
    /// lost by saying it once.
    fn spawn_backpressure_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(BACKPRESSURE_TICK) => {}
                        _ = shutdown.changed() => return,
                    }
                    this.drain_backpressure();
                }
            }
        });
        self.track(handle);
    }

    /// One drain: episodes become audit entries, quiet entities are
    /// forgotten. Returns how many episodes were recorded.
    ///
    /// A named method rather than a closure inside the loop, so a test can
    /// run exactly what the tick runs instead of a second implementation of
    /// it that agrees on the day it is written.
    pub fn drain_backpressure(&self) -> usize {
        let now = mesh_proto::now_micros();
        let guard = self.inner.engine.rate_guard();
        let episodes = guard.drain_new_episodes(now);
        for episode in &episodes {
            warn!(
                entity = %episode.entity,
                observed_per_minute = episode.observed_per_minute,
                refused = episode.refused_total,
                "refusing ops: one entity is above this node's rate limit"
            );
            // Attributed to the mesh's founder identity: the refusal is the
            // *node's* decision, not a user's request, and inventing a user
            // for it would put a person's name on a machine's judgement.
            crate::audit::record(
                &self.inner.engine,
                mesh_proto::UserId(self.inner.engine.genesis().body.founder_user),
                "state.backpressure",
                &episode.entity,
                "refused",
                &format!(
                    "{} ops/min against a limit of {}; {} refused so far{}",
                    episode.observed_per_minute,
                    guard.limit(),
                    episode.refused_total,
                    if episode.paused_until_micros > 0 {
                        " — auto-paused, and it expires by itself"
                    } else {
                        ""
                    }
                ),
                &[],
            );
        }
        // A node that has run a million jobs must not carry a million map
        // entries. Anything with refusals on the record is kept — that is
        // the diagnosis.
        guard.sweep(now, BACKPRESSURE_IDLE_SECS);
        episodes.len()
    }

    /// Every accelerator in the mesh, or one node's (C5 P1).
    ///
    /// Read from replicated state, which is why *any* node can answer for
    /// *every* node with no discovery of its own and nobody asked. The two
    /// node-local columns — who holds a device, and how busy it is — are
    /// filled in for this node and left empty for the rest, because a hold
    /// is not replicated and a remote reading is only as fresh as the last
    /// gossip tick that carried one.
    fn device_rows(&self, node: Option<&str>) -> Vec<mesh_proto::DeviceRow> {
        let me = self.node_id();
        let holders = self.inner.jobs.device_holders();
        let own_pressure = self.inner.jobs.device_pressure();
        let mut rows = self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::DeviceRow> = Vec::new();
            for (id, inventory) in &s.devices {
                let Some(member) = s.members.get(id).filter(|m| !m.left) else {
                    continue;
                };
                if let Some(want) = node
                    && !member.info.name.eq_ignore_ascii_case(want)
                    && !id.to_display_string().starts_with(want)
                {
                    continue;
                }
                let telemetry = if *id == me {
                    own_pressure.clone()
                } else {
                    self.inner.gossip.device_pressure_of(id)
                };
                for d in &inventory.devices {
                    let live = mesh_proto::reading_for(&telemetry, d, &inventory.devices);
                    rows.push(mesh_proto::DeviceRow {
                        node: *id,
                        node_name: member.info.name.clone(),
                        index: d.index,
                        kind: d.kind.as_str().to_string(),
                        vendor: d.vendor.clone(),
                        model: d.model.clone(),
                        uuid: d.uuid.clone(),
                        mem_mb: d.mem_mb,
                        numa_node: d.numa_node,
                        health: d.health.as_str().to_string(),
                        health_reason: d.health.reason().map(str::to_string),
                        held_by: (*id == me)
                            .then(|| holders.get(&d.key()).cloned())
                            .flatten(),
                        util_pct: live.and_then(|p| p.util_pct),
                        mem_free_mb: live.and_then(|p| p.mem_free_mb),
                        temp_c: live.and_then(|p| p.temp_c),
                        throttled: live.and_then(|p| p.throttled),
                    });
                }
            }
            rows
        });
        rows.sort_by(|a, b| (a.node_name.as_str(), a.index).cmp(&(b.node_name.as_str(), b.index)));
        rows
    }

    /// How busy the *best* device on `node` that could serve `req` is, in
    /// `0.0..=1.0` (C5 P5).
    ///
    /// The best rather than the average, because a job needs one device and
    /// the one it will get is the idlest that matches. A node with seven
    /// saturated cards and one free one is an excellent place to put a
    /// single-GPU job, and averaging would rank it below a node with two
    /// half-busy ones.
    ///
    /// `None` — a job with no device request, a peer below `ACCEL_MINOR`,
    /// telemetry that has aged out, a node whose inventory has not
    /// gossiped here — is neutral, the same rule every other unknown in
    /// the capacity model follows.
    fn device_busy(&self, node: &NodeId, req: Option<&mesh_proto::DeviceRequest>) -> Option<f64> {
        let req = req?;
        let telemetry = if *node == self.node_id() {
            self.inner.jobs.device_pressure()
        } else {
            self.inner.gossip.device_pressure_of(node)
        };
        if telemetry.is_empty() {
            return None;
        }
        // Only the devices this request could actually be placed on: a
        // 4090's idleness says nothing about a job that asked for an A100,
        // and a card the driver has taken out of service reads 0% busy
        // while being no place at all. `matching` already excludes the
        // unschedulable, because `Device::matches` does.
        let readings: Option<Vec<mesh_proto::DevicePressure>> = self.inner.engine.read(|s| {
            let inv = s.devices.get(node)?;
            Some(
                inv.matching(req)
                    .into_iter()
                    .filter_map(|d| mesh_proto::reading_for(&telemetry, d, &inv.devices))
                    .cloned()
                    .collect(),
            )
        });
        // No inventory here yet: every reading is a device this node has,
        // and the best of them is still the best guess there is.
        readings
            .as_deref()
            .unwrap_or(&telemetry)
            .iter()
            .filter_map(mesh_proto::DevicePressure::busy_fraction)
            .min_by(f64::total_cmp)
    }

    /// Publish this node's accelerators, and keep them true (C5 P2/P6).
    ///
    /// One task, three jobs, and they belong together because all three
    /// read the same probe: publish the inventory entity, re-advertise when
    /// the hardware changes, and sample live device telemetry for
    /// `PressureV2`.
    ///
    /// **Why a re-probe at all, when boot already answered.** A driver
    /// upgrade, a card pulled from a running box, an ECC error that takes
    /// one out of service — none of those restart the daemon, and a mesh
    /// that only learns about its accelerators at boot is a mesh that
    /// places GPU work on a machine whose GPU left an hour ago. The probe
    /// is two forks and it runs on the interval below; the *op* is only
    /// authored when the answer changed, so a stable machine publishes
    /// exactly one inventory op for its whole life.
    ///
    /// A node with no accelerators at all still runs the task, and still
    /// costs nothing: `detect()` finds no vendor binary, returns empty,
    /// compares equal to what it published, and authors nothing.
    fn spawn_device_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                // Publish what boot probed before waiting a whole interval:
                // the answer to "plug in a GPU box and the mesh knows what
                // it has within a gossip round" is this line.
                this.publish_device_inventory(this.inner.jobs.devices());
                loop {
                    // Telemetry is sampled far more often than the
                    // inventory is re-probed, because it is what changes:
                    // utilisation moves every second and a card's model
                    // never does.
                    for _ in 0..DEVICE_REPROBE_TICKS {
                        tokio::select! {
                            _ = tokio::time::sleep(DEVICE_PRESSURE_INTERVAL) => {}
                            _ = shutdown.changed() => return,
                        }
                        if this.inner.jobs.devices().is_empty() {
                            continue;
                        }
                        let sampled = tokio::task::spawn_blocking(crate::devices::pressure).await;
                        if let Ok(sampled) = sampled {
                            this.inner.jobs.set_device_pressure(sampled);
                        }
                    }
                    let Ok(fresh) = tokio::task::spawn_blocking(crate::devices::detect).await
                    else {
                        continue;
                    };
                    if fresh == this.inner.jobs.devices() {
                        continue;
                    }
                    info!(
                        devices = fresh.len(),
                        "device inventory changed; re-advertising"
                    );
                    this.inner.jobs.set_devices(fresh.clone());
                    // Both halves, and both are needed: the entity is what
                    // `nodes --devices` reads, and the tag is what a
                    // selector filters on. Publishing one without the other
                    // is how a mesh ends up sending GPU work to a node that
                    // no longer has one.
                    this.reannounce_device_tags(&fresh);
                    this.publish_device_inventory(fresh);
                }
            }
        });
        self.track(handle);
    }

    /// Author this node's device inventory, if it differs from what is
    /// already replicated about it.
    fn publish_device_inventory(&self, devices: Vec<mesh_proto::Device>) {
        let me = self.node_id();
        let inventory = mesh_proto::DeviceInventory::new(me, devices, mesh_proto::now_micros());
        // Debounce on the *devices*, not on the record: the publish instant
        // differs every time and comparing whole records would author an op
        // per interval forever.
        let unchanged = self
            .inner
            .engine
            .read(|s| s.devices.get(&me).map(|cur| cur.devices.clone()));
        if unchanged.as_ref() == Some(&inventory.devices) {
            return;
        }
        if inventory.devices.is_empty() && unchanged.is_none() {
            // A machine with no accelerators never publishes an empty
            // record. Most machines in most meshes have none, and one op
            // each to say so is a thousand ops that mean nothing.
            return;
        }
        if let Err(e) = self.inner.engine.author_op(OpBody::DeviceInventoryPut {
            inventory: Box::new(inventory),
        }) {
            warn!("cannot publish device inventory: {e}");
        }
    }

    /// Re-announce the coarse `cap.gpus` / `cap.vram_mb` filter tags after
    /// a driver change, preserving every other tag (§9.1).
    /// Probe what this node can run, announce it, and reap what the last
    /// run left behind (T1, T2, N6).
    ///
    /// Everything here shells out, which is why it is a task rather than a
    /// step in `Node::new`: five probes and a container-engine round trip
    /// must not stand between a restarted node and the mesh.
    ///
    /// The order matters. Detection first, because the reap needs the
    /// engine detection found; announcement between them, because a node
    /// that can run containers should be saying so before it spends time
    /// tidying up.
    fn spawn_runtime_probe(self) {
        self.clone().track(tokio::spawn(async move {
            let opts = crate::runtimes::ContainerOptions {
                plugin: self.inner.config.runtime.container_plugin.clone(),
                prefer: self
                    .inner
                    .config
                    .runtime
                    .container_engine
                    .as_deref()
                    .and_then(crate::engines::EngineKind::parse),
                engine_socket: Some(self.inner.config.layout().engine_socket()),
            };
            let reports = crate::runtimes::detect(&opts).await;
            let engine = match &opts.plugin {
                // A configured plugin owns the container path, so no engine
                // is installed beside it — two runners for one runtime is
                // how a node ends up starting a container one way and
                // stopping it the other.
                Some(p) if !p.trim().is_empty() => None,
                _ => {
                    crate::engines::detect(opts.prefer, opts.engine_socket.as_deref())
                        .await
                        .engine
                }
            };
            for r in &reports {
                match r.detection {
                    mesh_proto::runtimes::Detection::Ready => {
                        info!(runtime = r.runtime.as_str(), "ready: {}", r.detail);
                    }
                    // A runtime this machine simply does not have is not a
                    // problem to report at `warn` — most nodes run none of
                    // ansible, python or containers, and a node that warned
                    // about each would teach its operator to ignore warnings.
                    mesh_proto::runtimes::Detection::Absent => {
                        debug!(runtime = r.runtime.as_str(), "not detected: {}", r.remedy);
                    }
                    // Installed and refusing to work *is* worth saying: it
                    // is the state somebody meant to have working.
                    //
                    // **One line.** A remedy for something like rootless
                    // podman is several paragraphs and a heredoc, and
                    // journald renders a multi-line message as an indented
                    // block wider than most terminals — truncated, on the
                    // line that mattered, once per capacity refresh. The
                    // headline is written to stand alone for exactly this;
                    // the rest is in `meshnode runtimes`, which has room.
                    mesh_proto::runtimes::Detection::Broken => {
                        warn!(
                            runtime = r.runtime.as_str(),
                            "installed but not usable: {} — {} (run `meshnode runtimes` for \
                             the full diagnosis)",
                            r.detail,
                            r.headline()
                        );
                    }
                }
            }
            let engine_for_reap = engine.clone();
            self.inner.jobs.set_runtimes(reports, engine);
            self.reannounce_runtime_tags();

            // N6. A node knows one thing at boot it never knows again: it
            // is running nothing. Anything still labelled with this node's
            // id outlived the process that started it — conmon keeps a
            // container alive when `meshnode` is killed — and is holding a
            // host port the rebuilt ledger believes is free. Left alone it
            // becomes the next deployment's crash loop.
            match (
                self.inner.config.runtime.container_plugin.as_deref(),
                &engine_for_reap,
            ) {
                (Some(plugin), _) if !plugin.trim().is_empty() => {
                    crate::containers::reap_orphans(plugin, &self.node_id().short()).await;
                }
                (_, Some(engine)) => {
                    crate::engines::reap_orphans(engine, &self.node_id().short()).await;
                }
                _ => {}
            }
        }));
    }

    // -- mesh reset ------------------------------------------------------------

    /// `meshnode reset`: publish the founder's reset, hand it to every member
    /// this node can reach, say who has it, then leave with the rest.
    ///
    /// The report goes out before this node leaves, because it is the last
    /// thing the founder will see of this mesh: once every node has moved
    /// its op-log aside there is no "which machines did it reach?" to ask.
    async fn reset_stream(
        &self,
        user: UserId,
        env: SignedIpc,
        reset: mesh_proto::MeshReset,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let genesis = self.inner.engine.genesis().body.clone();
        let refused = if user.0 != genesis.founder_user {
            Some(
                "only the mesh's founder can reset it. It ends the mesh for every member, and \
                 that is not something an admin grant can hand on"
                    .to_string(),
            )
        } else if reset.by != user {
            Some("the reset is signed by a different user".to_string())
        } else if reset.mesh_id != genesis.mesh_id {
            Some(format!(
                "the reset names mesh {}, and this node is in {}",
                reset.mesh_id.to_display_string(),
                genesis.mesh_id.to_display_string()
            ))
        } else if !<[u8; 64]>::try_from(reset.signature.as_slice())
            .is_ok_and(|sig| verify_signature(&user.0, &reset.signing_bytes(), &sig))
        {
            Some("the reset's signature does not verify".to_string())
        } else if self.inner.resetting.swap(true, Ordering::SeqCst) {
            Some("this node is already leaving a reset mesh".to_string())
        } else {
            None
        };
        if let Some(message) = refused {
            audit::record(
                &self.inner.engine,
                user,
                "mesh.reset",
                &genesis.mesh_name,
                &format!("denied: {message}"),
                "",
                &env.signature,
            );
            let _ = out.send(IpcResponse::Error { message }).await;
            return;
        }
        self.audit_ok(&user, "mesh.reset", &genesis.mesh_name, "", &env);
        if let Err(e) = self.inner.engine.author_op(OpBody::MeshReset {
            reset: reset.clone(),
        }) {
            self.inner.resetting.store(false, Ordering::SeqCst);
            let _ = out
                .send(IpcResponse::Error {
                    message: format!("the reset could not be written to this node's log: {e}"),
                })
                .await;
            return;
        }
        warn!(mesh = %genesis.mesh_name, "the founder reset this mesh");
        let peers = self.stop_for_reset().await;
        let _ = out
            .send(IpcResponse::MeshReset(Box::new(
                mesh_proto::MeshResetReport {
                    mesh_name: genesis.mesh_name,
                    peers,
                },
            )))
            .await;
        self.leave_mesh(reset).await;
    }

    /// Act on a reset another member published: the same stop, relay and
    /// leave as [`Self::reset_stream`], found by looking once a second.
    ///
    /// Also the path for a node that applied a reset and then went down
    /// before it had left: it finds the reset in its own log at the next
    /// start and finishes the job.
    fn spawn_reset_watch(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = shutdown.changed() => return,
                    }
                    let Some(entry) = this.inner.engine.read(|s| s.mesh_reset.clone()) else {
                        continue;
                    };
                    // The founder's request on this node has it already.
                    if this.inner.resetting.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    warn!(
                        from = %entry.author.short(),
                        "the founder reset this mesh; leaving it"
                    );
                    let peers = this.stop_for_reset().await;
                    let reached = peers
                        .iter()
                        .filter(|p| p.outcome == mesh_proto::MeshResetOutcome::Delivered)
                        .count();
                    info!(reached, of = peers.len(), "passed the reset on");
                    this.leave_mesh(entry.reset).await;
                    return;
                }
            }
        });
        self.track(handle);
    }

    /// Stop everything this node is doing and pass the reset on to every
    /// member it can reach. Returns what became of each.
    ///
    /// **Every node relays, not just the founder's.** A member the founder
    /// cannot reach — behind a relay, on the far side of a WAN hop — is
    /// very often one some other member can, and each node that hears the
    /// reset tells everyone before it goes. That is also why a node does
    /// not leave the moment it applies the op.
    async fn stop_for_reset(&self) -> Vec<mesh_proto::MeshResetPeer> {
        use futures::StreamExt as _;
        use mesh_proto::MeshResetOutcome;

        // No new work, then nothing running: a job left behind would keep
        // writing into a directory that is about to be moved, for a mesh
        // that no longer exists to read its result.
        self.inner.draining.store(true, Ordering::SeqCst);
        let closed = self
            .inner
            .sessions
            .close_hosted("the mesh this machine was in has been reset by its founder");
        if closed > 0 {
            info!(sessions = closed, "reset closed live sessions");
        }
        for id in self.inner.jobs.active_ids() {
            if let Err(e) = self.inner.jobs.cancel(&id) {
                warn!(job = %id.short(), "reset could not stop a job: {e}");
            }
        }
        let me = self.node_id();
        let peers: Vec<MemberInfo> = self.inner.engine.read(|s| {
            s.members
                .values()
                .filter(|m| s.is_active_member(&m.info.node_id) && m.info.node_id != me)
                .map(|m| m.info.clone())
                .collect()
        });
        futures::stream::iter(peers)
            .map(|peer| async move {
                let outcome = match self.inner.gossip.sync_with(&peer).await {
                    Ok(()) => match self.inner.transport.peer_minor(&peer.node_id) {
                        Some(minor) if minor < mesh_proto::MESH_RESET_MINOR => {
                            MeshResetOutcome::TooOld
                        }
                        _ => MeshResetOutcome::Delivered,
                    },
                    Err(e) => MeshResetOutcome::Unreachable(e.to_string()),
                };
                mesh_proto::MeshResetPeer {
                    node_id: peer.node_id,
                    name: peer.name,
                    outcome,
                }
            })
            .buffer_unordered(RESET_RELAY_WIDTH)
            .collect()
            .await
    }

    /// Give cancelled work a moment to end, then ask the daemon loop to
    /// exit; [`run`] moves the mesh's data aside once the node is down.
    async fn leave_mesh(&self, reset: mesh_proto::MeshReset) {
        let deadline = Instant::now() + RESET_STOP_GRACE;
        while self.inner.jobs.running_count() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let left = self.inner.jobs.running_count();
        if left > 0 {
            warn!(
                jobs = left,
                "leaving the mesh with work that did not stop in time"
            );
        }
        *self.inner.left_mesh.lock() = Some(reset);
        let _ = self.inner.exit_tx.send(true);
    }

    /// Once a restored node's hold lifts, make the announcement its boot
    /// held back (ENG-13): its address may well have changed, and nothing
    /// else would say so until the next restart.
    fn spawn_restore_watch(self) {
        if self.inner.engine.restore_hold().is_none() {
            return;
        }
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = shutdown.changed() => return,
                    }
                    if this.inner.engine.restore_hold().is_none() {
                        this.announce_after_restore();
                        return;
                    }
                }
            }
        });
        self.track(handle);
    }

    fn announce_after_restore(&self) {
        let me = self.node_id();
        let live = self.live_config();
        let Some(recorded) = self
            .inner
            .engine
            .read(|s| s.members.get(&me).map(|m| m.announced_tags.clone()))
        else {
            warn!("caught up after a restore, and this node is not a member of its own state");
            return;
        };
        let mut addrs = advertised_addrs(
            live.listen.ip(),
            self.inner.listen_addr.port(),
            &live.network.advertise_interfaces,
        );
        for a in &live.wan.advertise {
            if !addrs.contains(a) {
                addrs.push(a.clone());
            }
        }
        let tags = capacity::refreshed_tags(
            &live,
            &self.inner.jobs.devices(),
            &self.inner.jobs.ready_runtimes(),
            &recorded,
        );
        if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
            node_id: me,
            name: live.node_name.clone(),
            addrs,
            tags,
            slots: live.limits.slots,
        }) {
            warn!("cannot announce this node after its restore: {e}");
        }
    }

    /// Re-announce this node's `cap.rt` once the probes have answered (T3).
    ///
    /// Deliberately the same shape as [`Self::reannounce_device_tags`]: one
    /// `MemberUpdate`, idempotent, skipped when nothing moved. It rebuilds
    /// the whole `cap.*` set from a fresh `Capacity` rather than patching
    /// one tag, because patching is how a tag set drifts from the struct
    /// that is supposed to describe it.
    fn reannounce_runtime_tags(&self) {
        let me = self.node_id();
        let Some(info) = self
            .inner
            .engine
            .read(|s| s.members.get(&me).map(|m| m.info.clone()))
        else {
            return;
        };
        // The live config (ENG-11): the boot one would re-announce the port
        // range, volumes and terminal switch this node booted with.
        let live = self.live_config();
        let tags = capacity::with_capacity_and_devices(
            &info.tags,
            &capacity::of(&live, &self.inner.jobs.ready_runtimes()),
            live.sessions.accept,
            &self.inner.jobs.devices(),
        );
        if tags == info.tags {
            return;
        }
        if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
            node_id: me,
            name: info.name,
            addrs: info.addrs,
            tags,
            slots: info.slots,
        }) {
            warn!("cannot announce which runtimes this node has: {e}");
        }
    }

    fn reannounce_device_tags(&self, devices: &[mesh_proto::Device]) {
        let me = self.node_id();
        let Some(info) = self
            .inner
            .engine
            .read(|s| s.members.get(&me).map(|m| m.info.clone()))
        else {
            return;
        };
        let live = self.live_config();
        let tags = capacity::with_capacity_and_devices(
            &info.tags,
            &capacity::of(&live, &self.inner.jobs.ready_runtimes()),
            live.sessions.accept,
            devices,
        );
        if tags == info.tags {
            return;
        }
        if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
            node_id: me,
            name: info.name,
            addrs: info.addrs,
            tags,
            slots: info.slots,
        }) {
            warn!("cannot re-announce device tags: {e}");
        }
    }

    /// Publish accumulated usage counters on a timer (C1 P1/P2).
    ///
    /// Separate from the compaction timer even though both are periodic
    /// housekeeping: this one runs every thirty seconds because a share
    /// must be visible within a gossip round, and that one runs hourly
    /// because a horizon is measured in days. Folding them together would
    /// mean choosing one of those two numbers wrongly.
    fn spawn_usage_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(crate::usage::FLUSH_INTERVAL) => {}
                        _ = shutdown.changed() => {
                            // A clean shutdown publishes what it has, so a
                            // planned restart costs nobody a share.
                            this.inner.jobs.meter().flush();
                            return;
                        }
                    }
                    let meter = this.inner.jobs.meter();
                    meter.expire(mesh_proto::now_micros());
                    let published = meter.flush();
                    if published > 0 {
                        tracing::debug!(windows = published, "published usage counters");
                    }
                    // Reclamation reads the shares this flush just made
                    // current, so it rides the same tick rather than a
                    // timer of its own that would sometimes act on
                    // yesterday's picture.
                    this.reclaim_pass().await;
                }
            }
        });
        self.track(handle);
    }

    /// Reclaim borrowed capacity from a principal who is over their share
    /// for one who is under it (C1 P5).
    ///
    /// Runs on this node, over this node's own running jobs, from
    /// replicated shares — no node asks another node anything, exactly as
    /// the fair-share ordering does not.
    ///
    /// **Only preemptible work is ever touched**, and it takes one job per
    /// pass. Reclaiming a whole node's worth at once would turn a share
    /// that drifted by a percent into a stampede, and the pass runs often
    /// enough that one at a time converges.
    async fn reclaim_pass(&self) {
        let jobs = self.inner.jobs.clone();
        let candidates = jobs.preemptible_running();
        if candidates.is_empty() {
            return;
        }
        let waiting = jobs.waiting_principals();
        if waiting.is_empty() {
            return; // nobody is being kept out; nothing to reclaim for
        }
        let shares = jobs.shares();
        let share_of = |who: &UserId| shares.get(who).copied().unwrap_or(0);

        // The principal furthest ahead who has preemptible work here, and
        // the principal furthest behind who is waiting for a slot here.
        let Some((victim_job, over)) = candidates
            .iter()
            .max_by_key(|(_, who)| share_of(who))
            .copied()
        else {
            return;
        };
        let Some(under) = waiting.keys().min_by_key(|who| share_of(who)).copied() else {
            return;
        };
        if over == under || share_of(&over) <= share_of(&under) {
            // Nobody is ahead of anybody. Preempting here would be churn
            // dressed up as fairness.
            return;
        }

        // Requeue, because finishing elsewhere is kinder than losing the
        // work, and `--preemptible` volunteered it for a move, not for a
        // deletion.
        let mode = mesh_proto::PreemptMode::Requeue;

        let detail = format!(
            "share {}‰ over {}‰; reclaimed for {}",
            share_of(&over),
            share_of(&under),
            self.inner
                .engine
                .read(|s| s.username(&under).map(str::to_string))
                .unwrap_or_else(|| under.to_display_string())
        );
        audit::record(
            &self.inner.engine,
            over,
            "preemption",
            &victim_job.to_display_string(),
            mode.as_str(),
            &detail,
            &[],
        );
        info!(job = %victim_job.short(), mode = mode.as_str(), detail, "preempting");

        self.preempt_job(victim_job, mode);
    }

    /// Stop one preemptible job: cancel it, or cancel it and hand its spec to
    /// another node once it has stopped (C1 P5). Shared by share reclamation and, since Phase C E10, a
    /// deployment of higher priority.
    fn preempt_job(&self, victim_job: JobId, mode: mesh_proto::PreemptMode) {
        let jobs = self.inner.jobs.clone();
        match mode {
            mesh_proto::PreemptMode::Off => {}
            mesh_proto::PreemptMode::Cancel => {
                if let Err(e) = jobs.cancel(&victim_job) {
                    warn!(job = %victim_job.short(), "preemption cancel failed: {e}");
                }
            }
            mesh_proto::PreemptMode::Requeue => {
                let Some(spec) = jobs.spec_of(&victim_job) else {
                    warn!(job = %victim_job.short(), "preemption: no spec to requeue");
                    return;
                };
                if let Err(e) = jobs.cancel(&victim_job) {
                    warn!(job = %victim_job.short(), "preemption cancel failed: {e}");
                    return;
                }
                let this = self.clone();
                tokio::spawn(async move {
                    // Wait for the local job to actually finish before
                    // handing it on. The two records share a job id and
                    // merge LWW by HLC, so a `Cancelled` published *after*
                    // the new executor's `Running` would win — and the job
                    // would show as cancelled while it ran.
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while this.inner.jobs.is_running(&victim_job) && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    this.inner.jobs.forget(&victim_job);
                    match this.redispatch(spec).await {
                        MoveOutcome::Moved(target) => {
                            info!(job = %victim_job.short(), to = %target,
                                "preempted job requeued elsewhere");
                        }
                        MoveOutcome::Ambiguous(target) => {
                            warn!(job = %victim_job.short(), target = %target,
                                "preempted job's new home is ambiguous; it is theirs");
                        }
                        MoveOutcome::NobodyTook(reason) => {
                            warn!(job = %victim_job.short(),
                                "nobody could take the preempted job ({reason}); it stays cancelled");
                        }
                    }
                });
            }
        }
    }

    fn spawn_rewrap_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(REWRAP_INTERVAL) => {
                            // Off the async workers (ENG-26's rule, applied
                            // to ENG-27's remainder). The pass is
                            // synchronous asymmetric crypto: an X25519 and
                            // an AEAD open per secret this node can read,
                            // and another per target it heals. A joiner
                            // arriving on a mesh with two hundred secrets
                            // makes every one of its three healers do all
                            // of that in one tick, on a runtime thread.
                            let pass = this.clone();
                            let _ =
                                tokio::task::spawn_blocking(move || pass.rewrap_secrets()).await;
                            // Folded audit rows whose window has closed get
                            // their summary here even if no further denial
                            // arrives to trigger it (SEC-6).
                            audit::flush_folded(&this.inner.engine, false);
                        }
                        _ = shutdown.changed() => {
                            audit::flush_folded(&this.inner.engine, true);
                            return;
                        }
                    }
                }
            }
        });
        self.track(handle);
    }

    /// Land relaxed op-log writes, and publish what finished jobs produced
    /// (ENG-26).
    ///
    /// **Its own task, because it used to be a passenger on the history
    /// indexer** — which starts with `let Some(history) = … else { return }`.
    /// So a node with `[history] enabled = false`, or one whose
    /// `history.redb` would not open, silently ran neither: peer ops
    /// written the relaxed way had no bound on how long they could sit
    /// unflushed, and nothing a finished job produced was ever published
    /// to the action cache. Two guarantees, switched off by a setting
    /// about a *query index*, with nothing saying so.
    ///
    /// They belong together and apart from the index: both are one commit
    /// and an fsync, off the path a person is waiting on, and neither has
    /// anything to do with being able to answer `meshnode history`.
    fn spawn_durability_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(HISTORY_INDEX_EVERY) => {
                            // Land any peer ops written without waiting for
                            // the disk (**H-3**). This is the bound on how
                            // long a relaxed write can sit unflushed. An
                            // empty commit when there is nothing
                            // outstanding. A store that has already failed
                            // is left to `recover_store`, which says so
                            // once rather than every tick.
                            if this.inner.engine.store_failure().is_none()
                                && let Err(e) = this.inner.engine.flush()
                            {
                                warn!("op-log flush failed: {e}");
                            }
                            // C7 P2: what a finished job produced becomes a
                            // cache entry other nodes can serve.
                            this.publish_finished_actions().await;
                        }
                        _ = shutdown.changed() => return,
                    }
                }
            }
        });
        self.track(handle);
    }

    /// Keep `state/history.redb` in step with replicated state (M14 §2).
    ///
    /// Change-driven, not a rescan: the state engine records which runs and
    /// jobs an applied op touched, and this drains that set. The cost of a
    /// tick is therefore proportional to *churn* — an idle mesh costs one
    /// mutex lock every two seconds — rather than to how much history the
    /// node has accumulated, which is the whole point of having an index.
    fn spawn_history_task(self) {
        let Some(history) = self.inner.history.clone() else {
            return;
        };
        let keep_days = self.inner.config.history.keep_days;
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                // A restart has to catch up on everything that happened
                // while this node was down, and on anything the last run
                // failed to write. One full pass, then incremental forever.
                //
                // Fact history rebuilds from the **op-log**, not from
                // current state (M18 §4.8): every observation is still in
                // there, so deleting the index and starting again gives
                // back a complete history rather than a truncated one.
                let mut broken = !this.index_everything(&history, false).await;
                let mut last_prune = Instant::now();
                let mut last_reopen = Instant::now();
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(HISTORY_INDEX_EVERY) => {
                            // An index that failed a write refuses every
                            // later one until it is reopened (ENG-4). Its
                            // changes wait in the engine meanwhile, and a
                            // reopened index is rebuilt in full, so nothing
                            // it missed stays missing.
                            if broken {
                                if last_reopen.elapsed() < STORE_RETRY_EVERY
                                    || !this.room_to_reopen()
                                {
                                    continue;
                                }
                                last_reopen = Instant::now();
                                let reopened = {
                                    let history = history.clone();
                                    tokio::task::spawn_blocking(move || history.reopen()).await
                                };
                                match reopened {
                                    Ok(Ok(())) => {
                                        drop(this.inner.engine.drain_dirty());
                                        if this.index_everything(&history, true).await {
                                            broken = false;
                                            info!("the history index takes writes again");
                                        }
                                    }
                                    Ok(Err(e)) => debug!("the history index cannot be reopened yet: {e}"),
                                    Err(e) => warn!("history index reopen task failed: {e}"),
                                }
                                continue;
                            }
                            let mut dirty = this.inner.engine.drain_dirty();
                            // Audit rows first, taken out of the change set
                            // so the run/job pass below sees only its own.
                            let audit = std::mem::take(&mut dirty.audit);
                            let mut written = if std::mem::take(&mut dirty.audit_overflowed) {
                                this.index_audit(&history, None).await
                            } else if !audit.is_empty() {
                                this.index_audit(&history, Some(audit)).await
                            } else {
                                true
                            };
                            if dirty.overflowed {
                                written &= this.index_history(&history, None).await;
                            } else if !dirty.is_empty() {
                                written &= this.index_history(&history, Some(dirty)).await;
                            }
                            if !written {
                                broken = true;
                                last_reopen = Instant::now();
                                continue;
                            }
                            // C8 P1 and C7 P2 ride this timer for the same
                            // reason the index does: both are a redb commit
                            // and an fsync, and neither belongs between a
                            // user pressing enter and their job starting.
                            this.flush_explains(&history).await;
                            if last_prune.elapsed() >= Duration::from_secs(3600) {
                                last_prune = Instant::now();
                                // M31 A1: the window is the **replicated**
                                // policy, and `[history] keep_days` is now
                                // this node's "keep less" preference rather
                                // than an independent horizon (Q8, decided).
                                // Before this the index had its own node-
                                // local cutoff defaulting to forever, which
                                // is exactly the per-node horizon C9 names
                                // as a mistake: two nodes would answer the
                                // same "show me last quarter" differently
                                // because of a config file.
                                let days = this
                                    .inner
                                    .engine
                                    .retention()
                                    .window(mesh_proto::RetentionTable::Runs)
                                    .resolve_for_node((keep_days > 0).then_some(keep_days));
                                if days > 0 {
                                    let cutoff = mesh_proto::now_micros().saturating_sub(
                                        u64::from(days) * 86_400 * 1_000_000,
                                    );
                                    match history.prune_before(cutoff) {
                                        Ok(0) => {}
                                        Ok(jobs) => {
                                            info!(
                                                jobs,
                                                days,
                                                "pruned history past the mesh retention window"
                                            );
                                        }
                                        Err(e) => warn!("history prune failed: {e}"),
                                    }
                                }
                                // The audit window is the mesh's, with its
                                // floor; `forever` keeps every row (ENG-3).
                                if let Some(cutoff) = this
                                    .inner
                                    .engine
                                    .retention()
                                    .window(mesh_proto::RetentionTable::Audit)
                                    .cutoff_micros(mesh_proto::now_micros())
                                {
                                    match history.prune_audit_before(cutoff) {
                                        Ok(0) => {}
                                        Ok(rows) => info!(
                                            rows,
                                            "pruned audit history past the mesh audit window"
                                        ),
                                        Err(e) => warn!("audit history prune failed: {e}"),
                                    }
                                }
                            }
                        }
                        _ = shutdown.changed() => return,
                    }
                }
            }
        });
        self.track(handle);
    }

    /// Write audit rows into the history index (ENG-3): the ones named in
    /// `marks`, or every row in the op-log when `None`.
    ///
    /// False if the index refused the write.
    async fn index_audit(
        &self,
        history: &Arc<crate::history::HistoryStore>,
        marks: Option<Vec<(NodeId, u64)>>,
    ) -> bool {
        let engine = self.inner.engine.clone();
        let history = history.clone();
        let written = tokio::task::spawn_blocking(move || {
            let ops = match marks {
                None => engine.audit_ops()?,
                Some(marks) => marks
                    .iter()
                    .filter_map(|(author, seq)| engine.op_at(author, *seq).ok().flatten())
                    .collect(),
            };
            history.index_audit(&ops)
        })
        .await;
        match written {
            Ok(Ok(n)) if n > 1000 => info!(rows = n, "indexed audit history"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!("audit history index write failed: {e}");
                return false;
            }
            Err(e) => warn!("audit history indexer panicked: {e}"),
        }
        true
    }

    /// A full pass over everything the index holds: at boot, and after the
    /// index was reopened (ENG-4). False if any of it could not be written.
    ///
    /// Audit rows only when asked. At boot, replaying the op-log has
    /// already marked every one, and reading them all back is a load of
    /// the whole log; after a reopen, the marks of the writes that failed
    /// are gone and nothing else would bring them back.
    async fn index_everything(
        &self,
        history: &Arc<crate::history::HistoryStore>,
        with_audit: bool,
    ) -> bool {
        let rows = self.index_history(history, None).await;
        let audit = !with_audit || self.index_audit(history, None).await;
        let facts = self.index_fact_history(history).await;
        rows && audit && facts
    }

    /// Whether the data directory has room enough to reopen a store whose
    /// last write failed; unmeasurable counts as room (ENG-4).
    fn room_to_reopen(&self) -> bool {
        crate::capacity::disk_free_mb(&self.inner.config.data_dir)
            .is_none_or(|free_mb| free_mb >= STORE_REOPEN_FREE_MB)
    }

    /// Audit rows older than everything the in-memory tail holds (ENG-3),
    /// newest first, at most `n`.
    ///
    /// From the history index; from a walk of the op-log where this node
    /// runs without one. Nothing at all while the tail has never been
    /// trimmed, because then it holds every row.
    fn older_audit(
        &self,
        filter: &crate::audit::AuditFilter,
        n: usize,
    ) -> Vec<mesh_proto::AuditEntry> {
        let Some(oldest) = self.inner.engine.audit_tail_start() else {
            return Vec::new();
        };
        if let Some(history) = &self.inner.history {
            return history
                .audit_page(filter, Some(oldest), n)
                .unwrap_or_else(|e| {
                    warn!("audit history read failed: {e}");
                    Vec::new()
                });
        }
        let mut older: Vec<mesh_proto::AuditEntry> = match self.inner.engine.audit_ops() {
            Ok(ops) => ops
                .iter()
                .filter_map(|op| match op.decode_body() {
                    Some(OpBody::AuditAppend { entry }) => Some(entry),
                    _ => None,
                })
                .filter(|e| e.audit_id < oldest && crate::audit::matches(filter, e))
                .collect(),
            Err(e) => {
                warn!("cannot read audit rows from the op-log: {e}");
                return Vec::new();
            }
        };
        older.sort_by_key(|e| std::cmp::Reverse(e.audit_id));
        older.truncate(n);
        older
    }

    /// Rebuild the fact history table from this node's op-log (M18 §4.8).
    ///
    /// One walk at boot. Affordable precisely because §4.2's rule holds: a
    /// stable machine authors one `FactsPublish` per collector, ever, so a
    /// year-old mesh of a thousand nodes has thousands of these ops, not
    /// millions.
    ///
    /// False if the index refused the write.
    async fn index_fact_history(&self, history: &Arc<crate::history::HistoryStore>) -> bool {
        let observations = match self.inner.engine.fact_observations() {
            Ok(o) if !o.is_empty() => o,
            Ok(_) => return true,
            Err(e) => {
                warn!("cannot read fact history from the op-log: {e}");
                return true;
            }
        };
        let history = history.clone();
        let written = tokio::task::spawn_blocking(move || {
            // One transaction for the whole history, not one per
            // observation (**H-4**).
            let rows: Vec<(&str, &str, u64, &str)> = observations
                .iter()
                .map(|(subject, collector, when, doc)| {
                    (subject.as_str(), collector.as_str(), *when, doc.as_str())
                })
                .collect();
            history.index_facts(&rows)
        })
        .await;
        match written {
            Ok(Ok(n)) if n > 0 => info!(observations = n, "indexed fact history"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!("fact history index write failed: {e}");
                return false;
            }
            Err(e) => warn!("fact history indexer panicked: {e}"),
        }
        true
    }

    /// Project state into index rows and write them.
    ///
    /// `dirty = None` means a full pass. The state read-lock is held only
    /// while copying scalars out of the maps; the redb commit — the part
    /// that fsyncs — happens on the blocking pool with no lock held at all.
    async fn index_history(
        &self,
        history: &Arc<crate::history::HistoryStore>,
        dirty: Option<crate::state::Dirty>,
    ) -> bool {
        let jobs = self.inner.engine.read(|state| match &dirty {
            Some(d) => d
                .jobs
                .iter()
                .filter_map(|id| state.jobs.get(id))
                .map(crate::history::project_job)
                .collect::<Vec<_>>(),
            None => state
                .jobs
                .values()
                .map(crate::history::project_job)
                .collect(),
        });
        // Facts, keyed by *when they were observed* — so a document that
        // arrives late still lands in the right place in the history.
        let facts: Vec<(String, String, u64, String)> = self.inner.engine.read(|state| {
            let mut out = Vec::new();
            let mut push =
                |subject: &mesh_proto::FactSubject, name: &str, entry: &crate::state::FactEntry| {
                    out.push((
                        subject.label(),
                        name.to_string(),
                        entry.collected_at_micros,
                        entry.doc.clone(),
                    ));
                };
            match &dirty {
                Some(d) => {
                    for (subject, collector) in &d.facts {
                        for (s, by) in &state.facts {
                            if &s.label() != subject {
                                continue;
                            }
                            if let Some(entry) = by.get(collector) {
                                push(s, collector, entry);
                            }
                        }
                    }
                }
                None => {
                    for (subject, by) in &state.facts {
                        for (collector, entry) in by {
                            push(subject, collector, entry);
                        }
                    }
                }
            }
            out
        });

        // Usage windows (C1 P1). Not change-driven like runs and jobs: a
        // counter has no id to mark dirty, the live set is bounded by
        // `principals × nodes × live windows`, and the upsert skips rows
        // that have not moved. Walking it every pass is cheaper than
        // tracking it, and it is what makes a report outlive the seven-day
        // decay horizon the op-log keeps.
        let usage: Vec<mesh_proto::UsageRow> = self.inner.engine.read(|state| {
            state
                .usage
                .iter()
                .map(|((node, _, _), partial)| mesh_proto::UsageRow::of(*node, partial))
                .collect()
        });

        if jobs.is_empty() && facts.is_empty() && usage.is_empty() {
            return true;
        }

        let history = history.clone();
        let written = tokio::task::spawn_blocking(move || {
            let j = history.index_jobs(&jobs)?;
            history.index_usage(&usage)?;
            // As above (**H-4**): one commit for the tick's facts.
            let fact_rows: Vec<(&str, &str, u64, &str)> = facts
                .iter()
                .map(|(subject, collector, when, doc)| {
                    (subject.as_str(), collector.as_str(), *when, doc.as_str())
                })
                .collect();
            history.index_facts(&fact_rows)?;
            Ok::<_, crate::error::CoreError>(j)
        })
        .await;

        match written {
            Ok(Ok(j)) if j > 0 => debug!(jobs = j, "indexed history"),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!("history index write failed: {e}");
                if let Some(d) = dirty {
                    self.inner.engine.requeue_dirty(d);
                }
                return false;
            }
            Err(e) => warn!("history indexer panicked: {e}"),
        }
        true
    }

    /// Hourly retention sweep (MASTERPLAN §9.1) + a fast promotion tick so
    /// memory-gated queued work starts as soon as pressure clears (§11).
    fn spawn_maintenance_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        // The scheduler's tick: it promotes queued work, so a node whose
        // tick stopped runs nothing new however free it is (ENG-1).
        let pulse = self
            .inner
            .vitals
            .register("scheduler", Some(SCHEDULER_STALL_AFTER));
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                // At boot as well as hourly: a node that has been off for a
                // month comes back holding a month of expired history, and
                // waiting an hour to agree with its peers is an hour of
                // answering questions differently.
                this.sweep_off_thread(Sweep::Retention, "boot").await;
                this.inner.engine.expire_closed(mesh_proto::now_micros());
                this.sweep_off_thread(Sweep::Content, "boot").await;
                let mut last_sweep = Instant::now();
                let mut last_objects = Instant::now();
                // Seeded so the first check happens on the first tick
                // rather than a minute in: a node that boots with a full
                // disk should say so immediately.
                let mut last_disk = Instant::now()
                    .checked_sub(DISK_CHECK_EVERY)
                    .unwrap_or_else(Instant::now);
                // Whether the last check was already unhappy, so a node
                // sitting below the line logs once rather than every
                // minute for a week (`crate::quiet`'s rule, applied here).
                let mut disk_warned = false;
                // The clear this node has already applied to its own disk.
                // Seeded from state rather than from zero, so a restart
                // does not re-walk the jobs directory for a clear that has
                // been settled since last week.
                let mut cleared_to = this.inner.engine.read(|s| s.jobs_cleared_before);
                // Last effective slot count, so a quiet window opening or
                // closing gets exactly one journal line. Without it the
                // only evidence of a cap is jobs quietly not starting,
                // which is indistinguishable from a scheduler bug at three
                // in the morning (`crate::quiet`).
                let mut last_effective = this.inner.jobs.effective_slots();
                let mut last_store_try = Instant::now();
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {
                            pulse.beat();
                            // A store that failed a write is reopened until
                            // it takes writes again (ENG-4).
                            if this.inner.engine.store_failure().is_some()
                                && last_store_try.elapsed() >= STORE_RETRY_EVERY
                            {
                                last_store_try = Instant::now();
                                // A failed write is most often a full disk:
                                // look now, not at the next minute's check.
                                last_disk = Instant::now();
                                this.check_disk_pressure(&mut disk_warned);
                                this.recover_store().await;
                            }
                            // The 2-second tick is also what makes a quiet
                            // window's *end* prompt: nothing else would
                            // notice that work may start again.
                            this.inner.jobs.try_promote();
                            let effective = this.inner.jobs.effective_slots();
                            if effective != last_effective {
                                let configured = this.inner.jobs.limits().slots;
                                match this.inner.jobs.quiet_state() {
                                    Some(state) => info!(
                                        slots = effective,
                                        configured,
                                        until = %crate::localtime::hhmm(state.until),
                                        "quiet hours: {} — {effective} of {configured} slots",
                                        state.window
                                    ),
                                    None => info!(
                                        slots = effective,
                                        "quiet hours over — all {effective} slots available"
                                    ),
                                }
                                last_effective = effective;
                            }
                            // A force-cancel that reached this node for work
                            // it is actually running: the override was for
                            // the silence, not the process, so the process
                            // goes too. The local cancel publishes a
                            // terminal report with a later HLC, which is
                            // the record that then wins everywhere.
                            // (Two steps, not a filter inside the read:
                            // `is_active` takes the scheduler lock, and the
                            // scheduler holds its lock while publishing
                            // into state — nesting them here would be the
                            // reverse order.)
                            //
                            // **Iterated from the small side** (**M-5**).
                            // This walked every force-cancel the mesh has
                            // ever recorded — a map that is deliberately
                            // never pruned, see `jobs_force_cancelled` —
                            // and took the scheduler lock once per entry to
                            // ask `is_active`. Both terms grew forever, on
                            // a two-second tick, to act on at most a
                            // handful of jobs.
                            //
                            // The jobs this node is actually running are
                            // bounded by its slots and queue, so crossing
                            // that set against the overrides costs one
                            // scheduler lock and one hash lookup per live
                            // job instead.
                            let active = this.inner.jobs.active_ids();
                            if !active.is_empty() {
                                let forced: Vec<mesh_proto::JobId> = this.inner.engine.read(|s| {
                                    active
                                        .iter()
                                        .filter(|id| s.jobs_force_cancelled.contains_key(*id))
                                        .copied()
                                        .collect()
                                });
                                for job_id in forced {
                                    info!(job = %job_id.short(), "force-cancel arrived for running work; stopping it");
                                    let _ = this.inner.jobs.cancel(&job_id);
                                }
                            }
                            // A clear authored *anywhere* takes this node's
                            // spec and log files with it. The op is the only
                            // signal — there is no message to a peer asking
                            // it to delete, and there does not need to be:
                            // every node reaches the same watermark and
                            // decides alone, the way `sweep_objects` does.
                            let mark = this.inner.engine.read(|s| s.jobs_cleared_before);
                            if mark > cleared_to {
                                cleared_to = mark;
                                this.inner.jobs.clear_finished(mark);
                            }
                            if last_sweep.elapsed() >= Duration::from_secs(3600) {
                                last_sweep = Instant::now();
                                // Two sweeps, one tick, two tiers. The
                                // first bounds this machine's disk; the
                                // second bounds the replicated projection
                                // in memory (M31 A1), and the second is
                                // what stops a long-uptime node answering
                                // differently from one that restarted an
                                // hour ago.
                                let retention = this.resolved_retention();
                                this.inner.jobs.sweep_retention(retention);
                                let released =
                                    this.inner.engine.expire_closed(mesh_proto::now_micros());
                                if released > 0 {
                                    info!(
                                        jobs = released,
                                        "released closed jobs past the mesh retention window"
                                    );
                                }
                                // Three tiers now. The content store is the
                                // one that grows without an upper bound —
                                // every release, every source sync, every
                                // bundle anyone ever deployed here — and
                                // until A2 nothing in the product ever
                                // deleted a byte of it.
                                this.sweep_off_thread(Sweep::Content, "retention").await;
                            }
                            // Disk pressure is checked far more often than
                            // retention, because the two failures are not
                            // the same shape: history growing past a window
                            // is a slow drift, and a disk filling is an
                            // afternoon. An hourly check would notice a
                            // full volume up to an hour after the node
                            // stopped being able to do anything.
                            let disk_every = if disk_warned {
                                DISK_RECHECK_EVERY
                            } else {
                                DISK_CHECK_EVERY
                            };
                            if last_disk.elapsed() >= disk_every {
                                last_disk = Instant::now();
                                this.check_disk_pressure(&mut disk_warned);
                            }
                            // Ephemeral objects are reclaimed far more often
                            // than job history, because they are the thing
                            // actually occupying memory (M9 §5.5).
                            if last_objects.elapsed() >= OBJECT_SWEEP_EVERY {
                                last_objects = Instant::now();
                                this.sweep_off_thread(Sweep::Objects, "timer").await;
                            }
                        }
                        _ = shutdown.changed() => return,
                    }
                }
            }
        });
        self.track(handle);
    }

    /// Reopen the op-log after a failed write (ENG-4), off the async
    /// workers. Still full: say so and try again later. Cannot be opened at
    /// all: the node cannot work, so its store pulse goes, and the daemon
    /// stops with a failure for the service manager to restart.
    async fn recover_store(&self) {
        // Not onto a disk that is still full: redb writes as it reopens a
        // store whose last commit failed, and a reopen that fails leaves
        // nothing to read from either.
        if !self.room_to_reopen() {
            debug!("the op-log stays closed to writes until there is room");
            return;
        }
        let engine = self.inner.engine.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let result = engine.recover_store();
            (result, engine.store_is_open())
        })
        .await;
        match outcome {
            Ok((Ok(()), _)) => info!("the op-log takes writes again"),
            Ok((Err(e), true)) => {
                debug!("the op-log still cannot be written: {e}");
            }
            Ok((Err(e), false)) => {
                error!("the op-log could not be reopened: {e}");
                drop(self.inner.store_pulse.lock().take());
            }
            Err(e) => warn!("op-log recovery task failed: {e}"),
        }
    }

    /// Reclaim objects and directory entries whose owning job is over, and
    /// drop dead nodes from the entries this node indexes (M9 §5.5).
    ///
    /// The refcount is the owning job, and the job's terminal state is
    /// already replicated — so a directory node can decide this alone, from
    /// state it has, with no release message from anyone. That is why there
    /// is no `ObjectRelease` on the wire: adding one would be inventing
    /// coordination the CRDT already does.
    fn sweep_objects(&self) {
        let job_state = |job: &JobId| {
            self.inner
                .engine
                .read(|s| s.jobs.get(job).map(|j| j.state.is_terminal()))
        };
        let released = self.inner.objects.sweep(job_state);
        let dropped = self.inner.objdir.sweep(job_state);
        for node in self.inner.engine.read(|s| {
            s.members
                .keys()
                .copied()
                .filter(|id| matches!(self.inner.gossip.liveness_of(id), Liveness::Dead))
                .collect::<Vec<_>>()
        }) {
            // A dead holder in an entry is worse than a missing entry: it
            // sends every fetcher on a dial that cannot succeed.
            self.inner.objdir.forget_holder(&node);
        }
        // A cache entry names one producer and no alternative — an object
        // is k=1 and so is the node holding it — so an entry whose producer
        // is gone is a guaranteed failed materialisation. That costs a
        // fetch attempt *before* it costs the execution it was going to
        // cost anyway, which makes it strictly worse than having no entry.
        let expired = self.inner.actions.forget_producer(|node| {
            self.inner.engine.read(|s| s.is_active_member(node))
                && !matches!(self.inner.gossip.liveness_of(node), Liveness::Dead)
        });
        if released > 0 || dropped > 0 || expired > 0 {
            debug!(released, dropped, expired, "swept ephemeral objects");
        }
    }

    // -- fan-out adoption (MASTERPLAN §10.3) -------------------------------

    /// Periodically scan replicated fan-out plans for groups whose owner is
    /// gone. The first live node in the group's rendezvous successor list
    /// claims ownership and drives the remaining dispatches; a node also
    /// resumes its *own* unfinished fan-outs after a restart. Executors
    /// never stop running work during adoption — worst case is delayed
    /// status, never duplicate execution (idempotent job ids).
    fn spawn_adoption_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(ADOPTION_SCAN_INTERVAL) => {
                            this.scan_adoptable();
                            this.scan_adoptable_gangs();
                        }
                        _ = shutdown.changed() => return,
                    }
                }
            }
        });
        self.track(handle);
    }

    /// The oldest instant a fan-out or gang may have started and still be
    /// driven (ENG-3): the start of the runs window. A plan older than that
    /// is history, and resuming or adopting it would re-run work somebody
    /// asked for months ago. `0` when the window is forever.
    fn drivable_since(&self) -> u64 {
        self.inner
            .engine
            .retention()
            .window(mesh_proto::RetentionTable::Runs)
            .cutoff_micros(mesh_proto::now_micros())
            .unwrap_or(0)
    }

    fn scan_adoptable(&self) {
        let me = self.node_id();
        let since = self.drivable_since();
        let open: Vec<(FanoutPlanRecord, NodeId)> = self.inner.engine.read(|s| {
            s.fanouts
                .values()
                .filter(|e| e.closed.is_none() && e.plan.created_at_micros >= since)
                .map(|e| (e.plan.clone(), e.owner()))
                .collect()
        });
        for (plan, owner) in open {
            let group = plan.group;
            if self.inner.active_drives.lock().contains(&group) {
                continue; // already driving it (as origin or adopter)
            }
            if owner == me {
                // Our own plan with no live driver: we restarted mid-fan-out.
                info!(group = %group.short(), "resuming own fan-out after restart");
                self.clone().spawn_drive(plan);
                continue;
            }
            let owner_gone = self.inner.engine.read(|s| !s.is_active_member(&owner))
                || matches!(self.inner.gossip.liveness_of(&owner), Liveness::Dead);
            if !owner_gone {
                continue;
            }
            // Successor election: rendezvous hashing of the group id over
            // live members, deterministic on every honest node (§10.3).
            let live: Vec<NodeId> = self.inner.engine.read(|s| {
                s.members
                    .values()
                    .filter(|m| s.is_active_member(&m.info.node_id))
                    .map(|m| m.info.node_id)
                    .filter(|id| {
                        *id != owner && matches!(self.inner.gossip.liveness_of(id), Liveness::Alive)
                    })
                    .collect()
            });
            let first = scheduler::rendezvous_rank(SUCCESSOR_CONTEXT, &group.0, live)
                .into_iter()
                .next();
            if first != Some(me) {
                continue;
            }
            info!(group = %group.short(), dead_owner = %owner.short(),
                "adopting orphaned fan-out");
            if let Err(e) = self
                .inner
                .engine
                .author_op(OpBody::FanoutAdopt { group, by: me })
            {
                warn!(group = %group.short(), "cannot record adoption: {e}");
                continue;
            }
            self.clone().spawn_drive(plan);
        }
    }

    fn spawn_drive(self, plan: FanoutPlanRecord) {
        let group = plan.group;
        if !self.inner.active_drives.lock().insert(group) {
            return;
        }
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                this.drive_fanout(plan).await;
                this.inner.active_drives.lock().remove(&group);
            }
        });
        self.track(handle);
    }

    /// Headless fan-out driver: dispatch the plan's remaining targets from
    /// replicated evidence alone (no client stream), honoring max-parallel
    /// and halt-on-fail, and close the group when every target is accounted
    /// for. Progress is read from replicated job summaries, so it composes
    /// with whatever the dead origin already dispatched.
    #[tracing::instrument(
        level = "info",
        name = "fanout",
        skip_all,
        fields(group = %plan.group.short())
    )]
    async fn drive_fanout(&self, plan: FanoutPlanRecord) {
        let me = self.node_id();
        let group = plan.group;
        // Adoption counts as a run this node owns (ENG-21), because it is
        // one: from here the closing op, the results and the failure of
        // the whole thing are this node's. Two nodes owning one group in
        // turn shows as two runs, on two machines, which is what happened.
        self.inner.groups.started(crate::observe::GroupKind::Fanout);
        let began = Instant::now();
        let total = u32::try_from(plan.targets.len()).unwrap_or(u32::MAX);
        let window = plan
            .controls
            .max_parallel
            .map(|n| (n.max(1)) as usize)
            .unwrap_or(plan.targets.len());
        let halt_at = plan.controls.halt_on_fail.map(|p| p.threshold(total));
        // `--race N` (M9 §5.6, generalized). Read off the replicated plan
        // environment rather than the controls struct, so a successor that
        // adopts this group after the origin dies races exactly as the
        // origin was going to — see `mesh_proto::REQ_RACE` for why the
        // policy does not live in the op body.
        let racing = mesh_proto::race_of(&plan.env).is_some();
        let mut attempts: HashMap<usize, u32> = HashMap::new();
        // When each target first failed to take its job (ENG-6).
        let mut failing_since: HashMap<usize, Instant> = HashMap::new();
        // Targets this driver has given up on: unreachable executors and
        // targets never dispatched because halt-on-fail tripped.
        let mut skipped: HashSet<usize> = HashSet::new();

        // Woken by job state changes rather than a 1 s clock (**M-13**).
        let mut jobs_changed = self.inner.engine.watch_topic(mesh_proto::StateTopic::Jobs);
        loop {
            if *self.inner.shutdown_tx.borrow() {
                return; // node is stopping; a successor (or we, after restart) resumes
            }
            if self
                .inner
                .engine
                .read(|s| s.fanouts.get(&group).is_some_and(|e| e.closed.is_some()))
            {
                return; // someone else closed it (racing adopter)
            }

            // Classify every target from replicated summaries.
            let mut succeeded = 0u32;
            let mut failed = 0u32;
            let mut in_flight = 0usize;
            let mut undispatched: Vec<usize> = Vec::new();
            self.inner.engine.read(|s| {
                for (i, t) in plan.targets.iter().enumerate() {
                    match s.jobs.get(&t.job_id) {
                        Some(j) if j.state == JobState::Succeeded => succeeded += 1,
                        Some(j) if j.state.is_terminal() => failed += 1,
                        Some(_) => {
                            // Dispatched but not finished. An executor that
                            // died mid-job will never report a terminal
                            // state — count it failed instead of waiting
                            // forever (its boot-time sweep also fails it if
                            // it comes back).
                            let executor_gone = !s.is_active_member(&t.node_id)
                                || matches!(
                                    self.inner.gossip.liveness_of(&t.node_id),
                                    Liveness::Dead
                                );
                            if executor_gone {
                                failed += 1;
                            } else {
                                in_flight += 1;
                            }
                        }
                        None if skipped.contains(&i) => {}
                        None => undispatched.push(i),
                    }
                }
            });
            let halted = halt_at.is_some_and(|h| failed >= h);
            if halted {
                skipped.extend(undispatched.drain(..));
            }

            // A race is over the moment anybody wins. Everything not yet
            // dispatched is abandoned, and everything still running is
            // cancelled — the answer is already in hand and the remaining
            // copies are now pure waste of somebody else's cores.
            //
            // Cancellation is best-effort *by design*: a copy whose
            // executor has gone unreachable cannot be told to stop, and
            // waiting for it would forfeit the entire latency win the
            // feature exists for. The loser's own result is simply never
            // read — `stream_race` takes the first success and returns.
            if racing && succeeded > 0 {
                skipped.extend(undispatched.drain(..));
                let losers: Vec<mesh_proto::JobId> = self.inner.engine.read(|s| {
                    plan.targets
                        .iter()
                        .filter(|t| {
                            s.jobs
                                .get(&t.job_id)
                                .is_some_and(|j| !j.state.is_terminal())
                        })
                        .map(|t| t.job_id)
                        .collect()
                });
                for job_id in losers {
                    let resp = self.ipc_cancel(job_id).await;
                    if let IpcResponse::Error { message } = resp {
                        debug!(group = %group.short(), job = %job_id.short(),
                            "race: cannot cancel losing copy: {message}");
                    }
                }
            }

            // Everything accounted for → close the group.
            let skipped_count = u32::try_from(skipped.len()).unwrap_or(u32::MAX);
            if undispatched.is_empty() && in_flight == 0 {
                info!(group = %group.short(), succeeded, failed, skipped = skipped_count,
                    "fan-out complete under this owner");
                self.inner.groups.finished(
                    crate::observe::GroupKind::Fanout,
                    if halted {
                        crate::observe::GroupOutcome::Halted
                    } else if failed > 0 {
                        crate::observe::GroupOutcome::Failed
                    } else {
                        crate::observe::GroupOutcome::Completed
                    },
                    // The time *this* owner spent on it. The origin's share
                    // died with the origin, and inventing a total from the
                    // plan's creation would report this node as having run
                    // for however long the dead one did.
                    began.elapsed(),
                    u64::from(succeeded),
                    u64::from(failed),
                    u64::from(skipped_count),
                );
                if let Err(e) = self.inner.engine.author_op(OpBody::FanoutClosed {
                    group,
                    succeeded,
                    failed,
                    skipped: skipped_count,
                }) {
                    warn!(group = %group.short(), "cannot close fan-out: {e}");
                }
                return;
            }

            // Dispatch as many pending targets as the window allows.
            for i in undispatched {
                if in_flight >= window {
                    break;
                }
                let target = &plan.targets[i];
                let reachable = self.inner.engine.read(|s| {
                    s.is_active_member(&target.node_id)
                        && !matches!(
                            self.inner.gossip.liveness_of(&target.node_id),
                            Liveness::Dead
                        )
                });
                if !reachable {
                    skipped.insert(i);
                    continue;
                }
                let spec = Self::fan_spec(&plan, i);
                // One attempt per round, with no waiting inside it: this
                // loop drives every target, and the next round asks again —
                // which is safe, since the job id is fixed and an executor
                // acknowledges one it already holds.
                let result = self.submit_once(target.node_id, spec).await;
                if let Err(e) = &result {
                    self.inner.dispatch_failures.note(e);
                }
                match result {
                    Ok(()) => {
                        failing_since.remove(&i);
                        in_flight += 1;
                        if target.node_id != me {
                            // Pull its summary back promptly.
                            let member = self
                                .inner
                                .engine
                                .read(|s| s.members.get(&target.node_id).map(|m| m.info.clone()));
                            if let Some(member) = member {
                                let gossip = self.inner.gossip.clone();
                                tokio::spawn(async move {
                                    let _ = gossip.sync_with(&member).await;
                                });
                            }
                        }
                    }
                    Err(e) => {
                        let tries = attempts.entry(i).or_insert(0);
                        *tries += 1;
                        warn!(group = %group.short(), target = %target.node_name,
                            attempt = *tries, "adopted dispatch failed: {e}");
                        // Give up on a definitive refusal after a few tries, and
                        // on an unreachable target once a restart would have
                        // finished.
                        let since = *failing_since.entry(i).or_insert_with(Instant::now);
                        let gave_up = if may_spill(&e) || is_undetermined(&e) {
                            since.elapsed() >= FANOUT_PATIENCE && *tries >= DRIVE_MAX_ATTEMPTS
                        } else {
                            *tries >= DRIVE_MAX_ATTEMPTS
                        };
                        if gave_up {
                            skipped.insert(i);
                        }
                    }
                }
            }
            // Woken when a job changes state, with the old interval kept
            // as a backstop for anything that is not a job op (**M-13**).
            if tokio::time::timeout(DRIVE_POLL, jobs_changed.changed())
                .await
                .is_ok()
            {
                tokio::time::sleep(DRIVE_FLOOR).await;
            }
        }
    }

    // -- Git sources (MASTERPLAN §8.1) -------------------------------------

    // -- M18 facts (M17-M21 plan §4) ---------------------------------------

    /// Describe this machine, and re-describe it when it changes (§4.5).
    ///
    /// **No election.** Each node runs the collectors whose selector
    /// matches it, on its own timer, and publishes about itself. There is
    /// nothing to coordinate because a node's facts exist in exactly one
    /// place — deliberately *unlike* M8's `elected_firer`, which exists
    /// because a schedule must fire once.
    ///
    /// The loop runs on every node with no configuration, because a fresh
    /// node must have facts before anybody has pushed a bundle to it — the
    /// bootstrap ordering problem is solved by not having one.
    fn spawn_facts_task(self) {
        if !self.inner.config.facts.collect {
            debug!("fact collection disabled ([facts] collect = \"off\")");
            return;
        }
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                // The built-in collector runs immediately at boot: "what is
                // this node" must be answerable the moment it joins, not
                // one interval later.
                let _ = this.collect_builtin().await;
                let mut last: BTreeMap<String, u64> = BTreeMap::new();
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(crate::facts::TICK) => {}
                        _ = shutdown.changed() => return,
                    }
                    this.facts_tick(&mut last).await;
                }
            }
        });
        self.track(handle);
    }

    /// One pass: the built-in collector on its own interval, then every
    /// published collector whose selector matches this node and whose
    /// interval has elapsed.
    async fn facts_tick(&self, last: &mut BTreeMap<String, u64>) {
        let now = mesh_proto::now_micros();
        let due = |last: &BTreeMap<String, u64>, name: &str, interval: u64| {
            last.get(name)
                .is_none_or(|at| now.saturating_sub(*at) >= interval * 1_000_000)
        };

        if due(
            last,
            mesh_proto::SYSTEM_COLLECTOR,
            mesh_proto::SYSTEM_INTERVAL_SECS,
        ) && self.collect_builtin().await
        {
            // Only a settled attempt starts the interval. An unsettled one
            // — this node is not yet a member of its own state — leaves the
            // timer unset so the next tick retries in 30 seconds, which is
            // the difference between "facts appear as the node joins" and
            // "facts appear a quarter of an hour later, if at all".
            last.insert(mesh_proto::SYSTEM_COLLECTOR.to_string(), now);
        }

        // Read the whole decision under one state lock, then act outside
        // it: running a collector spawns a job, and that does not belong
        // inside the lock.
        let me = self.node_id();
        let mine: Vec<mesh_proto::FactCollector> = self.inner.engine.read(|s| {
            let Some(member) = s.members.get(&me).filter(|m| !m.left) else {
                return Vec::new();
            };
            s.collectors
                .values()
                .filter(|e| e.is_armed())
                .filter(|e| Selector::parse(&e.collector.on).matches_in(s, &member.info))
                .map(|e| e.collector.clone())
                .collect()
        });
        for collector in mine {
            if !due(last, &collector.name, collector.interval()) {
                continue;
            }
            if self.run_collector(&collector).await {
                last.insert(collector.name.clone(), now);
            }
        }
        // A collector that no longer matches this node should not keep a
        // timer, or a re-match would look overdue by however long it was
        // away and publish immediately.
        last.retain(|name, _| {
            name == mesh_proto::SYSTEM_COLLECTOR
                || self.inner.engine.read(|s| s.collectors.contains_key(name))
        });
    }

    /// The in-process `system` collector (§4.5).
    ///
    /// Implemented here rather than as a published collector so a fresh
    /// node has facts before anybody has pushed anything to it — the
    /// bootstrap problem ("publish a collector to learn what the node is,
    /// on a node you cannot select yet") solved by not having it.
    ///
    /// Returns [`publish_facts`](Self::publish_facts)'s settled flag, so a
    /// node that ran this before it was a member of its own state is tried
    /// again on the next 30s tick rather than at the next 15-minute one.
    async fn collect_builtin(&self) -> bool {
        let doc = crate::facts::system_document(&self.inner.config.data_dir);
        let text = doc.to_string();
        // No redactor: the built-in collector reads `/etc/os-release`,
        // `/proc` and DMI, and every key it publishes is enumerated in
        // `facts::system_document`. There is no path for a credential to
        // reach it, which is a property of the collector rather than of
        // the filter.
        self.publish_facts(
            mesh_proto::SYSTEM_COLLECTOR,
            mesh_proto::FactSubject::Node(self.node_id()),
            &text,
            None,
        )
    }

    /// Run one published collector and publish what it printed.
    ///
    /// A collector is an ordinary job on this node: same executor, same
    /// sandbox, same secrets handling, same log spool. The only thing M18
    /// adds is that its `stdout` is read back through M19's capture — which
    /// is exactly the cross-milestone dependency the plan records (§8), and
    /// the reason M19 P1 lands before M18 P2.
    ///
    /// Returns whether every subject settled — see
    /// [`publish_facts`](Self::publish_facts).
    async fn run_collector(&self, collector: &mesh_proto::FactCollector) -> bool {
        let subjects: Vec<mesh_proto::FactSubject> = match collector.subject {
            mesh_proto::FactSubjectKind::Node => {
                vec![mesh_proto::FactSubject::Node(self.node_id())]
            }
            // A collector for inventory hosts, published before the host
            // inventory was removed. There are no hosts to describe, so it
            // runs nowhere; a new one is refused at publish.
            mesh_proto::FactSubjectKind::Host => Vec::new(),
        };
        if subjects.is_empty() {
            return true;
        }
        let mut settled = true;
        for subject in subjects {
            match self.collect_once(collector, &subject).await {
                Ok((text, redactor)) => {
                    settled &=
                        self.publish_facts(&collector.name, subject, &text, redactor.as_ref());
                }
                Err(e) => {
                    // A collector that failed to *run* has had its turn: the
                    // command is broken, or the host is down, and retrying
                    // it every 30 seconds would turn one bad record into a
                    // permanent load. It waits for its own interval.
                    warn!(collector = %collector.name, subject = %subject.label(),
                        "fact collection failed: {e}");
                }
            }
        }
        settled
    }

    /// Run a collector's `StepAction` here and return its stdout.
    async fn collect_once(
        &self,
        collector: &mesh_proto::FactCollector,
        subject: &mesh_proto::FactSubject,
    ) -> Result<(String, Option<crate::secrets::Redactor>)> {
        let payload = self.collector_payload(collector)?;
        let env: Vec<(String, String)> = vec![
            (
                mesh_proto::FACT_COLLECTOR_ENV.to_string(),
                collector.name.clone(),
            ),
            (mesh_proto::FACT_SUBJECT_ENV.to_string(), subject.label()),
        ];
        let spec = JobSpec {
            job_id: mesh_proto::JobId::generate(),
            payload,
            env,
            timeout_secs: Some(collector.timeout_secs.unwrap_or(120)),
            priority: JobPriority::Low,
            submitter: self.node_id(),
            submitter_user: collector.created_by,
            // The collector record **is** the authorization, exactly as a
            // schedule record is for a scheduled run (M8 §3.4): the job
            // never leaves this node, and its creator's role was verified
            // at merge time by every node in the mesh. The envelope carries
            // the collector's own signature for the same reason a scheduled
            // fire carries the schedule's — it is the signature a human
            // actually produced, and it is re-verifiable against state.
            auth: SignedIpc {
                user: collector.created_by,
                at_micros: collector.created_at_micros,
                nonce: 0,
                payload: Vec::new(),
                signature: collector.signature.clone(),
            },
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        };
        let job_id = spec.job_id;
        self.inner.jobs.accept(spec)?;

        let (tx, mut rx) = mpsc::channel::<IpcResponse>(64);
        let this = self.clone();
        let reader = tokio::spawn(async move {
            this.stream_job_logs(job_id, this.node_id(), true, tx).await;
        });
        let mut stdout = Vec::new();
        let mut state = None;
        while let Some(frame) = rx.recv().await {
            match frame {
                IpcResponse::LogChunk { stream: 1, data } => {
                    if stdout.len() < mesh_proto::MAX_FACT_DOC * 4 {
                        stdout.extend_from_slice(&data);
                    }
                }
                IpcResponse::JobDone { state: s, .. } => state = Some(s),
                _ => {}
            }
        }
        let _ = reader.await;
        if state != Some(mesh_proto::JobState::Succeeded) {
            return Err(CoreError::Job(format!(
                "collector {:?} exited {}",
                collector.name,
                state
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_else(|| "without finishing".into())
            )));
        }
        // R-4, and where it is actually enforced: the log spool applied the
        // job's own `Redactor` to these bytes **before a single one reached
        // disk** (MASTERPLAN §7.3), so what is read back here is already
        // clean of every secret this collector resolved. Re-running the
        // filter would mean the daemon holding the plaintext to build one,
        // which is the thing `Zeroizing` in `secrets::resolve_env` exists
        // to prevent — a second lint is not worth keeping a credential
        // alive for. [`crate::facts::prepare`] still takes a filter, for a
        // caller that legitimately holds one.
        Ok((String::from_utf8_lossy(&stdout).into_owned(), None))
    }

    fn collector_payload(&self, collector: &mesh_proto::FactCollector) -> Result<JobPayload> {
        match &collector.run {
            mesh_proto::StepAction::Command(cmd) => Ok(JobPayload::Command(cmd.clone())),
            mesh_proto::StepAction::Script(script) => Ok(JobPayload::Script(script.clone())),
            mesh_proto::StepAction::Service {
                name,
                version,
                inputs,
            } => {
                let exec = match version {
                    // A pinned version, resolved against replicated state
                    // exactly as a workflow step's pin is.
                    Some(v) => {
                        let def = self
                            .inner
                            .engine
                            .read(|s| {
                                s.services
                                    .get(name)
                                    .and_then(|e| e.versions.get(v).cloned())
                            })
                            .ok_or_else(|| CoreError::State(format!("no service {name}@{v}")))?;
                        services::resolve_service_version(&def, inputs.clone(), None)?
                    }
                    None => {
                        services::resolve_service(&self.inner.engine, name, inputs.clone(), None)?
                    }
                };
                Ok(JobPayload::Service(Box::new(exec)))
            }
            // A builtin task is a pure function of its inputs, so it can
            // only ever print the same document — a collector made of one
            // would author exactly one op and then be dead weight. An
            // Ansible play is a fan-out over an inventory, which is what a
            // *host* collector's `run` already is. Both are refused where
            // the collector is published, and again here.
            mesh_proto::StepAction::Builtin { task, .. } => Err(CoreError::Config(format!(
                "a collector runs a command, a script or a service — the built-in task \
                 {task:?} is a pure function of its inputs, so it would publish one \
                 document and then never change"
            ))),
            mesh_proto::StepAction::Ansible(_) => Err(CoreError::Config(
                "a collector runs a command, a script or a service — an Ansible play \
                 fans out over an inventory, which is what `subject: host` already does"
                    .into(),
            )),
        }
    }

    /// Publish a document, if the digest changed and the guard admits it.
    ///
    /// Everything that makes facts affordable is in this one function:
    /// canonical encoding, the byte cap, the redaction lint, publish-only-
    /// on-change, and the flap guard.
    /// Returns whether this attempt is **settled** — published, already
    /// known, refused, or held back as flapping. `false` means only that
    /// the op could not be authored *yet*, which is the ordinary state of a
    /// node that has read its own facts before its `MemberAdd` arrived: the
    /// apply parks such an op and parking drops it, so the caller must come
    /// back on the next tick rather than at the collector's next interval.
    fn publish_facts(
        &self,
        collector: &str,
        subject: mesh_proto::FactSubject,
        stdout: &str,
        redactor: Option<&crate::secrets::Redactor>,
    ) -> bool {
        let (doc, digest) = match crate::facts::prepare(collector, stdout, redactor) {
            Ok(pair) => pair,
            Err(e) => {
                warn!(collector, subject = %subject.label(), "fact publish refused: {e}");
                return true;
            }
        };
        // The cheapest check first: what this node already believes.
        let known = self.inner.engine.read(|s| {
            s.facts
                .get(&subject)
                .and_then(|by| by.get(collector))
                .map(|e| e.digest)
        });
        if known == Some(digest) {
            return true;
        }
        let key = crate::facts::guard_key(collector, &subject);
        let now = mesh_proto::now_micros();
        match self.inner.flap.admit(&key, &doc, now) {
            crate::facts::Admit::Unchanged => return true,
            crate::facts::Admit::Flapping(keys) => {
                warn!(
                    collector,
                    subject = %subject.label(),
                    keys = %keys.iter().map(|c| c.key.as_str()).collect::<Vec<_>>().join(", "),
                    "collector is flapping: it changed again inside the publish window. \
                     A fact is a property, not a measurement — `meshnode doctor` names \
                     the keys"
                );
                return true;
            }
            crate::facts::Admit::Yes => {}
        }
        if let Err(e) = self.inner.engine.author_op(OpBody::FactsPublish {
            subject: subject.clone(),
            collector: collector.to_string(),
            doc: doc.clone().into_bytes(),
            digest,
            collected_at_micros: now,
        }) {
            // Deliberately not recorded as published: nothing reached the
            // op-log, so the guard must still be able to admit this exact
            // document the next time round.
            debug!(collector, subject = %subject.label(), "facts not publishable yet: {e}");
            return false;
        }
        self.inner.flap.published(&key, &doc, now);
        debug!(collector, subject = %subject.label(), "facts published");
        true
    }

    /// Fire schedules (M8 §3.2, §3.3).
    ///
    /// The same shape as [`Self::spawn_git_task`], with one difference that
    /// is the whole milestone: a git sync is idempotent by content hash, so
    /// two watchers double-syncing costs nothing. A workflow is not
    /// idempotent, so instead of trying to prevent a double fire — which is
    /// impossible across a partition — the run's id is *derived* from
    /// `(schedule, occurrence)`, and two firers converge on one run.
    ///
    /// Due-ness is read from replicated state, never from this node's
    /// memory: "has 03:00 already fired?" is "does the run it would have
    /// created exist?", which every node answers identically and which
    /// survives a restart for free.
    fn spawn_deployment_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                let mut last_registry: Option<Vec<mesh_proto::Registration>> = None;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(DEPLOYMENT_TICK) => {}
                        _ = shutdown.changed() => return,
                    }
                    this.deployment_tick().await;
                    this.schedule_tick().await;
                    // After the health tick has had a chance to publish, so
                    // a registration carries the conclusion rather than the
                    // one before it.
                    this.registry_tick(&mut last_registry).await;
                }
            }
        });
        self.track(handle);
    }

    /// The executor-side health prober (C2 P3, §6.2).
    ///
    /// Health is judged **on the node running the instance**, never from a
    /// central prober — a check from elsewhere measures the network more
    /// than the service. This loop owns the per-instance failure counters
    /// across ticks, kills an instance that has failed past its threshold
    /// (turning it into an ordinary failed job the reconciler replaces and
    /// the failure budget counts), and publishes one per-node health record
    /// the elected reconciler gates rolling updates on.
    fn spawn_deployment_health_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                let mut trackers: std::collections::HashMap<JobId, InstanceHealthTracker> =
                    std::collections::HashMap::new();
                let mut last_published: Option<Vec<mesh_proto::InstanceHealth>> = None;
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(DEPLOYMENT_HEALTH_TICK) => {}
                        _ = shutdown.changed() => return,
                    }
                    this.deployment_health_tick(&mut trackers, &mut last_published)
                        .await;
                }
            }
        });
        self.track(handle);
    }

    /// One pass of the health prober: probe every local checked instance,
    /// kill the ones that have failed past threshold, and publish the
    /// per-node record if anything changed.
    async fn deployment_health_tick(
        &self,
        trackers: &mut std::collections::HashMap<JobId, InstanceHealthTracker>,
        last_published: &mut Option<Vec<mesh_proto::InstanceHealth>>,
    ) {
        let me = self.node_id();
        let now = mesh_proto::now_micros();

        // Gather this node's live checked instances from replicated state:
        // for each active deployment with a health check, derive its current
        // instances and keep the ones running here.
        struct Local {
            job_id: JobId,
            deployment: String,
            generation: u32,
            check: mesh_proto::HealthCheck,
            /// The port label to probe, when the check named one instead of
            /// a number (N6). Resolved against this node's own ledger just
            /// before the probe, because only the node that allocated the
            /// port knows it.
            port_label: Option<String>,
            started_at: u64,
            /// The instance is a container (Phase A, §2.5b), so an `exec`
            /// check runs inside it rather than on this host.
            container: bool,
        }
        let locals: Vec<Local> = self.inner.engine.read(|s| {
            let mut out = Vec::new();
            for entry in s.deployments.values() {
                if entry.removed {
                    continue;
                }
                let dep = &entry.deployment;
                let Some(check) = dep.health.clone() else {
                    continue;
                };
                let container = matches!(
                    Self::deployment_runtime(s, dep, false),
                    Some(mesh_proto::Runtime::Container)
                );
                let port_label = crate::deployments::policy_for(s, &dep.name)
                    .instance_env
                    .iter()
                    .find(|(k, _)| k == mesh_proto::REQ_HEALTH_PORT)
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.is_empty());
                // A live deployment: its service is published, so the lookup works
                // and the rehearsal-only fallback is not needed.
                let candidates = self.deployment_candidates(s, dep, false);
                let slots: Vec<crate::deployments::Slot> = match dep.count {
                    mesh_proto::Count::System => candidates
                        .iter()
                        .map(|m| crate::deployments::Slot::Node(m.node_id))
                        .collect(),
                    mesh_proto::Count::Fixed(n) => {
                        (0..n).map(crate::deployments::Slot::Replica).collect()
                    }
                };
                for slot in &slots {
                    for inc in 0..64u32 {
                        let id =
                            crate::deployments::instance_id(&dep.name, dep.generation, slot, inc);
                        match s.jobs.get(&id) {
                            Some(job) if !job.state.is_terminal() => {
                                if job.executor == me
                                    && matches!(job.state, mesh_proto::JobState::Running)
                                {
                                    out.push(Local {
                                        job_id: id,
                                        deployment: dep.name.clone(),
                                        generation: dep.generation,
                                        check: check.clone(),
                                        port_label: port_label.clone(),
                                        container,
                                        started_at: job
                                            .started_at_micros
                                            .unwrap_or(job.created_at_micros),
                                    });
                                }
                                break;
                            }
                            Some(_) => continue,
                            None => break,
                        }
                    }
                }
            }
            out
        });

        // Forget trackers for instances that are no longer local.
        let live: std::collections::HashSet<JobId> = locals.iter().map(|l| l.job_id).collect();
        trackers.retain(|id, _| live.contains(id));

        let mut records: Vec<mesh_proto::InstanceHealth> = Vec::with_capacity(locals.len());
        for local in &locals {
            let tracker = trackers
                .entry(local.job_id)
                .or_insert_with(|| InstanceHealthTracker {
                    healthy: false,
                    consecutive_failures: 0,
                    since_micros: now,
                    detail: "starting".into(),
                });

            // Grace: a service that takes time to warm up must not be killed
            // at second zero. During grace it reports "starting" (not
            // healthy for gating), and failures do not count.
            let in_grace = now.saturating_sub(local.started_at)
                < local.check.grace_secs.saturating_mul(1_000_000);

            // Resolve a labelled port against this node's ledger. The
            // allocation is node-local and only exists once the job is
            // running, which is exactly when this loop sees it.
            let mut check = local.check.clone();
            if let Some(label) = &local.port_label {
                match self
                    .inner
                    .jobs
                    .ports_held_by(local.job_id)
                    .into_iter()
                    .find(|p| &p.label == label)
                {
                    Some(alloc) => check.port = alloc.host,
                    None => {
                        // Not an unhealthy instance — an unprobeable one.
                        // Reporting "tcp: connection refused" against port
                        // 0 would be a lie about the service.
                        tracker.detail = format!("waiting for port {label:?} to be allocated");
                        continue;
                    }
                }
            }
            // Phase A, §2.5b: an `exec` check on a container runs inside the
            // container. A configured container plugin owns its containers
            // and no engine is installed beside it, so that one case keeps
            // the host probe — and `limits.md` says so rather than this
            // quietly checking the wrong machine.
            let engine = if local.container && matches!(check.kind, mesh_proto::HealthKind::Exec) {
                self.inner.jobs.container_engine()
            } else {
                None
            };
            let (ok, detail) = match engine {
                Some(engine) => {
                    crate::engines::exec_check(
                        &engine,
                        &local.job_id.short(),
                        &check.command,
                        Duration::from_secs(check.timeout_secs.max(1)),
                    )
                    .await
                }
                None => probe_health(&check).await,
            };
            if ok {
                if !tracker.healthy {
                    tracker.since_micros = now;
                }
                tracker.healthy = true;
                tracker.consecutive_failures = 0;
                tracker.detail = detail;
            } else if in_grace {
                tracker.detail = "starting".into();
            } else {
                tracker.consecutive_failures = tracker.consecutive_failures.saturating_add(1);
                if tracker.healthy {
                    tracker.since_micros = now;
                }
                tracker.healthy = false;
                tracker.detail = detail;
                if tracker.consecutive_failures >= local.check.failures_to_unhealthy.max(1) {
                    // Kill it: it becomes an ordinary failed job, which the
                    // reconciler replaces and the failure budget counts.
                    warn!(
                        deployment = %local.deployment,
                        job = %local.job_id.short(),
                        detail = %tracker.detail,
                        "instance failed health check; replacing it"
                    );
                    // `fail`, not `cancel`: a health failure counts against
                    // the restart budget, so a flapping service trips
                    // `degraded` rather than restarting forever (§6.2).
                    let _ = self.inner.jobs.fail(&local.job_id);
                }
            }

            records.push(mesh_proto::InstanceHealth {
                job_id: local.job_id,
                deployment: local.deployment.clone(),
                generation: local.generation,
                healthy: tracker.healthy,
                since_micros: tracker.since_micros,
                detail: tracker.detail.clone(),
            });
        }

        // Publish only when the set or a health bit changed — one op per
        // node, LWW, off the per-job write path (§6.2).
        records.sort_by_key(|a| a.job_id.0);
        // An idle node publishes nothing — like `ReservationsPut` and the
        // usage counters, and for a second reason too: an empty op still
        // rides the author's gapless stream, so a node that authored one
        // would block its own later ops from a pre-F0 peer that stops at
        // the first body it cannot decode. Only publish once there is
        // something to say, and once more to clear it.
        let nothing_to_say =
            records.is_empty() && last_published.as_ref().is_none_or(|p| p.is_empty());
        let changed = last_published
            .as_ref()
            .map(|prev| prev != &records)
            .unwrap_or(true);
        if changed && !nothing_to_say {
            let health = mesh_proto::NodeInstanceHealth {
                node: me,
                instances: records.clone(),
                published_at_micros: now,
            };
            if let Err(e) = self.inner.engine.author_op(OpBody::DeploymentHealthPut {
                health: Box::new(health),
            }) {
                warn!("failed to publish deployment health: {e}");
            } else {
                *last_published = Some(records);
            }
        }
    }

    /// Publish what this node currently stands behind (N3 P1).
    ///
    /// **Results, never checks, and only on a change.** The health
    /// conclusion is read from the record this node just published for its
    /// own instances, so the registry and `deployment status` can never
    /// disagree about whether an instance is passing — and a flapping
    /// service authors one op per *transition* rather than one per probe.
    /// An idle node publishes nothing at all, which matters twice over: an
    /// empty op still rides the author's gapless stream, so authoring one
    /// would block this node's later ops from a pre-F0 peer.
    async fn registry_tick(&self, last: &mut Option<Vec<mesh_proto::Registration>>) {
        let me = self.node_id();
        let now = mesh_proto::now_micros();
        let mut records = self.inner.jobs.registrations();
        if records.is_empty() && last.as_ref().is_none_or(|p| p.is_empty()) {
            // **Unless the mesh still holds an older word from this node
            // (2026-09-14).** `last` starts empty on every start, so a node
            // whose registered work ended while it was down — Director quit,
            // and its containers went with it — came back with nothing to say
            // and said nothing, and its last record went on listing copies
            // that no longer existed. Nobody else may replace a node's
            // record, so it stood for good. Clearing it is the "once more to
            // clear it" publish, not an empty op out of nowhere.
            let stale = last.is_none()
                && self
                    .inner
                    .engine
                    .read(|s| s.registry.get(&me).is_some_and(|r| !r.services.is_empty()));
            if !stale {
                return;
            }
        }

        // Overlay health from this node's own health record — the same
        // conclusion `deployment status` shows, rather than a second
        // judgement that could differ from it.
        //
        // A registration with no health record is `Unknown`, not
        // `Critical`: an unchecked service has nobody to say it is broken,
        // and refusing to resolve it would make declaring a health check a
        // prerequisite for being findable.
        let health: std::collections::HashMap<JobId, (bool, u64)> = self.inner.engine.read(|s| {
            s.deployment_health
                .get(&me)
                .map(|n| {
                    n.instances
                        .iter()
                        .map(|i| (i.job_id, (i.healthy, i.since_micros)))
                        .collect()
                })
                .unwrap_or_default()
        });
        for r in &mut records {
            if let Some((ok, since)) = health.get(&r.instance).copied() {
                r.health = if ok {
                    mesh_proto::Health::Passing
                } else {
                    mesh_proto::Health::Critical
                };
                r.since_micros = since;
            }
        }
        // `since_micros` moves whenever health does, so comparing the whole
        // record would republish on every probe of a *stable* service —
        // the flap the landmine names, arriving by the back door. Compare
        // everything except the timestamp.
        let same = last.as_ref().is_some_and(|prev| {
            prev.len() == records.len()
                && prev.iter().zip(records.iter()).all(|(a, b)| {
                    a.service == b.service
                        && a.instance == b.instance
                        && a.label == b.label
                        && a.address == b.address
                        && a.port == b.port
                        && a.tags == b.tags
                        && a.health == b.health
                        && a.deployment == b.deployment
                })
        });
        if same {
            return;
        }
        let payload = mesh_proto::NodeRegistrations {
            node: me,
            services: records.clone(),
            published_at_micros: now,
        };
        match self.inner.engine.author_op(OpBody::RegistryPut {
            registrations: Box::new(payload),
        }) {
            Ok(_) => *last = Some(records),
            Err(e) => warn!("failed to publish the service registry: {e}"),
        }
    }

    fn spawn_git_task(self) {
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        let handle = tokio::spawn({
            let this = self.clone();
            async move {
                // Local attempt clock so a failing source (git down) retries
                // on its interval instead of every tick.
                let mut last_attempt: HashMap<String, Instant> = HashMap::new();
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(GIT_SCAN_INTERVAL) => {}
                        _ = shutdown.changed() => return,
                    }
                    let me = this.node_id();
                    let now = mesh_proto::now_micros();
                    let due: Vec<GitSource> = this.inner.engine.read(|s| {
                        s.sources
                            .values()
                            .filter(|e| !e.removed)
                            .filter(|e| {
                                let last = e.last.as_ref().map(|l| l.at.wall_micros).unwrap_or(0);
                                now.saturating_sub(last)
                                    >= e.source.interval_secs.saturating_mul(1_000_000)
                            })
                            .filter(|e| {
                                gitsync::elected_watcher(s, &e.source.name, |id| {
                                    this.inner.gossip.liveness_of(id)
                                }) == Some(me)
                            })
                            .map(|e| e.source.clone())
                            .collect()
                    });
                    for source in due {
                        let recently_tried = last_attempt.get(&source.name).is_some_and(|at| {
                            at.elapsed() < Duration::from_secs(source.interval_secs.max(15))
                        });
                        if recently_tried {
                            continue;
                        }
                        last_attempt.insert(source.name.clone(), Instant::now());
                        this.clone().spawn_source_sync(source);
                    }
                }
            }
        });
        self.track(handle);
    }

    /// How this node runs `git` for a source, as it is configured now.
    fn git_run(&self) -> gitsync::GitRun {
        gitsync::GitRun {
            config: self.inner.config.git.clone(),
            sandbox: self.inner.jobs.sandbox_policy(),
        }
    }

    fn spawn_source_sync(self, source: GitSource) {
        if !self.inner.active_syncs.lock().insert(source.name.clone()) {
            return;
        }
        tokio::spawn(async move {
            let result = gitsync::sync_source(
                &self.inner.engine,
                &self.inner.content,
                &self.inner.config.data_dir,
                &source,
                &self.git_run(),
            )
            .await;
            if let Err(e) = result {
                warn!(source = %source.name, "git sync failed: {e}");
            }
            self.inner.active_syncs.lock().remove(&source.name);
        });
    }

    /// `Push`: read a tree from this machine into the content store.
    ///
    /// The one request that has the daemon open a path the caller chose,
    /// and until SEC-3 it opened any of them. A job's sandbox hides the
    /// node's `identity/` from the job — and a job holding its credential
    /// could ask the daemon to read `identity/` for it, announce the bundle
    /// mesh-wide, and read it back. The node key opens every secret sealed
    /// to the node. So did anyone who could reach the HTTP API with the
    /// content role.
    ///
    /// Two guards now, and the second is the one that matters:
    ///
    ///   * the path, resolved, must not be the node's own directory or a
    ///     credential store, nor contain one — except a job's own directory
    ///     for that job, which is where a task stages what it pushes;
    ///   * the reading happens on a thread of its own under a Landlock
    ///     ruleset: for a job credential, exactly what that job's processes
    ///     may read; for anyone else, everything but the node's directory
    ///     and the credential stores. The kernel enforces it for every file
    ///     the walk opens, so a directory swapped for a symlink mid-walk
    ///     reaches nothing the check above refused.
    ///
    /// On a machine without Landlock the first guard stands alone, and a
    /// job there is not sandboxed to begin with.
    fn ipc_push(&self, path: &str, job: Option<JobId>) -> IpcResponse {
        let error = |message: String| IpcResponse::Error { message };
        let root = match std::fs::canonicalize(path) {
            Ok(root) => root,
            Err(e) => return error(format!("{path}: {e}")),
        };
        let data_dir = std::fs::canonicalize(&self.inner.config.data_dir)
            .unwrap_or_else(|_| self.inner.config.data_dir.clone());
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let mut hidden = vec![data_dir.clone()];
        if let Some(home) = &home {
            hidden.extend(crate::sandbox::home_secrets(home));
        }
        let own = match job {
            Some(job) => match self.inner.jobs.readable_by_job(&job) {
                Some((dir, readable)) => {
                    Some((std::fs::canonicalize(&dir).unwrap_or(dir), readable))
                }
                None => return error("no journal for the job this credential belongs to".into()),
            },
            None => None,
        };
        let in_own = own.as_ref().is_some_and(|(dir, _)| root.starts_with(dir));
        for h in &hidden {
            let h = std::fs::canonicalize(h).unwrap_or_else(|_| h.clone());
            if (root.starts_with(&h) && !in_own) || (h.starts_with(&root) && h != root) {
                return error(format!(
                    "{} is, or contains, {} — the daemon does not read its own \
                     directory or a credential store into the mesh",
                    root.display(),
                    h.display()
                ));
            }
        }
        let content_dir = data_dir.join("content");
        let read = match own {
            Some((_, Some(readable))) => readable,
            // An unsandboxed job reads what the daemon reads, less the
            // node's own directory — but its own directory is in there, and
            // is where it staged what it is pushing.
            Some((dir, None)) => {
                let mut read = crate::sandbox::everything_but(&hidden);
                read.push(dir);
                read
            }
            None => crate::sandbox::everything_but(&hidden),
        };
        let content = &self.inner.content;
        // Nothing is logged from inside: the thread can open no file outside
        // its ruleset, and a log sink is not a thing to find that out with.
        let ingested = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let status = crate::sandbox::confine_this_thread(
                        &read,
                        std::slice::from_ref(&content_dir),
                    );
                    let pushed = content.ingest_path(&root).and_then(|manifest| {
                        let bundle_id = content.save_manifest(&manifest)?;
                        Ok((manifest, bundle_id))
                    });
                    (status, pushed)
                })
                .join()
        });
        let (manifest, bundle_id) = match ingested {
            Ok((status, pushed)) => {
                if status != "full" {
                    debug!(status, "push read without a full Landlock ruleset");
                }
                match pushed {
                    Ok(done) => done,
                    Err(e) => return error(e.to_string()),
                }
            }
            Err(_) => return error("the push thread panicked".into()),
        };
        let meta = content.meta(&manifest);
        if let Err(e) = services::announce_hold(&self.inner.engine, &meta) {
            return error(e.to_string());
        }
        IpcResponse::Pushed {
            bundle_id,
            file_count: meta.file_count,
            chunk_count: meta.chunk_count,
            total_size: meta.total_size,
        }
    }

    /// Why `path` is no place for a backup, if it is not (SEC-3).
    fn backup_target_refusal(&self, path: &str) -> Option<String> {
        let target = std::path::Path::new(path);
        let parent = match target.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => std::path::Path::new("."),
        };
        let parent = std::fs::canonicalize(parent).ok()?;
        let data_dir = std::fs::canonicalize(&self.inner.config.data_dir)
            .unwrap_or_else(|_| self.inner.config.data_dir.clone());
        parent.starts_with(&data_dir).then(|| {
            format!(
                "{path} is inside this node's data directory ({}); write the backup elsewhere",
                data_dir.display()
            )
        })
    }

    /// What a job credential may not ask for at all (SEC-3).
    ///
    /// A credential speaks for the job's submitter, and most of what that
    /// person may do it may do too. Not these: each has the daemon act on
    /// this machine outside the job's sandbox — a backup writes the node's
    /// identity to a path of the caller's choosing, and a session is a shell
    /// running as the daemon with no ruleset at all.
    fn credential_refusal(&self, env: &SignedIpc, req: &IpcRequest) -> Option<String> {
        self.inner.jobs.acting_for(&env.user)?;
        let what = match req {
            IpcRequest::Backup { .. } => "a backup",
            IpcRequest::SessionOpen { .. } => "a session",
            IpcRequest::MeshReset { .. } => "a mesh reset",
            _ => return None,
        };
        Some(format!(
            "a job's credential cannot ask for {what}: it would act on this machine \
             outside the job's sandbox"
        ))
    }

    /// [`Self::handle_signed`], for a caller that is **not on this
    /// machine** — the HTTP API (SEC-3).
    ///
    /// The same dispatch, less the requests that name a path on this
    /// node's own filesystem. Over the socket the caller is this machine's
    /// own account; over HTTP it is anyone with a key, and a path they
    /// choose on a host they are not on is a way to read or write files,
    /// not a feature.
    pub async fn handle_signed_remote(
        &self,
        env: SignedIpc,
        from: std::net::IpAddr,
    ) -> IpcResponse {
        // A request that crossed a network has to name this node (SEC-12).
        // An unbound one would be as good at every other node that knows
        // its signer for the rest of its window, and this listener is
        // plain HTTP.
        if self.envelope_binding(&env) != Some(crate::identity::IpcBinding::Bound(self.node_id())) {
            return IpcResponse::Error {
                message: format!(
                    "a request over the HTTP API must be signed for this node — mesh {} node {} \
                     (`Keypair::sign_request_for`)",
                    self.mesh_id().to_display_string(),
                    self.node_id().to_display_string()
                ),
            };
        }
        match postcard::from_bytes::<IpcRequest>(&env.payload) {
            Ok(IpcRequest::Push { .. }) | Ok(IpcRequest::Backup { .. }) => IpcResponse::Error {
                message: "this request names a path on the node's own filesystem and is \
                          served over the local socket only"
                    .into(),
            },
            Ok(IpcRequest::MeshReset { .. }) => IpcResponse::Error {
                message: RESET_LOCAL_ONLY.into(),
            },
            _ => self.handle_signed_from(env, Source::Remote(from)).await,
        }
    }

    fn definition_rows(&self) -> Vec<mesh_proto::DefinitionRow> {
        self.inner.engine.read(|s| {
            let mut rows = Vec::new();
            let mut push = |kind: &str,
                            what: &str,
                            op: &str,
                            count: usize,
                            last: Option<(u64, String)>,
                            executes: bool| {
                rows.push(mesh_proto::DefinitionRow {
                    kind: kind.into(),
                    what: what.into(),
                    op: op.into(),
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                    last_changed_micros: last.as_ref().map(|(at, _)| *at),
                    last_changed_what: last.map(|(_, what)| what),
                    executes,
                });
            };

            let live_services: Vec<_> = s.services.iter().filter(|(_, e)| !e.removed).collect();
            let latest_service = live_services
                .iter()
                .filter_map(|(name, e)| {
                    e.current()
                        .map(|v| (v.created_at_micros, format!("{name} v{}", v.version)))
                })
                .max_by_key(|(at, _)| *at);
            push(
                "services",
                "What a named piece of work is: runtime, bundle, entry point, contract",
                "ServiceVersionAdd / ServiceHead",
                live_services.len(),
                latest_service,
                false,
            );

            // Collectors *execute*, unprompted, on every node they match —
            // which is why they are the row that carries a canary (§3.3).
            let latest_collector = s
                .collectors
                .values()
                .map(|e| {
                    (
                        e.updated_hlc.wall_micros,
                        format!("collector {}", e.collector.name),
                    )
                })
                .max_by_key(|(at, _)| *at);
            push(
                "collectors",
                "What each node runs to describe itself — a script, on a timer, unprompted",
                "FactCollectorPut",
                s.collectors.len(),
                latest_collector,
                true,
            );

            let live_sessions: Vec<_> =
                s.session_policies.values().filter(|e| !e.removed).collect();
            let latest_session = live_sessions
                .iter()
                .map(|e| {
                    (
                        e.updated_hlc.wall_micros,
                        format!("terminal policy {}", e.policy.id),
                    )
                })
                .max_by_key(|(at, _)| *at);
            push(
                "session_policies",
                "Who may open a terminal, where",
                "SessionPolicyPut",
                live_sessions.len(),
                latest_session,
                false,
            );

            let live_sources: Vec<_> = s.sources.values().filter(|e| !e.removed).collect();
            let latest_source = live_sources
                .iter()
                .map(|e| {
                    (
                        e.updated_hlc.wall_micros,
                        format!("source {}", e.source.name),
                    )
                })
                .max_by_key(|(at, _)| *at);
            push(
                "sources",
                "Git repositories a watcher node turns into bundles",
                "SourceAdd",
                live_sources.len(),
                latest_source,
                true,
            );

            rows
        })
    }

    // -- M24 §4 fleet upgrade ------------------------------------------
    //
    // The binary and the definitions are two things that must never become
    // one thing. Definitions replicate through the op-log automatically;
    // binaries never do. This is the binary half, and every property of it
    // is chosen because a bad definition is recoverable and a bad binary is
    // a site visit.

    // -- M24 §3.2 bundle prefetch --------------------------------------
    //
    // Definitions replicate through the op-log automatically. Bundle
    // *content* does not: it is content-addressed and fetched lazily, by
    // the node that needs to run something. That is the right default for a
    // 200 MB Python environment nobody on `dc=west` will ever execute, and
    // it is also what makes a first run slow and unpredictable — which is
    // the `scp`-a-thousand-times complaint, precisely.
    //
    // Prefetching does not change what is authoritative. It is a
    // scheduling decision about *when* each node pulls.

    /// How much of `bundle_id` is on this node's disk right now.
    fn bundle_progress(&self, bundle_id: &BundleId) -> mesh_proto::BundleProgress {
        let meta = self
            .inner
            .engine
            .read(|s| s.bundles.get(bundle_id).map(|b| b.meta.clone()));
        let (total_chunks, total_bytes) = meta
            .map(|m| (m.chunk_count, m.total_size))
            .unwrap_or((0, 0));
        // Without the manifest there is no chunk list, so there is no
        // honest fraction to report — say zero rather than invent a
        // denominator the node cannot see yet.
        let Ok(Some(manifest)) = self.inner.content.load_manifest(bundle_id) else {
            return mesh_proto::BundleProgress {
                have_chunks: 0,
                total_chunks,
                have_bytes: 0,
                total_bytes,
                complete: false,
                failed: false,
            };
        };
        let chunks = manifest.distinct_chunks();
        let have: Vec<_> = chunks
            .iter()
            .filter(|c| self.inner.content.has_chunk(c))
            .collect();
        let have_chunks = u32::try_from(have.len()).unwrap_or(u32::MAX);
        let total_chunks = u32::try_from(chunks.len())
            .unwrap_or(u32::MAX)
            .max(total_chunks);
        // Bytes are estimated from the chunk ratio rather than stat()ed:
        // a progress bar is not worth a syscall per chunk per poll, and
        // the ratio is what a person reads anyway.
        let have_bytes = if total_chunks == 0 {
            0
        } else {
            total_bytes.saturating_mul(u64::from(have_chunks)) / u64::from(total_chunks)
        };
        let complete = have_chunks == total_chunks;
        mesh_proto::BundleProgress {
            have_chunks,
            total_chunks,
            have_bytes: if complete { total_bytes } else { have_bytes },
            total_bytes,
            complete,
            failed: false,
        }
    }

    /// Start fetching `bundle_id` in the background unless it is already
    /// here or already being fetched.
    ///
    /// Deliberately fire-and-forget. The fetch survives the asker hanging
    /// up — the same rule a drain follows, for the same reason: a node that
    /// abandons work because nobody is watching is a node whose behaviour
    /// depends on who is looking.
    fn start_prefetch(&self, bundle_id: BundleId) {
        if self.inner.content.has_bundle(&bundle_id) {
            return;
        }
        {
            let mut in_flight = self.inner.prefetching.lock();
            if !in_flight.insert(bundle_id) {
                return; // a second ask joins the fetch already running
            }
        }
        let engine = Arc::clone(&self.inner.engine);
        let transport = Arc::clone(&self.inner.transport);
        let content = self.inner.content.clone();
        let me = self.node_id();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let gossip = Arc::clone(&inner.gossip);
            match content_sync::ensure_bundle(
                &engine,
                &transport,
                &content,
                me,
                &bundle_id,
                &[],
                &move |id| gossip.liveness_of(&id) == Liveness::Alive,
            )
            .await
            {
                Ok(()) => {
                    info!(bundle = %bundle_id.short(), "prefetched bundle");
                    // Announce the holding so the next node to need it can
                    // fetch from here — a prefetch that does not widen the
                    // holder set has helped exactly one machine.
                    if let Err(e) = engine.author_op(OpBody::BundleHold {
                        bundle_id,
                        node_id: me,
                    }) {
                        warn!(bundle = %bundle_id.short(), error = %e, "could not announce prefetched bundle");
                    }
                }
                Err(e) => warn!(bundle = %bundle_id.short(), error = %e, "prefetch failed"),
            }
            inner.prefetching.lock().remove(&bundle_id);
        });
    }

    /// Ask every node a selector picks out to fetch a bundle now, and
    /// stream what each reports (M24 §3.2).
    ///
    /// One round of asks, then polls until every node is done or the
    /// deadline passes. A node that cannot be reached — or that predates
    /// [`mesh_proto::FLEET_MINOR`] — is reported as *not pushed to*
    /// rather than as failed: it still has the content available to it the
    /// moment it needs it, which is what the lazy default always gave.
    async fn prefetch_stream(
        &self,
        bundle_id: BundleId,
        selector: String,
        out: mpsc::Sender<IpcResponse>,
    ) {
        if self
            .inner
            .engine
            .read(|s| !s.bundles.contains_key(&bundle_id))
        {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!(
                        "no bundle {} in this mesh — publish it before pushing it",
                        bundle_id.short()
                    ),
                })
                .await;
            return;
        }
        let sel = Selector::parse(&selector);
        let me = self.node_id();
        let targets = self.inner.engine.read(|s| {
            membership::resolve_targets(s, &sel, me, |id| self.inner.gossip.liveness_of(id))
        });
        if targets.is_empty() {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!("selector {selector:?} matches no live node"),
                })
                .await;
            return;
        }

        let deadline = Instant::now() + PREFETCH_DEADLINE;
        let mut pending: Vec<MemberInfo> = targets;
        let mut complete = 0u32;
        let mut skipped = 0u32;
        loop {
            let mut still_going: Vec<MemberInfo> = Vec::new();
            for member in pending {
                let (progress, unreachable) = if member.node_id == me {
                    self.start_prefetch(bundle_id);
                    (self.bundle_progress(&bundle_id), None)
                } else {
                    match self.ask_prefetch(&member, bundle_id).await {
                        Ok(p) => (p, None),
                        Err(e) => (
                            mesh_proto::BundleProgress {
                                have_chunks: 0,
                                total_chunks: 0,
                                have_bytes: 0,
                                total_bytes: 0,
                                complete: false,
                                failed: true,
                            },
                            Some(e),
                        ),
                    }
                };
                let done = progress.complete;
                let gave_up = unreachable.is_some();
                if out
                    .send(IpcResponse::PrefetchProgress {
                        node: member.name.clone(),
                        progress,
                        unreachable,
                    })
                    .await
                    .is_err()
                {
                    return; // the caller hung up; the fetches continue
                }
                if done {
                    complete += 1;
                } else if gave_up {
                    skipped += 1;
                } else {
                    still_going.push(member);
                }
            }
            pending = still_going;
            if pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(PREFETCH_POLL).await;
        }
        let _ = out
            .send(IpcResponse::PrefetchDone {
                complete,
                fetching: u32::try_from(pending.len()).unwrap_or(u32::MAX),
                skipped,
            })
            .await;
    }

    /// One `BundleFetch` round trip, with the version gate stated as a
    /// sentence rather than a dropped stream.
    async fn ask_prefetch(
        &self,
        member: &MemberInfo,
        bundle_id: BundleId,
    ) -> std::result::Result<mesh_proto::BundleProgress, String> {
        let conn = self
            .inner
            .transport
            .member_conn(member.node_id, &member.addrs)
            .await
            .map_err(|e| e.to_string())?;
        // Checked *after* the dial, because the handshake is what
        // establishes the peer's minor — and stated as a sentence, because
        // a pre-1.11 peer would simply drop the stream and the asker would
        // learn nothing.
        let minor = self
            .inner
            .transport
            .peer_minor(&member.node_id)
            .unwrap_or(0);
        if minor < mesh_proto::FLEET_MINOR {
            return Err(format!(
                "runs an older build (protocol 1.{minor}) — it will fetch when it needs the bundle"
            ));
        }
        match self
            .inner
            .transport
            .request_one(&conn, &Request::BundleFetch { bundle_id })
            .await
        {
            Ok(Response::BundleProgress(p)) => Ok(p),
            Ok(Response::Error { message }) => Err(message),
            Ok(other) => Err(format!("unexpected answer: {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    }

    fn bundle_rows(&self) -> Vec<BundleRow> {
        let me = self.node_id();
        self.inner.engine.read(|s| {
            let mut rows: Vec<BundleRow> = s
                .bundles
                .values()
                .map(|b| BundleRow {
                    bundle_id: b.meta.bundle_id,
                    total_size: b.meta.total_size,
                    file_count: b.meta.file_count,
                    chunk_count: b.meta.chunk_count,
                    holders: b.holders.len(),
                    local: b.holders.contains(&me),
                })
                .collect();
            rows.sort_by_key(|r| r.bundle_id);
            rows
        })
    }

    fn service_rows(&self) -> Vec<ServiceRow> {
        self.inner.engine.read(|s| {
            let mut rows: Vec<ServiceRow> = s
                .services
                .values()
                .filter_map(|e| e.current().map(|def| service_row(def, e)))
                .collect();
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            rows
        })
    }

    fn service_row(&self, name: &str) -> Option<ServiceRow> {
        self.inner.engine.read(|s| {
            s.services
                .get(name)
                .and_then(|e| e.current().map(|def| service_row(def, e)))
        })
    }

    fn status(&self) -> StatusInfo {
        // **One census, counted the way every other fleet surface counts.**
        //
        // `top_rows` and `totals` already answer "which machines do I have"
        // over live, unrevoked members; this pair is the headline version
        // of the same question and has to fold over the same rows. It did
        // not: `total` kept **revoked** members, so retiring a node left
        // "4 / 5" on the screen for ever — and revocation is precisely the
        // remedy an operator reaches for when the count looks wrong. The
        // Prometheus gauge below has always described itself as "not left,
        // not revoked"; only the code disagreed.
        let (total, alive) = self.inner.engine.read(|s| {
            let census = || {
                s.members
                    .values()
                    .filter(|m| !m.left && !s.revoked.contains_key(&m.info.node_id))
            };
            let alive = census()
                .filter(|m| {
                    m.info.node_id == self.node_id()
                        || matches!(
                            self.inner.gossip.liveness_of(&m.info.node_id),
                            Liveness::Alive
                        )
                })
                .count();
            (census().count(), alive)
        });
        let genesis = self.inner.engine.genesis();
        // HLC wall = max(local wall, freshest remote wall seen): how far the
        // most skewed peer clock runs ahead of ours (MASTERPLAN §12 doctor).
        let hlc_wall_ahead_micros = self
            .inner
            .engine
            .clock()
            .now()
            .wall_micros
            .saturating_sub(mesh_proto::now_micros());
        StatusInfo {
            // T4. From the same list the `cap.rt` tag is built from, so
            // what `status` says and what the scheduler believes about this
            // node are the same sentence.
            runtimes: self.inner.jobs.ready_runtimes(),
            store_failure: self.inner.engine.store_failure(),
            contained_panics: self.inner.engine.contained_panics(),
            version: VERSION.to_string(),
            proto_major: mesh_proto::PROTOCOL_MAJOR,
            proto_minor: mesh_proto::PROTOCOL_MINOR,
            node_id: self.node_id(),
            node_name: self.inner.config.node_name.clone(),
            mesh_id: Some(genesis.body.mesh_id),
            mesh_name: Some(genesis.body.mesh_name.clone()),
            listen_addr: self.inner.listen_addr.to_string(),
            uptime_secs: self.inner.started.elapsed().as_secs(),
            members_alive: alive,
            members_total: total,
            jobs_running: self.inner.jobs.running_count(),
            slots: self.inner.jobs.limits().slots as usize,
            jobs_queued: self.inner.jobs.queued_count(),
            draining: self.is_draining(),
            hlc_wall_ahead_micros,
            wan: Some(self.inner.wan.info()),
            quiet: self.quiet_info(),
            founder_node: self.inner.engine.genesis().body.founder_node,
            drain: self.drain_info(),
            placement: self.placement(),
            dispatch: {
                let (count, p50_secs, p95_secs) = self.inner.dispatch.summary();
                // `None` until this node has dispatched something, rather
                // than zeroes: a node that has placed nothing has no
                // latency, and "0ms" would read as "instant".
                (count > 0).then_some(mesh_proto::DispatchInfo {
                    count,
                    p50_secs,
                    p95_secs,
                })
            },
        }
    }

    /// The timed drain in force, for `status`.
    fn drain_info(&self) -> Option<mesh_proto::DrainInfo> {
        self.inner
            .drain_until
            .lock()
            .as_ref()
            .map(|(until, reason)| mesh_proto::DrainInfo {
                until_micros: *until,
                reason: reason.clone(),
            })
    }

    /// A drain with an end (`meshnode drain --until` / `--for`): the same
    /// refusal a permanent drain makes — the `draining` tag mesh-wide and
    /// `Busy` to every offer — without the exit at the end. Work already
    /// running is never touched; a cap is a promotion gate, and this is a
    /// cap of zero for a while.
    ///
    /// The deadline lives in memory only. A restarted node is not
    /// draining, which is the rule the permanent drain already follows.
    async fn drain_timed(
        &self,
        until_micros: Option<u64>,
        reason: String,
        move_pending: bool,
    ) -> IpcResponse {
        let Some(until) = until_micros else {
            return self.lift_drain();
        };
        let now = mesh_proto::now_micros();
        if until <= now {
            return IpcResponse::Error {
                message: "the drain would end before it began — give a time in the future".into(),
            };
        }
        // A permanent drain in progress owns the flag; a timed one must not
        // be able to lift it from under it.
        {
            let mut slot = self.inner.drain_until.lock();
            if self.inner.draining.load(Ordering::SeqCst) && slot.is_none() {
                return IpcResponse::Error {
                    message: "this node is already draining for good; a timed drain cannot \
                              shorten that"
                        .into(),
                };
            }
            *slot = Some((until, reason.clone()));
        }
        let first = !self.inner.draining.swap(true, Ordering::SeqCst);
        if first {
            // Instances move by the end of the window or the default drain
            // deadline, whichever comes first (Phase C, E9).
            let by = now.saturating_add(mesh_proto::DEFAULT_DRAIN_DEADLINE_SECS * 1_000_000);
            self.announce_draining(Some(until.min(by)));
        }
        let moved = if move_pending {
            self.move_queued().await
        } else {
            0
        };
        info!(until, reason, moved, "timed drain started");
        // The lift is a task that sleeps to the deadline and then checks
        // the deadline is still the one it was started for — a later
        // `--until` replaces it, and a `--lift` empties it, and either
        // makes this task a no-op.
        let this = self.clone();
        let wait = Duration::from_micros(until.saturating_sub(now));
        let mut shutdown = self.inner.shutdown_tx.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => return,
            }
            let still = this
                .inner
                .drain_until
                .lock()
                .as_ref()
                .is_some_and(|(u, _)| *u == until);
            if still {
                let _ = this.lift_drain();
            }
        });
        IpcResponse::Notice {
            message: format!(
                "draining until {} — running jobs finish, new work goes elsewhere{}; \
                 the node rejoins on its own (`meshnode drain --lift` ends it early)",
                crate::localtime::describe_instant(until),
                if moved > 0 {
                    format!(" ({moved} queued job(s) moved)")
                } else {
                    String::new()
                }
            ),
        }
    }

    /// End a timed drain now: clear the flag, drop the tag, take work again.
    fn lift_drain(&self) -> IpcResponse {
        let had = self.inner.drain_until.lock().take();
        let Some((_, reason)) = had else {
            return if self.inner.draining.load(Ordering::SeqCst) {
                IpcResponse::Error {
                    message: "this node is draining for good, not for a while — there is \
                              nothing to lift"
                        .into(),
                }
            } else {
                IpcResponse::Notice {
                    message: "not draining".into(),
                }
            };
        };
        self.inner.draining.store(false, Ordering::SeqCst);
        self.announce_undrained();
        info!(reason, "timed drain lifted");
        IpcResponse::Notice {
            message: "drain lifted — this node takes work again".into(),
        }
    }

    /// The opposite of `announce_draining`: the reserved tag comes off, and
    /// the deadline with it (Phase C, E9).
    fn announce_undrained(&self) {
        let me = self.node_id();
        let Some((info, announced)) = self.inner.engine.read(|s| {
            s.members
                .get(&me)
                .map(|m| (m.info.clone(), m.announced_tags.clone()))
        }) else {
            return;
        };
        let mut tags = announced;
        tags.retain(|t| t != mesh_proto::DRAINING_TAG && !mesh_proto::is_drain_tag(t));
        if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
            node_id: me,
            name: info.name,
            addrs: info.addrs,
            tags,
            slots: info.slots,
        }) {
            warn!("cannot announce the end of a drain: {e}");
        }
    }

    /// Quiet hours as `status` reports them, or `None` on a node with none
    /// configured — which is every node that has not asked for any.
    fn quiet_info(&self) -> Option<mesh_proto::QuietInfo> {
        let windows = self.inner.jobs.quiet();
        if windows.is_empty() {
            return None;
        }
        let state = self.inner.jobs.quiet_state();
        Some(mesh_proto::QuietInfo {
            windows: windows.describe(),
            zone: crate::localtime::LocalZone::local().name.clone(),
            effective_slots: self.inner.jobs.effective_slots() as usize,
            pct: state.map(|s| s.window.pct),
            until: state.map(|s| crate::localtime::hhmm(s.until)),
        })
    }

    // -- observability (MASTERPLAN §12) -------------------------------------

    // -- config hot-reload (MASTERPLAN §12) ----------------------------------

    /// Apply a re-resolved configuration in place (SIGHUP). Returns
    /// human-readable notes on what changed and what needs a restart.
    pub fn apply_config(&self, new: &NodeConfig) -> Vec<String> {
        let mut notes = Vec::new();
        // One reload at a time: each diffs against what the last one left.
        let _reloading = self.inner.reloading.lock();
        let old = self.live_config();
        // What this reload applies, section by section; everything else
        // stays as it is running.
        let mut applied = (*old).clone();
        applied.log_level = new.log_level.clone();

        if new.quiet != self.inner.jobs.quiet() {
            self.inner.jobs.set_quiet(new.quiet.clone());
            notes.push(format!("quiet hours: {}", new.quiet.describe()));
        }
        applied.quiet = new.quiet.clone();

        // The DNS listener is bound once, at boot; the rest of `[network]`
        // reloads.
        let mut network = new.network.clone();
        network.dns = old.network.dns;
        network.dns_bind = old.network.dns_bind;
        if network != old.network {
            self.inner
                .jobs
                .set_network(network.clone(), advertise_host(new), job_bind_host(new));
            notes.push(match &new.network.dynamic {
                Some(r) => format!(
                    "ports: {}-{}{}",
                    r.min,
                    r.max,
                    if new.network.reserved.is_empty() {
                        String::new()
                    } else {
                        format!(", {} reserved", new.network.reserved.len())
                    }
                ),
                // Worth a note rather than silence: this node has just
                // stopped being a candidate for anything that wants a port.
                None => "ports: none advertised — this node will take no work \
                         that needs one"
                    .to_string(),
            });
        }
        // Moving mesh traffic to another NIC has to reach the mesh, or the
        // reload changed a file and nothing else (ENG-22, and ENG-11's
        // rule: a reload note must describe what was actually applied).
        // Re-announced below, once `applied` is live, because the announce
        // reads the config this call is in the middle of installing.
        let readvertise = network.advertise_interfaces != old.network.advertise_interfaces;
        if readvertise {
            notes.push(if network.advertise_interfaces.is_empty() {
                "addresses: advertising every interface".to_string()
            } else {
                format!(
                    "addresses: advertising {}",
                    network.advertise_interfaces.join(", ")
                )
            });
        }
        applied.network = network;

        applied.runtime.container_plugin = new.runtime.container_plugin.clone();
        applied.runtime.sandbox = new.runtime.sandbox.clone();
        if new.runtime.container_plugin != old.runtime.container_plugin {
            self.inner
                .jobs
                .set_container_plugin(new.runtime.container_plugin.clone());
            notes.push(match &new.runtime.container_plugin {
                Some(p) => format!("container plugin: {p}"),
                // Worth a note rather than silence: this node has just
                // stopped being able to run a whole runtime, and every
                // container job will now spill past it.
                None => "container plugin: none — this node runs no containers".to_string(),
            });
        }

        if new.runtime.sandbox != old.runtime.sandbox {
            self.inner.jobs.set_sandbox(new.runtime.sandbox.clone());
            notes.push(format!(
                "job sandbox: {:?}{}",
                new.runtime.sandbox.mode,
                match crate::sandbox::unavailable_reason(&new.runtime.sandbox) {
                    Some(why) if new.runtime.sandbox.mode != crate::sandbox::Mode::Off => {
                        format!(" (but {why})")
                    }
                    _ => String::new(),
                }
            ));
        }

        // `[compute] placement` is the **one** field in this section that
        // genuinely reloads (N6 §12), and it does because it is read from a
        // live holder at each dispatch rather than out of the boot config.
        //
        // Applied first, and *removed from the comparison below*, so that
        // the "NOT applied" warning keeps being true about the fields it is
        // actually about. A blanket warning that fired on a setting which
        // had just been applied would be the same defect pointing the other
        // way.
        let placement_changed = new.compute.placement != old.compute.placement;
        applied.compute.placement = new.compute.placement;
        if placement_changed {
            *self.inner.placement.lock() = new.compute.placement;
            notes.push(format!(
                "placement: {} — {}",
                new.compute.placement.as_str(),
                match new.compute.placement {
                    mesh_proto::FitPolicy::Pack => "fill a machine before starting the next one",
                    mesh_proto::FitPolicy::Spread => "keep every machine cool",
                }
            ));
        }

        // The rest of `[compute]` documents itself as hot-reloadable except
        // for the RAM budget. None of it actually reloads: the straggler
        // policy is read from `inner.config` on every dispatch and
        // `inner.config` is fixed at startup. An operator who edited
        // `speculate` and sent SIGHUP got no error, no note and no change —
        // the worst of the three.
        //
        // Saying so is not the fix, but it is the difference between a
        // setting that does nothing and a setting that does nothing *and
        // tells you*. `placement` above is what the fix looks like when it
        // is done: a live holder the dispatch path reads, not a field this
        // function copies.
        if compute_without_placement(&new.compute) != compute_without_placement(&old.compute) {
            // A note is logged at info, and a node at the default log level
            // never shows info. A setting the operator changed and the node
            // ignored is a warning by any reading, so it is emitted as one
            // as well as returned — being quiet about this is the whole
            // defect.
            warn!(
                "[compute] changed in the config file but was NOT applied — it is \
                 read once at startup. Restart the node for `speculate` and the rest \
                 of that section to take effect."
            );
            notes.push(
                "[compute] changed — NOT applied. Every setting in this section except \
                 `placement` is read once at startup, including `speculate`; restart \
                 the node for it to take effect"
                    .to_string(),
            );
        }

        let current_limits = self.inner.jobs.limits();
        if new.limits.slots != current_limits.slots
            || new.limits.queue != current_limits.queue
            || new.limits.min_free_mem_mb != current_limits.min_free_mem_mb
        {
            self.inner.jobs.set_limits(
                new.limits,
                capacity::of(new, &self.inner.jobs.ready_runtimes()),
            );
            applied.limits = new.limits;
            notes.push(format!(
                "limits: slots {} queue {} min_free_mem {}MB",
                new.limits.slots, new.limits.queue, new.limits.min_free_mem_mb
            ));
            // Advertised capacity changed: re-announce.
            if new.limits.slots != current_limits.slots {
                let me = self.node_id();
                if let Some(info) = self
                    .inner
                    .engine
                    .read(|s| s.members.get(&me).map(|m| m.info.clone()))
                    && let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
                        node_id: me,
                        name: info.name,
                        addrs: info.addrs,
                        tags: info.tags,
                        slots: new.limits.slots,
                    })
                {
                    warn!("cannot re-announce slots: {e}");
                }
            }
        }

        {
            applied.tags = new.tags.clone();
            if new.tags != old.tags {
                let me = self.node_id();
                if let Some(info) = self
                    .inner
                    .engine
                    .read(|s| s.members.get(&me).map(|m| m.info.clone()))
                {
                    // Preserve runtime-managed reserved tags. Capacity
                    // (M9 §5.1) is preserved by prefix rather than by exact
                    // match, so an operator's config edit can never drop
                    // what hardware the node reports; the block below
                    // re-probes it.
                    let mut tags = mesh_proto::user_tags(&new.tags);
                    for reserved in [
                        mesh_proto::NO_EXECUTE_TAG,
                        mesh_proto::DRAINING_TAG,
                        mesh_proto::RELAY_TAG,
                    ] {
                        if info.tags.iter().any(|t| t == reserved)
                            && !tags.iter().any(|t| t == reserved)
                        {
                            tags.push(reserved.to_string());
                        }
                    }
                    notes.push(format!("tags: {}", tags.join(",")));
                    tags.extend(
                        info.tags
                            .iter()
                            .filter(|t| mesh_proto::is_capacity_tag(t))
                            .cloned(),
                    );
                    if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
                        node_id: me,
                        name: info.name,
                        addrs: info.addrs,
                        tags,
                        slots: info.slots,
                    }) {
                        warn!("cannot re-announce tags: {e}");
                    }
                }
            }
        }

        // `[sessions]` reloads whole (ENG-11): every check reads it at the
        // moment a terminal is asked for, and the announcement below
        // follows `accept`.
        applied.sessions = new.sessions;
        if new.sessions != old.sessions && new.sessions.accept == old.sessions.accept {
            notes.push(format!(
                "terminals: at most {} per peer, recording {}, root {}",
                new.sessions.max_per_peer,
                if new.sessions.record { "on" } else { "off" },
                if new.sessions.allow_root {
                    "allowed"
                } else {
                    "refused"
                }
            ));
        }

        // `[disk]` reloads too: the pressure check reads it on each pass.
        applied.disk = new.disk;
        if new.disk != old.disk {
            self.inner
                .jobs
                .set_output_limit(new.disk.job_output_max_bytes());
            notes.push(format!(
                "disk: floor {} MB / {}%, job output {}",
                new.disk.min_free_mb,
                new.disk.min_free_pct,
                match new.disk.job_output_max_mb {
                    0 => "uncapped".to_string(),
                    mb => format!("{mb} MB a stream"),
                }
            ));
        }

        // Advertised capacity is re-probed on every reload (M9 §5.1), so a
        // SIGHUP picks up hardware that changed under the node — a grown
        // volume, a resized VM. Idempotent: identical hardware authors no op.
        //
        // Terminal acceptance rides the same re-announcement (M17 §3.4). It
        // has to: `[sessions] accept` is hot-reloadable, and a node that
        // switched terminals off while still telling the mesh it accepts
        // them would leave every other node's Director offering a button
        // that is refused on arrival.
        {
            let me = self.node_id();
            // **The probes' answer, not an empty list.** `capacity::of`
            // takes the runtimes because the re-announce below rebuilds
            // this node's whole tag set from what it returns — so passing
            // nothing here would erase `cap.rt` on the first unrelated
            // `SIGHUP`, which is landmine 1 and is precisely how N5 lost
            // the volume list.
            let fresh = capacity::of(new, &self.inner.jobs.ready_runtimes());
            let accepts = new.sessions.accept;
            if let Some(info) = self
                .inner
                .engine
                .read(|s| s.members.get(&me).map(|m| m.info.clone()))
            {
                let announced = mesh_proto::Capacity::from_tags(&info.tags);
                let capacity_changed = announced.as_ref() != Some(&fresh);
                // Read off the same comparison rather than its own, so the
                // note below can never disagree with the re-announce above
                // it about whether the volume list moved.
                let volumes_changed = announced
                    .as_ref()
                    .map(|a| a.volumes != fresh.volumes)
                    .unwrap_or(!fresh.volumes.is_empty());
                let terminals_changed = mesh_proto::accepts_terminals(&info.tags) != accepts;
                // A node that swapped its binary and came back must say so
                // — that announcement is the whole of "which of my nodes
                // are outdated" (M24 §4). Boot re-announces unconditionally,
                // so this is the belt to that braces.
                let version_changed =
                    mesh_proto::version_of(&info.tags).as_deref() != Some(crate::capacity::VERSION);
                if capacity_changed || terminals_changed || version_changed {
                    if version_changed {
                        notes.push(format!("version: {}", crate::capacity::VERSION));
                    }
                    if capacity_changed {
                        notes.push(format!(
                            "capacity: {} cores / {} MB RAM / {} MB disk",
                            fresh.cores, fresh.mem_mb, fresh.disk_mb
                        ));
                    }
                    // Said separately, because the line above would not
                    // mention it and a `[[volume]]` edit changes nothing
                    // else. Worth naming in both directions: gaining one
                    // makes this node a candidate for work it could not take
                    // before, and losing one stops it being one — silently,
                    // since a job already holding the volume keeps running.
                    if volumes_changed {
                        notes.push(if fresh.volumes.is_empty() {
                            "volumes: none lent — this node will take no work \
                             that claims one"
                                .to_string()
                        } else {
                            format!("volumes: {}", fresh.volumes.join(", "))
                        });
                    }
                    if terminals_changed {
                        notes.push(format!(
                            "terminals: {}",
                            if accepts { "accepted" } else { "refused" }
                        ));
                    }
                    let tags = capacity::with_capacity_and_devices(
                        &info.tags,
                        &fresh,
                        accepts,
                        &self.inner.jobs.devices(),
                    );
                    if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
                        node_id: me,
                        name: info.name,
                        addrs: info.addrs,
                        tags,
                        slots: info.slots,
                    }) {
                        warn!("cannot re-announce this node: {e}");
                    }
                }
            }
        }

        // `[wan]` is fully hot-reloadable: the WAN task re-evaluates roles
        // and re-announces candidates at once (MASTERPLAN M7).
        applied.wan = new.wan.clone();
        if new.wan != self.inner.wan.config() {
            notes.push(format!(
                "wan: advertise {:?} relay {} relay_client {}",
                new.wan.advertise,
                if new.wan.relay { "on" } else { "off" },
                if new.wan.relay_client { "auto" } else { "off" }
            ));
            self.inner.wan.set_config(new.wan.clone());
        }

        applied.retention = new.retention;
        {
            let mut retention = self.inner.retention.lock();
            if new.retention != *retention {
                *retention = new.retention;
                notes.push(format!(
                    "retention: jobs {} days / {} MB, content {}",
                    new.retention.job_days,
                    new.retention.job_max_bytes / (1024 * 1024),
                    new.retention.content
                ));
            }
        }

        let providers_changed = {
            let old_names: Vec<&String> = old.secret_providers.keys().collect();
            let new_names: Vec<&String> = new.secret_providers.keys().collect();
            old_names != new_names
                || old
                    .secret_providers
                    .iter()
                    .zip(new.secret_providers.iter())
                    .any(|((_, a), (_, b))| a.plugin != b.plugin || a.config != b.config)
        };
        applied.secret_providers = new.secret_providers.clone();
        if providers_changed {
            self.inner.jobs.set_providers(new.secret_providers.clone());
            notes.push(format!("secret providers: {}", new.secret_providers.len()));
        }

        for (changed, what) in [
            (new.listen != old.listen, "listen address"),
            (
                new.network.dns != old.network.dns || new.network.dns_bind != old.network.dns_bind,
                "the DNS listener",
            ),
            (new.data_dir != old.data_dir, "data_dir"),
            (new.node_name != old.node_name, "node_name"),
            (new.http_addr != old.http_addr, "http endpoint"),
            (
                new.log.dir != old.log.dir || new.log.json != old.log.json,
                "log destination/format",
            ),
        ] {
            if changed {
                notes.push(format!("{what} changed — restart required to apply"));
            }
        }
        *self.inner.live.write() = Arc::new(applied);
        if readvertise {
            self.reannounce_addrs();
        }
        notes
    }

    /// Re-announce this node's addresses after `[network]
    /// advertise_interfaces` changed (ENG-22).
    ///
    /// The same shape as [`Self::reannounce_device_tags`], and skipped
    /// when nothing moved for the same reason: a `MemberUpdate` per
    /// SIGHUP would put an op in every node's log for a reload that
    /// changed nothing about where this node can be reached.
    fn reannounce_addrs(&self) {
        let me = self.node_id();
        let Some(info) = self
            .inner
            .engine
            .read(|s| s.members.get(&me).map(|m| m.info.clone()))
        else {
            return;
        };
        let live = self.live_config();
        let mut addrs = advertised_addrs(
            live.listen.ip(),
            self.inner.listen_addr.port(),
            &live.network.advertise_interfaces,
        );
        // Operator-declared public endpoints outlive the interface list:
        // `[wan] advertise` is a statement about the internet, not about a
        // NIC, and dropping it here would take a NATed mesh's rendezvous
        // address away on an unrelated reload.
        for a in &live.wan.advertise {
            if !addrs.contains(a) {
                addrs.push(a.clone());
            }
        }
        if addrs == info.addrs {
            return;
        }
        info!(addrs = ?addrs, "re-announcing this node's addresses");
        if let Err(e) = self.inner.engine.author_op(OpBody::MemberUpdate {
            node_id: me,
            name: info.name,
            addrs,
            tags: info.tags,
            slots: info.slots,
        }) {
            warn!("cannot re-announce this node's addresses: {e}");
        }
    }

    fn identity_info(&self) -> IdentityInfo {
        let genesis = self.inner.engine.genesis();
        IdentityInfo {
            node_id: self.node_id(),
            node_name: self.inner.config.node_name.clone(),
            // `IdentityInfo::user_id` is a protocol field and a String, so
            // "no key here" is said in words rather than by changing the
            // wire type for it. A node that did not found the mesh, or whose
            // key is sealed, genuinely has no user identity of its own.
            user_id: match self.inner.identity.user.as_ref() {
                Some(k) => NodeId(k.public_bytes()).to_display_string(),
                None => "(none on this node — sealed, or not the founder)".to_string(),
            },
            mesh_id: Some(genesis.body.mesh_id),
            mesh_name: Some(genesis.body.mesh_name.clone()),
            data_dir: self.inner.config.data_dir.display().to_string(),
        }
    }

    /// Ping a member by id. The addresses come from its own announcement
    /// and the route is the dialer's to choose — a caller that had to name
    /// an address would be guessing at loopback (§ [`Transport::ping_member`]).
    async fn ipc_ping_node(&self, node: NodeId) -> IpcResponse {
        if node == self.node_id() {
            return IpcResponse::Error {
                message: "that is this node — there is no round trip to make".into(),
            };
        }
        let member = self
            .inner
            .engine
            .read(|s| s.members.get(&node).map(|m| m.info.clone()));
        let Some(info) = member else {
            return IpcResponse::Error {
                message: format!("unknown node {}", node.short()),
            };
        };
        // No announced address is only fatal when nothing is open either: a
        // peer that dialed us (the NATed case) is reachable over its own
        // inbound connection, which `member_conn` will find.
        if info.addrs.is_empty() && self.inner.transport.pooled_conn(&node).is_none() {
            return IpcResponse::Error {
                message: format!("{} has announced no dialable address yet", info.name),
            };
        }
        let genesis = self.inner.engine.genesis();
        match self.inner.transport.ping_member(node, &info.addrs).await {
            Ok((rtt_micros, via)) => IpcResponse::Ping(PingResult {
                peer_node_id: node,
                peer_node_name: info.name,
                peer_mesh_id: Some(genesis.body.mesh_id),
                peer_mesh_name: Some(genesis.body.mesh_name.clone()),
                // The dialer pins the peer's key and this record is in our
                // own replicated state: a member that answers is in our mesh.
                same_mesh: true,
                rtt_micros,
                via,
            }),
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        }
    }

    /// Repair another member's recorded addresses (founder only, M7).
    ///
    /// The merge rule already permits this — `MemberUpdate` admits an op
    /// about node N from N *or from the founder* — and this is the one
    /// situation the exception was written for: every recorded address in
    /// the mesh went stale at once, so no node can reach any other, and
    /// the self-healing path (each node re-announcing itself on boot)
    /// never gets a chance to run because it has nowhere to run *to*.
    ///
    /// What this authors is deliberately weak. It is a first-contact
    /// address, overwritten by the subject's own announcement the moment
    /// the two can talk — which is why it needs no ceremony beyond being
    /// the founder, and why it is not a way to lie about a fleet: the lie
    /// survives exactly until the node it is about answers for itself.
    fn ipc_node_addr_repair(
        &self,
        user: &UserId,
        node_id: NodeId,
        addrs: Vec<String>,
        env: &SignedIpc,
    ) -> IpcResponse {
        let err = |message: String| IpcResponse::Error { message };

        // Only the founder's op survives the merge. Anywhere else this
        // would author an op that every replica — including this one —
        // ignores, which is the worst possible outcome: a command that
        // reports success and changes nothing. Refuse, and say where to go.
        let founder = self.inner.engine.genesis().body.founder_node;
        let me = self.node_id();
        if me != founder {
            return err(format!(
                "only the founder node may repair another node's record, and this is {}. \
                 Run this on {} ({}).",
                me.short(),
                self.member_name(&founder),
                founder.short()
            ));
        }
        if node_id == me {
            return err(
                "a node speaks for itself: this would be overwritten by the next \
                 announcement. Set `[wan] advertise` in meshnode.toml and reload instead."
                    .into(),
            );
        }

        let Some(info) = self
            .inner
            .engine
            .read(|s| s.members.get(&node_id).map(|m| m.info.clone()))
        else {
            return err(format!("{} is not a member of this mesh", node_id.short()));
        };
        if self.inner.engine.read(|s| s.revoked.contains_key(&node_id)) {
            return err(format!(
                "{} is revoked; revocation is terminal",
                node_id.short()
            ));
        }

        let mut cleaned: Vec<String> = Vec::with_capacity(addrs.len());
        for a in &addrs {
            let a = a.trim();
            if a.is_empty() {
                continue;
            }
            // By prefix, not by `parse_relay_candidate`: a `relay:` entry
            // whose id does not parse is still an operator reaching for
            // the wrong thing, and telling them it "must end in a port
            // number" sends them off to fix the id.
            if a.starts_with(mesh_proto::RELAY_CANDIDATE_PREFIX) {
                return err(format!(
                    "{a:?} is a relay candidate; a node announces its own relays. \
                     Give a host:port this node answers on."
                ));
            }
            if let Some(why) = host_port_problem(a) {
                return err(format!("{a:?} {why}"));
            }
            if !cleaned.iter().any(|e| e == a) {
                cleaned.push(a.to_string());
            }
        }
        if cleaned.is_empty() {
            return err("give at least one host:port address".into());
        }

        let detail = cleaned.join(",");
        match self.inner.engine.author_op(OpBody::MemberUpdate {
            node_id,
            name: info.name,
            addrs: cleaned,
            tags: info.tags,
            slots: info.slots,
        }) {
            Ok(_) => {
                // Forget any connection we were holding for this peer, so
                // the next gossip tick (one second) dials the repaired
                // address rather than a route built from the stale one.
                // The negative dial cache needs no clearing: it keys on the
                // address set, and a changed set is a different key.
                self.inner.transport.drop_conn(&node_id);
                self.audit_ok(
                    user,
                    "node.addr",
                    &node_id.to_display_string(),
                    &detail,
                    env,
                );
                IpcResponse::Ok
            }
            Err(e) => err(e.to_string()),
        }
    }

    async fn ipc_ping(&self, addr: &str) -> IpcResponse {
        let sa = match crate::transport::resolve(addr).await {
            Ok(sa) => sa,
            Err(e) => {
                return IpcResponse::Error {
                    message: e.to_string(),
                };
            }
        };
        match self.inner.transport.ping(sa).await {
            Ok(r) => IpcResponse::Ping(r),
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        }
    }

    // -- parallel compute (MASTERPLAN M9 §5.4) ------------------------------

    // -- M18 facts (§4.9) ---------------------------------------------------

    /// Validate a client-signed collector and replicate it.
    ///
    /// Validation happens **here**, at publish, with the fix inline —
    /// M10's validator standard (§9). A collector that cannot produce a
    /// document is refused while somebody is watching, rather than failing
    /// silently on a thousand nodes an hour later.
    fn publish_collector(&self, collector: mesh_proto::FactCollector) -> Result<()> {
        crate::facts::validate_collector(&collector)?;
        // The same probe the run loop does, so an unrunnable `run` is a
        // refusal now rather than a warning in a log nobody reads.
        self.collector_payload(&collector)?;
        // An edited collector must not be judged on what the old one
        // printed, or a legitimate rewrite would read as flapping.
        self.inner
            .flap
            .forget_prefix(&format!("{}\u{1}", collector.name));
        self.inner.engine.author_op(OpBody::FactCollectorPut {
            collector: Box::new(collector),
        })?;
        Ok(())
    }

    fn collector_rows(&self) -> Vec<mesh_proto::FactCollectorRow> {
        let flapping: BTreeMap<String, Vec<mesh_proto::FactChange>> = self
            .inner
            .flap
            .flapping()
            .into_iter()
            .map(|(key, keys)| {
                let name = key.split('\u{1}').next().unwrap_or(&key).to_string();
                (name, keys)
            })
            .collect();
        self.inner.engine.read(|s| {
            let count = |name: &str| {
                u32::try_from(s.facts.values().filter(|by| by.contains_key(name)).count())
                    .unwrap_or(u32::MAX)
            };
            // The built-in leads, because it is the one every node has and
            // the one somebody reading this list is trying to account for.
            let mut rows = vec![mesh_proto::FactCollectorRow {
                name: mesh_proto::SYSTEM_COLLECTOR.to_string(),
                run: "(built in)".to_string(),
                on: "any".to_string(),
                interval_secs: mesh_proto::SYSTEM_INTERVAL_SECS,
                subject: mesh_proto::FactSubjectKind::Node.as_str().to_string(),
                paused: false,
                built_in: true,
                description: "OS, kernel, CPU, memory, disk and boot identity, read \
                              in process on every node"
                    .to_string(),
                created_by: mesh_proto::UserId(self.inner.engine.genesis().body.founder_user),
                created_at_micros: 0,
                subjects: count(mesh_proto::SYSTEM_COLLECTOR),
                flapping_keys: Vec::new(),
                source: String::new(),
            }];
            rows.extend(s.collectors.values().map(|e| {
                let c = &e.collector;
                mesh_proto::FactCollectorRow {
                    name: c.name.clone(),
                    run: describe_collector_action(&c.run),
                    on: c.on.clone(),
                    interval_secs: c.interval(),
                    subject: c.subject.as_str().to_string(),
                    paused: c.paused,
                    built_in: false,
                    description: c.description.clone(),
                    created_by: c.created_by,
                    created_at_micros: c.created_at_micros,
                    subjects: count(&c.name),
                    flapping_keys: flapping
                        .get(&c.name)
                        .map(|keys| keys.iter().map(|k| k.key.clone()).collect())
                        .unwrap_or_default(),
                    source: crate::facts::to_yaml(c),
                }
            }));
            rows
        })
    }

    // -- M19 results (§5.3) -------------------------------------------------

    /// Answer `meshnode results <fan-id>` from this node's own index.
    ///
    /// Three answers, and the difference between them matters: rows, "that
    /// group belongs to *hermes* — ask there", and "no such fan-out". The
    /// middle one is the honest answer a decentralized system can give, and
    /// it is only useful because it carries the address.
    fn ipc_results(&self, group: mesh_proto::FanId, failed_only: bool) -> IpcResponse {
        let Some(history) = self.inner.history.as_ref() else {
            return IpcResponse::Error {
                message: "the history index is disabled on this node \
                          ([history] enabled = false), and result rows live in it"
                    .to_string(),
            };
        };
        match history.results(&group, failed_only) {
            Ok(Some(set)) => IpcResponse::Results(Box::new(set)),
            Ok(None) => {
                // Replicated state still knows who *drove* it, even on a
                // node that holds none of its rows: the plan and every
                // adoption claim are ops.
                let known = self
                    .inner
                    .engine
                    .read(|s| s.fanouts.get(&group).map(|e| (e.owner(), e.plan.submitter)));
                match known {
                    // Adopted after the origin died: the rows were teed out
                    // of a log stream this node never saw, and inventing
                    // them from job summaries would be a different answer
                    // wearing the same name (§10.3).
                    Some((owner, submitter)) if owner == self.node_id() && submitter != owner => {
                        IpcResponse::Error {
                            message: format!(
                                "this node adopted {} after {} stopped answering, so it holds \
                                 no rows for it — the outputs were captured on the origin as \
                                 the logs streamed past, and that stream is gone",
                                group.short(),
                                self.member_name(&submitter)
                            ),
                        }
                    }
                    Some((owner, _)) if owner == self.node_id() => IpcResponse::Error {
                        message: format!(
                            "this node owns {} but holds no rows for it — it was run \
                             without --json/--outputs/--expect, so nothing was captured",
                            group.short()
                        ),
                    },
                    Some((owner, _)) => IpcResponse::Error {
                        message: format!(
                            "{} is owned by {} — run this there: \
                             meshnode results {}",
                            group.short(),
                            self.member_name(&owner),
                            group.to_display_string()
                        ),
                    },
                    None => IpcResponse::Error {
                        message: format!("no fan-out {} is known here", group.short()),
                    },
                }
            }
            Err(e) => IpcResponse::Error {
                message: format!("reading results failed: {e}"),
            },
        }
    }

    /// Store one fan-out's rows locally. Best-effort by design: a node
    /// whose index is disabled still streams every row to the client, it
    /// just cannot answer for them an hour later.
    fn record_results(&self, set: &mesh_proto::ResultSet) {
        let Some(history) = self.inner.history.as_ref() else {
            return;
        };
        if let Err(e) = history.index_results(set) {
            warn!(group = %set.group.short(), "cannot index result rows: {e}");
        }
    }

    /// Spill one oversized output value into an ephemeral object, so a
    /// build log becomes a reference rather than a value.
    fn spill_output(&self, job_id: mesh_proto::JobId, key: &str, value: &str) -> Option<String> {
        match self.inner.objects.put(job_id, value.as_bytes()) {
            Ok(obj) => Some(outputs::object_ref(&obj.id, obj.size)),
            Err(e) => {
                warn!(job = %job_id.short(), key, "cannot store a large output: {e}");
                None
            }
        }
    }

    /// A member's display name, for progress rows. Falls back to the short
    /// id for a node we have no record of.
    fn member_name(&self, id: &NodeId) -> String {
        self.inner
            .engine
            .read(|s| s.members.get(id).map(|m| m.info.name.clone()))
            .unwrap_or_else(|| id.short())
    }

    /// Own a parallel job: cut the item list into chunks, offer it to every
    /// node that may run it, and stream what comes back (M9 §5.4).
    ///
    /// The owner is an ordinary participant as well as the coordinator —
    /// its own lanes pull from the same queue — so a single-node mesh runs
    /// a parallel job with no special case.
    #[tracing::instrument(
        level = "info",
        name = "parallel",
        skip_all,
        fields(job = tracing::field::Empty)
    )]

    /// Write (or rewrite) a parallel run's record into this node's history.
    /// Best-effort: a node with history off simply has no `--resume`.
    fn remember_parallel(&self, record: &mesh_proto::parallel::ParallelRecord) {
        let Some(history) = self.inner.history.as_ref() else {
            return;
        };
        if let Err(e) = history.put_parallel_record(record) {
            warn!(job = %record.job.short(), "cannot record the parallel run: {e}");
        }
    }

    /// Fetch one object and stream its bytes to the client (M9 §5.5).
    ///
    /// Held here, or fetched from a holder the directory names — but never
    /// from "the origin", because there is no origin: an object lives where
    /// it was computed. Streamed a chunk at a time because an object is
    /// routinely larger than one IPC frame.
    async fn object_stream(
        &self,
        id: mesh_proto::ObjectId,
        hints: Vec<NodeId>,
        out: mpsc::Sender<IpcResponse>,
    ) {
        let me = self.node_id();
        let manifest = match self.inner.objects.manifest(&id) {
            Some(manifest) if self.inner.objects.complete(&manifest) => manifest,
            _ => {
                // Local directory first — this node may be one of the two
                // that index it, which makes the lookup free.
                let mut holders: Vec<NodeId> = self
                    .inner
                    .objdir
                    .holders(&id)
                    .into_iter()
                    .map(|r| r.holder)
                    .collect();
                holders.extend(
                    object_sync::locate(&self.inner.engine, &self.inner.transport, me, &id, &hints)
                        .await,
                );
                holders.retain(|h| *h != me);
                holders.dedup();
                // An object is refcounted to a job. A copy fetched on a
                // client's behalf inherits that job, so it is swept on the
                // same schedule as the original instead of outliving it.
                // When this node is not one of the directory nodes it does
                // not know the job, and a fresh id gives the copy the
                // unknown-owner grace period — which is exactly right: the
                // node has no basis to reclaim it sooner.
                let job = self
                    .inner
                    .objdir
                    .owner_job(&id)
                    .unwrap_or_else(JobId::generate);
                match object_sync::fetch(
                    &self.inner.engine,
                    &self.inner.transport,
                    &self.inner.objects,
                    me,
                    job,
                    &id,
                    &holders,
                )
                .await
                {
                    Ok(manifest) => manifest,
                    Err(e) => {
                        let _ = out
                            .send(IpcResponse::Error {
                                message: e.to_string(),
                            })
                            .await;
                        return;
                    }
                }
            }
        };
        for chunk in &manifest.chunks {
            let Some(data) = self.inner.objects.read_chunk(chunk) else {
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!("object {} lost a chunk mid-read", id.short()),
                    })
                    .await;
                return;
            };
            if out.send(IpcResponse::ObjectChunk { data }).await.is_err() {
                return;
            }
        }
        let _ = out
            .send(IpcResponse::ObjectEnd {
                size: manifest.size,
            })
            .await;
    }

    /// The straggler policy in force on this node, or `None` when the
    /// operator turned it off (`[compute] speculate = false`, M9 §5.6).
    fn straggler(&self) -> Option<parallel::Straggler> {
        self.inner.config.compute.straggler
    }

    /// Run the reduce step over a finished map (M9 §5.6, §6.3).
    ///
    /// A reduce is deliberately **not a special kind of job**: it is a
    /// one-item parallel job whose declared inputs are every output of the
    /// map before it. That reuse is what gives it authorization, adoption,
    /// output capture and the object fetch path for free, and it is what
    /// makes the plan's `inputs` field general rather than a reduce hack.
    ///
    /// Placement is the pre-reduce rule of §5.5: it runs on whichever node
    /// already holds the most output bytes, so the least data moves. That
    /// is rung 3 in one line — a task placed where its inputs already are.
    ///
    /// Returns whether the reduce succeeded.
    async fn reduce_stream(
        &self,
        map: &Arc<mesh_proto::ParallelPlan>,
        reduce: String,
        reduce_on: Option<String>,
        inputs: &[mesh_proto::ObjectRef],
        out: &mpsc::Sender<IpcResponse>,
    ) -> bool {
        let me = self.node_id();
        let target = match &reduce_on {
            // An explicit selector wins: an operator who names the node
            // knows something about it the byte count cannot express.
            Some(selector) => {
                let selector = Selector::parse(selector);
                let chosen = self.inner.engine.read(|s| {
                    membership::resolve_targets(s, &selector, me, |id| {
                        self.inner.gossip.liveness_of(id)
                    })
                    .into_iter()
                    .next()
                });
                match chosen {
                    Some(m) => m,
                    None => {
                        let _ = out
                            .send(IpcResponse::Error {
                                message: format!(
                                    "no live node matching {selector:?} for --reduce-on"
                                ),
                            })
                            .await;
                        return false;
                    }
                }
            }
            None => match self.most_output_holder(map, inputs) {
                Some(m) => m,
                None => {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: "no live node can run the reduce".into(),
                        })
                        .await;
                    return false;
                }
            },
        };

        // The single "item" names the map it reduces, so the run is
        // self-describing in a job row and in an audit line.
        let plan = Arc::new(mesh_proto::ParallelPlan {
            job: JobId::generate(),
            command: reduce,
            exec: None,
            item_input: None,
            items: vec![map.job.to_display_string()],
            env: map.env.clone(),
            timeout_secs: map.timeout_secs,
            chunk: Some(1),
            capture: mesh_proto::Capture::Object,
            inputs: inputs.to_vec(),
            workers: vec![target.node_id],
            submitter_user: map.submitter_user,
            auth: map.auth.clone(),
        });
        // No speculation on a one-item job: there is nothing to race, and
        // a duplicate reduce would write the answer twice.
        let (job, mut results) = parallel::ParallelJob::own_with(plan.clone(), None);
        self.inner.parallel.insert(job.clone());
        let dispatch = self.start_lanes(&job, std::slice::from_ref(&target)).await;
        if dispatch.workers.is_empty() {
            self.inner.parallel.remove(&job.id());
            let _ = out
                .send(IpcResponse::Error {
                    message: format!(
                        "the reduce could not start on {}: {}",
                        target.name,
                        dispatch.declined.join("; ")
                    ),
                })
                .await;
            return false;
        }

        let outcome = loop {
            tokio::select! {
                got = results.recv() => match got {
                    Some(result) => {
                        if let Some(outcome) = result.outcomes.into_iter().next() {
                            break Some(outcome);
                        }
                    }
                    None => break None,
                },
                _ = tokio::time::sleep(Duration::from_millis(400)) => {
                    self.reap_dead_workers(&job);
                    if job.finished() {
                        break None;
                    }
                }
            }
        };
        self.inner.parallel.remove(&job.id());

        let Some(outcome) = outcome else {
            let _ = out
                .send(IpcResponse::Error {
                    message: "the reduce did not report a result".into(),
                })
                .await;
            return false;
        };
        let succeeded = outcome.succeeded();
        // The reduce's own answer was captured into an object like any
        // other item's, so it has to be read back to be shown. That is the
        // one place a reduce differs from a map item in practice: the whole
        // point of reducing is that the answer is now small enough to put
        // on a screen, and leaving it as an id the operator has to go and
        // fetch would waste the reduction.
        let answer = match outcome.output {
            Some(obj) => self.read_object_here(plan.job, &obj).await,
            None => None,
        };
        let _ = out
            .send(IpcResponse::ParallelReduced {
                node_name: target.name.clone(),
                exit_code: outcome.exit_code,
                stdout: answer.unwrap_or(outcome.stdout),
                stderr: outcome.stderr,
                output: outcome.output,
                inputs: inputs.len() as u64,
            })
            .await;
        succeeded
    }

    /// Read one object's bytes, fetching it here first if it lives
    /// elsewhere. Bounded on the way out: this exists for answers, not
    /// payloads.
    async fn read_object_here(&self, job: JobId, obj: &mesh_proto::ObjectRef) -> Option<String> {
        let me = self.node_id();
        if !self.inner.objects.has(&obj.id) {
            let holders = object_sync::locate(
                &self.inner.engine,
                &self.inner.transport,
                me,
                &obj.id,
                &[obj.holder],
            )
            .await;
            if let Err(e) = object_sync::fetch(
                &self.inner.engine,
                &self.inner.transport,
                &self.inner.objects,
                me,
                job,
                &obj.id,
                &holders,
            )
            .await
            {
                warn!(object = %obj.id.short(), "cannot read the reduce's answer: {e}");
                return None;
            }
        }
        self.inner
            .objects
            .read(&obj.id)
            .ok()
            .map(|bytes| mesh_proto::parallel::bound_output(&bytes))
    }

    /// The live node holding the most of a map's output. Ties break on the
    /// node id, so two nodes with identical shares always give the same
    /// answer — placement stays deterministic, as everywhere else.
    fn most_output_holder(
        &self,
        map: &mesh_proto::ParallelPlan,
        inputs: &[mesh_proto::ObjectRef],
    ) -> Option<MemberInfo> {
        let mut bytes: HashMap<NodeId, u64> = HashMap::new();
        for obj in inputs {
            *bytes.entry(obj.holder).or_default() += obj.size;
        }
        let mut ranked: Vec<(u64, NodeId)> = bytes.into_iter().map(|(n, b)| (b, n)).collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.inner.engine.read(|s| {
            ranked
                .iter()
                .map(|(_, node)| *node)
                // Fall back to the plan's workers, then to this node: a
                // reduce over nothing still has to run somewhere.
                .chain(map.workers.iter().copied())
                .chain(std::iter::once(self.node_id()))
                .filter(|id| !matches!(self.inner.gossip.liveness_of(id), Liveness::Dead))
                .find_map(|id| s.members.get(&id).map(|m| m.info.clone()))
        })
    }

    /// Put every worker to work on a job we own: local lanes here, an offer
    /// to everyone else. Returns the total lanes now pulling.
    async fn start_lanes(
        &self,
        job: &Arc<parallel::ParallelJob>,
        workers: &[MemberInfo],
    ) -> LaneReport {
        let me = self.node_id();
        let job_id = job.id();

        // Offer to every remote worker *before* starting our own lanes, and
        // to all of them at once. The obvious order hides two faults: the
        // owner's lanes drain a short queue before the first offer even
        // lands, so a small job never leaves the machine that launched it,
        // and dialing workers one at a time makes that head start grow with
        // the size of the mesh. A worker that has not been offered the job
        // yet cannot pull from it, so the offers go first.
        let offers = workers.iter().filter(|w| w.node_id != me).map(|w| {
            let lanes = Self::lanes_for(w);
            async move { (w, lanes, self.offer_parallel(w, &job.plan, lanes).await) }
        });

        let mut report = LaneReport::default();
        for (worker, lanes, outcome) in futures::future::join_all(offers).await {
            match outcome {
                Ok(()) => {
                    job.add_lanes(lanes);
                    report.lanes += lanes;
                    report.workers.push(worker.name.clone());
                }
                Err(e) => {
                    // A worker sitting out is worth saying out loud: the job
                    // still runs, on fewer nodes, and silence there reads as
                    // "the mesh is not working" to whoever launched it.
                    warn!(node = %worker.name, job = %job_id.short(),
                        "worker did not take the parallel offer: {e}");
                    report.declined.push(format!("{}: {e}", worker.name));
                }
            }
        }

        if let Some(mine) = workers.iter().find(|w| w.node_id == me) {
            let lanes = Self::lanes_for(mine);
            job.add_lanes(lanes);
            report.lanes += lanes;
            report.workers.insert(0, mine.name.clone());
            for _ in 0..lanes {
                let task = tokio::spawn(self.clone().own_lane(job.clone(), me, mine.name.clone()));
                self.inner.tasks.lock().push(task);
            }
        }
        report
    }

    /// Stream one chunk's items. `false` means the client hung up.
    ///
    /// `wanted` is M19 §5.4's one addition to `parallel`: an `outputs`
    /// field on the frame, derived here on the owner from the item's own
    /// stdout. Derived rather than carried, deliberately — `ItemOutcome`
    /// travels *nested* inside a chunk report on the node-to-node path, and
    /// a nested struct cannot grow at the tail the way a top-level frame
    /// can. The bytes are already here; parsing them again costs one pass
    /// over a bounded string, and it costs nothing at all when the caller
    /// asked for nothing.
    async fn emit_items(
        &self,
        out: &mpsc::Sender<IpcResponse>,
        result: parallel::ChunkResult,
        produced: &mut Vec<mesh_proto::ObjectRef>,
        wanted: &outputs::Wanted,
        recorded: &mut Vec<mesh_proto::parallel::ParallelItemRecord>,
    ) -> bool {
        let declared = wanted.declared();
        for outcome in result.outcomes {
            if let Some(obj) = outcome.output {
                produced.push(obj);
            }
            // The digest is taken here, at capture, from bytes already in
            // hand (L3 D-L3.1): no second pass, no re-fetch.
            recorded.push(mesh_proto::parallel::ParallelItemRecord {
                item: outcome.item.clone(),
                exit_code: outcome.exit_code,
                node_name: result.worker_name.clone(),
                micros: outcome.micros,
                stdout_digest: mesh_proto::parallel::stdout_digest(&outcome.stdout),
                output: outcome.output.as_ref().map(|o| o.id),
            });
            let item_outputs = if wanted.is_empty() {
                Vec::new()
            } else {
                let mut out = outputs::parse_outputs(&outcome.stdout, &declared);
                out.extend(outputs::implicit_outputs(
                    &outcome.stdout,
                    &outcome.stderr,
                    outcome.exit_code,
                    wanted.as_slice(),
                    &declared,
                ));
                out
            };
            let sent = out
                .send(IpcResponse::ParallelItem {
                    item: outcome.item,
                    exit_code: outcome.exit_code,
                    stdout: outcome.stdout,
                    stderr: outcome.stderr,
                    node: result.worker,
                    node_name: result.worker_name.clone(),
                    output: outcome.output,
                    outputs: item_outputs,
                })
                .await;
            if sent.is_err() {
                return false;
            }
        }
        true
    }

    /// Chunks held by a node the mesh now believes is dead go back on the
    /// queue at once, rather than waiting out the inflight grace period.
    fn reap_dead_workers(&self, job: &Arc<parallel::ParallelJob>) {
        let me = self.node_id();
        for row in job.progress().per_node {
            if row.node != me && matches!(self.inner.gossip.liveness_of(&row.node), Liveness::Dead)
            {
                job.release_worker(&row.node);
            }
        }
    }

    /// How many chunks a node should work on at once: its executor slots,
    /// which already derive from its cores (MASTERPLAN §11). This is the
    /// whole of the "32-core node does twice the work" mechanism — it pulls
    /// through twice as many lanes.
    fn lanes_for(member: &MemberInfo) -> u32 {
        const MAX_LANES: u32 = 64;
        let cores = mesh_proto::Capacity::from_tags(&member.tags).map_or(0, |c| c.cores);
        member.slots.max(cores).clamp(1, MAX_LANES)
    }

    /// One replicated summary for the whole parallel job — never one per
    /// chunk (M9 §3.4, §7).
    fn publish_parallel(
        &self,
        job: &Arc<parallel::ParallelJob>,
        state: JobState,
        created_at_micros: u64,
        finished_at_micros: Option<u64>,
    ) {
        let failed = job.progress().failed;
        let summary = JobSummary {
            job_id: job.id(),
            cmd_preview: format!("parallel ×{} — {}", job.total(), job.plan.what()),
            runtime: "parallel".into(),
            submitter: self.node_id(),
            submitter_user: job.plan.submitter_user,
            executor: self.node_id(),
            state,
            exit_code: finished_at_micros.map(|_| i32::from(failed > 0)),
            created_at_micros,
            started_at_micros: Some(created_at_micros),
            finished_at_micros,
            fanout: None,
            updated_hlc: self.inner.engine.clock().now(),
        };
        if let Err(e) = self.inner.engine.author_op(OpBody::JobUpsert { summary }) {
            warn!(job = %job.id().short(), "cannot publish parallel job summary: {e}");
        }
    }

    /// Offer a parallel job to one worker. `Busy` and version refusals are
    /// ordinary answers, not failures: the job simply runs on fewer nodes.
    async fn offer_parallel(
        &self,
        worker: &MemberInfo,
        plan: &Arc<mesh_proto::ParallelPlan>,
        lanes: u32,
    ) -> Result<()> {
        // Bounded, because the offers are awaited before any local lane
        // starts (§5.4) and a node that has stopped answering must not be
        // able to hold the whole job at the gate. Measured: a worker frozen
        // *before* submission delayed a five-second job by thirty, which is
        // the connect timeout — and is indistinguishable, to the operator,
        // from the mesh being broken.
        tokio::time::timeout(
            OFFER_TIMEOUT,
            self.offer_parallel_inner(worker, plan, lanes),
        )
        .await
        .unwrap_or_else(|_| {
            Err(CoreError::Transport(format!(
                "{} did not answer within {}s",
                worker.name,
                OFFER_TIMEOUT.as_secs()
            )))
        })
    }

    async fn offer_parallel_inner(
        &self,
        worker: &MemberInfo,
        plan: &Arc<mesh_proto::ParallelPlan>,
        lanes: u32,
    ) -> Result<()> {
        let conn = self
            .inner
            .transport
            .member_conn(worker.node_id, &worker.addrs)
            .await?;
        // A peer below the compute minor cannot decode the offer at all.
        if self
            .inner
            .transport
            .peer_minor(&worker.node_id)
            .unwrap_or(0)
            < mesh_proto::COMPUTE_MINOR
        {
            return Err(CoreError::Transport(format!(
                "{} speaks an older protocol minor",
                worker.name
            )));
        }
        let request = Request::ParallelOffer {
            plan: Box::new((**plan).clone()),
            lanes,
        };
        match self.inner.transport.request_one(&conn, &request).await? {
            Response::Ok => Ok(()),
            Response::Busy { running, queued } => Err(CoreError::Busy { running, queued }),
            Response::Error { message } => Err(CoreError::Job(message)),
            other => Err(CoreError::Job(format!("unexpected offer reply: {other:?}"))),
        }
    }

    /// Worker side of an offer: check the authorization once, then start
    /// pulling. A re-offer from an owner that adopted the job after the
    /// original died just re-points the lanes already running.
    async fn accept_parallel_offer(
        &self,
        owner: NodeId,
        plan: mesh_proto::ParallelPlan,
        lanes: u32,
    ) -> Response {
        if self.inner.draining.load(Ordering::SeqCst) {
            return Response::Busy {
                running: u32::try_from(self.inner.jobs.running_count()).unwrap_or(u32::MAX),
                queued: u32::try_from(self.inner.jobs.queued_count()).unwrap_or(u32::MAX),
            };
        }
        if let Some(message) = self.quarantined_here() {
            return Response::Error { message };
        }
        // Enforcement happens on the node that will run the work, before it
        // agrees to run any (MASTERPLAN §7.2). A claim resting on something
        // not merged here yet gets one pull, as a job does (SEC-1).
        let mut verdict = self.verify_plan_auth(&plan, owner);
        if matches!(&verdict, Err(CoreError::Auth(m)) if m.contains(NOT_KNOWN_HERE)) {
            for peer in Self::acting_pull_targets(&plan.env, owner) {
                self.pull_state_from(peer).await;
            }
            verdict = self.verify_plan_auth(&plan, owner);
        }
        if let Err(e) = verdict {
            return Response::Error {
                message: e.to_string(),
            };
        }
        let (worker_job, fresh) = self.inner.working.offer(Arc::new(plan), owner);
        if !fresh {
            return Response::Ok;
        }
        // Lanes are this node's own decision, capped by what it has spare.
        let slots = self.inner.jobs.limits().slots.max(1);
        let taken = self.inner.lanes.load(Ordering::Relaxed).min(slots);
        let lanes = lanes.min(slots - taken).max(1);
        self.inner.lanes.fetch_add(lanes, Ordering::Relaxed);
        worker_job.add_lanes(lanes);
        for _ in 0..lanes {
            let task = tokio::spawn(self.clone().worker_lane(worker_job.clone()));
            self.inner.tasks.lock().push(task);
        }
        Response::Ok
    }

    /// One worker lane: pull a chunk, run it, report, repeat until the owner
    /// says there is nothing left. Nobody pushes work at a worker, so a slow
    /// node simply asks less often — that is the whole of work stealing
    /// (M9 §5.4).
    async fn worker_lane(self, worker_job: Arc<parallel::WorkerJob>) {
        let job = worker_job.plan.job;
        let shutdown = self.inner.shutdown_tx.subscribe();
        let mut misses = 0u32;
        let mut waits = 0u32;
        loop {
            if self.inner.draining.load(Ordering::SeqCst) || *shutdown.borrow() {
                break;
            }
            let owner = worker_job.owner();
            match self.pull_and_run(&worker_job, owner).await {
                LanePull::Ran => {
                    misses = 0;
                    waits = 0;
                }
                LanePull::NoWork => break,
                LanePull::Waited => {
                    misses = 0;
                    waits += 1;
                    if waits > LANE_WAIT_BUDGET {
                        warn!(job = %job.short(),
                            "no work for a very long time; retiring this lane");
                        break;
                    }
                    tokio::time::sleep(LANE_WAIT).await;
                }
                LanePull::Lost => {
                    // The owner did not answer. A busy owner is not a dead
                    // one, and the mesh needs several gossip rounds to tell
                    // the difference — so wait it out rather than giving up
                    // first, or there would be no lane left to re-home.
                    misses += 1;
                    if self.rehome(&worker_job, owner).await {
                        misses = 0;
                        continue;
                    }
                    if misses >= LANE_PATIENCE {
                        warn!(job = %job.short(), "giving up on a silent parallel owner");
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250 * u64::from(misses.min(4)))).await;
                }
            }
        }
        // Newer toolchains rename this `try_update` and warn on the old name;
        // the old name is the one the 1.96 MSRV has, and CI denies warnings.
        #[allow(deprecated)]
        self.inner
            .lanes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            })
            .ok();
        if worker_job.lane_finished() == 0 {
            self.inner.working.remove(&job);
        }
    }

    /// One pull/run/report cycle against `owner`.
    async fn pull_and_run(&self, worker_job: &Arc<parallel::WorkerJob>, owner: NodeId) -> LanePull {
        let plan = &worker_job.plan;
        // After adopting, a node is both owner and worker for the same job.
        // Its lanes pull from the local queue rather than dialing itself.
        if owner == self.node_id() {
            let Some(job) = self.inner.parallel.get(&plan.job) else {
                return LanePull::NoWork;
            };
            let name = self.member_name(&owner);
            let assignment = match job.next_chunk(owner, &name) {
                parallel::Pull::Work(a) => *a,
                parallel::Pull::Wait => return LanePull::Waited,
                parallel::Pull::Done => return LanePull::NoWork,
            };
            let outcomes = self.run_chunk(plan, &assignment, owner).await;
            worker_job.record_done(assignment.indices());
            job.report(
                owner,
                &mesh_proto::ChunkReport {
                    job: plan.job,
                    chunk: assignment.chunk,
                    outcomes,
                },
            );
            return LanePull::Ran;
        }
        let addrs = self
            .inner
            .engine
            .read(|s| s.members.get(&owner).map(|m| m.info.addrs.clone()))
            .unwrap_or_default();
        let Ok(conn) = self.inner.transport.member_conn(owner, &addrs).await else {
            return LanePull::Lost;
        };
        let request = Request::ChunkPull {
            job: plan.job,
            completed: worker_job.claim_for(owner),
        };
        let assignment = match self.inner.transport.request_one(&conn, &request).await {
            Ok(Response::ChunkWork(Some(a))) => *a,
            // Outstanding work elsewhere may still come back to us — as an
            // orphan or as a straggler's copy — so this lane waits rather
            // than retiring (M9 §5.6).
            Ok(Response::ChunkWait) => return LanePull::Waited,
            // No work left, or the owner does not know this job any more.
            Ok(_) => return LanePull::NoWork,
            Err(e) => {
                debug!(job = %plan.job.short(), "chunk pull failed: {e}");
                return LanePull::Lost;
            }
        };
        let outcomes = self.run_chunk(plan, &assignment, owner).await;
        worker_job.record_done(assignment.indices());
        let report = mesh_proto::ChunkReport {
            job: plan.job,
            chunk: assignment.chunk,
            outcomes,
        };
        if let Err(e) = self
            .inner
            .transport
            .request_one(&conn, &Request::ChunkDone(Box::new(report)))
            .await
        {
            debug!(job = %plan.job.short(), "chunk report failed: {e}");
            return LanePull::Lost;
        }
        LanePull::Ran
    }

    /// The owner stopped answering. If the mesh believes it is dead, work
    /// out who takes over — every participant computes the same successor
    /// from the job id, so this costs no election (M9 §5.4, MASTERPLAN
    /// §10.3). Returns whether the lane now has somewhere to pull from.
    async fn rehome(&self, worker_job: &Arc<parallel::WorkerJob>, owner: NodeId) -> bool {
        if !matches!(self.inner.gossip.liveness_of(&owner), Liveness::Dead) {
            return false;
        }
        let plan = &worker_job.plan;
        let Some(next) = parallel::successor(plan, |id| {
            !matches!(self.inner.gossip.liveness_of(id), Liveness::Dead)
        }) else {
            return false;
        };
        if next == owner {
            return false;
        }
        info!(job = %plan.job.short(), from = %owner.short(), to = %next.short(),
            "re-homing parallel job");
        worker_job.set_owner(next);
        if next == self.node_id() {
            self.adopt_parallel(worker_job).await;
        }
        true
    }

    /// Take over a parallel job whose owner died: rebuild the queue from
    /// the plan every worker holds, credit what this node already finished,
    /// and put the surviving workers back to work (M9 §5.4).
    ///
    /// The submitting client's stream died with the original owner, so
    /// there is nobody to stream to. What adoption preserves is the *work*
    /// and the record of it — the same guarantee origin-failure adoption
    /// already gives an ordinary fan-out (MASTERPLAN §10.3).
    async fn adopt_parallel(&self, worker_job: &Arc<parallel::WorkerJob>) {
        let me = self.node_id();
        let plan = worker_job.plan.clone();
        let (job, mut results) = parallel::ParallelJob::own(plan.clone());
        if !self.inner.parallel.adopt(job.clone()) {
            return;
        }
        job.credit_completed(me, &worker_job.completed());
        info!(job = %plan.job.short(), "adopting parallel job after owner loss");
        let created = mesh_proto::now_micros();
        self.publish_parallel(&job, JobState::Running, created, None);

        // Our own lanes re-homed onto us the moment we took over, so only
        // the other survivors need an offer.
        let survivors: Vec<MemberInfo> = self.inner.engine.read(|s| {
            plan.workers
                .iter()
                .filter(|id| **id != me)
                .filter(|id| !matches!(self.inner.gossip.liveness_of(id), Liveness::Dead))
                .filter_map(|id| s.members.get(id).map(|m| m.info.clone()))
                .collect()
        });
        job.add_lanes(self.inner.lanes.load(Ordering::Relaxed).max(1));
        self.start_lanes(&job, &survivors).await;

        // Drive to completion in the background: nobody is streaming, but
        // the job still has to finish and be recorded.
        let this = self.clone();
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(400));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    got = results.recv() => {
                        if got.is_none() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        this.reap_dead_workers(&job);
                        if job.finished() {
                            break;
                        }
                    }
                }
            }
            let state = if job.progress().failed > 0 {
                JobState::Failed
            } else {
                JobState::Succeeded
            };
            this.publish_parallel(&job, state, created, Some(mesh_proto::now_micros()));
            this.inner.parallel.remove(&job.id());
        });
        self.inner.tasks.lock().push(task);
    }

    /// One owner-local lane: the same loop without the wire.
    async fn own_lane(self, job: Arc<parallel::ParallelJob>, me: NodeId, name: String) {
        let shutdown = self.inner.shutdown_tx.subscribe();
        let mut waits = 0u32;
        while !*shutdown.borrow() {
            let assignment = match job.next_chunk(me, &name) {
                parallel::Pull::Work(a) => *a,
                // The owner's own lanes wait at the tail exactly as a
                // remote worker's do, and for the same reason: the copy
                // that races a straggler needs a lane to run on (§5.6).
                parallel::Pull::Wait => {
                    waits += 1;
                    if waits > LANE_WAIT_BUDGET {
                        break;
                    }
                    tokio::time::sleep(LANE_WAIT).await;
                    continue;
                }
                parallel::Pull::Done => break,
            };
            waits = 0;
            let outcomes = self.run_chunk(&job.plan, &assignment, me).await;
            job.report(
                me,
                &mesh_proto::ChunkReport {
                    job: assignment.job,
                    chunk: assignment.chunk,
                    outcomes,
                },
            );
        }
    }

    /// Run every item of a chunk, in order, in a scratch directory that is
    /// removed afterwards.
    async fn run_chunk(
        &self,
        plan: &mesh_proto::ParallelPlan,
        assignment: &ChunkAssignment,
        owner: NodeId,
    ) -> Vec<mesh_proto::ItemOutcome> {
        // One scratch directory per *attempt*, not per chunk.
        //
        // Speculation deliberately runs a second copy of a chunk while the
        // first is still going (M9 §5.6), and both copies can land here:
        // the chunk number is "stable across re-issue" by design, so
        // `parallel/<job>/<chunk>` was the same path for both. Two
        // consequences, both of which produced silently corrupt output that
        // reported success:
        //
        //   * they share `out/<item>`, so one copy's `put_file` could
        //     ingest the other's half-written file — a correct-length
        //     object with a hole in it, hashed and announced as good;
        //   * whichever copy finished first ran `remove_dir_all` on the
        //     directory the other was still writing into.
        //
        // The attempt counter is process-local and never leaves this node:
        // nothing on the wire needs to know which copy this is, only that
        // the two do not share a filesystem path.
        static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self
            .inner
            .config
            .data_dir
            .join("parallel")
            .join(plan.job.to_display_string())
            .join(format!("{}.{attempt}", assignment.chunk));
        let timeout = plan.timeout_secs.map(Duration::from_secs);
        let items = assignment.slice(&plan.items);
        // One scratch directory per chunk, not per item.
        if let Err(e) = std::fs::create_dir_all(dir.join("work")) {
            warn!(job = %plan.job.short(), "cannot create chunk scratch dir: {e}");
        }
        if plan.capture.is_object()
            && let Err(e) = std::fs::create_dir_all(dir.join("out"))
        {
            warn!(job = %plan.job.short(), "cannot create chunk output dir: {e}");
        }
        // Declared object inputs land once per chunk, fetched straight from
        // whoever produced them — never through the node that owns the job
        // (M9 §5.5, G4). This is the task→task edge, and a reduce is only
        // its most obvious user.
        let inputs_dir = dir.join("inputs");
        if let Err(e) = self.materialize_inputs(plan, &inputs_dir).await {
            let items = assignment.slice(&plan.items);
            return items
                .iter()
                .map(|item| mesh_proto::ItemOutcome {
                    item: item.clone(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("cannot fetch this task's inputs: {e}"),
                    micros: 0,
                    output: None,
                })
                .collect();
        }
        // C7 P3 bookkeeping. Publication happens *after* `capture_objects`,
        // because under `map` an item's answer is an object that does not
        // exist until then — publishing before it would cache the item's
        // stdout and hand back nothing where the output should be.
        let mut to_publish: Vec<Option<(usize, mesh_proto::ActionSpec)>> = Vec::new();
        let mut outcomes = match &plan.exec {
            Some(exec) => {
                self.run_script_chunk(
                    plan,
                    exec,
                    items,
                    assignment.start as usize,
                    timeout,
                    &dir,
                    owner,
                    &mut to_publish,
                )
                .await
            }
            None => {
                let mut outcomes = Vec::with_capacity(items.len());
                // Items whose bytes the origin sent, because no worker could
                // see them (R4 P3). Materialised into this chunk's own
                // directory and substituted for the original path, so the
                // command runs against a real local file and neither the
                // command nor the script has to know anything happened.
                let staged = staged_manifest(&plan.env);
                let grants = self.plan_grants(plan);
                for (offset, item) in items.iter().enumerate() {
                    let index = assignment.start as usize + offset;
                    let original = item.as_str();
                    let item = &match self
                        .materialize_staged(&staged, item, index, &dir, plan.job)
                        .await
                    {
                        Ok(local) => local,
                        Err(e) => {
                            outcomes.push(mesh_proto::ItemOutcome {
                                item: item.clone(),
                                exit_code: None,
                                stdout: String::new(),
                                stderr: format!("input could not be staged here: {e}"),
                                micros: 0,
                                output: None,
                            });
                            to_publish.push(None);
                            continue;
                        }
                    };
                    let command = plan.command_for(item);
                    let env = self.item_env(plan, item, &dir, index);
                    // C7 P3: **per item**, which is where the milestone's
                    // headline number actually lives. A `map` of five
                    // hundred items run twice is five hundred separate
                    // actions, each a pure function of its own item — and
                    // an item whose command, inputs and environment have
                    // not moved has nothing to recompute.
                    //
                    // Nothing here is on the path of a job that did not ask
                    // for it: `item_action` returns `None` the moment the
                    // plan's environment lacks `MESHWARE_TASK_CACHE`, and
                    // the whole block costs one `iter().any()`.
                    let action = self
                        .inner
                        .actions
                        .enabled()
                        .then(|| {
                            plan_item_action(
                                plan,
                                original,
                                self.inner.actions.epoch(),
                                &crate::cache::arch(),
                            )
                        })
                        .flatten();
                    if let Some(action) = &action
                        && let Some(mut outcome) = self
                            .cached_item(action, item, plan.capture.is_object())
                            .await
                    {
                        outcome.item = item.clone();
                        outcomes.push(outcome);
                        continue;
                    }
                    let mut outcome = self
                        .inner
                        .jobs
                        .run_item(&command, &env, timeout, &dir, &grants, &plan.submitter_user)
                        .await;
                    outcome.item = item.clone();
                    outcomes.push(outcome);
                    to_publish.push(action.map(|a| (offset, a)));
                }
                outcomes
            }
        };
        // Under `map`, the output goes into this node's object store and
        // only the reference travels. That single substitution is the whole
        // difference between a data plane and a funnel through the origin
        // (M9 §5.5, G4).
        if plan.capture.is_object() {
            self.capture_objects(plan, assignment, &mut outcomes, &dir)
                .await;
        }
        for (offset, action) in to_publish.into_iter().flatten() {
            if let Some(outcome) = outcomes.get(offset)
                && outcome.exit_code == Some(0)
            {
                self.publish_item(plan, &action, outcome).await;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        outcomes
    }

    /// What a per-item run's service declared it reads and writes (R4 P1),
    /// resolved against the inputs this run supplied.
    ///
    /// Same source as the single-job path — the signed, replicated service
    /// definition — and the same node-side refusal of anything under the
    /// data directory or a credential store. An ad-hoc command grants
    /// nothing, which is right: nobody signed it.
    fn plan_grants(&self, plan: &mesh_proto::ParallelPlan) -> crate::sandbox::Grants {
        let Some(exec) = &plan.exec else {
            return crate::sandbox::Grants::default();
        };
        let contract = self.inner.engine.read(|s| {
            s.services
                .get(&exec.service_name)
                .and_then(|e| e.versions.get(&exec.version))
                .and_then(|v| v.contract.clone())
        });
        let Some(contract) = contract else {
            return crate::sandbox::Grants::default();
        };
        let d = crate::script_spec::ScriptSpec::from_contract(&contract);
        crate::sandbox::Grants {
            read: crate::script_spec::resolve_paths(&d.reads, &exec.inputs),
            write: crate::script_spec::resolve_paths(&d.writes, &exec.inputs),
            ..Default::default()
        }
    }

    /// Bring one staged item's bytes here, and answer with the local path
    /// to run against (R4 P3).
    ///
    /// An item nobody staged comes back unchanged, which is every item of
    /// every run on a mesh that shares a filesystem — the cost of this
    /// feature to everyone not using it is one vector lookup.
    ///
    /// The local copy keeps the original basename inside a per-item
    /// directory, so a command that does anything with its input's *name*
    /// — and a transcode names its output after its input — behaves as it
    /// would have on a node that could see the original.
    async fn materialize_staged(
        &self,
        staged: &[String],
        item: &str,
        index: usize,
        dir: &std::path::Path,
        job: mesh_proto::JobId,
    ) -> Result<String> {
        let Some(id) = staged.get(index).filter(|s| !s.is_empty()) else {
            return Ok(item.to_string());
        };
        let id = mesh_proto::ObjectId::parse(id)
            .map_err(|e| CoreError::Job(format!("staged item {index}: {e}")))?;
        let name = std::path::Path::new(item.trim())
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from(format!("item-{index}")));
        let into = dir.join("staged").join(index.to_string());
        std::fs::create_dir_all(&into)?;
        let local = into.join(&name);
        if local.exists() {
            return Ok(local.to_string_lossy().into_owned());
        }
        if !self.inner.objects.has(&id) {
            let me = self.node_id();
            let holders =
                object_sync::locate(&self.inner.engine, &self.inner.transport, me, &id, &[]).await;
            object_sync::fetch(
                &self.inner.engine,
                &self.inner.transport,
                &self.inner.objects,
                me,
                job,
                &id,
                &holders,
            )
            .await
            .map_err(|e| CoreError::Job(format!("fetching staged input: {e}")))?;
        }
        self.inner.objects.write_to(&id, &local)?;
        Ok(local.to_string_lossy().into_owned())
    }

    /// Fetch a plan's declared object inputs into `dest`, one file per
    /// input named by its position (M9 §5.5).
    ///
    /// Each object is looked up through the rendezvous-hashed directory —
    /// or straight from the holder the reference already names, which is
    /// the common case and costs no lookup at all — and fetched from a node
    /// that has it. The owner of the job is never in that path: it never
    /// held these bytes and has no reason to.
    async fn materialize_inputs(
        &self,
        plan: &mesh_proto::ParallelPlan,
        dest: &std::path::Path,
    ) -> Result<()> {
        if plan.inputs.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(dest)?;
        let me = self.node_id();
        for (index, want) in plan.inputs.iter().enumerate() {
            if !self.inner.objects.has(&want.id) {
                let holders = object_sync::locate(
                    &self.inner.engine,
                    &self.inner.transport,
                    me,
                    &want.id,
                    &[want.holder],
                )
                .await;
                object_sync::fetch(
                    &self.inner.engine,
                    &self.inner.transport,
                    &self.inner.objects,
                    me,
                    plan.job,
                    &want.id,
                    &holders,
                )
                .await?;
            }
            self.inner
                .objects
                .write_to(&want.id, &dest.join(index.to_string()))?;
        }
        Ok(())
    }

    /// Turn each successful item's output into an ephemeral object held
    /// here, then tell the objects' directory nodes where they are.
    ///
    /// Two sources, in order: the file the item was invited to write at
    /// `$MESHWARE_OUT`, or — if it wrote nothing there — its stdout. The
    /// fallback is what lets an ordinary `grep`-shaped command take part
    /// without being modified for the mesh.
    ///
    /// **A failed item produces no object.** Half-written output is not
    /// something a reduce step should be handed, and the operator wants the
    /// bytes on screen rather than in a store, so a failure keeps its
    /// bounded inline stdout and stderr instead.
    async fn capture_objects(
        &self,
        plan: &mesh_proto::ParallelPlan,
        assignment: &ChunkAssignment,
        outcomes: &mut [mesh_proto::ItemOutcome],
        dir: &std::path::Path,
    ) {
        let me = self.node_id();
        let mut announce: Vec<(mesh_proto::ObjectId, u64)> = Vec::new();
        for (offset, outcome) in outcomes.iter_mut().enumerate() {
            if !outcome.succeeded() {
                continue;
            }
            // A cache hit already carries its answer as an object (C7 P3).
            // Re-storing it would write the item's now-empty stdout over
            // the output it was served.
            if outcome.output.is_some() {
                continue;
            }
            let index = assignment.start as usize + offset;
            let path = out_path(dir, index);
            let stored = match std::fs::metadata(&path) {
                Ok(meta) if meta.len() > 0 => {
                    self.inner
                        .objects
                        .put_file_off_thread(plan.job, &path)
                        .await
                }
                // Nothing written: the item's stdout *is* its output, and
                // it is already in memory — no file to hash, nothing to
                // move off the worker.
                _ => self.inner.objects.put(plan.job, outcome.stdout.as_bytes()),
            };
            match stored {
                Ok(mut obj) => {
                    obj.holder = me;
                    announce.push((obj.id, obj.size));
                    outcome.output = Some(obj);
                    // The bytes are in the store now; shipping them inline
                    // as well would defeat the point of putting them there.
                    outcome.stdout = String::new();
                }
                Err(e) => {
                    warn!(job = %plan.job.short(), "cannot store item output: {e}");
                    outcome.exit_code = None;
                    outcome.stderr = format!("output could not be stored: {e}");
                }
            }
        }
        self.announce_objects(plan.job, &announce).await;
    }

    /// Publish a batch of objects to their directory nodes (M9 §5.5).
    ///
    /// Grouped by directory node, so a chunk of a thousand items costs one
    /// small request per directory node rather than a thousand round trips.
    /// Entries whose directory node is this one are recorded locally with
    /// no wire traffic at all.
    async fn announce_objects(&self, job: JobId, objects: &[(mesh_proto::ObjectId, u64)]) {
        if objects.is_empty() {
            return;
        }
        let me = self.node_id();
        let members: Vec<NodeId> = self.inner.engine.read(|s| {
            s.members
                .keys()
                .copied()
                .filter(|id| {
                    s.is_active_member(id)
                        && !matches!(self.inner.gossip.liveness_of(id), Liveness::Dead)
                })
                .collect()
        });
        if members.is_empty() {
            return;
        }
        let grouped = crate::objects::group_by_directory(objects, &members);
        let sends = grouped.into_iter().filter_map(|(node, batch)| {
            if node == me {
                for (id, size) in &batch {
                    self.inner.objdir.record(*id, job, *size, me);
                }
                return None;
            }
            Some(async move {
                let addrs = self
                    .inner
                    .engine
                    .read(|s| s.members.get(&node).map(|m| m.info.addrs.clone()))
                    .unwrap_or_default();
                let Ok(conn) = self.inner.transport.member_conn(node, &addrs).await else {
                    return;
                };
                let request = Request::ObjectAnnounce {
                    job,
                    objects: batch,
                };
                if let Err(e) = self.inner.transport.request_one(&conn, &request).await {
                    // A lost announcement costs a re-execution later, not
                    // correctness: the object is still on this node, and a
                    // fetcher that cannot find it re-runs the task that made
                    // it (M9 §10, Q3).
                    debug!(node = %node.short(), "object announce failed: {e}");
                }
            })
        });
        futures::future::join_all(sends).await;
    }

    /// Run a whole chunk in one invocation (M9 §5.4, the interpreter-startup
    /// half).
    ///
    /// The task is handed a file of items and a path to write answers to,
    /// and reports one line per item. Everything that makes a chunk a chunk
    /// is unchanged — it is still pulled, still adaptively sized, still
    /// speculated against when it straggles; the only difference is that
    /// the process starts once instead of `count` times.
    ///
    /// **Failure granularity is the whole design problem here**, and the
    /// protocol answers it rather than the executor: a task that catches
    /// its own per-item exceptions reports ninety-nine successes and one
    /// failure, and a task that dies half way through reports what it
    /// managed and the rest are failed with the process's own stderr. What
    /// is *not* on offer is a batched chunk that silently loses items —
    /// every index in the chunk gets an outcome, always.
    #[allow(clippy::too_many_arguments)]
    async fn run_batched_chunk(
        &self,
        plan: &mesh_proto::ParallelPlan,
        exec: &mesh_proto::ServiceExec,
        items: &[String],
        timeout: Option<Duration>,
        dir: &std::path::Path,
        checkout: &std::path::Path,
    ) -> Vec<mesh_proto::ItemOutcome> {
        let fail_all = |reason: String| -> Vec<mesh_proto::ItemOutcome> {
            items
                .iter()
                .map(|item| mesh_proto::ItemOutcome {
                    item: item.clone(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: reason.clone(),
                    micros: 0,
                    output: None,
                })
                .collect()
        };

        let items_path = dir.join("items.jsonl");
        let results_path = dir.join("results.jsonl");
        let mut listing = String::new();
        for item in items {
            // One item per line, JSON-encoded, so an item containing a
            // newline is still exactly one line to the reader.
            match serde_json::to_string(item) {
                Ok(line) => {
                    listing.push_str(&line);
                    listing.push('\n');
                }
                Err(e) => return fail_all(format!("cannot encode chunk items: {e}")),
            }
        }
        if let Err(e) = std::fs::write(&items_path, listing) {
            return fail_all(format!("cannot write the chunk item list: {e}"));
        }

        let prepared = match crate::runtime::prepare(
            &self.inner.config.data_dir,
            checkout,
            exec,
            dir,
            &plan.env,
            // A per-item script, never a container: `map` fans one
            // script across items, and an image is not the shape of that
            // work.
            crate::runtime::ContainerContext::default(),
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(e) => return fail_all(format!("cannot prepare the script runtime: {e}")),
        };

        let mut env = exec.env.clone();
        env.extend(prepared.extra_env.clone());
        // The batch env, built from the plan rather than from any one item:
        // there is no `MESHWARE_ITEM` here, and a task that reads one is
        // telling us it was not written for this mode.
        env.extend(plan.env.iter().cloned());
        env.retain(|(k, _)| {
            k != mesh_proto::parallel::ITEM_ENV && k != mesh_proto::objects::OUT_ENV
        });
        env.push((
            mesh_proto::parallel::ITEMS_ENV.to_string(),
            items_path.to_string_lossy().into_owned(),
        ));
        env.push((
            mesh_proto::parallel::RESULTS_ENV.to_string(),
            results_path.to_string_lossy().into_owned(),
        ));
        // `MESHWARE_OUT` is deliberately *not* set for a batched chunk.
        // It names one file per item, addressed by the item's index into
        // the whole job — a number a batched task has no way to know, since
        // it is handed a chunk and counts from zero. A task writing to a
        // path it guessed would have its output attributed to somebody
        // else's item, which is worse than not having the feature.
        //
        // Nothing is lost: `capture_objects` already stores an item's
        // reported stdout as its object when no file was written, so
        // `map(..., keep_output=True)` over a batched task keeps working and
        // keeps the bytes on the node that produced them.
        if !plan.inputs.is_empty() {
            env.retain(|(k, _)| k != mesh_proto::objects::INPUTS_ENV);
            env.push((
                mesh_proto::objects::INPUTS_ENV.to_string(),
                dir.join("inputs").to_string_lossy().into_owned(),
            ));
        }

        // The chunk's timeout is the sum of what its items were allowed,
        // because the items now share one process. A per-item timeout that
        // killed the batch at the first slow item would fail the chunk for
        // a reason the caller did not ask for.
        let chunk_timeout =
            timeout.map(|t| t.saturating_mul(u32::try_from(items.len()).unwrap_or(1)));
        let started = Instant::now();
        let outcome = self
            .inner
            .jobs
            .run_prepared(
                &prepared.program,
                &prepared.args,
                &prepared.workdir,
                &env,
                chunk_timeout,
                dir,
                &self.plan_grants(plan),
                &plan.submitter_user,
            )
            .await;
        let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);

        // Read back whatever the task reported. A missing or unreadable
        // file is not an error in itself — it means nothing was reported,
        // and every item then falls through to the process's own outcome.
        let reported = std::fs::read_to_string(&results_path).unwrap_or_default();
        let mut rows: HashMap<usize, mesh_proto::ItemOutcome> = HashMap::new();
        for line in reported.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(index) = value.get("i").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            let index = usize::try_from(index).unwrap_or(usize::MAX);
            if index >= items.len() {
                continue;
            }
            let mut out = value
                .get("out")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut err = value
                .get("err")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            out.truncate(mesh_proto::parallel::MAX_ITEM_OUTPUT);
            err.truncate(mesh_proto::parallel::MAX_ITEM_OUTPUT);
            rows.insert(
                index,
                mesh_proto::ItemOutcome {
                    item: items[index].clone(),
                    exit_code: value
                        .get("code")
                        .and_then(serde_json::Value::as_i64)
                        .map(|c| i32::try_from(c).unwrap_or(1)),
                    stdout: out,
                    stderr: err,
                    // Per-item timing the task measured itself, when it
                    // bothered to; the chunk's own elapsed time otherwise,
                    // shared out evenly. The owner sizes the next chunk
                    // from this, so a plausible number matters more than a
                    // precise one.
                    micros: value
                        .get("micros")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(elapsed / items.len().max(1) as u64),
                    output: None,
                },
            );
        }

        // Every index gets an outcome. An item the task never reported
        // inherits the process's failure, which is what actually happened
        // to it.
        (0..items.len())
            .map(|i| {
                rows.remove(&i).unwrap_or_else(|| {
                    let mut inherited = mesh_proto::ItemOutcome {
                        item: items[i].clone(),
                        exit_code: outcome.exit_code,
                        stdout: String::new(),
                        stderr: if outcome.stderr.is_empty() {
                            format!(
                                "the batched task reported no result for this item \
                                 (chunk item {} of {})",
                                i + 1,
                                items.len()
                            )
                        } else {
                            outcome.stderr.clone()
                        },
                        micros: elapsed / items.len().max(1) as u64,
                        output: None,
                    };
                    // A batch that exited 0 but skipped an item did not
                    // succeed at that item, whatever it says about itself.
                    if inherited.exit_code == Some(0) {
                        inherited.exit_code = None;
                    }
                    inherited
                })
            })
            .collect()
    }

    /// The job environment plus this item, so any script or command can read
    /// what it was handed without the caller having to thread it through argv.
    fn item_env(
        &self,
        plan: &mesh_proto::ParallelPlan,
        item: &str,
        dir: &std::path::Path,
        index: usize,
    ) -> Vec<(String, String)> {
        item_env_for(plan, item, dir, index)
    }

    /// Run a chunk whose per-item work is a Library script (MASTERPLAN M9
    /// §5.4 + §6.4: "a per-item command is an ordinary service").
    ///
    /// The bundle is fetched and materialized **once per chunk** — that is
    /// the whole reason chunks exist for script work, since materializing a
    /// bundle per item would dwarf the item itself. Runtime preparation
    /// (a venv, an Ansible inventory) is content-hashed and reused across
    /// items and chunks alike.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    async fn run_script_chunk(
        &self,
        plan: &mesh_proto::ParallelPlan,
        exec: &mesh_proto::ServiceExec,
        items: &[String],
        start: usize,
        timeout: Option<Duration>,
        dir: &std::path::Path,
        owner: NodeId,
        to_publish: &mut Vec<Option<(usize, mesh_proto::ActionSpec)>>,
    ) -> Vec<mesh_proto::ItemOutcome> {
        let checkout = dir.join("checkout");
        // Hint the owner and the other participants. A worker may well be
        // running its first chunk before the bundle's announcement has
        // gossiped to it, and "no known holder" would fail the whole chunk
        // over a timing gap — the owner always holds it.
        let mut hints = vec![owner];
        hints.extend(plan.workers.iter().copied());
        if let Err(e) = self.ensure_bundle(&exec.bundle_id, &hints).await {
            return items
                .iter()
                .map(|item| mesh_proto::ItemOutcome {
                    item: item.clone(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("cannot fetch the script's bundle: {e}"),
                    micros: 0,
                    output: None,
                })
                .collect();
        }
        if let Err(e) = self.inner.content.materialize(&exec.bundle_id, &checkout) {
            return items
                .iter()
                .map(|item| mesh_proto::ItemOutcome {
                    item: item.clone(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("cannot materialize the script: {e}"),
                    micros: 0,
                    output: None,
                })
                .collect();
        }

        // One invocation for the whole chunk, when the task was written for
        // it (`MESHWARE_TASK_BATCH`). The bundle and the runtime are already
        // prepared once per chunk; this makes the *process* once per chunk
        // too, which is where a Python task's `import numpy` stops being
        // paid per item.
        if mesh_proto::is_batched(&plan.env) {
            return self
                .run_batched_chunk(plan, exec, items, timeout, dir, &checkout)
                .await;
        }

        let mut outcomes = Vec::with_capacity(items.len());
        for (offset, item) in items.iter().enumerate() {
            let index = start + offset;
            let per_item = per_item_exec(plan, exec, item);
            let prepared = match crate::runtime::prepare(
                &self.inner.config.data_dir,
                &checkout,
                &per_item,
                dir,
                &plan.env,
                // A per-item script, never a container: `map` fans one
                // script across items, and an image is not the shape of that
                // work.
                crate::runtime::ContainerContext::default(),
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(e) => {
                    outcomes.push(mesh_proto::ItemOutcome {
                        item: item.clone(),
                        exit_code: None,
                        stdout: String::new(),
                        stderr: format!("cannot prepare the script runtime: {e}"),
                        micros: 0,
                        output: None,
                    });
                    continue;
                }
            };
            // Precedence matches an ordinary job: service env, runtime env,
            // then the job's own env (which carries the item).
            let mut env = per_item.env.clone();
            env.extend(prepared.extra_env.clone());
            env.extend(self.item_env(plan, item, dir, index));
            // C7 P3 — `map --service X --cacheable`. The same per-item
            // caching the command path gets, and it must be here rather
            // than nowhere: a `--cacheable` that quietly did nothing for
            // exactly the payload class §11.2 says is *most* cacheable
            // would be the worst version of this feature.
            //
            // The action is the *service*, not a bash command, so the key
            // carries its runtime, entrypoint, bundle root and this item's
            // resolved inputs — which is what makes two items of one map
            // two different actions.
            let action = self
                .inner
                .actions
                .enabled()
                .then(|| {
                    plan_item_action(
                        plan,
                        item,
                        self.inner.actions.epoch(),
                        &crate::cache::arch(),
                    )
                })
                .flatten();
            if let Some(action) = &action
                && let Some(mut outcome) = self
                    .cached_item(action, item, plan.capture.is_object())
                    .await
            {
                outcome.item = item.clone();
                outcomes.push(outcome);
                to_publish.push(None);
                continue;
            }
            let mut outcome = self
                .inner
                .jobs
                .run_prepared(
                    &prepared.program,
                    &prepared.args,
                    &prepared.workdir,
                    &env,
                    timeout,
                    dir,
                    &self.plan_grants(plan),
                    &plan.submitter_user,
                )
                .await;
            outcome.item = item.clone();
            outcomes.push(outcome);
            to_publish.push(action.map(|a| (offset, a)));
        }
        outcomes
    }

    /// Attach this node's signed statement that the envelope's signer speaks
    /// for `user`, when the signer is one of our own job credentials.
    ///
    /// A no-op for every ordinary caller — a person at a terminal signs as
    /// themselves and there is nothing to attest. See `mesh_proto::acting`
    /// for what this buys a remote executor and, just as importantly, what
    /// it does not.
    fn attest_acting_for(&self, env: &SignedIpc, user: &UserId, req: IpcRequest) -> IpcRequest {
        // `env.user` is the raw signer; `user` is who `verify_envelope`
        // resolved it to. Equal means nothing was mapped.
        if env.user == *user {
            return req;
        }
        let Some((job, _)) = self.inner.jobs.acting_for(&env.user) else {
            return req;
        };
        // The evidence (SEC-1): the authorization this node accepted the job
        // under, and — when a credential signed that too — the claim the job
        // itself arrived with, so the chain ends in the user's own
        // signature. Without it a remote executor has only this node's word.
        let Some(spec) = self.inner.jobs.spec_of(&job) else {
            warn!(job = %job.short(), "no journal for a live job credential; not attesting");
            return req;
        };
        let inner = if spec.auth.user == spec.submitter_user {
            Vec::new()
        } else {
            mesh_proto::acting::act_pairs(&spec.env)
        };
        let proof = mesh_proto::acting::ActingProof {
            envelope: spec.auth,
            inner,
        };
        if proof.depth() > mesh_proto::acting::ACT_MAX_DEPTH {
            warn!(
                job = %job.short(),
                "job credentials nested deeper than {}; not attesting",
                mesh_proto::acting::ACT_MAX_DEPTH
            );
            return req;
        }
        let claim = mesh_proto::acting::ActingFor {
            key: env.user,
            user: *user,
            job,
            node: self.node_id(),
            // A deadline, so a leaked per-job key is not a standing licence
            // even on a node the job's terminal summary never reaches
            // (S-M8). The job's own lifetime is still the real bound and is
            // checked separately.
            expires_at_micros: Some(
                mesh_proto::now_micros().saturating_add(mesh_proto::acting::ACT_TTL_MICROS),
            ),
            proof: Some(proof),
        };
        let attestation = claim.to_env(|bytes| self.inner.identity.node.sign(bytes));
        let extend = |mut e: Vec<(String, String)>| {
            e.retain(|(k, _)| !k.starts_with(mesh_proto::acting::ACT_ENV_PREFIX));
            e.extend(attestation.iter().cloned());
            e
        };
        match req {
            IpcRequest::Run {
                command,
                on,
                env,
                timeout_secs,
                fanout,
                priority,
                max_parallel,
                halt_on_fail,
            } => IpcRequest::Run {
                command,
                on,
                env: extend(env),
                timeout_secs,
                fanout,
                priority,
                max_parallel,
                halt_on_fail,
            },
            IpcRequest::Parallel {
                command,
                service,
                inputs,
                item_input,
                items,
                on,
                env,
                timeout_secs,
                chunk,
                capture,
                reduce,
                reduce_on,
            } => IpcRequest::Parallel {
                command,
                service,
                inputs,
                item_input,
                items,
                on,
                env: extend(env),
                timeout_secs,
                chunk,
                capture,
                reduce,
                reduce_on,
            },
            // Everything else either carries no environment or is not work
            // a task submits. Left exactly as it arrived, which means an
            // executor sees no attestation and applies the ordinary rule.
            other => other,
        }
    }

    /// Does `env`'s environment carry a live member's **proven** statement
    /// that `signer` speaks for `user`?
    ///
    /// The one place a remote executor is allowed to accept an envelope
    /// signed by a key that is not the submitter. See `mesh_proto::acting`
    /// for the model; [`Self::check_claim`] is where each part of it is
    /// enforced.
    fn acting_check(
        &self,
        job_env: &[(String, String)],
        signer: UserId,
        user: UserId,
    ) -> ActingCheck {
        match mesh_proto::acting::ActingFor::from_env(job_env, signer, user) {
            Some((claim, signature)) => {
                self.check_claim(&claim, &signature, mesh_proto::now_micros(), 0)
            }
            None => ActingCheck::Absent,
        }
    }

    /// One link of an acting-for chain, judged as of `at`.
    ///
    /// `at` is *now* for the claim a job arrived with. For the links beneath
    /// it, it is the instant the link above was signed: what has to have
    /// been true is that the credential spoke for the user **when it signed
    /// the next request**, not that the job it came from is still running.
    /// A job that submitted work and exited has not revoked that work.
    ///
    /// Every check here closes a way SEC-1 found to speak for somebody:
    ///
    ///   * **no proof** (a pre-1.30 claim) — the claim was a node's word;
    ///   * **a job the node never ran for that user** — a claim could cite
    ///     anything, and a summary is authored by the executor it names, so
    ///     the summary alone is not the evidence: the user's envelope is;
    ///   * **an envelope the mesh replicates** — a fan-out or gang plan
    ///     puts its envelope in front of every member, so it proves only
    ///     the targets its plan names;
    ///   * **a record-authorized job** — a deployment instance or scheduled
    ///     job has no envelope of the user's to show, only a record every
    ///     member can read.
    fn check_claim(
        &self,
        claim: &mesh_proto::acting::ActingFor,
        signature: &[u8],
        at: u64,
        depth: usize,
    ) -> ActingCheck {
        use mesh_proto::acting::{ACT_MAX_DEPTH, ACT_PROOF_MAX_AGE_MICROS, ACT_TTL_MICROS};
        let refuse = |why: String| {
            debug!(
                node = %claim.node.short(),
                job = %claim.job.short(),
                depth,
                "acting-for claim refused: {why}"
            );
            ActingCheck::Refused(why)
        };
        if depth >= ACT_MAX_DEPTH {
            return refuse(format!(
                "the credential chain is deeper than {ACT_MAX_DEPTH}"
            ));
        }
        let Some(proof) = &claim.proof else {
            return refuse(format!(
                "node {} vouched without proof of the user's request, which \
                 means it runs a build older than protocol 1.30",
                claim.node.short()
            ));
        };
        let Some(until) = claim.expires_at_micros else {
            return refuse("the claim carries no deadline".into());
        };
        if at > until {
            return refuse("the claim has expired".into());
        }
        // The deadline is the claimer's own choice. No honest node picks one
        // further out than the TTL it mints with.
        if until > at.saturating_add(ACT_TTL_MICROS + ACT_CLAIM_SKEW_MICROS) {
            return refuse("the claim's deadline is further out than any node mints".into());
        }
        let Ok(sig) = <[u8; 64]>::try_from(signature) else {
            return refuse("the claim's signature is malformed".into());
        };
        let (standing, job) = self.inner.engine.read(|s| {
            (
                s.members.contains_key(&claim.node) && s.is_active_member(&claim.node),
                s.jobs.get(&claim.job).cloned(),
            )
        });
        if !standing {
            return refuse(format!(
                "node {} is not an active member",
                claim.node.short()
            ));
        }
        if !verify_signature(&claim.node.0, &claim.signing_bytes(), &sig) {
            return refuse("the claim's signature does not verify".into());
        }
        // A claim this node made, about a credential it minted: the
        // credential table is the truth, and it holds only live jobs.
        if depth == 0 && claim.node == self.node_id() {
            return if self.inner.jobs.acting_for(&claim.key) == Some((claim.job, claim.user)) {
                ActingCheck::Proven
            } else {
                refuse("this node holds no live credential for the cited job".into())
            };
        }

        // -- the job the claim cites ---------------------------------------
        let Some(job) = job else {
            return ActingCheck::Unknown {
                what: format!("job {}", claim.job.short()),
            };
        };
        if job.submitter_user != claim.user {
            return refuse("the cited job was submitted by someone else".into());
        }
        if job.executor != claim.node {
            return refuse("the cited job was not run by the node vouching for it".into());
        }
        if job.state.is_terminal() && job.finished_at_micros.is_none_or(|done| done < at) {
            return refuse("the cited job had already finished".into());
        }

        // -- what the user signed ------------------------------------------
        let envelope = &proof.envelope;
        if crate::identity::verify_ipc(envelope, self.mesh_id(), &[job.submitter]).is_none() {
            return refuse("the proof's signature does not verify".into());
        }
        let Ok(request) = postcard::from_bytes::<IpcRequest>(&envelope.payload) else {
            return refuse("the proof's request does not decode".into());
        };
        let request = match request {
            IpcRequest::BreakGlass { request, .. }
            | IpcRequest::DryRun { request }
            | IpcRequest::ExplainKey { request } => *request,
            other => other,
        };
        if matches!(
            request,
            IpcRequest::DeploymentFire { .. } | IpcRequest::ScheduleFire { .. }
        ) {
            return refuse(
                "a deployment instance or a scheduled job cannot act for its creator \
                 on another node — run it under a service account instead"
                    .into(),
            );
        }
        let window = u64::try_from(JOB_AUTH_WINDOW.as_micros()).unwrap_or(u64::MAX);
        if envelope.at_micros.abs_diff(job.created_at_micros) > window {
            return refuse("the proof's request does not date from the cited job".into());
        }
        if at.saturating_sub(envelope.at_micros) > ACT_PROOF_MAX_AGE_MICROS {
            return refuse(
                "the request the claim stands on was signed more than a week ago".into(),
            );
        }
        match self.planned_binding(&job, envelope, claim.node, &request) {
            PlanBinding::Listed | PlanBinding::Unplanned => {}
            PlanBinding::Missing => {
                return ActingCheck::Unknown {
                    what: format!("the plan behind job {}", job.job_id.short()),
                };
            }
            PlanBinding::Refused(why) => return refuse(why.into()),
        }

        // -- who signed it -------------------------------------------------
        if envelope.user == claim.user {
            return ActingCheck::Proven;
        }
        let Some((inner, inner_sig)) = proof.inner_claim(claim.user) else {
            return refuse(
                "the proof is signed by neither the user nor a credential vouched for".into(),
            );
        };
        self.check_claim(&inner, &inner_sig, envelope.at_micros, depth + 1)
    }

    /// Does a replicated plan stand between `job`'s envelope and the node
    /// claiming it? (SEC-1)
    ///
    /// A fan-out or gang plan replicates its envelope to every member. So
    /// for a planned job the envelope proves exactly the `(job, node)`
    /// pairs its plan lists, and for an unplanned one the envelope must not
    /// be one any plan carries — otherwise a copy read out of replicated
    /// state would be as good as being handed the work.
    fn planned_binding(
        &self,
        job: &JobSummary,
        envelope: &SignedIpc,
        node: NodeId,
        request: &IpcRequest,
    ) -> PlanBinding {
        self.inner.engine.read(|s| {
            let fanout = job.fanout.and_then(|f| s.fanouts.get(&f.group)).map(|e| {
                (
                    &e.plan.auth,
                    e.plan.submitter_user,
                    e.plan
                        .targets
                        .iter()
                        .any(|t| t.job_id == job.job_id && t.node_id == node),
                )
            });
            let planned = fanout.or_else(|| {
                s.gangs
                    .values()
                    .find(|g| g.plan.members.iter().any(|m| m.job_id == job.job_id))
                    .map(|g| {
                        (
                            &g.plan.auth,
                            g.plan.submitter_user,
                            g.plan
                                .members
                                .iter()
                                .any(|m| m.job_id == job.job_id && m.node_id == node),
                        )
                    })
            });
            match planned {
                Some((auth, _, _)) if auth.signature != envelope.signature => {
                    PlanBinding::Refused("the cited job's plan carries a different request")
                }
                Some((_, user, _)) if user != job.submitter_user => {
                    PlanBinding::Refused("the cited job's plan belongs to someone else")
                }
                Some((_, _, false)) => PlanBinding::Refused(
                    "the cited job's plan does not send it to the node vouching for it",
                ),
                Some(_) => PlanBinding::Listed,
                None => {
                    let carried = s
                        .fanouts
                        .values()
                        .any(|e| e.plan.auth.signature == envelope.signature)
                        || s.gangs
                            .values()
                            .any(|g| g.plan.auth.signature == envelope.signature);
                    if carried {
                        PlanBinding::Refused(
                            "the proof's request was planned for other jobs than the one cited",
                        )
                    } else if job.fanout.is_some() || Self::is_planned_request(request) {
                        PlanBinding::Missing
                    } else {
                        PlanBinding::Unplanned
                    }
                }
            }
        })
    }

    /// Is `request` one whose envelope the mesh replicates in a plan — a
    /// fan-out, or a gang?
    fn is_planned_request(request: &IpcRequest) -> bool {
        let gang = |env: &[(String, String)]| env.iter().any(|(k, _)| k == mesh_proto::REQ_GANG);
        match request {
            IpcRequest::Run { fanout, env, .. } => fanout.is_some() || gang(env),
            IpcRequest::RunService { fanout, .. } => fanout.is_some(),
            IpcRequest::RunScript { env, .. } | IpcRequest::Parallel { env, .. } => gang(env),
            _ => false,
        }
    }

    /// Who to pull from when a claim rests on something not known here yet:
    /// the node that sent the work, and the node vouching for it.
    fn acting_pull_targets(env: &[(String, String)], sender: NodeId) -> Vec<NodeId> {
        let mut targets = vec![sender];
        if let Some(node) = env
            .iter()
            .find(|(k, _)| k == mesh_proto::acting::ACT_NODE)
            .and_then(|(_, v)| NodeId::parse(v.trim()).ok())
            && node != sender
        {
            targets.push(node);
        }
        targets
    }

    /// Does the script a plan is about to run match the one the user
    /// actually signed?
    ///
    /// Half of `verify_plan_auth`'s payload check, and the half that says
    /// *which code*. A named service compares by name, because the name is
    /// what the envelope carries and the version behind it is immutable and
    /// signed by its author. An anonymous bundle has no name to compare, so
    /// this re-derives the exec from the signed environment and compares
    /// what identifies the code — which is a **stronger** check than the
    /// name was: bundle, entrypoint, runtime and working directory all have
    /// to match what the submitter put their signature over.
    ///
    /// `inputs` are deliberately not compared. Every chunk substitutes its
    /// own item into them; that is what a map *is*, and the item list is
    /// checked in full by the caller.
    fn exec_matches_signed(exec: Option<&mesh_proto::ServiceExec>, service: Option<&str>) -> bool {
        match (exec, service) {
            (None, None) => true,
            // A named service: the envelope names it, so compare names.
            (Some(exec), Some(name)) => exec.service_name == name,
            // No name in the envelope but a script on the plan. The one
            // thing that used to make this legitimate was an anonymous
            // bundle, retired with the Python SDK on 2026-09-20 — nothing
            // can author one now, so a plan carrying an exec the envelope
            // did not name is a mismatch.
            (Some(_), None) => false,
            // The envelope names a service the plan does not run.
            (None, Some(_)) => false,
        }
    }

    /// The executing node re-verifies the user's signed request and its own
    /// view of that user's grants before agreeing to run anything — a
    /// parallel job is authorized exactly like an ordinary one
    /// (MASTERPLAN §7.2).
    fn verify_plan_auth(&self, plan: &mesh_proto::ParallelPlan, offered_by: NodeId) -> Result<()> {
        self.verify_plan_auth_for(plan, offered_by, self.node_id())
    }

    /// [`Self::verify_plan_auth`], with the grant checked for `runner` —
    /// the worker whose cache entry a replica is checking (SEC-18).
    fn verify_plan_auth_for(
        &self,
        plan: &mesh_proto::ParallelPlan,
        offered_by: NodeId,
        runner: NodeId,
    ) -> Result<()> {
        let env = &plan.auth;
        // As `verify_job_auth`: the submitter, or a key a live member has
        // signed a statement for. The two checks below are untouched — the
        // envelope still has to cover this exact command over these exact
        // items, and the grant check is still this node's own.
        if env.user != plan.submitter_user {
            self.acting_check(&plan.env, env.user, plan.submitter_user)
                .into_result("parallel authorization")?;
        }
        if <[u8; 64]>::try_from(env.signature.as_slice()).is_err() {
            return Err(CoreError::Auth(
                "malformed parallel authorization signature".into(),
            ));
        }
        // Bound, if at all, to the node that took the request: the owner
        // offering it, or — after an adoption — the one that published the
        // job (SEC-12).
        let origin = self
            .inner
            .engine
            .read(|s| s.jobs.get(&plan.job).map(|j| j.submitter));
        let mut audiences = vec![offered_by, self.node_id(), runner];
        audiences.extend(origin);
        if crate::identity::verify_ipc(env, self.mesh_id(), &audiences).is_none() {
            return Err(CoreError::Auth("invalid parallel authorization".into()));
        }
        // Deliberately no freshness window here, unlike an ordinary job's
        // (`verify_job_auth`). A parallel job can outlive one, and an
        // adopting owner re-offers the same plan minutes later — expiring
        // the envelope would make surviving the owner's death impossible.
        // The signature, the payload match below, and the live grant check
        // are what authorize the work.
        let req: IpcRequest = postcard::from_bytes(&env.payload)
            .map_err(|_| CoreError::Auth("malformed parallel authorization payload".into()))?;
        if let Some(key) = loader_key(&plan.env) {
            return Err(CoreError::Auth(format!(
                "parallel environment sets {key}, which is never accepted"
            )));
        }
        // The environment and deadline, as for an ordinary job (SEC-4). A
        // reduce carries its map's, so one check covers both.
        if let IpcRequest::Parallel {
            env: signed_env,
            timeout_secs,
            ..
        } = &req
            && (!mesh_proto::env_matches_signed(&plan.env, signed_env)
                || plan.timeout_secs != *timeout_secs)
        {
            return Err(CoreError::Auth(
                "parallel environment or timeout does not match what the user signed".into(),
            ));
        }
        let service = match &req {
            IpcRequest::Parallel {
                command,
                service,
                items,
                env: signed_env,
                ..
            } if *items == plan.items
                && *command == plan.command
                && Self::exec_matches_signed(plan.exec.as_deref(), service.as_deref()) =>
            {
                service.clone()
            }
            // A reduce carries the same envelope as the map it follows, so
            // the two plans differ from each other while both match what
            // the user actually signed. The reduce command is signed in
            // that envelope, which is the part that has to be true: a
            // compromised origin cannot substitute a different one
            // (MASTERPLAN §7.2, M9 §5.6).
            IpcRequest::Parallel {
                reduce: Some(reduce),
                ..
            } if *reduce == plan.command && plan.items.len() == 1 && plan.exec.is_none() => None,
            _ => {
                return Err(CoreError::Auth(
                    "parallel authorization does not match the work".into(),
                ));
            }
        };
        let founder = self.inner.engine.genesis().body.founder_user;
        let allowed = self.inner.engine.read(|s| {
            s.members.get(&runner).is_some_and(|m| {
                rbac::can_run_on(
                    s,
                    &founder,
                    &plan.submitter_user,
                    &m.info,
                    service.as_deref(),
                    mesh_proto::now_micros(),
                )
            })
        });
        if !allowed {
            return Err(CoreError::Denied(format!(
                "user {} has no grant covering this node",
                plan.submitter_user.short()
            )));
        }
        Ok(())
    }

    /// Live pressure for any member (M9 §5.2): our own is read directly,
    /// a peer's is whatever the gossip tick last collected. Placement uses
    /// this rather than the gossip map alone, so the local node is judged
    /// by the same measurements as everyone else.
    /// What placement reads about every candidate.
    ///
    /// This node measures itself directly — always the freshest reading
    /// there is, and the one place where "unknown" would be inexcusable —
    /// and every peer comes from gossip, rich or bare depending on what it
    /// speaks.
    fn pressure_lookup(&self) -> impl Fn(&NodeId) -> Option<mesh_proto::SystemSnapshot> + use<'_> {
        let me = self.node_id();
        let own = self.system();
        move |id: &NodeId| {
            if *id == me {
                Some(own.clone())
            } else {
                self.inner.gossip.live_of(id)
            }
        }
    }

    /// This node's live pressure right now (M9 §5.2). Never persisted,
    /// never authored as an op — it is answered on demand and forgotten.
    pub fn pressure(&self) -> mesh_proto::PressureSnapshot {
        capacity::pressure(
            &self.inner.config.data_dir,
            u32::try_from(self.inner.jobs.running_count()).unwrap_or(u32::MAX),
            u32::try_from(self.inner.jobs.queued_count()).unwrap_or(u32::MAX),
            self.inner.engine.clock().now(),
        )
    }

    /// This node's live telemetry: everything [`Self::pressure`] reports
    /// plus the numbers that move when the machine gets busy.
    pub fn system(&self) -> mesh_proto::SystemSnapshot {
        capacity::system(
            &self.inner.config.data_dir,
            &self.inner.cpu_sampler,
            &self.inner.net_sampler,
            u32::try_from(self.inner.jobs.running_count()).unwrap_or(u32::MAX),
            u32::try_from(self.inner.jobs.queued_count()).unwrap_or(u32::MAX),
            self.inner.engine.clock().now(),
        )
    }

    /// The answer to a telemetry question: our own fresh reading, then
    /// everything we hold that has not aged out. That is what turns a pull
    /// that only ever reached one peer per tick into an epidemic.
    fn telemetry_entries(&self) -> Vec<mesh_proto::TelemetryEntryV2> {
        let mut entries = vec![mesh_proto::TelemetryEntryV2 {
            node_id: self.node_id(),
            system: self.system(),
            devices: self.inner.jobs.device_pressure(),
        }];
        entries.extend(self.inner.gossip.telemetry_heard());
        entries
    }

    // -- M8 triggers (§3) ---------------------------------------------------

    // -- C2 deployments (C0-C9 §6) ---------------------------------------

    /// Matching, live, executable nodes for a deployment's selector — the
    /// slots for a `system` count, the placement pool for a fixed one.
    /// What runtime a deployment's instances need (T3).
    ///
    /// The service names it — a deployment names a service and the service
    /// names its runtime. `inline_container` covers the one case where that
    /// lookup cannot work: a **rehearsal** of a file carrying
    /// `[deployment.container]`, where the service it implies has
    /// deliberately not been published yet. Without it a dry run modelled a
    /// deployment with no runtime and reported "no problems" about a mesh
    /// where no node could run it.
    ///
    /// One function because two would agree on the day they were written:
    /// `deployment_candidates` decides where instances go and
    /// `deployment_placement_refusal` explains where they did not, and a
    /// refusal that disagrees with the filter is worse than no refusal.
    fn deployment_runtime(
        s: &crate::state::MeshState,
        dep: &mesh_proto::Deployment,
        inline_container: bool,
    ) -> Option<mesh_proto::Runtime> {
        crate::deployments::runtime_for(s, dep, inline_container)
    }

    fn deployment_candidates(
        &self,
        s: &crate::state::MeshState,
        dep: &mesh_proto::Deployment,
        inline_container: bool,
    ) -> Vec<MemberInfo> {
        // Every permanent gate, not only the runtime: see
        // `deployments::placement_pool` for the machine that was asked for four
        // cores it did not have, every five seconds, forever.
        crate::deployments::placement_pool(
            s,
            dep,
            self.node_id(),
            |id| self.inner.gossip.liveness_of(id),
            inline_container,
        )
    }

    /// What each node can run (T4 §5).
    ///
    /// Two halves with two confidences, and saying so is the point: this
    /// node's own probes in full, because it ran them; every other node
    /// from the `cap.rt` it announced, because a probe is node-local and a
    /// replicated diagnosis would describe a machine as it was at boot.
    ///
    /// The second half is exactly what `audit_feasible_full` reads, so the
    /// table and the scheduler cannot disagree about who can run what.
    fn runtimes_view(&self, node: Option<&str>) -> IpcResponse {
        let me = self.inner.engine.read(|s| {
            s.members
                .get(&self.node_id())
                .map(|m| m.info.name.clone())
                .unwrap_or_else(|| self.node_id().short())
        });
        if let Some(name) = node.filter(|n| *n != me) {
            return IpcResponse::Error {
                message: format!(
                    concat!(
                        "a runtime probe is node-local: this is {}, not {}. The table ",
                        "below still lists every node's announced runtimes; for {}'s ",
                        "own detail and remedies, ask that node's daemon."
                    ),
                    me, name, name
                ),
            };
        }
        let mesh = self.inner.engine.read(|s| {
            let mut rows: Vec<mesh_proto::runtimes::NodeRuntimes> = s
                .members
                .values()
                .filter(|m| !m.left && !s.revoked.contains_key(&m.info.node_id))
                .map(|m| mesh_proto::runtimes::NodeRuntimes {
                    node: m.info.name.clone(),
                    // From the *merged* tag list, so an admin's assigned
                    // tags are read the way every other selector reads
                    // them. `assigned_tags` unions into `info.tags`
                    // wherever a member is read, which is here.
                    runtimes: mesh_proto::runtimes::from_tags(&m.info.tags),
                })
                .collect();
            rows.sort_by(|a, b| a.node.cmp(&b.node));
            rows
        });
        IpcResponse::Runtimes(Box::new(mesh_proto::runtimes::RuntimesView {
            node: me,
            local: self.inner.jobs.runtime_reports(),
            mesh,
        }))
    }

    /// The mesh-wide shed, if one is in force (N6 §12).
    ///
    /// `None` when nothing is shed, so every call site reads as *"is
    /// anything held back"* rather than having to remember that a record
    /// with both switches clear means no.
    fn shed(&self) -> Option<mesh_proto::shed::Shed> {
        self.inner.engine.read(|s| {
            s.shed
                .as_ref()
                .map(|e| e.shed.clone())
                .filter(|shed| shed.sheds())
        })
    }

    /// How much of the mesh will actually honour a shed (N6 §12).
    ///
    /// Load-bearing rather than decorative: this is the one capability gate
    /// in meshware that fails **open**, so an operator shedding load on a
    /// half-upgraded mesh has to be told which half they actually shed.
    fn shed_readiness(&self) -> mesh_proto::shed::ShedReadiness {
        let me = self.node_id();
        let transport = self.inner.transport.clone();
        self.inner.engine.read(|s| {
            let mut out = mesh_proto::shed::ShedReadiness::default();
            for entry in s.members.values() {
                let id = entry.info.node_id;
                if entry.left || s.revoked.contains_key(&id) {
                    continue;
                }
                out.members += 1;
                // This node speaks the current minor by construction — it
                // is this build — and `peer_minor` would answer `None`
                // about ourselves.
                let minor = if id == me {
                    Some(mesh_proto::PROTOCOL_MINOR)
                } else {
                    transport.peer_minor(&id)
                };
                match minor {
                    Some(m) if m >= mesh_proto::SHED_MINOR => out.honouring += 1,
                    // Unknown counts as *behind*, which is the opposite of
                    // the rule every other unknown in this product follows.
                    // Deliberate: elsewhere an unmeasured node is neutral
                    // because the cost of guessing wrong is a worse
                    // placement. Here the cost is a node that keeps taking
                    // work while the operator believes it stopped.
                    _ => out.behind.push(entry.info.name.clone()),
                }
            }
            out.behind.sort();
            out
        })
    }

    /// The shed as an operator reads it, including who will honour it.
    fn shed_view(&self) -> mesh_proto::ShedView {
        let readiness = self.shed_readiness();
        self.inner.engine.read(|s| {
            let entry = s.shed.as_ref();
            let shed = entry.map(|e| &e.shed);
            mesh_proto::ShedView {
                submissions: shed.is_some_and(|v| v.submissions),
                reconcile: shed.is_some_and(|v| v.reconcile),
                reason: shed.map(|v| v.reason.clone()).unwrap_or_default(),
                // Named only when something is actually shed. "Lifted by
                // Mia four days ago" is history; the question this answers
                // is *who do I go and talk to*, and on a mesh shedding
                // nothing there is nobody.
                by: shed.filter(|v| v.sheds()).map(|v| v.created_by),
                by_name: shed.filter(|v| v.sheds()).and_then(|v| {
                    s.users
                        .get(&v.created_by)
                        .map(|u| u.record.username.clone())
                }),
                at_micros: shed.map(|v| v.created_at_micros).unwrap_or(0),
                readiness,
            }
        })
    }

    /// Pack or spread, live (N6 §12).
    fn placement(&self) -> mesh_proto::FitPolicy {
        *self.inner.placement.lock()
    }

    /// This node's own name, as an operator would recognise it.
    fn my_display_name(&self) -> String {
        self.inner.engine.read(|s| {
            s.members
                .get(&self.node_id())
                .map(|m| m.info.name.clone())
                .unwrap_or_else(|| self.node_id().short())
        })
    }

    /// Re-offer this node's volumes after the provisioned set changed
    /// (N5 P2).
    ///
    /// **Through `apply_config`, which is the `SIGHUP` path.** A second
    /// route that set the executor's list without re-announcing the tag
    /// would be exactly the bug N5 already shipped once: a node lending a
    /// volume the mesh cannot see it lends is a node that never gets the
    /// work. Reusing the reload path means the create verb and the reload
    /// cannot drift, and the tag is re-derived rather than patched.
    fn reoffer_volumes(&self, store: &crate::volumes::Store) {
        let mut config = (*self.live_config()).clone();
        // The hand-written half of the list, kept: `source` is how the two
        // are told apart everywhere else, so it is how they are told apart
        // here rather than by re-reading a file this process no longer
        // holds. A `[[volume]]` block is untouched by any of this.
        let mut volumes: Vec<mesh_proto::Volume> = config
            .network
            .volumes
            .iter()
            .filter(|v| v.source == mesh_proto::volumes::SOURCE_STATIC)
            .cloned()
            .collect();
        for v in store.offered() {
            if !volumes.iter().any(|have| have.name == v.name) {
                volumes.push(v);
            }
        }
        config.network.volumes = volumes;
        // **Set, then reload.** `apply_config` diffed against the *boot*
        // config, so provisioning a volume, removing it and provisioning it
        // again left the executor holding the second state: the third
        // equalled the first and the diff saw nothing. Found by doing
        // exactly that on a live node. It diffs against the live config now
        // (ENG-11), and starting from the live config here is what keeps a
        // `volumes create` from quietly undoing an earlier `SIGHUP`; the
        // executor's list is still set outright, so neither ordering can
        // leave it behind.
        //
        // The re-announce stays on the reload path, which is what keeps the
        // create verb and the reload from ever disagreeing about how a
        // volume becomes visible.
        self.inner.jobs.set_network(
            config.network.clone(),
            advertise_host(&config),
            job_bind_host(&config),
        );
        for note in self.apply_config(&config) {
            tracing::info!("reload: {note}");
        }
    }

    /// Why this deployment can place nothing, when it can place nothing
    /// (N1 P2).
    ///
    /// **`Some` only for a real refusal.** A shortfall is not a refusal:
    /// three replicas of one service are perfectly happy on one node, so
    /// "fewer feasible nodes than `count`" is normal and says nothing. The
    /// question worth answering is whether the mesh has *any* node that
    /// could take an instance right now, and the answer is only
    /// interesting when it is no.
    ///
    /// The requirements come off the pinned service version's environment
    /// — the same `MESHWARE_TASK_*` keys admission reads — so the refusal
    /// predicts the decision `jobs::accept` is about to make rather than
    /// modelling it a second time.
    fn deployment_placement_refusal(
        &self,
        s: &crate::state::MeshState,
        dep: &mesh_proto::Deployment,
        // T3. True when this file carries `[deployment.container]` and the
        // service it describes has not been published yet — the dry-run
        // case. The service lookup below then finds nothing, and without
        // this the rehearsal would model a deployment with no runtime at
        // all and cheerfully report "no problems" about a mesh where no
        // node can run it.
        inline_container: bool,
    ) -> Option<mesh_proto::PlacementRefusal> {
        let selector = Selector::parse(&dep.targets);
        // Both halves of the request: `[deployment.resources]` lives in the
        // side-car, and reading only the service's environment explained a
        // four-core instance as though it asked for nothing.
        let def_env = crate::deployments::instance_request_env(s, dep);
        let reqs = mesh_proto::Requirements::from_env(&def_env);
        let devices = mesh_proto::DeviceRequest::from_env(&def_env);
        // **The instance's environment, not the service's.** `[deployment
        // .network]` and `[[deployment.volume]]` reach an instance through
        // the policy side-car, and that is exactly the env the fire path
        // composes — so reading it here is what makes this refusal predict
        // the decision `jobs::accept` is about to make rather than model a
        // different job.
        //
        // Without it the two most legible refusals a stateful deployment
        // can have — *no node lends `data`*, *the port is taken* — came
        // back as "every node is saturated", which sends the reader to look
        // at load on nodes that were never candidates.
        let instance_env = crate::deployments::policy_for(s, &dep.name).instance_env;
        let ports = mesh_proto::PortRequests::from_env(&instance_env);
        let volumes = mesh_proto::VolumeRequests::from_env(&instance_env);
        // T3. From the *service definition*, which is where a deployment's
        // runtime is settled — a deployment names a service and the service
        // names its runtime. Reading it off the instance environment would
        // work only for the jobs that have already been composed, and this
        // refusal has to explain a deployment that has placed nothing.
        let runtime = Self::deployment_runtime(s, dep, inline_container);
        let pressure_of = self.pressure_lookup();
        let (_, refusal) = membership::audit_feasible_full(
            s,
            &selector,
            self.node_id(),
            |id| self.inner.gossip.liveness_of(id),
            &reqs,
            devices.as_ref(),
            &ports,
            &volumes,
            runtime,
            &pressure_of,
        );
        (refusal.feasible == 0).then_some(refusal)
    }

    /// The node currently elected to reconcile `name`, as a display name.
    fn deployment_reconciler_name(
        &self,
        s: &crate::state::MeshState,
        name: &str,
    ) -> Option<String> {
        crate::deployments::elected_reconciler(s, name, |id| self.inner.gossip.liveness_of(id))
            .and_then(|id| s.members.get(&id).map(|m| m.info.name.clone()))
    }

    /// Rows for `deployments ls` / `deployments show`.
    fn deployment_rows(
        &self,
        only: Option<&str>,
        with_source: bool,
    ) -> Vec<mesh_proto::DeploymentRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            s.deployments
                .values()
                .filter(|e| !e.removed)
                .filter(|e| only.is_none_or(|name| e.deployment.name == name))
                .map(|e| {
                    let dep = &e.deployment;
                    // A live deployment: its service is published, so the lookup works
                    // and the rehearsal-only fallback is not needed.
                    let candidates = self.deployment_candidates(s, dep, false);
                    let policy = crate::deployments::policy_for(s, &dep.name);
                    let degraded = crate::deployments::plan_reconcile(
                        dep,
                        s,
                        &candidates,
                        now,
                        &policy,
                        |id| self.inner.gossip.not_alive_since_micros(id),
                    )
                    .degraded
                    .is_some();
                    let source = with_source.then(|| {
                        let policy = crate::deployments::policy_for(s, &dep.name);
                        crate::deployments::to_toml_with(
                            dep,
                            &policy,
                            &crate::deployments::knobs_for(s, &dep.name),
                        )
                    });
                    crate::deployments::row(dep, s, &founder, &candidates, degraded, source)
                })
                .collect()
        })
    }

    /// Measure jobs this node runs, concurrently (Phase B, E3): each one's
    /// CPU window overlaps the others', so ten instances cost one window
    /// rather than ten.
    async fn measure_local_instances(&self, jobs: Vec<JobId>) -> Vec<mesh_proto::InstanceStat> {
        let mut set = tokio::task::JoinSet::new();
        for job in jobs {
            let container = self.inner.engine.read(|s| {
                s.jobs
                    .get(&job)
                    .is_some_and(|j| j.runtime == mesh_proto::Runtime::Container.as_str())
            });
            let engine = if container {
                self.inner.jobs.container_engine()
            } else {
                None
            };
            let cgroup = if container {
                None
            } else {
                self.inner.jobs.cgroup_dir_of(&job)
            };
            let pid = self.inner.jobs.pid_of(&job);
            set.spawn(crate::instance_stats::measure(job, engine, cgroup, pid));
        }
        let mut out = Vec::new();
        while let Some(done) = set.join_next().await {
            if let Ok(stat) = done {
                out.push(stat);
            }
        }
        out
    }

    /// What these instances are using, wherever they run (Phase B, E3).
    ///
    /// This node's own are measured here; every other node is asked once for
    /// all of its instances, and only if it is new enough to understand the
    /// question. A node that cannot be asked leaves its instances
    /// unmeasured — `None` in the answer — rather than failing it.
    async fn instance_stats_for(
        &self,
        targets: Vec<(JobId, mesh_proto::NodeId)>,
    ) -> HashMap<JobId, mesh_proto::InstanceStat> {
        let me = self.node_id();
        let mut by_node: BTreeMap<mesh_proto::NodeId, Vec<JobId>> = BTreeMap::new();
        for (job, node) in targets {
            by_node.entry(node).or_default().push(job);
        }
        let mut out = HashMap::new();
        for (node, jobs) in by_node {
            let stats = if node == me {
                self.measure_local_instances(jobs).await
            } else {
                let addrs = self
                    .inner
                    .engine
                    .read(|s| s.members.get(&node).map(|m| m.info.addrs.clone()))
                    .unwrap_or_default();
                let Ok(conn) = self.inner.transport.member_conn(node, &addrs).await else {
                    continue;
                };
                if self
                    .inner
                    .transport
                    .peer_minor(&node)
                    .is_some_and(|minor| minor < mesh_proto::INSTANCE_MINOR)
                {
                    continue;
                }
                match self
                    .inner
                    .transport
                    .request_one(&conn, &Request::InstanceStats { jobs })
                    .await
                {
                    Ok(Response::InstanceStats(stats)) => stats,
                    _ => continue,
                }
            };
            for stat in stats {
                out.insert(stat.job, stat);
            }
        }
        out
    }

    /// Is `id` Dead to this node's own liveness view (Phase A, §2.3)?
    ///
    /// **Only Dead.** A Suspect verdict on a relayed or CGNAT link is
    /// usually a blip, and discovery that flapped with every lost probe
    /// would be worse than discovery that is a probe late.
    fn node_is_dead(&self, id: &mesh_proto::NodeId) -> bool {
        self.inner.gossip.liveness_of(id) == mesh_proto::Liveness::Dead
    }

    /// The full status view for `deployments status <name>`.
    fn deployment_status_view(&self, name: &str) -> Option<mesh_proto::DeploymentStatusView> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        self.inner.engine.read(|s| {
            let entry = s.deployments.get(name).filter(|e| !e.removed)?;
            let dep = &entry.deployment;
            // A live deployment: its service is published, so the lookup works
            // and the rehearsal-only fallback is not needed.
            let candidates = self.deployment_candidates(s, dep, false);
            let policy = crate::deployments::policy_for(s, &dep.name);
            let plan =
                crate::deployments::plan_reconcile(dep, s, &candidates, now, &policy, |id| {
                    self.inner.gossip.not_alive_since_micros(id)
                });
            let reconciler = self.deployment_reconciler_name(s, name);
            let placement = self.deployment_placement_refusal(s, dep, false);
            Some(crate::deployments::status_view(
                dep,
                s,
                &founder,
                &candidates,
                reconciler,
                plan.degraded,
                plan.stalled,
                plan.canarying,
                plan.awaiting_promotion,
                // Only when there is a policy to lose (landmine 8). A
                // mixed mesh with no canary anywhere is not a problem, and
                // a warning that fires on every mixed mesh is one nobody
                // reads.
                if policy.is_inert() && crate::deployments::knobs_for(s, name).auto_promote() {
                    Vec::new()
                } else {
                    // Phase B, E2: a node that cannot read the knobs promotes
                    // on health alone, which is exactly the hold a manual
                    // deployment exists for.
                    let needed = if crate::deployments::knobs_for(s, name).auto_promote() {
                        mesh_proto::DEPLOY_POLICY_MINOR
                    } else {
                        mesh_proto::PROMOTE_MINOR
                    };
                    crate::deployments::policy_readiness(s, self.node_id(), needed, |id| {
                        self.inner.transport.peer_minor(id)
                    })
                    .into_iter()
                    .map(|(_, name, _)| name)
                    .collect()
                },
                policy.ports().0.iter().map(|p| p.label.clone()).collect(),
                placement,
                &policy,
                // The reconciler's own notion of "unknown": not Alive. A
                // status that called an instance fine while the reconciler
                // was treating it as gone would be two answers to one
                // question on one screen.
                |id| self.inner.gossip.not_alive_since_micros(id).is_some(),
            ))
        })
    }

    /// Publish (or replace) a deployment, or report a dry run.
    fn deployment_put(
        &self,
        user: UserId,
        env: &SignedIpc,
        deployment: mesh_proto::Deployment,
        policy: mesh_proto::DeploymentPolicy,
        dry_run: bool,
        inline_service: bool,
    ) -> IpcResponse {
        // **Nodes that could actually take it**, which is not the same as
        // nodes the selector matches. `deployment_candidates` deliberately
        // never narrows to nothing — an instance that is placed and fails
        // visibly beats one that is never placed with nothing saying why —
        // so its count can be non-zero on a mesh where nothing can run the
        // work. Printing that number beside a refusal saying "nothing can
        // take it" is the check contradicting itself on consecutive lines.
        let matching = {
            let inline = dry_run && inline_service;
            self.inner.engine.read(|s| {
                let targets = self.deployment_candidates(s, &deployment, inline);
                let n = match Self::deployment_runtime(s, &deployment, inline) {
                    Some(rt) => targets
                        .iter()
                        .filter(|m| mesh_proto::runtimes::tags_can_run(&m.tags, rt))
                        .count(),
                    None => targets.len(),
                };
                u32::try_from(n).unwrap_or(u32::MAX)
            })
        };
        let view = self.inner.engine.read(|s| {
            let placement =
                self.deployment_placement_refusal(s, &deployment, dry_run && inline_service);
            crate::deployments::check_view(
                &deployment,
                &policy,
                s,
                matching,
                placement,
                dry_run && inline_service,
            )
        });
        if dry_run {
            return IpcResponse::DeploymentCheck(Box::new(view));
        }
        if !view.issues.is_empty() {
            return IpcResponse::Error {
                message: view.issues.join("; "),
            };
        }
        let name = deployment.name.clone();
        let replacing = self
            .inner
            .engine
            .read(|s| s.deployments.get(&name).is_some_and(|e| !e.removed));
        let paused = deployment.paused;
        let generation = deployment.generation;
        let version = mesh_proto::DeploymentVersion {
            deployment: deployment.clone(),
            policy: policy.clone(),
        };
        match crate::deployments::put(&self.inner.engine, deployment) {
            Ok(()) => {
                // The policy follows the record it governs, never precedes
                // it: a policy that lands first governs nothing until the
                // deployment arrives, which is inert and fine, whereas a
                // deployment that starts reconciling before its canary
                // count arrives would roll the whole thing at once — once,
                // unrecoverably, in the window between two ops.
                if let Err(e) = crate::deployments::put_policy(&self.inner.engine, policy) {
                    warn!(deployment = %name, "deployment policy not published: {e}");
                    return IpcResponse::Error {
                        message: format!(
                            "deployment '{name}' was published but its rollout policy was not: {e}"
                        ),
                    };
                }
                // Phase B, E1: what was just published, both halves, for
                // `deployment history`. Secondary to the publish it records
                // — a running deployment is not failed by a missing line in
                // its history — so a failure is logged, not returned.
                if let Err(e) = crate::deployments::put_version(&self.inner.engine, version) {
                    warn!(deployment = %name, "deployment version not recorded: {e}");
                }
                let action = match (replacing, paused) {
                    (true, true) => "deployment.pause",
                    (true, false) => "deployment.put",
                    (false, _) => "deployment.create",
                };
                self.audit_ok(
                    &user,
                    action,
                    &name,
                    &format!("generation {generation}"),
                    env,
                );
                IpcResponse::Ok
            }
            Err(e) => IpcResponse::Error {
                message: e.to_string(),
            },
        }
    }

    /// Fire one occurrence: resolve the service, build the job, place it.
    async fn fire_schedule(&self, schedule: &mesh_proto::ServiceSchedule, occurrence: u64) {
        let job_id = mesh_proto::JobId::derive_occurrence(&schedule.name, occurrence);
        // Idempotency, the same rule `start_instance` follows: a job already
        // in state was fired by somebody who got here first.
        if self.inner.engine.read(|s| s.jobs.contains_key(&job_id)) {
            return;
        }
        let def = self.inner.engine.read(|s| match schedule.service_version {
            Some(v) => s
                .services
                .get(&schedule.service)
                .and_then(|e| e.versions.get(&v).cloned()),
            None => s.service_current(&schedule.service).cloned(),
        });
        let Some(def) = def else {
            warn!(
                schedule = %schedule.name,
                service = %schedule.service_ref(),
                "not fired: the service version is not available on this node yet"
            );
            return;
        };
        let exec =
            match crate::services::resolve_service_version(&def, schedule.inputs.clone(), None) {
                Ok(exec) => exec,
                Err(e) => {
                    warn!(schedule = %schedule.name, "not fired: {e}");
                    return;
                }
            };

        // Placement: the ordinary scorer over the schedule's own selector,
        // so a scheduled job lands where an identical hand-submitted one
        // would. Nothing about being scheduled changes where work goes.
        let selector = Selector::parse(&schedule.targets);
        let reqs = mesh_proto::Requirements::from_env(&def.env);
        let pressure_of = self.pressure_lookup();
        let targets = self.inner.engine.read(|s| {
            membership::resolve_feasible(
                s,
                &selector,
                self.node_id(),
                |id| self.inner.gossip.liveness_of(id),
                &reqs,
                &pressure_of,
            )
        });
        let ranked = self.inner.engine.read(|s| {
            scheduler::rank_with_pressure(
                s,
                targets,
                Some(&def.bundle_id),
                self.node_id(),
                |id| self.inner.transport.rtt_micros(id),
                &pressure_of,
                self.placement(),
            )
        });
        if ranked.is_empty() {
            // Not an error, and deliberately not a skip either: the
            // occurrence has not fired, so the next tick will try it again
            // until the catch-up grace runs out. A mesh where nothing
            // matches at 03:00 and something does at 03:02 should run the
            // backup at 03:02.
            warn!(
                schedule = %schedule.name,
                targets = %schedule.targets,
                "not fired yet: no node matches"
            );
            return;
        }

        let mut env = exec.env.clone();
        env.push((mesh_proto::SCHEDULE_ENV.to_string(), schedule.name.clone()));
        env.push((
            mesh_proto::SCHEDULE_OCCURRENCE_ENV.to_string(),
            occurrence.to_string(),
        ));
        let spec = JobSpec {
            job_id,
            payload: JobPayload::Service(Box::new(exec)),
            env,
            timeout_secs: schedule.timeout_secs,
            priority: schedule.priority,
            submitter: self.node_id(),
            submitter_user: schedule.created_by,
            auth: schedule_fire_envelope(schedule, occurrence),
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        };
        // Best candidate first, and on to the next when one is full or
        // cannot be reached — as a job submitted by hand would (ENG-6).
        let mut result = Err(CoreError::Job("no candidate".into()));
        let mut node = self.node_id();
        for candidate in &ranked {
            node = candidate.node_id;
            result = self.deliver(node, spec.clone(), Duration::ZERO).await;
            match &result {
                Err(e) if may_spill(e) => {
                    info!(schedule = %schedule.name, node = %candidate.name,
                        "{e}; trying the next candidate");
                    self.inner
                        .dispatch_failures
                        .spilled
                        .fetch_add(1, Ordering::Relaxed);
                }
                _ => break,
            }
        }
        match result {
            Ok(()) => {
                info!(
                    schedule = %schedule.name,
                    job = %job_id.short(),
                    on = %node.short(),
                    "schedule fired"
                );
                crate::audit::record(
                    &self.inner.engine,
                    schedule.created_by,
                    "schedule.fired",
                    &schedule.name,
                    "fired",
                    &format!(
                        "occurrence {occurrence} -> job {} on {}",
                        job_id.short(),
                        node.short()
                    ),
                    &schedule.signature,
                );
            }
            // **Audited, not only logged** (ENG-27). A fire that came due,
            // tried every candidate and failed was a `warn!` and nothing
            // else — so the one state an operator most needs to find, weeks
            // later, was the one state the audit trail did not have. A
            // *skip* is audited and a *success* is audited; this is the
            // gap between them, and it is the interesting one.
            //
            // Once per occurrence, on the same rule and for the same reason
            // `record_schedule_skip` follows it: `AuditAppend` never
            // compacts, and a schedule retrying every tick until its
            // catch-up grace runs out must not turn one failure into a
            // permanent row per attempt.
            Err(e) => {
                warn!(
                    schedule = %schedule.name,
                    "fire failed on {}: {e}",
                    node.short()
                );
                let marker = format!("occurrence {occurrence} ");
                let already = self.inner.engine.read(|s| {
                    s.audit.values().rev().take(200).any(|a| {
                        a.action == "schedule.failed"
                            && a.target == schedule.name
                            && a.detail.starts_with(&marker)
                    })
                });
                if !already {
                    crate::audit::record(
                        &self.inner.engine,
                        schedule.created_by,
                        "schedule.failed",
                        &schedule.name,
                        "failed",
                        &format!(
                            "occurrence {occurrence} could not be placed: {e} (last candidate {}, \
                             {} tried)",
                            node.short(),
                            ranked.len()
                        ),
                        &schedule.signature,
                    );
                }
            }
        }
    }

    /// Record an occurrence that came due and is deliberately not running.
    ///
    /// **Audited, once per occurrence.** A schedule that has quietly done
    /// nothing for three weeks is this feature's likeliest failure, and the
    /// only defence is that not-firing is exactly as visible as firing.
    fn record_schedule_skip(
        &self,
        schedule: &mesh_proto::ServiceSchedule,
        occurrence: u64,
        skip: &crate::schedules::Skip,
    ) {
        let marker = format!("occurrence {occurrence} ");
        let already = self.inner.engine.read(|s| {
            s.audit.values().rev().take(200).any(|e| {
                e.action == "schedule.skipped"
                    && e.target == schedule.name
                    && e.detail.starts_with(&marker)
            })
        });
        if already {
            return;
        }
        let reason = skip.reason();
        info!(schedule = %schedule.name, reason, "schedule occurrence skipped");
        crate::audit::record(
            &self.inner.engine,
            schedule.created_by,
            "schedule.skipped",
            &schedule.name,
            "skipped",
            &format!("{marker}{reason}"),
            &schedule.signature,
        );
    }

    /// One pass over every active deployment this node is elected for.
    async fn deployment_tick(&self) {
        // N6 §12. The half of the shed that matters when a mesh is
        // thrashing: this is the loop that keeps starting things while an
        // operator is trying to stop them.
        //
        // A *skip*, not a pause flag on each deployment: the record is
        // mesh-wide and lifting it must bring every deployment back
        // without touching any of them, which a per-deployment edit could
        // not promise. Running instances are untouched — a shed is about
        // what starts next.
        if self.shed().is_some_and(|s| s.reconcile) {
            return;
        }
        let me = self.node_id();
        let now = mesh_proto::now_micros();
        let founder = self.inner.engine.genesis().body.founder_user;
        // What this node learned placing instances on earlier ticks, minus
        // anything whose record has since arrived (Phase B hardening).
        let hints = {
            let mut memory = self.inner.placements.lock();
            memory.prune(now, |id| {
                self.inner.engine.read(|s| s.jobs.contains_key(id))
            });
            crate::deployments::PlacementHints {
                // Phase C, E8: instances pack or spread the way a `run` from
                // this node does.
                fit: self.placement(),
                ..memory.hints(now)
            }
        };
        // Decide everything under one read; act outside it, exactly as the
        // schedule tick does — a start authors a job and a cancel crosses
        // the network, and neither belongs inside the state lock.
        let plans: Vec<(
            mesh_proto::Deployment,
            mesh_proto::DeploymentPolicy,
            crate::deployments::ReconcilePlan,
        )> = self.inner.engine.read(|s| {
            s.deployments
                .values()
                .filter(|e| e.is_active())
                .filter(|e| {
                    crate::deployments::creator_may_still_run(s, &founder, &e.deployment, now)
                })
                .filter(|e| {
                    crate::deployments::elected_reconciler(s, &e.deployment.name, |id| {
                        self.inner.gossip.liveness_of(id)
                    }) == Some(me)
                })
                .map(|e| {
                    let dep = e.deployment.clone();
                    let candidates = self.deployment_candidates(s, &dep, false);
                    let policy = crate::deployments::policy_for(s, &dep.name);
                    let plan = crate::deployments::plan_reconcile_hinted(
                        &dep,
                        s,
                        &candidates,
                        now,
                        &policy,
                        |id| self.inner.gossip.not_alive_since_micros(id),
                        &hints,
                    );
                    (dep, policy, plan)
                })
                .collect()
        });

        // Removed deployments still elect a node, and their instances are
        // ordinary jobs that must be retired (§6.2). Gather teardown cancels
        // separately — a tombstone is not `is_active`, so the pass above
        // skips it by design.
        let teardowns: Vec<crate::deployments::CancelInstance> = self.inner.engine.read(|s| {
            s.deployments
                .values()
                .filter(|e| e.removed)
                .filter(|e| {
                    crate::deployments::elected_reconciler(s, &e.deployment.name, |id| {
                        self.inner.gossip.liveness_of(id)
                    }) == Some(me)
                })
                .flat_map(|e| {
                    let candidates = self.deployment_candidates(s, &e.deployment, false);
                    crate::deployments::plan_teardown(&e.deployment, s, &candidates)
                })
                .collect()
        });
        for cancel in &teardowns {
            info!(
                job = %cancel.job_id.short(),
                why = %cancel.why,
                "retiring deployment instance"
            );
            let _ = self.ipc_cancel(cancel.job_id).await;
        }

        for (dep, policy, plan) in plans {
            if let Some(reason) = &plan.degraded {
                self.record_deployment_degraded(&dep, reason);
            }
            // N1 P3. A stalled roll is not a crash loop — nothing may have
            // failed at all — so it is recorded under its own action and,
            // when the policy asked for it, reverted.
            // A stall that is about machines, not the release, is said on the
            // instance rows by `deployment.unplaced` and is not recorded against
            // this version — that record is what a later revert skips — and is
            // never answered by reverting a release whose instances are healthy.
            if let Some(reason) = plan.stalled.as_ref().filter(|_| !plan.stall_is_placement) {
                self.record_deployment_stalled(&dep, reason);
                if policy.auto_revert {
                    self.auto_revert(&dep, &policy, reason).await;
                    // The revert is a new generation; this generation's
                    // plan is stale the moment it lands, so nothing else
                    // from it should be executed.
                    continue;
                }
            }
            for cancel in &plan.cancels {
                info!(
                    deployment = %dep.name,
                    job = %cancel.job_id.short(),
                    why = %cancel.why,
                    "retiring deployment instance"
                );
                let _ = self.ipc_cancel(cancel.job_id).await;
            }
            // Phase C, E7: a slot the deployment's own rules left nowhere to
            // go says so where a refused start does.
            for unplaced in &plan.unplaced {
                self.record_unplaced_slot(&dep, unplaced.generation, &unplaced.slot, &unplaced.why);
            }
            for start in &plan.starts {
                self.start_instance(&dep, start).await;
            }
        }
    }

    /// Record that a roll stopped making progress, once per generation
    /// (N1 P3). The `record_deployment_degraded` dedup rule verbatim.
    fn record_deployment_stalled(&self, dep: &mesh_proto::Deployment, reason: &str) {
        let marker = format!("generation {} ", dep.generation);
        let already = self.inner.engine.read(|s| {
            s.audit.values().rev().take(200).any(|e| {
                e.action == "deployment.stalled"
                    && e.target == dep.name
                    && e.detail.starts_with(&marker)
            })
        });
        if already {
            return;
        }
        warn!(deployment = %dep.name, reason, "deployment rollout has stalled");
        // The version rides at the end of the detail (Phase A, §2.5a):
        // `auto_revert` reads these rows to skip versions that stalled
        // before, and the marker above stays a prefix so the dedup rule
        // still matches rows written before this existed.
        crate::audit::record(
            &self.inner.engine,
            dep.created_by,
            "deployment.stalled",
            &dep.name,
            "stalled",
            &format!(
                "{marker}{reason} {}{})",
                crate::deployments::STALLED_VERSION_TAG,
                dep.service_version
            ),
            &dep.signature,
        );
    }

    /// Point a stalled deployment back at the version it rolled from
    /// (N1 P3, `auto_revert`).
    ///
    /// ## Who signs it
    ///
    /// **This node's own user identity, and nothing else.** A deployment
    /// record carries a user's signature over its own bytes, and the merge
    /// verifies it against that user's grants — so a node cannot mint one
    /// on somebody else's behalf, and a design where it could would make
    /// every node in the mesh able to publish deployments as an admin.
    ///
    /// The consequences are worth stating plainly, because they are the
    /// feature's real shape:
    ///
    /// - A node with **no openable user key** — a worker that never
    ///   enrolled a user, or one whose key is behind a passphrase nobody
    ///   has typed — cannot revert. It says so, loudly, once.
    /// - A node whose user **lacks `DEPLOYMENTS`** authors an op the mesh
    ///   drops at merge. That is the ordinary RBAC path and it is correct:
    ///   auto-revert is not an escape from who may change what.
    ///
    /// Both are reported rather than swallowed, because a rollback that
    /// silently did not happen is worse than one that never claimed it
    /// would — and the roll is already stalled, so somebody is about to
    /// look.
    ///
    /// ## What it reverts to
    ///
    /// `service_version - 1`, at `generation + 1`. Not "the last
    /// generation that went fully healthy", which would need a history
    /// this record does not keep: a deployment carries one version and one
    /// generation, and the version before the one that stalled is the one
    /// that was running when somebody rolled it. A deployment at version 1
    /// has nothing to revert to and says so.
    async fn auto_revert(
        &self,
        dep: &mesh_proto::Deployment,
        policy: &mesh_proto::DeploymentPolicy,
        reason: &str,
    ) {
        let _ = policy;
        // **Phase A, §2.5a.** The newest earlier version whose own rollout
        // never stalled — not simply the one before, which is how two bad
        // versions in a row used to revert onto each other. The history is
        // the audit log every node already replicates: each stall and each
        // revert names the version it was about.
        let (target, all_stalled) = self.inner.engine.read(|s| {
            let published: Vec<u32> = s
                .services
                .get(&dep.service)
                .map(|e| e.versions.keys().copied().collect())
                .unwrap_or_default();
            let stalled = crate::deployments::stalled_versions(
                s.audit
                    .values()
                    .filter(|e| e.target == dep.name)
                    .map(|e| (e.action.as_str(), e.detail.as_str())),
            );
            crate::deployments::revert_target(dep.service_version, &published, &stalled)
        });
        let Some(target) = target else {
            warn!(
                deployment = %dep.name,
                "auto_revert is set but {} has no earlier version to return to",
                dep.service_ref()
            );
            return;
        };
        if all_stalled {
            warn!(
                deployment = %dep.name,
                to = target,
                "every earlier version of {} has stalled a rollout too; reverting one back anyway",
                dep.service
            );
        }
        let Some(user) = self.inner.identity.user.as_ref() else {
            warn!(
                deployment = %dep.name,
                "auto_revert is set and this roll has stalled, but this node holds no openable \
                 user key and cannot sign the revert — roll back by hand, or elect a node that can"
            );
            return;
        };
        // Author it as an ordinary edit: fetch, flip, bump, re-sign. The
        // generation bump is what makes the revert a *forward* roll to an
        // older, immutable version rather than a rewrite of history.
        let mut reverted = dep.clone();
        reverted.service_version = target;
        reverted.generation = dep.generation.saturating_add(1);
        let signed = crate::deployments::sign(user, reverted);
        let by = signed.created_by;
        match crate::deployments::put(&self.inner.engine, signed) {
            Ok(()) => {
                warn!(
                    deployment = %dep.name,
                    from = dep.service_version,
                    to = target,
                    reason,
                    "auto-reverting a stalled rollout"
                );
                crate::audit::record(
                    &self.inner.engine,
                    by,
                    "deployment.auto_revert",
                    &dep.name,
                    "reverted",
                    &format!(
                        "generation {} stalled ({reason}); reverted {} -> {target}",
                        dep.generation, dep.service_version
                    ),
                    &dep.signature,
                );
            }
            Err(e) => warn!(deployment = %dep.name, "auto-revert could not be published: {e}"),
        }
    }

    /// Record that a deployment tripped its failure budget, once per
    /// generation. Read out of the audit log, the schedule-skip dedup rule,
    /// so a degraded deployment does not bury the log with one entry a tick.
    fn record_deployment_degraded(&self, dep: &mesh_proto::Deployment, reason: &str) {
        let marker = format!("generation {} ", dep.generation);
        let already = self.inner.engine.read(|s| {
            s.audit.values().rev().take(200).any(|e| {
                e.action == "deployment.degraded"
                    && e.target == dep.name
                    && e.detail.starts_with(&marker)
            })
        });
        if already {
            return;
        }
        warn!(deployment = %dep.name, reason, "deployment is degraded; stopping restarts");
        crate::audit::record(
            &self.inner.engine,
            dep.created_by,
            "deployment.degraded",
            &dep.name,
            "degraded",
            &format!("{marker}{reason}"),
            &dep.signature,
        );
    }

    /// Start one deployment instance: resolve the service version, build the
    /// instance job with its deployment authorization, and place it.
    async fn start_instance(
        &self,
        dep: &mesh_proto::Deployment,
        start: &crate::deployments::StartInstance,
    ) {
        // Idempotency: a job already in state was started by a previous
        // owner — adopt it rather than dispatching twice, the same invariant
        // the workflow dispatcher relies on.
        if self
            .inner
            .engine
            .read(|s| s.jobs.contains_key(&start.job_id))
        {
            return;
        }
        let def = self.inner.engine.read(|s| {
            s.services
                .get(&dep.service)
                .and_then(|e| e.versions.get(&dep.service_version).cloned())
        });
        let Some(def) = def else {
            warn!(
                deployment = %dep.name,
                service = %dep.service_ref(),
                "instance not started: service version is not available here yet"
            );
            self.record_unplaced(
                dep,
                start,
                &format!(
                    "{} has not reached the reconciling node yet",
                    dep.service_ref()
                ),
            );
            return;
        };
        let exec = match crate::services::resolve_service_version(&def, dep.inputs.clone(), None) {
            Ok(exec) => exec,
            Err(e) => {
                warn!(deployment = %dep.name, "instance not started: {e}");
                self.record_unplaced(
                    dep,
                    start,
                    &format!("the service could not be prepared: {e}"),
                );
                return;
            }
        };
        let slot_label = match &start.slot {
            crate::deployments::Slot::Replica(i) => i.to_string(),
            crate::deployments::Slot::Node(n) => self
                .inner
                .engine
                .read(|s| s.members.get(n).map(|m| m.info.name.clone()))
                .unwrap_or_else(|| n.short()),
        };
        let mut env = vec![
            (mesh_proto::DEPLOY_ENV.to_string(), dep.name.clone()),
            (
                mesh_proto::DEPLOY_GEN_ENV.to_string(),
                start.generation.to_string(),
            ),
            (mesh_proto::DEPLOY_SLOT_ENV.to_string(), slot_label),
        ];
        // N2 P2. `[deployment.network]` reaches the instance as the same
        // reserved key `meshnode run --port` produces, so the executor has
        // one thing to read and the request needed no wire change.
        env.extend(
            self.inner
                .engine
                .read(|s| crate::deployments::policy_for(s, &dep.name))
                .instance_env,
        );
        // Phase C, E10. Read from replicated state, like every executor does.
        let priority = self.inner.engine.read(|s| {
            mesh_proto::priority_of_env(&crate::deployments::policy_for(s, &dep.name).instance_env)
        });
        let spec = JobSpec {
            job_id: start.job_id,
            payload: JobPayload::Service(Box::new(exec)),
            env,
            // A service is supposed to stay up: no wall-clock timeout. Its
            // liveness is the health check's job, not a deadline's.
            timeout_secs: None,
            // The default of 50 is `Normal`, which is what every instance
            // queued as before a file could say otherwise.
            priority: JobPriority::from_score(priority),
            submitter: self.node_id(),
            submitter_user: dep.created_by,
            auth: deployment_fire_envelope(dep),
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        };
        // Offer it to the planned machine, then down the alternates while the
        // answer is a definite refusal (Phase B hardening). Before this a
        // refusal was a log line: nothing recorded it, nothing spilled, and the
        // next tick planned the same machine again.
        let mut spec = spec;
        let mut refusals: Vec<(NodeId, String)> = Vec::new();
        let mut busy: Vec<NodeId> = Vec::new();
        let mut round: Vec<NodeId> = std::iter::once(start.node)
            .chain(start.alternates.iter().copied())
            .collect();
        // **Phase C, E10 — a second round, allowed to make room.** Only when
        // every machine said no, some said it because they were full, and the
        // file asked for a priority above the default: a deployment that
        // says nothing about priority never preempts anything.
        for pass in 0..2 {
            if pass == 1 {
                if priority <= mesh_proto::DEFAULT_PRIORITY || busy.is_empty() {
                    break;
                }
                info!(
                    deployment = %dep.name,
                    priority,
                    machines = busy.len(),
                    "every machine was busy; offering the instance again, allowed to preempt"
                );
                spec.env
                    .push((mesh_proto::REQ_MAY_PREEMPT.to_string(), "1".to_string()));
                round = std::mem::take(&mut busy);
            }
            for node in round.clone() {
                match self.offer_instance(node, spec.clone()).await {
                    Offer::Accepted => {
                        self.inner.placements.lock().placed(
                            start.job_id,
                            node,
                            mesh_proto::now_micros(),
                        );
                        // Phase C, E8: `jobs explain` answers for an instance too,
                        // naming the machine that took it and whether that was the
                        // first choice.
                        if let Some(mut explain) = start.explain.clone() {
                            explain.chosen = Some(node);
                            explain.spilled = node != start.node;
                            self.stage_explain(start.job_id, explain);
                        }
                        info!(
                            deployment = %dep.name,
                            slot = %match &start.slot {
                                crate::deployments::Slot::Replica(i) => i.to_string(),
                                crate::deployments::Slot::Node(n) => n.short(),
                            },
                            generation = start.generation,
                            node = %self.member_name(&node),
                            passed_over = refusals.len(),
                            "deployment instance started"
                        );
                        return;
                    }
                    Offer::Refused { why, busy: full } => {
                        info!(
                            deployment = %dep.name,
                            node = %self.member_name(&node),
                            "instance refused: {why}; offering it to the next machine"
                        );
                        self.inner.placements.lock().refused(
                            start.job_id,
                            node,
                            mesh_proto::now_micros(),
                        );
                        if pass == 0 {
                            if full {
                                busy.push(node);
                            }
                            refusals.push((node, why));
                        } else if let Some(entry) = refusals.iter_mut().find(|(n, _)| *n == node) {
                            entry.1 =
                                format!("{}; nothing there could be preempted: {why}", entry.1);
                        }
                    }
                    Offer::Unknown(why) => {
                        // It may be running there. Pin it: offered to that machine
                        // again next tick, and to no other until the pin expires.
                        warn!(
                            deployment = %dep.name,
                            node = %self.member_name(&node),
                            "instance offer unanswered ({why}); waiting on that machine"
                        );
                        self.inner.placements.lock().placed(
                            start.job_id,
                            node,
                            mesh_proto::now_micros(),
                        );
                        return;
                    }
                }
            }
        }
        let why = refusals
            .iter()
            .map(|(node, why)| format!("{} refused it ({why})", self.member_name(node)))
            .collect::<Vec<_>>()
            .join("; ");
        warn!(deployment = %dep.name, "instance not placed: {why}");
        self.record_unplaced(dep, start, &why);
    }

    /// Offer one instance to one machine, and say which of the three things
    /// in [`Offer`] happened.
    async fn offer_instance(&self, node: NodeId, spec: JobSpec) -> Offer {
        if node == self.node_id() {
            // Local admission journals nothing unless it accepts, so every
            // error here is a refusal.
            return match self.accept_job(spec).await {
                Ok(()) => Offer::Accepted,
                Err(e) => Offer::Refused {
                    busy: matches!(e, CoreError::Busy { .. }),
                    why: e.to_string(),
                },
            };
        }
        let conn = match self.remote_conn(node).await {
            Ok(conn) => conn,
            // Nothing was sent.
            Err(e) => {
                return Offer::Refused {
                    why: format!("unreachable: {e}"),
                    busy: false,
                };
            }
        };
        match self
            .inner
            .transport
            .request_one(&conn, &Request::SubmitJob(spec))
            .await
        {
            Ok(Response::JobAccepted { .. }) => Offer::Accepted,
            Ok(Response::Busy { running, queued }) => Offer::Refused {
                why: CoreError::Busy { running, queued }.to_string(),
                busy: true,
            },
            Ok(Response::Error { message }) => Offer::Refused {
                why: message,
                busy: false,
            },
            Ok(other) => Offer::Unknown(format!("unexpected answer: {other:?}")),
            Err(e) => Offer::Unknown(e.to_string()),
        }
    }

    /// Say why an instance could not be placed, at most once per reason per
    /// half hour (Phase B hardening). Replicated, so `status` on every node
    /// and Director's instance rows show what this node learned.
    fn record_unplaced(
        &self,
        dep: &mesh_proto::Deployment,
        start: &crate::deployments::StartInstance,
        why: &str,
    ) {
        self.record_unplaced_slot(dep, start.generation, &start.slot, why);
    }

    /// [`Self::record_unplaced`] for a slot that never got as far as a start
    /// (Phase C, E7).
    fn record_unplaced_slot(
        &self,
        dep: &mesh_proto::Deployment,
        generation: u32,
        slot: &crate::deployments::Slot,
        why: &str,
    ) {
        let now = mesh_proto::now_micros();
        let (detail, already) = self.inner.engine.read(|s| {
            let detail = format!(
                "{}{why}",
                crate::deployments::unplaced_prefix(generation, &slot.label(s))
            );
            let already = s.audit.values().rev().take(500).any(|e| {
                e.action == crate::deployments::UNPLACED_ACTION
                    && e.target == dep.name
                    && e.detail == detail
                    && now.saturating_sub(e.at_micros) < crate::deployments::UNPLACED_DEDUP_MICROS
            });
            (detail, already)
        });
        if !already {
            crate::audit::record(
                &self.inner.engine,
                dep.created_by,
                crate::deployments::UNPLACED_ACTION,
                &dep.name,
                "unplaced",
                &detail,
                &dep.signature,
            );
        }
    }

    /// Narrow placement candidates to nodes that can actually run this
    /// inventory job (M12 §6.3 P3, and the 1.8 safety gate).
    ///
    /// Two filters, in this order, each with its own refusal:
    ///
    /// **Vantage points.** A job runs on one node, so the node has to reach
    /// *every* host the play targets — the intersection of the hosts'
    /// `reachable_from` selectors, not the union. An empty intersection is
    /// a real answer with a real remedy, and saying "split it across
    /// vantage points with `meshnode parallel --hosts`" is more use than
    /// "no live node matches selector Any".
    ///
    /// **Protocol.** A pre-1.8 executor has no code to materialize the
    /// inventory, would pass no `-i`, and would run the play against an
    /// implicit localhost. Configuring the wrong machine silently is worse
    /// than refusing, so this is where M12 refuses.
    /// Narrow candidates to the ones that can actually see the data (R4 P2).
    ///
    /// Returns the workers to keep and the names of any dropped, so the
    /// caller can say what happened rather than silently running on half the
    /// mesh.
    ///
    /// **Fails open, in three different ways, on purpose.** A node is
    /// excluded only when it *answered no*. A node that is too old to be
    /// asked, that does not answer, or that cannot be dialled is kept — it
    /// gets work exactly as it does today, and a genuinely missing path
    /// still fails at the item with the message it always had. The opposite
    /// default would let one unreachable peer empty the candidate list and
    /// turn a working run into "nowhere to run this".
    async fn filter_for_reach(
        &self,
        targets: Vec<MemberInfo>,
        paths: &[String],
        always: &[String],
        tools: &[String],
    ) -> (Vec<MemberInfo>, Vec<String>) {
        // The directory *and* a sample of real items: an unmounted mount
        // point is an empty directory that exists, so asking only about the
        // directory got a "yes" from nodes that could see nothing.
        //
        // `always` is asked about verbatim rather than sampled — it is the
        // declared *output* directory, and there is exactly one of it. Left
        // to the sampler it could be crowded out by four hundred inputs, or
        // collapse the common ancestor to `/` when the results land on a
        // different mount from the data.
        let mut ask = crate::reach::probe_paths(paths);
        for p in always {
            if std::path::Path::new(p).is_absolute() && !ask.contains(p) {
                ask.push(p.clone());
            }
        }
        if ask.is_empty() && tools.is_empty() {
            return (targets, Vec::new());
        }
        let tools: Vec<String> = tools.to_vec();
        let me = self.node_id();
        let mut keep = Vec::with_capacity(targets.len());
        let mut dropped = Vec::new();
        for m in targets {
            if m.node_id == me {
                // This node can answer for itself without a round trip.
                let path_ok = ask.iter().all(|p| std::path::Path::new(p).exists());
                let missing: Vec<&String> = tools.iter().filter(|t| !which_on_path(t)).collect();
                if !path_ok {
                    dropped.push(format!("{}: cannot see the data", m.name));
                } else if let Some(t) = missing.first() {
                    dropped.push(format!("{}: {t} is not installed", m.name));
                } else {
                    keep.push(m);
                }
                continue;
            }
            if self.inner.transport.peer_minor(&m.node_id).unwrap_or(0) < mesh_proto::REACH_MINOR {
                keep.push(m);
                continue;
            }
            let addrs = self
                .inner
                .engine
                .read(|s| s.members.get(&m.node_id).map(|x| x.info.addrs.clone()))
                .unwrap_or_default();
            let Ok(conn) = self.inner.transport.member_conn(m.node_id, &addrs).await else {
                keep.push(m);
                continue;
            };
            match self
                .inner
                .transport
                .request_one(
                    &conn,
                    &Request::CanReach {
                        paths: ask.clone(),
                        tools: tools.clone(),
                    },
                )
                .await
            {
                Ok(Response::Reachable { ok, tools_ok }) => {
                    // Only an explicit "no" excludes. A short or absent
                    // answer is treated as "did not say", which keeps the
                    // node — see the fail-open note above.
                    if ok.iter().any(|v| !v) {
                        dropped.push(format!("{}: cannot see the data", m.name));
                    } else if let Some(i) = tools_ok.iter().position(|t| !t) {
                        let name = tools
                            .get(i)
                            .map(String::as_str)
                            .unwrap_or("a required tool");
                        dropped.push(format!("{}: {name} is not installed", m.name));
                    } else {
                        keep.push(m);
                    }
                }
                _ => keep.push(m),
            }
        }
        (keep, dropped)
    }

    /// Narrow candidates to peers that can decode a `RunScript` envelope
    /// (1.15, `mesh_proto::script`).
    ///
    /// The same shape as the inventory gate above and for a stricter
    /// reason: an older executor handed one of these does not do the wrong
    /// thing, it fails to parse the authorization and reports a malformed
    /// payload — true, and useless to whoever has to work out why their
    /// build stopped. Refusing to place says the actual thing.
    fn narrow_to_script_capable(
        &self,
        mut targets: Vec<MemberInfo>,
    ) -> std::result::Result<Vec<MemberInfo>, String> {
        let me = self.node_id();
        let before = targets.len();
        targets.retain(|m| {
            m.node_id == me
                || self
                    .inner
                    .transport
                    .peer_minor(&m.node_id)
                    .unwrap_or(mesh_proto::FARM_MINOR)
                    >= mesh_proto::FARM_MINOR
        });
        if targets.is_empty() {
            return Err(format!(
                "all {before} candidate node(s) predate protocol 1.{} and cannot run a script \
                 carried in the request — upgrade them, or narrow the selector to ones that \
                 can. (`meshnode cc` falls back to compiling locally rather than failing a \
                 build over this.)",
                mesh_proto::FARM_MINOR
            ));
        }
        Ok(targets)
    }

    fn node_rows(&self) -> Vec<NodeRow> {
        let me = self.node_id();
        self.inner.engine.read(|s| {
            let mut rows: Vec<NodeRow> = s
                .members
                .values()
                .map(|m| {
                    let revoked = s.revoked.contains_key(&m.info.node_id);
                    NodeRow {
                        node_id: m.info.node_id,
                        name: m.info.name.clone(),
                        addrs: m.info.addrs.clone(),
                        tags: mesh_proto::user_tags(&m.info.tags),
                        capacity: mesh_proto::Capacity::from_tags(&m.info.tags),
                        liveness: if revoked || m.left {
                            Liveness::Dead
                        } else {
                            self.inner.gossip.liveness_of(&m.info.node_id)
                        },
                        is_self: m.info.node_id == me,
                        revoked,
                        left: m.left,
                        joined_at_micros: m.info.joined_at_micros,
                        stale: self.inner.gossip.legacy_lag(&m.info.node_id).map(|lag| {
                            mesh_proto::StalePeer {
                                minor: lag.minor,
                                authors_blocked: u32::try_from(lag.authors).unwrap_or(u32::MAX),
                                ops_withheld: lag.ops,
                            }
                        }),
                        accepts_terminals: mesh_proto::accepts_terminals(&m.info.tags),
                        // This node is never behind itself, and saying
                        // "100%" about it would put a meaningless row in
                        // every fleet-wide sync readout.
                        version: mesh_proto::version_of(&m.info.tags),
                        contributes: s.may_execute(&m.info.node_id),
                        quarantined: s.quarantine(&m.info.node_id).map(|q| q.reason.clone()),
                        contribute_reason: s
                            .execution_refusal(&m.info.node_id)
                            .map(|d| d.reason.clone())
                            .unwrap_or_default(),
                        // The assigned half, so a surface offering to
                        // remove a tag can tell which ones it is actually
                        // able to remove from here.
                        assigned_tags: s
                            .assigned(&m.info.node_id)
                            .map(|a| a.tags.clone())
                            .unwrap_or_default(),
                        sync: (m.info.node_id != me)
                            .then(|| self.inner.gossip.sync_lag(&m.info.node_id))
                            .flatten()
                            .map(|lag| mesh_proto::SyncProgress {
                                behind: lag.behind,
                                total: lag.total,
                                authors: lag.authors,
                                age_micros: u64::try_from(lag.observed.elapsed().as_micros())
                                    .unwrap_or(u64::MAX),
                            }),
                    }
                })
                .collect();
            rows.sort_by_key(|r| (!r.is_self, r.node_id));
            rows
        })
    }

    // -- M10 workflow runs (§4.2) -----------------------------------------
    //
    // The fan-out driver, one graph up. Everything structural is the same:
    // the plan replicates before the first dispatch, progress is read back
    // from replicated job summaries rather than held in the driver's head,
    // a successor elected by rendezvous hashing resumes a dead owner's run
    // from that same evidence, and pre-allocated job ids make re-dispatch
    // idempotent. What is new is only *which* work to start next, and that
    // is a pure function living in `wfrun`.

    /// Every job, newest first — the whole table, for
    /// [`IpcRequest::Jobs`].
    ///
    /// Kept because older clients send that variant, and now
    /// served through the same machinery [`Self::jobs_page`] uses
    /// (**H-1**): the availability of every row is worked out *after* the
    /// state lock is released, so a listing no longer holds up replication
    /// for the length of a disk scan.
    ///
    /// **Still unbounded, deliberately.** `Jobs` is defined as "all jobs"
    /// and answers with a bare `Vec<JobRow>` that has nowhere to admit a
    /// truncation, so capping it would turn a loud failure (a response
    /// past `MAX_FRAME_LEN`) into a silent wrong answer. Anything new
    /// should send `JobsQuery`, which is bounded and says how much it left
    /// behind.
    fn job_rows(&self) -> Vec<JobRow> {
        self.jobs_page(&mesh_proto::JobsQuery {
            limit: mesh_proto::JobsQuery::UNBOUNDED,
            ..Default::default()
        })
        .rows
    }

    /// One page of the live job table: filtered, counted, sorted and
    /// bounded (**H-1**).
    ///
    /// **The shape is the fix.** The old listing built a `JobRow` for every
    /// job the mesh remembers, and each row asked the disk whether the
    /// output was still here — a `stat()`, and for a finished job whose
    /// spool had gone, a redb read — *inside* `engine.read`, which is the
    /// lock every arriving op needs. Listing jobs therefore cost one
    /// syscall per job and paused replication while it ran, twice a second
    /// for as long as a Director window was open.
    ///
    /// Three things changed and each removes one term:
    ///
    /// 1. **`has_record` is a lookup** ([`crate::jobs::JobManager::spooled_ids`]),
    ///    so "are the bytes here" costs nothing.
    /// 2. **Only the returned page resolves availability**, so the redb
    ///    reads are bounded by `limit` rather than by history.
    /// 3. **Nothing that touches the disk happens under the lock.** The
    ///    closure collects the rows and the handful of facts about their
    ///    executors that only replicated state knows; the answer is
    ///    finished outside.
    ///
    /// Counts are tallied over the *unfiltered* table in the same pass, so
    /// a paged UI can still show "12 running of 40 000" without a second
    /// request.
    fn jobs_page(&self, query: &mesh_proto::JobsQuery) -> mesh_proto::JobsPage {
        use mesh_proto::JobsPage;

        let me = self.node_id();
        let limit = query.effective_limit();
        let offset = query.offset as usize;
        // Lowercased once, not once per row.
        let prefix = query
            .prefix
            .as_deref()
            .map(|p| p.trim().trim_start_matches("mwj1").to_ascii_lowercase());
        let contains = query.contains.as_deref().map(str::to_ascii_lowercase);
        let state = query.state.as_deref().map(str::to_ascii_lowercase);

        struct Page {
            rows: Vec<JobRow>,
            total: u64,
            counts: mesh_proto::JobCounts,
            executors: HashMap<NodeId, (bool, String)>,
            /// Just enough of each returned job to finish its availability
            /// answer once the lock is released — see [`AvailFacts`].
            avail: Vec<AvailFacts>,
        }

        let page = self.inner.engine.read(|s| {
            let mut counts = mesh_proto::JobCounts::default();
            let mut matched: Vec<&JobSummary> = Vec::new();
            for j in s.jobs.values() {
                counts.tally(j.state);
                if !job_matches(
                    j,
                    prefix.as_deref(),
                    state.as_deref(),
                    contains.as_deref(),
                    query,
                ) {
                    continue;
                }
                matched.push(j);
            }
            let total = matched.len() as u64;
            // Newest first. `select_nth_unstable` would be cheaper for a
            // small page, but the table is already in memory and the sort
            // is the only O(n log n) term left on this path.
            matched.sort_unstable_by_key(|j| std::cmp::Reverse(j.created_at_micros));
            let window: &[&JobSummary] = matched
                .get(offset..)
                .map(|rest| &rest[..rest.len().min(limit)])
                .unwrap_or(&[]);

            let mut executors: HashMap<NodeId, (bool, String)> = HashMap::new();
            let mut rows = Vec::with_capacity(window.len());
            let mut avail = Vec::with_capacity(window.len());
            for j in window {
                executors.entry(j.executor).or_insert_with(|| {
                    let member = s.members.get(&j.executor);
                    let gone = member.is_none_or(|m| m.left) || s.revoked.contains_key(&j.executor);
                    (
                        gone,
                        member.map(|m| m.info.name.clone()).unwrap_or_default(),
                    )
                });
                rows.push(JobRow {
                    // Filled in below, off the lock.
                    output: mesh_proto::OutputAvailability::Available,
                    job_id: j.job_id,
                    cmd_preview: j.cmd_preview.clone(),
                    runtime: j.runtime.clone(),
                    submitter: j.submitter,
                    submitter_user: j.submitter_user,
                    submitter_username: s
                        .users
                        .get(&j.submitter_user)
                        .map(|u| u.record.username.clone()),
                    executor: j.executor,
                    executor_name: s.members.get(&j.executor).map(|m| m.info.name.clone()),
                    state: j.state,
                    exit_code: j.exit_code,
                    created_at_micros: j.created_at_micros,
                    finished_at_micros: j.finished_at_micros,
                    fanout: j.fanout,
                });
                avail.push(AvailFacts {
                    job_id: j.job_id,
                    state: j.state,
                    executor: j.executor,
                    finished_at_micros: j.finished_at_micros,
                });
            }
            Page {
                rows,
                total,
                counts,
                executors,
                avail,
            }
        });

        let Page {
            mut rows,
            total,
            counts,
            executors,
            avail,
        } = page;
        // Off the lock: the per-row answer may `stat()` and may read redb,
        // and must not do either while replication is waiting.
        for (row, job) in rows.iter_mut().zip(avail.iter()) {
            row.output = self.output_availability_with(job, me, &executors);
        }
        JobsPage {
            rows,
            total,
            counts,
        }
    }

    /// Where a job's output is, and why you cannot see it (M31 A2).
    ///
    /// **Every reader goes through here.** The rule A2 exists for — that a
    /// pruned log reads *"output expired"* and never *"not found"* or
    /// *"failed"* — is exactly the kind that holds in three renderers and
    /// fails in the fourth, so there is one resolver and no caller composes
    /// its own answer.
    ///
    /// The order of the checks is the whole design:
    ///
    /// 1. **Still running, or the bytes are here** → available. Cheapest
    ///    and commonest; nothing else is consulted.
    /// 2. **Not terminal** → available. A queued job has no output *yet*,
    ///    which is not the same as having lost it, and calling that
    ///    "expired" would be the first lie.
    /// 3. **The producer left the mesh** → expired, `ProducerGone`. Not
    ///    "unavailable": waiting will never help, and saying so would send
    ///    an operator to look for a machine that no longer exists.
    /// 4. **The producer is offline** → unavailable, named. **Temporary**,
    ///    and the single most important distinction in this function: a
    ///    machine that is merely asleep must never read as expired, because
    ///    an operator who learns the states are approximate stops believing
    ///    "expired" too.
    /// 5. **The producer is alive** → available, and the fetch is what
    ///    discovers otherwise. Guessing here would mean deciding a remote
    ///    node's retention from this one's config, which is the C9 mistake.
    /// 6. **We are the producer and the bytes are gone** → expired,
    ///    `Retention`, dated from the finish instant plus this node's
    ///    resolved window.
    fn output_availability_in(
        &self,
        state: &crate::state::MeshState,
        job: &mesh_proto::JobSummary,
        me: NodeId,
    ) -> mesh_proto::OutputAvailability {
        let member = state.members.get(&job.executor);
        let gone = member.is_none_or(|m| m.left) || state.revoked.contains_key(&job.executor);
        let executors = HashMap::from([(
            job.executor,
            (
                gone,
                member.map(|m| m.info.name.clone()).unwrap_or_default(),
            ),
        )]);
        self.output_availability_with(&AvailFacts::of(job), me, &executors)
    }

    /// [`Self::output_availability_in`] with the one thing it needs from
    /// replicated state already extracted, so it can run **off the state
    /// lock** (**H-1**).
    ///
    /// The order of the checks is the design and is unchanged; what moved
    /// is only where the inputs come from. `executors` maps a job's
    /// executor to *(has it left or been revoked, what is it called)* — the
    /// only two questions this asks of `MeshState` — and it is gathered
    /// once per page rather than once per row. The disk work that is left
    /// (`has_record`, and the locator read behind it) now happens with the
    /// lock released, which is what stops a listing from pausing
    /// replication for the length of a scan.
    fn output_availability_with(
        &self,
        job: &AvailFacts,
        me: NodeId,
        executors: &HashMap<NodeId, (bool, String)>,
    ) -> mesh_proto::OutputAvailability {
        use mesh_proto::{ExpiryReason, OutputAvailability};

        // 1 & 2. Anything not finished still has its output ahead of it,
        // and anything whose bytes are on this disk is simply readable.
        if !job.state.is_terminal() || self.inner.jobs.has_record(&job.job_id) {
            return OutputAvailability::Available;
        }

        // 2b. **Exported, and no longer here** (M31 B3 §14.3 item 7). This
        // is the fourth state, and it ranks *below* a local copy and above
        // every kind of absence: if the bytes are on this disk we read them
        // from here, and if they are not, a locator is a better answer than
        // "expired".
        //
        // The locator is only ever *recorded*, never inferred: a sink that
        // exists is not a promise that this job's output went through it,
        // and softening `Expired` into `Elsewhere` on the strength of a
        // configured destination would be exactly the promise §14.3 item 7
        // forbids.
        if let Some((sink, key)) = self.payload_locator(&job.job_id) {
            return OutputAvailability::Elsewhere { sink, key };
        }

        if job.executor != me {
            // Absent from the map only when the caller did not look this
            // executor up — which is the same information "not a member"
            // carries, so it reads the same way.
            let (gone, name) = executors
                .get(&job.executor)
                .cloned()
                .unwrap_or((true, String::new()));
            if gone {
                // 3. Terminal in a different sense: that machine is not
                // coming back, so the bytes are not either.
                return OutputAvailability::Expired {
                    when_micros: job.finished_at_micros.unwrap_or(0),
                    reason: ExpiryReason::ProducerGone,
                };
            }
            if !matches!(
                self.inner.gossip.liveness_of(&job.executor),
                mesh_proto::Liveness::Alive
            ) {
                // 4. Asleep, not gone.
                return OutputAvailability::Unavailable {
                    node: job.executor,
                    node_name: name,
                };
            }
            // 5. Alive: ask it. Its retention is its own business (C9).
            return OutputAvailability::Available;
        }

        // 6. Ours, terminal, and no longer on disk: `[retention]` took it.
        // Dated from the finish instant plus the window that applied, which
        // is an honest "at or after this" rather than a deletion timestamp
        // nothing recorded.
        let days = self.resolved_retention().job_days;
        OutputAvailability::Expired {
            when_micros: job
                .finished_at_micros
                .map(|f| f.saturating_add(u64::from(days) * 86_400 * 1_000_000))
                .unwrap_or(0),
            reason: ExpiryReason::Retention,
        }
    }

    /// Where a sink put this job's output, if one did.
    ///
    /// Read from the local index rather than replicated state: the locator
    /// is the *exporting* node's discovery, recorded where A1's prune of
    /// the op-log row cannot reach it (§7 crossing 3). A node that never
    /// indexed the job has no locator to offer, and says so by returning
    /// `None` rather than guessing.
    fn payload_locator(&self, job_id: &mesh_proto::JobId) -> Option<(String, String)> {
        let row = self.inner.history.as_ref()?.job(job_id).ok()??;
        Some((row.payload_sink?, row.payload_key?))
    }

    /// [`Self::output_availability_in`] for a caller that does not already
    /// hold the state lock.
    pub fn output_availability(
        &self,
        job_id: &mesh_proto::JobId,
    ) -> Option<mesh_proto::OutputAvailability> {
        let me = self.node_id();
        self.inner.engine.read(|s| {
            s.jobs
                .get(job_id)
                .map(|j| self.output_availability_in(s, j, me))
        })
    }

    /// One row, by id.
    ///
    /// A point lookup since **H-1**: this used to build the whole table and
    /// scan it, so `jobs show` did the work of `jobs ls` on a mesh that had
    /// stopped being able to do `jobs ls` at all.
    fn job_row(&self, id: &mesh_proto::JobId) -> Option<JobRow> {
        let me = self.node_id();
        let (mut row, summary, executors) = self.inner.engine.read(|s| {
            let j = s.jobs.get(id)?;
            let member = s.members.get(&j.executor);
            let gone = member.is_none_or(|m| m.left) || s.revoked.contains_key(&j.executor);
            let executors = HashMap::from([(
                j.executor,
                (
                    gone,
                    member.map(|m| m.info.name.clone()).unwrap_or_default(),
                ),
            )]);
            let row = JobRow {
                output: mesh_proto::OutputAvailability::Available,
                job_id: j.job_id,
                cmd_preview: j.cmd_preview.clone(),
                runtime: j.runtime.clone(),
                submitter: j.submitter,
                submitter_user: j.submitter_user,
                submitter_username: s
                    .users
                    .get(&j.submitter_user)
                    .map(|u| u.record.username.clone()),
                executor: j.executor,
                executor_name: member.map(|m| m.info.name.clone()),
                state: j.state,
                exit_code: j.exit_code,
                created_at_micros: j.created_at_micros,
                finished_at_micros: j.finished_at_micros,
                fanout: j.fanout,
            };
            Some((row, AvailFacts::of(j), executors))
        })?;
        row.output = self.output_availability_with(&summary, me, &executors);
        Some(row)
    }

    /// The what-if planner (C8 P3): place a submission nobody made.
    ///
    /// **A simulation over the real scorer, not a second model of the
    /// scheduler.** Every number here comes from the same functions a
    /// submission would call — `resolve_feasible_with_devices` to filter,
    /// `rank_explained` to rank, the same live pressure, the same device
    /// telemetry — so the plan cannot drift from what the scheduler will
    /// actually do. A second implementation would agree on the day it was
    /// written and diverge silently forever after, which is the failure
    /// this design exists to avoid.
    ///
    /// **It reports a range, never a time.** Historical durations are a
    /// distribution; a planner that says "17 minutes" is wrong precisely
    /// often enough to be untrusted forever.
    async fn plan_estimate(
        &self,
        user: UserId,
        on: Option<String>,
        service: Option<String>,
        env: Vec<(String, String)>,
        fanout: Option<u32>,
    ) -> mesh_proto::PlanEstimate {
        let selector = Selector::parse(on.as_deref().unwrap_or("any"));
        let me = self.node_id();
        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        // Only for the refusal sentence below — the filter derives its own
        // from the same env, in the one place that decides.
        let device_req = mesh_proto::DeviceRequest::from_env(&env);
        let pressure_of = self.pressure_lookup();

        let empty = |refusal: String| mesh_proto::PlanEstimate {
            placement: mesh_proto::PlacementExplain {
                candidates: Vec::new(),
                chosen: None,
                considered: 0,
                spilled: false,
                at_micros: now,
            },
            duration: None,
            wait_secs: 0,
            finish: None,
            delays: Vec::new(),
            slots_free: 0,
            slots_wanted: fanout.unwrap_or(1).max(1),
            refusal: Some(refusal),
        };

        // T3. A plan must model the placement it is predicting, so it
        // filters on the runtime the real dispatch will filter on. A named
        // service carries its own; an ad-hoc command is a shell body.
        // Getting this wrong would make `--plan` promise a node the run
        // then refuses, which is worse than not planning at all.
        let planned_runtime = self.inner.engine.read(|s| {
            service
                .as_deref()
                .and_then(|name| s.services.get(name))
                .and_then(|e| e.current())
                .map(|def| def.runtime)
                .unwrap_or(mesh_proto::Runtime::Bash)
        });
        let placement_env = mesh_proto::runtimes::env_for_placement(&env, planned_runtime);
        let targets = self.inner.engine.read(|s| {
            membership::resolve_feasible_for_env(
                s,
                &selector,
                me,
                |id| self.inner.gossip.liveness_of(id),
                &placement_env,
                &pressure_of,
            )
        });
        if targets.is_empty() {
            let volumes = mesh_proto::VolumeRequests::from_env(&env);
            return empty(match (&device_req, volumes.is_empty()) {
                (Some(req), _) => format!("no live node advertises {}", req.describe()),
                // Named before the selector, because it is the more
                // specific answer: a volume nobody lends is a fact about
                // the mesh's storage, not about who matched `on`.
                (None, false) => format!("no live node lends {}", volumes.names().join(", ")),
                (None, true) => format!("no live node matches selector {selector:?}"),
            });
        }
        let bundle = service.as_ref().and_then(|name| {
            self.inner.engine.read(|s| {
                s.services
                    .get(name)
                    .and_then(|e| e.current().map(|v| v.bundle_id))
            })
        });
        let device_busy_of = |id: &NodeId| self.device_busy(id, device_req.as_ref());
        let (ranked, mut placement) = self.inner.engine.read(|s| {
            let allowed: Vec<MemberInfo> = targets
                .into_iter()
                .filter(|m| rbac::can_run_on(s, &founder, &user, m, service.as_deref(), now))
                .collect();
            scheduler::rank_explained(
                s,
                allowed,
                bundle.as_ref(),
                me,
                |id| self.inner.transport.rtt_micros(id),
                &pressure_of,
                |_| None,
                device_busy_of,
                self.placement(),
            )
        });
        if ranked.is_empty() {
            return empty("your grants do not cover any matching node".to_string());
        }
        // The planner names a winner where a real placement waits for one:
        // nothing is being dispatched, so there is no `Busy` to spill on,
        // and the top of the ranking is the honest answer to "where would
        // it go".
        placement.chosen = ranked.first().map(|m| m.node_id);

        let wanted = match fanout {
            Some(0) => ranked.len() as u32,
            Some(n) => n.min(ranked.len() as u32),
            None => 1,
        };
        // Free slots across the candidates it would actually use, from
        // replicated state: a node's advertised slots minus the non-terminal
        // jobs the mesh knows it is holding.
        let slots_free = self.inner.engine.read(|s| {
            ranked
                .iter()
                .take(wanted.max(1) as usize)
                .map(|m| {
                    let busy = s
                        .jobs
                        .values()
                        .filter(|j| j.executor == m.node_id && !j.state.is_terminal())
                        .count() as u32;
                    m.slots.saturating_sub(busy)
                })
                .sum::<u32>()
        });

        let duration = self.historic_duration(service.as_deref(), &ranked);
        // The wait is the queue in front of this submission, priced at the
        // *median* of the same distribution. Using p90 here would compound
        // pessimism across every job in the queue and produce a number no
        // operator would believe twice.
        let queue = self.inner.jobs.queue_rows();
        let ahead = queue.len() as u64;
        let per_job = duration.map(|d| d.p50_secs).unwrap_or(0);
        let wait_secs = if slots_free >= wanted {
            0
        } else {
            ahead.saturating_mul(per_job)
        };
        let finish = duration.map(|d| mesh_proto::DurationRange {
            p10_secs: d.p10_secs.saturating_add(wait_secs),
            p50_secs: d.p50_secs.saturating_add(wait_secs),
            p90_secs: d.p90_secs.saturating_add(wait_secs),
            samples: d.samples,
        });

        // …and whose work it delays. C1 ordered that queue by weighted
        // dominant share; a submission entering it pushes back exactly the
        // principals whose share is *above* the submitter's, because those
        // are the ones this job now runs in front of.
        let mine = self
            .inner
            .jobs
            .shares()
            .get(&user)
            .copied()
            .unwrap_or_default();
        let mut delayed: BTreeMap<UserId, (String, u32)> = BTreeMap::new();
        for row in &queue {
            if row.principal == user || row.share_permille < mine {
                continue;
            }
            let entry = delayed
                .entry(row.principal)
                .or_insert_with(|| (row.username.clone(), 0));
            entry.1 += 1;
        }
        let delays = delayed
            .into_iter()
            .map(|(user, (name, jobs))| mesh_proto::DelayedPrincipal {
                user,
                name,
                jobs,
                by_secs: per_job.saturating_mul(u64::from(wanted)),
            })
            .collect();

        mesh_proto::PlanEstimate {
            placement,
            duration,
            wait_secs,
            finish,
            delays,
            slots_free,
            slots_wanted: wanted,
            refusal: None,
        }
    }

    /// How long work like this has taken here before, from the M14 archive.
    ///
    /// Matched on the service name where there is one, and on the candidate
    /// nodes otherwise — the second is a weaker signal and the sample count
    /// on the returned range is what says so. `None` when nothing
    /// comparable has run, which is the honest answer: extrapolating a
    /// distribution from zero observations is how a planner earns its
    /// reputation for lying.
    fn historic_duration(
        &self,
        service: Option<&str>,
        candidates: &[MemberInfo],
    ) -> Option<mesh_proto::DurationRange> {
        let history = self.inner.history.as_ref()?;
        let query = mesh_proto::HistoryQuery {
            state: Some("succeeded".into()),
            contains: service.map(str::to_string),
            node: service
                .is_none()
                .then(|| candidates.first().map(|m| m.node_id))
                .flatten(),
            limit: 200,
            ..Default::default()
        };
        let page = history.query_jobs(&query).ok()?;
        let samples: Vec<u64> = page
            .rows
            .iter()
            .filter_map(|r| r.duration_micros())
            .map(|micros| micros / 1_000_000)
            .collect();
        mesh_proto::DurationRange::from_secs(samples)
    }

    /// Why a job landed where it did, and what the cache said (C8 P1).
    ///
    /// **Answered from the local index, or not answered.** A node that did
    /// not place the job has no breakdown and no honest way to invent one:
    /// re-scoring today's mesh would produce a confident answer to a
    /// different question. So it names the node that does have it and stops
    /// — which is a better product than a plausible fabrication, and is the
    /// same rule `meshnode results` already follows for fan-out rows.
    fn job_explain(&self, id: &mesh_proto::JobId) -> Option<mesh_proto::JobExplainView> {
        let row = self.job_row(id)?;
        let (placement, marker) = match &self.inner.history {
            Some(history) => (
                history.explain(id).ok().flatten(),
                history.cache_marker(id).ok().flatten(),
            ),
            None => (None, None),
        };
        Some(mesh_proto::JobExplainView {
            job: row.job_id,
            cmd_preview: row.cmd_preview.clone(),
            state: row.state.as_str().to_string(),
            executor: row.executor,
            executor_name: row
                .executor_name
                .clone()
                .unwrap_or_else(|| row.executor.short()),
            submitter_user: row.submitter_user,
            created_at_micros: row.created_at_micros,
            duration_micros: row
                .finished_at_micros
                .map(|end| end.saturating_sub(row.created_at_micros)),
            // Only when somebody *else* can answer. Telling an operator to
            // ask the node they are already on is worse than saying there
            // is no breakdown — and there genuinely is not one for a cache
            // hit, because nothing was placed.
            placement_on: (placement.is_none() && row.submitter != self.node_id())
                .then_some(row.submitter),
            placement,
            action_key: marker.as_ref().map(|m| m.key),
            cache_hit: marker.as_ref().and_then(|m| m.hit.clone()),
            key_fields: marker.map(|m| m.fields).unwrap_or_default(),
        })
    }

    /// Stage a placement breakdown for a job this node just dispatched.
    ///
    /// Bounded, and dropped oldest-first when it overflows: an unbounded
    /// map on the submission path is precisely the *memory leak with a
    /// chart on it* §12.2 refuses to ship. Losing an explanation costs an
    /// unanswerable question about one job; keeping every one costs the
    /// daemon.
    fn stage_explain(&self, job: JobId, explain: mesh_proto::PlacementExplain) {
        const EXPLAIN_PENDING_MAX: usize = 4096;
        let mut pending = self.inner.pending_explains.lock();
        if pending.len() >= EXPLAIN_PENDING_MAX {
            let drop_to = pending.len() + 1 - EXPLAIN_PENDING_MAX;
            pending.drain(..drop_to);
        }
        pending.push((job, explain));
    }

    /// Flush staged placement breakdowns and cache markers into the local
    /// index. Called by the history task, on its timer, off the submission
    /// path — writing an explanation is a redb commit and an fsync, and the
    /// job-submission path is the last place that belongs.
    async fn flush_explains(&self, history: &Arc<crate::history::HistoryStore>) {
        let staged: Vec<(JobId, mesh_proto::PlacementExplain)> = {
            let mut pending = self.inner.pending_explains.lock();
            std::mem::take(&mut *pending)
        };
        if staged.is_empty() {
            return;
        }
        let history = history.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut n = 0usize;
            for (job, explain) in staged {
                match history.put_explain(&job, &explain) {
                    Ok(()) => n += 1,
                    Err(e) => warn!(job = %job.short(), "placement explain write failed: {e}"),
                }
            }
            n
        })
        .await;
        if let Ok(n) = written
            && n > 0
        {
            debug!(explains = n, "indexed placement explanations");
        }
    }

    async fn ipc_cancel(&self, job_id: mesh_proto::JobId) -> IpcResponse {
        self.ipc_cancel_as(job_id, None).await
    }

    /// Serve [`Request::CancelJobV2`]: a cancel a person signed, arriving
    /// from the node they signed it to (S1).
    ///
    /// Four things are checked, in the order that makes a refusal cheapest:
    /// the envelope's own signature and freshness (`verify_envelope`, which
    /// also spends the nonce against *this* node's replay table), that the
    /// payload really is a cancel and names the job it claims to, and then
    /// `require_cancel` against this node's view of who submitted it. The
    /// forwarding peer is a courier throughout — it contributes nothing the
    /// signature does not already carry.
    async fn serve_cancel_v2(&self, env: SignedIpc) -> Response {
        let user = match self.verify_envelope(&env) {
            Ok(user) => user,
            Err(message) => return Response::Error { message },
        };
        let job_id = match postcard::from_bytes::<IpcRequest>(&env.payload) {
            Ok(IpcRequest::JobCancel { job_id }) => job_id,
            Ok(IpcRequest::JobCancelForce { job_id, .. }) => job_id,
            Ok(_) => {
                return Response::Error {
                    message: "forwarded envelope is not a cancel".into(),
                };
            }
            Err(e) => {
                return Response::Error {
                    message: format!("forwarded envelope will not decode: {e}"),
                };
            }
        };
        let target = job_id.to_display_string();
        match self.job_submitter(&job_id) {
            Some((submitter, submitter_name)) => {
                if let Err(resp) = self.gate_cancel(
                    &user,
                    &submitter,
                    submitter_name.as_deref(),
                    false,
                    "job.cancel",
                    &target,
                    &env,
                ) {
                    return Response::Error {
                        message: match *resp {
                            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                                message
                            }
                            other => format!("{other:?}"),
                        },
                    };
                }
            }
            // No summary here yet. The flat role gate still stands, so a
            // peer probing ids learns nothing it did not already know.
            None => {
                let founder = self.inner.engine.genesis().body.founder_user;
                let verdict = self.inner.engine.read(|s| {
                    rbac::require_role(s, &founder, &user, rbac::RUNNERS, mesh_proto::now_micros())
                });
                if let Err(message) = verdict {
                    return Response::Error { message };
                }
            }
        }
        match self.inner.jobs.cancel(&job_id) {
            Ok(()) => {
                self.audit_ok(&user, "job.cancel", &target, "", &env);
                Response::Ok
            }
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        }
    }

    /// Cancel, optionally carrying **the person's own envelope** onward to
    /// the executor (S1).
    ///
    /// `env` is `Some` exactly when a human asked: the IPC gate here has
    /// already applied `require_cancel` against this node's view, and
    /// forwarding the envelope lets the executor apply it against *its*
    /// view rather than taking a peer node's word for it. `None` is the
    /// system's own cancels — a losing race copy, a retired deployment
    /// instance, a run being torn down — which have no person behind them
    /// and were authorized at the level above.
    async fn ipc_cancel_as(
        &self,
        job_id: mesh_proto::JobId,
        env: Option<&SignedIpc>,
    ) -> IpcResponse {
        // Local (running or still queued)?
        if self.inner.jobs.is_active(&job_id) {
            return match self.inner.jobs.cancel(&job_id) {
                Ok(()) => IpcResponse::Ok,
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            };
        }
        // Remote: route to the executor.
        let executor = self
            .inner
            .engine
            .read(|s| s.jobs.get(&job_id).map(|j| j.executor));
        match executor {
            Some(exec) if exec == self.node_id() => IpcResponse::Error {
                message: "job is not running".into(),
            },
            Some(exec) => match self.remote_conn(exec).await {
                Ok(conn) => {
                    // V2 when there is a person to name and the executor can
                    // read it; V1 otherwise, because a cancel that silently
                    // did nothing through a rolling upgrade would be worse
                    // than one the executor authorizes less tightly.
                    let speaks_v2 = self
                        .inner
                        .transport
                        .peer_minor(&exec)
                        .is_some_and(|m| m >= mesh_proto::AUTHZ_MINOR);
                    let request = match env {
                        Some(env) if speaks_v2 => Request::CancelJobV2 {
                            env: Box::new(env.clone()),
                        },
                        _ => Request::CancelJob { job_id },
                    };
                    match self.inner.transport.request_one(&conn, &request).await {
                        Ok(Response::Ok) => IpcResponse::Ok,
                        Ok(Response::Error { message }) => IpcResponse::Error { message },
                        Ok(other) => IpcResponse::Error {
                            message: format!("unexpected: {other:?}"),
                        },
                        Err(e) => IpcResponse::Error {
                            message: e.to_string(),
                        },
                    }
                }
                Err(e) => IpcResponse::Error {
                    message: e.to_string(),
                },
            },
            None => IpcResponse::Error {
                message: "unknown job".into(),
            },
        }
    }

    async fn remote_conn(&self, node_id: NodeId) -> Result<quinn::Connection> {
        let addrs = self
            .inner
            .engine
            .read(|s| s.members.get(&node_id).map(|m| m.info.addrs.clone()))
            .ok_or_else(|| CoreError::Membership(format!("unknown node {}", node_id.short())))?;
        self.inner.transport.member_conn(node_id, &addrs).await
    }

    // -- M11 change control (§5) -------------------------------------------

    /// Live nodes the selector matched that this user's grants exclude.
    ///
    /// Named rather than elided in a preview: "it matched ten and will hit
    /// three" is precisely the surprise `--dry-run` exists to prevent.
    fn excluded_names(
        &self,
        selector: &Selector,
        chosen: &[MemberInfo],
        user: &UserId,
        service: Option<&str>,
        now: u64,
    ) -> Vec<String> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let me = self.node_id();
        self.inner.engine.read(|s| {
            membership::resolve_targets(s, selector, me, |id| self.inner.gossip.liveness_of(id))
                .into_iter()
                .filter(|m| !chosen.iter().any(|c| c.node_id == m.node_id))
                .filter(|m| !rbac::can_run_on(s, &founder, user, m, service, now))
                .map(|m| m.name)
                .collect()
        })
    }

    /// Route one execution submission, in whichever mode M11 put it in.
    ///
    /// The three verbs that can start work — an ad-hoc command, a service,
    /// a workflow — share a gate, a preview and an override path, so they
    /// share an entry point. `DryRun` and `BreakGlass` reach this with the
    /// *same* inner request the caller would otherwise have sent, which is
    /// how a preview is guaranteed to describe the real submission.
    async fn dispatch_submission(
        &self,
        user: UserId,
        env: SignedIpc,
        req: IpcRequest,
        mode: SubmitMode,
        out: mpsc::Sender<IpcResponse>,
    ) {
        // If this is a task acting for the person who submitted its job,
        // say so in writing before anything builds a spec or a plan. Every
        // downstream path carries the job env, so attaching it once here is
        // what keeps a remote executor's check whole (`mesh_proto::acting`).
        let req = self.attest_acting_for(&env, &user, req);
        // Said here, where the person is, rather than as a refusal from
        // every executor the job reaches (SEC-4).
        if let IpcRequest::Run { env: job_env, .. }
        | IpcRequest::RunScript { env: job_env, .. }
        | IpcRequest::Parallel { env: job_env, .. } = &req
            && let Some(key) = loader_key(job_env)
        {
            let _ = out
                .send(IpcResponse::Error {
                    message: format!(
                        "{key} cannot be set in a job's environment; if the command needs \
                         it, put it in the command, where it shows"
                    ),
                })
                .await;
            return;
        }
        match req {
            IpcRequest::Run {
                command,
                on,
                env: job_env,
                timeout_secs,
                fanout,
                priority,
                max_parallel,
                halt_on_fail,
            } => {
                if let Err(resp) = self.gate(&user, rbac::RUNNERS, "job.run", &command, &env, true)
                {
                    let _ = out.send(*resp).await;
                    return;
                }
                self.dispatch_and_stream(
                    user,
                    env,
                    JobPayload::Command(command),
                    None,
                    on,
                    job_env,
                    timeout_secs,
                    priority,
                    fanout,
                    FanoutControls {
                        max_parallel,
                        halt_on_fail,
                    },
                    mode,
                    out,
                )
                .await;
            }
            IpcRequest::RunScript {
                script,
                on,
                env: job_env,
                timeout_secs,
                priority,
            } => {
                // Gated as an ordinary run, because that is what it is: the
                // same verb with the body in the request instead of in the
                // Library (`mesh_proto::script`). A separate role here
                // would mean a user who may `run` a published script may
                // not `run` the same lines typed out, which is a
                // distinction nobody asked for and one the audit log
                // already records either way.
                if let Err(resp) = self.gate(
                    &user,
                    rbac::RUNNERS,
                    "job.run",
                    &script.preview(),
                    &env,
                    true,
                ) {
                    let _ = out.send(*resp).await;
                    return;
                }
                // The routing marker (`mesh_proto::script::SCRIPT_AUTH`).
                // Stamped here, on the origin, so a user cannot set it and
                // so it is present before placement reads it — and stamped
                // unconditionally, because whether it *matters* is a
                // question about the candidates, which is placement's to
                // answer and not this function's.
                let mut job_env = job_env;
                job_env.retain(|(k, _)| k != mesh_proto::script::SCRIPT_AUTH);
                job_env.push(mesh_proto::script::auth_marker());
                self.dispatch_and_stream(
                    user,
                    env,
                    JobPayload::Script(script),
                    None,
                    on,
                    job_env,
                    timeout_secs,
                    priority,
                    None,
                    FanoutControls::default(),
                    mode,
                    out,
                )
                .await;
            }
            IpcRequest::RunService {
                name,
                on,
                inputs,
                inventory,
                timeout_secs,
                fanout,
                priority,
                max_parallel,
                halt_on_fail,
            } => {
                if let Err(resp) = self.gate(&user, rbac::RUNNERS, "job.run", &name, &env, true) {
                    let _ = out.send(*resp).await;
                    return;
                }
                // `--inventory` rendered the host inventory, which was
                // removed. The field stays in the request — it is
                // positional — and an older client that fills it is told
                // why rather than handed a run against the wrong machines.
                if inventory
                    .as_deref()
                    .is_some_and(|sel| !sel.trim().is_empty())
                {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: format!("--inventory: {INVENTORY_RETIRED}"),
                        })
                        .await;
                    return;
                }
                let mut job_env: Vec<(String, String)> = Vec::new();
                // M19 §5.2, amendment 8: a service run's capture keys ride
                // `inputs`, because `RunService` has no `env` field and
                // *growing* it is the one thing the carrier argument
                // forbids — an executor re-decodes this request out of the
                // signed envelope, so a 1.9 executor reading a 1.8
                // origin's request would fail to decode a field the origin
                // never wrote. `inputs` is a map that already exists, and
                // the keys come off here, before `resolve_service`
                // validates against the contract: a service whose schema
                // says `additionalProperties: false` must never see them.
                let mut inputs = inputs;
                let (want, expect) = outputs::Wanted::take_from_env(&mut inputs);
                job_env.extend(want.to_env_with(expect.as_deref()));
                // C7's `--cacheable` / `--cache-outputs` ride `inputs` for
                // the same reason and come off at the same point: before
                // `resolve_service` validates against the contract, so a
                // service whose schema says `additionalProperties: false`
                // never sees a key the CLI put there.
                inputs.retain(|(k, v)| {
                    let reserved = mesh_proto::is_reserved_input(k);
                    if reserved {
                        job_env.push((k.clone(), v.clone()));
                    }
                    !reserved
                });
                let exec = match services::resolve_service(&self.inner.engine, &name, inputs, None)
                {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = out
                            .send(IpcResponse::Error {
                                message: e.to_string(),
                            })
                            .await;
                        return;
                    }
                };
                // C7 P3: `cacheable` on the service version. A **tag**,
                // not a field, for the rule §14.1 restates every milestone
                // — `ServiceVersion` is signed and replicated, so it may
                // never grow a field — and the claim it carries is the
                // author's, about code they wrote, which is the only party
                // in the system in a position to make it.
                //
                // The submitter can still opt in per-run with
                // `--cacheable`, and cannot opt *out* of a service that
                // declares it: the author's claim of hermeticity is not a
                // preference the caller gets to override, it is a property
                // of the code.
                if self.inner.engine.read(|s| {
                    s.services
                        .get(&name)
                        .and_then(|e| e.current())
                        .is_some_and(|v| v.tags.iter().any(|t| t == mesh_proto::CACHEABLE_TAG))
                }) && !job_env.iter().any(|(k, _)| k == mesh_proto::REQ_CACHE)
                {
                    job_env.push((mesh_proto::REQ_CACHE.to_string(), "1".into()));
                }
                self.dispatch_and_stream(
                    user,
                    env,
                    JobPayload::Service(Box::new(exec)),
                    Some(name),
                    on,
                    job_env,
                    timeout_secs,
                    priority,
                    fanout,
                    FanoutControls {
                        max_parallel,
                        halt_on_fail,
                    },
                    mode,
                    out,
                )
                .await;
            }
            IpcRequest::Parallel {
                command,
                service,
                inputs,
                item_input,
                items,
                on,
                env: job_env,
                timeout_secs,
                chunk,
                capture,
                reduce,
                reduce_on,
            } => {
                let what = service.clone().unwrap_or_else(|| command.clone());
                if let Err(resp) = self.gate(&user, rbac::RUNNERS, "job.run", &what, &env, true) {
                    let _ = out.send(*resp).await;
                    return;
                }
                // A Library script resolves on the origin, exactly as it does
                // for `RunService`: the contract is validated once, here, and
                // workers receive an already-checked `ServiceExec`.
                let exec = match &service {
                    Some(name) => {
                        // The contract is validated against a *real* item —
                        // the first one — because the input that receives
                        // each item is empty until an item exists, and a
                        // required input cannot be checked against nothing.
                        // Every chunk substitutes its own item over this.
                        let mut inputs = inputs;
                        if let (Some(key), Some(first)) = (&item_input, items.first()) {
                            match inputs.iter_mut().find(|(k, _)| k == key) {
                                Some(slot) => slot.1 = first.clone(),
                                None => inputs.push((key.clone(), first.clone())),
                            }
                        }
                        match services::resolve_service(&self.inner.engine, name, inputs, None) {
                            Ok(exec) => Some(Box::new(exec)),
                            Err(e) => {
                                let _ = out
                                    .send(IpcResponse::Error {
                                        message: e.to_string(),
                                    })
                                    .await;
                                return;
                            }
                        }
                    }
                    // No service name: a plain command. The anonymous
                    // bundle path went with the Python SDK (2026-09-20),
                    // which was the only thing that ever named one.
                    None => {
                        let _ = inputs;
                        None
                    }
                };
                if reduce.is_some() && !capture.is_object() {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: "a reduce needs map outputs to reduce over: \
                                      use `meshnode map`, not `parallel`"
                                .into(),
                        })
                        .await;
                    return;
                }
                self.parallel_stream(
                    ParallelRequest {
                        user,
                        auth: env,
                        command,
                        exec,
                        item_input,
                        items,
                        on,
                        mode,
                        env: job_env,
                        timeout_secs,
                        chunk,
                        capture,
                        reduce,
                        reduce_on,
                    },
                    out,
                )
                .await;
            }
            other => {
                let _ = out
                    .send(IpcResponse::Error {
                        message: format!(
                            "{} cannot be previewed or overridden — \
                             --dry-run and --break-glass apply to run, service run, \
                             parallel and workflow run",
                            request_name(&other)
                        ),
                    })
                    .await;
            }
        }
    }

    // -- M17 the fabric (§3) -------------------------------------------
    //
    // Two legs, one authorization. `serve_wire_session` is the host end of
    // the QUIC leg; `serve_ipc_session` is the origin end of the local one
    // and is *also* the host end when the target is this node. Both go
    // through `authorize_session`, which is where the MASTERPLAN's
    // "enforcement happens on the executing node" is actually spelled out
    // for a shell.

    /// Live sessions on this node, for `meshnode sessions` and Director.
    pub fn session_rows(&self) -> Vec<mesh_proto::SessionRow> {
        self.inner.sessions.rows()
    }

    /// Session policies, with whether each one covers *this* node — which
    /// is the answer to "why can I not get a prompt here".
    pub fn session_policy_rows(&self) -> Vec<mesh_proto::SessionPolicyRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let me = self.node_id();
        self.inner.engine.read(|s| {
            let info = s.members.get(&me).map(|m| m.info.clone());
            let mut rows: Vec<mesh_proto::SessionPolicyRow> = s
                .session_policies
                .values()
                .filter(|e| !e.removed)
                .map(|e| mesh_proto::SessionPolicyRow {
                    id: e.policy.id.clone(),
                    description: e.policy.description.clone(),
                    users: e.policy.users.clone(),
                    nodes: e.policy.nodes.clone(),
                    allow: e
                        .policy
                        .allow
                        .iter()
                        .map(|p| p.as_str().to_string())
                        .collect(),
                    expires_at_micros: e.policy.expires_at_micros,
                    created_at_micros: e.policy.created_at_micros,
                    created_by_username: rbac::username_of(s, &founder, &e.policy.created_by),
                    covers_this_node: info
                        .as_ref()
                        .is_some_and(|i| Selector::parse(&e.policy.nodes).matches_in(s, i)),
                })
                .collect();
            rows.sort_by(|a, b| a.id.cmp(&b.id));
            rows
        })
    }

    /// The session registry, for `/metrics` and the drain path.
    pub fn sessions(&self) -> &Arc<crate::sessions::Registry> {
        &self.inner.sessions
    }

    /// May `user` open `kind` on **this** node right now?
    ///
    /// §3.4: the required grant is `rbac::RUNNERS` scoped to this node —
    /// `can_run_on` with `service = None`, which is already the ad-hoc
    /// command rule and therefore already requires an unrestricted `*`
    /// services scope. A `viewer` or `auditor` is refused. **A user who
    /// cannot run `meshnode run` on a node cannot get a shell on it, and
    /// vice versa** — and a port-forward is authorized identically, because
    /// forwarding to `localhost:5432` is at least as powerful as a prompt
    /// and a weaker role for it would be a hole shaped like a convenience.
    fn authorize_session(
        &self,
        user: &UserId,
        kind: &mesh_proto::SessionKind,
        reason: &str,
    ) -> std::result::Result<(), String> {
        let me_name = &self.inner.config.node_name;

        // A terminal is the most audit-worthy thing this node grants, and it
        // is not granted while the audit trail cannot be written (SEC-6).
        if let Some(why) = self.inner.engine.write_failure() {
            return Err(format!(
                "this node cannot write to its log right now ({why}), so it opens no \
                 session it could not record"
            ));
        }

        // 0. Is this machine quarantined? (E2.4)
        //
        //    **First, and before anything else**, because this is the one
        //    refusal that is about the machine being *suspected* rather
        //    than about who is asking. Quarantine is what separates the
        //    security verb from `nodes contribute off`: a cordoned node
        //    still gives an on-call engineer a terminal, and a quarantined
        //    one must not — the whole point is that nobody goes near it
        //    until somebody knows what happened.
        //
        //    Checked on the *host* node against its own replicated copy,
        //    like every other authorization in the product, so a
        //    quarantine holds even if the operator's console never heard
        //    about it.
        let me = self.inner.engine.node_id();
        if let Some(q) = self
            .inner
            .engine
            .read(|s| s.quarantine(&me).map(|q| (q.reason.clone(), q.at_micros)))
        {
            return Err(format!(
                "{me_name} is quarantined and is not accepting terminals — {} (since {}). \
                 Lift it with `meshnode nodes quarantine {me_name} --lift` once you know what \
                 happened",
                if q.0.is_empty() {
                    "no reason given"
                } else {
                    &q.0
                },
                crate::localtime::describe_instant(q.1),
            ));
        }

        // 1. Is this shape of session offered at all? M17 v1 is the
        //    terminal; the others are on the wire and refused by name.
        if let Some(message) = kind.refusal() {
            return Err(message);
        }

        // 2. Has this node's operator turned terminals on? Off by default:
        //    an agent installed to run jobs does not silently become an
        //    agent somebody can get a prompt through.
        // The live switch (ENG-11): an operator who turned terminals off
        // and reloaded meant it now, not at the next restart.
        let sessions = self.live_config().sessions;
        if !sessions.accept {
            return Err(format!(
                "{me_name} does not accept terminals — an operator has to enable them there \
                 ([sessions] accept = \"on\")"
            ));
        }

        // 3. A reason is required and recorded. Cheap for the operator,
        //    and it is the first column an auditor reads.
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(
                "opening a terminal needs a reason — it is written to the audit log".into(),
            );
        }
        if reason.len() > mesh_proto::MAX_SESSION_REASON {
            return Err(format!(
                "that reason is longer than the {}-character limit",
                mesh_proto::MAX_SESSION_REASON
            ));
        }

        let founder = self.inner.engine.genesis().body.founder_user;
        let now = mesh_proto::now_micros();
        let me = self.node_id();
        let (info, username) = self.inner.engine.read(|s| {
            (
                s.members.get(&me).map(|m| m.info.clone()),
                rbac::username_of(s, &founder, user),
            )
        });
        let Some(info) = info else {
            return Err(format!(
                "{me_name} does not have a member record for itself"
            ));
        };

        // 4. The ordinary runner grant, exactly as `meshnode run` needs it
        //    (§3.4). A session policy is *necessary, not sufficient*:
        //    nobody gains a terminal here who could not already run a
        //    command here.
        let allowed = self.inner.engine.read(|s| {
            rbac::require_role(s, &founder, user, rbac::RUNNERS, now).is_ok()
                && rbac::can_run_on(s, &founder, user, &info, None, now)
        });
        if !allowed {
            return Err(format!(
                "opening a terminal on {me_name} needs a runner grant covering this node \
                 (admin, operator or developer with an unrestricted service scope)"
            ));
        }

        // 5. …and a session policy that names this user and this node.
        //
        //    This is the half that is *about terminals* rather than about
        //    running work, and it is why a shell is not simply implied by
        //    a runner grant. It fails closed by construction: a policy only
        //    grants, so a node that has none — or cannot decode the ones it
        //    has — refuses.
        let root_daemon = self.running_as_root();
        let needed = if root_daemon {
            mesh_proto::SessionPermission::ShellPrivileged
        } else {
            mesh_proto::SessionPermission::Shell
        };
        let permitted = self.inner.engine.read(|s| {
            s.session_policies
                .values()
                .filter(|e| !e.removed && !e.policy.is_expired(now))
                .filter(|e| rbac::glob_match(&e.policy.users, &username))
                .filter(|e| Selector::parse(&e.policy.nodes).matches_in(s, &info))
                .any(|e| e.policy.permits(needed))
        });
        if !permitted {
            return Err(if root_daemon {
                format!(
                    "{me_name} runs its daemon as root, so a terminal there is a root \
                     terminal: it needs a session policy granting `shell-privileged` to \
                     {username}"
                )
            } else {
                format!(
                    "no session policy grants {username} a terminal on {me_name} \
                     (`meshnode sessions policy put …`)"
                )
            });
        }

        // 6. A root daemon needs the node's own consent as well as the
        //    policy's. Two independent switches, because "the daemon
        //    happens to be running as root" is not a decision anybody made
        //    about terminals.
        if root_daemon && !sessions.allow_root {
            return Err(format!(
                "{me_name} runs its daemon as root and has not opted into root terminals \
                 ([sessions] allow_root = \"on\")"
            ));
        }

        // 7. The reserved tags govern *execution*, never participation
        //    (§3.9) — and `no-shell` is the same idea one notch narrower:
        //    a node that still runs jobs but is not a place anybody gets a
        //    prompt. Replicated, so an admin can carve out production from
        //    anywhere instead of editing a TOML file on six hundred boxes.
        if info.tags.iter().any(|t| t == mesh_proto::NO_SHELL_TAG) {
            return Err(format!(
                "{me_name} is tagged {} — it runs jobs, but it is not a place to get a prompt",
                mesh_proto::NO_SHELL_TAG
            ));
        }
        if kind.spawns_a_process() {
            if info.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG) {
                return Err(format!(
                    "{me_name} is marked no-execute — it does not run processes"
                ));
            }
            if self.is_draining() {
                return Err(format!(
                    "{me_name} is draining and is not starting new work"
                ));
            }
        }
        // 8. Phase B, E3: a shell *inside* an instance needs that instance
        //    to be a running container this node executes. **Last**, after
        //    every check about who is asking and whether this machine gives
        //    prompts at all, so a caller refused a terminal never learns
        //    which containers run here.
        if let mesh_proto::SessionKind::Instance { job, .. } = kind {
            self.instance_container(job)?;
        }
        Ok(())
    }

    /// Is this daemon running as uid 0?
    ///
    /// The packaged systemd unit runs as an unprivileged `meshnode` user,
    /// so this is normally false and the whole root story is moot. It is
    /// true for someone who ran `meshnode up` as root, which is exactly the
    /// case worth catching: a terminal there is a root terminal whether or
    /// not anybody intended one.
    fn running_as_root(&self) -> bool {
        #[cfg(unix)]
        {
            rustix::process::getuid().is_root()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// The OS account a terminal on this node runs as, for the audit entry.
    fn session_os_user(&self) -> String {
        std::env::var("USER")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| {
                #[cfg(unix)]
                {
                    format!("uid {}", rustix::process::getuid().as_raw())
                }
                #[cfg(not(unix))]
                {
                    "unknown".to_string()
                }
            })
    }

    /// Verify a session's envelope the way the executor verifies a job's,
    /// and return the user it proves.
    ///
    /// Freshness is the ordinary [`AUTH_SKEW`] rather than
    /// `JOB_AUTH_WINDOW`'s hour: a fan-out plan can be adopted much later
    /// and a session is *opened* once, so it needs no long window (§3.4).
    fn verify_session_envelope(
        &self,
        spec: &mesh_proto::SessionSpec,
    ) -> std::result::Result<(UserId, String), String> {
        let user = self.verify_envelope(&spec.auth)?;
        let req: IpcRequest = postcard::from_bytes(&spec.auth.payload)
            .map_err(|_| "malformed session authorization payload".to_string())?;
        let IpcRequest::SessionOpen { node, kind, reason } = req else {
            return Err("session authorization does not describe a session".into());
        };
        // The machine the user named is part of what they signed (SEC-11).
        // Without this, an envelope for a shell on one node opened a shell
        // on any other that received it inside the freshness window —
        // each node keeps its own nonce cache, so none of them had seen it.
        self.session_names_me(&node)?;
        // The signature covers what the *user* asked for. Without this
        // check a compromised origin could forward a valid envelope for a
        // port-forward and open a shell with it.
        if kind != spec.kind {
            return Err("session authorization describes a different session".into());
        }
        // The reason rides inside the signature too, so it is the
        // operator's own statement and not something a relaying node could
        // put in their mouth.
        Ok((user, reason))
    }

    /// Does `target`, as a user typed it for a session, name this node —
    /// and only this node? (SEC-11)
    ///
    /// By id, exactly. By name, only when no other active member carries the
    /// same one: a member can rename itself, and a name two machines share
    /// is a way to be handed a session meant for the other.
    fn session_names_me(&self, target: &str) -> std::result::Result<(), String> {
        let me = self.node_id();
        if target == me.to_display_string() || target == me.short() {
            return Ok(());
        }
        let (mine, shared) = self.inner.engine.read(|s| {
            (
                s.members.get(&me).is_some_and(|m| m.info.name == target),
                s.members.values().any(|m| {
                    m.info.node_id != me
                        && m.info.name == target
                        && s.is_active_member(&m.info.node_id)
                }),
            )
        });
        if !mine {
            return Err(format!(
                "this session was signed for {target:?}, which is not this node"
            ));
        }
        if shared {
            return Err(format!(
                "{target:?} names more than one machine in this mesh — open the session by node id"
            ));
        }
        Ok(())
    }

    /// What a session on this node is held away from (SEC-8), under the
    /// same `[runtime] sandbox` setting jobs obey.
    fn session_confinement(&self) -> Option<crate::sessions::host::Confine> {
        use crate::sandbox::{self, Mode};
        let policy = self.inner.jobs.sandbox_policy();
        if matches!(policy.mode, Mode::Off) {
            return None;
        }
        let mut hidden = vec![self.inner.config.data_dir.clone()];
        if let Some(home) = std::env::var_os("HOME") {
            hidden.extend(sandbox::home_secrets(std::path::Path::new(&home)));
        }
        let shim = if sandbox::applies(&policy) {
            Ok(sandbox::shim_binary(&policy))
        } else {
            match (policy.mode, sandbox::unavailable_reason(&policy)) {
                (Mode::On, Some(why)) => Err(format!(
                    "[runtime] sandbox = \"on\", but {why} — so this node opens no terminal it \
                     cannot confine"
                )),
                _ => Ok(None),
            }
        };
        Some(crate::sessions::host::Confine { hidden, shim })
    }

    fn session_record_dir(&self, id: u64) -> Option<std::path::PathBuf> {
        self.live_config().sessions.record.then(|| {
            self.inner
                .config
                .layout()
                .sessions_dir()
                .join(id.to_string())
        })
    }

    /// Host end of the wire leg: the transport has handed us both halves.
    async fn serve_wire_session(
        &self,
        peer: crate::transport::PeerCtx,
        spec: mesh_proto::SessionSpec,
        mut send: quinn::SendStream,
        recv: quinn::RecvStream,
    ) {
        // Every other wire request is refused to a non-member before it is
        // served; a session skipped that door (SEC-11). And the peer's name
        // is what replicated state says, not what its handshake claimed.
        let is_member = self
            .inner
            .engine
            .read(|s| s.is_active_member(&peer.node_id));
        let peer_name = self.member_name(&peer.node_id);
        let (user, reason) = match (if is_member {
            Ok(())
        } else {
            Err("not a member of this mesh".to_string())
        })
        .and_then(|()| self.verify_session_envelope(&spec))
        .and_then(|(user, reason)| {
            self.authorize_session(&user, &spec.kind, &reason)
                .map(|()| (user, reason))
        }) {
            Ok(pair) => pair,
            Err(message) => {
                // A refusal arrives as an `Error` frame and nothing else,
                // so a client never has to guess whether silence meant
                // "denied" or "still connecting".
                // The *claimed* user, which is all a failed signature
                // leaves — and exactly what an auditor wants to see.
                self.audit_session(
                    &spec.auth.user,
                    spec.kind.audit_action(),
                    &peer_name,
                    &format!("denied: {message}"),
                    &spec.kind.describe(),
                    &spec.auth.signature,
                );
                let _ = mesh_proto::write_frame(
                    &mut send,
                    &mesh_proto::SessionServerFrame::Error { message },
                )
                .await;
                let _ = send.finish();
                return;
            }
        };
        let username = self.username_of(&user);
        let guard = match self.inner.sessions.admit(
            spec.kind.clone(),
            true,
            peer.node_id,
            peer_name.clone(),
            user,
            username.clone(),
            self.live_config().sessions.max_per_peer,
            reason.clone(),
            self.session_os_user(),
            self.live_config().sessions.limits,
        ) {
            Ok(guard) => guard,
            Err(message) => {
                let _ = mesh_proto::write_frame(
                    &mut send,
                    &mesh_proto::SessionServerFrame::Error { message },
                )
                .await;
                let _ = send.finish();
                return;
            }
        };
        self.run_hosted_session(spec.kind, recv, send, guard, user, &peer_name, &reason)
            .await;
    }

    /// The container an instance session opens on this node (Phase B, E3):
    /// a running container job this node itself executes, and an engine to
    /// `exec` through.
    ///
    /// Asked twice — by [`Self::authorize_session`], which turns a `No` into
    /// the refusal the initiator reads, and again for the spawn — from the
    /// same replicated state, so the two cannot disagree about which
    /// container this is.
    fn instance_container(
        &self,
        job: &str,
    ) -> std::result::Result<crate::sessions::host::InstanceTarget, String> {
        let me = self.node_id();
        let me_name = &self.inner.config.node_name;
        // The CLI names a job by its short id and Director by its full one;
        // either is the same job, and the container is named by the short.
        let found = self.inner.engine.read(|s| {
            s.jobs
                .values()
                .find(|j| j.job_id.short() == job || j.job_id.to_display_string() == job)
                .map(|j| (j.executor, j.state, j.runtime.clone(), j.job_id.short()))
        });
        let Some((executor, state, runtime, short)) = found else {
            return Err(format!("there is no job {job} in this mesh"));
        };
        if executor != me {
            return Err(format!("{job} does not run on {me_name}"));
        }
        if !matches!(state, mesh_proto::JobState::Running) {
            return Err(format!(
                "{job} is {} on {me_name}, not running",
                state.as_str()
            ));
        }
        if runtime != mesh_proto::Runtime::Container.as_str() {
            return Err(format!(
                concat!(
                    "{} is a {} instance, and a shell inside one is for containers — ",
                    "`meshnode shell {}` opens a terminal on the machine instead"
                ),
                job, runtime, me_name
            ));
        }
        let Some(engine) = self.inner.jobs.container_engine() else {
            return Err(format!(
                concat!(
                    "{} has no container engine of its own to exec through — it runs ",
                    "containers through a plugin, or not at all"
                ),
                me_name
            ));
        };
        Ok(crate::sessions::host::InstanceTarget {
            program: engine.program.clone(),
            global_args: engine.global_args(),
            container: crate::engines::container_name(&short),
            env: crate::engines::engine_env(),
        })
    }

    /// The half both legs share: audit open, run, audit close.
    #[allow(clippy::too_many_arguments)]
    async fn run_hosted_session<R, W>(
        &self,
        kind: mesh_proto::SessionKind,
        reader: R,
        writer: W,
        guard: crate::sessions::Guard,
        user: UserId,
        peer_name: &str,
        reason: &str,
    ) where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let started = Instant::now();
        let detail = kind.describe();
        let os_user = self.session_os_user();
        // **There is one audit log, and a shell is in it** (§3.5). Recording
        // the *terminal* is optional and off by default; this is neither —
        // and it carries what an auditor actually asks for: who, where, as
        // which OS account, which session, and why.
        self.audit_session(
            &user,
            kind.audit_action(),
            peer_name,
            "ok",
            &format!(
                "session {} · {detail} · as {os_user} · reason: {reason}",
                guard.id()
            ),
            &[],
        );
        let opts = crate::sessions::host::Opts {
            record_dir: self.session_record_dir(guard.id()),
            node_name: self.inner.config.node_name.clone(),
            shim: self.inner.config.pty_shim_override.clone(),
            // Resolved again from the same state `authorize_session`
            // checked, a moment ago, so the two cannot disagree about which
            // container this is.
            instance: match &kind {
                mesh_proto::SessionKind::Instance { job, .. } => self.instance_container(job).ok(),
                _ => None,
            },
            confine: self.session_confinement(),
        };
        let ending = crate::sessions::host::run(kind.clone(), reader, writer, &guard, opts).await;
        let secs = started.elapsed().as_secs();
        // The end *reason* is its own field rather than prose, because
        // "how many terminals did the idle timeout close last month" is a
        // question somebody filters on.
        self.audit_session(
            &user,
            kind.close_action(),
            peer_name,
            &ending.outcome(),
            &format!("session {} · {detail} · {secs}s", guard.id()),
            &[],
        );
    }

    fn audit_session(
        &self,
        user: &UserId,
        action: &str,
        target: &str,
        outcome: &str,
        detail: &str,
        signature: &[u8],
    ) {
        audit::record(
            &self.inner.engine,
            *user,
            action,
            target,
            outcome,
            detail,
            signature,
        );
    }

    /// Every invite this node knows of, newest expiry first (SEC-17).
    fn invite_rows(&self) -> Vec<mesh_proto::InviteRow> {
        let founder = self.inner.engine.genesis().body.founder_user;
        let mut rows = self.inner.engine.read(|s| {
            s.invites
                .values()
                .map(|inv| mesh_proto::InviteRow {
                    invite_id: inv.public.invite_id,
                    created_by_user: inv.public.created_by_user,
                    created_by_username: rbac::username_of(
                        s,
                        &founder,
                        &inv.public.created_by_user,
                    ),
                    expires_at_micros: inv.public.expires_at_micros,
                    uses: inv.public.uses,
                    used: u32::try_from(inv.consumed_by.len()).unwrap_or(u32::MAX),
                    revoked: inv.revoked,
                    tags: inv.public.tags.clone(),
                })
                .collect::<Vec<_>>()
        });
        rows.sort_by_key(|r| std::cmp::Reverse(r.expires_at_micros));
        rows
    }

    /// A join is presenting a sound invite this node has no record of.
    /// The create may still be on its way from the admin's node, so ask a
    /// few live members before the join is refused for it (SEC-17).
    ///
    /// Not more than once every couple of seconds per invite: the token
    /// has already passed its signature check, but replaying one should
    /// not become a way to make this node sync on demand. Every distinct
    /// invite took an admin to mint.
    async fn learn_invite(&self, token: &mesh_proto::InviteToken) {
        const EVERY: Duration = Duration::from_secs(2);
        if !membership::invite_not_yet_known(&self.inner.engine, token) {
            return;
        }
        {
            let mut asked = self.inner.invite_pull.lock();
            asked.retain(|_, at| at.elapsed() < EVERY);
            if asked.contains_key(&token.invite_id) {
                return;
            }
            asked.insert(token.invite_id, Instant::now());
        }
        let me = self.node_id();
        let gossip = self.inner.gossip.clone();
        let peers: Vec<mesh_proto::MemberInfo> = self.inner.engine.read(|s| {
            s.members
                .values()
                .filter(|m| m.info.node_id != me && s.is_active_member(&m.info.node_id))
                .filter(|m| matches!(gossip.liveness_of(&m.info.node_id), Liveness::Alive))
                .map(|m| m.info.clone())
                .take(3)
                .collect()
        });
        for peer in peers {
            match tokio::time::timeout(Duration::from_secs(5), gossip.sync_with(&peer)).await {
                Ok(Err(e)) => debug!(peer = %peer.node_id.short(), "invite sync failed: {e}"),
                Err(_) => debug!(peer = %peer.node_id.short(), "invite sync timed out"),
                Ok(Ok(_)) => {}
            }
            if !membership::invite_not_yet_known(&self.inner.engine, token) {
                return;
            }
        }
    }

    /// Does `user` hold no role on this mesh — somebody who is not a user
    /// yet, or no longer one (SEC-19)?
    fn is_anonymous(&self, user: &UserId) -> bool {
        let founder = self.inner.engine.genesis().body.founder_user;
        self.inner
            .engine
            .read(|s| rbac::roles_at(s, &founder, user, mesh_proto::now_micros()).is_empty())
    }

    fn username_of(&self, user: &UserId) -> String {
        let founder = self.inner.engine.genesis().body.founder_user;
        self.inner
            .engine
            .read(|s| rbac::username_of(s, &founder, user))
    }

    /// Origin end of the local leg: the IPC loop has handed us the whole
    /// Unix stream (R4 — the local socket is one-shot too, so an
    /// interactive feature has to solve this twice).
    pub async fn serve_ipc_session(&self, env: SignedIpc, stream: tokio::net::UnixStream) {
        let (read_half, mut write_half) = stream.into_split();
        let user = match self.verify_envelope(&env) {
            Ok(user) => user,
            Err(message) => {
                let _ = mesh_proto::write_frame(
                    &mut write_half,
                    &mesh_proto::SessionServerFrame::Error { message },
                )
                .await;
                return;
            }
        };
        let req: std::result::Result<IpcRequest, _> = postcard::from_bytes(&env.payload);
        if let Ok(req) = &req
            && let Some(message) = self.credential_refusal(&env, req)
        {
            let _ = mesh_proto::write_frame(
                &mut write_half,
                &mesh_proto::SessionServerFrame::Error { message },
            )
            .await;
            return;
        }
        let Ok(IpcRequest::SessionOpen { node, kind, reason }) = req else {
            let _ = mesh_proto::write_frame(
                &mut write_half,
                &mesh_proto::SessionServerFrame::Error {
                    message: "not a session request".into(),
                },
            )
            .await;
            return;
        };

        // The target, by name or id. Its own name is not a special case
        // worth a round trip: a shell into this machine runs the host
        // handler right here.
        let me = self.node_id();
        let matches: Vec<MemberInfo> = self.inner.engine.read(|s| {
            s.members
                .values()
                .filter(|m| s.is_active_member(&m.info.node_id))
                .filter(|m| {
                    m.info.name == node
                        || m.info.node_id.to_display_string() == node
                        || m.info.node_id.short() == node
                })
                .map(|m| m.info.clone())
                .collect()
        });
        // The host refuses a name two machines share, so say it here, where
        // the person is, rather than dialling one of them (SEC-11).
        if matches.len() > 1 {
            let _ = mesh_proto::write_frame(
                &mut write_half,
                &mesh_proto::SessionServerFrame::Error {
                    message: format!(
                        "{node:?} names {} machines in this mesh — open the session by node id",
                        matches.len()
                    ),
                },
            )
            .await;
            return;
        }
        let target = matches.into_iter().next();
        let Some(target) = target else {
            let _ = mesh_proto::write_frame(
                &mut write_half,
                &mesh_proto::SessionServerFrame::Error {
                    message: format!("no live member named {node:?}"),
                },
            )
            .await;
            return;
        };

        if target.node_id == me {
            if let Err(message) = self.authorize_session(&user, &kind, &reason) {
                self.audit_session(
                    &user,
                    kind.audit_action(),
                    &target.name,
                    &format!("denied: {message}"),
                    &kind.describe(),
                    &env.signature,
                );
                let _ = mesh_proto::write_frame(
                    &mut write_half,
                    &mesh_proto::SessionServerFrame::Error { message },
                )
                .await;
                return;
            }
            let username = self.username_of(&user);
            let guard = match self.inner.sessions.admit(
                kind.clone(),
                true,
                me,
                target.name.clone(),
                user,
                username,
                self.live_config().sessions.max_per_peer,
                reason.clone(),
                self.session_os_user(),
                self.live_config().sessions.limits,
            ) {
                Ok(guard) => guard,
                Err(message) => {
                    let _ = mesh_proto::write_frame(
                        &mut write_half,
                        &mesh_proto::SessionServerFrame::Error { message },
                    )
                    .await;
                    return;
                }
            };
            let name = target.name.clone();
            self.run_hosted_session(kind, read_half, write_half, guard, user, &name, &reason)
                .await;
            return;
        }

        // Remote: dial the member (the pooled connection, the punched path
        // or the relay — M7 already decided which), check it speaks 1.9,
        // and splice.
        let conn = match self
            .inner
            .transport
            .member_conn(target.node_id, &target.addrs)
            .await
        {
            Ok(conn) => conn,
            Err(e) => {
                let _ = mesh_proto::write_frame(
                    &mut write_half,
                    &mesh_proto::SessionServerFrame::Error {
                        message: format!("cannot reach {}: {e}", target.name),
                    },
                )
                .await;
                return;
            }
        };
        let minor = self
            .inner
            .transport
            .pooled_conn(&target.node_id)
            .map(|c| c.minor)
            .unwrap_or(mesh_proto::PROTOCOL_MINOR);
        if minor < mesh_proto::SESSION_MINOR {
            // Named rather than hung: a 1.8 node would fail to decode the
            // request and silently drop the stream, which from this end is
            // indistinguishable from a peer that never answers.
            let _ = mesh_proto::write_frame(
                &mut write_half,
                &mesh_proto::SessionServerFrame::Error {
                    message: format!(
                        "{} speaks protocol 1.{minor} and sessions need 1.{} — upgrade it",
                        target.name,
                        mesh_proto::SESSION_MINOR
                    ),
                },
            )
            .await;
            return;
        }
        // Phase B, E3: a node older than the instance kind cannot decode it
        // and would drop the stream — said here, by name, instead.
        if matches!(kind, mesh_proto::SessionKind::Instance { .. })
            && minor < mesh_proto::INSTANCE_MINOR
        {
            let _ = mesh_proto::write_frame(
                &mut write_half,
                &mesh_proto::SessionServerFrame::Error {
                    message: format!(
                        "{} speaks protocol 1.{minor}, and a shell inside an instance needs 1.{} — upgrade it, or use `meshnode shell {}`",
                        target.name,
                        mesh_proto::INSTANCE_MINOR,
                        target.name
                    ),
                },
            )
            .await;
            return;
        }
        let spec = mesh_proto::SessionSpec {
            kind: kind.clone(),
            auth: env,
        };
        let (host_write, host_read) = match self.inner.transport.open_session(&conn, spec).await {
            Ok(pair) => pair,
            Err(e) => {
                let _ = mesh_proto::write_frame(
                    &mut write_half,
                    &mesh_proto::SessionServerFrame::Error {
                        message: format!("cannot open a session on {}: {e}", target.name),
                    },
                )
                .await;
                return;
            }
        };
        let username = self.username_of(&user);
        // Registered as *not* hosting: an initiated session spends nothing
        // a host's cap is protecting, and counting it against one would
        // make this node refuse its own operator.
        let guard = match self.inner.sessions.admit(
            kind,
            false,
            target.node_id,
            target.name.clone(),
            user,
            username,
            u32::MAX,
            reason,
            // The OS account is the *host's* business, and this end is not
            // the host.
            String::new(),
            // Timeouts belong to the node hosting the process, which is the
            // one whose resources a forgotten terminal holds.
            crate::sessions::Limits {
                idle: None,
                max: None,
            },
        ) {
            Ok(guard) => guard,
            Err(message) => {
                let _ = mesh_proto::write_frame(
                    &mut write_half,
                    &mesh_proto::SessionServerFrame::Error { message },
                )
                .await;
                return;
            }
        };
        crate::sessions::splice::run(read_half, write_half, host_read, host_write, &guard).await;
    }

    // -- run: schedule + stream (the M1 payoff) ---------------------------

    /// `meshnode gangs show <id>`.
    fn gang_view(&self, gang: mesh_proto::GangId) -> Option<mesh_proto::GangView> {
        let row = self.gang_rows(true).into_iter().find(|r| r.gang == gang)?;
        self.inner.engine.read(|s| {
            let entry = s.gangs.get(&gang)?;
            let members = entry
                .plan
                .members
                .iter()
                .enumerate()
                .map(|(rank, m)| mesh_proto::GangMemberView {
                    rank: u32::try_from(rank).unwrap_or(u32::MAX),
                    job_id: m.job_id,
                    node_id: m.node_id,
                    node_name: m.node_name.clone(),
                    addr: m.addr.clone(),
                    state: s.jobs.get(&m.job_id).map(|j| j.state),
                })
                .collect();
            // Unfinished gangs that outrank this one. The answer to "why is
            // mine not running" — and an empty list is itself an answer:
            // whatever is wrong, it is not that somebody is ahead.
            let mut ahead: Vec<(mesh_proto::GangTicket, mesh_proto::GangId)> = s
                .gangs
                .values()
                .filter(|e| e.closed.is_none() && e.plan.ticket.is_senior_to(&entry.plan.ticket))
                .map(|e| (e.plan.ticket, e.plan.gang))
                .collect();
            ahead.sort();
            Some(mesh_proto::GangView {
                row,
                members,
                ahead: ahead.into_iter().map(|(_, g)| g).collect(),
            })
        })
    }

    /// Leases outstanding on other nodes, inferred from replicated plans.
    ///
    /// Leases are node-local by design (§8.2), so this is the only thing a
    /// node can honestly say about another node's reservations: a gang plan
    /// that is open and has not committed means its members' leases are out
    /// there, unless the plan is older than the longest lease anybody will
    /// grant — past which they have expired and nobody had to be told.
    ///
    /// An upper bound inside one lease lifetime and exact outside it, which
    /// is the right shape for a column whose job is to explain why a node
    /// with free slots refused work.
    fn inferred_leases(&self) -> HashMap<NodeId, (u32, u32)> {
        let horizon = mesh_proto::now_micros()
            .saturating_sub(mesh_proto::MAX_LEASE_TTL_SECS.saturating_mul(1_000_000));
        let mut out: HashMap<NodeId, (u32, u32)> = HashMap::new();
        self.inner.engine.read(|s| {
            for entry in s.gangs.values() {
                if entry.closed.is_some()
                    || entry.plan.created_at_micros < horizon
                    || entry.committed(&s.jobs)
                {
                    continue;
                }
                let per_member = mesh_proto::Requirements::from_env(&entry.plan.env);
                for m in &entry.plan.members {
                    let slot = out.entry(m.node_id).or_insert((0, 0));
                    slot.0 = slot.0.saturating_add(per_member.cpus);
                    slot.1 = slot.1.saturating_add(1);
                }
            }
        });
        out
    }

    /// The address a gang member tells its peers to meet it on.
    ///
    /// A relay candidate is skipped: `MESHWARE_GANG_LEADER` is handed to a
    /// training script that will open its own socket to it, and a
    /// `relay:<node-id>` string is a meshware-internal scheme no such
    /// script can dial. A member with only relay candidates gets an empty
    /// string, which is visible in `gangs show` rather than silently wrong.
    fn gang_addr(member: &mesh_proto::MemberInfo) -> String {
        member
            .addrs
            .iter()
            .find(|a| !a.starts_with("relay:"))
            .cloned()
            .unwrap_or_default()
    }

    /// How many members of a gang one candidate could hold at once.
    ///
    /// Advertised hardware, not live availability: this only decides the
    /// *shape* of the plan, and whether the reservation is really there is
    /// what the offer phase is for. Being optimistic here costs an offer
    /// round; being pessimistic would refuse gangs the mesh could run.
    fn gang_capacity(member: &mesh_proto::MemberInfo, reqs: &mesh_proto::Requirements) -> u32 {
        let slots = member.slots.max(1);
        let cap = mesh_proto::Capacity::from_tags(&member.tags);
        let by_cpu = match (reqs.cpus, cap.as_ref().map(|c| c.cores).unwrap_or(0)) {
            (0, _) => slots,
            (want, 0) => slots / want.max(1),
            (want, cores) => cores / want.max(1),
        };
        let by_mem = match (reqs.mem_mb, cap.as_ref().map(|c| c.mem_mb).unwrap_or(0)) {
            (0, _) | (_, 0) => u32::MAX,
            (want, total) => u32::try_from(total / want.max(1)).unwrap_or(u32::MAX),
        };
        by_cpu.min(by_mem).min(slots)
    }

    /// Lay `size` members over `ranked`, best-first, honouring placement.
    ///
    /// `pack` fills each node before moving on, because packing a gang onto
    /// the fewest nodes minimises the inter-task network path — which is
    /// what a gang exists for. `spread` takes one member per node, for
    /// fault tolerance, and is never the default.
    fn assign_gang(
        ranked: &[mesh_proto::MemberInfo],
        size: u32,
        reqs: &mesh_proto::Requirements,
        placement: mesh_proto::GangPlacement,
    ) -> Vec<mesh_proto::MemberInfo> {
        let mut out: Vec<mesh_proto::MemberInfo> = Vec::with_capacity(size as usize);
        match placement {
            mesh_proto::GangPlacement::Spread => {
                for m in ranked {
                    if out.len() as u32 >= size {
                        break;
                    }
                    if Self::gang_capacity(m, reqs) >= 1 {
                        out.push(m.clone());
                    }
                }
            }
            mesh_proto::GangPlacement::Pack => {
                for m in ranked {
                    let room = Self::gang_capacity(m, reqs);
                    for _ in 0..room {
                        if out.len() as u32 >= size {
                            break;
                        }
                        out.push(m.clone());
                    }
                    if out.len() as u32 >= size {
                        break;
                    }
                }
            }
        }
        out
    }

    /// Members grouped by the node that holds them, in rank order.
    fn gang_by_node(plan: &mesh_proto::GangPlanRecord) -> Vec<(NodeId, String, u32)> {
        let mut out: Vec<(NodeId, String, u32)> = Vec::new();
        for m in &plan.members {
            match out.iter_mut().find(|(id, _, _)| *id == m.node_id) {
                Some((_, _, n)) => *n += 1,
                None => out.push((m.node_id, m.node_name.clone(), 1)),
            }
        }
        out
    }

    /// Ask one node to hold `members` reservations for this gang.
    async fn gang_offer(
        &self,
        node: NodeId,
        plan: &mesh_proto::GangPlanRecord,
        members: u32,
    ) -> mesh_proto::GangOfferReply {
        let reqs = mesh_proto::Requirements::from_env(&plan.env);
        if node == self.node_id() {
            return self.inner.jobs.offer_lease(
                plan.gang,
                plan.ticket,
                reqs,
                members,
                mesh_proto::DEFAULT_LEASE_TTL_SECS,
                mesh_proto::DeviceRequest::from_env(&plan.env),
            );
        }
        let refused = |why: String| mesh_proto::GangOfferReply::Refused {
            reason: mesh_proto::GangRefusal::NotAccepting { reason: why },
        };
        let Ok(conn) = self.remote_conn(node).await else {
            return refused(format!("{} is unreachable", self.member_name(&node)));
        };
        let request = Request::GangOffer {
            gang: plan.gang,
            ticket: plan.ticket,
            reqs,
            members,
            ttl_secs: mesh_proto::DEFAULT_LEASE_TTL_SECS,
        };
        match self.inner.transport.request_one(&conn, &request).await {
            Ok(Response::GangOffer(reply)) => reply,
            Ok(Response::Error { message }) => refused(message),
            Ok(other) => refused(format!("unexpected offer response: {other:?}")),
            Err(e) => refused(e.to_string()),
        }
    }

    /// Turn one node's lease into an admission. `Err` carries the reason,
    /// and the reason is always the same class of thing: the reservation
    /// did not survive to the commit, so the gang did not get all `N`.
    async fn gang_commit(&self, node: NodeId, gang: mesh_proto::GangId) -> Result<()> {
        if node == self.node_id() {
            return match self.inner.jobs.commit_lease(&gang) {
                Some(_) => Ok(()),
                None => Err(CoreError::Job(format!(
                    "the reservation on {} is gone",
                    self.member_name(&node)
                ))),
            };
        }
        let conn = self.remote_conn(node).await?;
        match self
            .inner
            .transport
            .request_one(&conn, &Request::GangCommit { gang })
            .await?
        {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(CoreError::Job(message)),
            other => Err(CoreError::Job(format!("unexpected commit: {other:?}"))),
        }
    }

    /// Give back every reservation this gang might be holding.
    ///
    /// Best-effort and deliberately unchecked: a release that does not
    /// arrive costs one lease lifetime, which is the same cost as the
    /// origin having died, and the TTL already covers that case. There is
    /// nothing here that has to be retried until it succeeds, which is what
    /// "no distributed cleanup path" means in practice.
    async fn gang_release_all(&self, plan: &mesh_proto::GangPlanRecord) {
        for (node, _, _) in Self::gang_by_node(plan) {
            if node == self.node_id() {
                self.inner.jobs.release_lease(&plan.gang);
                continue;
            }
            if let Ok(conn) = self.remote_conn(node).await {
                let _ = self
                    .inner
                    .transport
                    .request_one(&conn, &Request::GangRelease { gang: plan.gang })
                    .await;
            }
        }
    }

    /// One offer round: hold every member's reservation, then commit them
    /// all, or hold none.
    ///
    /// Offers go out concurrently, because the deadline is for the round
    /// and not for each node in turn — a serial walk over eight nodes would
    /// let one slow peer eat the whole window while seven leases sat idle.
    async fn gang_round(&self, plan: &mesh_proto::GangPlanRecord) -> GangRoundOutcome {
        let by_node = Self::gang_by_node(plan);
        let deadline = Duration::from_secs(mesh_proto::OFFER_DEADLINE_SECS.max(1));

        let offers = futures::future::join_all(by_node.iter().map(|(node, name, members)| {
            let this = self.clone();
            let node = *node;
            let name = name.clone();
            let members = *members;
            async move {
                let reply = match tokio::time::timeout(
                    deadline,
                    this.gang_offer(node, plan, members),
                )
                .await
                {
                    Ok(reply) => reply,
                    Err(_) => mesh_proto::GangOfferReply::Refused {
                        reason: mesh_proto::GangRefusal::NotAccepting {
                            reason: format!("{name} did not answer inside the offer deadline"),
                        },
                    },
                };
                (node, name, reply)
            }
        }))
        .await;

        let mut held = 0u32;
        let mut behind: Option<mesh_proto::GangId> = None;
        let mut refusal: Option<String> = None;
        for (_, name, reply) in &offers {
            match reply {
                mesh_proto::GangOfferReply::Accepted { .. } => held += 1,
                mesh_proto::GangOfferReply::Refused { reason } => {
                    if let mesh_proto::GangRefusal::Senior { gang } = reason {
                        behind = Some(*gang);
                    }
                    if refusal.is_none() {
                        refusal = Some(format!("{name}: {}", reason.message()));
                    }
                }
            }
        }

        if held as usize != by_node.len() {
            // Not all `N`. Give back what we took — holding it while we
            // retry is what makes two large gangs deadlock, and giving it
            // back is what lets the senior one through.
            self.gang_release_all(plan).await;
            return GangRoundOutcome::Refused {
                held,
                behind,
                reason: refusal.unwrap_or_else(|| "no candidate accepted".into()),
            };
        }

        // Every candidate is holding. Commit them all; a commit that fails
        // means that node's lease did not survive (wounded or expired), and
        // the answer is to release the rest rather than start a partial
        // gang — the one thing this milestone exists to prevent.
        for (node, name, _) in &by_node {
            if let Err(e) = self.gang_commit(*node, plan.gang).await {
                warn!(gang = %plan.gang.short(), node = %name, "gang commit failed: {e}");
                self.gang_release_all(plan).await;
                return GangRoundOutcome::Refused {
                    held,
                    behind: None,
                    reason: format!("{name}: {e}"),
                };
            }
        }
        GangRoundOutcome::Committed
    }

    /// The spec for one gang member, built from the plan alone so that the
    /// origin and any adopting successor produce identical bytes — the
    /// idempotency key of §10.3, applied to a gang.
    fn gang_spec(plan: &mesh_proto::GangPlanRecord, rank: usize) -> JobSpec {
        let member = &plan.members[rank];
        let mut env = plan.env.clone();
        // The submission-side keys never reach an executor: `--gang 8` is a
        // question for the origin, and what a member needs is its rank.
        env.retain(|(k, _)| {
            k != mesh_proto::REQ_GANG
                && k != mesh_proto::REQ_GANG_PLACEMENT
                && k != mesh_proto::REQ_GANG_RESTART
        });
        env.extend(plan.runtime_env(rank));
        JobSpec {
            job_id: member.job_id,
            payload: plan.payload.clone(),
            env,
            timeout_secs: plan.timeout_secs,
            priority: plan.priority,
            submitter: plan.submitter,
            submitter_user: plan.submitter_user,
            auth: plan.auth.clone(),
            fanout: None,
            created_at_micros: plan.created_at_micros,
        }
    }

    /// Dispatch every member at once, after the commit.
    ///
    /// Sequential dispatch would defeat the point: the reservations are
    /// held, so the only thing left to get wrong is the *simultaneity* the
    /// whole milestone is about.
    async fn gang_dispatch(&self, plan: &mesh_proto::GangPlanRecord) -> Vec<Result<()>> {
        futures::future::join_all((0..plan.members.len()).map(|rank| {
            let this = self.clone();
            let spec = Self::gang_spec(plan, rank);
            let node = plan.members[rank].node_id;
            async move {
                // Each member holds a reservation, so there is nowhere else
                // to send it; an unanswered submission is settled with the
                // member itself rather than failed (ENG-6).
                this.deliver(node, spec, Duration::ZERO).await
            }
        }))
        .await
    }

    /// Hand `spec` to `executor` — this node or a peer — and settle what
    /// happened (ENG-6).
    ///
    /// - Accepted: `Ok`.
    /// - Not delivered (`Unreachable`) or refused as full (`Busy`): the
    ///   job is not there, and a caller with other candidates may spill.
    ///   `Unreachable` is retried for up to `patience` first.
    /// - Sent with no answer: the executor may have journaled it. It is
    ///   asked again, which is safe because an executor acknowledges a job
    ///   id it already holds, for up to `patience` or [`RESUBMIT_WINDOW`],
    ///   whichever is longer. An answer settles it either way — `Busy` from
    ///   an executor proves it does not hold the job. Still no answer is
    ///   an `undetermined:` error, and a caller must not send the job
    ///   anywhere else.
    async fn deliver(&self, executor: NodeId, spec: JobSpec, patience: Duration) -> Result<()> {
        if executor == self.node_id() {
            return self.accept_job(spec).await;
        }
        let started = Instant::now();
        let mut backoff = Duration::from_millis(250);
        let mut sent_blind: Option<String> = None;
        loop {
            let e = match self.submit_once(executor, spec.clone()).await {
                Ok(()) => return Ok(()),
                Err(e) if matches!(e, CoreError::Unreachable(_)) || is_undetermined(&e) => e,
                // An answer, and a definitive one.
                Err(e) => {
                    self.inner.dispatch_failures.note(&e);
                    return Err(e);
                }
            };
            if is_undetermined(&e) && sent_blind.is_none() {
                sent_blind = Some(e.to_string());
            }
            let window = if sent_blind.is_some() {
                patience.max(RESUBMIT_WINDOW)
            } else {
                patience
            };
            if started.elapsed() + backoff > window {
                let e = match sent_blind {
                    None => e,
                    Some(first) => CoreError::Job(format!(
                        "{UNDETERMINED} {} may have accepted job {} — the submission went out ({first}) and asking again got no answer ({e}); it was not sent anywhere else",
                        self.member_name(&executor),
                        spec.job_id.short()
                    )),
                };
                self.inner.dispatch_failures.note(&e);
                return Err(e);
            }
            debug!(job = %spec.job_id.short(), executor = %executor.short(),
                "submission not settled, asking again in {backoff:?}: {e}");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(4));
        }
    }

    /// One attempt at handing `spec` to `executor`, under the submission
    /// deadline, with nothing settled afterwards (ENG-6): for a caller that
    /// will ask again itself.
    async fn submit_once(&self, executor: NodeId, spec: JobSpec) -> Result<()> {
        if executor == self.node_id() {
            return self.accept_job(spec).await;
        }
        match tokio::time::timeout(SUBMIT_DEADLINE, self.submit_remote(executor, spec)).await {
            Ok(result) => result,
            Err(_) => Err(CoreError::Transport(format!(
                "no answer within {}s",
                SUBMIT_DEADLINE.as_secs()
            ))),
        }
    }

    async fn submit_remote(&self, executor: NodeId, spec: JobSpec) -> Result<()> {
        let conn = self.remote_conn(executor).await?;
        // The authoritative half of the 1.8 inventory gate. Placement
        // filtered on the peer's *known* minor, and a peer this node has
        // not dialed recently has no known minor — so the check runs again
        // here, where the connection exists and the answer is certain.
        //
        // A refusal rather than a spill: only `Response::Busy` may spill
        // (MASTERPLAN §10.2 — an ambiguous failure must never re-dispatch a
        // job that might already be running), and this is loud, rare and
        // retryable, where running the play against the executor's own
        // localhost would be none of those.
        // The same check for the 1.15 script gate, for the same reason and
        // with a worse failure to prevent: a peer that cannot decode the
        // envelope reports "malformed job authorization payload", which
        // names neither the cause nor the fix.
        if mesh_proto::script::requires_script_support(&spec.env)
            && self
                .inner
                .transport
                .peer_minor(&executor)
                .unwrap_or(mesh_proto::FARM_MINOR)
                < mesh_proto::FARM_MINOR
        {
            return Err(CoreError::Job(format!(
                "{} predates protocol 1.{} and cannot decode a script job's authorization — \
                 upgrade it, or narrow the selector",
                self.member_name(&executor),
                mesh_proto::FARM_MINOR
            )));
        }
        match self
            .inner
            .transport
            .request_one(&conn, &Request::SubmitJob(spec))
            .await?
        {
            Response::JobAccepted { .. } => Ok(()),
            Response::Busy { running, queued } => Err(CoreError::Busy { running, queued }),
            Response::Error { message } => Err(CoreError::Job(message)),
            other => Err(CoreError::Job(format!(
                "unexpected submit response: {other:?}"
            ))),
        }
    }

    /// Stream a job's logs (local or remote) to an IPC sender.
    pub async fn stream_job_logs(
        &self,
        job_id: mesh_proto::JobId,
        executor: NodeId,
        follow: bool,
        out: mpsc::Sender<IpcResponse>,
    ) {
        self.stream_job_logs_as(job_id, executor, follow, None, out)
            .await;
    }

    /// [`Self::stream_job_logs`] on a person's behalf: a remote executor is
    /// shown their signed request (SEC-15).
    async fn stream_job_logs_as(
        &self,
        job_id: mesh_proto::JobId,
        executor: NodeId,
        follow: bool,
        env: Option<&SignedIpc>,
        out: mpsc::Sender<IpcResponse>,
    ) {
        if executor == self.node_id() || self.inner.jobs.has_record(&job_id) {
            let mut rx = self.inner.jobs.follow(job_id, 0, 0, follow);
            while let Some(ev) = rx.recv().await {
                let msg = match ev {
                    LogEvent::Chunk { stream, data } => IpcResponse::LogChunk { stream, data },
                    LogEvent::Done { state, exit_code } => {
                        IpcResponse::JobDone { state, exit_code }
                    }
                    // The retention sweep ran between the availability
                    // check and the read — a narrow race, and the honest
                    // answer to it is the same one the check would have
                    // given. Never a fabricated outcome (M31 A2).
                    LogEvent::Gone => {
                        if let Some(availability) = self.output_availability(&job_id) {
                            let _ = out
                                .send(IpcResponse::LogsUnavailable { availability })
                                .await;
                        }
                        match self.inner.engine.read(|s| s.jobs.get(&job_id).cloned()) {
                            Some(summary) => IpcResponse::JobDone {
                                state: summary.state,
                                exit_code: summary.exit_code,
                            },
                            None => IpcResponse::Error {
                                message: format!(
                                    "no job {} in this mesh's memory. If it ran long enough \
                                     ago, the retention window dropped it.",
                                    job_id.short()
                                ),
                            },
                        }
                    }
                };
                let done = matches!(msg, IpcResponse::JobDone { .. } | IpcResponse::Error { .. });
                if out.send(msg).await.is_err() || done {
                    return;
                }
            }
            return;
        }

        // Remote follow.
        let conn = match self.remote_conn(executor).await {
            Ok(c) => c,
            Err(e) => {
                let _ = out
                    .send(IpcResponse::Error {
                        message: e.to_string(),
                    })
                    .await;
                return;
            }
        };
        let speaks_v2 = self
            .inner
            .transport
            .peer_minor(&executor)
            .is_some_and(|m| m >= mesh_proto::LOGS_MINOR);
        let req = match env {
            Some(env) if speaks_v2 => Request::FollowJobV2 {
                job_id,
                stdout_offset: 0,
                stderr_offset: 0,
                follow,
                env: Box::new(env.clone()),
            },
            _ => Request::FollowJob {
                job_id,
                stdout_offset: 0,
                stderr_offset: 0,
                follow,
            },
        };
        let mut stream = match self.inner.transport.request_stream(&conn, &req).await {
            Ok(s) => s,
            Err(e) => {
                let _ = out
                    .send(IpcResponse::Error {
                        message: e.to_string(),
                    })
                    .await;
                return;
            }
        };
        loop {
            match stream.next().await {
                Ok(Some(Response::LogChunk { stream, data })) => {
                    if out
                        .send(IpcResponse::LogChunk { stream, data })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Some(Response::JobEnd { summary })) => {
                    let _ = out
                        .send(IpcResponse::JobDone {
                            state: summary.state,
                            exit_code: summary.exit_code,
                        })
                        .await;
                    return;
                }
                Ok(Some(Response::Error { message })) => {
                    let _ = out.send(IpcResponse::Error { message }).await;
                    return;
                }
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(e) => {
                    let _ = out
                        .send(IpcResponse::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            }
        }
    }

    /// The invite rendezvous set (MASTERPLAN M7 remote invite UX): our own
    /// direct candidates public-first (advertised, reflexive, LAN, loopback
    /// — never `relay:`, a joiner is not a member yet), then the public
    /// candidates of up to two other verified-public members (relay-tagged
    /// first), so a token minted on a LAN node still carries a cloud
    /// address the joiner can reach.
    fn reachable_addrs(&self) -> Vec<String> {
        let me = self.node_id();
        let mut out = self.inner.wan.public_first_candidates();
        let others: Vec<MemberInfo> = self.inner.engine.read(|s| {
            let mut v: Vec<MemberInfo> = s
                .members
                .values()
                .filter(|m| s.is_active_member(&m.info.node_id) && m.info.node_id != me)
                .filter(|m| self.inner.gossip.liveness_of(&m.info.node_id) == Liveness::Alive)
                .filter(|m| !m.info.tags.iter().any(|t| t == mesh_proto::DRAINING_TAG))
                .map(|m| m.info.clone())
                .collect();
            v.sort_by_key(|m| {
                (
                    !m.tags.iter().any(|t| t == mesh_proto::RELAY_TAG),
                    m.node_id,
                )
            });
            v
        });
        let mut added = 0usize;
        for m in others {
            if added >= 2 {
                break;
            }
            let relay_tagged = m.tags.iter().any(|t| t == mesh_proto::RELAY_TAG);
            let public: Vec<String> = m
                .addrs
                .iter()
                .filter(|a| mesh_proto::parse_relay_candidate(a).is_none())
                .filter(|a| {
                    a.parse::<SocketAddr>()
                        .map(|sa| crate::wan::is_public_ip(sa.ip()))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            // Relay-tagged members verified their own reachability; others
            // count only when they carry a globally routable address.
            if public.is_empty() && !relay_tagged {
                continue;
            }
            let mut any = false;
            for a in public {
                if !out.contains(&a) {
                    out.push(a);
                    any = true;
                }
            }
            if any {
                added += 1;
            }
        }
        if out.is_empty() {
            out = advertised_addrs(
                self.inner.config.listen.ip(),
                self.inner.listen_addr.port(),
                &self.live_config().network.advertise_interfaces,
            );
        }
        out
    }
}

/// Build a `ServiceRow` from a definition + its entry (version history).
/// Everything a parallel submission carries into the owner's stream.
/// Gathered into a struct rather than a fourteen-argument signature,
/// because M9 kept adding to it: resources in P3, a script in P5, capture
/// and reduce in P6/P7.
struct ParallelRequest {
    user: UserId,
    auth: SignedIpc,
    command: String,
    exec: Option<Box<mesh_proto::ServiceExec>>,
    item_input: Option<String>,
    items: Vec<String>,
    on: Option<String>,
    /// Whether change control is being previewed or overridden (M11 §5.2).
    mode: SubmitMode,
    env: Vec<(String, String)>,
    timeout_secs: Option<u64>,
    chunk: Option<u32>,
    capture: mesh_proto::Capture,
    reduce: Option<String>,
    reduce_on: Option<String>,
}

/// Where item `index` is invited to write its output under `map`
/// (M9 §5.5). Named by the item's index in the job's list rather than by
/// the item itself: an item is very often a path, and a path is not a
/// filename.
/// Does this command resolve on this machine's `PATH`? (R5 P2)
///
/// A transcode dispatched to a node without ffmpeg used to fail as exit 127
/// at the item, an hour into a four-hundred-item run, four hundred times.
/// This is the cheapest question that prevents it — a stat of each `PATH`
/// entry, no execution. A command with a slash is taken as a path and
/// checked directly, which is what a shell does with it too.
fn which_on_path(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return false;
    }
    if cmd.contains('/') {
        return std::path::Path::new(cmd).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file())
}

/// The staged manifest a plan carries, as object ids by item index (R4 P3).
fn staged_manifest(env: &[(String, String)]) -> Vec<String> {
    env.iter()
        .find(|(k, _)| k == mesh_proto::STAGED_ITEMS)
        .map(|(_, v)| v.split(',').map(str::to_string).collect())
        .unwrap_or_default()
}

fn item_env_for(
    plan: &mesh_proto::ParallelPlan,
    item: &str,
    dir: &std::path::Path,
    index: usize,
) -> Vec<(String, String)> {
    let mut env = plan.env.clone();
    env.retain(|(k, _)| {
        k != mesh_proto::parallel::ITEM_ENV
            && k != mesh_proto::objects::OUT_ENV
            // Anonymous-bundle keys are the origin's own plumbing: they
            // said which code this job is, and the exec on the plan is
            // that answer. Passing them on would put a bundle id in
            // front of the task for no reason. The acting-for
            // attestation comes off for the same reason — it is read
            // off `plan.env` by the executor before any of this, and
            // the task itself has no business with it.
            && !k.starts_with(mesh_proto::acting::ACT_ENV_PREFIX)
    });
    env.push((mesh_proto::parallel::ITEM_ENV.to_string(), item.to_string()));
    if plan.capture.is_object() {
        // An invitation, not an obligation: an item that writes here
        // gets that file stored as its output, and an item that does
        // not has its stdout stored instead.
        env.push((
            mesh_proto::objects::OUT_ENV.to_string(),
            out_path(dir, index).to_string_lossy().into_owned(),
        ));
    }
    if !plan.inputs.is_empty() {
        env.retain(|(k, _)| k != mesh_proto::objects::INPUTS_ENV);
        env.push((
            mesh_proto::objects::INPUTS_ENV.to_string(),
            dir.join("inputs").to_string_lossy().into_owned(),
        ));
    }
    env
}

/// The action tuple for one `map` item, or `None` when the plan did not
/// ask to be cached (C7 P3).
///
/// **A pure function of the plan and the item**, because two nodes compute
/// it: the worker, to look the item up and publish its answer, and every
/// directory replica the answer is published to, to check that the key
/// really is this item's (SEC-18). A staged item has none — it runs
/// against a path under this chunk's own directory, which no other run
/// will ever name.
///
/// A command item's identity is its command with the item substituted; a
/// script item's is its *service* — runtime, entrypoint, bundle root and
/// resolved inputs — rather than a command string. One function with two
/// arms, not one shape for both: hashing a command that does not exist, or
/// a bundle that does not, is a key that collides across two things that
/// are not the same work.
fn plan_item_action(
    plan: &mesh_proto::ParallelPlan,
    item: &str,
    epoch: u64,
    arch: &str,
) -> Option<mesh_proto::ActionSpec> {
    if !mesh_proto::is_cacheable(&plan.env) || staged_manifest(&plan.env).iter().any(|s| s == item)
    {
        return None;
    }
    let mut action = match &plan.exec {
        None => mesh_proto::ActionSpec {
            runtime: "bash".into(),
            // Named rather than left empty so an entry is self-describing
            // in a dump: this action is one item of a map, not a service.
            entrypoint: "(map item)".into(),
            argv: vec![plan.command_for(item)],
            // Built where the item runs; the paths in it are scratch, and
            // `canonicalise` drops them.
            env: item_env_for(plan, item, std::path::Path::new(""), 0),
            bundle_root: None,
            inputs: plan.inputs.iter().map(|r| r.id).collect(),
            source_digest: None,
            outputs: Vec::new(),
            sandbox_class: crate::cache::SANDBOX_PROCESS.into(),
            arch: arch.to_string(),
            epoch,
        },
        Some(exec) => {
            let exec = per_item_exec(plan, exec, item);
            let mut argv = Vec::with_capacity(exec.inputs.len() * 2);
            for (k, v) in &exec.inputs {
                argv.push(format!("--{k}"));
                argv.push(v.clone());
            }
            mesh_proto::ActionSpec {
                runtime: exec.runtime.as_str().to_string(),
                entrypoint: exec.entrypoint.clone(),
                argv,
                // The service's own static env is as much an input as
                // anything the submitter typed; the plan's env carries the
                // rest.
                env: exec
                    .env
                    .iter()
                    .cloned()
                    .chain(plan.env.iter().cloned())
                    .collect(),
                bundle_root: Some(exec.bundle_id),
                inputs: plan.inputs.iter().map(|r| r.id).collect(),
                source_digest: None,
                outputs: Vec::new(),
                sandbox_class: crate::cache::SANDBOX_PROCESS.into(),
                arch: arch.to_string(),
                epoch,
            }
        }
    };
    action.canonicalise();
    Some(action)
}

/// A script item's service: the item becomes the value of the chosen
/// input, so the script receives it as the `--flag value` its contract
/// declares.
fn per_item_exec(
    plan: &mesh_proto::ParallelPlan,
    exec: &mesh_proto::ServiceExec,
    item: &str,
) -> mesh_proto::ServiceExec {
    let mut per_item = exec.clone();
    if let Some(key) = &plan.item_input {
        match per_item.inputs.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = item.to_string(),
            None => per_item.inputs.push((key.clone(), item.to_string())),
        }
    }
    per_item
}

fn out_path(dir: &std::path::Path, index: usize) -> std::path::PathBuf {
    dir.join("out").join(index.to_string())
}

fn service_row(def: &mesh_proto::ServiceVersion, entry: &crate::state::ServiceEntry) -> ServiceRow {
    ServiceRow {
        name: def.name.clone(),
        runtime: def.runtime,
        version: def.version,
        bundle_id: def.bundle_id,
        working_dir: def.working_dir.clone(),
        entrypoint: def.entrypoint.clone(),
        has_contract: def.contract.is_some(),
        contract: def.contract.clone(),
        description: def.description.clone(),
        tags: def.tags.clone(),
        versions: entry.versions.keys().copied().collect(),
        env: def.env.clone(),
    }
}

/// On startup, re-announce every bundle whose manifest + chunks are complete
/// on disk, so restart re-establishes our holds in replicated state.
fn reannounce_local_bundles(engine: &Arc<StateEngine>, content: &ContentStore) {
    // Only re-announce bundles the mesh already knows about (avoids scanning
    // the blob dir); a freshly-restarted founder re-holds what it pushed.
    let known: Vec<BundleId> = engine.read(|s| s.bundles.keys().copied().collect());
    for id in known {
        if content.has_bundle(&id)
            && let Ok(Some(manifest)) = content.load_manifest(&id)
        {
            let _ = services::record_hold_from_manifest(engine, content, &manifest);
        }
    }
}

/// The first key in `env` that changes what a process loads before its own
/// first line, if any (SEC-4).
fn loader_key(env: &[(String, String)]) -> Option<&str> {
    env.iter()
        .map(|(k, _)| k.as_str())
        .find(|k| mesh_proto::is_loader_env(k))
}

/// Admission control closure: revoked keys are refused at the transport
/// edge; members and joiners are allowed; probes are always allowed.
fn make_admission(engine: Arc<StateEngine>) -> AdmissionCheck {
    Arc::new(move |node_id: &NodeId, purpose: ConnPurpose| {
        if engine.read(|s| s.revoked.contains_key(node_id)) {
            return Err("node key has been revoked".into());
        }
        match purpose {
            ConnPurpose::Probe | ConnPurpose::Join => Ok(()),
            // Members dial *out* to pair; they never accept Pair inbound.
            ConnPurpose::Pair => Err(
                "this node is a mesh member — run `meshnode join --pair` on the joining machine"
                    .into(),
            ),
            ConnPurpose::Member => {
                if engine.read(|s| s.is_active_member(node_id)) {
                    Ok(())
                } else {
                    // Not a known member yet — allow; gossip may be mid-flight.
                    // State-level auth still rejects any ops they send.
                    Ok(())
                }
            }
        }
    })
}

/// Who took part in a parallel job, and who did not (M9 §5.4). The refusals
/// matter as much as the acceptances: a job quietly running on one node
/// because everyone else declined looks exactly like a broken mesh.
#[derive(Debug, Default)]
struct LaneReport {
    lanes: u32,
    workers: Vec<String>,
    declined: Vec<String>,
}

/// How many unanswered pulls a lane tolerates before abandoning a job. The
/// budget has to outlast gossip's dead-detection — several seconds — or a
/// lane would quit before the mesh could tell it whom to re-home to.
const LANE_PATIENCE: u32 = 60;

/// How often ephemeral objects and directory entries are reclaimed
/// (M9 §5.5). Far more often than job-history retention, because objects
/// are what actually occupies memory.
/// How often the history indexer drains the change set. Two seconds is
/// under the threshold at which a run finishing feels absent from a search,
/// and far above the rate at which draining an empty set costs anything.
const HISTORY_INDEX_EVERY: Duration = Duration::from_secs(2);

const OBJECT_SWEEP_EVERY: Duration = Duration::from_secs(60);

/// How often a node looks at how much room it has left (M31 A2).
const DISK_CHECK_EVERY: Duration = Duration::from_secs(60);

/// How often it looks while it is already short of room (ENG-4): a node
/// refusing work for want of space should notice the space coming back in
/// seconds, not a minute later.
const DISK_RECHECK_EVERY: Duration = Duration::from_secs(10);

/// How long the owner waits for one worker to take a parallel job on.
///
/// Offers go out before any local lane starts, and all at once (§5.4), so
/// this bounds how long a silent node can delay the *whole* job. Generous
/// enough for a WAN dial and a hole-punch; far short of the transport's own
/// connect timeout, which is what made a frozen node cost thirty seconds
/// before this existed.
const OFFER_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a lane waits before asking again when the owner has nothing
/// right now. Short enough that a straggler's copy starts promptly, long
/// enough that a tail of idle lanes is negligible traffic.
const LANE_WAIT: Duration = Duration::from_millis(250);

/// How many consecutive waits a lane tolerates — a backstop, not a policy.
/// The queue answers `Done` the moment a job finishes or is forgotten, so
/// waiting normally ends on its own; this only stops a lane leaking if it
/// somehow does not.
const LANE_WAIT_BUDGET: u32 = 2_400;

/// What one turn of a worker lane did (M9 §5.4).
enum LanePull {
    /// A chunk was pulled, run and reported.
    Ran,
    /// The owner had nothing right now but the job is not over (M9 §5.6).
    Waited,
    /// The owner has nothing left: this lane is finished.
    NoWork,
    /// The owner did not answer — it may be busy, or it may be gone.
    Lost,
}

/// Where one key stands in the enrolment queue, read out of state (M23).
///
/// A free function rather than a method because both callers already hold
/// the state lock, and because there is exactly one definition of "you are
/// pending" — the requester's screen and the admin's queue must never be
/// able to disagree about it.
fn enrolment_state_in(
    state: &crate::state::MeshState,
    user: &UserId,
    registered: bool,
    now_micros: u64,
) -> EnrolmentState {
    if registered {
        return EnrolmentState::Enrolled;
    }
    match state.enrolments.get(user) {
        None => EnrolmentState::None,
        Some(entry) if entry.is_declined() => EnrolmentState::Declined,
        Some(entry) if entry.request.is_expired_at(now_micros) => EnrolmentState::Expired,
        Some(_) => EnrolmentState::Pending,
    }
}

/// A user-typed release version as the version string a daemon reports:
/// `v0.6.0`, `0.6.0` and ` V0.6.0 ` all mean `0.6.0`.
///
/// The tag form (`v…`) is what the release channel uses and the bare form
/// is what `meshnode --version` prints; this is the one place the two are
/// reconciled, so a rollout cannot compare a tag against a version and
/// conclude a node never upgraded.
/// Why one node could not be driven, and whether waiting could help
/// (M24 §4).
///
/// The distinction exists because a rollout spends its time waiting on
/// purpose — a node mid-restart refuses connections, and a freshly
/// published plan takes a gossip round to arrive — so "no answer yet" and
/// "the answer is no" look identical for the first few seconds and must
/// never be treated the same afterwards. A transient failure is retried to
/// the deadline; a terminal one stops the rollout in the round trip that
/// found it.
struct AskFailure {
    why: String,
    terminal: bool,
    /// The node answered, and its answer was "that plan is not here yet".
    /// Only this refusal can be resolved by the *op-log* rather than by
    /// time, which is what makes it the one worth cross-examining against
    /// how far the peer has synced.
    pending: bool,
    /// The build the refusing node reported it is running, when it got far
    /// enough to say.
    peer_version: Option<String>,
}

impl AskFailure {
    fn terminal(why: String) -> AskFailure {
        AskFailure {
            why,
            terminal: true,
            pending: false,
            peer_version: None,
        }
    }

    fn transient(why: String) -> AskFailure {
        AskFailure {
            why,
            terminal: false,
            pending: false,
            peer_version: None,
        }
    }
}

/// Has this peer been handed everything in our log since `published`?
///
/// Read against a node that is *still* refusing for want of the plan, this
/// separates the two cases that sentence covers: a record that has not
/// arrived, and a record that arrived and could not be understood. Only a
/// reading taken **after** the plan entered our log answers the question —
/// a peer that caught up a second before we authored it is fully synced and
/// has never been offered the record.
fn peer_holds_the_record(lag: Option<crate::gossip::SyncLag>, published: Instant) -> bool {
    lag.is_some_and(|lag| lag.behind == 0 && lag.observed > published)
}

/// Compare dotted versions numerically, so `0.10.0` beats `0.9.0`.
/// One comparison for the whole workspace (`mesh_proto::version_cmp`).
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    mesh_proto::version_cmp(a, b)
}

fn normalize_release_tag(v: &str) -> String {
    v.trim().trim_start_matches(['v', 'V']).to_string()
}

/// The authorization a scheduled run carries (M8 §3.4).
///
/// Not a real IPC envelope and deliberately not shaped like one: the
/// signature is the **schedule record's own**, over the schedule's signing
/// bytes, because that is the signature a human actually produced and the
/// one every executor can re-verify against replicated state. `at_micros`
/// carries the occurrence rather than "now" so that two nodes firing the
/// same occurrence build byte-identical envelopes.
/// Per-instance health state the executor's prober carries across ticks.
struct InstanceHealthTracker {
    healthy: bool,
    consecutive_failures: u32,
    since_micros: u64,
    detail: String,
}

/// Run one health probe against a local instance (C2 P3, §6.2). Returns
/// `(healthy, detail)`. Every kind is evaluated over loopback, on this node
/// — the only vantage point from which the answer is about the service
/// rather than the network.
async fn probe_health(check: &mesh_proto::HealthCheck) -> (bool, String) {
    let timeout = Duration::from_secs(check.timeout_secs.max(1));
    match check.kind {
        mesh_proto::HealthKind::Tcp => {
            let addr = format!("127.0.0.1:{}", check.port);
            match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await {
                Ok(Ok(_)) => (true, String::new()),
                Ok(Err(e)) => (false, format!("tcp: {e}")),
                Err(_) => (false, "tcp: timed out".into()),
            }
        }
        mesh_proto::HealthKind::Http => {
            match tokio::time::timeout(timeout, http_probe(check.port, &check.path)).await {
                Ok(Ok(code)) if (200..300).contains(&code) => (true, String::new()),
                Ok(Ok(code)) => (false, format!("http {code}")),
                Ok(Err(e)) => (false, format!("http: {e}")),
                Err(_) => (false, "http: timed out".into()),
            }
        }
        mesh_proto::HealthKind::Exec => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(&check.command);
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
            match tokio::time::timeout(timeout, cmd.status()).await {
                Ok(Ok(status)) if status.success() => (true, String::new()),
                Ok(Ok(status)) => (false, format!("exec: exit {}", status.code().unwrap_or(-1))),
                Ok(Err(e)) => (false, format!("exec: {e}")),
                Err(_) => (false, "exec: timed out".into()),
            }
        }
    }
}

/// A minimal loopback HTTP/1.0 GET that returns the status code. No client
/// crate: a health check is one request to localhost, and the executor
/// already speaks raw TCP for the mesh.
async fn http_probe(port: u16, path: &str) -> std::io::Result<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let path = if path.is_empty() { "/" } else { path };
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::with_capacity(256);
    // We only need the status line; read a bounded prefix.
    let mut chunk = [0u8; 256];
    let n = stream.read(&mut chunk).await?;
    buf.extend_from_slice(&chunk[..n]);
    let head = String::from_utf8_lossy(&buf);
    // `HTTP/1.1 200 OK`
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no status line"))?;
    Ok(code)
}

/// The authorization a scheduled job carries (N1 P5).
///
/// The **schedule record's own signature**, over the record's own bytes —
/// never a stored user envelope. A schedule outlives the session that made
/// it many times over, so there is no live envelope to carry; and authority
/// re-derived from the record on every executor at every fire is what makes
/// revoking the creator stop the schedule everywhere, with no signature to
/// recall.
fn schedule_fire_envelope(schedule: &mesh_proto::ServiceSchedule, occurrence: u64) -> SignedIpc {
    SignedIpc {
        user: schedule.created_by,
        at_micros: schedule.created_at_micros,
        nonce: 0,
        payload: postcard::to_stdvec(&IpcRequest::ScheduleFire {
            schedule: schedule.name.clone(),
            occurrence_micros: occurrence,
        })
        .unwrap_or_default(),
        signature: schedule.signature.clone(),
    }
}

/// The address a job on this node should advertise itself on (N2 P2).
///
/// `advertise` first, then the listen address, then loopback. A wildcard
/// bind is skipped rather than published: `0.0.0.0:21000` is a true
/// statement about what the node listens on and a useless one to hand a
/// peer, and a job that put it in a health-check URL would be checking
/// nothing.
/// `[compute]` with `placement` neutralised, for the reload comparison.
///
/// So the "NOT applied" warning stays true: it is about the fields that are
/// read once at startup, and `placement` is not one of them any more.
fn compute_without_placement(c: &crate::config::ComputeConfig) -> crate::config::ComputeConfig {
    crate::config::ComputeConfig {
        placement: mesh_proto::FitPolicy::default(),
        ..*c
    }
}

/// This node's accelerators as Prometheus gauges (N6 §12).
///
/// **This node's own cards.** An exporter reports the machine it runs on; a
/// node exporting its peers' GPUs would double-count every card in the mesh
/// once per node that can see it.
///
/// A free function rather than a method so it can be tested against a made
/// up inventory: the machine this is written on has no accelerator probe,
/// and a renderer that is only ever exercised on hardware nobody has in CI
/// is a renderer nobody has read the output of.
fn render_devices(
    out: &mut String,
    inventory: &[mesh_proto::Device],
    telemetry: &[mesh_proto::DevicePressure],
    held: &std::collections::BTreeMap<mesh_proto::DeviceKey, String>,
) {
    use std::fmt::Write as _;
    for d in inventory {
        // Absent on a card whose telemetry has not arrived, which is a
        // different state from an idle card and both are honest: a gauge
        // invented as `0` for something nobody can read would show a
        // saturated GPU as free.
        let Some(live) = mesh_proto::reading_for(telemetry, d, inventory) else {
            continue;
        };
        let labels = format!(
            "index=\"{}\",vendor=\"{}\",model=\"{}\"",
            d.index,
            escape_label(&d.vendor),
            escape_label(&d.model),
        );
        if let Some(pct) = live.util_pct {
            let _ = writeln!(
                out,
                "# HELP meshware_gpu_utilisation_pct Accelerator utilisation, 0-100, as the \
                 driver reports it.\n\
                 # TYPE meshware_gpu_utilisation_pct gauge\n\
                 meshware_gpu_utilisation_pct{{{labels}}} {pct}"
            );
        }
        if let Some(mb) = live.mem_free_mb {
            let _ = writeln!(
                out,
                "# HELP meshware_gpu_memory_free_mb Free device memory in MB.\n\
                 # TYPE meshware_gpu_memory_free_mb gauge\n\
                 meshware_gpu_memory_free_mb{{{labels}}} {mb}"
            );
        }
        if let Some(c) = live.temp_c {
            let _ = writeln!(
                out,
                "# HELP meshware_gpu_temperature_celsius Device temperature.\n\
                 # TYPE meshware_gpu_temperature_celsius gauge\n\
                 meshware_gpu_temperature_celsius{{{labels}}} {c}"
            );
        }
        if let Some(t) = live.throttled {
            let _ = writeln!(
                out,
                "# HELP meshware_gpu_throttled 1 when the driver reports the device throttled \
                 — the reason a card at 100% is doing less work than yesterday.\n\
                 # TYPE meshware_gpu_throttled gauge\n\
                 meshware_gpu_throttled{{{labels}}} {}",
                u8::from(t)
            );
        }
        // Leased, which is a different question from busy: a card held by a
        // job that has not started using it yet reads 0% and is not
        // available to anybody.
        let _ = writeln!(
            out,
            "# HELP meshware_gpu_held 1 when a job or gang holds a lease on this device. \
             Different from utilisation: a held card at 0% is not a free card.\n\
             # TYPE meshware_gpu_held gauge\n\
             meshware_gpu_held{{{labels}}} {}",
            u8::from(held.contains_key(&d.key()))
        );
    }
}

/// One Prometheus label value, escaped.
///
/// A GPU's model string comes from the driver verbatim — `NVIDIA GeForce
/// RTX 4090` today, and whatever a vendor ships tomorrow. An unescaped
/// backslash or quote in it would not make the scrape wrong, it would make
/// the whole exposition unparseable, taking fifty-two working metrics down
/// with it. Escaped per the text format: backslash, quote, newline.
pub(crate) fn escape_label(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn advertise_host(config: &NodeConfig) -> String {
    config
        .wan
        .advertise
        .iter()
        .filter_map(|a| host_of(a))
        .next()
        // `listen` is a real `SocketAddr`, so the wildcard question is
        // asked of the IP rather than of a string.
        .or_else(|| {
            let ip = config.listen.ip();
            (!ip.is_unspecified()).then(|| ip.to_string())
        })
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// The address a job is told to bind its ports on (`MESHWARE_IP_<label>`).
///
/// **Not the advertised address.** `[wan] advertise` is what other machines
/// dial, and behind NAT it is on no interface of this machine, so a job told
/// to bind it could not. The listen address is on this machine by definition,
/// and a wildcard listen — every real node — tells the job every interface.
fn job_bind_host(config: &NodeConfig) -> String {
    config.listen.ip().to_string()
}

/// The host half of `host:port`, IPv6 brackets handled, wildcards refused.
fn host_of(addr: &str) -> Option<String> {
    let addr = addr.trim();
    let host = match addr.rfind(']') {
        // `[::1]:4000` — the last colon after the bracket is the separator.
        Some(close) => addr.get(1..close)?,
        None => addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr),
    };
    let host = host.trim();
    (!host.is_empty() && host != "0.0.0.0" && host != "::" && host != "*").then(|| host.to_string())
}

fn deployment_fire_envelope(dep: &mesh_proto::Deployment) -> SignedIpc {
    SignedIpc {
        user: dep.created_by,
        at_micros: dep.created_at_micros,
        nonce: 0,
        payload: postcard::to_stdvec(&IpcRequest::DeploymentFire {
            deployment: dep.name.clone(),
            generation: dep.generation,
        })
        .unwrap_or_default(),
        signature: dep.signature.clone(),
    }
}

/// How large one payload POST may get before it is sent (**H-6**).
///
/// Payload lines are job output, so a single chatty job can be megabytes.
/// Batching without a cap would trade a hundred small requests for one
/// unbounded one, which is how a warehouse starts refusing bodies.
const PAYLOAD_POST_MAX: usize = 4 * 1024 * 1024;

/// Rough encoded size of some payload lines, for [`PAYLOAD_POST_MAX`].
///
/// The serialised length of each line, which is what the destination will
/// actually receive.
///
/// **Called on the newly-added lines and accumulated**, never on the whole
/// batch: re-measuring the batch after each job would re-serialise every
/// line already in it, which is quadratic in the number of jobs and would
/// have made this guard cost more than the request it guards.
fn payload_bytes(lines: &[serde_json::Value]) -> usize {
    lines.iter().map(|l| l.to_string().len() + 1).sum()
}

/// POST whatever payload lines have accumulated, and settle the jobs they
/// came from (**H-6**).
///
/// Returns `false` when the destination refused, which stops the export
/// walking the rest of the page into the same wall. The cursor is advanced
/// only for jobs whose bytes were actually accepted, so a refusal leaves
/// them to the next tick.
#[allow(clippy::too_many_arguments)]
async fn flush_payloads(
    sink: &mesh_proto::Sink,
    credential: &str,
    mesh_id: &str,
    pending: &mut Vec<serde_json::Value>,
    pending_jobs: &mut Vec<(mesh_proto::JobId, crate::sinks::Mark)>,
    cursor: &mut crate::sinks::SinkCursor,
    locators: &mut Vec<(mesh_proto::JobId, String, String)>,
    advanced: &mut bool,
) -> bool {
    use crate::sinks::WriteOutcome;
    if pending.is_empty() {
        pending_jobs.clear();
        return true;
    }
    let outcome = crate::sinks::http_write(sink, pending, credential).await;
    pending.clear();
    let jobs = std::mem::take(pending_jobs);
    match outcome {
        WriteOutcome::Accepted => {
            for (job_id, here) in jobs {
                cursor.payloads_sent += 1;
                cursor.set_payload(here);
                *advanced = true;
                // P0's locator, filled at last. Written to the **index**
                // row; `mesh-archive` copies it into the Parquet row on its
                // next ingest, so the six-month question stays answerable
                // after A1 has pruned the op-log row (§7 crossing 3).
                locators.push((
                    job_id,
                    sink.name.clone(),
                    crate::sinks::payload_key(mesh_id, &job_id),
                ));
            }
            true
        }
        other => {
            debug!(
                sink = %sink.name,
                jobs = jobs.len(),
                "payload export deferred: {other:?}"
            );
            false
        }
    }
}

/// The four things [`Node::output_availability_with`] asks of a job.
///
/// Extracted under the state lock and carried out of it (**H-1**). The
/// alternative — cloning the whole `JobSummary` — copies a command preview,
/// a runtime name and a fan-out record per row for an answer that reads
/// none of them, which on the unbounded legacy listing means cloning the
/// entire job table to compute a column.
#[derive(Clone, Copy)]
struct AvailFacts {
    job_id: mesh_proto::JobId,
    state: JobState,
    executor: NodeId,
    finished_at_micros: Option<u64>,
}

impl AvailFacts {
    fn of(job: &JobSummary) -> Self {
        AvailFacts {
            job_id: job.job_id,
            state: job.state,
            executor: job.executor,
            finished_at_micros: job.finished_at_micros,
        }
    }
}

/// Does this job satisfy a [`mesh_proto::JobsQuery`]? (**H-1**)
///
/// `prefix`, `state` and `contains` arrive already lowercased — they are
/// per-*query* constants, and folding case per row is exactly the kind of
/// per-row allocation this whole change exists to remove.
fn job_matches(
    job: &JobSummary,
    prefix: Option<&str>,
    state: Option<&str>,
    contains: Option<&str>,
    query: &mesh_proto::JobsQuery,
) -> bool {
    if let Some(p) = prefix {
        // The display form is `mwj1<base32>`; the caller may have typed
        // either half, so both are stripped to the body before comparing.
        let id = job.job_id.to_display_string();
        if !id.trim_start_matches("mwj1").starts_with(p) {
            return false;
        }
    }
    if let Some(want) = state {
        let ok = match want {
            // What the UI's tabs actually mean, named so a client does not
            // have to send five requests to build one counter.
            "live" | "active" => !job.state.is_terminal(),
            "terminal" | "finished" => job.state.is_terminal(),
            other => job.state.as_str() == other,
        };
        if !ok {
            return false;
        }
    }
    if let Some(u) = query.user
        && job.submitter_user != u
    {
        return false;
    }
    if let Some(n) = query.node
        && job.executor != n
    {
        return false;
    }
    if let Some(f) = query.fan
        && job.fanout.is_none_or(|info| info.group != f)
    {
        return false;
    }
    if let Some(since) = query.since_micros
        && job.created_at_micros < since
    {
        return false;
    }
    if let Some(needle) = contains
        && !job.cmd_preview.to_ascii_lowercase().contains(needle)
    {
        return false;
    }
    true
}

fn payload_preview(payload: &JobPayload) -> String {
    const MAX: usize = 120;
    let raw = match payload {
        JobPayload::Command(cmd) => cmd.replace('\n', " "),
        JobPayload::Service(exec) => format!("{}@{}", exec.service_name, exec.version),
        JobPayload::Script(script) => format!("{} script", script.runtime.as_str()),
    };
    if raw.chars().count() <= MAX {
        return raw;
    }
    let cut: String = raw.chars().take(MAX - 1).collect();
    format!("{cut}…")
}

/// A submission resolved far enough to describe: what `--dry-run` prints.
struct Resolved<'a> {
    /// One line a human can read: the command, or `service@4`.
    action: String,
    /// The selector as written.
    selector: String,
    /// What it resolved to, in scheduler order.
    targets: &'a [MemberInfo],
    priority: JobPriority,
}

/// Assemble the answer to a job or service `--dry-run`.
fn job_preview(
    resolved: &Resolved<'_>,
    payload: &JobPayload,
    env: &[(String, String)],
    timeout_secs: Option<u64>,
    excluded: &[String],
) -> mesh_proto::JobPreview {
    let (service, version, bundle_id, runtime) = match payload {
        JobPayload::Service(exec) => (
            Some(exec.service_name.clone()),
            Some(exec.version),
            Some(exec.bundle_id),
            Some(exec.runtime.as_str().to_string()),
        ),
        JobPayload::Command(_) => (None, None, None, Some("bash".to_string())),
        JobPayload::Script(script) => (None, None, None, Some(script.runtime.as_str().to_string())),
    };
    // Secret *names*, never values — a preview is a thing people paste
    // into tickets (§5.2).
    let mut secrets_used: Vec<String> = Vec::new();
    let mut scan = |text: &str| {
        for u in secrets::find_refs(text) {
            let name = match &u.source {
                secrets::SecretSource::Vault { name } => name.clone(),
                secrets::SecretSource::Provider {
                    provider,
                    path,
                    key,
                    ..
                } => {
                    if key.is_empty() {
                        format!("{provider}:{path}")
                    } else {
                        format!("{provider}:{path}#{key}")
                    }
                }
            };
            if !secrets_used.contains(&name) {
                secrets_used.push(name);
            }
        }
    };
    for (_, value) in env {
        scan(value);
    }
    if let JobPayload::Command(cmd) = payload {
        scan(cmd);
    }
    if let JobPayload::Service(exec) = payload {
        for (_, value) in &exec.env {
            scan(value);
        }
    }

    mesh_proto::JobPreview {
        kind: if service.is_some() {
            "service.run".into()
        } else {
            "job.run".into()
        },
        action: resolved.action.clone(),
        service,
        version,
        bundle_id,
        runtime,
        selector: resolved.selector.clone(),
        targets: resolved.targets.iter().map(|m| m.name.clone()).collect(),
        excluded: excluded.to_vec(),
        env_keys: env.iter().map(|(k, _)| k.clone()).collect(),
        secrets: secrets_used,
        priority: resolved.priority.as_str().to_string(),
        timeout_secs,
        blast_radius: {
            let unique: std::collections::BTreeSet<&NodeId> =
                resolved.targets.iter().map(|m| &m.node_id).collect();
            u32::try_from(unique.len()).unwrap_or(u32::MAX)
        },
    }
}

/// The CLI-facing name of a request, for the one error that has to say
/// which verb it was handed.
/// What a **non-member** gets when it asks for anti-entropy (S1).
///
/// Not a refusal, and not the log either. A node admitted a moment ago is a
/// real case — it holds the op that admits it and the peer it dials may not
/// have heard yet — and refusing outright would break the round trip that
/// heals exactly that (the client aborts a tick on an error and never
/// reaches its push leg). So the pull is answered with our version vector
/// and **no ops**: enough for the peer to push us what we lack, which is how
/// its `MemberAdd` arrives, gets its signature and author checked by the
/// merge rules, and makes the next tick an ordinary one.
///
/// Liveness is withheld along with the ops. It is a list of node ids and
/// states, the peer does not need it to compute its push, and a stranger has
/// no business with a roll call.
fn introduce_v1(engine: &crate::state::StateEngine) -> Response {
    Response::Sync(mesh_proto::SyncResponse {
        ops: Vec::new(),
        more: false,
        vv: engine.version_vector(),
        liveness: Vec::new(),
    })
}

/// [`introduce_v1`] for a peer that speaks `OPLOG_MINOR`.
fn introduce_v2(engine: &crate::state::StateEngine) -> Response {
    Response::SyncV2(mesh_proto::SyncResponseV2 {
        ops: Vec::new(),
        more: false,
        vv: engine.version_vector(),
        liveness: Vec::new(),
    })
}

/// The replay guard: a two-bucket time wheel with a hard ceiling (S-M1).
///
/// The old shape was a `HashMap<(UserId, u64), u64>` swept with `retain` on
/// **every request**, which made the cost of one request linear in the
/// number of requests already seen — the classic accidental quadratic — and
/// put no ceiling on the map at all.
///
/// Neither half needed a caller to be authorized. `verify_envelope`
/// authenticates the *signature*; whether that user may do anything is a
/// separate question answered later, so a freshly generated Ed25519 key
/// produces envelopes that pass and land in the table before the request is
/// refused. Signing is cheap. The table was not.
///
/// Two buckets and a rotation: a nonce is remembered for at least `skew` and
/// at most `2 * skew`, which is what the `retain` was approximating, and
/// forgetting is one `Vec` drop per window rather than a scan per request.
/// Both halves are O(1) amortized.
#[derive(Debug, Default)]
struct NonceWheel {
    /// Nonces seen since the last rotation.
    current: HashSet<(UserId, u64)>,
    /// The window before it, still refused, waiting to be dropped whole.
    previous: HashSet<(UserId, u64)>,
    /// When `current` began, in micros.
    rotated_at: u64,
}

/// Nonces one bucket may hold before the wheel turns early.
///
/// Roughly 40 bytes of key each, so two full buckets are tens of megabytes
/// — a bound a person can reason about, rather than "however much the
/// attacker felt like sending". Turning early shortens the replay window
/// under flood, which is the right way round: a replay needs a captured
/// envelope *and* the freshness window still open, while an unbounded table
/// needs only a socket.
const NONCE_BUCKET_CAP: usize = 1 << 18;

impl NonceWheel {
    /// Record `key`; `false` if it has been seen inside the window.
    fn accept(&mut self, key: (UserId, u64), now: u64, skew: u64) -> bool {
        if now.saturating_sub(self.rotated_at) >= skew || self.current.len() >= NONCE_BUCKET_CAP {
            self.previous = std::mem::take(&mut self.current);
            self.rotated_at = now;
        }
        if self.previous.contains(&key) {
            return false;
        }
        self.current.insert(key)
    }
}

fn request_name(req: &IpcRequest) -> &'static str {
    match req {
        IpcRequest::Run { .. } => "run",
        IpcRequest::RunService { .. } => "service run",
        IpcRequest::Parallel { .. } => "parallel",
        _ => "this request",
    }
}

/// Human phrasing for a resource refusal: "4 cores, 48 GB RAM".
fn describe_requirements(reqs: &mesh_proto::Requirements) -> String {
    let mut parts = Vec::with_capacity(3);
    if reqs.cpus > 0 {
        parts.push(format!(
            "{} core{}",
            reqs.cpus,
            if reqs.cpus == 1 { "" } else { "s" }
        ));
    }
    if reqs.mem_mb > 0 {
        parts.push(format!("{} GB RAM", reqs.mem_mb / 1024));
    }
    if reqs.disk_mb > 0 {
        parts.push(format!("{} GB disk", reqs.disk_mb / 1024));
    }
    parts.join(", ")
}

/// Candidate addresses to advertise, using the *real* bound port. When the
/// configured IP is unspecified (0.0.0.0), advertise a routable interface IP
/// plus loopback (interface discovery is refined in M5/M7).
///
/// Routable first, loopback last. Dialers race the whole list either way, but
/// everything that shows or takes *one* address off it — a node table's
/// address column, an invite — should get the one another machine can use.
/// Our loopback is only ever reachable by a peer that shares this host.
/// What is wrong with a `host:port` endpoint, or `None` if nothing is.
///
/// Deliberately not a resolution attempt. A repaired address may well name
/// a host that DNS cannot answer for yet — the operator is often fixing
/// the network and the name service in the same sitting — and refusing it
/// here would turn a typo check into a connectivity check.
///
/// It returns the reason rather than a bool because the reasons are not
/// interchangeable to the person reading them. `"10.20.30.11:0"` is the
/// one worth the words: it is a perfectly good
/// [`SocketAddr`](std::net::SocketAddr) and a completely useless address,
/// because port 0 is the reserved "has not announced a dialable address
/// yet" sentinel that `Transport::classify` skips. Told only "must be
/// host:port", an operator looks at `10.20.30.11:0`, sees a host and a
/// port, and has nowhere to go.
fn host_port_problem(s: &str) -> Option<&'static str> {
    let Some((host, port)) = s.rsplit_once(':') else {
        return Some("must be host:port");
    };
    match port.parse::<u16>() {
        Ok(0) => {
            return Some(
                "has port 0 — the reserved \"no dialable address yet\" sentinel. \
                 Give the port the node actually listens on (`meshnode status` \
                 there shows it; the default is 7777)",
            );
        }
        Ok(_) => {}
        Err(_) => return Some("must end in a port number"),
    }
    if host.is_empty() {
        return Some("has no host");
    }
    if (host.starts_with('[') || host.contains(':')) && s.parse::<std::net::SocketAddr>().is_err() {
        // An IPv6 literal has to be bracketed to be unambiguous, and
        // `resolve` would not take it otherwise either.
        return Some("looks like IPv6 — write it bracketed, as [2001:db8::1]:7777");
    }
    None
}

/// What this node tells the mesh it can be reached at (ENG-22).
///
/// A *specific* `listen` address is the operator being specific, and is
/// advertised as written and alone — they said where this node answers.
///
/// An unspecified one (`0.0.0.0`, `[::]`) means "everywhere", and used to
/// be answered with the IPv4 default route plus loopback: two addresses,
/// on a node listening on every interface. That is right for the
/// single-NIC IPv4 machine and silently wrong for an IPv6-only host —
/// where the v4 probe finds nothing and the node advertises only loopback
/// — and for a multi-NIC host, where the fabric mesh traffic belongs on
/// is by definition not the one holding the default route. See
/// [`crate::addrs`] for what is chosen and in what order.
fn advertised_addrs(
    configured_ip: std::net::IpAddr,
    real_port: u16,
    only_interfaces: &[String],
) -> Vec<String> {
    if !configured_ip.is_unspecified() {
        return vec![crate::addrs::socket_string(configured_ip, real_port)];
    }
    crate::addrs::choose(
        &crate::addrs::enumerate(),
        &crate::addrs::default_route_ips(),
        only_interfaces,
        crate::addrs::Families::of(configured_ip, crate::addrs::dual_stack()),
        real_port,
    )
}

/// Frontend hooks for the daemon loop: how to re-resolve configuration on
/// SIGHUP and how to retune the log filter (the tracing subscriber lives in
/// the binary, not in mesh-core).
/// How the daemon re-resolves configuration on SIGHUP.
pub type ReloadFn = Box<dyn Fn() -> Result<NodeConfig> + Send + Sync>;
/// How the daemon retunes the log filter live.
pub type SetLogLevelFn = Box<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub struct RunHooks {
    pub reload: Option<ReloadFn>,
    pub set_log_level: Option<SetLogLevelFn>,
}

impl std::fmt::Debug for RunHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunHooks")
    }
}

/// Said to a mesh reset that did not come from this machine's own socket.
const RESET_LOCAL_ONLY: &str = "a mesh reset is made at a terminal on the founder's machine \
                                (`meshnode reset`), and is not accepted from anywhere else";

/// How many members a leaving node tells at once.
const RESET_RELAY_WIDTH: usize = 32;

/// How long a leaving node waits for the work it cancelled to end.
const RESET_STOP_GRACE: Duration = Duration::from_secs(30);

/// The exit statuses `meshnode up` uses for refusals a restart cannot fix
/// (ENG-1). The packaged unit lists both in `RestartPreventExitStatus=`,
/// so systemd leaves the node down and says why instead of retrying.
pub mod exit_status {
    /// The configuration does not load or does not check (`EX_CONFIG`).
    pub const CONFIG: i32 = 78;
    /// The data directory was written by a newer release (`EX_DATAERR`).
    pub const DATA_TOO_NEW: i32 = 65;
}

/// Confirm a pending upgrade to this build once the node has been healthy
/// for [`crate::upgrade_guard::HEALTHY_FOR`] without a break (ENG-12).
///
/// Healthy is what the watchdog feeds on: every critical loop alive and
/// moving. Until then each start of this binary counts against it, and
/// enough of them put the previous one back.
fn confirm_upgrade_when_healthy(node: &Node) {
    let dir = node.config().data_dir.clone();
    let Some(pending) = crate::upgrade_guard::pending(&dir) else {
        return;
    };
    if pending.to != VERSION {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    info!(
        from = %pending.from,
        start = pending.starts,
        of = crate::upgrade_guard::MAX_STARTS,
        "upgraded; confirming once healthy for {}s",
        crate::upgrade_guard::HEALTHY_FOR.as_secs()
    );
    let node = node.clone();
    let handle = tokio::spawn({
        let node = node.clone();
        async move {
            let mut healthy_since: Option<Instant> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !node.vitals().check().is_empty() {
                    healthy_since = None;
                    continue;
                }
                let since = *healthy_since.get_or_insert_with(Instant::now);
                if since.elapsed() < crate::upgrade_guard::HEALTHY_FOR {
                    continue;
                }
                match crate::upgrade_guard::confirm(&dir, VERSION, &exe) {
                    Ok(Some(p)) => info!(
                        "upgrade to {VERSION} confirmed; `meshnode upgrade --rollback` still \
                         restores {} from {}",
                        p.from,
                        p.prev.display()
                    ),
                    Ok(None) => {}
                    Err(e) => warn!("cannot confirm the upgrade to {VERSION}: {e}"),
                }
                return;
            }
        }
    });
    node.track(handle);
}

/// Run the full daemon (transport + gossip + IPC) until SIGINT/SIGTERM, a
/// completed drain, or — on SIGHUP — hot-reload what can safely reload
/// (MASTERPLAN §12).
pub async fn run(config: NodeConfig, hooks: RunHooks) -> Result<()> {
    let socket_path = config.socket_path();
    // Replaying a large op-log, or repairing one after a crash, can outlast
    // the unit's start timeout. Ask for more time while — and only while —
    // that work is moving (ENG-1).
    let starting = systemd::extend_while_starting();
    let node = Node::start(config).await;
    drop(starting);
    let node = node?;
    // Under systemd's `Type=notify`: the listener is bound and state is
    // loaded, so *now* the unit is up — not when the process appeared.
    // The watchdog is fed only while the node's critical loops are alive
    // and moving (`crate::vitals`), so a wedged node gets the restart it
    // has earned.
    systemd::ready(&node);
    systemd::watchdog(&node);
    confirm_upgrade_when_healthy(&node);

    #[cfg(unix)]
    let ipc_task = {
        let node = node.clone();
        let socket_path = socket_path.clone();
        let pulse = node.vitals().register("ipc", None);
        tokio::spawn(async move {
            let _pulse = pulse;
            if let Err(e) = crate::ipc::serve(socket_path, node).await {
                error!("ipc server failed: {e}");
            }
        })
    };

    let outcome = wait_for_shutdown(&node, &hooks).await;
    let reason = match &outcome {
        Ok(reason) => {
            info!("shutting down ({reason})");
            (*reason).to_string()
        }
        Err(why) => {
            error!("shutting down: {why}");
            format!("failed: {why}")
        }
    };
    systemd::stopping(&reason);

    // A requested stop lets accepted work finish first (ENG-15). A failed
    // one does not: a core loop has *ended*, so the jobs this node thinks
    // it is running are no longer being supervised by anything, and
    // waiting for them would only hold the restart back.
    if outcome.is_ok() {
        node.shutdown_grace().await;
    }

    #[cfg(unix)]
    {
        node.vitals().stopping();
        ipc_task.abort();
        let _ = std::fs::remove_file(&socket_path);
    }
    node.stop().await;
    // Leaving a reset mesh: now that nothing can write to the op-log, move
    // the mesh's data aside, and stop with the status the unit does not
    // restart — a restart would only find a home waiting to be adopted.
    if let Some(reset) = node.left_mesh() {
        let genesis = node.inner.engine.genesis().clone();
        let marker = crate::reset::leave(&node.config().layout(), &genesis, &reset)?;
        info!(
            mesh = %genesis.body.mesh_name,
            aside = %marker.aside.display(),
            "left the mesh; waiting to be adopted into a new one"
        );
        return Err(CoreError::AwaitingAdoption(marker.awaiting()));
    }
    // A failure exits non-zero, so the service manager restarts the node.
    outcome.map(|_| ()).map_err(CoreError::Failed)
}

/// Resolves when a critical loop has ended (ENG-2), with what ended.
///
/// A stall is left to the watchdog: a stalled loop may still recover, and
/// the watchdog's deadline is the allowance for that. An ended one cannot,
/// so waiting would only keep a node up that nothing can use.
async fn a_loop_ended(vitals: &crate::vitals::Vitals) -> String {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let ended: Vec<String> = vitals
            .check()
            .into_iter()
            .filter(crate::vitals::Problem::is_ended)
            .map(|p| p.to_string())
            .collect();
        if !ended.is_empty() {
            return ended.join("; ");
        }
    }
}

/// The daemon's side of `Type=notify` (`sd_notify(3)`): readiness once,
/// a status line for `systemctl status`, a heartbeat if the unit set
/// `WatchdogSec`, and a stopping note on the way out. Every call is a
/// no-op when `NOTIFY_SOCKET` is unset — a node started by hand, or on a
/// machine without systemd, notices nothing.
mod systemd {
    use super::Node;

    /// Stops the start-deadline extension when dropped.
    pub struct Starting {
        #[cfg(target_os = "linux")]
        done: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    impl Drop for Starting {
        fn drop(&mut self) {
            #[cfg(target_os = "linux")]
            if let Some(done) = &self.done {
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// How often the extension is renewed, and by how much. The service
    /// manager times the start out `EXTEND_BY` after the last renewal once
    /// the unit's own `TimeoutStartSec` has passed.
    #[cfg(target_os = "linux")]
    const EXTEND_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
    #[cfg(target_os = "linux")]
    const EXTEND_BY: std::time::Duration = std::time::Duration::from_secs(30);

    /// While the node starts, renew `EXTEND_TIMEOUT_USEC` each time replay
    /// or repair has advanced since the last look (ENG-1).
    ///
    /// On its own thread, because `Node::start` replays the log on the
    /// thread that is running it, and a renewal that waited for a worker
    /// would arrive after the deadline it was meant to move. Progress, not
    /// time, is what renews: a replay that has stopped moving is exactly
    /// the start that should time out.
    #[cfg(target_os = "linux")]
    pub fn extend_while_starting() -> Starting {
        if std::env::var_os("NOTIFY_SOCKET").is_none() {
            return Starting { done: None };
        }
        let done = Extender {
            every: EXTEND_EVERY,
            by: EXTEND_BY,
        }
        .spawn(crate::vitals::startup_progress_value, |usec, status| {
            let _ = sd_notify::notify(
                false,
                &[
                    sd_notify::NotifyState::ExtendTimeoutUsec(usec),
                    sd_notify::NotifyState::Status(status),
                ],
            );
        });
        Starting { done }
    }

    /// The renewal loop, with its clock, its progress source and its
    /// sender passed in so it can be tested without a service manager.
    #[cfg(target_os = "linux")]
    pub(super) struct Extender {
        pub every: std::time::Duration,
        pub by: std::time::Duration,
    }

    #[cfg(target_os = "linux")]
    impl Extender {
        pub fn spawn(
            self,
            progress: impl Fn() -> u64 + Send + 'static,
            send: impl Fn(u32, &str) + Send + 'static,
        ) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            let done = Arc::new(AtomicBool::new(false));
            let flag = done.clone();
            let usec = u32::try_from(self.by.as_micros()).unwrap_or(u32::MAX);
            let step = (self.every / 8).max(std::time::Duration::from_millis(1));
            let spawned = std::thread::Builder::new()
                .name("meshnode-start".into())
                .spawn(move || {
                    let mut seen = progress();
                    loop {
                        let mut waited = std::time::Duration::ZERO;
                        while waited < self.every {
                            if flag.load(Ordering::Relaxed) {
                                return;
                            }
                            std::thread::sleep(step);
                            waited += step;
                        }
                        let now = progress();
                        if now == seen {
                            continue;
                        }
                        seen = now;
                        send(
                            usec,
                            &format!("starting: replaying and checking state ({now} steps)"),
                        );
                    }
                });
            match spawned {
                Ok(_) => Some(done),
                Err(e) => {
                    tracing::debug!("cannot watch startup progress: {e}");
                    None
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    pub fn ready(node: &Node) {
        use sd_notify::NotifyState;
        let status = format!(
            "{} · listening on {}",
            node.config().node_name,
            node.listen_addr()
        );
        if let Err(e) =
            sd_notify::notify(false, &[NotifyState::Ready, NotifyState::Status(&status)])
        {
            tracing::debug!("sd_notify READY: {e}");
        }
    }

    /// Feed the watchdog while — and only while — the node's critical loops
    /// are alive and moving (ENG-1).
    ///
    /// The heartbeat used to be a bare timer on the runtime, so it went on
    /// ticking with gossip, IPC or dispatch dead, and `WatchdogSec` never
    /// fired for anything but a runtime stopped outright. Now a stalled or
    /// ended loop withholds it, and systemd restarts the node.
    #[cfg(target_os = "linux")]
    pub fn watchdog(node: &Node) {
        let mut usec = 0u64;
        if !sd_notify::watchdog_enabled(false, &mut usec) || usec == 0 {
            return;
        }
        // Half the interval, as the manual recommends: the unit's
        // `WatchdogSec` is the deadline, not the cadence.
        let every =
            std::time::Duration::from_micros(usec / 2).max(std::time::Duration::from_secs(1));
        tracing::info!(interval = ?every, "systemd watchdog armed");
        let vitals = node.vitals().clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            let mut withheld = false;
            loop {
                tick.tick().await;
                let problems = vitals.check();
                if problems.is_empty() {
                    if withheld {
                        tracing::info!("core loops are healthy again; watchdog heartbeat resumed");
                        withheld = false;
                    }
                    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]) {
                        tracing::debug!("sd_notify WATCHDOG: {e}");
                    }
                    continue;
                }
                let why = problems
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                if !withheld {
                    tracing::error!("withholding the watchdog heartbeat: {why}");
                    let status = format!("unhealthy: {why}");
                    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Status(&status)]);
                    withheld = true;
                }
            }
        });
    }

    #[cfg(target_os = "linux")]
    pub fn stopping(reason: &str) {
        use sd_notify::NotifyState;
        let status = format!("stopping ({reason})");
        let _ = sd_notify::notify(
            false,
            &[NotifyState::Stopping, NotifyState::Status(&status)],
        );
    }

    #[cfg(not(target_os = "linux"))]
    pub fn extend_while_starting() -> Starting {
        Starting {}
    }
    #[cfg(not(target_os = "linux"))]
    pub fn ready(_node: &Node) {}
    #[cfg(not(target_os = "linux"))]
    pub fn watchdog(_node: &Node) {}
    #[cfg(not(target_os = "linux"))]
    pub fn stopping(_reason: &str) {}
}

/// What the cache publish pass has already looked at (ENG-19).
///
/// Two halves so that "already considered" stays bounded without ever
/// being *forgotten*: everything that finished at or before
/// `through_micros` has been looked at, and `recent` holds the ids after
/// it, with the instant each finished. When `recent` fills, the watermark
/// swallows the older half.
///
/// The predecessor was a plain `HashSet` cleared wholesale when full,
/// which meant a node with a long history periodically re-read every
/// finished job's spec off disk — the biggest burst of I/O on the busiest
/// node, caused by nothing but its own past.
#[derive(Debug, Default)]
struct Considered {
    through_micros: u64,
    recent: HashMap<JobId, u64>,
}

/// This process's resident set, in bytes (ENG-21). `None` where `/proc`
/// does not answer, which is honest: an invented zero would read as a
/// process using no memory.
fn process_resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// How many file descriptors this process holds (ENG-21).
fn process_open_fds() -> Option<u64> {
    Some(std::fs::read_dir("/proc/self/fd").ok()?.count() as u64)
}

/// Why the daemon is stopping: a request (`Ok`), or a failure (`Err`).
/// Why the node asked its own daemon loop to stop.
fn exit_reason(node: &Node) -> &'static str {
    if node.left_mesh().is_some() {
        "the founder reset the mesh"
    } else {
        "drain complete"
    }
}

async fn wait_for_shutdown(
    node: &Node,
    hooks: &RunHooks,
) -> std::result::Result<&'static str, String> {
    let mut exit_rx = node.exit_requested();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!("cannot install SIGTERM handler: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return Ok("SIGINT");
            }
        };
        let ended = a_loop_ended(node.vitals());
        tokio::pin!(ended);
        let mut sighup = signal(SignalKind::hangup()).ok();
        loop {
            let hup = async {
                match sighup.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => return Ok("SIGINT"),
                _ = sigterm.recv() => return Ok("SIGTERM"),
                _ = exit_rx.changed() => return Ok(exit_reason(node)),
                why = &mut ended => return Err(why),
                _ = hup => {
                    let Some(reload) = hooks.reload.as_ref() else {
                        warn!("SIGHUP received but no reload hook is installed");
                        continue;
                    };
                    match reload() {
                        Ok(new) => {
                            if let Some(set) = hooks.set_log_level.as_ref() {
                                set(&new.log_level);
                            }
                            let notes = node.apply_config(&new);
                            if notes.is_empty() {
                                info!("SIGHUP: configuration unchanged");
                            }
                            for note in notes {
                                info!("SIGHUP: {note}");
                            }
                        }
                        Err(e) => warn!("SIGHUP: reload failed, keeping current config: {e}"),
                    }
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = hooks;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Ok("ctrl-c"),
            _ = exit_rx.changed() => Ok(exit_reason(node)),
            why = a_loop_ended(node.vitals()) => Err(why),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod start_extension_tests {
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::systemd::Extender;

    /// ENG-1: the start deadline is renewed while replay moves, not
    /// merely while time passes, and the renewals stop with the start.
    #[test]
    fn the_start_deadline_is_renewed_only_while_startup_moves() {
        let progress = Arc::new(AtomicU64::new(0));
        let sent: Arc<Mutex<Vec<u32>>> = Arc::default();
        let done = Extender {
            every: Duration::from_millis(40),
            by: Duration::from_secs(30),
        }
        .spawn(
            {
                let progress = progress.clone();
                move || progress.load(Ordering::Relaxed)
            },
            {
                let sent = sent.clone();
                move |usec, _status| sent.lock().push(usec)
            },
        )
        .expect("thread");

        // Moving: renewed, by the configured amount.
        for _ in 0..6 {
            progress.fetch_add(10, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(50));
        }
        let moving = sent.lock().clone();
        assert!(moving.len() >= 2, "renewals while moving: {moving:?}");
        assert!(moving.iter().all(|u| *u == 30_000_000));

        // Stuck: nothing more, however long it waits.
        std::thread::sleep(Duration::from_millis(100));
        let settled = sent.lock().len();
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(sent.lock().len(), settled, "renewed while stuck");

        // Started: the thread stops, even if progress moves again.
        done.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(60));
        progress.fetch_add(10, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(sent.lock().len(), settled, "renewed after start");
    }
}

#[cfg(test)]
mod nonce_wheel_tests {
    use super::{NONCE_BUCKET_CAP, NonceWheel};
    use mesh_proto::UserId;

    const SKEW: u64 = 300_000_000; // 300s in micros, `AUTH_SKEW`.

    fn user(n: u8) -> UserId {
        UserId([n; 32])
    }

    #[test]
    fn a_nonce_is_refused_a_second_time() {
        let mut wheel = NonceWheel::default();
        assert!(wheel.accept((user(1), 7), 1_000, SKEW));
        assert!(!wheel.accept((user(1), 7), 1_100, SKEW));
        // A different user, or a different nonce, is a different request.
        assert!(wheel.accept((user(2), 7), 1_100, SKEW));
        assert!(wheel.accept((user(1), 8), 1_100, SKEW));
    }

    #[test]
    fn a_nonce_survives_one_rotation_and_not_two() {
        let mut wheel = NonceWheel::default();
        assert!(wheel.accept((user(1), 7), SKEW, SKEW));
        // One window on: moved to `previous`, still refused. This is the
        // guarantee the skew window needs — an envelope is only fresh for
        // `skew`, so a nonce remembered for at least that long is enough.
        assert!(!wheel.accept((user(1), 7), 2 * SKEW, SKEW));
        // Two windows on: forgotten, and by then no envelope carrying it
        // could still pass the freshness check.
        assert!(wheel.accept((user(1), 7), 4 * SKEW, SKEW));
    }

    /// The finding: cost per request grew with the number of requests
    /// already seen, and the table had no ceiling. Both are bounded now.
    #[test]
    fn the_table_is_bounded_however_much_is_thrown_at_it() {
        let mut wheel = NonceWheel::default();
        for n in 0..(NONCE_BUCKET_CAP as u64 * 3) {
            // Same instant throughout: only the cap can turn this wheel,
            // which is the case the clock cannot help with.
            assert!(wheel.accept((user(9), n), 1_000, SKEW));
        }
        assert!(
            wheel.current.len() + wheel.previous.len() <= NONCE_BUCKET_CAP * 2,
            "two buckets is the whole budget"
        );
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::render_devices;

    fn a_card(index: u32, model: &str) -> mesh_proto::Device {
        mesh_proto::Device {
            index,
            kind: mesh_proto::DeviceKind::Gpu,
            vendor: "nvidia".into(),
            model: model.into(),
            uuid: format!("GPU-{index}"),
            mem_mb: 24_576,
            numa_node: None,
            health: mesh_proto::DeviceHealth::Ok,
        }
    }

    #[test]
    fn a_card_with_telemetry_renders_every_gauge() {
        let mut out = String::new();
        let card = a_card(0, "NVIDIA GeForce RTX 4090");
        let held = [(card.key(), "job-abc".to_string())].into_iter().collect();
        render_devices(
            &mut out,
            std::slice::from_ref(&card),
            &[mesh_proto::DevicePressure {
                index: 0,
                space: Some(mesh_proto::OrdinalSpace::of(&card)),
                util_pct: Some(73.5),
                mem_free_mb: Some(8192),
                temp_c: Some(64.0),
                throttled: Some(true),
                processes: 2,
            }],
            &held,
        );
        let labels = r#"{index="0",vendor="nvidia",model="NVIDIA GeForce RTX 4090"}"#;
        assert!(
            out.contains(&format!("meshware_gpu_utilisation_pct{labels} 73.5")),
            "{out}"
        );
        assert!(
            out.contains(&format!("meshware_gpu_memory_free_mb{labels} 8192")),
            "{out}"
        );
        assert!(
            out.contains(&format!("meshware_gpu_temperature_celsius{labels} 64")),
            "{out}"
        );
        assert!(
            out.contains(&format!("meshware_gpu_throttled{labels} 1")),
            "{out}"
        );
        // Held and busy are different questions, and both are answered.
        assert!(
            out.contains(&format!("meshware_gpu_held{labels} 1")),
            "{out}"
        );
    }

    /// The distinction the whole block exists to keep: **absent is not
    /// zero**. A card nobody can read must not be exported as an idle one,
    /// or a saturated GPU shows up on the dashboard as free.
    #[test]
    fn a_card_with_no_telemetry_exports_nothing_rather_than_zero() {
        let mut out = String::new();
        render_devices(&mut out, &[a_card(0, "Some GPU")], &[], &Default::default());
        assert!(out.is_empty(), "{out}");

        // And a card whose telemetry arrived with holes exports the fields
        // that came and omits the ones that did not — `held` still lands,
        // because that one is this node's own bookkeeping and is never
        // unknown.
        let mut out = String::new();
        render_devices(
            &mut out,
            &[a_card(1, "Some GPU")],
            // Unnamed, as a pre-1.35 peer's reading is: one card, so the
            // ordinal is enough.
            &[mesh_proto::DevicePressure {
                index: 1,
                space: None,
                util_pct: None,
                mem_free_mb: Some(1024),
                temp_c: None,
                throttled: None,
                processes: 0,
            }],
            &Default::default(),
        );
        assert!(!out.contains("meshware_gpu_utilisation_pct"), "{out}");
        assert!(!out.contains("meshware_gpu_temperature_celsius"), "{out}");
        assert!(out.contains("meshware_gpu_memory_free_mb"), "{out}");
        assert!(
            out.contains("meshware_gpu_held{index=\"1\",vendor=\"nvidia\",model=\"Some GPU\"} 0"),
            "{out}"
        );
    }

    /// A discrete NVIDIA card and an AMD integrated GPU, both device 0: each
    /// series carries its own card's numbers, and a hold on one is not a
    /// hold on the other.
    #[test]
    fn two_vendors_device_zero_export_as_two_cards() {
        let nvidia = a_card(0, "NVIDIA GeForce RTX 4090");
        let amd = mesh_proto::Device {
            vendor: "amd".into(),
            ..a_card(0, "Radeon 680M")
        };
        let reading = |card: &mesh_proto::Device, util: f64| mesh_proto::DevicePressure {
            index: 0,
            space: Some(mesh_proto::OrdinalSpace::of(card)),
            util_pct: Some(util),
            mem_free_mb: None,
            temp_c: None,
            throttled: None,
            processes: 0,
        };
        let held = [(nvidia.key(), "job-abc".to_string())]
            .into_iter()
            .collect();
        let mut out = String::new();
        render_devices(
            &mut out,
            &[nvidia.clone(), amd.clone()],
            &[reading(&nvidia, 97.0), reading(&amd, 3.0)],
            &held,
        );
        let nv = r#"{index="0",vendor="nvidia",model="NVIDIA GeForce RTX 4090"}"#;
        let am = r#"{index="0",vendor="amd",model="Radeon 680M"}"#;
        assert!(
            out.contains(&format!("meshware_gpu_utilisation_pct{nv} 97")),
            "{out}"
        );
        assert!(
            out.contains(&format!("meshware_gpu_utilisation_pct{am} 3")),
            "{out}"
        );
        assert!(out.contains(&format!("meshware_gpu_held{nv} 1")), "{out}");
        assert!(out.contains(&format!("meshware_gpu_held{am} 0")), "{out}");

        // Unnamed readings cannot say which card they are, so neither card
        // exports one.
        let mut out = String::new();
        let unnamed = mesh_proto::DevicePressure {
            space: None,
            ..reading(&nvidia, 97.0)
        };
        render_devices(&mut out, &[nvidia, amd], &[unnamed], &Default::default());
        assert!(out.is_empty(), "{out}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{peer_holds_the_record, request_kind};
    use crate::gossip::SyncLag;

    /// The span field naming a request must be the variant's name, and it
    /// must not cost the payload to produce — this runs for every request
    /// the node ever answers, including a `Run` carrying a megabyte of
    /// script (ENG-21).
    #[test]
    fn a_request_is_named_without_formatting_what_it_carries() {
        let big = "x".repeat(1_000_000);
        let req = mesh_proto::IpcRequest::Run {
            command: big.clone(),
            on: None,
            env: vec![],
            timeout_secs: None,
            fanout: None,
            priority: mesh_proto::JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        };
        assert_eq!(request_kind(&req), "Run");
        // The premise, so this cannot pass by accident: `Debug` on the same
        // value really is a megabyte, and the name did not come from it.
        assert!(format!("{req:?}").len() > big.len());

        // A unit variant has no fields to stop at and must still name
        // itself rather than coming back empty.
        assert_eq!(request_kind(&mesh_proto::IpcRequest::Jobs), "Jobs");
    }

    fn lag(behind: u64, observed: Instant) -> Option<SyncLag> {
        Some(SyncLag {
            behind,
            total: 100,
            authors: 1,
            observed,
        })
    }

    /// The whole point of the reading: caught up, *after* the plan was
    /// published, and still saying it has no plan. It has the op and cannot
    /// read it, and no amount of waiting changes a build.
    #[test]
    fn a_peer_caught_up_after_the_publish_has_been_handed_the_record() {
        let published = Instant::now();
        assert!(peer_holds_the_record(
            lag(0, published + Duration::from_secs(1)),
            published
        ));
    }

    /// The false positive worth avoiding: a peer that synced to zero a
    /// moment *before* the plan existed is fully caught up on a log that
    /// did not contain it yet. Keep waiting.
    #[test]
    fn a_reading_older_than_the_plan_proves_nothing() {
        let published = Instant::now();
        assert!(!peer_holds_the_record(
            lag(0, published - Duration::from_secs(1)),
            published
        ));
    }

    /// A peer still taking ops is simply mid-gossip — the case the retry
    /// loop exists for.
    #[test]
    fn a_peer_still_behind_is_still_catching_up() {
        let published = Instant::now();
        assert!(!peer_holds_the_record(
            lag(7, published + Duration::from_secs(1)),
            published
        ));
    }

    /// A peer that has not synced since this daemon started says nothing
    /// either way, and silence must not read as a verdict.
    #[test]
    fn no_reading_is_not_a_verdict() {
        assert!(!peer_holds_the_record(None, Instant::now()));
    }
}

/// What one attempt at a gang did (C4 §8.2).
///
/// Separate from the *outcome* because an attempt is not necessarily the
/// end: `--gang-restart` runs another one, and telling the client "done"
/// between two of them would be a lie the client acts on. So the terminal
/// frame is the caller's to emit, and this is what it emits from.
#[derive(Debug)]
struct GangAttempt {
    outcome: mesh_proto::GangOutcome,
    succeeded: u32,
    failed: u32,
    /// The sentence a person needs when the attempt did not get as far as
    /// running. `None` means the members ran and the counts tell the story.
    detail: Option<String>,
}

impl GangAttempt {
    fn stopped(outcome: mesh_proto::GangOutcome, detail: Option<String>) -> GangAttempt {
        GangAttempt {
            outcome,
            succeeded: 0,
            failed: 0,
            detail,
        }
    }
}

/// What one offer round did (C4 §8.2).
#[derive(Debug)]
enum GangRoundOutcome {
    /// Every candidate held its reservation and every commit landed. The
    /// members may start.
    Committed,
    /// At least one candidate would not hold, so everything taken this
    /// round was given back. `behind` names the senior gang, when there was
    /// one — the difference between "the mesh is full" and "you are behind
    /// somebody", which is the difference between waiting and resizing.
    Refused {
        held: u32,
        behind: Option<mesh_proto::GangId>,
        reason: String,
    },
}

/// One line describing what a fact collector runs.
///
/// `FactCollector::run` is a [`mesh_proto::StepAction`] (M18 §4.5) and stayed
/// one when the workflow engine was removed — the type is still the mesh's
/// vocabulary for "a thing a node executes", and collectors are the live
/// user of it. This is the old `describe_step_action`, kept for that caller
/// and named for it.
fn describe_collector_action(action: &mesh_proto::StepAction) -> String {
    const MAX: usize = 60;
    let raw = match action {
        mesh_proto::StepAction::Command(cmd) => cmd.replace('\n', " "),
        mesh_proto::StepAction::Service { name, version, .. } => match version {
            Some(v) => format!("{name}@{v}"),
            None => format!("{name}@latest"),
        },
        mesh_proto::StepAction::Script(script) => format!("{} script", script.runtime.as_str()),
        mesh_proto::StepAction::Builtin { task, .. } => task.clone(),
        mesh_proto::StepAction::Ansible(play) => format!("ansible {}", play.playbook),
    };
    if raw.chars().count() <= MAX {
        return raw;
    }
    let cut: String = raw.chars().take(MAX - 1).collect();
    format!("{cut}…")
}
