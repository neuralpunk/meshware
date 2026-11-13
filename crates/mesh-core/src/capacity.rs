//! Local hardware probes and the tags a node announces (MASTERPLAN M9 §5.1).
//!
//! Everything here reads the machine, cheaply, and hands the result to the
//! announcement path in [`crate::daemon`] or to the memory admission gate in
//! [`crate::jobs`]. The wire-visible shape — the reserved `cap.*` tags and
//! the typed [`Capacity`] — lives in `mesh-proto`; this module is only the
//! Linux-facing half.
//!
//! Every probe is best-effort. A failed probe reports `None`/0, which the
//! consumers treat as *unknown*, never as *zero capacity*: an unknown value
//! is neutral in scoring and never disqualifying, the same rule the
//! scheduler already applies to an unknown RTT.

use parking_lot::Mutex;
use std::path::Path;
use std::time::{Duration, Instant};

use mesh_proto::{Capacity, Hlc, PressureSnapshot};

use crate::config::NodeConfig;

/// Probe this machine's static capacity. `data_dir` selects which
/// filesystem the disk figure describes — jobs, bundles and objects all
/// land under it, so it is the volume that matters.
pub fn detect(data_dir: &Path) -> Capacity {
    Capacity {
        cores: cores(),
        mem_mb: mem_total_mb().unwrap_or(0),
        disk_mb: disk_total_mb(data_dir).unwrap_or(0),
        arch: std::env::consts::ARCH.to_string(),
        // Not probed: a port range is a *policy*, not a property of the
        // machine, so it comes from `[network]` and is filled in by `of`.
        // A bare `detect` is the hardware, and the hardware has no opinion
        // about which ports its operator is willing to lend. `[[volume]]`
        // is the same kind of statement and arrives the same way.
        ports: None,
        volumes: Vec::new(),
        // Probed, but not here: the runtime probes shell out and this is
        // called on the boot path. `of` fills it in from what
        // `crate::runtimes::detect` found, once it has finished (T1 §5).
        runtimes: Vec::new(),
    }
}

/// What this node announces: its declared capacity if one is configured,
/// otherwise a fresh probe of the machine — plus the port range from
/// `[network]`, which is policy either way (N2 P1).
pub fn of(config: &NodeConfig, runtimes: &[mesh_proto::Runtime]) -> Capacity {
    let mut cap = config
        .capacity_override
        .clone()
        .unwrap_or_else(|| detect(&config.data_dir));
    // Applied *after* the override, deliberately. `capacity_override` is
    // how a test or an operator lies about the hardware; the port range is
    // a separate decision in a separate section, and folding it into the
    // lie would make `[network]` silently ignorable.
    cap.ports = config.network.dynamic.clone();
    // N5, and beside the port range for the same reason: `[[volume]]` is a
    // separate decision in a separate section, and folding it into the lie
    // would make it silently ignorable.
    //
    // The *names*, not the paths: a selector needs to answer "does this node
    // have `data`", and where that directory actually sits on the machine is
    // nobody else's business.
    cap.volumes = config
        .network
        .volumes
        .iter()
        .map(|v| v.name.clone())
        .collect();
    // T3, and the same rule for the same reason: a probe's answer is not
    // the operator's to lie about with `capacity_override`.
    //
    // **Empty is left empty on purpose.** A caller that does not yet know
    // what this node can run — the boot announcement, which happens before
    // the probes finish — announces no `cap.rt`, and an absent tag means
    // *unknown*, which is neutral. Announcing an empty list instead would
    // mean "this node can run nothing", and every deployment would step
    // off it for the few seconds between boot and the first probe.
    cap.runtimes = runtimes.to_vec();
    cap
}

/// Hardware threads. This is the *machine's* size, deliberately not
/// `[limits] slots`: `slots` is the operator's cap on how many jobs run at
/// once, while `cap.cores` answers "what hardware does this node have".
/// Bin-packing needs the honest number; concurrency still obeys the cap.
pub fn cores() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

/// The tags this node announces: its configured tags plus freshly probed
/// capacity. Capacity tags supplied by config or CLI are dropped — a node
/// describes its own hardware and nobody describes it for them.
pub fn announced_tags(config: &NodeConfig, runtimes: &[mesh_proto::Runtime]) -> Vec<String> {
    announced_tags_with_devices(config, &crate::devices::detect(), runtimes)
}

/// The tags a node should announce on a boot *refresh*, given what the mesh
/// already records for it.
///
/// The refresh exists to republish addresses and hardware, and it used to
/// replace the whole tag list with what the local config implies. That
/// silently erased every tag the node did not carry in its own
/// `meshnode.toml` — which is all of them for a tag set by
/// `meshnode nodes tag` (that writes replicated state, not config) and for
/// a tag carried in a join token (`invite create --tag`). Both
/// survived until the node restarted and then vanished, which is the worst
/// shape a bug can have: it looks like it works.
///
/// The rule is: **runtime-managed tags are always the fresh ones, and every
/// other recorded tag is kept.** `cap.*`, the terminal tag and the version
/// tag describe what this machine is right now and must never be stale;
/// anything else was put there by a person or a token and is not this
/// function's to discard.
pub fn refreshed_tags<S: AsRef<str>>(
    config: &NodeConfig,
    devices: &[mesh_proto::Device],
    runtimes: &[mesh_proto::Runtime],
    recorded: &[S],
) -> Vec<String> {
    let mut tags = announced_tags_with_devices(config, devices, runtimes);
    for tag in recorded {
        let tag = tag.as_ref();
        // Runtime-managed: the freshly derived value wins, always.
        if mesh_proto::is_capacity_tag(tag) || is_boot_cleared(tag) {
            continue;
        }
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
    }
    tags
}

/// Tags a restart is *supposed* to clear.
///
/// `draining` is a statement about a process that no longer exists — its own
/// documentation says it is "cleared by the node's boot-time
/// re-announcement (a restarted node is not draining)" — so preserving it
/// across a restart would leave an upgraded node permanently ineligible for
/// work, which `rolling_upgrade.rs` catches and which is exactly the bug
/// that a naive "keep every recorded tag" merge introduces.
///
/// `console` is the same shape of statement: "there is a `meshnode` on this
/// box, so this peer is somebody's window rather than a machine". Director
/// decides it afresh on every launch (`reconcile_console_tag`) from what is
/// actually running, and puts it in the launch tags when it holds — so the
/// recorded copy is either redundant or stale. Keeping it made it sticky:
/// one boot beside a stray `meshnode` (a demo, a test harness) and the
/// machine was hidden from every fleet list, dropped from the compute
/// census and drawn without a load reading for as long as the mesh lived,
/// while the toggle that would have fixed it said the machine was fine.
///
/// A drain's own deadline tags (`drain.by=…`, `drain.keep-system`) are the
/// same statement as `draining` and go with it. Kept, they left every node a
/// rolling upgrade had passed through looking drained for good — excluded
/// from placement by `drain_of`, until the mesh had nowhere to put work and
/// `rolling_upgrade.rs` failed on every run (ENG-6). A drain an admin
/// *assigned* (`meshnode nodes drain`) is not in the node's announcement and
/// survives the restart through the assignment.
fn is_boot_cleared(tag: &str) -> bool {
    tag == mesh_proto::DRAINING_TAG || mesh_proto::is_drain_tag(tag)
}

/// [`announced_tags`] against an already-probed device list (C5 §9.1).
///
/// Split out because the device probe forks two processes and the boot path
/// already has the answer — but also because the *coarse device tags are
/// derived from the inventory rather than probed separately*, so the tag
/// and the entity can never disagree about how many cards a node has.
pub fn announced_tags_with_devices(
    config: &NodeConfig,
    devices: &[mesh_proto::Device],
    runtimes: &[mesh_proto::Runtime],
) -> Vec<String> {
    let mut tags = mesh_proto::user_tags(&config.tags);
    tags.extend(of(config, runtimes).to_tags());
    tags.extend(device_tags(devices));
    if config.sessions.accept {
        tags.push(mesh_proto::term_tag());
    }
    tags.push(mesh_proto::version_tag(VERSION));
    tags
}

/// The coarse filter tags a device list implies: `cap.gpus` and
/// `cap.vram_mb` (§9.1).
///
/// **The filter, not the model.** Everything a device actually is lives in
/// the replicated [`mesh_proto::DeviceInventory`]; these two exist so a
/// selector can answer "has a GPU" without parsing one.
pub fn device_tags(devices: &[mesh_proto::Device]) -> Vec<String> {
    mesh_proto::DeviceInventory::new(mesh_proto::NodeId([0; 32]), devices.to_vec(), 0).filter_tags()
}

/// The build this node is running, announced as `cap.ver` (M24 §4).
///
/// Re-announced on every boot, which is what makes a fleet upgrade
/// observable: a node that swapped its binary and came back says so, from
/// anywhere, without being asked.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Replace the `cap.*` tags in an existing list with `fresh`, preserving
/// every other tag and its order. Used on re-announcement, where runtime
/// tags (`draining`, `relay`, an operator's `nodes tag --add`) must survive.
///
/// `accepts_terminals` rides along because it is announced the same way and
/// has to survive the same replacement — a SIGHUP that turns terminals off
/// has to stop announcing them, which is the whole point of re-announcing.
pub fn with_capacity(tags: &[String], fresh: &Capacity, accepts_terminals: bool) -> Vec<String> {
    with_capacity_and_devices(tags, fresh, accepts_terminals, &[])
}

/// [`with_capacity`] carrying the device filter tags too (C5 §9.1).
///
/// Devices are replaced wholesale rather than merged, exactly as the
/// scalars are, and for the same reason: a card pulled out of the machine
/// has to *stop* being advertised, which is the whole point of
/// re-announcing.
pub fn with_capacity_and_devices(
    tags: &[String],
    fresh: &Capacity,
    accepts_terminals: bool,
    devices: &[mesh_proto::Device],
) -> Vec<String> {
    let mut out = mesh_proto::user_tags(tags);
    out.extend(fresh.to_tags());
    out.extend(device_tags(devices));
    if accepts_terminals {
        out.push(mesh_proto::term_tag());
    }
    out.push(mesh_proto::version_tag(VERSION));
    out
}

/// Read this node's live pressure (M9 §5.2). Cheap enough to answer on
/// demand from the gossip tick: four small `/proc` reads and two counters.
/// Every probe is independent, so a kernel missing PSI still reports load
/// and memory.
pub fn pressure(data_dir: &Path, running: u32, queued: u32, hlc: Hlc) -> PressureSnapshot {
    PressureSnapshot {
        load1: load1(),
        mem_available_mb: mem_available_mb(),
        disk_free_mb: disk_free_mb(data_dir),
        psi_cpu: psi_some_avg10("cpu"),
        psi_io: psi_some_avg10("io"),
        psi_mem: psi_some_avg10("memory"),
        running,
        queued,
        hlc,
    }
}

/// Samples real CPU utilisation from `/proc/stat`.
///
/// A utilisation figure is a *rate*, so it needs two readings of a
/// cumulative counter and a window between them. That has two consequences
/// this type exists to handle.
///
/// **The window belongs to the sampler, not to the caller.** A naive
/// version that differenced against the previous call on every call let
/// whoever asked define the interval — so two readers in quick succession
/// (a gossip tick and a `meshnode top`, which is exactly what happens)
/// each got a near-zero window: one reset the baseline, the other measured
/// microseconds of it, and both reported nonsense or nothing. Here a read
/// inside [`MIN_WINDOW`] returns the last computed figure instead of
/// taking a new one, so every reader in that window sees the same number
/// and the number covers a real interval.
///
/// **The first reading is unknown, not zero.** One sample of a counter is
/// not a rate, and reporting 0% for a machine nobody has measured yet is a
/// claim rather than an absence.
#[derive(Debug, Default)]
pub struct CpuSampler {
    inner: Mutex<CpuState>,
}

#[derive(Debug, Default)]
struct CpuState {
    /// The previous `(idle, total)` jiffies, and when they were taken.
    last: Option<((u64, u64), Instant)>,
    /// The last computed percentage, served to every reader inside the
    /// window so concurrent readers agree.
    current: Option<f64>,
}

/// The shortest interval a utilisation figure is computed over. Below this
/// the jiffy counters have barely moved and the quotient is noise.
const MIN_WINDOW: Duration = Duration::from_millis(500);

impl CpuSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Busy percentage over the most recent completed window, 0–100.
    ///
    /// `/proc/stat`'s `cpu` line is cumulative jiffies per state since
    /// boot. Busy is everything that is not idle or iowait — iowait counts
    /// as idle because a core waiting on a disk is a core available to run
    /// something else, which is the question a scheduler is asking.
    pub fn sample(&self) -> Option<f64> {
        let mut state = self.inner.lock();
        let now = Instant::now();
        if let Some((_, taken)) = state.last
            && now.duration_since(taken) < MIN_WINDOW
        {
            return state.current;
        }
        let Some((idle, total)) = read_cpu_times() else {
            return state.current;
        };
        let previous = state.last.replace(((idle, total), now));
        let ((prev_idle, prev_total), _) = previous?;
        let Some(d_total) = total.checked_sub(prev_total) else {
            return state.current;
        };
        if d_total == 0 {
            return state.current;
        }
        let d_idle = idle.saturating_sub(prev_idle);
        let busy = d_total.saturating_sub(d_idle) as f64 / d_total as f64;
        state.current = Some((busy * 100.0).clamp(0.0, 100.0));
        state.current
    }
}

/// `(idle, total)` jiffies from the aggregate `cpu` line of /proc/stat.
fn read_cpu_times() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|f| f.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal guest guest_nice
    if fields.len() < 5 {
        return None;
    }
    let idle = fields[3] + fields[4];
    // `guest` and `guest_nice` are already counted inside `user` and
    // `nice`; adding them again would inflate the denominator on a
    // hypervisor and quietly under-report every VM in the fleet.
    let total: u64 = fields.iter().take(8).sum();
    Some((idle, total))
}

/// Samples network throughput from `/proc/net/dev`.
///
/// The same windowed shape as [`CpuSampler`], for the same reason: bytes
/// in `/proc/net/dev` are cumulative since boot, so a throughput figure is
/// a rate and the window belongs to the sampler rather than to whoever
/// asked.
///
/// ## Which interfaces count
///
/// Only those with a real device behind them —
/// `/sys/class/net/<if>/device` exists. That one test does two jobs:
///
/// - it drops `lo`, bridges, `veth`, `tun`/`tap`, WireGuard and the rest,
///   which are not the machine's link to the world;
/// - and it stops **double counting**. A container's traffic appears on
///   its `veth` *and* on the NIC it leaves by; a VLAN's appears on
///   `eth0.100` *and* on `eth0`; a bond's on `bond0` *and* on its slaves.
///   Counting only physical devices counts each byte where it actually
///   crossed the wire, once.
#[derive(Debug, Default)]
pub struct NetSampler {
    inner: Mutex<NetState>,
}

#[derive(Debug, Default)]
struct NetState {
    last: Option<((u64, u64), Instant)>,
    current: Option<(u64, u64)>,
}

impl NetSampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// `(rx, tx)` bytes per second over the most recent completed window.
    pub fn sample(&self) -> Option<(u64, u64)> {
        let mut state = self.inner.lock();
        let now = Instant::now();
        if let Some((_, taken)) = state.last
            && now.duration_since(taken) < MIN_WINDOW
        {
            return state.current;
        }
        let Some((rx, tx)) = read_net_bytes() else {
            return state.current;
        };
        let previous = state.last.replace(((rx, tx), now));
        let ((prev_rx, prev_tx), taken) = previous?;
        let secs = now.duration_since(taken).as_secs_f64();
        if secs <= 0.0 {
            return state.current;
        }
        // A counter that went backwards is an interface that came, went and
        // came back — reporting the wrap as a burst would put a spike on
        // the chart that never happened.
        let rate = |now: u64, then: u64| -> u64 {
            now.checked_sub(then)
                .map_or(0, |d| (d as f64 / secs).round() as u64)
        };
        state.current = Some((rate(rx, prev_rx), rate(tx, prev_tx)));
        state.current
    }
}

/// Total `(rx, tx)` bytes across interfaces with a real device behind them.
fn read_net_bytes() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/net/dev").ok()?;
    let mut rx_total = 0u64;
    let mut tx_total = 0u64;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if !std::path::Path::new(&format!("/sys/class/net/{name}/device")).exists() {
            continue;
        }
        let fields: Vec<u64> = rest
            .split_whitespace()
            .map(|f| f.parse().unwrap_or(0))
            .collect();
        // receive: bytes packets errs drop fifo frame compressed multicast
        // transmit: bytes ...
        if fields.len() < 9 {
            continue;
        }
        rx_total = rx_total.saturating_add(fields[0]);
        tx_total = tx_total.saturating_add(fields[8]);
    }
    Some((rx_total, tx_total))
}

/// Seconds since boot, from /proc/uptime.
pub fn uptime_secs() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some(secs as u64)
}

/// Tasks in `R` state right now, from the `procs_running` line of
/// /proc/stat. Instantaneous, where `load1` is a one-minute average.
pub fn procs_running() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().find(|l| l.starts_with("procs_running"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The full live picture of this machine: everything `pressure` reported,
/// plus the numbers that actually move when it gets busy.
pub fn system(
    data_dir: &Path,
    cpu: &CpuSampler,
    net: &NetSampler,
    running: u32,
    queued: u32,
    hlc: Hlc,
) -> mesh_proto::SystemSnapshot {
    let (net_rx_bps, net_tx_bps) = match net.sample() {
        Some((rx, tx)) => (Some(rx), Some(tx)),
        None => (None, None),
    };
    mesh_proto::SystemSnapshot {
        base: pressure(data_dir, running, queued, hlc),
        cpu_pct: cpu.sample(),
        mem_total_mb: mem_total_mb(),
        disk_total_mb: disk_total_mb(data_dir),
        uptime_secs: uptime_secs(),
        procs_running: procs_running(),
        net_rx_bps,
        net_tx_bps,
    }
}

/// How much memory this node has, in MB — **the smaller of the machine's
/// and the cgroup's** (ENG-23).
///
/// `/proc/meminfo` is not namespaced, so inside a container it is the
/// host's. A node with a 2 GB limit on a 512 GB machine advertised 512 GB,
/// was sent work sized for it, and was OOM-killed; ENG-1 restarted it, it
/// advertised 512 GB again, and it took the same work. A crash loop whose
/// cause is a number.
///
/// Not only containers: `meshnode.service` with `MemoryMax=8G` is the same
/// statement by another route, and the operator who wrote that line meant
/// it as a bound on this node.
pub fn mem_total_mb() -> Option<u64> {
    let machine = machine_mem_total_mb();
    match (machine, crate::cgroup::memory_limit()) {
        (Some(machine), Some((limit, _))) => Some(machine.min(limit)),
        // A limit with no `/proc/meminfo` is still a limit worth honouring.
        (None, Some((limit, _))) => Some(limit),
        (machine, None) => machine,
    }
}

/// `MemTotal` from `/proc/meminfo`: the **machine's** memory, before any
/// cgroup limit is applied (ENG-23).
///
/// Almost every caller wants [`mem_total_mb`] instead — this node's memory
/// is the smaller of the two. This exists for the one place that has to
/// report both numbers and say which is binding.
pub fn machine_mem_total_mb() -> Option<u64> {
    meminfo_field("MemTotal:")
}

/// How much memory is free to take, in MB — again the smaller of the two
/// answers (ENG-23). `None` off-Linux or on parse failure, which callers
/// treat as unconstrained.
///
/// The cgroup's share is `memory.max - memory.current`. `MemAvailable` on
/// its own is the host's idea of free memory, which on a limited node is
/// both far too large and completely unrelated to what the next allocation
/// will be allowed to do.
pub fn mem_available_mb() -> Option<u64> {
    let machine = meminfo_field("MemAvailable:");
    let cgroup =
        crate::cgroup::memory_limit().map(|(limit, current)| limit.saturating_sub(current));
    match (machine, cgroup) {
        (Some(machine), Some(cgroup)) => Some(machine.min(cgroup)),
        (None, Some(cgroup)) => Some(cgroup),
        (machine, None) => machine,
    }
}

fn meminfo_field(prefix: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with(prefix))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

/// Total size of the filesystem backing `path`, in MB.
#[cfg(unix)]
pub fn disk_total_mb(path: &Path) -> Option<u64> {
    let v = rustix::fs::statvfs(path).ok()?;
    Some(v.f_blocks.saturating_mul(v.f_frsize) / (1024 * 1024))
}

/// Free space available to an unprivileged writer under `path`, in MB.
#[cfg(unix)]
pub fn disk_free_mb(path: &Path) -> Option<u64> {
    let v = rustix::fs::statvfs(path).ok()?;
    Some(v.f_bavail.saturating_mul(v.f_frsize) / (1024 * 1024))
}

#[cfg(not(unix))]
pub fn disk_total_mb(_path: &Path) -> Option<u64> {
    None
}

#[cfg(not(unix))]
pub fn disk_free_mb(_path: &Path) -> Option<u64> {
    None
}

/// `some avgN` from a /proc/pressure/<resource> file, as a percent. cgroup
/// v2 PSI; `None` when the kernel does not expose it.
pub fn psi_some_avg10(resource: &str) -> Option<f64> {
    let text = std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok()?;
    let line = text.lines().find(|l| l.starts_with("some"))?;
    let field = line.split_whitespace().find(|f| f.starts_with("avg10="))?;
    field.trim_start_matches("avg10=").parse().ok()
}

/// 1-minute load average from /proc/loadavg.
pub fn load1() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_reports_this_machine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cap = detect(dir.path());
        assert!(cap.cores >= 1, "a running test has at least one thread");
        assert_eq!(cap.arch, std::env::consts::ARCH);
        if cfg!(target_os = "linux") {
            assert!(cap.mem_mb > 0, "linux exposes MemTotal");
            assert!(cap.disk_mb > 0, "the temp dir lives on a real filesystem");
        }
        // Round-trips through the tag form the mesh actually carries.
        assert_eq!(Capacity::from_tags(&cap.to_tags()), Some(cap));
    }

    /// The boot refresh keeps what a person put on a node and re-derives
    /// what the machine says about itself. `console` is the latter: it is
    /// Director's launch-time finding, so a recorded copy that the launch
    /// tags no longer carry is stale and must go — and one they do carry
    /// must stay.
    #[test]
    fn a_boot_refresh_re_decides_console() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("meshnode.toml");
        std::fs::write(&p, "").expect("write config");
        let load = |tags: &[&str]| {
            NodeConfig::load(
                Some(&p),
                &crate::config::Overrides {
                    tags: Some(tags.iter().map(|t| t.to_string()).collect()),
                    ..Default::default()
                },
            )
            .expect("load")
        };
        let recorded = ["role=director", "draining", "cap.cores=2"];

        // A tag the operator configured survives the re-announce; one
        // the node only ever announced about its own boot state does not.
        let out = refreshed_tags(&load(&["role=director"]), &[], &[], &recorded);
        assert!(out.iter().any(|t| t == "role=director"), "{out:?}");
        assert!(
            !out.iter().any(|t| t == mesh_proto::DRAINING_TAG),
            "{out:?}"
        );
    }

    /// The bug this exists to stop, found by reading the reload path (N5):
    /// the volume names were announced by `announced_tags_with_devices` and
    /// nowhere else, so the *re*-announce — which rebuilds the whole tag set
    /// from `Capacity::to_tags` — dropped them. The node kept running, kept
    /// holding whatever volumes it had already lent, and quietly stopped
    /// being a candidate for any new work that claimed one. Nothing failed;
    /// jobs just went somewhere else forever.
    ///
    /// The fix was to put the names *in* `Capacity`, which also makes the
    /// second half true: a `[[volume]]` edit is now a capacity change, so
    /// `SIGHUP` notices it at all.
    #[test]
    fn a_reload_cannot_lose_the_volumes_this_node_lends() {
        let fresh = Capacity {
            cores: 8,
            mem_mb: 1024,
            disk_mb: 2048,
            arch: "x86_64".to_string(),
            ports: None,
            volumes: vec!["data".to_string(), "models".to_string()],
            runtimes: Vec::new(),
        };
        let announced = with_capacity(&["linux".to_string()], &fresh, false);
        assert!(
            announced.iter().any(|t| t == "cap.volumes=data,models"),
            "a re-announce dropped the volume tag: {announced:?}"
        );

        // And it survives the round trip, so the *next* reload compares
        // like with like instead of re-announcing on every SIGHUP.
        let read_back = Capacity::from_tags(&announced).expect("capacity");
        assert_eq!(read_back.volumes, fresh.volumes);
        assert_eq!(read_back, fresh);
    }

    /// A volume list that moved is a capacity change, which is the only
    /// thing on the reload path that triggers a re-announce at all.
    #[test]
    fn changing_the_volume_list_is_a_capacity_change() {
        let before = Capacity {
            cores: 8,
            mem_mb: 1024,
            disk_mb: 2048,
            arch: "x86_64".to_string(),
            ports: None,
            volumes: vec!["data".to_string()],
            runtimes: Vec::new(),
        };
        let after = Capacity {
            volumes: vec!["data".to_string(), "scratch".to_string()],
            ..before.clone()
        };
        let tags = with_capacity(&[], &before, false);
        assert_ne!(Capacity::from_tags(&tags).as_ref(), Some(&after));

        // …and removing the last one too, which is the direction that
        // silently stops this node taking work it used to take.
        let none = Capacity {
            volumes: Vec::new(),
            runtimes: Vec::new(),
            ..before.clone()
        };
        assert_ne!(Capacity::from_tags(&tags).as_ref(), Some(&none));
    }

    #[test]
    fn capacity_tags_replace_not_accumulate() {
        let existing = vec![
            "linux".to_string(),
            "cap.cores=8".to_string(),
            "draining".to_string(),
        ];
        let fresh = Capacity {
            cores: 16,
            mem_mb: 0,
            disk_mb: 0,
            arch: String::new(),
            ports: None,
            volumes: Vec::new(),
            runtimes: Vec::new(),
        };
        let out = with_capacity(&existing, &fresh, false);
        assert_eq!(
            out,
            vec![
                "linux",
                "draining",
                "cap.cores=16",
                &mesh_proto::version_tag(VERSION)
            ]
        );

        // The build rides the same replacement, which is what makes a
        // fleet upgrade observable: a node that came back on a new binary
        // re-announces and the mesh sees it (M24 §4).
        assert_eq!(
            mesh_proto::version_of(&out).as_deref(),
            Some(VERSION),
            "a re-announcement must carry the build this node is running"
        );
        let stale = with_capacity(&["cap.ver=0.0.1".to_string()], &fresh, false);
        assert_eq!(
            mesh_proto::version_of(&stale).as_deref(),
            Some(VERSION),
            "an older version tag must be replaced, never accumulated"
        );

        // …and terminal acceptance is announced the same way, so a reload
        // that turns it on or off changes what the mesh is told.
        let on = with_capacity(&existing, &fresh, true);
        assert!(mesh_proto::accepts_terminals(&on), "{on:?}");
        assert!(!mesh_proto::accepts_terminals(&out), "{out:?}");
        // It is a `cap.` tag, so it is stripped from an operator's own tag
        // list rather than being something anyone can assert by hand.
        let forged = vec!["cap.term=1".to_string(), "web".to_string()];
        let clean = with_capacity(&forged, &fresh, false);
        assert!(!mesh_proto::accepts_terminals(&clean), "{clean:?}");
    }
}
