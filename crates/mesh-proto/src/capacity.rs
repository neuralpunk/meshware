//! Advertised hardware capacity (M9 §5.1).
//!
//! A node announces what hardware it actually has as reserved tags on the
//! existing [`crate::MemberInfo::tags`], carried by the ordinary
//! `MemberUpdate` op:
//!
//! ```text
//! cap.cores=32   cap.mem_mb=127890   cap.disk_mb=896000   cap.arch=x86_64
//! ```
//!
//! Riding tags is what makes the whole capacity model wire-neutral: no new
//! op, no reshaped signature-covered struct, and a peer on an older minor
//! simply sees tags it does not recognise and ignores them (every existing
//! tag consumer matches an exact reserved string). The strings are parsed
//! once, at the state boundary, into the typed [`Capacity`] below — the
//! scheduler never parses a string on its hot path.
//!
//! These tags are *runtime-managed*: the node re-derives them on boot and on
//! config reload, they are preserved across user tag edits, and they are
//! rejected from user-supplied tag lists (a node may only describe its own
//! hardware).

use serde::{Deserialize, Serialize};

use crate::Hlc;

/// Prefix owned by the capacity model. Any tag starting with this is
/// runtime-managed and never user-editable.
pub const CAP_TAG_PREFIX: &str = "cap.";

/// Key for the hardware thread count.
pub const CAP_CORES: &str = "cap.cores";
/// Key for total physical memory, in MB.
pub const CAP_MEM_MB: &str = "cap.mem_mb";
/// Key for total data-dir filesystem size, in MB.
pub const CAP_DISK_MB: &str = "cap.disk_mb";
/// Key for whether this node accepts interactive terminals (M17 §3.4).
///
/// Not hardware, and it rides the `cap.` family anyway — because what that
/// prefix really means is *runtime-managed self-description that nobody
/// edits by hand*, which is exactly this. A node saying "I accept
/// terminals" is the same kind of statement as one saying it has 32 cores:
/// true of the machine, known only to the machine, and worthless if an
/// operator elsewhere could assert it on the machine's behalf.
///
/// It exists so the answer to "can anyone get a prompt on that box" is
/// visible from *any* node rather than only by reading a TOML file on that
/// one — which is what made the feature hard to reason about. Absent means
/// no, which is right for a pre-1.9 node and right for one with terminals
/// off.
pub const CAP_TERM: &str = "cap.term";

/// Key for the CPU architecture (`x86_64`, `aarch64`, …).
pub const CAP_ARCH: &str = "cap.arch";

/// Key for how many schedulable GPUs this node has (C5 §9.1).
///
/// **The filter, not the model.** A selector needs to answer "has a GPU"
/// cheaply from replicated state without parsing an inventory, and that is
/// all this is for; everything a device actually *is* — index, model, UUID,
/// memory, NUMA node, health — lives in the [`crate::device::DeviceInventory`]
/// entity, because a device is not a scalar and encoding one as flat
/// `key=value` strings produces a namespace nobody can validate.
///
/// Derived from the inventory rather than probed separately, so the two can
/// never disagree about how many devices a node has.
pub const CAP_GPUS: &str = "cap.gpus";

/// Key for the memory on the node's **largest single** GPU, in MB.
///
/// The largest, not the total, and the difference is the whole use: a box
/// with four 12 GB cards has 48 GB and cannot run a 40 GB task. A filter
/// that summed would send the job to a node that then refuses it.
pub const CAP_VRAM_MB: &str = "cap.vram_mb";

/// Key for the meshnode build this node is running (`cap.ver=0.6.0`).
///
/// Rides the `cap.` family for the same reason [`CAP_TERM`] does: what the
/// prefix really means is *runtime-managed self-description that nobody
/// edits by hand*, and a node's own build version is the purest example in
/// the set. It is true of the machine, known only to the machine, and
/// actively dangerous if an operator elsewhere could assert it on the
/// machine's behalf — a fleet upgrade decides what to do next by reading
/// this, so a node that could be *told* what version it runs would be a
/// node that could be skipped by lying about it.
///
/// It exists because "which of my thousand nodes are outdated" had no
/// answer short of asking each one. The handshake carries a protocol
/// minor, not a build, so a mesh could see that a peer was *stale* without
/// ever being able to say what it was running or what it should be.
///
/// Absent means unknown — a pre-M24 node — which surfaces as `-` rather
/// than as a version somebody might act on.
pub const CAP_VER: &str = "cap.ver";

/// Static hardware facts about a node: what it *has*, not what is free
/// right now. Live availability is ephemeral and travels out-of-band
/// (M9 §5.2), never in the op-log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    /// Hardware threads. 0 = unknown.
    pub cores: u32,
    /// Total RAM in MB. 0 = unknown.
    pub mem_mb: u64,
    /// Total size of the filesystem backing the data dir, in MB. 0 = unknown.
    pub disk_mb: u64,
    /// CPU architecture, empty when unknown.
    pub arch: String,
    /// The dynamic host-port range this node will allocate from (N2 P1).
    ///
    /// `None` means *this node advertises no range*, which for a port
    /// request is disqualifying rather than neutral — the device rule, not
    /// the memory rule. See [`crate::PortRange::from_tags`] for why the
    /// inversion is deliberate here.
    ///
    /// Safe to append to this struct where it would not be safe on a
    /// signed record: `Capacity` never travels as a postcard struct. It is
    /// rendered to `cap.*` tags by [`Self::to_tags`] and recovered by
    /// [`Self::from_tags`], and tags are a set of strings an older node
    /// simply does not recognise.
    pub ports: Option<crate::ports::PortRange>,
    /// The volumes this node lends, by name (N5).
    ///
    /// Here rather than announced on its own beside the `cap.*` tags,
    /// because *this* is the struct the reload path diffs to decide whether
    /// to re-announce, and the struct the re-announce rebuilds the tag set
    /// from. A volume list living outside it was a list `SIGHUP` could
    /// neither notice had changed nor put back — so the first unrelated
    /// reload dropped the node's volumes from its tags and it silently
    /// stopped being a candidate for work it could run.
    ///
    /// Empty means *lends none*, which disqualifies a claim exactly as an
    /// absent port range disqualifies a port request: a node that offers no
    /// `data` cannot be given a job that needs `data`.
    pub volumes: Vec<String>,
    /// The runtimes this node can actually serve (T3).
    ///
    /// **In `Capacity` rather than beside it**, and that is the whole of
    /// landmine 1: the tag set is rebuilt from *this struct* on every
    /// re-announce, so anything the struct does not carry is silently
    /// dropped by the first unrelated `SIGHUP`. N5 lost the volume list
    /// exactly this way, and it read as working until a reload happened to
    /// land.
    ///
    /// **Empty means unknown, not none**, which inverts the rule `ports`
    /// and `volumes` follow — see
    /// [`crate::runtimes::from_tags`] for why, and note the consequence
    /// here: a caller that does not know this node's runtimes emits no
    /// `cap.rt` and the node stays a candidate for everything, which is the
    /// safe default for a field that arrives a moment after boot.
    pub runtimes: Vec<crate::content::Runtime>,
}

impl Capacity {
    /// Render as reserved tags, in a stable order so two announcements of
    /// the same hardware compare equal byte-for-byte.
    pub fn to_tags(&self) -> Vec<String> {
        let mut tags = Vec::with_capacity(4);
        if self.cores > 0 {
            tags.push(format!("{CAP_CORES}={}", self.cores));
        }
        if self.mem_mb > 0 {
            tags.push(format!("{CAP_MEM_MB}={}", self.mem_mb));
        }
        if self.disk_mb > 0 {
            tags.push(format!("{CAP_DISK_MB}={}", self.disk_mb));
        }
        if let Some(range) = &self.ports {
            tags.push(format!(
                "{}={}",
                crate::ports::CAP_PORTS,
                range.as_tag_value()
            ));
        }
        if let Some(tag) = crate::volumes::cap_tag(&self.volumes) {
            tags.push(tag);
        }
        if let Some(tag) = crate::runtimes::cap_tag(&self.runtimes) {
            tags.push(tag);
        }
        if !self.arch.is_empty() {
            tags.push(format!("{CAP_ARCH}={}", self.arch));
        }
        tags
    }

    /// Parse the `cap.*` tags out of a member's tag list. `None` when the
    /// node advertises no capacity at all — a pre-M9 node, or one whose
    /// probes all failed. Unknown `cap.*` keys are ignored, so a future
    /// node adding `cap.gpus` stays parseable here.
    pub fn from_tags<S: AsRef<str>>(tags: &[S]) -> Option<Capacity> {
        let mut cap = Capacity {
            cores: 0,
            mem_mb: 0,
            disk_mb: 0,
            arch: String::new(),
            ports: None,
            volumes: Vec::new(),
            runtimes: Vec::new(),
        };
        let mut any = false;
        for tag in tags {
            let Some((key, value)) = tag.as_ref().split_once('=') else {
                continue;
            };
            match key {
                CAP_CORES => {
                    if let Ok(v) = value.parse() {
                        cap.cores = v;
                        any = true;
                    }
                }
                CAP_MEM_MB => {
                    if let Ok(v) = value.parse() {
                        cap.mem_mb = v;
                        any = true;
                    }
                }
                CAP_DISK_MB => {
                    if let Ok(v) = value.parse() {
                        cap.disk_mb = v;
                        any = true;
                    }
                }
                k if k == crate::ports::CAP_PORTS => {
                    // Fails closed: a malformed range advertises none.
                    if let Some(range) = crate::ports::PortRange::parse(value) {
                        cap.ports = Some(range);
                        any = true;
                    }
                }
                k if k == crate::volumes::CAP_VOLUMES => {
                    // The same fail-closed rule: a name this build would
                    // refuse is a name no job here can claim, so carrying it
                    // forward would only advertise a volume nobody reaches.
                    cap.volumes = value
                        .split(',')
                        .map(str::trim)
                        .filter(|n| crate::volumes::is_valid_volume_name(n))
                        .map(str::to_string)
                        .collect();
                    any = any || !cap.volumes.is_empty();
                }
                k if k == crate::runtimes::CAP_RUNTIMES => {
                    // Fails closed per name, like the volume list: a word
                    // this build cannot run is work it must not be sent.
                    cap.runtimes = value
                        .split(',')
                        .map(str::trim)
                        .filter_map(crate::content::Runtime::parse)
                        .collect();
                    any = true;
                }
                CAP_ARCH if !value.is_empty() => {
                    cap.arch = value.to_string();
                    any = true;
                }
                _ => {}
            }
        }
        any.then_some(cap)
    }
}

/// How loaded a node is *right now* (M9 §5.2) — the live half of the
/// capacity model.
///
/// Deliberately not in the op-log: per-second telemetry in a replicated
/// CRDT would turn the state store into a time-series database. It travels
/// on its own request pair, gated on the compute minor, piggybacked on the
/// gossip tick, held in memory, and never persisted.
///
/// Every probed field is optional because every probe can fail: a kernel
/// without PSI, a non-Linux host, an unreadable `/proc`. An absent field is
/// *unknown*, and unknown is neutral — never disqualifying — the same rule
/// the scheduler already applies to an unknown RTT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PressureSnapshot {
    /// 1-minute load average. Compare against [`Capacity::cores`], not
    /// against a constant — 8 on a 32-core box is idle.
    pub load1: Option<f64>,
    /// `MemAvailable`, in MB.
    pub mem_available_mb: Option<u64>,
    /// Free space on the data-dir filesystem, in MB.
    pub disk_free_mb: Option<u64>,
    /// cgroup v2 PSI `some avg10`, as a percent, per resource.
    pub psi_cpu: Option<f64>,
    pub psi_io: Option<f64>,
    pub psi_mem: Option<f64>,
    /// Jobs running and queued on this node right now.
    pub running: u32,
    pub queued: u32,
    /// When the reporter took this reading. Freshest observation wins.
    pub hlc: Hlc,
}

impl PressureSnapshot {
    /// Load per core, when both numbers are known. This is the comparable
    /// figure across a heterogeneous mesh: 1.0 means "as busy as it has
    /// cores for".
    pub fn load_per_core(&self, cores: u32) -> Option<f64> {
        match (self.load1, cores) {
            (Some(load), c) if c > 0 => Some(load / f64::from(c)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Live system telemetry (`TELEMETRY_MINOR`)
// ---------------------------------------------------------------------------

/// What a machine is *actually doing*, sampled fresh and spread to the
/// whole mesh rather than pulled one peer at a time.
///
/// ## Why this exists at all
///
/// [`PressureSnapshot`] reports `load1` and cgroup PSI, and neither one
/// answers "is this box busy right now". Measured on a 16-core machine
/// with every core pegged for twelve seconds:
///
/// | signal | reading | reads as |
/// |---|---|---|
/// | `load1` | 3.16 (0.20/core) | idle |
/// | `psi_cpu` | 2.61% | idle |
/// | actual utilisation | **100%** | — |
///
/// `load1` is a 60-second exponential average of the *run-queue length*,
/// so it lags a burst by minutes and only approaches core count after
/// several of them. PSI `some` measures *contention* — tasks waiting for a
/// CPU — which stays near zero when you run exactly as many threads as
/// there are cores, because nothing is waiting. Both are the right answer
/// to a question nobody was asking. [`Self::cpu_pct`] is the one that was.
///
/// ## Why a new struct rather than three more fields
///
/// [`PressureSnapshot`] is embedded positionally in `Response::Pressure`,
/// so growing it makes every older peer misparse every reply. This is the
/// same rule [`crate::device::DevicePressure`] was written under, and the
/// fifth application of it here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemSnapshot {
    /// Everything `Response::Pressure` already carried, unchanged: load,
    /// PSI, available memory, free disk, and the job counters. Placement
    /// still reads these, so nothing about scheduling changes shape.
    pub base: PressureSnapshot,
    /// **Busy CPU over the interval since the previous sample**, 0–100,
    /// from `/proc/stat` deltas. The number `load1` is persistently
    /// mistaken for, and the only one here that moves the instant a
    /// machine starts working.
    ///
    /// `None` on the very first sample of a node's life — one reading of a
    /// cumulative counter is not a rate — and off-Linux.
    pub cpu_pct: Option<f64>,
    /// Totals, so a percentage is computable from this struct alone. The
    /// base carries only what is *available*, which cannot be turned into
    /// "83% full" without knowing of what.
    pub mem_total_mb: Option<u64>,
    pub disk_total_mb: Option<u64>,
    /// Seconds since the machine booted. A node that keeps reappearing
    /// with a small number here is rebooting, which no other signal says.
    pub uptime_secs: Option<u64>,
    /// Tasks in `R` state — runnable right now. Unlike `load1` this is
    /// instantaneous, and unlike `cpu_pct` it survives a machine whose
    /// work is one thread spinning on one core out of sixty-four.
    pub procs_running: Option<u32>,
    /// **Network throughput in bytes per second**, received and
    /// transmitted, summed over the interfaces with a real device behind
    /// them (so container `veth`s and VLANs are not counted twice).
    ///
    /// ## Why this is a rate and not a percentage
    ///
    /// Every other number here has a denominator: `cpu_pct` is of the
    /// cores, `mem_used_pct` is of the memory, `disk_used_pct` is of the
    /// volume. Throughput has none the machine can be asked for. Link
    /// speed is not reliably knowable — `ethtool` reports nothing useful
    /// for virtio, bonds and many cloud NICs, and a negotiated 10G link
    /// behind a 1G uplink reports 10G — so 400 MB/s might be 4% of the
    /// path or 100% of it, and the machine cannot tell you which.
    ///
    /// So this is reported as what it is and **kept out of the two places
    /// a fabricated denominator would do damage**: it is not folded into
    /// [`Self::busy`] and it carries no scheduler weight. Guessing a
    /// capacity in order to have a percentage would move real work on the
    /// strength of a number nobody measured. As an operator signal it
    /// stands on its own — "this node is moving 400 MB/s" is the answer to
    /// why a job on it is slow, and it needs no denominator to be that.
    #[serde(default)]
    pub net_rx_bps: Option<u64>,
    #[serde(default)]
    pub net_tx_bps: Option<u64>,
}

impl SystemSnapshot {
    /// Lift a bare [`PressureSnapshot`] into the richer shape, with every
    /// live measurement unknown.
    ///
    /// What a peer below `TELEMETRY_MINOR` gives us: its pressure is real
    /// and its utilisation was never measured. `None` rather than zero,
    /// because zero is a claim and this is an absence — and every reader
    /// here already treats unknown as neutral.
    pub fn from_pressure(base: PressureSnapshot) -> Self {
        Self {
            base,
            cpu_pct: None,
            mem_total_mb: None,
            disk_total_mb: None,
            uptime_secs: None,
            procs_running: None,
            net_rx_bps: None,
            net_tx_bps: None,
        }
    }

    /// Total throughput both ways, in bytes per second.
    pub fn net_total_bps(&self) -> Option<u64> {
        match (self.net_rx_bps, self.net_tx_bps) {
            (None, None) => None,
            (rx, tx) => Some(rx.unwrap_or(0).saturating_add(tx.unwrap_or(0))),
        }
    }

    /// Memory in use, as a percent, when both halves are known.
    pub fn mem_used_pct(&self) -> Option<f64> {
        match (self.mem_total_mb, self.base.mem_available_mb) {
            (Some(total), Some(avail)) if total > 0 => {
                Some((total.saturating_sub(avail) as f64 / total as f64) * 100.0)
            }
            _ => None,
        }
    }

    /// Disk in use on the data-dir filesystem, as a percent.
    pub fn disk_used_pct(&self) -> Option<f64> {
        match (self.disk_total_mb, self.base.disk_free_mb) {
            (Some(total), Some(free)) if total > 0 => {
                Some((total.saturating_sub(free) as f64 / total as f64) * 100.0)
            }
            _ => None,
        }
    }

    /// One number for "how loaded is this machine", 0–1, for ranking and
    /// for a bar. The **worst** of the resources, because a node with idle
    /// CPUs and no memory left is not half busy — it is full.
    pub fn busy(&self) -> Option<f64> {
        let mut worst: Option<f64> = None;
        let mut note = |v: Option<f64>| {
            if let Some(v) = v {
                worst = Some(worst.map_or(v, |w: f64| w.max(v)));
            }
        };
        note(self.cpu_pct.map(|p| p / 100.0));
        note(self.mem_used_pct().map(|p| p / 100.0));
        // Network is deliberately absent: throughput has no denominator
        // this machine can supply (see `net_rx_bps`), and a term whose
        // scale is invented would move work on a number nobody measured.
        //
        // Disk is not contention the way CPU and memory are — a full disk
        // stops work rather than slowing it — so it only counts once it is
        // genuinely tight, where it becomes the whole answer.
        if self.disk_used_pct().is_some_and(|p| p > 90.0) {
            note(Some(1.0));
        }
        worst.map(|w| w.clamp(0.0, 1.0))
    }
}

/// One node's telemetry as it travels. Carries the node id because — unlike
/// `Response::Pressure`, which could only ever be about the peer answering
/// it — this spreads: a node passes on what it has heard, so a reading
/// reaches the whole mesh in log(n) rounds instead of waiting for every
/// node to happen to pick every other node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEntry {
    pub node_id: crate::NodeId,
    pub system: SystemSnapshot,
    pub devices: Vec<crate::device::DevicePressureV1>,
}

/// [`TelemetryEntry`] with readings that say which device they are about
/// (`DEVICE_KEY_MINOR`). What this node keeps and passes on; the V1 shape
/// is only ever produced for, or read from, an older peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEntryV2 {
    pub node_id: crate::NodeId,
    pub system: SystemSnapshot,
    pub devices: Vec<crate::device::DevicePressure>,
}

impl From<TelemetryEntryV2> for TelemetryEntry {
    fn from(e: TelemetryEntryV2) -> Self {
        TelemetryEntry {
            node_id: e.node_id,
            system: e.system,
            devices: e.devices.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<TelemetryEntry> for TelemetryEntryV2 {
    fn from(e: TelemetryEntry) -> Self {
        TelemetryEntryV2 {
            node_id: e.node_id,
            system: e.system,
            devices: e.devices.into_iter().map(Into::into).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Task requirements (M9 §5.3)
// ---------------------------------------------------------------------------

/// Prefix owned by the resource model. Reserved on the *job environment*
/// the same way `cap.` is reserved on tags: set by the submitting node,
/// rejected from user-supplied env.
pub const REQ_ENV_PREFIX: &str = "MESHWARE_TASK_";

/// Is this input key a *directive to the daemon* rather than a value for
/// the service?
///
/// Reserved keys ride a service run's `inputs` because `RunService` has no
/// `env` field and may never grow one — the executor re-decodes the request
/// from a signed envelope, so an older executor could not read a newer
/// origin's submission at all. The origin reads them off and strips them
/// **before** the contract is validated, so a schema with
/// `additionalProperties: false` never sees a key the CLI put there.
///
/// **A prefix test, not a list of the keys that exist today.** C7 added two
/// and enumerated them; C5's device request was then added to the CLI and
/// the enumeration was not updated, so `service run --gpus 1` failed
/// contract validation on a key the user never wrote. A prefix cannot be
/// forgotten by the next milestone.
pub fn is_reserved_input(key: &str) -> bool {
    key.starts_with(REQ_ENV_PREFIX)
}

/// Items whose bytes the origin staged, because no worker could see them
/// (R4 P3): object ids in item order, comma-separated, empty for an item
/// that was left where it was.
///
/// **An environment key rather than a field on `ParallelPlan`**, for the
/// reason this module gives for every other submission-side directive: a
/// plan crosses the peer wire, postcard encodes a struct positionally, and
/// a new field means a node one version behind cannot decode *any* plan
/// from a newer origin — not just a staged one. The environment is a map
/// that already exists and already travels.
pub const STAGED_ITEMS: &str = "MESHWARE_TASK_STAGED_ITEMS";

/// Whole cores this task needs (M9 §10 Q6: integers, never millicores).
pub const REQ_CPUS: &str = "MESHWARE_TASK_CPUS";
/// Memory this task needs, in MB.
pub const REQ_MEM_MB: &str = "MESHWARE_TASK_MEM_MB";
/// Scratch disk this task needs under the data dir, in MB.
pub const REQ_DISK_MB: &str = "MESHWARE_TASK_DISK_MB";

/// `--preemptible`: this work may be killed to give a principal back their
/// share (C1 P5).
///
/// A reserved environment key rather than a field on `JobSpec`, for the
/// reason M9 §5.3 gives for requirements and M19 gives for capture keys: a
/// spec is re-decoded from a *signed* envelope on the executing node, so
/// growing the request type means an older executor cannot read a newer
/// origin's submission at all. The environment is a map that already
/// exists and already travels.
///
/// **Opt-in, per submission, and never a default.** Silently killing
/// somebody's four-hour run to satisfy a share is how a scheduler loses a
/// user permanently (§5.2).
pub const REQ_PREEMPTIBLE: &str = "MESHWARE_TASK_PREEMPTIBLE";

/// Whether a job's environment marks it preemptible.
pub fn is_preemptible(env: &[(String, String)]) -> bool {
    env.iter()
        .any(|(k, v)| k == REQ_PREEMPTIBLE && matches!(v.as_str(), "1" | "true" | "yes"))
}

/// `--race N`: run this job on N nodes at once and keep the first answer
/// (M9 §5.6, generalized from straggler speculation).
///
/// A reserved environment key for the same reason [`REQ_PREEMPTIBLE`] is
/// one — and for a second reason specific to this feature. A race is
/// driven by whichever node currently owns the fan-out group, and
/// ownership moves: if the origin dies, a successor adopts the replicated
/// plan and carries on. Putting the policy in
/// [`crate::FanoutControls`] would have meant growing a struct nested
/// inside an op body, which changes the encoding of an existing op variant
/// and is exactly what F0's forward-compatibility story does *not* cover
/// (it carries unknown *variants*, not unknown *fields*). The plan's `env`
/// is a map that already replicates, so the successor reads the same
/// policy the origin had with no schema change at all.
///
/// **Only safe for work that is a pure function of its inputs.** Racing
/// three copies of `systemctl restart app` restarts it three times, so the
/// CLI refuses the flag unless the submission is also marked cacheable —
/// which is already the vocabulary for "hermetic, no side effects" (C7).
pub const REQ_RACE: &str = "MESHWARE_TASK_RACE";

/// `run --singleton NAME`: refuse this submission while another job under
/// the same name is still going. Origin-only, read off the environment
/// before placement and never seen by an executor — the same conveyance
/// as [`REQ_RACE`], for the same reason.
pub const REQ_SINGLETON: &str = "MESHWARE_TASK_SINGLETON";

/// How many copies of this job are racing, from its environment. `None`
/// when it is an ordinary job, which is every job that did not ask.
///
/// A value below 2 is not a race and reads as `None`: one copy of a job
/// running by itself is the normal path, and treating it as a degenerate
/// race would put the whole first-wins code path on jobs that never asked
/// for it.
/// `batch=True`: hand this task its whole chunk in one invocation instead
/// of spawning it once per item (M9 §5.4, the interpreter-startup half).
///
/// The measurement this exists for: a chunk is sized to a few seconds of
/// work, and a Python task that imports numpy pays 0.4–2s of that *per
/// item* before it computes anything. Spawning once per chunk amortises
/// the import across every item in it, which is the difference between a
/// map that is four times faster on the mesh and one that is two hundred
/// times faster.
///
/// **Not a default, and not inferable.** A task written for one item at a
/// time cannot be handed a hundred and be expected to notice; batching
/// changes the contract between the executor and the task, so the task has
/// to have been written for it. A batch shim sets this for itself
/// and nowhere else.
pub const REQ_BATCH: &str = "MESHWARE_TASK_BATCH";

/// Whether a job's environment asks for its chunk in one invocation.
pub fn is_batched(env: &[(String, String)]) -> bool {
    env.iter()
        .any(|(k, v)| k == REQ_BATCH && matches!(v.as_str(), "1" | "true" | "yes"))
}

pub fn race_of(env: &[(String, String)]) -> Option<u32> {
    env.iter()
        .find(|(k, _)| k == REQ_RACE)
        .and_then(|(_, v)| v.parse::<u32>().ok())
        .filter(|n| *n >= 2)
}

/// What one task needs to run. Zero means *unstated*, which is how every
/// pre-M9 job looks: unstated requirements fit everywhere, exactly as they
/// did before requirements existed.
///
/// **Why these ride the job environment.** Requirements have to reach the
/// executor, and both obvious carriers are closed: `JobSpec` travels
/// positionally inside `Request::SubmitJob`, and the `IpcRequest` the
/// executor re-decodes out of `JobSpec::auth` to re-verify authorization is
/// positional too — growing either would mean a 1.6 origin's job could not
/// be decoded by a 1.5 executor, or vice versa. `JobSpec::env` is an
/// existing `Vec<(String, String)>`, so reserved keys inside it change no
/// encoding at all (M9 §5.3, the same idiom as `cap.*` tags and `relay:`
/// candidates). A 1.5 executor simply does not enforce what it does not
/// recognise — the documented degradation.
///
/// The keys are `MESHWARE_TASK_*` because they are *also* the answer to
/// M9 §10 Q5: jobs run with `env_clear()`, so a task that wants to size its
/// own parallelism cannot read the host's core count and get a useful
/// answer. Carrying the requirement in the environment puts it in front of
/// the task for free.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirements {
    pub cpus: u32,
    pub mem_mb: u64,
    pub disk_mb: u64,
}

impl Requirements {
    /// Nothing declared — the pre-M9 default, and the common case.
    pub fn is_unstated(&self) -> bool {
        *self == Requirements::default()
    }

    /// Render as reserved environment entries. Unstated fields are omitted
    /// rather than written as `0`, so a task can distinguish "one core" from
    /// "nobody said".
    pub fn to_env(&self) -> Vec<(String, String)> {
        let mut env = Vec::with_capacity(3);
        if self.cpus > 0 {
            env.push((REQ_CPUS.to_string(), self.cpus.to_string()));
        }
        if self.mem_mb > 0 {
            env.push((REQ_MEM_MB.to_string(), self.mem_mb.to_string()));
        }
        if self.disk_mb > 0 {
            env.push((REQ_DISK_MB.to_string(), self.disk_mb.to_string()));
        }
        env
    }

    /// Recover requirements from a job's environment. Unparseable values are
    /// ignored: a bad number must not turn into an accidental demand.
    pub fn from_env(env: &[(String, String)]) -> Requirements {
        let mut reqs = Requirements::default();
        for (key, value) in env {
            match key.as_str() {
                REQ_CPUS => reqs.cpus = value.parse().unwrap_or(0),
                REQ_MEM_MB => reqs.mem_mb = value.parse().unwrap_or(0),
                REQ_DISK_MB => reqs.disk_mb = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        reqs
    }

    /// Could this task *ever* run on hardware like `cap`? This is the
    /// permanent question — a 48 GB task never fits a 16 GB box, however
    /// idle it gets — and it is what disqualifies a placement candidate.
    ///
    /// Unknown capacity is not a refusal: a node that advertises nothing is
    /// treated as able, and finds out at admission.
    pub fn fits_capacity(&self, cap: Option<&Capacity>) -> bool {
        let Some(cap) = cap else {
            return true;
        };
        (cap.cores == 0 || self.cpus <= cap.cores)
            && (cap.mem_mb == 0 || self.mem_mb <= cap.mem_mb)
            && (cap.disk_mb == 0 || self.disk_mb <= cap.disk_mb)
    }

    /// Could this task run on that node *right now*? The transient
    /// question, answered from live pressure. Unknown pressure is neutral.
    pub fn fits_pressure(&self, pressure: Option<&PressureSnapshot>) -> bool {
        let Some(p) = pressure else {
            return true;
        };
        p.mem_available_mb.is_none_or(|free| self.mem_mb <= free)
            && p.disk_free_mb.is_none_or(|free| self.disk_mb <= free)
    }
}

// ---------------------------------------------------------------------------
// Result capture (M19 §5.2)
// ---------------------------------------------------------------------------

/// Prefix owned by the result convention. Reserved on the *job environment*
/// exactly as `MESHWARE_TASK_` and `MESHWARE_INV_` are: set by the
/// submitting node, rejected from user-supplied env.
pub const OUT_ENV_PREFIX: &str = "MESHWARE_OUT_";

/// The output keys this submission wants captured, comma-separated.
///
/// **Why capture rides the environment.** `--json`, `--outputs` and
/// `--expect` have to reach the origin daemon, and the obvious carrier is
/// closed: the `IpcRequest` an executor re-decodes out of `JobSpec::auth`
/// to re-verify authorization is positional, so growing `Run` would mean a
/// 1.8 executor could not decode a 1.9 origin's job. `JobSpec::env` is an
/// existing `Vec<(String, String)>`, so a reserved key inside it changes no
/// encoding at all (M9 §5.3's idiom, third use).
///
/// Its *absence* is the pre-M19 behaviour and the common case: no key, no
/// capture, no log read back across the mesh.
pub const OUT_WANT: &str = "MESHWARE_OUT_WANT";

/// The `--expect` assertion, verbatim. Read and **removed** by the origin
/// daemon, which is the only node that evaluates it: an assertion is a
/// question about the whole set of rows, and there is no set on an
/// executor. Leaving it on the spec would put a user's assertion text in
/// front of six hundred machines for no reason.
pub const OUT_EXPECT: &str = "MESHWARE_OUT_EXPECT";

/// Is this environment key runtime-managed resource metadata? Such keys are
/// rejected from user-supplied env: a task cannot talk its way into a
/// bigger reservation than the operator asked for.
pub fn is_reserved_env(key: &str) -> bool {
    key.starts_with(REQ_ENV_PREFIX)
        || key.starts_with(crate::workflow::WF_ENV_PREFIX)
        || key.starts_with(crate::content::ANSIBLE_ENV_PREFIX)
        || key.starts_with(OUT_ENV_PREFIX)
        || key.starts_with(crate::gang::GANG_ENV_PREFIX)
        || key.starts_with(crate::device::DEVICE_ENV_PREFIX)
        || key.starts_with(crate::cache::CACHE_ENV_PREFIX)
        || key.starts_with(crate::acting::ACT_ENV_PREFIX)
        || key.starts_with(crate::facts::FACT_ENV_PREFIX)
        || crate::hosts::is_inventory_env(key)
        // N1 P5. A job cannot claim to have been fired by a schedule it
        // was not fired by — the same rule every other runtime-managed key
        // follows, for the same reason.
        || key == crate::schedule::SCHEDULE_ENV
        || key == crate::schedule::SCHEDULE_OCCURRENCE_ENV
}

/// Is this tag runtime-managed capacity?
pub fn is_capacity_tag(tag: &str) -> bool {
    tag.starts_with(CAP_TAG_PREFIX)
}

/// Does this member accept interactive terminals?
///
/// Read from the member's own announced tags, so it is that node's answer
/// about itself. `false` for a node with terminals off, and for a pre-1.9
/// node that has never heard of them — which is the same answer either way
/// and the safe one.
pub fn accepts_terminals<S: AsRef<str>>(tags: &[S]) -> bool {
    tags.iter()
        .any(|t| matches!(t.as_ref().split_once('='), Some((CAP_TERM, "1"))))
}

/// The tag a node announces while it is accepting terminals.
pub fn term_tag() -> String {
    format!("{CAP_TERM}=1")
}

/// The tag a node announces its own build as.
pub fn version_tag(version: &str) -> String {
    format!("{CAP_VER}={version}")
}

/// What build a member is running, from its own announced tags.
///
/// `None` for a node that predates version announcement, which is a real
/// state and not a zero: a rollout reports it as unknown rather than
/// assuming it is outdated (and upgrading it blind) or current (and
/// skipping it forever).
pub fn version_of<S: AsRef<str>>(tags: &[S]) -> Option<String> {
    tags.iter().find_map(|t| match t.as_ref().split_once('=') {
        Some((CAP_VER, v)) if !v.is_empty() => Some(v.to_string()),
        _ => None,
    })
}

/// A tag list with capacity stripped — what user-facing tag columns show,
/// since capacity gets a column of its own.
pub fn user_tags<S: AsRef<str>>(tags: &[S]) -> Vec<String> {
    tags.iter()
        .map(|t| t.as_ref())
        .filter(|t| !is_capacity_tag(t))
        .map(str::to_string)
        .collect()
}

/// May an admin assign this tag to **another** node, or is it the node's
/// own to state? `Some(why)` refuses, in words the caller can print.
///
/// The line is not "reserved or not" — it is *who is in a position to
/// know*. `no-shell` is a policy an operator declares about a machine and
/// its own documentation says an admin should be able to set it from
/// anywhere rather than editing TOML on six hundred boxes; `cap.cores=32`
/// is something only the machine can count. Asserting the second kind from
/// a distance does not change the machine, it just makes the mesh lie about
/// it — and every one of these already has a verb that does the real thing,
/// which is what the refusal names.
pub fn assignable_tag(tag: &str) -> Result<(), String> {
    let refuse = |what: &str, instead: &str| {
        Err(format!(
            "`{tag}` is {what}, so it cannot be assigned from here — {instead}"
        ))
    };
    if is_capacity_tag(tag) {
        return refuse(
            "hardware the node counts for itself",
            "it is re-derived on that node at every boot",
        );
    }
    if tag == crate::DRAINING_TAG {
        return refuse(
            "a node's own operational state — it is shutting down",
            "`meshnode nodes drain <node>` moves another node's instances off without stopping it, \
             and `meshnode drain` on the node itself shuts it down",
        );
    }
    if tag == crate::RELAY_TAG {
        return refuse(
            "announced only once a node has verified it is publicly reachable",
            "turn on `[wan] relay` on that node instead",
        );
    }
    if tag == crate::NO_EXECUTE_TAG {
        return refuse(
            "a capacity decision with its own signed verb",
            "use `meshnode nodes contribute <node> --off --reason …`, which records who decided and why",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn every_reserved_key_is_recognised_as_one() {
        // The regression this exists for: C7 enumerated the two keys it
        // added, C5's device key was added to the CLI later, and
        // `service run --gpus 1` then failed contract validation on a key
        // the user never wrote. Any key the daemon reads off a service run
        // must answer true here — add one to this list when you add one.
        for key in [
            super::REQ_CPUS,
            super::REQ_MEM_MB,
            super::REQ_DISK_MB,
            super::REQ_PREEMPTIBLE,
            crate::REQ_DEVICES,
            crate::REQ_CACHE,
            crate::REQ_CACHE_OUTPUTS,
            crate::REQ_CACHE_INPUTS,
            crate::REQ_GANG,
            crate::REQ_GANG_PLACEMENT,
            crate::REQ_GANG_RESTART,
        ] {
            assert!(super::is_reserved_input(key), "{key} is not reserved");
        }
    }

    #[test]
    fn an_ordinary_service_input_is_not_reserved() {
        // And the other direction, which is the one that would silently
        // eat somebody's input: a contract property must survive.
        for key in ["prompts", "steps", "model", "seed", "meshware", "TASK_CPUS"] {
            assert!(!super::is_reserved_input(key), "{key} was swallowed");
        }
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;

    fn sample() -> Capacity {
        Capacity {
            cores: 32,
            mem_mb: 127_890,
            disk_mb: 896_000,
            arch: "x86_64".into(),
            ports: None,
            volumes: Vec::new(),
            runtimes: Vec::new(),
        }
    }

    #[test]
    fn tags_roundtrip() {
        let cap = sample();
        let tags = cap.to_tags();
        assert_eq!(
            tags,
            vec![
                "cap.cores=32",
                "cap.mem_mb=127890",
                "cap.disk_mb=896000",
                "cap.arch=x86_64"
            ]
        );
        assert_eq!(Capacity::from_tags(&tags), Some(cap));
    }

    /// **Landmine 1, as a test.** The tag set is rebuilt from this struct
    /// on every re-announce, so a field the round trip drops is a field the
    /// first unrelated `SIGHUP` erases from the mesh's view of this node.
    /// N5 lost the volume list exactly that way, and it read as working
    /// until a reload happened to land; T3's runtime list is the same shape
    /// and this is what keeps it whole.
    #[test]
    fn every_field_survives_the_round_trip() {
        use crate::content::Runtime;
        let cap = Capacity {
            cores: 8,
            mem_mb: 16_384,
            disk_mb: 512_000,
            arch: "aarch64".into(),
            ports: crate::ports::PortRange::parse("24000-24099"),
            volumes: vec!["data".into(), "cache".into()],
            runtimes: vec![Runtime::Exec, Runtime::Bash, Runtime::Container],
        };
        let back = Capacity::from_tags(&cap.to_tags()).expect("capacity");
        assert_eq!(back, cap, "a dropped field is a field SIGHUP will erase");

        // …and the re-announce's own comparison agrees, which is the thing
        // that actually decides whether an op is authored.
        assert_eq!(Capacity::from_tags(&back.to_tags()).as_ref(), Some(&cap));
    }

    /// An empty runtime list emits **no tag at all**, so a caller that does
    /// not know this node's runtimes leaves it "unknown" rather than
    /// announcing "can run nothing". The inverse of the volume rule, and
    /// deliberate — see `runtimes::from_tags`.
    #[test]
    fn no_runtimes_means_no_tag() {
        let cap = sample();
        assert!(cap.runtimes.is_empty());
        assert!(
            !cap.to_tags().iter().any(|t| t.starts_with("cap.rt")),
            "an unknown runtime list must not be announced as an empty one"
        );
    }

    #[test]
    fn mixed_with_ordinary_tags() {
        let tags = vec![
            "linux".to_string(),
            "cap.cores=16".to_string(),
            "relay".to_string(),
        ];
        let cap = Capacity::from_tags(&tags).expect("cores alone is capacity");
        assert_eq!(cap.cores, 16);
        assert_eq!(cap.mem_mb, 0);
        assert_eq!(user_tags(&tags), vec!["linux", "relay"]);
    }

    #[test]
    fn no_capacity_is_none() {
        // A pre-M9 node: ordinary tags only.
        assert_eq!(Capacity::from_tags(&["linux", "git-sync"]), None);
        // Garbage under the prefix does not fabricate a capacity.
        assert_eq!(Capacity::from_tags(&["cap.cores=lots", "cap.arch="]), None);
        // An unknown future key is ignored, not an error.
        assert_eq!(Capacity::from_tags(&["cap.gpus=4"]), None);
    }

    #[test]
    fn unknown_keys_survive_alongside_known() {
        let cap = Capacity::from_tags(&["cap.gpus=4", "cap.cores=8"]).expect("cores parsed");
        assert_eq!(cap.cores, 8);
    }

    #[test]
    fn requirements_round_trip_through_env() {
        let reqs = Requirements {
            cpus: 4,
            mem_mb: 48 * 1024,
            disk_mb: 100 * 1024,
        };
        let env = reqs.to_env();
        assert_eq!(env[0], ("MESHWARE_TASK_CPUS".into(), "4".into()));
        assert_eq!(Requirements::from_env(&env), reqs);
        // Mixed with ordinary user env, and order-independent.
        let mut mixed = vec![("HOME".to_string(), "/root".to_string())];
        mixed.extend(env);
        assert_eq!(Requirements::from_env(&mixed), reqs);
    }

    #[test]
    fn unstated_requirements_fit_anything() {
        let none = Requirements::default();
        assert!(none.is_unstated());
        assert!(none.to_env().is_empty());
        assert_eq!(Requirements::from_env(&[]), none);
        assert!(none.fits_capacity(Some(&sample())));
        assert!(none.fits_capacity(None));
    }

    #[test]
    fn capacity_is_a_permanent_ceiling() {
        let small = Capacity {
            cores: 16,
            mem_mb: 16 * 1024,
            disk_mb: 100 * 1024,
            arch: "x86_64".into(),
            ports: None,
            volumes: Vec::new(),
            runtimes: Vec::new(),
        };
        let big_mem = Requirements {
            cpus: 1,
            mem_mb: 48 * 1024,
            disk_mb: 0,
        };
        assert!(!big_mem.fits_capacity(Some(&small)));
        assert!(big_mem.fits_capacity(Some(&sample())), "127 GB box fits it");
        // Unknown capacity never disqualifies.
        assert!(big_mem.fits_capacity(None));
        // Nor does an unknown *field* within a known capacity.
        let no_mem_probe = Capacity {
            mem_mb: 0,
            ..small.clone()
        };
        assert!(big_mem.fits_capacity(Some(&no_mem_probe)));
        // Cores are a ceiling too.
        let wide = Requirements {
            cpus: 32,
            mem_mb: 0,
            disk_mb: 0,
        };
        assert!(!wide.fits_capacity(Some(&small)));
    }

    #[test]
    fn pressure_is_a_transient_ceiling() {
        let reqs = Requirements {
            cpus: 0,
            mem_mb: 8192,
            disk_mb: 0,
        };
        let mut p = PressureSnapshot {
            load1: None,
            mem_available_mb: Some(4096),
            disk_free_mb: None,
            psi_cpu: None,
            psi_io: None,
            psi_mem: None,
            running: 0,
            queued: 0,
            hlc: Hlc::default(),
        };
        assert!(!reqs.fits_pressure(Some(&p)));
        p.mem_available_mb = Some(16384);
        assert!(reqs.fits_pressure(Some(&p)));
        // Unknown pressure, and unknown fields within it, are neutral.
        assert!(reqs.fits_pressure(None));
        p.mem_available_mb = None;
        assert!(reqs.fits_pressure(Some(&p)));
    }

    #[test]
    fn reserved_env_is_prefix_matched() {
        assert!(is_reserved_env("MESHWARE_TASK_CPUS"));
        assert!(is_reserved_env("MESHWARE_TASK_SOMETHING_FUTURE"));
        // M10 §4.2: step->run correlation rides the same idiom.
        assert!(is_reserved_env(OUT_WANT));
        assert!(is_reserved_env(crate::facts::FACT_SUBJECT_ENV));
        assert!(is_reserved_env("MESHWARE_FACT_SOMETHING_FUTURE"));
        assert!(is_reserved_env("MESHWARE_OUT_SOMETHING_FUTURE"));
        assert!(is_reserved_env("MESHWARE_WF_RUN"));
        assert!(is_reserved_env("MESHWARE_WF_STEP"));
        assert!(is_reserved_env("MESHWARE_WF_SOMETHING_FUTURE"));
        assert!(is_reserved_env(crate::content::ANSIBLE_LIMIT));
        assert!(is_reserved_env("MESHWARE_ANSIBLE_SOMETHING_FUTURE"));
        assert!(!is_reserved_env("MESHWARE_HOME"));
        assert!(!is_reserved_env("PATH"));
    }

    #[test]
    fn zero_fields_are_not_advertised() {
        let cap = Capacity {
            cores: 4,
            mem_mb: 0,
            disk_mb: 0,
            arch: String::new(),
            ports: None,
            volumes: Vec::new(),
            runtimes: Vec::new(),
        };
        assert_eq!(cap.to_tags(), vec!["cap.cores=4"]);
    }
}
