//! Job execution (MASTERPLAN §9): accept, queue, spawn, stream, cancel,
//! persist.
//!
//! Every job gets a private directory under `<data_dir>/jobs/<job_id>/`
//! holding `spec.json`, `status.json`, `stdout.log`, `stderr.log`, and an
//! empty `work/` cwd. Output streams to disk from the first byte; followers
//! tail the files, so live view and replay are the same code path. Cancel
//! kills the whole process group — no orphan hunting (their gateway makes
//! you `ps` for strays; we don't).
//!
//! Since M5, acceptance is capacity-aware (MASTERPLAN §10.2, §11): jobs run
//! in `slots` concurrent slots (derived from cores unless capped), overflow
//! waits in a bounded priority queue, and a full queue is a definitive
//! `Busy` refusal the submitter spills over to another candidate.
//! Acceptance is idempotent by job id — a re-dispatch (spill retry,
//! adoption after origin failure) of an already-journaled job is a no-op,
//! the invariant that makes at-least-once delivery safe (§10.3).
//!
//! Secrets (MASTERPLAN §7.3): `{{secret:…}}` references in env values are
//! resolved here, at spawn time, in memory. The persisted `spec.json` keeps
//! only the references; resolved values are registered with the log spool's
//! redaction filter before the process starts, and file-injected secrets
//! live in a 0700 `secrets/` dir that is shredded after exit.

use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tracing::{Instrument, info, warn};

use crate::config::{Limits, ProviderConfig};
use crate::content::ContentStore;
use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::secrets::{self, Redactor};
use crate::state::StateEngine;
use mesh_proto::{
    Capacity, Hlc, JobId, JobPayload, JobSpec, JobState, JobSummary, OpBody, Requirements, UserId,
};

const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// How often a follower re-checks a job it has heard nothing from (**M-8**).
///
/// This was the *cadence*: every follower woke 12.5 times a second, opened
/// both log files, `fstat`ed them and took the scheduler lock to ask
/// whether the job was still alive — so a hundred followers cost ~2 500
/// `open`s a second and contended with `accept` for the same mutex. Writes
/// now wake followers directly (see [`JobManager::log_writes`]), so this is
/// only the backstop that notices *termination*, which produces no bytes to
/// be woken by.
const TAIL_IDLE: Duration = Duration::from_millis(500);
/// How long a timed-out parallel item is given to go down after each
/// signal, before the next one (ENG-16). Short on purpose: the item's own
/// deadline has already passed, and this is only the courtesy that lets a
/// tool remove its temporary files.
const ITEM_KILL_GRACE: Duration = Duration::from_secs(2);
const CHUNK: usize = 64 * 1024;

/// Executor-local job status, persisted as `status.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub started_at_micros: Option<u64>,
    pub finished_at_micros: Option<u64>,
    /// What this job cost, filled in at exit (C1 P1). `#[serde(default)]`
    /// because every status file written before C1 lacks it, and a node
    /// that cannot read its own job records is a worse problem than a job
    /// whose cost is unknown.
    #[serde(default)]
    pub charge: Option<crate::accounting::Charge>,
}

/// What a running job's process group is, written beside the job so that a
/// restart can find what the old daemon left behind (ENG-16).
///
/// Every job is spawned into its own process group (`process_group(0)`, so
/// pgid = pid), which is how cancel reaches a whole tree. That number lived
/// only in [`RunningJob::pid`], in memory — so a daemon that crashed left
/// its jobs running with nothing that knew their pids, holding CPU,
/// memory, ports and file locks on a node that had already decided the
/// slots were free. On a busy executor that is how a machine ends up with
/// a dozen forgotten `ansible-playbook`s on it.
///
/// **A pid on its own is not safe to kill.** Pids are reused, and killing a
/// whole process group by a recycled number would kill a stranger's work.
/// Two fields close that:
///
/// - `boot_id` — pids from a previous boot do not exist at all, so a record
///   from before a reboot is discarded without looking at anything.
/// - `starttime` — the kernel's own start time for that pid, in clock ticks
///   since boot (`/proc/<pid>/stat` field 22). Within one boot it is what
///   distinguishes our process from whatever inherited its number.
///
/// Both must match before a signal is sent. If either cannot be read, the
/// record is dropped and nothing is killed: leaving a stray process is a
/// bounded, visible problem, and killing the wrong one is not.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcRecord {
    /// pgid, which is the main process's pid.
    pid: u32,
    /// `/proc/<pid>/stat` field 22, at the moment we recorded it.
    starttime: u64,
    /// `/proc/sys/kernel/random/boot_id` — this boot of this machine.
    boot_id: String,
}

#[derive(Debug)]
struct RunningJob {
    cancel: watch::Sender<bool>,
    /// Who is charged for it (C1 P3).
    principal: mesh_proto::UserId,
    /// `--preemptible`: this may be killed to give somebody back their
    /// share (C1 P5). Opt-in per submission, never a default.
    preemptible: bool,
    /// What this job reserved when it started (M9 §5.3). Held so the next
    /// admission decision knows what is already spoken for.
    reqs: Requirements,
    /// Its main process, once spawned (Phase B, E3) — for measuring what a
    /// running instance uses. In the running table rather than a map of its
    /// own, so it is forgotten by the same removal that frees the slot.
    pid: Option<u32>,
}

/// A job waiting for a free slot (MASTERPLAN §10.2).
#[derive(Debug)]
struct QueuedJob {
    spec: JobSpec,
    /// Arrival order within a priority class.
    seq: u64,
}

/// Slot + queue bookkeeping, guarded by one lock so the capacity check and
/// the insert are atomic.
#[derive(Debug, Default)]
struct Sched {
    running: HashMap<JobId, RunningJob>,
    queued: Vec<QueuedJob>,
    next_seq: u64,
    /// Soft reservations held for gangs whose commit has not landed yet
    /// (C4 P1). Under *this* lock and no other, because a lease is only
    /// worth anything if the capacity check and the reservation are atomic
    /// with respect to ordinary admission — a gang that raced a `SubmitJob`
    /// and lost half its cores has reserved nothing.
    leases: crate::gangs::Leases,
    /// Jobs that have passed admission and are being journalled right now
    /// (**H-2**, ENG-20).
    ///
    /// `accept` used to hold this lock across two fsynced file writes and
    /// an fsynced op commit — so accepting one job froze the node's whole
    /// scheduler until the disk had confirmed several writes, and log
    /// followers, status queries, cancels and every other submission
    /// queued behind the disk. The writes now happen with the lock
    /// released, which opens a window in which a second `accept` could see
    /// capacity the first one has already spent.
    ///
    /// This closes it. An in-flight acceptance counts exactly as a queued
    /// job does, in the slot/queue test and in the resource total — the
    /// conservative direction, and the same arithmetic, so no admission
    /// rule had to learn a new case.
    admitting: Vec<(JobId, Requirements)>,
}

impl Sched {
    /// Pop the best pending job: highest priority, then **whoever is
    /// furthest behind their fair share**, then earliest arrival (C1 P4).
    ///
    /// Priority stays the outer key deliberately. A share is a statement
    /// about *whose turn*, and it must not quietly overrule an operator who
    /// said *this first* — the two answer different questions and a
    /// scheduler that conflates them surprises people.
    ///
    /// Arrival order breaks every remaining tie, so a mesh with no usage
    /// behaves exactly as it did before this existed.
    /// `inventory` is the node's devices, and the reason this takes an
    /// argument at all (C5 P4): a queued job that asked for a GPU passed
    /// admission when it arrived, and by the time a slot frees the card may
    /// be held by something that started in between. Promoting it anyway
    /// would start a two-GPU job with no GPUs — the resource model's
    /// authoritative check silently skipped because it happens at
    /// *acceptance* and this is a *promotion*.
    ///
    /// So a job whose devices are not free right now is not the best
    /// candidate; it stays queued, and the next `try_promote` — which runs
    /// on every slot release and on the maintenance tick — reconsiders it.
    fn pop_best(
        &mut self,
        shares: &BTreeMap<mesh_proto::UserId, u32>,
        inventory: &[mesh_proto::Device],
    ) -> Option<JobSpec> {
        let now = mesh_proto::now_micros();
        let leases = &self.leases;
        let best = self
            .queued
            .iter()
            .enumerate()
            .filter(
                |(_, q)| match mesh_proto::DeviceRequest::from_env(&q.spec.env) {
                    None => true,
                    Some(req) => {
                        leases.free_devices(inventory, &req, now, |_| None).len()
                            >= req.effective_count() as usize
                    }
                },
            )
            .max_by_key(|(_, q)| {
                let share = shares.get(&q.spec.submitter_user).copied().unwrap_or(0);
                (
                    q.spec.priority,
                    std::cmp::Reverse(share),
                    std::cmp::Reverse(q.seq),
                )
            })?
            .0;
        Some(self.queued.swap_remove(best).spec)
    }

    /// The pending queue, for `meshnode queue` (C1 P4).
    fn pending(&self) -> Vec<(mesh_proto::UserId, JobId, mesh_proto::JobPriority, u64)> {
        self.queued
            .iter()
            .map(|q| (q.spec.submitter_user, q.spec.job_id, q.spec.priority, q.seq))
            .collect()
    }

    /// Resources this node has already promised — to running work, to work
    /// it accepted but has not started yet (M9 §5.3), and to gang leases
    /// (C4 P1). Admission counts all three: a queued job's reservation is a
    /// promise too, a lease is a promise the mesh is relying on, and
    /// handing the same cores out twice is how a mesh ends up with one
    /// overloaded node beside an idle one.
    ///
    /// Two escape hatches, which the gang path needs and
    /// nothing else does.
    ///
    /// `own_gang` drops the lease held for the gang whose own member job is
    /// being admitted: that reservation is *about to become* this job's, so
    /// counting both would make a gang refuse itself at the last step.
    ///
    /// `wounding` drops leases the caller is proposing to yield, so
    /// wound-wait can ask "would there be room if these junior gangs gave
    /// theirs back" without mutating anything first.
    fn reserved_excluding(
        &self,
        own_gang: Option<mesh_proto::GangId>,
        wounding: &[mesh_proto::GangId],
    ) -> Requirements {
        let running = self.running.values().map(|job| job.reqs);
        let queued = self
            .queued
            .iter()
            .map(|q| Requirements::from_env(&q.spec.env));
        let leased = self
            .leases
            .iter()
            .filter(|l| Some(l.gang) != own_gang && !wounding.contains(&l.gang))
            .map(|l| l.reqs);
        // In-flight acceptances (H-2): admitted, not yet in `queued` or
        // `running` because their journal is still being written.
        let admitting = self.admitting.iter().map(|(_, reqs)| *reqs);
        running.chain(queued).chain(leased).chain(admitting).fold(
            Requirements::default(),
            |acc, reqs| Requirements {
                cpus: acc.cpus.saturating_add(reqs.cpus),
                mem_mb: acc.mem_mb.saturating_add(reqs.mem_mb),
                disk_mb: acc.disk_mb.saturating_add(reqs.disk_mb),
            },
        )
    }
}

/// Monotonic counters and latencies for the work this node executes
/// (ENG-21).
///
/// **Counted as it happens, never derived from replicated state.**
/// `meshware_jobs_finished_total` used to be computed by counting the
/// terminal job summaries this node still held — so the retention sweep
/// made it go *down*, which Prometheus reads as a counter reset and turns
/// into a spike of phantom completions. A counter has to be the number of
/// times a thing happened, and the only place that is known is where it
/// happens.
///
/// They reset when the process does, which is what every counter in every
/// exporter does and what `rate()` is built to handle. What they must not
/// do is move backwards while the process is up.
#[derive(Debug)]
pub struct JobMeters {
    /// Accepted onto this node, whether it ran or queued.
    pub accepted: AtomicU64,
    /// Promoted out of the queue into a slot.
    pub started: AtomicU64,
    pub succeeded: AtomicU64,
    pub failed: AtomicU64,
    pub cancelled: AtomicU64,
    pub timed_out: AtomicU64,
    /// Refused at admission — the number an operator sizes a node by.
    pub refused: AtomicU64,
    /// How long a job waited in the queue before it started. A *latency*:
    /// a queue wait past ten seconds is a capacity answer, not a shape.
    pub queue_wait: crate::latency::Histogram,
    /// How long a job ran, from start to terminal. On the work ladder
    /// ([`crate::latency::WORK_BUCKETS`]), because a job is not a call:
    /// against the latency buckets every real job landed in `+Inf` and the
    /// histogram said only "longer than ten seconds", for all of them.
    pub duration: crate::latency::Histogram,
}

impl Default for JobMeters {
    fn default() -> JobMeters {
        JobMeters {
            accepted: AtomicU64::new(0),
            started: AtomicU64::new(0),
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            queue_wait: crate::latency::Histogram::default(),
            duration: crate::latency::Histogram::for_work(),
        }
    }
}

impl JobMeters {
    /// One finished job, by the state it finished in.
    pub fn finished(&self, state: JobState) {
        let counter = match state {
            JobState::Succeeded => &self.succeeded,
            JobState::Failed => &self.failed,
            JobState::Cancelled => &self.cancelled,
            JobState::TimedOut => &self.timed_out,
            // Not terminal: nothing finished.
            JobState::Queued | JobState::Running => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Every terminal count, for rendering, with the label each carries.
    pub fn finished_by_state(&self) -> [(&'static str, u64); 4] {
        [
            ("succeeded", self.succeeded.load(Ordering::Relaxed)),
            ("failed", self.failed.load(Ordering::Relaxed)),
            ("cancelled", self.cancelled.load(Ordering::Relaxed)),
            ("timed-out", self.timed_out.load(Ordering::Relaxed)),
        ]
    }
}

/// Capacity held for a job between passing admission and being placed
/// (**H-2**, ENG-20).
///
/// `accept` decides under the scheduler lock, journals without it, and
/// places under it again. In between, the job exists nowhere the admission
/// tests can see it — so this puts it in `Sched::admitting`, where they
/// count it as they would a queued job, and takes it out again however the
/// acceptance ends.
///
/// A guard rather than two bare calls because the journalling in between is
/// full of `?`: a write that fails must not leave capacity reserved for a
/// job that will never exist, and remembering to release it on each of six
/// error paths is the kind of thing that is right when written and wrong a
/// year later.
struct Reservation {
    manager: Arc<JobManager>,
    job: JobId,
    /// Cleared by `commit`, so the drop does nothing after a success.
    held: bool,
}

impl Reservation {
    /// Takes the *held* guard, not the mutex: this has to happen inside
    /// the section that made the decision, or the reservation and the
    /// decision are not atomic and the window is back.
    fn take(
        manager: &Arc<JobManager>,
        sched: &mut Sched,
        job: JobId,
        reqs: Requirements,
    ) -> Reservation {
        sched.admitting.push((job, reqs));
        Reservation {
            manager: manager.clone(),
            job,
            held: true,
        }
    }

    /// The job is being placed now: the reservation has done its work.
    fn commit(mut self, sched: &mut Sched) {
        sched.admitting.retain(|(id, _)| *id != self.job);
        self.held = false;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        // An error path. Give the capacity back, and say nothing: the
        // caller is already returning the error that explains it.
        self.manager
            .sched
            .lock()
            .admitting
            .retain(|(id, _)| *id != self.job);
    }
}

/// One follower frame.
#[derive(Debug, Clone)]
pub enum LogEvent {
    Chunk {
        stream: u8,
        data: Vec<u8>,
    },
    Done {
        state: JobState,
        exit_code: Option<i32>,
    },
    /// This node has no record of the job on disk (M31 A2).
    ///
    /// **It says nothing about the job's outcome**, deliberately, and that
    /// restraint is the point: the previous code guessed `Failed` here and
    /// so reported a successful job as failed once `[retention]` removed
    /// its directory. Only the *replicated* summary knows what happened,
    /// and the caller holds it.
    Gone,
}

/// How this node removes a container it started (N6, T2).
///
/// One enum rather than two call sites, because `slot_freed` must not care
/// which it is: the plugin and the built-in engine answer the same question
/// and the resolution order between them is settled once, in
/// `runtime::prepare_container`.
#[derive(Debug, Clone)]
enum Runner {
    Plugin(String),
    Engine(crate::engines::Engine),
}

#[derive(Debug)]
pub struct JobManager {
    engine: Arc<StateEngine>,
    content: ContentStore,
    /// This node's identity key, for unwrapping secret data keys.
    node_key: Keypair,
    /// External secret providers configured on this node. Behind a lock so
    /// a SIGHUP config reload can swap them live (MASTERPLAN §12).
    providers: Mutex<BTreeMap<String, ProviderConfig>>,
    data_dir: PathBuf,
    jobs_dir: PathBuf,
    /// What a job's output may cost this node's disk (ENG-4): bytes kept
    /// per stream, and whether output is being kept at all — the daemon
    /// pauses it while the data directory is below its floor, so the
    /// op-log keeps the room it needs.
    output: OutputBudget,
    /// Per-job filesystem sandboxing (`crate::sandbox`). Behind a lock so
    /// a SIGHUP reload can change it without a restart.
    sandbox: Mutex<crate::sandbox::Policy>,
    /// Per-job cgroup enforcement (E2.1), when this node can do it.
    /// `None` means declared cpu/memory limits are scheduling inputs only,
    /// which the daemon logged at startup and `doctor` reports.
    cgroups: Option<crate::cgroup::Delegation>,
    /// Capacity-derived concurrency + bounded queue (MASTERPLAN §10.2, §11),
    /// atomics so a SIGHUP reload can retune them without a restart.
    slots: AtomicU32,
    queue: AtomicU32,
    min_free_mem_mb: AtomicU64,
    /// This node's advertised hardware (M9 §5.1), the permanent ceiling
    /// resource admission bin-packs against. Behind a lock so a SIGHUP
    /// reload can refresh it alongside the limits.
    capacity: Mutex<Capacity>,
    sched: Mutex<Sched>,
    /// Where finished work is charged (C1 P1). Accumulates in memory and
    /// publishes on a timer, so accounting costs ops proportional to
    /// *principals* rather than to jobs.
    meter: Arc<crate::usage::UsageMeter>,
    /// This node's accelerators (C5 §9.1), refreshed on boot, on SIGHUP
    /// reload and whenever the periodic re-probe sees the hardware change.
    /// Behind a lock for the same reason `capacity` is: a driver upgrade
    /// must be able to re-advertise without a restart.
    devices: Mutex<Vec<mesh_proto::Device>>,
    /// The freshest device telemetry this node took of itself (C5 P5).
    /// Read by allocation to prefer the idlest card, and answered to peers
    /// through `Response::PressureV2`.
    device_pressure: Mutex<Vec<mesh_proto::DevicePressure>>,
    /// Running jobs the deployment supervisor has asked to be recorded as
    /// **failed** rather than cancelled when killed (C2 P3): a health-check
    /// failure is a failure, and it must count against the restart budget
    /// so a health-flapping service trips `degraded` instead of restarting
    /// forever. Ordinary cancels (drain, rollout cutover, removal) are not
    /// marked, and stay `Cancelled`.
    fail_marked: Mutex<HashSet<JobId>>,
    /// Live job credentials: an ephemeral public key, the job that owns it,
    /// and the user it speaks for. See [`JobManager::mint_job_credential`].
    ///
    /// In memory and node-local on purpose. There is no replicated record
    /// of these and there must not be — the property that makes them safe
    /// is that the key means nothing anywhere else, and nothing at all once
    /// the job it belongs to has ended.
    job_credentials: Mutex<HashMap<UserId, (JobId, UserId)>>,
    /// Time-boxed caps on `slots` (`crate::quiet`). Read on every admission
    /// and promotion decision; empty on almost every node, which is why the
    /// read is a lock rather than something cleverer.
    quiet: Mutex<crate::quiet::QuietHours>,
    /// `[network]` — the dynamic port range, the reserved list, and the
    /// volumes this node lends (N2 P1, N5 P1). Behind a lock so a SIGHUP
    /// can change any of them without a restart.
    network: Mutex<crate::config::NetworkConfig>,
    /// The address a job is told to advertise itself on (N3).
    ///
    /// `advertise` where one is configured and the listen address
    /// otherwise — a job behind NAT that advertised its listen address
    /// would be publishing an address nothing can reach.
    advertise: Mutex<String>,
    /// The address a job is told to bind its ports on,
    /// `MESHWARE_IP_<label>` — this node's listen address, so a wildcard
    /// listen tells the job every interface.
    bind_host: Mutex<String>,
    /// `[runtime] container_plugin` — an operator's override for how this
    /// node runs containers (N4, T2).
    ///
    /// `None` is the ordinary case since T2: the node drives whatever
    /// engine it detected. Behind a lock like `sandbox` so a SIGHUP can
    /// install one without a restart.
    container_plugin: Mutex<Option<String>>,
    /// The container engine this node detected for itself (T2).
    ///
    /// Detection shells out, so it is done once on the runtime probe and
    /// held here — never per job start, where it would add a process spawn
    /// to the latency MASTERPLAN bet #3 calls "the product".
    container_engine: Mutex<Option<crate::engines::Engine>>,
    /// What every runtime probe last answered (T1), for `meshnode
    /// runtimes`, `doctor`, and this node's own `cap.rt` tag.
    runtime_reports: Mutex<Vec<mesh_proto::runtimes::RuntimeReport>>,
    /// What each running job registered itself as (N3 P1).
    ///
    /// Built at start, where the spec's environment and the port
    /// allocation are both in hand — and they are only ever both in hand
    /// *here*, on the executor, at that moment. A dynamic port does not
    /// exist before the allocation and the request does not survive into
    /// `JobSummary`, so anywhere else would be reconstructing one half
    /// from the other. Dropped with the job, by the same path that gives
    /// the port back.
    registrations: Mutex<HashMap<JobId, Vec<mesh_proto::Registration>>>,
    /// Per-job log-write counters, for waking followers (**M-8**).
    ///
    /// The spool bumps a job's counter after every write; a follower holds
    /// a `watch::Receiver` on it and is woken the moment bytes land instead
    /// of rediscovering them up to 80 ms later. A `watch` rather than a
    /// `Notify` because a `watch` cannot drop an edge: it carries the
    /// latest value, so a follower that was busy sending the previous chunk
    /// when the next write happened sees the change immediately rather than
    /// sleeping through it.
    ///
    /// Entries are removed when the job's process ends, which is the last
    /// moment anything can write to its log.
    log_writes: Mutex<HashMap<JobId, watch::Sender<u64>>>,
    /// Job ids this node still holds a journal for — the in-memory mirror
    /// of "`<jobs_dir>/<id>/spec.json` exists" (**H-1**).
    ///
    /// [`Self::has_record`] is asked once per row by every reader of the
    /// job table, and answering it with a `stat()` made listing jobs cost
    /// one syscall per job the mesh remembers — under the state read lock,
    /// which is the lock every arriving op needs. The set is seeded from
    /// the directory at construction and maintained at the four places a
    /// journal is created ([`Self::accept`], [`Self::record_cached`]) or
    /// removed ([`Self::forget`], [`Self::take_queued`],
    /// [`Self::clear_finished`], [`Self::sweep_retention`]) — every one of
    /// them inside this type, which is why the mirror can be exact rather
    /// than a cache with a staleness story.
    spooled: RwLock<HashSet<JobId>>,
    /// What this node has actually done, counted as it happens (ENG-21).
    meters: JobMeters,
}

/// The smallest HLC that still wins LWW against `hlc` — used to close a
/// record-less summary without outranking anything authored after it.
fn next_after(hlc: Hlc) -> Hlc {
    match hlc.logical.checked_add(1) {
        Some(logical) => Hlc {
            wall_micros: hlc.wall_micros,
            logical,
        },
        None => Hlc {
            wall_micros: hlc.wall_micros.saturating_add(1),
            logical: 0,
        },
    }
}

impl JobManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Arc<StateEngine>,
        content: ContentStore,
        node_key: Keypair,
        providers: BTreeMap<String, ProviderConfig>,
        data_dir: &Path,
        limits: Limits,
        quiet: crate::quiet::QuietHours,
        capacity: Capacity,
        sandbox: crate::sandbox::Policy,
    ) -> Arc<Self> {
        let meter = crate::usage::UsageMeter::new(engine.clone());
        // Said once, at startup, so "why is nothing sandboxed" has an
        // answer in the log rather than a search.
        match (sandbox.mode, crate::sandbox::unavailable_reason(&sandbox)) {
            (crate::sandbox::Mode::Off, _) => info!("job sandbox off ([runtime] sandbox)"),
            (_, Some(why)) => warn!("jobs run unsandboxed: {why}"),
            (_, None) => info!("jobs run under a Landlock sandbox"),
        }
        // E2.1: per-job cgroups, if this node can. Said once at startup for
        // the same reason the sandbox is — and the *negative* is the one
        // that matters, because an operator who believes `--mem` is
        // enforced when it is not has been misled by us.
        let cgroups = match crate::cgroup::Delegation::probe() {
            Ok(d) => match d.prepare() {
                Ok(()) => {
                    d.reap_orphans();
                    Some(d)
                }
                Err(why) => {
                    warn!("declared cpu/memory limits are NOT enforced: {why}");
                    None
                }
            },
            Err(why) => {
                warn!("declared cpu/memory limits are NOT enforced: {why}");
                None
            }
        };
        let manager = Arc::new(Self {
            cgroups,
            engine,
            content,
            node_key,
            providers: Mutex::new(providers),
            data_dir: data_dir.to_path_buf(),
            jobs_dir: data_dir.join("jobs"),
            output: OutputBudget::default(),
            sandbox: Mutex::new(sandbox),
            slots: AtomicU32::new(limits.slots),
            queue: AtomicU32::new(limits.queue),
            min_free_mem_mb: AtomicU64::new(limits.min_free_mem_mb),
            quiet: Mutex::new(quiet),
            capacity: Mutex::new(capacity),
            sched: Mutex::new(Sched::default()),
            meter,
            fail_marked: Mutex::new(HashSet::new()),
            job_credentials: Mutex::new(HashMap::new()),
            devices: Mutex::new(Vec::new()),
            device_pressure: Mutex::new(Vec::new()),
            network: Mutex::new(crate::config::NetworkConfig::default()),
            advertise: Mutex::new(String::new()),
            bind_host: Mutex::new(std::net::Ipv4Addr::UNSPECIFIED.to_string()),
            container_plugin: Mutex::new(None),
            container_engine: Mutex::new(None),
            runtime_reports: Mutex::new(Vec::new()),
            registrations: Mutex::new(HashMap::new()),
            log_writes: Mutex::new(HashMap::new()),
            spooled: RwLock::new(HashSet::new()),
            meters: JobMeters::default(),
        });
        // Before `sweep_orphans`, which reads it through `has_record`.
        manager.seed_spooled();
        manager.sweep_orphans();
        manager
    }

    /// Read the journal directory once at boot to seed [`Self::spooled`]
    /// (**H-1**). The one directory scan; every later answer is a lookup.
    fn seed_spooled(&self) {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else {
            return;
        };
        let mut set = HashSet::new();
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|s| JobId::parse(s).ok())
            else {
                continue;
            };
            if entry.path().join("spec.json").exists() {
                set.insert(id);
            }
        }
        *self.spooled.write() = set;
    }

    /// Note that a journal now exists for `id`.
    fn mark_spooled(&self, id: &JobId) {
        self.spooled.write().insert(*id);
    }

    /// Note that a journal no longer exists for `id`.
    fn unmark_spooled(&self, id: &JobId) {
        self.spooled.write().remove(id);
    }

    /// The write counter for a job's logs, creating it if needed (**M-8**).
    /// Keep at most `bytes` of each job stream (ENG-4).
    pub fn set_output_limit(&self, bytes: u64) {
        self.output.limit.store(bytes, Ordering::Relaxed);
    }

    /// Stop, or resume, keeping job output (ENG-4).
    pub fn set_output_paused(&self, paused: bool) {
        self.output.paused.store(paused, Ordering::Relaxed);
    }

    fn log_writes_of(&self, id: &JobId) -> watch::Sender<u64> {
        self.log_writes
            .lock()
            .entry(*id)
            .or_insert_with(|| watch::channel(0u64).0)
            .clone()
    }

    /// Stop tracking a finished job's log writes. Followers still watching
    /// fall back to [`TAIL_IDLE`], which is what notices the termination.
    fn drop_log_writes(&self, id: &JobId) {
        self.log_writes.lock().remove(id);
    }

    /// How many job journals this node holds. Cheap; the mirror knows.
    pub fn spooled_count(&self) -> usize {
        self.spooled.read().len()
    }

    /// Every competing principal's weighted dominant share (C1 P4).
    ///
    /// Replicated counters for the mesh, **plus this node's own unflushed
    /// totals**, which is the difference between ordering a queue on
    /// thirty-second-old news and ordering it on what just happened. A
    /// node's own meter is always ahead of what it has published, and it is
    /// the node's own queue being ordered.
    pub fn shares(&self) -> BTreeMap<mesh_proto::UserId, u32> {
        let me = self.engine.node_id();
        let now = mesh_proto::now_micros();
        let (mut usage, mut mine) = self.engine.read(|s| {
            (
                s.usage_by_principal_excluding(&me, now),
                s.own_usage_windows(&me, now),
            )
        });
        // Per-component max, exactly as the merge itself does — never a
        // sum. The meter's total for a window and the replicated copy of
        // that same window are two readings of one number, and adding them
        // would hand the busiest principal a share that climbs by itself.
        for partial in self.meter.totals() {
            if !mesh_proto::usage_window_is_live(partial.window, now) {
                continue;
            }
            mine.entry((partial.principal, partial.window))
                .and_modify(|cur: &mut mesh_proto::UsagePartial| cur.absorb(&partial))
                .or_insert(partial);
        }
        for partial in mine.values() {
            let e = usage.entry(partial.principal).or_insert((0, 0, 0));
            e.0 = e.0.saturating_add(partial.cpu_secs);
            e.1 = e.1.saturating_add(partial.mem_mb_secs);
            e.2 = e.2.saturating_add(partial.gpu_secs);
        }
        crate::usage::weighted_shares(&usage)
    }

    /// Running jobs that opted in to being preempted, with their owner
    /// (C1 P5). Cheapest possible answer: it never reads the disk and never
    /// takes the state lock.
    pub fn preemptible_running(&self) -> Vec<(JobId, mesh_proto::UserId)> {
        self.sched
            .lock()
            .running
            .iter()
            .filter(|(_, job)| job.preemptible)
            .map(|(id, job)| (*id, job.principal))
            .collect()
    }

    /// Principals with work waiting here, and how much.
    pub fn waiting_principals(&self) -> BTreeMap<mesh_proto::UserId, u32> {
        let mut out: BTreeMap<mesh_proto::UserId, u32> = BTreeMap::new();
        for q in &self.sched.lock().queued {
            *out.entry(q.spec.submitter_user).or_default() += 1;
        }
        out
    }

    /// The spec of a job this node journaled, read back from its own
    /// record. Used to hand a preempted job to somebody else (C1 P5).
    pub fn spec_of(&self, id: &JobId) -> Option<JobSpec> {
        let bytes = std::fs::read(self.job_dir(id).join("spec.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Forget a job's local journal so a re-dispatch elsewhere is the only
    /// record of it (C1 P5, and the same rule `take_queued` follows).
    pub fn forget(&self, id: &JobId) {
        let _ = std::fs::remove_dir_all(self.job_dir(id));
        self.unmark_spooled(id);
    }

    /// The pending queue as `meshnode queue` shows it: who is waiting,
    /// behind whom, and why (C1 P4).
    pub fn queue_rows(&self) -> Vec<mesh_proto::QueueRow> {
        let shares = self.shares();
        let mut pending = self.sched.lock().pending();
        // Exactly the order `pop_best` would run them in, so the table is
        // the schedule rather than a description of one.
        pending.sort_by_key(|(who, _, priority, seq)| {
            let share = shares.get(who).copied().unwrap_or(0);
            (std::cmp::Reverse(*priority), share, *seq)
        });
        let names = self.engine.read(|s| {
            pending
                .iter()
                .map(|(who, _, _, _)| {
                    (
                        *who,
                        s.username(who)
                            .map(str::to_string)
                            .unwrap_or_else(|| who.to_display_string()),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        });
        pending
            .into_iter()
            .enumerate()
            .map(|(position, (who, job, priority, _))| mesh_proto::QueueRow {
                position: position as u32 + 1,
                job,
                principal: who,
                username: names.get(&who).cloned().unwrap_or_default(),
                priority,
                share_permille: shares.get(&who).copied().unwrap_or(0),
            })
            .collect()
    }

    /// The usage meter, for the daemon's flush timer and for `usage --live`.
    pub fn meter(&self) -> &Arc<crate::usage::UsageMeter> {
        &self.meter
    }

    /// Charge a finished job to its submitter (C1 P1).
    ///
    /// Called from exactly one place — the moment a status becomes terminal
    /// — so a job is charged once and only once however it ended. Cancelled
    /// and timed-out work is charged too: it held the slot, and a principal
    /// who cancels late has still spent the cluster's time.
    fn charge(&self, spec: &JobSpec, status: &JobStatus) -> Option<crate::accounting::Charge> {
        let reqs = Requirements::from_env(&spec.env);
        let measured = status.charge.and_then(|c| c.measured_cpu_secs);
        let chargeable = crate::accounting::charge_for(
            spec.submitter_user,
            reqs,
            status.started_at_micros,
            status.finished_at_micros,
            measured,
        )?;
        self.meter.charge(&chargeable);
        Some(chargeable.charge)
    }

    /// Stamp a job's cost onto its status and charge it, once.
    ///
    /// Called at every point a status first becomes terminal. Idempotent by
    /// construction — a status that already carries a charge is not charged
    /// again — which is what makes it safe to call from the several places
    /// a job can end without keeping a set of charged ids in memory.
    fn finalize(&self, spec: &JobSpec, status: &mut JobStatus) {
        if !status.state.is_terminal() || status.charge.is_some_and(|c| c.wall_secs > 0) {
            return;
        }
        if let Some(charge) = self.charge(spec, status) {
            status.charge = Some(charge);
        }
    }

    /// This node's own work counters and latencies (ENG-21).
    pub fn meters(&self) -> &JobMeters {
        &self.meters
    }

    pub fn running_count(&self) -> usize {
        self.sched.lock().running.len()
    }

    pub fn queued_count(&self) -> usize {
        self.sched.lock().queued.len()
    }

    pub fn limits(&self) -> Limits {
        Limits {
            slots: self.slots.load(Ordering::Relaxed),
            queue: self.queue.load(Ordering::Relaxed),
            min_free_mem_mb: self.min_free_mem_mb.load(Ordering::Relaxed),
        }
    }

    /// The configured quiet windows, for `status` and the config reload.
    pub fn quiet(&self) -> crate::quiet::QuietHours {
        self.quiet.lock().clone()
    }

    /// Swap the quiet windows live (SIGHUP reload), then re-examine the
    /// queue: leaving a window is exactly the moment work should start,
    /// and nothing else will notice for up to a tick otherwise.
    pub fn set_quiet(self: &Arc<Self>, quiet: crate::quiet::QuietHours) {
        *self.quiet.lock() = quiet;
        self.try_promote();
    }

    /// How many slots this node will actually start work in **right now**.
    ///
    /// This is `[limits] slots` with any quiet-hours cap applied, and it is
    /// what every admission and promotion decision asks. The configured
    /// number stays what [`Self::limits`] reports and what the mesh has
    /// been told: a quiet node advertises its full size and simply answers
    /// `Busy` sooner, which spills work to a neighbour through the path
    /// that already exists (`crate::quiet`).
    pub fn effective_slots(&self) -> u32 {
        self.effective_limits().slots
    }

    /// [`Self::limits`] with the quiet-hours cap applied to **both** the
    /// slot count and the queue depth.
    ///
    /// The queue has to shrink with the slots, and the reason is the whole
    /// point of the feature. `queue` defaults to `4 x slots` and exists to
    /// absorb *momentary* saturation — a burst the running work will clear
    /// in seconds. A quiet window is not momentary. Leaving the queue at
    /// full depth would make a 0% node accept sixteen jobs and start none
    /// of them until 23:00, and a submitter watching `meshnode run` sit
    /// there cannot tell that apart from a hang.
    ///
    /// So the node refuses instead, with the same `Busy` a full node
    /// answers, and the origin does what it already does with a `Busy`:
    /// hands the work to the next-best candidate (MASTERPLAN §10.2). "Not
    /// here, not now" is a useful answer. "Maybe at midnight" is not.
    pub fn effective_limits(&self) -> Limits {
        let limits = self.limits();
        let quiet = self.quiet.lock();
        let Some(state) = quiet.state_at(
            crate::localtime::LocalZone::local(),
            crate::localtime::now_unix(),
            limits.slots,
        ) else {
            return limits;
        };
        Limits {
            slots: state.slots,
            queue: crate::quiet::capped(limits.queue, state.window.pct),
            ..limits
        }
    }

    /// What quiet hours are doing to this node right now, if anything.
    pub fn quiet_state(&self) -> Option<crate::quiet::QuietState> {
        let slots = self.slots.load(Ordering::Relaxed);
        self.quiet.lock().state_at(
            crate::localtime::LocalZone::local(),
            crate::localtime::now_unix(),
            slots,
        )
    }

    /// When the quiet cap next changes, as an absolute Unix second.
    pub fn quiet_next_change(&self) -> Option<i64> {
        self.quiet.lock().next_change_after(
            crate::localtime::LocalZone::local(),
            crate::localtime::now_unix(),
        )
    }

    /// Retune capacity live (SIGHUP reload, MASTERPLAN §12). A shrunken
    /// slot count never kills running work — it only stops promotions.
    pub fn set_limits(self: &Arc<Self>, limits: Limits, capacity: Capacity) {
        self.slots.store(limits.slots, Ordering::Relaxed);
        self.queue.store(limits.queue, Ordering::Relaxed);
        self.min_free_mem_mb
            .store(limits.min_free_mem_mb, Ordering::Relaxed);
        *self.capacity.lock() = capacity;
        self.try_promote();
    }

    /// The hardware ceiling this node admits against.
    fn capacity(&self) -> Capacity {
        self.capacity.lock().clone()
    }

    /// Swap the external secret providers live (SIGHUP reload).
    pub fn set_providers(&self, providers: BTreeMap<String, ProviderConfig>) {
        *self.providers.lock() = providers;
    }

    /// Settle what a crash or a restart left behind: jobs that were
    /// *running* can never finish, so they are marked failed; jobs that
    /// were still *queued* never started, so they are put back on the queue
    /// (ENG-15).
    ///
    /// The distinction is the whole point. `systemctl restart` is the
    /// documented package-upgrade path, and failing work that had not begun
    /// — work no process ever touched, whose spec is still on disk intact —
    /// made every upgrade lose the backlog. A running job is a different
    /// case: its process is gone with the old daemon, its slot accounting
    /// and its logs stop mid-stream, and there is nothing to resume.
    fn sweep_orphans(self: &Arc<Self>) {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else {
            return;
        };
        let mut requeue: Vec<JobSpec> = Vec::new();
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|s| JobId::parse(s).ok())
            else {
                continue;
            };
            let Some(mut status) = self.read_status(&id) else {
                continue;
            };
            if status.state.is_terminal() {
                continue;
            }
            // Never started: no process ran, no output was written, no
            // slot was held. `started_at_micros` is what says so — the
            // state alone would also match a job promoted a moment before
            // the crash, whose status had not been rewritten yet.
            if status.state == JobState::Queued
                && status.started_at_micros.is_none()
                && let Ok(bytes) = std::fs::read(self.job_dir(&id).join("spec.json"))
                && let Ok(spec) = serde_json::from_slice::<JobSpec>(&bytes)
            {
                requeue.push(spec);
                continue;
            }
            // A queued status with no readable spec is not a job any
            // more; it falls through and is closed like the rest.
            // Its processes may still be here, even though the daemon that
            // started them is not (ENG-16). Killed before the job is
            // settled, so the slot this releases is not handed to new work
            // while the old work is still holding the machine.
            self.kill_leftover(&id);
            status.state = JobState::Failed;
            status.finished_at_micros = Some(mesh_proto::now_micros());
            self.meters.finished(JobState::Failed);
            if let Ok(bytes) = std::fs::read(self.job_dir(&id).join("spec.json"))
                && let Ok(spec) = serde_json::from_slice::<JobSpec>(&bytes)
            {
                // Charged, not forgiven: the work held a slot until this
                // node died, and the principal spent that time.
                self.finalize(&spec, &mut status);
                let _ = self.write_status(&id, &status);
                warn!(job = %id.short(), "orphaned by restart; marked failed");
                self.publish(&spec, &status);
            } else {
                // No spec to rebuild a summary from — `sweep_stranded`
                // closes it off the replicated one instead.
                let _ = self.write_status(&id, &status);
            }
        }
        self.requeue_pending(requeue);
        self.sweep_stranded();
    }

    /// Put jobs that were still queued when the daemon stopped back on the
    /// queue, oldest first (ENG-15).
    ///
    /// The queue *cap* is not applied here. It is an admission control on
    /// work arriving, and none of this is arriving — it was admitted once,
    /// by a node that then went down before it could run it. Refusing it
    /// now would fail jobs for having survived a restart, which is the
    /// behaviour this exists to remove. The cap resumes governing the next
    /// submission, and the backlog drains through the slots as usual.
    ///
    /// Nothing is published: each summary already reads `queued` mesh-wide,
    /// with the HLC it was submitted under, and re-stating it would only
    /// churn the op-log once per restart per job.
    fn requeue_pending(self: &Arc<Self>, mut specs: Vec<JobSpec>) {
        if specs.is_empty() {
            return;
        }
        // Submission order, so the queue comes back in the order it went
        // down in. `try_promote` re-applies priority on top of that, the
        // same as it does for a queue nobody restarted.
        specs.sort_by_key(|spec| spec.created_at_micros);
        let count = specs.len();
        {
            let mut sched = self.sched.lock();
            for spec in specs {
                let seq = sched.next_seq;
                sched.next_seq += 1;
                sched.queued.push(QueuedJob { spec, seq });
            }
        }
        info!(jobs = count, "queued work restored after restart");
        // Slots are all free at this point, so most of this starts now.
        self.try_promote();
    }

    /// Jobs the mesh still credits to *this* node and still shows as live,
    /// for which this node no longer has a record at all.
    ///
    /// [`Self::sweep_orphans`] can only speak for jobs it can still see on
    /// disk. A record that went while its summary was still `running` —
    /// drained and re-dispatched (`take_queued`), preempted
    /// ([`Self::forget`]), retention-swept after a crash left a terminal
    /// status nothing published, or a data dir restored without its job
    /// journal — leaves a row behind that **only this node is allowed to
    /// write**: `JobUpsert` is dropped on apply from any author but the
    /// executor. Nobody else can close it, this node has stopped looking
    /// at it, and `jobs clear` cannot forget it either, because the
    /// watermark only forgets jobs that finished. So it sits at `running`
    /// for as long as the mesh lives, ageing, and an operator's only cure
    /// is a force-cancel by hand — which is not a cure a user should have
    /// to know about.
    ///
    /// Safe at exactly this moment and nowhere else: `sweep_orphans` runs
    /// from the constructor, before a single job has been accepted, so
    /// there is no live work for a stale summary to be mistaken for.
    fn sweep_stranded(&self) {
        let me = self.engine.node_id();
        let stranded: Vec<JobSummary> = self.engine.read(|s| {
            s.jobs
                .values()
                .filter(|j| j.executor == me && !j.state.is_terminal())
                .cloned()
                .collect()
        });
        for mut summary in stranded {
            // A record still here is `sweep_orphans`'s to settle: it has
            // the spec, so it can charge the work and keep the logs.
            if self.has_record(&summary.job_id) {
                continue;
            }
            let id = summary.job_id;
            // Failed, not cancelled, for the reason the orphan sweep gives:
            // the work held a slot on a node that died under it. What is
            // *not* re-derivable is the charge — that needs the spec, and
            // the spec is what went missing.
            summary.state = JobState::Failed;
            summary.finished_at_micros = Some(mesh_proto::now_micros());
            // **One tick past the summary we are closing, not `now`.**
            // Losing a record is also how a *handoff* looks from here:
            // `drain --move` and preemption both delete the journal so the
            // job can run somewhere else, and the summary they leave
            // behind is `queued`/`running` with this node as executor. A
            // close stamped at boot time would carry a fresh HLC and beat
            // the new executor's real report on merge — a job that ran and
            // succeeded elsewhere would read `failed` mesh-wide, which is
            // the same trap the preemption path documents when it waits
            // before handing a job on. Stamped minimally, this op is the
            // latest word only where nothing newer exists: any genuine
            // report authored after the handoff outranks it, and the
            // stranded row is closed only when there really is nobody
            // else to speak for it.
            summary.updated_hlc = next_after(summary.updated_hlc);
            match self.engine.author_op(OpBody::JobUpsert { summary }) {
                Ok(_) => warn!(
                    job = %id.short(),
                    "no local record for a job the mesh still shows running here; closed it"
                ),
                Err(e) => warn!(job = %id.short(), "failed to close stranded job summary: {e}"),
            }
        }
    }

    fn job_dir(&self, id: &JobId) -> PathBuf {
        self.jobs_dir.join(id.to_display_string())
    }

    /// Where a job's outputs live on this node (C7 P2). The cache
    /// publisher needs it, and it needs to be *this* function rather than a
    /// second path-building convention that could drift from it.
    pub fn job_dir_of(&self, id: &JobId) -> PathBuf {
        self.job_dir(id)
    }

    /// A finished job's recorded status, from disk.
    pub fn status_of(&self, id: &JobId) -> Option<JobStatus> {
        self.read_status(id)
    }

    /// Where a job's process-group record lives.
    fn proc_path(&self, id: &JobId) -> PathBuf {
        self.job_dir(id).join("proc.json")
    }

    /// Note the process group this job was spawned into, so that a restart
    /// after a crash can find it (ENG-16).
    ///
    /// Best-effort throughout: a job that runs but whose pid could not be
    /// recorded is better than a job refused because of it, and the only
    /// cost of a missing record is a leftover that has to be found by hand.
    fn record_proc(&self, id: &JobId, pid: Option<u32>) {
        let (Some(pid), Some(boot_id)) = (pid, boot_id()) else {
            return;
        };
        // Read back from /proc rather than stamped from the clock: this has
        // to be the *kernel's* idea of when the process started, because
        // that is what the check on the way back up compares against.
        let Some(starttime) = proc_starttime(pid) else {
            return;
        };
        let record = ProcRecord {
            pid,
            starttime,
            boot_id,
        };
        match serde_json::to_vec(&record) {
            Ok(bytes) => {
                if let Err(e) = crate::durable::write(&self.proc_path(id), &bytes) {
                    tracing::debug!(job = %id.short(), "could not record the process group: {e}");
                }
            }
            Err(e) => tracing::debug!(job = %id.short(), "could not encode the process group: {e}"),
        }
    }

    /// Forget a job's process group: it has been waited for, so there is
    /// nothing left to kill and the record would only be misleading.
    fn clear_proc(&self, id: &JobId) {
        let _ = std::fs::remove_file(self.proc_path(id));
    }

    /// Kill what a previous daemon left running for `id`, if it is still
    /// there and still the same process (ENG-16).
    ///
    /// `SIGKILL`, not `SIGTERM`. By the time this runs the job has been
    /// settled as failed: nothing is reading its output, nothing is
    /// waiting on its exit, and its slot has already been given away. A
    /// polite signal would only mean an unbounded period in which the node
    /// believes it has capacity that something else is still using.
    fn kill_leftover(&self, id: &JobId) -> bool {
        let path = self.proc_path(id);
        let record = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<ProcRecord>(&b).ok());
        let _ = std::fs::remove_file(&path);
        let Some(record) = record else {
            return false;
        };
        // A different boot: these pids are not ours, and nothing from
        // before the reboot is still running anyway.
        if boot_id().as_deref() != Some(record.boot_id.as_str()) {
            return false;
        }
        // The pid is gone, or has been reused by something that is not our
        // job. Either way this is not ours to kill.
        if proc_starttime(record.pid) != Some(record.starttime) {
            return false;
        }
        warn!(
            job = %id.short(),
            pid = record.pid,
            "a previous daemon left this job's processes running; killing the group"
        );
        kill_group(Some(record.pid), true);
        true
    }

    /// A job's status, written so that a power cut leaves the previous
    /// state or the new one and never a truncated file (ENG-16).
    ///
    /// This file is the only thing that can settle a job after a crash.
    /// Written in place it spent a moment empty on every transition, and a
    /// node that stopped in that moment came back holding a job whose
    /// status could not be read — which the orphan sweep skips, so it sat
    /// at `running` mesh-wide for ever.
    fn write_status(&self, id: &JobId, status: &JobStatus) -> Result<()> {
        let path = self.job_dir(id).join("status.json");
        let bytes = serde_json::to_vec_pretty(status)
            .map_err(|e| CoreError::Job(format!("encode status: {e}")))?;
        crate::durable::write(&path, &bytes)
    }

    fn read_status(&self, id: &JobId) -> Option<JobStatus> {
        let bytes = std::fs::read(self.job_dir(id).join("status.json")).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn publish(&self, spec: &JobSpec, status: &JobStatus) {
        let runtime = match &spec.payload {
            JobPayload::Command(_) => "bash".to_string(),
            JobPayload::Service(s) => s.runtime.as_str().to_string(),
            JobPayload::Script(s) => s.runtime.as_str().to_string(),
        };
        let summary = JobSummary {
            job_id: spec.job_id,
            cmd_preview: spec.preview(),
            runtime,
            submitter: spec.submitter,
            submitter_user: spec.submitter_user,
            executor: self.engine.node_id(),
            state: status.state,
            exit_code: status.exit_code,
            created_at_micros: spec.created_at_micros,
            started_at_micros: status.started_at_micros,
            finished_at_micros: status.finished_at_micros,
            fanout: spec.fanout,
            updated_hlc: self.engine.clock().now(),
        };
        if let Err(e) = self.engine.author_op(OpBody::JobUpsert { summary }) {
            warn!(job = %spec.job_id.short(), "failed to publish job summary: {e}");
        }
    }

    /// Accept a job: start it if a slot is free, queue it if the bounded
    /// queue has room, refuse with [`CoreError::Busy`] otherwise so the
    /// submitter can spill to another candidate (MASTERPLAN §10.2).
    ///
    /// Idempotent by job id: a job already journaled here (spill retry or
    /// adoption re-dispatch, §10.3) is acknowledged without running twice.
    pub fn accept(self: &Arc<Self>, spec: JobSpec) -> Result<()> {
        let dir = self.job_dir(&spec.job_id);
        // Outside the scheduler lock: a stat and, at most, a re-publish of
        // a record this node already holds. It touches nothing the
        // scheduler owns, and it used to hold the lock across an op commit
        // (**H-2**).
        if dir.join("spec.json").exists() {
            // Already journaled here: acknowledged, and never run twice.
            //
            // **And said again when the mesh has no record of a job that
            // finished** (Phase B hardening). A deployment's instance ids are
            // derived, so a start can meet a journal whose record replicated
            // state has let go of — the same name removed and created again,
            // or a closed job past retention. Acknowledging in silence left
            // the reconciler waiting forever for a record nothing would ever
            // publish; the record moves it on to the next incarnation.
            if !self.engine.read(|s| s.jobs.contains_key(&spec.job_id))
                && let Some(status) = self.read_status(&spec.job_id)
                && status.state.is_terminal()
            {
                let journaled = std::fs::read(dir.join("spec.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<JobSpec>(&b).ok());
                self.publish(journaled.as_ref().unwrap_or(&spec), &status);
            }
            return Ok(());
        }
        let effective = self.effective_limits();
        let slots = effective.slots as usize;
        let queue = effective.queue as usize;
        let mut sched = self.sched.lock();
        // The authoritative idempotency test, in memory and under the lock
        // (**H-2**). The stat above is the fast path and cannot be the
        // whole answer any more: with the journalling moved out of the
        // critical section, two re-dispatches of the same job can both find
        // no `spec.json` and race. `admitting` covers the window from the
        // decision to the placement, and `has_record` covers it from there
        // on — between them a job is never invisible to a second accept.
        if self.has_record(&spec.job_id)
            || sched.admitting.iter().any(|(id, _)| *id == spec.job_id)
            || sched.running.contains_key(&spec.job_id)
            || sched.queued.iter().any(|q| q.spec.job_id == spec.job_id)
        {
            // Acknowledged, and not run twice. Whoever got here first
            // publishes the summary.
            return Ok(());
        }
        // A member of a committed gang redeems the reservation this node
        // already granted (C4 P1). The capacity question was settled at
        // offer time — re-asking it here would let ordinary work that
        // arrived in between refuse a member the mesh had already promised,
        // which is precisely the double-booking the lease exists to stop.
        //
        // The lease is discharged *before* the slot check, because the
        // member it was standing in for is the job now taking that slot.
        let redeemed = gang_of(&spec.env)
            .is_some_and(|gang| sched.leases.discharge(&gang, mesh_proto::now_micros()));
        let busy = || {
            // Every admission refusal goes through here, which is what
            // makes it the one place to count them (ENG-21). A node that
            // refuses steadily is a node that is too small, and until this
            // existed the only trace of it was on the *submitter's* side.
            self.meters.refused.fetch_add(1, Ordering::Relaxed);
            CoreError::Busy {
                running: u32::try_from(sched.running.len()).unwrap_or(u32::MAX),
                queued: u32::try_from(sched.queued.len() + sched.admitting.len())
                    .unwrap_or(u32::MAX),
            }
        };
        if !redeemed
            && sched.running.len() + sched.leases.slots() >= slots
            && sched.queued.len() + sched.admitting.len() >= queue
        {
            return Err(busy());
        }
        // Resource admission (M9 §5.3), at the one choke point that already
        // exists. A declared task is admitted only if this node still has
        // the hardware to promise it; otherwise `Busy` — the refusal the
        // origin already spills on (MASTERPLAN §10.2) — hands it to the
        // next-best candidate rather than parking it behind work that will
        // not finish soon. That spill is precisely what makes a 32-core
        // node take twice the tasks of a 16-core one, with nobody deciding.
        let reqs = Requirements::from_env(&spec.env);
        if !redeemed && !self.admits(&sched, &reqs) {
            return Err(busy());
        }
        // Device admission (C5 P3), at the same choke point and for the
        // same reason: a 2-GPU job refused by the 1-GPU node spills to the
        // 4-GPU one rather than queueing behind work that will not free a
        // card any sooner. A redeemed gang member skips it because its
        // devices were reserved at offer time and are about to be
        // transferred onto it.
        if !redeemed
            && let Some(req) = mesh_proto::DeviceRequest::from_env(&spec.env)
            && !self.admits_devices(&req)
        {
            return Err(busy());
        }
        // Admitted. From here the decision is made, so the scheduler lock
        // is given back and the journalling — two fsynced writes and an
        // fsynced op commit — happens without it (**H-2**, ENG-20).
        //
        // The reservation is what makes that safe: until this job is in
        // `queued` or `running`, it sits in `admitting`, where every
        // admission test counts it exactly as a queued job. The guard
        // releases it on every path out, including the `?` on the writes
        // below, so a failed journal cannot leave capacity spoken for.
        let reservation = Reservation::take(self, &mut sched, spec.job_id, reqs);
        drop(sched);

        // Journal the acceptance before anything can observe it (§10.3:
        // executors journal accepted work; the journal is the idempotency
        // record a re-dispatch checks against).
        std::fs::create_dir_all(dir.join("work"))?;
        // Durably (ENG-16): this file *is* the acceptance. A job whose spec
        // did not survive the crash is one the orphan sweep cannot charge,
        // cannot summarise and cannot settle.
        crate::durable::write(
            &dir.join("spec.json"),
            &serde_json::to_vec_pretty(&spec)
                .map_err(|e| CoreError::Job(format!("encode spec: {e}")))?,
        )?;
        self.mark_spooled(&spec.job_id);
        let status = JobStatus {
            state: JobState::Queued,
            exit_code: None,
            started_at_micros: None,
            finished_at_micros: None,
            charge: None,
        };
        self.write_status(&spec.job_id, &status)?;
        self.publish(&spec, &status);

        // Journalled; take the lock back to place it. Nothing between the
        // two critical sections could have given this job's capacity away,
        // because the reservation held it.
        let mut sched = self.sched.lock();
        reservation.commit(&mut sched);
        self.meters.accepted.fetch_add(1, Ordering::Relaxed);

        // Memory admission (MASTERPLAN §11): a free slot alone is not
        // enough — under memory pressure new work waits in the queue and
        // the promotion ticker starts it once pressure clears.
        // A redeemed gang member starts now, not when the queue lets it.
        // The whole guarantee is *simultaneously*: a member parked behind
        // ordinary work is a gang whose barrier never opens, and every
        // other member is holding a core while it waits.
        // …and the device the job asked for has to be free *now*, not
        // merely present on this node. A job whose card is busy queues; the
        // promotion path reconsiders it the moment one is released.
        if redeemed
            || (sched.running.len() < slots
                && self.mem_gate_open()
                && self.devices_free_now(&sched, &spec))
        {
            self.start_locked(&mut sched, spec);
        } else {
            let seq = sched.next_seq;
            sched.next_seq += 1;
            sched.queued.push(QueuedJob { spec, seq });
        }
        drop(sched);
        Ok(())
    }

    /// Record a job whose result came from the action cache (C7 P2).
    ///
    /// **The hit is written as a real job, not as a special case.** Same
    /// directory, same `spec.json`, same `status.json`, same published
    /// summary — so `meshnode jobs logs`, `jobs show`, the history index,
    /// accounting and Director all work on it without knowing it was a hit.
    /// Every place that would otherwise have needed an "unless it was
    /// cached" branch is a place that would eventually get one wrong.
    ///
    /// What it does *not* do is take a slot, sample CPU, or run anything:
    /// there is no process, which is the entire point.
    ///
    /// The visible mark is [`crate::history::JobCacheMarker`], written by
    /// the caller beside this — *a hit is visible or it is untrustworthy*.
    pub fn record_cached(
        self: &Arc<Self>,
        spec: &JobSpec,
        exit_code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
        outputs: &[(String, Vec<u8>)],
    ) -> Result<JobStatus> {
        let dir = self.job_dir(&spec.job_id);
        // Declared outputs land where the process would have produced them,
        // which is not one directory for every payload class — a service
        // runs in its bundle's checkout under its `working_dir`, everything
        // else in `work/`. Writing them anywhere else would make a hit and
        // a run leave different directories behind, and anything reading
        // the job afterwards would find one of them empty.
        let work = match &spec.payload {
            JobPayload::Service(exec) if exec.working_dir.is_empty() => dir.join("checkout"),
            JobPayload::Service(exec) => dir.join("checkout").join(&exec.working_dir),
            JobPayload::Command(_) | JobPayload::Script(_) => dir.join("work"),
        };
        std::fs::create_dir_all(&work)?;
        crate::durable::write(
            &dir.join("spec.json"),
            &serde_json::to_vec_pretty(spec)
                .map_err(|e| CoreError::Job(format!("encode spec: {e}")))?,
        )?;
        self.mark_spooled(&spec.job_id);
        std::fs::write(dir.join("stdout.log"), stdout)?;
        std::fs::write(dir.join("stderr.log"), stderr)?;
        for (path, bytes) in outputs {
            // A declared output glob is relative to the working directory
            // and may name a subdirectory. Anything that escapes it is
            // refused rather than sanitised: an entry that could write
            // outside the job directory would make a cache hit a remote
            // write primitive, and "we cleaned up the path" is not a
            // sentence anyone should have to trust.
            let rel = std::path::Path::new(path);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(CoreError::Job(format!(
                    "cached output path escapes the job directory: {path}"
                )));
            }
            let dest = work.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(dest, bytes)?;
        }
        let now = mesh_proto::now_micros();
        let status = JobStatus {
            state: if exit_code == Some(0) {
                JobState::Succeeded
            } else {
                JobState::Failed
            },
            exit_code,
            started_at_micros: Some(now),
            finished_at_micros: Some(now),
            // No charge. The mesh spent no CPU on this and billing a
            // principal for work nobody did would make the cache a tax on
            // using it, which is the opposite of the incentive C7 exists to
            // create.
            charge: None,
        };
        self.write_status(&spec.job_id, &status)?;
        self.publish(spec, &status);
        Ok(status)
    }

    // -- devices (C5) --------------------------------------------------

    /// Replace this node's advertised device inventory (C5 P2).
    ///
    /// Called on boot, on SIGHUP reload and whenever the periodic re-probe
    /// sees the hardware change — a driver upgrade, a card pulled, an ECC
    /// error that took one out of service. Running work keeps the devices
    /// it holds: an inventory refresh describes the machine, it does not
    /// evict anybody, and a card that vanished from the inventory is
    /// already not going to be allocated again.
    pub fn set_devices(self: &Arc<Self>, devices: Vec<mesh_proto::Device>) {
        *self.devices.lock() = devices;
        self.try_promote();
    }

    /// This node's advertised devices.
    pub fn devices(&self) -> Vec<mesh_proto::Device> {
        self.devices.lock().clone()
    }

    // -- the network: ports, volumes, and the address a job advertises ----

    /// Install `[network]`, the advertised host and the host jobs bind on.
    /// Boot and every SIGHUP.
    pub fn set_network(
        self: &Arc<Self>,
        network: crate::config::NetworkConfig,
        advertise: String,
        bind_host: String,
    ) {
        *self.network.lock() = network;
        *self.advertise.lock() = advertise;
        *self.bind_host.lock() = bind_host;
    }

    /// The address a job is told to advertise itself on (N3).
    pub fn advertised_host(&self) -> String {
        self.advertise.lock().clone()
    }

    /// The dynamic range this node will allocate from, or `None` when it
    /// advertises none — which disqualifies it from port work entirely
    /// rather than falling back to a default (N2 P1).
    fn port_range(&self) -> Option<mesh_proto::PortRange> {
        self.network.lock().dynamic.clone()
    }

    /// The volumes this node lends (N5 P1).
    pub fn volumes(&self) -> Vec<mesh_proto::Volume> {
        self.network.lock().volumes.clone()
    }

    /// This node's port ledger, for `meshnode ports` (N2 P1).
    ///
    /// Node-local by construction: a dynamic port is chosen here, at spawn,
    /// and there is no mesh-wide answer to give.
    pub fn port_ledger(&self, node: String) -> mesh_proto::PortLedgerView {
        let network = self.network.lock().clone();
        let held = {
            let sched = self.sched.lock();
            sched.leases.port_ledger()
        };
        // `0 free` and `advertises no range` mean opposite things, and the
        // view keeps them apart: `range: None` is the second.
        let free = match &network.dynamic {
            None => 0,
            Some(r) => {
                let total = r.count();
                let taken = held.len() as u32 + network.reserved.len() as u32;
                total.saturating_sub(taken)
            }
        };
        mesh_proto::PortLedgerView {
            node,
            range: network.dynamic,
            reserved: network.reserved,
            held,
            free,
        }
    }

    /// The ports a job currently holds (N2 P2).
    ///
    /// Read by the health prober, which needs the *allocated* number rather
    /// than the declared one: a check that names its port by label follows
    /// whatever this node gave the job.
    pub fn ports_held_by(&self, job: JobId) -> Vec<mesh_proto::PortAllocation> {
        let sched = self.sched.lock();
        sched.leases.ports_of(&crate::gangs::Holder::Job(job))
    }

    /// The volumes this node lends, with who is holding each (N5 P1).
    pub fn volume_rows(&self, node: &str) -> Vec<mesh_proto::volumes::VolumeRow> {
        let declared = self.volumes();
        let sched = self.sched.lock();
        declared
            .into_iter()
            .map(|v| {
                let (holders, write_held) = sched.leases.volume_holders(&v.name);
                mesh_proto::volumes::VolumeRow {
                    // A writer excludes everything; readers coexist. So
                    // "could a writer take it right now" is *nobody holds
                    // it at all*, not merely *no writer holds it*.
                    writable_now: !v.read_only && holders.is_empty(),
                    read_only: v.read_only,
                    name: v.name,
                    node: node.to_string(),
                    path: v.path,
                    source: v.source,
                    holders,
                    write_held,
                }
            })
            .collect()
    }

    // -- containers and runtimes ------------------------------------------

    /// Install (or withdraw) the container plugin. Boot and every SIGHUP.
    pub fn set_container_plugin(self: &Arc<Self>, plugin: Option<String>) {
        *self.container_plugin.lock() = plugin.filter(|p| !p.trim().is_empty());
    }

    /// The container plugin, if this node has one configured.
    pub fn container_plugin(&self) -> Option<String> {
        self.container_plugin.lock().clone()
    }

    /// Record what the runtime probes found (T1). Boot, every refresh, and
    /// every SIGHUP.
    ///
    /// **The engine is stored beside the reports rather than derived from
    /// them later**, so the table an operator reads and the engine a job is
    /// actually given can never disagree — the failure where `doctor` says
    /// podman is ready while jobs fail with "no container engine".
    pub fn set_runtimes(
        self: &Arc<Self>,
        reports: Vec<mesh_proto::runtimes::RuntimeReport>,
        engine: Option<crate::engines::Engine>,
    ) {
        *self.container_engine.lock() = engine;
        *self.runtime_reports.lock() = reports;
    }

    /// The container engine this node detected, if any.
    pub fn container_engine(&self) -> Option<crate::engines::Engine> {
        self.container_engine.lock().clone()
    }

    /// What the runtime probes last answered.
    pub fn runtime_reports(&self) -> Vec<mesh_proto::runtimes::RuntimeReport> {
        self.runtime_reports.lock().clone()
    }

    /// The runtimes this node can serve, for its `cap.rt` tag (T3).
    pub fn ready_runtimes(&self) -> Vec<mesh_proto::Runtime> {
        mesh_proto::runtimes::ready_of(&self.runtime_reports.lock())
    }

    // -- the service registry (N3 P1) -------------------------------------

    /// Every registration this node currently stands behind.
    ///
    /// Only jobs that are actually *running*: a queued one has nothing
    /// listening yet, and handing out its address would be the same bug as
    /// resolving a starting instance.
    pub fn registrations(&self) -> Vec<mesh_proto::Registration> {
        let sched = self.sched.lock();
        let held = self.registrations.lock();
        let mut out: Vec<mesh_proto::Registration> = held
            .iter()
            .filter(|(id, _)| sched.running.contains_key(id))
            .flat_map(|(_, rs)| rs.iter().cloned())
            .collect();
        out.sort_by(|a, b| {
            (&a.service, a.instance.0, &a.label).cmp(&(&b.service, b.instance.0, &b.label))
        });
        out
    }

    /// Withdraw a job's registrations without stopping it (N3 P2).
    ///
    /// The first step of the drain ordering: deregister, wait, then signal.
    /// That window is the whole point — it is what lets a load balancer
    /// notice before the process dies.
    pub fn deregister(&self, job: &JobId) -> bool {
        self.registrations.lock().remove(job).is_some()
    }

    /// Record what a starting job registers itself as (N3 P1).
    ///
    /// **Only when something asked.** `MESHWARE_TASK_SERVICE` is absent by
    /// default, so an ad-hoc `meshnode run` registers nothing — a mesh
    /// where every command appeared in DNS would be a mesh where DNS means
    /// nothing.
    fn note_registration(&self, spec: &JobSpec) {
        let get = |key: &str| {
            spec.env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let Some(service) = get(mesh_proto::REQ_SERVICE) else {
            return;
        };
        if !crate::registry::is_valid_service_name(&service) {
            warn!(
                job = %spec.job_id.short(),
                "not registered: {service:?} is not a usable service name"
            );
            return;
        }
        let tags: Vec<String> = get(mesh_proto::REQ_SERVICE_TAGS)
            .map(|t| {
                t.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let wanted_label = get(mesh_proto::REQ_SERVICE_PORT);
        let host = self.advertised_host();
        let deployment = get(mesh_proto::DEPLOY_ENV).unwrap_or_default();
        // The allocation, read back out of the environment this node just
        // wrote — so the registration cannot disagree with what the job was
        // actually told, which it could if it were recomputed from the
        // request.
        let requests = mesh_proto::PortRequests::from_env(&spec.env);
        let mut out = Vec::new();
        for (key, value) in &spec.env {
            let Some(suffix) = key.strip_prefix(mesh_proto::PORT_ENV_PREFIX) else {
                continue;
            };
            let Ok(port) = value.parse::<u16>() else {
                continue;
            };
            let label = requests
                .0
                .iter()
                .find(|r| r.key_suffix() == suffix)
                .map(|r| r.label.clone())
                .unwrap_or_else(|| suffix.to_ascii_lowercase());
            // A job with several ports publishes the one it named, or all
            // of them when it named none — the second is right because a
            // single-port job is the common case and naming its only label
            // would be ceremony.
            if wanted_label.as_deref().is_some_and(|w| w != label) {
                continue;
            }
            out.push(mesh_proto::Registration {
                service: service.clone(),
                instance: spec.job_id,
                label,
                address: host.clone(),
                port,
                tags: tags.clone(),
                // Health is overlaid by the publisher from the same
                // conclusion the health record carries, so the two can
                // never disagree about whether an instance is passing.
                health: mesh_proto::Health::Unknown,
                since_micros: mesh_proto::now_micros(),
                deployment: deployment.clone(),
            });
        }
        if out.is_empty() {
            warn!(
                job = %spec.job_id.short(),
                "asked to register as {service:?} but has no allocated port — \
                 add a port request (`--port http:9095`, or a [deployment.network] block)"
            );
            return;
        }
        self.registrations.lock().insert(spec.job_id, out);
    }

    /// Record the freshest device telemetry this node took of itself.
    pub fn set_device_pressure(&self, pressure: Vec<mesh_proto::DevicePressure>) {
        *self.device_pressure.lock() = pressure;
    }

    pub fn device_pressure(&self) -> Vec<mesh_proto::DevicePressure> {
        self.device_pressure.lock().clone()
    }

    /// Every device hold on this node right now, as `nodes --devices` and
    /// `top` show them: device → the job or gang holding it. Keyed by the
    /// whole [`mesh_proto::DeviceKey`], because two vendors' device 0 are
    /// two devices.
    ///
    /// Only ever answered about *this* node. A hold is deliberately not
    /// replicated (§8.2, and devices inherit the rule), so there is no
    /// honest way to read a remote node's ledger — which is exactly what
    /// C4's `top` column says about leases and is said again here rather
    /// than implying a precision the design cannot deliver.
    pub fn device_holders(&self) -> BTreeMap<mesh_proto::DeviceKey, String> {
        let mut sched = self.sched.lock();
        sched.leases.sweep_devices(mesh_proto::now_micros());
        sched
            .leases
            .device_holds()
            .iter()
            .map(|h| {
                let who = match h.holder {
                    crate::gangs::Holder::Gang(g) => format!("gang {}", g.short()),
                    crate::gangs::Holder::Job(j) => format!("job {}", j.short()),
                };
                (h.key.clone(), who)
            })
            .collect()
    }

    /// Could this node *ever* satisfy `req`? The permanent question, and
    /// the one admission asks (C5 P3).
    ///
    /// Deliberately blind to what is held right now, mirroring the split
    /// [`mesh_proto::Requirements`] already makes between `fits_capacity`
    /// (permanent, and a refusal) and live pressure (transient, and a
    /// wait). A 2-GPU job on a 1-GPU node is refused so the origin spills
    /// it to a node that can run it; a 1-GPU job on a *busy* 1-GPU node is
    /// **queued**, because the card will come back and moving the job would
    /// only find the same contention somewhere else.
    fn admits_devices(&self, req: &mesh_proto::DeviceRequest) -> bool {
        if req.is_unstated() {
            return true;
        }
        let inventory = self.devices();
        let matching = inventory.iter().filter(|d| d.matches(req)).count();
        matching >= req.effective_count() as usize
    }

    /// Are the devices this job needs free *right now*? The transient
    /// question, asked at the moment it would start rather than the moment
    /// it arrives.
    ///
    /// Caller holds the sched lock, so this and the allocation that follows
    /// it are atomic with respect to every other admission — which is the
    /// whole reason the device ledger lives in the lease table rather than
    /// beside it.
    fn devices_free_now(&self, sched: &Sched, spec: &JobSpec) -> bool {
        let Some(req) = mesh_proto::DeviceRequest::from_env(&spec.env) else {
            return true;
        };
        let inventory = self.devices();
        sched
            .leases
            .free_devices(&inventory, &req, mesh_proto::now_micros(), |_| None)
            .len()
            >= req.effective_count() as usize
    }

    /// Give a job the devices it asked for, under the sched lock.
    ///
    /// Two paths in, and the difference matters. A **gang member** takes a
    /// transfer of what its gang already reserved: re-running the choice
    /// would let ordinary work that arrived in between take a device the
    /// mesh had already promised. Everything else allocates fresh.
    fn allocate_devices(&self, sched: &mut Sched, spec: &JobSpec) {
        let Some(req) = mesh_proto::DeviceRequest::from_env(&spec.env) else {
            return;
        };
        let want = req.effective_count() as usize;
        if let Some(gang) = gang_of(&spec.env) {
            let moved = sched.leases.claim_from_gang(&gang, spec.job_id, want);
            if moved.len() == want {
                return;
            }
            // The gang lease is gone — expired, or this member arrived
            // without one. Fall through and allocate: refusing here would
            // fail a job the mesh has already started elsewhere.
        }
        let telemetry = self.device_pressure();
        let inventory = self.devices();
        let chosen: Vec<mesh_proto::Device> = sched
            .leases
            .free_devices(&inventory, &req, mesh_proto::now_micros(), |d| {
                mesh_proto::reading_for(&telemetry, d, &inventory)
                    .and_then(mesh_proto::DevicePressure::busy_fraction)
            })
            .into_iter()
            .take(want)
            .cloned()
            .collect();
        let refs: Vec<&mesh_proto::Device> = chosen.iter().collect();
        sched.leases.hold_devices(
            crate::gangs::Holder::Job(spec.job_id),
            &refs,
            req.share,
            None,
        );
    }

    /// Give this job the host ports it asked for, and tell it which (N2 P1).
    ///
    /// Written into the spec's environment as `MESHWARE_PORT_<LABEL>`,
    /// which is what a job actually reads, and what the registration and
    /// the health prober both read back afterwards so that none of the
    /// three can disagree about which port this job got.
    ///
    /// A request this node cannot satisfy leaves the environment untouched.
    /// The job then starts without the variable and fails on its own bind,
    /// which is worse than refusing — but refusing here is not available:
    /// the slot is already committed. Admission is where a node that cannot
    /// serve a port request declines it, and the placement filter is where
    /// a node that advertises no range is never offered the work at all.
    fn allocate_ports(&self, sched: &mut Sched, spec: &mut JobSpec) {
        let requests = mesh_proto::PortRequests::from_env(&spec.env);
        if requests.is_empty() {
            return;
        }
        let Some(range) = self.port_range() else {
            warn!(
                job = %spec.job_id.short(),
                "asked for a port but this node advertises no dynamic range"
            );
            return;
        };
        let reserved = self.network.lock().reserved.clone();
        // **Checked against the host, not only against the ledger.**
        //
        // The table knows what meshware handed out; it does not know what
        // else on the machine is listening. A container that outlived its
        // job, a database somebody started by hand, anything at all — the
        // ledger calls that port free and the job crash-loops on `Address
        // already in use` while `meshnode ports` says nothing holds it.
        let allocated = sched.leases.hold_ports_checked(
            crate::gangs::Holder::Job(spec.job_id),
            &requests.0,
            &range,
            &reserved,
            None,
            &crate::gangs::host_can_bind,
        );
        let Some(allocated) = allocated else {
            warn!(
                job = %spec.job_id.short(),
                "could not allocate {} port(s) from {}-{}",
                requests.len(),
                range.min,
                range.max
            );
            return;
        };
        // The port, and where to bind it. With the mesh proxy gone an
        // instance is reached at its node's address, so a job that binds
        // loopback is reachable from nowhere else; `MESHWARE_IP_<label>` is
        // the address that works, as `NOMAD_IP_<label>` is for Nomad.
        let bind_ip = self
            .bind_host
            .lock()
            .parse::<std::net::IpAddr>()
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        for alloc in &allocated {
            let suffix = mesh_proto::ports::label_key_suffix(&alloc.label);
            for (prefix, value) in [
                (mesh_proto::PORT_ENV_PREFIX, alloc.host.to_string()),
                (mesh_proto::IP_ENV_PREFIX, bind_ip.to_string()),
                (
                    mesh_proto::ADDR_ENV_PREFIX,
                    std::net::SocketAddr::new(bind_ip, alloc.host).to_string(),
                ),
            ] {
                let key = format!("{prefix}{suffix}");
                spec.env.retain(|(k, _)| k != &key);
                spec.env.push((key, value));
            }
        }
    }

    /// Claim the volumes this job asked for, and tell it where they are
    /// (N5 P1).
    ///
    /// The claim is an **exclusive lease** — one writer excludes
    /// everything, readers coexist — and it is taken here, under the sched
    /// lock, before the job is observable. `MESHWARE_VOLUME_<NAME>` carries
    /// the host path, which is what a process runtime uses directly and
    /// what a container plugin turns into a mount.
    fn allocate_volumes(&self, sched: &mut Sched, spec: &mut JobSpec) {
        let requests = mesh_proto::VolumeRequests::from_env(&spec.env);
        if requests.0.is_empty() {
            return;
        }
        let available = self.volumes();
        let granted = sched.leases.hold_volumes(
            crate::gangs::Holder::Job(spec.job_id),
            &requests.0,
            &available,
            None,
        );
        let Some(granted) = granted else {
            warn!(
                job = %spec.job_id.short(),
                "could not claim volume(s) {} — another holder has one, or this \
                 node does not lend it",
                requests.names().join(", ")
            );
            return;
        };
        for vol in &granted {
            let key = format!(
                "{}{}",
                mesh_proto::volumes::VOLUME_ENV_PREFIX,
                mesh_proto::volumes::key_suffix(&vol.name)
            );
            spec.env.retain(|(k, _)| k != &key);
            spec.env.push((key, vol.path.clone()));
        }
    }

    /// The devices a job holds, in allocation order.
    fn devices_for(&self, job: &JobId) -> Vec<mesh_proto::Device> {
        let keys = {
            let sched = self.sched.lock();
            sched.leases.devices_of(&crate::gangs::Holder::Job(*job))
        };
        let inventory = self.devices();
        // Matched on the whole identity. `find` on the ordinal alone
        // returns whichever vendor happens to sort first, so a job holding
        // the discrete card was handed the integrated GPU beside it — and
        // then masked for the wrong vendor, which is how it presented: an
        // empty `CUDA_VISIBLE_DEVICES` on a job that had been given a
        // perfectly good NVIDIA card.
        keys.into_iter()
            .filter_map(|k| inventory.iter().find(|d| d.key() == k).cloned())
            .collect()
    }

    // -- gang leases (C4 P1) ------------------------------------------
    //
    // Every one of these takes the *scheduler's* lock, and that is the
    // point: a lease is only worth something if the capacity check and the
    // reservation are atomic with respect to ordinary admission. A gang
    // that raced a `SubmitJob` and lost half its cores has reserved
    // nothing, and would then start N-1 of N members — the exact failure
    // all-or-nothing admission exists to prevent.

    /// Hold `reqs` × `members` for `gang` until the TTL runs out (§8.2).
    ///
    /// The holder caps the TTL at [`mesh_proto::MAX_LEASE_TTL_SECS`]
    /// whatever the offer asked for: a TTL is a promise *this* node keeps,
    /// and letting a remote origin name an arbitrary one would hand every
    /// node in the mesh a way to park somebody else's cores for an hour.
    ///
    /// `devices` is what **one** member needs in the way of accelerators,
    /// and it does not travel on the offer (C5 §9.1): `Requirements` is
    /// embedded in `Request::GangOffer` and a struct in a wire message may
    /// never grow a field. It is read instead from the gang's **replicated
    /// plan**, which the origin authors before the first offer goes out —
    /// so the figure this node reserves against is the same one the member
    /// job will declare when it arrives, from a record both ends can see.
    /// A plan that has not gossiped here yet means no device reservation,
    /// and the member's own admission then does the check.
    pub fn offer_lease(
        self: &Arc<Self>,
        gang: mesh_proto::GangId,
        ticket: mesh_proto::GangTicket,
        reqs: Requirements,
        members: u32,
        ttl_secs: u64,
        devices: Option<mesh_proto::DeviceRequest>,
    ) -> mesh_proto::GangOfferReply {
        let now = mesh_proto::now_micros();
        let ttl = ttl_secs.clamp(1, mesh_proto::MAX_LEASE_TTL_SECS);
        let expires = now.saturating_add(ttl.saturating_mul(1_000_000));
        // A gang reservation is a promise to start every member at once, so
        // it is bounded by what this node will start *now* — a quiet node
        // that promised on its full slot count would hold the whole gang
        // waiting for a window it cannot fill (C4 P1).
        let slots = self.effective_slots() as usize;
        let mut sched = self.sched.lock();
        sched.leases.sweep(now);

        // Split the borrow: `offer` needs `&mut leases` while `fits` reads
        // the running and queued work beside it, so the rest of the
        // scheduler's state is snapshotted first. It is a handful of
        // integers, and it is taken under the same lock, so the check and
        // the insert stay atomic.
        let cap = self.capacity();
        let fallback_slots = self.slots.load(Ordering::Relaxed);
        let running_and_queued = {
            let running = sched.running.values().map(|job| job.reqs);
            let queued = sched
                .queued
                .iter()
                .map(|q| Requirements::from_env(&q.spec.env));
            running
                .chain(queued)
                .fold(Requirements::default(), |acc, r| Requirements {
                    cpus: acc.cpus.saturating_add(r.cpus),
                    mem_mb: acc.mem_mb.saturating_add(r.mem_mb),
                    disk_mb: acc.disk_mb.saturating_add(r.disk_mb),
                })
        };
        let busy_slots = sched.running.len();
        let leased: Vec<(mesh_proto::GangId, Requirements, u32)> = sched
            .leases
            .iter()
            .map(|l| (l.gang, l.total(), l.members))
            .collect();

        let outcome = sched.leases.offer(
            gang,
            ticket,
            reqs,
            members,
            expires,
            now,
            |want, members, ignoring| {
                let held = leased
                    .iter()
                    .filter(|(g, _, _)| !ignoring.contains(g))
                    .fold(running_and_queued, |acc, (_, r, _)| Requirements {
                        cpus: acc.cpus.saturating_add(r.cpus),
                        mem_mb: acc.mem_mb.saturating_add(r.mem_mb),
                        disk_mb: acc.disk_mb.saturating_add(r.disk_mb),
                    });
                let want_total = Requirements {
                    cpus: want.cpus.saturating_mul(members),
                    mem_mb: want.mem_mb.saturating_mul(u64::from(members)),
                    disk_mb: want.disk_mb.saturating_mul(u64::from(members)),
                };
                // A slot as well as the hardware. The two gates have to
                // agree or the offer answers a different question from the
                // commit: a node can have the cores for a reservation and
                // no slot to start the member in, and that member would
                // then queue behind ordinary work while the rest of the
                // gang holds theirs idle.
                let leased_slots: usize = leased
                    .iter()
                    .filter(|(g, _, _)| !ignoring.contains(g))
                    .map(|(_, _, m)| *m as usize)
                    .fold(0usize, usize::saturating_add);
                if busy_slots + leased_slots + members as usize > slots {
                    return false;
                }
                fits_capacity(&cap, fallback_slots, &held, &want_total)
            },
        );

        // Devices, if the plan says this gang needs any. Held under the
        // same lock as the cores, because a gang that reserved its cores
        // and lost its GPUs to a racing submission has reserved nothing
        // that matters.
        if let crate::gangs::OfferOutcome::Accepted { .. } = &outcome
            && let Some(req) = devices.filter(|r| !r.is_unstated())
        {
            let want = req.effective_count() as usize * members.max(1) as usize;
            sched.leases.sweep_devices(now);
            let inventory = self.devices();
            let chosen: Vec<mesh_proto::Device> = sched
                .leases
                .free_devices(&inventory, &req, now, |_| None)
                .into_iter()
                .take(want)
                .cloned()
                .collect();
            if chosen.len() < want {
                // Not enough cards for every member this node was asked to
                // hold. Give the whole reservation back and refuse: half a
                // gang member's devices is the failure all-or-nothing
                // admission exists to prevent, one level down.
                sched.leases.release(&gang);
                drop(sched);
                return mesh_proto::GangOfferReply::Refused {
                    reason: mesh_proto::GangRefusal::NoRoom,
                };
            }
            let refs: Vec<&mesh_proto::Device> = chosen.iter().collect();
            sched.leases.hold_devices(
                crate::gangs::Holder::Gang(gang),
                &refs,
                req.share,
                Some(expires),
            );
        }

        drop(sched);
        match outcome {
            crate::gangs::OfferOutcome::Accepted { lease, wounded } => {
                for gang in &wounded {
                    // Wound-wait, and the audit trail for it. The wounded
                    // gang's origin finds out when its own commit is
                    // refused, which is the only moment the information
                    // changes what it does.
                    info!(
                        wounded = %gang.short(),
                        by = %ticket.gang.short(),
                        "yielded a junior gang's lease to a senior offer"
                    );
                }
                mesh_proto::GangOfferReply::Accepted { lease }
            }
            crate::gangs::OfferOutcome::NoRoom => mesh_proto::GangOfferReply::Refused {
                reason: mesh_proto::GangRefusal::NoRoom,
            },
            crate::gangs::OfferOutcome::Senior { gang } => mesh_proto::GangOfferReply::Refused {
                reason: mesh_proto::GangRefusal::Senior { gang },
            },
        }
    }

    /// Turn an offered lease into a committed one. `None` means the lease
    /// is gone — expired, or wounded by a senior gang — and the origin has
    /// to hear that, because it is the difference between starting the gang
    /// and starting N-1 of it.
    pub fn commit_lease(&self, gang: &mesh_proto::GangId) -> Option<mesh_proto::LeaseId> {
        let now = mesh_proto::now_micros();
        let mut sched = self.sched.lock();
        let lease = sched.leases.commit(gang, now)?;
        // The devices keep the lease's clock, so a committed gang's cards
        // survive as long as its cores do while the member job travels.
        let until = now.saturating_add(crate::gangs::COMMIT_GRACE_SECS * 1_000_000);
        sched.leases.extend_device_holds(gang, until);
        Some(lease)
    }

    /// Give a gang's reservation back. Idempotent, so an adopting successor
    /// can release blindly before it re-drives.
    pub fn release_lease(self: &Arc<Self>, gang: &mesh_proto::GangId) -> bool {
        let released = self.sched.lock().leases.release(gang);
        if released {
            self.try_promote();
        }
        released
    }

    /// Every reservation this node is holding right now, expired ones
    /// already swept.
    pub fn leases(&self) -> Vec<mesh_proto::LeaseView> {
        let mut sched = self.sched.lock();
        sched.leases.sweep(mesh_proto::now_micros());
        sched.leases.views()
    }

    /// Memory admission gate (MASTERPLAN §11): don't start new work while
    /// `MemAvailable` is under the configured floor or memory PSI shows
    /// sustained pressure. Never blocks already-running jobs.
    fn mem_gate_open(&self) -> bool {
        let floor_mb = self.min_free_mem_mb.load(Ordering::Relaxed);
        if floor_mb == 0 {
            return true;
        }
        if let Some(avail_mb) = crate::capacity::mem_available_mb()
            && avail_mb < floor_mb
        {
            return false;
        }
        if let Some(some_avg10) = crate::capacity::psi_some_avg10("memory")
            && some_avg10 > 40.0
        {
            return false;
        }
        true
    }

    /// Promote queued work into free slots while capacity and the memory
    /// gate allow. Called when a slot frees and from the periodic ticker
    /// (pressure clears without any slot changing hands).
    pub fn try_promote(self: &Arc<Self>) {
        // Computed *before* the sched lock: this reads replicated state,
        // and taking the state lock while holding the scheduler's is the
        // one lock order this file must never establish.
        let shares = self.shares();
        // Read once, outside the lock and outside the loop: the inventory
        // is a clone of a small Vec and the promotion loop can run once per
        // free slot.
        let inventory = self.devices();
        // Read before the sched lock, like `shares` above: it takes the
        // quiet lock, and nesting that inside the scheduler's would
        // establish a second lock order this file does not need.
        let slots = self.effective_slots() as usize;
        let mut sched = self.sched.lock();
        while sched.running.len() < slots && self.mem_gate_open() {
            let Some(next) = sched.pop_best(&shares, &inventory) else {
                break;
            };
            self.start_locked(&mut sched, next);
        }
    }

    /// Can this node promise `reqs` on top of everything it has already
    /// promised (M9 §5.3)? Bin-packing against advertised hardware rather
    /// than against a slot count.
    ///
    /// Unstated requirements always fit — that is every pre-M9 job, and
    /// slots still bound those. An unadvertised dimension is not a refusal
    /// either: a node that could not probe its memory is trusted here, and
    /// the memory admission gate (§11) still guards it at start time.
    fn admits(&self, sched: &Sched, reqs: &Requirements) -> bool {
        self.admits_excluding(sched, reqs, None, &[])
    }

    /// [`Self::admits`] against a reservation total with some leases lifted
    /// out of it (C4 P1).
    ///
    /// The gang path needs both hatches: a gang's own lease must not count
    /// against the member job it was taken for, and wound-wait has to ask a
    /// hypothetical — *would there be room if these junior gangs gave
    /// theirs back* — before it yields anything.
    fn admits_excluding(
        &self,
        sched: &Sched,
        reqs: &Requirements,
        own_gang: Option<mesh_proto::GangId>,
        wounding: &[mesh_proto::GangId],
    ) -> bool {
        if reqs.is_unstated() {
            return true;
        }
        let cap = self.capacity();
        let reserved = sched.reserved_excluding(own_gang, wounding);
        if reqs.cpus > 0 {
            // No advertised core count (an unprobed node) falls back to the
            // slot count, which derives from cores anyway (§11).
            let budget = if cap.cores > 0 {
                cap.cores
            } else {
                self.slots.load(Ordering::Relaxed)
            };
            if reserved.cpus.saturating_add(reqs.cpus) > budget {
                return false;
            }
        }
        if reqs.mem_mb > 0
            && cap.mem_mb > 0
            && reserved.mem_mb.saturating_add(reqs.mem_mb) > cap.mem_mb
        {
            return false;
        }
        if reqs.disk_mb > 0
            && cap.disk_mb > 0
            && reserved.disk_mb.saturating_add(reqs.disk_mb) > cap.disk_mb
        {
            return false;
        }
        true
    }

    /// Pull every still-queued job out of the pending queue and forget its
    /// local journal — `meshnode drain --move` re-dispatches them elsewhere
    /// (MASTERPLAN §12). The caller re-accepts any spec it cannot place.
    pub fn take_queued(&self) -> Vec<JobSpec> {
        let mut sched = self.sched.lock();
        let mut queued = std::mem::take(&mut sched.queued);
        queued.sort_by_key(|q| (std::cmp::Reverse(q.spec.priority), q.seq));
        drop(sched);
        queued
            .into_iter()
            .map(|q| {
                // The job never ran here: drop the local journal so the new
                // executor's summary (same job id) is the only record.
                let _ = std::fs::remove_dir_all(self.job_dir(&q.spec.job_id));
                self.unmark_spooled(&q.spec.job_id);
                q.spec
            })
            .collect()
    }

    /// Begin executing a job in a slot. Caller holds the sched lock.
    fn start_locked(self: &Arc<Self>, sched: &mut Sched, spec: JobSpec) {
        // Devices are assigned here, under the sched lock, *before* the
        // job is observable as running — so the ledger and the running set
        // can never disagree about who holds device 3.
        self.allocate_devices(sched, &spec);
        // Ports and volumes on the same rule and for the same reason (N2
        // P1, N5 P1). Both *write into the spec's environment*: an
        // allocation the job cannot read is an allocation it cannot use,
        // and the environment is the one carrier every runtime already
        // reads.
        let mut spec = spec;
        self.allocate_ports(sched, &mut spec);
        self.allocate_volumes(sched, &mut spec);
        // …and only now, with the allocation in hand, can this node say
        // what the job registers as (N3 P1). The port did not exist a
        // moment ago.
        self.note_registration(&spec);
        // Counted here rather than derived later (ENG-21): this is the
        // moment a job stops waiting and starts running, and the wait is
        // the number an operator sizes a queue by.
        self.meters.started.fetch_add(1, Ordering::Relaxed);
        self.meters.queue_wait.record(Duration::from_micros(
            mesh_proto::now_micros().saturating_sub(spec.created_at_micros),
        ));
        let (cancel_tx, cancel_rx) = watch::channel(false);
        sched.running.insert(
            spec.job_id,
            RunningJob {
                cancel: cancel_tx,
                principal: spec.submitter_user,
                preemptible: mesh_proto::is_preemptible(&spec.env),
                reqs: Requirements::from_env(&spec.env),
                pid: None,
            },
        );
        let this = self.clone();
        // **The span a job's whole life hangs off** (ENG-21). Everything
        // under `execute` — the sandbox, the cgroup, secret resolution,
        // the container teardown — logged its own line with no way to tell
        // which job it belonged to on a node running sixteen at once. The
        // job id is also the join across nodes: it is what the submitter's
        // plan replicated and what this node journaled.
        let span = tracing::info_span!(
            "job",
            job = %spec.job_id.short(),
            user = %spec.submitter_user.short(),
        );
        tokio::spawn(
            async move {
                let id = spec.job_id;
                let outcome = this.execute(&spec, cancel_rx).await;
                match outcome {
                    Ok(status) => {
                        info!(state = status.state.as_str(),
                            code = ?status.exit_code, "job finished");
                    }
                    Err(e) => warn!("job execution error: {e}"),
                }
                this.slot_freed(&id);
            }
            .instrument(span),
        );
    }

    /// A job left its slot: promote queued work (priority, then arrival).
    fn slot_freed(self: &Arc<Self>, id: &JobId) {
        // **The container goes before the port does (N6).**
        //
        // `podman run --rm` removes the container when *the client* exits
        // cleanly, and the client is not the container: `podman run` hands
        // the workload to conmon, which reparents it. So a job that is
        // cancelled, times out or is `SIGKILL`ed leaves the container
        // running — still holding its host port, after the ledger has
        // already recorded that port as free. The next instance is handed
        // the same number, the orphan still has it, and every replacement
        // crash-loops on `Address already in use` while `meshnode ports`
        // says nothing is holding it.
        //
        // Spawned rather than awaited because this is not an async fn and
        // the slot must be given back now; the stop is idempotent, so a
        // container that has already gone is a success.
        let runner = self
            .container_plugin()
            .map(Runner::Plugin)
            .or_else(|| self.container_engine().map(Runner::Engine));
        if let Some(runner) = runner {
            let job = id.short();
            tokio::spawn(async move {
                let outcome = match &runner {
                    Runner::Plugin(p) => crate::containers::stop(p, &job).await,
                    Runner::Engine(e) => crate::engines::stop(e, &job).await,
                };
                if let Err(e) = outcome {
                    warn!(job = %job, "could not remove this job's container: {e}");
                }
            });
        }
        {
            let mut sched = self.sched.lock();
            sched.running.remove(id);
            // The device goes back the instant the job is gone, however it
            // went — exit, cancel, timeout or `SIGKILL`. A release is a
            // table edit rather than a driver call, which is what makes
            // "a killed job's device is immediately reusable" true rather
            // than eventually true.
            sched
                .leases
                .release_devices(&crate::gangs::Holder::Job(*id));
            // Ports and volumes on the same rule: the lease is the job's,
            // and the job is over.
            sched.leases.release_ports(&crate::gangs::Holder::Job(*id));
            sched
                .leases
                .release_volumes(&crate::gangs::Holder::Job(*id));
        }
        // The registration goes with the port, and for the same reason: a
        // name that still resolves to a dead process is worse than a name
        // that resolves to nothing.
        self.registrations.lock().remove(id);
        self.try_promote();
    }

    /// Run one item of a parallel job (M9 §5.4).
    ///
    /// Deliberately *not* a job: no journal, no slot, no replicated summary.
    /// A chunk item is the unit below a job, and putting a record of each
    /// one in the op-log is the thing M9 §7 rules out. Concurrency is
    /// bounded by the worker's lane count instead, which is derived from
    /// the same slot figure jobs use.
    ///
    /// Everything else is the executor hygiene an ordinary job gets: a
    /// cleared environment with a minimal allowlist, late-bound secrets,
    /// and output redaction.
    ///
    /// `dir` is the chunk's scratch directory and `dir/work` must already
    /// exist — the caller creates it once per chunk rather than once per
    /// item, so a 4,000-item chunk does not make 4,000 blocking `mkdir`
    /// calls on a runtime thread.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_item(
        &self,
        command: &str,
        env: &[(String, String)],
        timeout: Option<Duration>,
        dir: &Path,
        grants: &crate::sandbox::Grants,
        // Whose work this is, for the secrets it may use (SEC-7).
        user: &mesh_proto::UserId,
    ) -> mesh_proto::ItemOutcome {
        // Secret refs are env-only: argv is world-readable via /proc.
        if !secrets::find_refs(command).is_empty() {
            return item_failure(
                "secret references are not allowed in the command line (visible in /proc)".into(),
                0,
            );
        }
        self.run_prepared(
            "bash",
            &["-c".to_string(), command.to_string()],
            &dir.join("work"),
            env,
            timeout,
            dir,
            grants,
            user,
        )
        .await
    }

    /// This node's inventory hostname: the name a rendered mesh inventory
    /// refers to it by, which is the member name and not the node id.
    ///
    /// Read from replicated state rather than from config so it is the same
    /// string the *origin* put in the document — a node whose config name
    /// and member record disagree would silently fail the `local`-if-this-
    /// is-me test and run the play over SSH to itself.
    /// Where this node is listening, for a task that talks back to the
    /// mesh it is running in.
    ///
    /// A task calling back into the mesh has to find its own daemon, and
    /// the job environment is cleared apart from a six-name allowlist.
    /// Without this it fell back to the pointer file under
    /// `$HOME/.meshware`, which is the executing node's daemon only by
    /// coincidence — and on a node started with `--data-dir`, the wrong
    /// daemon or none at all.
    ///
    /// A default, not an override: it goes in before the service, runtime
    /// and job env, so a step that sets it for its own reasons still wins.
    fn socket_env(&self) -> (String, String) {
        (
            crate::runtime::CLIENT_SOCKET_ENV.to_string(),
            crate::config::socket_path_for(&self.data_dir)
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// This node's own `meshnode` binary, for a job that needs to invoke it.
    ///
    /// See [`crate::runtime::NODE_BINARY_ENV`] for why this is not `PATH`.
    ///
    /// **Two paths, and the second one matters more than it looks.**
    /// `current_exe` normally gives a clean absolute path, which is what
    /// belongs in a log and in a job's environment. But it resolves through
    /// `/proc/self/exe`, and when the binary has been *replaced in place* —
    /// which is precisely what `meshnode upgrade --self` does to a daemon
    /// that keeps running — Linux returns the old path with `" (deleted)"`
    /// appended. Handing that to a job produces `No such file or directory`,
    /// and it does so on every node in the fleet at once, some time after
    /// an upgrade, with nothing to connect the failure to its cause.
    ///
    /// So: use the resolved path when it still exists, and otherwise fall
    /// back to `/proc/<pid>/exe`, which is a magic symlink the kernel keeps
    /// pointing at the *running image* whether or not that image still has
    /// a name on disk. Executing it is well-defined and gets the exact code
    /// this daemon is running — which for a benchmark is not a nicety but
    /// the entire basis of comparing one node's score with another's.
    ///
    /// `None` where neither is available. A caller must treat the variable
    /// as optional: on a platform without `/proc` there is no honest answer,
    /// and inventing `"meshnode"` would be worse than saying nothing.
    fn binary_env(&self) -> Option<(String, String)> {
        let value = crate::sandbox::self_exe()?;
        Some((
            crate::runtime::NODE_BINARY_ENV.to_string(),
            value.to_string_lossy().into_owned(),
        ))
    }

    /// The mesh's name for this node. See [`crate::runtime::NODE_NAME_ENV`]
    /// for why a job cannot just read the hostname.
    fn node_name_env(&self) -> (String, String) {
        (crate::runtime::NODE_NAME_ENV.to_string(), self.self_name())
    }

    /// Mint this job's credential: a keypair that *is* its submitter, for
    /// as long as the job runs.
    ///
    /// The other half of the promise above. A step that calls back into the
    /// mesh has to sign, and the only key on the executing node is that
    /// machine's own — which nobody enrolled, so every such request was
    /// denied everywhere but the founder's laptop. Handing the step the
    /// submitter's *actual* key is not an option and never will be: it is
    /// on their machine, not this one, and it stays there.
    ///
    /// So the node vouches for a process it started itself. A fresh keypair
    /// per job, the seed written into that job's own directory, and an
    /// in-memory note — on this node only — that this public key speaks for
    /// `spec.submitter_user`. That asserts nothing new: the daemon already
    /// verified that user's signature over this very job (`JobSpec::auth`,
    /// re-checked inside `JOB_AUTH_WINDOW`). It carries an authorization
    /// this node has already proved, to the process it was proved for.
    ///
    /// **Not a delegated token; R15 stands.** Nothing is published, no
    /// `UserRecord` exists, no peer is asked to trust anything, and the key
    /// is meaningless one process away — signed at any other node it is an
    /// unknown user, exactly as it should be. It dies with the job.
    fn mint_job_credential(&self, spec: &JobSpec, dir: &Path) -> Result<(String, String)> {
        let identity = dir.join("identity");
        std::fs::create_dir_all(&identity)?;
        let path = identity.join(crate::identity::USER_KEY_FILE);
        let keypair = Keypair::generate()?;
        keypair.persist(&path)?;
        self.job_credentials
            .lock()
            .insert(keypair.user_id(), (spec.job_id, spec.submitter_user));
        Ok((
            crate::runtime::CLIENT_IDENTITY_ENV.to_string(),
            path.to_string_lossy().into_owned(),
        ))
    }

    /// The same two keys for one *item* of a parallel job.
    ///
    /// An item is the shipped function, which by design never imports
    /// `meshware` — workers are handed the task in a bundle. So this mints
    /// nothing of its own and reuses the enclosing job's credential when
    /// there is one, which keeps a mapped function that *does* reach back
    /// speaking as the same person the job does, and keeps the number of
    /// live credentials one per job rather than one per item.
    fn item_client_env(&self, dir: &Path) -> Vec<(String, String)> {
        let mut env = vec![self.socket_env(), self.node_name_env()];
        env.extend(self.binary_env());
        let credential = dir.join("identity").join(crate::identity::USER_KEY_FILE);
        if credential.is_file() {
            env.push((
                crate::runtime::CLIENT_IDENTITY_ENV.to_string(),
                credential.to_string_lossy().into_owned(),
            ));
        }
        env
    }

    /// Who a request signed by `key` is really from, if `key` is a live job
    /// credential. `None` for every ordinary caller, which is the common
    /// case and costs one uncontended lock.
    pub fn acting_user(&self, key: &UserId) -> Option<UserId> {
        self.acting_for(key).map(|(_, user)| user)
    }

    /// The same, with the job it belongs to — which a remote executor needs
    /// named in the attestation, so a binding cannot outlive the job that
    /// justified it (`mesh_proto::acting`).
    pub fn acting_for(&self, key: &UserId) -> Option<(JobId, UserId)> {
        self.job_credentials.lock().get(key).copied()
    }

    /// Forget a finished job's credential, and remove the seed.
    ///
    /// On every exit path. A credential outliving its job would be a
    /// standing key on disk that speaks for somebody, which is the thing
    /// this design exists to not have.
    fn retire_job_credential(&self, job_id: &JobId, dir: &Path) {
        self.job_credentials
            .lock()
            .retain(|_, (owner, _)| owner != job_id);
        let _ = std::fs::remove_file(dir.join("identity").join(crate::identity::USER_KEY_FILE));
    }

    /// Replace `[runtime] sandbox*` live (SIGHUP).
    /// Are declared cpu/memory limits real ceilings on this node? (E2.1)
    pub fn enforcing_limits(&self) -> bool {
        self.cgroups.is_some()
    }

    /// This node's sandbox policy, as jobs are started under it.
    pub fn sandbox_policy(&self) -> crate::sandbox::Policy {
        self.sandbox.lock().clone()
    }

    pub fn set_sandbox(&self, policy: crate::sandbox::Policy) {
        *self.sandbox.lock() = policy;
    }

    /// The extra paths this job's service declared it needs (R4 P1).
    ///
    /// Read from the **signed, replicated service definition** — not from
    /// the job's environment, and not from anything the submitter attached
    /// to this run. A submitter who could name their own grants could name
    /// `~/.ssh`; the service author names which *argument* is a path, and
    /// the node decides what is never grantable at all
    /// ([`crate::sandbox::grantable`]).
    ///
    /// A service with no declaration, one registered before this existed, or
    /// an ad-hoc command all produce no grants and the sandbox is exactly
    /// what it always was.
    fn declared_grants(&self, spec: &JobSpec) -> crate::sandbox::Grants {
        let JobPayload::Service(exec) = &spec.payload else {
            return crate::sandbox::Grants::default();
        };
        let contract = self.engine.read(|s| {
            s.services
                .get(&exec.service_name)
                .and_then(|e| e.versions.get(&exec.version))
                .and_then(|v| v.contract.clone())
        });
        let Some(contract) = contract else {
            return crate::sandbox::Grants::default();
        };
        let declared = crate::script_spec::ScriptSpec::from_contract(&contract);
        crate::sandbox::Grants {
            read: crate::script_spec::resolve_paths(&declared.reads, &exec.inputs),
            write: crate::script_spec::resolve_paths(&declared.writes, &exec.inputs),
            // Filled in by `leased_grants` from the *lease table*, not from
            // here: what a job claimed is node-local state, and a service
            // contract has nothing to say about it.
            leased_read: Vec::new(),
            leased_write: Vec::new(),
        }
    }

    /// What a job's own processes may read: its directory, and every path
    /// its ruleset grants for reading or writing (SEC-3).
    ///
    /// For the daemon doing file work *on that job's behalf* — a task
    /// pushing a tree through its credential — which must reach no further
    /// than the task itself could. The paths are `None` when this node runs
    /// jobs unsandboxed, because then a task reads what the daemon reads and
    /// a ruleset it never had would only break it. `None` altogether for a
    /// job with no journal here.
    pub fn readable_by_job(&self, job: &JobId) -> Option<(PathBuf, Option<Vec<PathBuf>>)> {
        let spec = self.spec_of(job)?;
        let dir = self.job_dir(job);
        let policy = self.sandbox.lock().clone();
        if !crate::sandbox::applies(&policy) {
            return Some((dir, None));
        }
        let mut grants = self.declared_grants(&spec);
        self.leased_grants(job, &mut grants);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let rules =
            crate::sandbox::Rules::for_job(&policy, &dir, &self.data_dir, home.as_deref(), &grants);
        Some((
            dir,
            Some(rules.read.into_iter().chain(rules.write).collect()),
        ))
    }

    /// The volume paths this job holds, for the sandbox ruleset (N5 P1).
    ///
    /// **Re-derived from this node's own `[[volume]]` list**, never from
    /// `$MESHWARE_VOLUME_*` in the job's environment. The environment is
    /// something a submitter can write, and a sandbox that granted whatever
    /// a job said it had been given would be no sandbox at all. The lease
    /// table says which names this job actually holds; the node's own
    /// config says where those names live.
    ///
    /// Without this a job with a perfectly good writable lease got
    /// `Permission denied` from Landlock, because the mount was granted and
    /// the ruleset never heard about it.
    fn leased_grants(&self, job: &JobId, grants: &mut crate::sandbox::Grants) {
        let declared = self.volumes();
        let sched = self.sched.lock();
        for v in &declared {
            let (holders, _) = sched.leases.volume_holders(&v.name);
            if !holders.iter().any(|h| h == &job.short()) {
                continue;
            }
            let path = std::path::PathBuf::from(&v.path);
            // A node that lent a volume read-only refuses a write claim at
            // admission, so anything held here that is `read_only` is held
            // for reading however the job asked.
            if v.read_only {
                grants.leased_read.push(path);
            } else {
                grants.leased_write.push(path);
            }
        }
    }

    /// The command to actually spawn for a job: the program itself, or
    /// this binary re-executed as the sandbox shim around it — plus the
    /// environment the sandbox adds (a private `TMPDIR`), empty when the
    /// job runs plainly.
    #[allow(clippy::type_complexity)]
    fn sandboxed(
        &self,
        program: &str,
        args: &[String],
        job_dir: &Path,
        cgroup: Option<&Path>,
        grants: &crate::sandbox::Grants,
    ) -> Result<(
        std::ffi::OsString,
        Vec<std::ffi::OsString>,
        Vec<(String, String)>,
        bool,
    )> {
        let policy = self.sandbox.lock().clone();
        match crate::sandbox::wrap_granted(
            &policy,
            job_dir,
            &self.data_dir,
            program,
            args,
            cgroup,
            grants,
        )
        .map_err(CoreError::Job)?
        {
            // The `bool` is *"a shim will join the cgroup for us"*, and it
            // is not the same question as "is the sandbox on" (E2.1).
            // `wrap_in` returns `None` for three different reasons — mode
            // `Off`, Landlock unavailable, no shim binary — and the first
            // version of this checked only the first, so on a kernel
            // without Landlock every declared limit was silently unapplied
            // while `doctor` cheerfully reported enforcement. One flag,
            // from the code that actually knows.
            Some(w) => Ok((w.program.into_os_string(), w.args, w.env, true)),
            None => Ok((
                program.into(),
                args.iter().map(std::ffi::OsString::from).collect(),
                Vec::new(),
                false,
            )),
        }
    }

    fn self_name(&self) -> String {
        let me = self.engine.node_id();
        self.engine
            .read(|s| s.members.get(&me).map(|m| m.info.name.clone()))
            .unwrap_or_else(|| me.short())
    }

    /// Run one already-prepared program as a parallel-job item: a Library
    /// script's interpreter and argv, or a bash one-liner. Same executor
    /// hygiene as [`Self::run_item`]; the difference is only what gets
    /// spawned, so a script is runnable per item exactly as it is on Run.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_prepared(
        &self,
        program: &str,
        args: &[String],
        workdir: &Path,
        env: &[(String, String)],
        timeout: Option<Duration>,
        dir: &Path,
        // What this item's service declared it reads and writes (R4 P1).
        //
        // **This was `Grants::default()` and it was a hole.** Derived grants
        // reached a job submitted as one job and stopped at the door of the
        // path that runs four hundred of them — so `service run` could write
        // to a declared directory and `parallel --service` could not, which
        // is backwards: the per-item run is the one with somewhere to put
        // four hundred results.
        grants: &crate::sandbox::Grants,
        user: &mesh_proto::UserId,
    ) -> mesh_proto::ItemOutcome {
        let started = std::time::Instant::now();
        let fail = item_failure;
        let work = workdir.to_path_buf();
        let providers = self.providers.lock().clone();
        let resolved = match secrets::resolve_env(
            &self.engine,
            &self.node_key,
            &providers,
            self.item_client_env(dir)
                .into_iter()
                .chain(env.iter().cloned())
                .collect(),
            &dir.join("secrets"),
            Some(user),
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(e) => return fail(format!("secret resolution failed: {e}"), 0),
        };

        // Parallel items share their parent job's slot and its cgroup is
        // the parent's, so an item is not given one of its own (E2.1).
        let (exe, argv, sandbox_env, _) = match self.sandboxed(program, args, dir, None, grants) {
            Ok(triple) => triple,
            Err(e) => return fail(e.to_string(), 0),
        };
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(argv)
            .current_dir(&work)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in ["PATH", "HOME", "LANG", "LC_ALL", "USER", "TERM"] {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        for (k, v) in &resolved.env {
            cmd.env(k, v);
        }
        // The sandbox's own scratch (`TMPDIR` inside the job dir) — after
        // everything else, so nothing in the job's environment can point
        // its tools back at a `/tmp` it cannot write.
        for (k, v) in &sandbox_env {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        cmd.process_group(0);

        // Spawned rather than run through `Command::output()`, so that a
        // timeout has something to kill (ENG-16).
        //
        // `timeout(limit, cmd.output())` only *drops* the future. Tokio does
        // not kill a child on drop unless asked to, and even if it did it
        // would signal the one process it spawned — while this item, like
        // every job, runs in its own process group precisely because the
        // interesting part is usually the tree below it. So a timed-out
        // item was recorded as timed out and went on running: a 4,000-item
        // parallel job with a 30 s per-item limit could leave hundreds of
        // processes on a node that believed the work was over.
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return fail(format!("spawn failed: {e}"), 0),
        };
        let pid = child.id();
        let finished = child.wait_with_output();
        tokio::pin!(finished);

        let output = match timeout {
            Some(limit) => match tokio::time::timeout(limit, &mut finished).await {
                Ok(result) => result,
                Err(_) => {
                    // TERM first, so a tool that cleans up after itself gets
                    // to — then KILL, because an item that ignores TERM is
                    // exactly the item this limit exists for. Awaited in
                    // between so the pipes drain and the tree is reaped
                    // here rather than left to the orphan queue.
                    kill_group(pid, false);
                    if tokio::time::timeout(ITEM_KILL_GRACE, &mut finished)
                        .await
                        .is_err()
                    {
                        kill_group(pid, true);
                        let _ = tokio::time::timeout(ITEM_KILL_GRACE, &mut finished).await;
                    }
                    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    return fail(format!("timed out after {}s", limit.as_secs()), micros);
                }
            },
            None => finished.await,
        };
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let output = match output {
            Ok(output) => output,
            Err(e) => return fail(format!("spawn failed: {e}"), micros),
        };
        let redact = |bytes: &[u8]| {
            let cleaned = match Redactor::new(&resolved.redactions) {
                Some(mut r) => {
                    let mut out = r.feed(bytes);
                    out.extend(r.finish());
                    out
                }
                None => bytes.to_vec(),
            };
            mesh_proto::parallel::bound_output(&cleaned)
        };
        mesh_proto::ItemOutcome {
            item: String::new(),
            exit_code: output.status.code(),
            stdout: redact(&output.stdout),
            stderr: redact(&output.stderr),
            micros,
            // The executor never decides where output goes; the caller
            // does, because only it knows whether this job captures
            // inline or into an object (M9 §5.5).
            output: None,
        }
    }

    /// Turn a job payload into a spawnable command with a clean environment,
    /// resolving secret references at the last moment (MASTERPLAN §7.3).
    /// Returns the command plus the resolution outcome (redaction values +
    /// labels for the audit trail).
    #[allow(clippy::type_complexity)]
    async fn build_command(
        &self,
        spec: &JobSpec,
        dir: &Path,
    ) -> Result<(
        tokio::process::Command,
        secrets::ResolvedEnv,
        Option<crate::cgroup::JobCgroup>,
        // False when nothing will join the job cgroup for us, so the caller
        // must place the child itself after spawn.
        bool,
    )> {
        // Secret refs are env-only: argv is world-readable via /proc.
        if let JobPayload::Command(command) = &spec.payload
            && !secrets::find_refs(command).is_empty()
        {
            return Err(CoreError::Secret(
                "secret references are not allowed in the command line (visible in /proc) — \
                 pass them with --env NAME={{secret:…}}"
                    .into(),
            ));
        }
        let argv_inputs: &[(String, String)] = match &spec.payload {
            JobPayload::Service(exec) => &exec.inputs,
            JobPayload::Script(script) => &script.inputs,
            JobPayload::Command(_) => &[],
        };
        if argv_inputs
            .iter()
            .any(|(_, v)| !secrets::find_refs(v).is_empty())
        {
            return Err(CoreError::Secret(
                "secret references are not allowed in step inputs (they become argv) — \
                 use env or {{secret_file:…}}"
                    .into(),
            ));
        }

        let (program, mut args, workdir, extra_env, forward_env_at): (
            String,
            Vec<String>,
            PathBuf,
            Vec<(String, String)>,
            Option<usize>,
        ) = match &spec.payload {
            JobPayload::Command(command) => (
                "bash".into(),
                vec!["-c".into(), command.clone()],
                dir.join("work"),
                Vec::new(),
                None,
            ),
            JobPayload::Service(exec) => {
                // Materialize the bundle into the job's checkout dir. The
                // daemon guarantees the bundle is present before spawn.
                //
                // **Unless there is no bundle** (N4). A container service
                // derived from `[deployment.container]` is the image and
                // nothing else, and `materialize` on a `NONE` id fails — so
                // an image-only service could not start at all. An empty
                // checkout is the honest representation of "no code here".
                let checkout = dir.join("checkout");
                if exec.bundle_id.is_none() {
                    std::fs::create_dir_all(&checkout)?;
                } else {
                    self.content.materialize(&exec.bundle_id, &checkout)?;
                }
                let plugin = self.container_plugin();
                let detected = self.container_engine();
                let cgroup_parent = self
                    .cgroups
                    .as_ref()
                    .map(|d| d.jobs_dir().display().to_string());
                let node = self.engine.node_id().short();
                let prepared = crate::runtime::prepare(
                    &self.data_dir,
                    &checkout,
                    exec,
                    dir,
                    &spec.env,
                    crate::runtime::ContainerContext {
                        plugin: plugin.as_deref(),
                        engine: detected.as_ref(),
                        cgroup_parent: cgroup_parent.as_deref(),
                        node: &node,
                    },
                )
                .await?;
                (
                    prepared.program,
                    prepared.args,
                    prepared.workdir,
                    prepared.extra_env,
                    prepared.forward_env_at,
                )
            }
            JobPayload::Script(script) => {
                // No bundle to fetch: the body travelled with the spec.
                let prepared =
                    crate::runtime::prepare_inline(&self.data_dir, &dir.join("work"), script)
                        .await?;
                (
                    prepared.program,
                    prepared.args,
                    prepared.workdir,
                    prepared.extra_env,
                    prepared.forward_env_at,
                )
            }
        };

        // Assemble the job environment in precedence order (service static
        // env, runtime env, job env), then late-bind secret references.
        // Where the daemon is, and who this job is. Both go in first, so a
        // step that sets either for its own reasons still wins.
        let mut env_pairs: Vec<(String, String)> = vec![self.socket_env()];
        env_pairs.push(self.node_name_env());
        env_pairs.extend(self.binary_env());
        env_pairs.push(self.mint_job_credential(spec, dir)?);
        if let JobPayload::Service(exec) = &spec.payload {
            env_pairs.extend(exec.env.iter().cloned());
        }
        env_pairs.extend(extra_env.iter().cloned());
        // The acting-for claim stays behind (SEC-1). The daemon reads it off
        // the journaled spec when this job's credential calls back; the task
        // has no use for it, and its proof carries a whole signed request,
        // which is no thing to hand `execve` as one variable.
        env_pairs.extend(
            spec.env
                .iter()
                .filter(|(k, _)| !k.starts_with(mesh_proto::acting::ACT_ENV_PREFIX))
                .cloned(),
        );
        // The device allocation (C5 P4). Last, so nothing a user or a
        // service definition set can talk a task into seeing a card it was
        // not given.
        //
        // A job that asked for **no** devices gets the vendor masks set to
        // *empty*, not left unset: unset means "every device on the box" to
        // every vendor runtime there is, and isolation that only holds for
        // the jobs that asked for it is not isolation.
        let held = self.devices_for(&spec.job_id);
        env_pairs.extend(if held.is_empty() {
            mesh_proto::empty_allocation_env()
        } else {
            mesh_proto::allocation_env(&held)
        });
        let providers = self.providers.lock().clone();
        let resolved = secrets::resolve_env(
            &self.engine,
            &self.node_key,
            &providers,
            env_pairs,
            &dir.join("secrets"),
            Some(&spec.submitter_user),
        )
        .await?;

        // E2.1: the job's own cgroup, so `--cpus` and `--mem` are ceilings
        // rather than scheduling hints. `None` on a node that cannot
        // enforce — said once at startup and reported by `doctor`, never
        // silently.
        let cgroup = self
            .cgroups
            .as_ref()
            .and_then(|d| d.job(&spec.job_id.to_string(), &Requirements::from_env(&spec.env)));
        let mut grants = self.declared_grants(spec);
        // …plus the volumes this job actually holds (N5 P1). Read from the
        // lease table rather than from the job's environment — see
        // `leased_grants`. Without this a job with a perfectly good
        // writable lease got `Permission denied` from Landlock: the mount
        // was granted and the ruleset never heard about it.
        self.leased_grants(&spec.job_id, &mut grants);
        // **A container never goes through the shim (N4).**
        //
        // The process spawned for a container job is the *engine client*,
        // not the workload: `podman run` hands the workload to conmon,
        // which reparents it out of any Landlock ruleset applied here. So
        // confining this process constrains podman and not the container —
        // it buys nothing, and it breaks rootless podman outright, which
        // needs to write its own runtime directory under
        // `/run/user/<uid>`:
        //
        //   Error: creating events dirs:
        //   mkdir /run/user/1000/libpod/tmp/events: permission denied
        //
        // What a container has instead is the engine's own controls —
        // `--memory`, `--cpus`, `--read-only`, `--cap-drop=ALL`,
        // `--security-opt=no-new-privileges` — which `doctor` reports by
        // name, including when a runner claims none. That is the honest
        // answer to N4's landmine: not "Landlock still applies", which
        // would be false, but "here is exactly what does".
        let via_plugin = matches!(
            &spec.payload,
            JobPayload::Service(exec) if exec.runtime.is_plugin()
        );
        // The container's environment, **by name**: `--env NAME` makes the
        // engine read the value from its own environment, set just below,
        // so a resolved secret never appears in an argument list.
        if let Some(at) = forward_env_at {
            let mut names: Vec<&str> = resolved
                .env
                .iter()
                .map(|(k, _)| k.as_str())
                .filter(|k| crate::containers::forwards_into_container(k, &extra_env))
                .collect();
            names.sort_unstable();
            names.dedup();
            let flags = names
                .into_iter()
                .flat_map(|k| ["--env".to_string(), k.to_string()]);
            args.splice(at..at, flags);
        }
        let (exe, argv, sandbox_env, shim_joins_cgroup) = if via_plugin {
            (
                std::ffi::OsString::from(&program),
                args.iter().map(std::ffi::OsString::from).collect(),
                Vec::new(),
                false,
            )
        } else {
            self.sandboxed(
                &program,
                &args,
                dir,
                cgroup.as_ref().map(|c| c.path()),
                &grants,
            )?
        };
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(argv)
            .current_dir(&workdir)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Inherit a minimal, safe base environment.
        for var in ["PATH", "HOME", "LANG", "LC_ALL", "USER", "TERM"] {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        for (k, v) in &resolved.env {
            cmd.env(k, v);
        }
        // The sandbox's own scratch (`TMPDIR` inside the job dir) — after
        // everything else, so nothing in the job's environment can point
        // its tools back at a `/tmp` it cannot write.
        for (k, v) in &sandbox_env {
            cmd.env(k, v);
        }
        // Own process group so cancel kills the whole tree. (Unix-only:
        // non-unix builds are Director's non-executing peers, D6/D10.)
        #[cfg(unix)]
        cmd.process_group(0);
        Ok((cmd, resolved, cgroup, shim_joins_cgroup))
    }

    async fn execute(
        &self,
        spec: &JobSpec,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<JobStatus> {
        let dir = self.job_dir(&spec.job_id);
        let mut status = JobStatus {
            state: JobState::Running,
            exit_code: None,
            started_at_micros: Some(mesh_proto::now_micros()),
            finished_at_micros: None,
            charge: None,
        };
        self.write_status(&spec.job_id, &status)?;
        self.publish(spec, &status);

        // Build the command from the payload. Service preparation (bundle
        // materialization + env build) and secret resolution can fail;
        // surface that as job failure with the reason spooled to stderr.
        let build = self.build_command(spec, &dir).await;
        let (mut cmd, resolved, cgroup, shim_joins_cgroup) = match build {
            Ok(pair) => pair,
            Err(e) => {
                let _ = tokio::fs::write(dir.join("stderr.log"), format!("meshware: {e}\n")).await;
                secrets::shred_dir(&dir.join("secrets"));
                self.retire_job_credential(&spec.job_id, &dir);
                status.state = JobState::Failed;
                status.finished_at_micros = Some(mesh_proto::now_micros());
                self.meters.finished(JobState::Failed);
                self.finalize(spec, &mut status);
                self.write_status(&spec.job_id, &status)?;
                self.publish(spec, &status);
                return Err(e);
            }
        };

        // Audit every secret access, attributed to the submitting user
        // (MASTERPLAN §7.4 — the auditor can reproduce exactly who read what).
        for label in &resolved.resolved {
            crate::audit::record(
                &self.engine,
                spec.submitter_user,
                "secret.resolve",
                label,
                "ok",
                &format!("job {}", spec.job_id.short()),
                &[],
            );
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::write(dir.join("stderr.log"), format!("meshware: spawn: {e}\n"))
                    .await;
                secrets::shred_dir(&dir.join("secrets"));
                self.retire_job_credential(&spec.job_id, &dir);
                status.state = JobState::Failed;
                status.finished_at_micros = Some(mesh_proto::now_micros());
                self.meters.finished(JobState::Failed);
                self.finalize(spec, &mut status);
                self.write_status(&spec.job_id, &status)?;
                self.publish(spec, &status);
                return Err(CoreError::Job(format!("spawn: {e}")));
            }
        };
        let pid = child.id();
        if let Some(running) = self.sched.lock().running.get_mut(&spec.job_id) {
            running.pid = pid;
        }
        // …and on disk, so that a daemon which does not come back the same
        // way can still find this process group (ENG-16).
        self.record_proc(&spec.job_id, pid);

        // When the sandbox is off there is no shim, so nothing has joined
        // the cgroup — move the child in from here (E2.1).
        //
        // **This path has a window the shim path does not**: between spawn
        // and this write, the job is running with no ceiling. It is still
        // worth doing — a limit that applies a millisecond late is a limit —
        // and it is a further reason the sandbox is on by default.
        if let (Some(cg), Some(pid)) = (cgroup.as_ref(), pid)
            && !shim_joins_cgroup
            && let Err(e) = std::fs::write(cg.path().join("cgroup.procs"), pid.to_string())
        {
            tracing::debug!(job = %spec.job_id.short(), "could not place an unsandboxed job in its cgroup: {e}");
        }

        // Sample consumption while the job runs (C1 P1). Where cgroups are
        // enforcing, the kernel also tracks a peak; `/proc/<pid>/stat`
        // while the job lives is what is available either way.
        let sampled = Arc::new(AtomicU64::new(0));
        let sampler = tokio::spawn({
            let sampled = sampled.clone();
            async move {
                let Some(pid) = pid else { return };
                loop {
                    tokio::time::sleep(crate::accounting::SAMPLE_INTERVAL).await;
                    match crate::accounting::measure_cpu_secs(pid) {
                        // Monotonic: a reading taken as the process exits
                        // can come back short, and a counter that goes
                        // backwards is worse than one that stops.
                        Some(secs) => {
                            sampled.fetch_max(secs, Ordering::Relaxed);
                        }
                        None => return,
                    }
                }
            }
        });

        // Spool stdout/stderr to files, redacting registered secret values
        // before a single byte reaches disk (MASTERPLAN §7.3).
        let writes = self.log_writes_of(&spec.job_id);
        let out_task = spool(
            child.stdout.take(),
            dir.join("stdout.log"),
            Redactor::new(&resolved.redactions),
            writes.clone(),
            self.output.clone(),
        );
        let err_task = spool(
            child.stderr.take(),
            dir.join("stderr.log"),
            Redactor::new(&resolved.redactions),
            writes,
            self.output.clone(),
        );

        let timeout = spec.timeout_secs.map(Duration::from_secs);
        let final_state;
        let mut exit_code = None;

        let waited = async {
            tokio::select! {
                result = child.wait() => Ok(result),
                _ = cancel.changed() => Err("cancel"),
                _ = async {
                    match timeout {
                        Some(t) => tokio::time::sleep(t).await,
                        None => std::future::pending::<()>().await,
                    }
                } => Err("timeout"),
            }
        }
        .await;

        match waited {
            Ok(Ok(exit)) => {
                exit_code = exit.code();
                final_state = if exit.success() {
                    JobState::Succeeded
                } else {
                    JobState::Failed
                };
            }
            Ok(Err(e)) => {
                warn!(job = %spec.job_id.short(), "wait failed: {e}");
                final_state = JobState::Failed;
            }
            Err(why) => {
                // Cancelled or timed out: terminate the whole group.
                if why == "cancel" {
                    kill_group(pid, false);
                    let graceful = tokio::time::timeout(CANCEL_GRACE, child.wait()).await;
                    match graceful {
                        Ok(Ok(exit)) => exit_code = exit.code(),
                        _ => {
                            kill_group(pid, true);
                            if let Some(cg) = cgroup.as_ref() {
                                cg.kill();
                            }
                            let _ = child.wait().await;
                        }
                    }
                    final_state = if self.fail_marked.lock().remove(&spec.job_id) {
                        JobState::Failed
                    } else {
                        JobState::Cancelled
                    };
                } else {
                    kill_group(pid, true);
                    // The group signal misses anything that left the group
                    // (`setsid`); the cgroup does not (ENG-8).
                    if let Some(cg) = cgroup.as_ref() {
                        cg.kill();
                    }
                    let _ = child.wait().await;
                    final_state = JobState::TimedOut;
                }
            }
        }

        // The job is over when its main process is (ENG-8). Whatever it
        // left running and still holding its output gets a moment, then is
        // stopped: the process group first, politely, then the group and the
        // cgroup outright. What still holds the pipes after that — a
        // `setsid` escapee on a node without cgroups — is no longer waited
        // for, and the log says its output stops there.
        let (out_abort, err_abort) = (out_task.abort_handle(), err_task.abort_handle());
        let mut spools = tokio::spawn(async move {
            let _ = out_task.await;
            let _ = err_task.await;
        });
        let mut leftovers = None;
        if tokio::time::timeout(OUTPUT_GRACE, &mut spools)
            .await
            .is_err()
        {
            kill_group(pid, false);
            let stopped = tokio::time::timeout(Duration::from_secs(1), &mut spools)
                .await
                .is_ok();
            if !stopped {
                kill_group(pid, true);
                if let Some(cg) = cgroup.as_ref() {
                    cg.kill();
                }
            }
            let released = stopped
                || tokio::time::timeout(OUTPUT_GRACE, &mut spools)
                    .await
                    .is_ok();
            if !released {
                out_abort.abort();
                err_abort.abort();
                let _ = spools.await;
            }
            leftovers = Some(released);
            warn!(
                job = %spec.job_id.short(),
                output_collected = released,
                "the job's main process exited with processes it started still \
                 holding its output; they were stopped"
            );
        }
        if let Some(released) = leftovers {
            let note = if released {
                "\nmeshware: this job's main process exited while processes it started still \
                 held its output; they were stopped so the job could end.\n"
            } else {
                "\nmeshware: this job's main process exited while processes it started still \
                 held its output, and not all of them could be stopped (a process that left \
                 the job's process group, on a node without cgroups). Output after this \
                 point was not collected.\n"
            };
            if let Ok(mut f) = tokio::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(dir.join("stderr.log"))
                .await
            {
                use tokio::io::AsyncWriteExt as _;
                let _ = f.write_all(note.as_bytes()).await;
            }
        }
        // Both spools have finished, so nothing can write to this job's
        // logs again: drop its wake counter (**M-8**). Followers still
        // attached fall back to `TAIL_IDLE`, which is what notices the
        // termination — and without this the map would grow by one entry
        // per job the node ever ran.
        self.drop_log_writes(&spec.job_id);

        // E2.1: a job the kernel OOM-killed exits as a bare SIGKILL, which
        // reads identically to "somebody cancelled it" and sends the reader
        // to the wrong place entirely. The cgroup knows which it was.
        //
        // **Appended after the spools have finished, never written over
        // them.** The job's own stderr is the first thing somebody will
        // want and truncating it to explain the failure would be its own
        // small betrayal.
        if final_state == JobState::Failed
            && let Some(cg) = cgroup.as_ref()
            && cg.was_oom_killed()
        {
            let asked = Requirements::from_env(&spec.env).mem_mb;
            let peak = cg.peak_memory_bytes().unwrap_or(0) / (1024 * 1024);
            let note = format!(
                "\nmeshware: killed by the kernel — this job exceeded the {asked} MB it asked \
                 for (peak {peak} MB). Raise --mem, or reduce what it allocates.\n"
            );
            if let Ok(mut f) = tokio::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(dir.join("stderr.log"))
                .await
            {
                use tokio::io::AsyncWriteExt as _;
                let _ = f.write_all(note.as_bytes()).await;
            }
            warn!(
                job = %spec.job_id.short(), asked_mb = asked, peak_mb = peak,
                "job OOM-killed at its declared memory limit"
            );
        }

        // Injected secret files never outlive the job (MASTERPLAN §7.3).
        secrets::shred_dir(&dir.join("secrets"));
        // Nor does the credential that let it act as its own submitter.
        self.retire_job_credential(&spec.job_id, &dir);
        // Nor the process-group note: this job has been waited for, so a
        // record of what to kill would only be a pid for the next daemon
        // to misread (ENG-16).
        self.clear_proc(&spec.job_id);

        status.state = final_state;
        status.exit_code = exit_code;
        status.finished_at_micros = Some(mesh_proto::now_micros());
        // Counted where it happened (ENG-21). One of four sites, each
        // reached once per job on its own path: here for work that ran,
        // and the two build/spawn failures above, and the orphan sweep for
        // work a crash ended.
        self.meters.finished(final_state);
        if let Some(started) = status.started_at_micros {
            self.meters.duration.record(Duration::from_micros(
                status
                    .finished_at_micros
                    .unwrap_or(started)
                    .saturating_sub(started),
            ));
        }
        // The sampler's last reading, before the process is reaped. A job
        // shorter than one sample interval measures as nothing, which is
        // the truthful answer rather than a rounded-up guess — and it does
        // not affect the charge, which is reservation-based.
        let measured = sampled.load(Ordering::Relaxed);
        status.charge = Some(crate::accounting::Charge {
            measured_cpu_secs: (measured > 0).then_some(measured),
            ..Default::default()
        });
        sampler.abort();
        self.finalize(spec, &mut status);
        self.write_status(&spec.job_id, &status)?;
        self.publish(spec, &status);
        Ok(status)
    }

    /// Request cancellation of a locally-running or locally-queued job.
    pub fn cancel(&self, id: &JobId) -> Result<()> {
        let mut sched = self.sched.lock();
        if let Some(job) = sched.running.get(id) {
            let _ = job.cancel.send(true);
            return Ok(());
        }
        // Still waiting for a slot: remove it from the queue directly.
        if let Some(pos) = sched.queued.iter().position(|q| q.spec.job_id == *id) {
            let q = sched.queued.remove(pos);
            let status = JobStatus {
                state: JobState::Cancelled,
                exit_code: None,
                started_at_micros: None,
                finished_at_micros: Some(mesh_proto::now_micros()),
                charge: None,
            };
            self.write_status(id, &status)?;
            self.publish(&q.spec, &status);
            return Ok(());
        }
        Err(CoreError::Job(format!(
            "job {} is not running on this node",
            id.short()
        )))
    }

    /// Kill a running instance and record it as **failed** (C2 P3). Unlike
    /// [`Self::cancel`], the terminal state is `Failed`, so the deployment
    /// reconciler counts it against the restart budget.
    pub fn fail(&self, id: &JobId) -> Result<()> {
        self.fail_marked.lock().insert(*id);
        match self.cancel(id) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.fail_marked.lock().remove(id);
                Err(e)
            }
        }
    }

    /// The main process of a job running here, once it has one (Phase B, E3).
    pub fn pid_of(&self, id: &JobId) -> Option<u32> {
        self.sched.lock().running.get(id).and_then(|r| r.pid)
    }

    /// Where a running job's cgroup is, on a node that enforces cgroups
    /// (Phase B, E3). Named exactly as the cgroup was created.
    pub fn cgroup_dir_of(&self, id: &JobId) -> Option<std::path::PathBuf> {
        self.cgroups.as_ref().map(|d| d.job_dir(&id.to_string()))
    }

    pub fn is_running(&self, id: &JobId) -> bool {
        self.sched.lock().running.contains_key(id)
    }

    /// Every job running or queued here, under one lock (**M-5**).
    ///
    /// Bounded by this node's concurrency — slots plus the queue — never by
    /// how much history the mesh keeps, which is what makes it the right
    /// side to iterate when crossing it against a grow-only set.
    pub fn active_ids(&self) -> HashSet<JobId> {
        let sched = self.sched.lock();
        sched
            .running
            .keys()
            .copied()
            .chain(sched.queued.iter().map(|q| q.spec.job_id))
            .collect()
    }

    /// Running *or* queued here — the job will still make progress.
    pub fn is_active(&self, id: &JobId) -> bool {
        let sched = self.sched.lock();
        sched.running.contains_key(id) || sched.queued.iter().any(|q| q.spec.job_id == *id)
    }

    /// Does this node still hold this job's journal?
    ///
    /// **The mirror answers "no"; the disk confirms "yes"** (**H-1**).
    ///
    /// That asymmetry is the whole of why the mirror is safe. Nothing
    /// creates a journal outside this type, so a job the mirror has never
    /// heard of certainly has no record — and on the path this exists for,
    /// listing a mesh's jobs, *most* of them are somebody else's and take
    /// exactly that branch, at no syscall. A "yes" is still checked against
    /// the filesystem, because a directory can go without asking us: a data
    /// dir restored from a backup that did not include the job journal, or
    /// an operator with `rm`. Confirming costs one `stat()` per job this
    /// node actually holds, which is bounded by local retention rather than
    /// by the mesh's whole memory.
    pub fn has_record(&self, id: &JobId) -> bool {
        self.spooled.read().contains(id) && self.job_dir(id).join("spec.json").exists()
    }

    /// Delete this node's records — spec, status, stdout, stderr — for every
    /// job that had already finished at `before_micros`.
    ///
    /// The disk half of a mesh-wide clear. Running and queued work is
    /// skipped on both counts a retention sweep skips it on: non-terminal
    /// status, and [`Self::is_active`] for the job whose status file has
    /// not caught up with the scheduler yet. Returns how many records went.
    pub fn clear_finished(&self, before_micros: u64) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else {
            return 0;
        };
        let mut cleared = 0usize;
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|s| JobId::parse(s).ok())
            else {
                continue;
            };
            let Some(status) = self.read_status(&id) else {
                continue;
            };
            if !status.state.is_terminal() || self.is_active(&id) {
                continue;
            }
            // The same discriminator the replicated watermark uses, so the
            // disk and the state map forget exactly the same set: a job
            // still running when the clear was issued finishes after it.
            if status.finished_at_micros.unwrap_or(u64::MAX) >= before_micros {
                continue;
            }
            if std::fs::remove_dir_all(self.job_dir(&id)).is_ok() {
                self.unmark_spooled(&id);
                cleared += 1;
            }
        }
        if cleared > 0 {
            info!(cleared, "job history cleared");
        }
        cleared
    }

    /// Enforce job-history retention (MASTERPLAN §9.1: default 30 days /
    /// 1 GB per node): prune terminal jobs past the age cap, then prune the
    /// oldest terminal jobs until the total size fits. Active jobs are
    /// never touched. Returns how many job records were pruned.
    ///
    /// ## What happens to the bytes afterwards — M31 §19 Q9, decided
    ///
    /// **Nothing. There is no second durable copy inside meshware.**
    ///
    /// The question M26 raised was whether raw stdout should be kept past
    /// `[retention]` somewhere in the node. The answer is no, and it is a
    /// property rather than a limitation:
    ///
    /// > meshware keeps the **record** for as long as you say and the
    /// > **output** for as long as it is useful. If you need every byte for
    /// > five years, that is what a sink is for.
    ///
    /// Building a second durable payload tier here would duplicate what a
    /// sink does (M31 B2), put unbounded bytes back on every producer, and
    /// give the *"keep everything"* answer that §4 decision 7 exists to
    /// refuse — a default nobody chose, on a disk nobody sized.
    ///
    /// What A2 adds instead is that the absence is **legible**: a record
    /// whose bytes this swept reads *"output expired"* with the date and
    /// the reason, never *"not found"* and never a fabricated failure. See
    /// [`mesh_proto::OutputAvailability`].
    pub fn sweep_retention(&self, retention: crate::config::Retention) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else {
            return 0;
        };
        struct Row {
            id: JobId,
            finished: u64,
            bytes: u64,
        }
        let mut terminal: Vec<Row> = Vec::new();
        let mut total_bytes = 0u64;
        for entry in entries.flatten() {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|s| JobId::parse(s).ok())
            else {
                continue;
            };
            let bytes = dir_size(&entry.path());
            total_bytes += bytes;
            let Some(status) = self.read_status(&id) else {
                continue;
            };
            if !status.state.is_terminal() || self.is_active(&id) {
                continue;
            }
            terminal.push(Row {
                id,
                finished: status.finished_at_micros.unwrap_or(0),
                bytes,
            });
        }
        // Oldest first: age pruning and size pruning both walk this order.
        terminal.sort_by_key(|r| r.finished);
        let now = mesh_proto::now_micros();
        let age_cutoff = (retention.job_days > 0)
            .then(|| now.saturating_sub(u64::from(retention.job_days) * 86_400 * 1_000_000));
        let mut pruned = 0usize;
        for row in terminal {
            let too_old = age_cutoff.is_some_and(|cut| row.finished < cut);
            let too_big = retention.job_max_bytes > 0 && total_bytes > retention.job_max_bytes;
            if !too_old && !too_big {
                break; // sorted oldest-first: nothing later qualifies either
            }
            if std::fs::remove_dir_all(self.job_dir(&row.id)).is_ok() {
                self.unmark_spooled(&row.id);
                total_bytes = total_bytes.saturating_sub(row.bytes);
                pruned += 1;
            }
        }
        if pruned > 0 {
            info!(pruned, "job history retention sweep");
        }
        pruned
    }

    /// Follow a local job's output: replay from offsets, then live-tail
    /// until the job ends (or dump-and-done when `follow` is false).
    pub fn follow(
        self: &Arc<Self>,
        id: JobId,
        out_off: u64,
        err_off: u64,
        follow: bool,
    ) -> mpsc::Receiver<LogEvent> {
        let (tx, rx) = mpsc::channel::<LogEvent>(64);
        let this = self.clone();
        // Subscribed *before* the first drain, so a write that lands between
        // the drain and the wait is still seen as a change rather than
        // slept through (**M-8**).
        let mut writes = this.log_writes_of(&id).subscribe();
        tokio::spawn(async move {
            let dir = this.job_dir(&id);
            // Opened once and held, not reopened twice per poll (**M-8**).
            let mut out_tail = Tail::new(dir.join("stdout.log"), out_off);
            let mut err_tail = Tail::new(dir.join("stderr.log"), err_off);
            loop {
                let sent_out = out_tail.drain(1, &tx).await;
                let sent_err = err_tail.drain(2, &tx).await;
                if sent_out.is_err() || sent_err.is_err() {
                    return; // receiver hung up
                }
                // Queued counts as alive: the job will run once a slot frees.
                let running = this.is_active(&id);
                if !running {
                    // Final drain to catch bytes written between checks.
                    let _ = out_tail.drain(1, &tx).await;
                    let _ = err_tail.drain(2, &tx).await;
                    // **A missing status file is not a failed job** (M31
                    // A2). It used to fabricate `Failed` with no exit code
                    // here, which meant a job that succeeded ninety days
                    // ago reported as *failed* the moment `[retention]`
                    // removed its directory — a wrong answer, which is
                    // worse than a missing one and is the single most
                    // trust-damaging thing the product did.
                    //
                    // `Gone` says only what this node actually knows: the
                    // record is not on this disk. The caller holds the
                    // replicated summary and reports the job's real state;
                    // `daemon::stream_job_logs` is the only caller and does
                    // exactly that.
                    let done = match this.read_status(&id) {
                        Some(status) => LogEvent::Done {
                            state: status.state,
                            exit_code: status.exit_code,
                        },
                        None => LogEvent::Gone,
                    };
                    let _ = tx.send(done).await;
                    return;
                }
                if !follow {
                    // Here the fallback *is* honest: this branch is only
                    // reached while `is_active` says the job is running, so
                    // a status file that has not been written yet means
                    // running rather than lost.
                    let status = this.read_status(&id).unwrap_or(JobStatus {
                        state: JobState::Running,
                        exit_code: None,
                        started_at_micros: None,
                        finished_at_micros: None,
                        charge: None,
                    });
                    let _ = tx
                        .send(LogEvent::Done {
                            state: status.state,
                            exit_code: status.exit_code,
                        })
                        .await;
                    return;
                }
                // **Woken by writes, not by a clock** (M-8). The timeout
                // is the backstop for the one event that produces no
                // bytes — the job ending — and for a job whose counter has
                // already been dropped because its process is gone.
                //
                // **And by the reader leaving** (ENG-26). A hangup used to
                // be noticed only by a failed `send`, and a quiet job sends
                // nothing: someone who pressed ^C on `meshnode logs -f` of
                // a service that logs once an hour left this task, its
                // channel and two open file handles behind until the job
                // ended — which for a service is never. A long-lived node
                // with people watching services leaks a follower per
                // abandoned window, and runs out of descriptors long
                // before it runs out of anything else.
                tokio::select! {
                    _ = tokio::time::sleep(TAIL_IDLE) => {}
                    _ = writes.changed() => {}
                    _ = tx.closed() => return,
                }
            }
        });
        rx
    }
}

/// An item that never got as far as an exit code.
fn item_failure(message: String, micros: u64) -> mesh_proto::ItemOutcome {
    mesh_proto::ItemOutcome {
        item: String::new(),
        exit_code: None,
        stdout: String::new(),
        stderr: message,
        micros,
        output: None,
    }
}

/// Recursive directory size in bytes (job dirs are small trees).
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// Copy a child pipe into a log file until EOF, passing bytes through the
/// redaction filter when one is registered.
/// How long a job's output may stay open once its main process has exited,
/// before whatever is still holding it is stopped — and, once stopped, how
/// long it has to let go (ENG-8).
///
/// A script that starts `nohup ./svc &` and exits leaves `svc` holding the
/// job's stdout and stderr. The executor waited for both to close, so the
/// job never ended: it held its slot, its fan-out never completed, and a
/// cancel did nothing because the process it would have stopped was gone.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// What job output may cost (ENG-4); shared by every spool.
#[derive(Debug, Clone)]
struct OutputBudget {
    limit: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

impl Default for OutputBudget {
    fn default() -> Self {
        OutputBudget {
            limit: Arc::new(AtomicU64::new(
                crate::config::DiskConfig::default().job_output_max_bytes(),
            )),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }
}

fn spool<R>(
    reader: Option<R>,
    path: PathBuf,
    mut redactor: Option<Redactor>,
    writes: watch::Sender<u64>,
    budget: OutputBudget,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut reader) = reader else { return };
        let Ok(mut file) = tokio::fs::File::create(&path).await else {
            warn!("cannot create {}", path.display());
            return;
        };
        /// Tell every follower there are new bytes (**M-8**).
        fn woke(writes: &watch::Sender<u64>) {
            writes.send_modify(|n| *n = n.wrapping_add(1));
        }
        let mut buf = vec![0u8; CHUNK];
        // Bytes kept so far, and why the rest is not (ENG-4). The pipe is
        // still read to the end whatever happens: a job whose output is
        // not being kept must not block on a full pipe.
        let mut kept = 0u64;
        let mut dropping: Option<&'static str> = None;
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if dropping.is_none() {
                        if budget.paused.load(Ordering::Relaxed) {
                            dropping = Some(
                                "\nmeshware: output from here on was not kept — this node's disk \
                                 is below its free-space floor\n",
                            );
                        } else if kept.saturating_add(n as u64)
                            > budget.limit.load(Ordering::Relaxed)
                        {
                            dropping = Some(
                                "\nmeshware: output from here on was not kept — this stream \
                                 reached [disk] job_output_max_mb\n",
                            );
                        }
                        if let Some(note) = dropping {
                            let _ = file.write_all(note.as_bytes()).await;
                            let _ = file.flush().await;
                            woke(&writes);
                        }
                    }
                    if dropping.is_some() {
                        continue;
                    }
                    kept += n as u64;
                    // **No copy when nothing is being redacted** (M-8). The
                    // `to_vec` on this branch allocated and copied every
                    // chunk of every job's output for a filter that was not
                    // installed — which is the common case, since most jobs
                    // resolve no secrets.
                    let written = match redactor.as_mut() {
                        Some(r) => file.write_all(&r.feed(&buf[..n])).await,
                        None => file.write_all(&buf[..n]).await,
                    };
                    if written.is_err() {
                        // The disk refused it. Keep draining the pipe, or
                        // the job blocks writing to it (ENG-4); there is no
                        // room for a note.
                        dropping = Some("");
                        continue;
                    }
                    let _ = file.flush().await;
                    woke(&writes);
                }
            }
        }
        if let Some(mut r) = redactor.take() {
            let tail = r.finish();
            if !tail.is_empty() {
                let _ = file.write_all(&tail).await;
            }
        }
        let _ = file.flush().await;
        woke(&writes);
    })
}

/// One log file being followed, with its handle held open (**M-8**).
///
/// The free function this replaces `open`ed the file on every pass, so a
/// follower cost two `open`s and two `fstat`s 12.5 times a second for as
/// long as somebody watched a job — and a hundred followers made that
/// ~2 500 opens a second. The handle is kept; only the length is re-read,
/// which is the one thing that actually changes.
///
/// Opening is still lazy and still forgiving: a job that has not written
/// to stderr has no `stderr.log` yet, and that is not an error, so the
/// open is retried on each pass until it succeeds.
struct Tail {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    offset: u64,
}

impl Tail {
    fn new(path: PathBuf, offset: u64) -> Self {
        Tail {
            path,
            file: None,
            offset,
        }
    }

    /// Send any new bytes as chunks. Errors only when the receiver is gone.
    async fn drain(
        &mut self,
        stream: u8,
        tx: &mpsc::Sender<LogEvent>,
    ) -> std::result::Result<(), ()> {
        if self.file.is_none() {
            match tokio::fs::File::open(&self.path).await {
                Ok(f) => self.file = Some(f),
                Err(_) => return Ok(()), // not created yet
            }
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        // A file that shrank was replaced under us (a re-run reusing the
        // directory); start again rather than seek past its end.
        if len < self.offset {
            self.offset = 0;
        }
        while self.offset < len {
            if file
                .seek(std::io::SeekFrom::Start(self.offset))
                .await
                .is_err()
            {
                return Ok(());
            }
            let want = usize::try_from((len - self.offset).min(CHUNK as u64)).unwrap_or(CHUNK);
            let mut buf = vec![0u8; want];
            match file.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.truncate(n);
                    self.offset += n as u64;
                    if tx
                        .send(LogEvent::Chunk { stream, data: buf })
                        .await
                        .is_err()
                    {
                        return Err(());
                    }
                }
                Err(_) => break,
            }
        }
        Ok(())
    }
}

/// This boot of this machine, as the kernel names it (ENG-16).
///
/// `None` on a kernel that does not expose it, which makes every leftover
/// record unverifiable and so leaves every leftover alone — the safe
/// direction.
fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// When `pid` started, in clock ticks since boot: `/proc/<pid>/stat` field
/// 22, counting from 1.
///
/// Parsed from the *last* `)` rather than by splitting on whitespace,
/// because field 2 is the executable name, it is not escaped, and a
/// process named `foo bar) baz` would otherwise shift every field after it.
fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // After the name: state is field 3, so starttime (22) is the 20th.
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// Kill an entire process group (the child was started with pgid = its pid).
#[cfg(unix)]
fn kill_group(pid: Option<u32>, force: bool) {
    let Some(pid) = pid else { return };
    let Ok(raw) = i32::try_from(pid) else { return };
    let signal = if force {
        rustix::process::Signal::KILL
    } else {
        rustix::process::Signal::TERM
    };
    if let Some(pgid) = rustix::process::Pid::from_raw(raw)
        && let Err(e) = rustix::process::kill_process_group(pgid, signal)
    {
        warn!("kill process group {raw}: {e}");
    }
}

/// Non-unix builds exist only for Director's non-executing peers (D6/D10);
/// nothing spawns, so there is nothing to kill.
#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>, _force: bool) {}

/// The gang a job belongs to, from its runtime environment (C4 §8.2).
///
/// Reads the *runtime* key rather than the submission one: by the time a
/// spec reaches an executor the origin has already resolved `--gang N` into
/// the member set, and what travels is the gang's identity.
pub fn gang_of(env: &[(String, String)]) -> Option<mesh_proto::GangId> {
    env.iter()
        .find(|(k, _)| k == mesh_proto::GANG_ID)
        .and_then(|(_, v)| mesh_proto::GangId::parse(v).ok())
}

/// Would `want` fit on top of `held`, given advertised `cap`?
///
/// Lifted out of [`JobManager::admits`] so the lease path can ask the same
/// question about a hypothetical set of reservations without a `Sched` in
/// hand. One definition of "fits" or two answers that drift.
fn fits_capacity(
    cap: &Capacity,
    fallback_slots: u32,
    held: &Requirements,
    want: &Requirements,
) -> bool {
    if want.is_unstated() {
        return true;
    }
    if want.cpus > 0 {
        let budget = if cap.cores > 0 {
            cap.cores
        } else {
            fallback_slots
        };
        if held.cpus.saturating_add(want.cpus) > budget {
            return false;
        }
    }
    if want.mem_mb > 0 && cap.mem_mb > 0 && held.mem_mb.saturating_add(want.mem_mb) > cap.mem_mb {
        return false;
    }
    if want.disk_mb > 0
        && cap.disk_mb > 0
        && held.disk_mb.saturating_add(want.disk_mb) > cap.disk_mb
    {
        return false;
    }
    true
}
