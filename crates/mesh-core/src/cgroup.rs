//! Per-job resource **enforcement** with cgroups v2 (E2.1).
//!
//! ## The bug this closes
//!
//! Before this, `meshnode run --cpus 2 --mem 4G` was an input to
//! *admission* and nothing else. The scheduler used it to decide where the
//! job fit; the executor then spawned a process group and the job could use
//! every core on the box and all of its memory. `jobs.rs` said so plainly
//! in a comment — *"meshware spawns a process group rather than a cgroup"*
//! — and MASTERPLAN §7.5 had promised "cgroups v2 + Landlock + seccomp"
//! since M0 while shipping only the middle one.
//!
//! That is a correctness problem before it is a security one. C1's fair
//! share and preemption are both computed from *reserved* resources,
//! and a principal who reserves two cores and uses sixty-four has taken
//! from everybody else while being charged for two. And one job with a
//! memory leak takes the node down with every other job on it.
//!
//! ## What it does
//!
//! One cgroup per job, under a subtree systemd delegated to the daemon:
//!
//! ```text
//!   <service cgroup>/                 ← delegated to us
//!     ├── daemon/                     ← the daemon moves itself here
//!     └── jobs/
//!         ├── job-7k1x8q/             ← cpu.max, memory.max, pids.max
//!         └── job-9c2m4b/
//! ```
//!
//! The daemon moving itself out is not optional and is the part that
//! surprises people: **a cgroup cannot both contain processes and enable
//! controllers for its children** (the "no internal process" rule). So the
//! daemon's own PID goes into `daemon/`, which frees the base to turn
//! `cpu`, `memory` and `pids` on for the job cgroups beside it.
//!
//! ## `memory.swap.max` is the line that makes `memory.max` mean anything
//!
//! Measured, because the first version of this was wrong: with
//! `memory.max = 64M` and swap available, a process allocating 512 MB
//! **succeeds** — the kernel swaps rather than OOM-kills, and the limit
//! silently becomes a performance hint. Setting `memory.swap.max = 0`
//! beside it is what turns the ceiling into a ceiling. A limit that does
//! not bite is worse than no limit, because somebody will rely on it.
//!
//! ## Degrading honestly
//!
//! Enforcement needs cgroup v2, a delegated writable subtree, and the
//! controllers actually available in it. A node in a container, under a
//! non-systemd init, or on a unit without `Delegate=yes` has none of that.
//!
//! **The rule is that the node says so rather than pretending.**
//! [`Delegation::probe`] returns why it cannot enforce, `meshnode doctor`
//! reports it, and `/metrics` carries `meshware_cgroup_enforcing`. What the
//! node must never do is accept `--mem 4G` while silently not enforcing it:
//! an operator reading the docs would believe a guarantee the machine is
//! not making.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use mesh_proto::Requirements;

/// Where the unified hierarchy is mounted. Fixed by the kernel ABI.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The controllers a job cgroup needs. `cpu` for `--cpus`, `memory` for
/// `--mem`, `pids` for a fork-bomb ceiling nobody asked for but everybody
/// wants the first time they need it.
const WANTED: [&str; 3] = ["cpu", "memory", "pids"];

/// CPU quota period, in microseconds. 100 ms is the kernel default and the
/// value every other scheduler uses; `--cpus 2` becomes `200000 100000`.
const CPU_PERIOD_US: u64 = 100_000;

/// Process ceiling for one job. Generous — a parallel build legitimately
/// forks hundreds — and finite, which is the point.
const PIDS_MAX: u64 = 4096;

/// The child cgroup the daemon moves itself into, so the base is free to
/// enable controllers for the job cgroups beside it.
const SUPERVISOR: &str = "daemon";

/// A delegated cgroup subtree this daemon may write in.
#[derive(Debug, Clone)]
pub struct Delegation {
    /// The base the daemon was placed in, e.g.
    /// `/sys/fs/cgroup/system.slice/meshnode.service`.
    base: PathBuf,
    /// `<base>/jobs`, where one cgroup per job is created.
    jobs: PathBuf,
    /// Which of [`WANTED`] the kernel actually offered here.
    enabled: Vec<String>,
}

impl Delegation {
    /// The directory one job's cgroup is created under, as a path a
    /// container runtime can be pointed at with `--cgroup-parent` (N4).
    ///
    /// Handed to the container plugin so a container's accounting lands in
    /// the same tree as everything else this node runs. Whether the plugin
    /// uses it is the plugin's business — and whether it *did* is what the
    /// `controls` block it declares is for.
    pub fn jobs_dir(&self) -> &Path {
        &self.jobs
    }

    /// This process's own cgroup path, from `/proc/self/cgroup`.
    ///
    /// The unified hierarchy is the line beginning `0::`. A v1 controller
    /// line may also be present on a hybrid host and is not ours.
    fn own_path() -> Option<PathBuf> {
        let text = fs::read_to_string("/proc/self/cgroup").ok()?;
        let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?;
        Some(PathBuf::from(CGROUP_ROOT).join(rel.trim_start_matches('/')))
    }

    /// Can this node enforce? `Err` carries the sentence `doctor` prints.
    ///
    /// Deliberately a probe with a *reason* rather than a bool: "cgroup
    /// enforcement is off" sends an operator nowhere, and "the unit has no
    /// Delegate=yes" sends them to a one-line fix.
    pub fn probe() -> std::result::Result<Delegation, String> {
        if !cfg!(target_os = "linux") {
            return Err("resource enforcement needs Linux (cgroups v2)".into());
        }
        if !Path::new(CGROUP_ROOT).join("cgroup.controllers").exists() {
            return Err(format!(
                "no cgroup v2 hierarchy at {CGROUP_ROOT} — this host is on cgroup v1 or has \
                 none, and declared CPU/memory limits cannot be enforced"
            ));
        }
        let mut base = Self::own_path().ok_or_else(|| {
            "could not read this process's cgroup from /proc/self/cgroup".to_string()
        })?;
        // **Idempotence.** `prepare` moves the daemon into `<base>/daemon`,
        // so a second probe — after a config reload, or on the retry path
        // when the first attempt failed — would otherwise take *that* as
        // the base and report a nonsense reason ("the subtree offers none
        // of cpu, memory, pids") about a cgroup we created ourselves. If we
        // are standing in our own supervisor directory, the base is its
        // parent.
        if base.file_name().is_some_and(|n| n == SUPERVISOR)
            && let Some(parent) = base.parent()
        {
            base = parent.to_path_buf();
        }
        if !base.exists() {
            return Err(format!(
                "this process's cgroup {} does not exist — running in a namespace whose \
                 hierarchy is not mounted here",
                base.display()
            ));
        }

        // The delegation test that matters is not "am I root" but "can I
        // create a child here", which is exactly what systemd's
        // `Delegate=yes` grants and what a container runtime may not.
        let probe_dir = base.join(".meshware-probe");
        let _ = fs::remove_dir(&probe_dir);
        if let Err(e) = fs::create_dir(&probe_dir) {
            return Err(format!(
                "cannot create a cgroup under {} ({e}) — the systemd unit needs `Delegate=yes` \
                 for meshware to enforce declared CPU and memory limits. Without it a job that \
                 asked for two cores can use every core on the box",
                base.display()
            ));
        }
        let _ = fs::remove_dir(&probe_dir);

        let available = fs::read_to_string(base.join("cgroup.controllers")).unwrap_or_default();
        let enabled: Vec<String> = WANTED
            .iter()
            .filter(|c| available.split_whitespace().any(|a| a == **c))
            .map(|c| c.to_string())
            .collect();
        if enabled.is_empty() {
            return Err(format!(
                "the delegated subtree {} offers none of {WANTED:?} — nothing to enforce with",
                base.display()
            ));
        }

        Ok(Delegation {
            jobs: base.join("jobs"),
            base,
            enabled,
        })
    }

    /// Move this process into `<base>/daemon` and turn the controllers on
    /// for the job cgroups beside it.
    ///
    /// **The move is required, not tidiness.** A cgroup cannot both hold
    /// processes and enable controllers for its children, so leaving the
    /// daemon in the base makes `cgroup.subtree_control` unwritable and
    /// every job limit silently unavailable.
    pub fn prepare(&self) -> std::result::Result<(), String> {
        let supervisor = self.base.join(SUPERVISOR);
        fs::create_dir_all(&supervisor)
            .map_err(|e| format!("cannot create {}: {e}", supervisor.display()))?;

        // **Every process, not just this one.** The no-internal-process
        // rule counts *all* of them: a base cgroup with one stray pid in it
        // refuses `cgroup.subtree_control` with EBUSY, and the first
        // version of this moved only itself and then failed with a message
        // that named the wrong problem.
        //
        // Under systemd with `Delegate=yes` the base holds the daemon and
        // nothing else, so this is one write. In a shared user session it
        // holds a shell and whatever else is running there, none of which
        // is ours to move — and that is a real refusal, reported below
        // rather than papered over.
        let procs = fs::read_to_string(self.base.join("cgroup.procs")).unwrap_or_default();
        let mut stuck = Vec::new();
        for pid in procs.split_whitespace() {
            if write_cgroup_file(&supervisor.join("cgroup.procs"), pid).is_err() {
                stuck.push(pid.to_string());
            }
        }
        // Ours, by name, whatever the file said — "0" is the writing
        // process and cannot be the wrong pid.
        write_cgroup_file(&supervisor.join("cgroup.procs"), "0")
            .map_err(|e| format!("cannot move the daemon into its own cgroup: {e}"))?;

        let want: String = self
            .enabled
            .iter()
            .map(|c| format!("+{c}"))
            .collect::<Vec<_>>()
            .join(" ");
        write_cgroup_file(&self.base.join("cgroup.subtree_control"), &want).map_err(|e| {
            if stuck.is_empty() {
                format!("cannot enable {want:?} on {}: {e}", self.base.display())
            } else {
                format!(
                    "cannot enable {want:?} on {}: {e} — {} process(es) in that cgroup could \
                     not be moved out of the way ({}), and a cgroup with processes in it cannot \
                     enable controllers for its children. This is normal in a shared login \
                     session and should not happen under a systemd unit with `Delegate=yes`, \
                     where the daemon is alone in its own cgroup",
                    self.base.display(),
                    stuck.len(),
                    stuck.join(", ")
                )
            }
        })?;

        fs::create_dir_all(&self.jobs)
            .map_err(|e| format!("cannot create {}: {e}", self.jobs.display()))?;
        // …and again one level down, so a job cgroup's own children (a
        // build that spawns a supervisor) stay inside the same ceiling.
        //
        // This one is not fatal — a job cgroup still gets its own limits
        // without it, only its grandchildren escape — but it is logged,
        // because "mostly enforced" is a state somebody needs to know about.
        if let Err(e) = write_cgroup_file(&self.jobs.join("cgroup.subtree_control"), &want) {
            warn!("could not enable {want:?} on {}: {e}", self.jobs.display());
        }

        info!(
            base = %self.base.display(),
            controllers = %self.enabled.join(","),
            "cgroup enforcement active — declared cpu/memory limits are real"
        );
        Ok(())
    }

    /// Where one job's cgroup lives, **without creating it** (Phase B, E3) —
    /// for reading what a running job uses. The same name [`Self::job`]
    /// creates, so a reader and the writer cannot look in different places.
    pub fn job_dir(&self, job_id: &str) -> PathBuf {
        self.jobs.join(sanitize(job_id))
    }

    pub fn controllers(&self) -> &[String] {
        &self.enabled
    }

    /// Create the cgroup for one job and write its limits.
    ///
    /// A requirement of zero means *unset*, and an unset limit is left
    /// unset rather than written as `max` — so a job that asked for nothing
    /// is bounded by the node, exactly as it was before this existed.
    pub fn job(&self, job_id: &str, reqs: &Requirements) -> Option<JobCgroup> {
        let dir = self.jobs.join(sanitize(job_id));
        if let Err(e) = fs::create_dir_all(&dir) {
            warn!(job = job_id, "cannot create job cgroup: {e}");
            return None;
        }

        // **Every failure here is reported.** The first version of this
        // swallowed them with `let _ =`, and the result was a node whose
        // `doctor` said "enforced" while nothing was — the single most
        // dishonest state this feature could be in, and undiagnosable from
        // the outside. A limit that could not be written is a limit the
        // operator has to hear about.
        let mut applied: Vec<&str> = Vec::new();
        let mut set = |file: &str, value: String| match write_cgroup_file(&dir.join(file), &value) {
            Ok(()) => applied.push(Box::leak(file.to_string().into_boxed_str())),
            Err(e) => warn!(
                job = job_id, file, %value,
                "could not write a job cgroup limit — it is NOT enforced for this job: {e}"
            ),
        };

        if self.has("cpu") && reqs.cpus > 0 {
            let quota = u64::from(reqs.cpus) * CPU_PERIOD_US;
            set("cpu.max", format!("{quota} {CPU_PERIOD_US}"));
        }
        if self.has("memory") && reqs.mem_mb > 0 {
            set(
                "memory.max",
                reqs.mem_mb.saturating_mul(1024 * 1024).to_string(),
            );
            // **Without this the ceiling is a suggestion.** Measured: with
            // swap available a process allocating eight times its
            // `memory.max` succeeds, because the kernel swaps instead of
            // OOM-killing. A limit that does not bite is worse than none.
            set("memory.swap.max", "0".to_string());
        }
        if self.has("pids") {
            set("pids.max", PIDS_MAX.to_string());
        }

        debug!(job = job_id, dir = %dir.display(), applied = ?applied, "job cgroup ready");
        Some(JobCgroup { dir })
    }

    fn has(&self, c: &str) -> bool {
        self.enabled.iter().any(|e| e == c)
    }

    /// Remove job cgroups left behind by a previous run of the daemon.
    ///
    /// A crash leaves empty directories; they cost nothing but they make
    /// `/sys/fs/cgroup` unreadable to a person debugging. Only empty ones
    /// go — `rmdir` on a cgroup with processes in it fails, which is the
    /// safety property we want and is why this does not check first.
    pub fn reap_orphans(&self) {
        let Ok(entries) = fs::read_dir(&self.jobs) else {
            return;
        };
        let mut reaped = 0;
        for e in entries.flatten() {
            if fs::remove_dir(e.path()).is_ok() {
                reaped += 1;
            }
        }
        if reaped > 0 {
            info!(reaped, "removed job cgroups left by a previous run");
        }
    }
}

/// One job's cgroup. Removed on drop.
#[derive(Debug)]
pub struct JobCgroup {
    dir: PathBuf,
}

impl JobCgroup {
    /// The path the sandbox shim writes its own PID into, before it applies
    /// Landlock and execs the job.
    ///
    /// **The shim joins, not the daemon**, and the ordering is the whole
    /// reason: a process moved into a cgroup *after* it starts has already
    /// had a window in which no limit applied. The shim joins first, then
    /// restricts the filesystem, then becomes the job — so the job is
    /// inside its ceiling from its first instruction.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Peak memory this job reached, in bytes, if the kernel tracked it.
    /// Read at exit for accounting, and `None` on a node not enforcing.
    pub fn peak_memory_bytes(&self) -> Option<u64> {
        fs::read_to_string(self.dir.join("memory.peak"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Kill everything in this cgroup, however it got there (ENG-8).
    ///
    /// `cgroup.kill` (Linux 5.14) reaches the processes a job started with
    /// `setsid` or a double fork, which the process-group signal does not.
    /// Returns whether the kernel took the request; `false` on an older
    /// kernel, where the group signal is all there is.
    pub fn kill(&self) -> bool {
        write_cgroup_file(&self.dir.join("cgroup.kill"), "1").is_ok()
    }

    /// Did the kernel OOM-kill anything in here?
    ///
    /// Worth surfacing rather than leaving a job to report a bare SIGKILL:
    /// "killed" and "asked for 512 MB and used more" are different problems
    /// with different fixes.
    pub fn was_oom_killed(&self) -> bool {
        fs::read_to_string(self.dir.join("memory.events"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("oom_kill "))
                    .and_then(|n| n.trim().parse::<u64>().ok())
            })
            .is_some_and(|n| n > 0)
    }
}

impl Drop for JobCgroup {
    fn drop(&mut self) {
        // Fails while anything is still in it, which is correct: a leftover
        // directory is visible and harmless, and forcing it would mean
        // killing processes from a destructor.
        if let Err(e) = fs::remove_dir(&self.dir) {
            debug!(dir = %self.dir.display(), "job cgroup not removed yet: {e}");
        }
    }
}

/// Write to a cgroup control file.
///
/// No trailing newline and one `write` call: cgroup files are parsed per
/// write, so a buffered writer that splits a value across two syscalls
/// produces `EINVAL` on the second half.
fn write_cgroup_file(path: &Path, value: &str) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new().write(true).open(path)?;
    f.write_all(value.as_bytes())
}

/// A job id is already `job-<base32>`, but this is a path segment and the
/// id arrives from the wire.
fn sanitize(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(96)
        .collect()
}

/// Join the cgroup at `dir` — called by the sandbox shim in the child,
/// before Landlock and before exec.
pub fn join(dir: &Path) -> std::io::Result<()> {
    write_cgroup_file(&dir.join("cgroup.procs"), "0")
}

// -- what *this* node is allowed to use (ENG-23) ----------------------------
//
// The other direction from everything above. The rest of this module puts a
// job in a cgroup; this reads the cgroup the *daemon* is already in, because
// on a node inside a container `/proc/meminfo` is the host's and is a lie —
// it is not namespaced, so a node with 2 GB advertises the machine's 512,
// takes work sized for it, and is OOM-killed. ENG-1 then restarts it, it
// advertises 512 GB again, and takes the same work.
//
// This is not only containers. `meshnode.service` with `MemoryMax=8G` is
// the same statement by another route, and an operator who wrote that line
// meant it as a bound on the node.

/// How much memory this process may use and is using, from the cgroup —
/// `(limit_mb, current_mb)`, or `None` when nothing limits it.
///
/// **Every ancestor, not just our own cgroup.** A limit is usually set on
/// the container's or the service's cgroup while the daemon sits in a child
/// of it, so reading one level finds `max` and concludes there is no limit.
/// The tightest bound on the path from our cgroup to the root is the one
/// that will actually kill this process.
///
/// `current` is read at the level the limit came from, because that is what
/// the limit is measured against: our own usage is not what a limit on a
/// parent counts.
pub fn memory_limit() -> Option<(u64, u64)> {
    let root = Path::new(CGROUP_ROOT);
    memory_limit_under(&Delegation::own_path()?, root)
        // cgroup v1, which Docker on an older host still puts a container
        // in. The rest of this module refuses v1 — it cannot *enforce*
        // there — but reading a limit is not enforcing one, and a node
        // that mis-advertises its memory is OOM-killed either way.
        .or_else(|| v1_memory_limit(root))
}

/// [`memory_limit`] against an arbitrary hierarchy, so the walk can be
/// tested against a tree on disk rather than the machine's own cgroups.
fn memory_limit_under(own: &Path, root: &Path) -> Option<(u64, u64)> {
    let mut tightest: Option<(u64, u64)> = None;
    let mut dir = own;
    loop {
        if let Some(limit) = read_bytes(&dir.join("memory.max")) {
            let current = read_bytes(&dir.join("memory.current")).unwrap_or(0);
            if tightest.is_none_or(|(t, _)| limit < t) {
                tightest = Some((limit, current));
            }
        }
        if dir == root {
            break;
        }
        match dir.parent() {
            // Never walk above the hierarchy root, whatever
            // `/proc/self/cgroup` said.
            Some(parent) if parent.starts_with(root) => dir = parent,
            _ => break,
        }
    }
    tightest.map(|(limit, current)| (to_mb(limit), to_mb(current)))
}

/// cgroup v1's answer, read at the mount the container's namespace shows.
fn v1_memory_limit(root: &Path) -> Option<(u64, u64)> {
    let dir = root.join("memory");
    let limit = read_bytes(&dir.join("memory.limit_in_bytes"))?;
    let current = read_bytes(&dir.join("memory.usage_in_bytes")).unwrap_or(0);
    Some((to_mb(limit), to_mb(current)))
}

/// Bytes past which a "limit" is the absence of one.
///
/// v2 writes the literal `max`, which does not parse and is `None`
/// already. v1 writes a sentinel near `u64::MAX` instead, and a limit
/// nobody could reach is not a limit. A petabyte is past any machine this
/// runs on and far below the sentinel.
const NO_LIMIT_BYTES: u64 = 1 << 50;

fn to_mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// A cgroup byte-valued file, or `None` when it says there is no limit.
/// v2's `max` does not parse; v1's sentinel is caught by
/// [`NO_LIMIT_BYTES`]. That distinction is what the whole of
/// [`memory_limit`] turns on: a *number* here is a bound this node will be
/// killed for exceeding, and anything else is the host's memory.
fn read_bytes(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    let bytes = text.trim().parse::<u64>().ok()?;
    (bytes < NO_LIMIT_BYTES).then_some(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Build a cgroup-shaped tree: `dir/<rel>/<file> = <contents>`.
    fn write_at(root: &Path, rel: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            fs::write(dir.join(name), body).unwrap();
        }
        dir
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The container case (ENG-23), and the reason the walk goes up: the
    /// limit is on the container's cgroup and the daemon sits in a child
    /// of it, so reading one level finds `max` and concludes there is no
    /// limit — which is how a node with 2 GB advertised the host's 512.
    #[test]
    fn a_limit_on_an_ancestor_is_the_one_that_will_kill_us() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_at(root, ".", &[("memory.max", "max")]);
        write_at(
            root,
            "container",
            &[
                ("memory.max", &(2 * GIB).to_string()),
                ("memory.current", &(GIB / 2).to_string()),
            ],
        );
        let own = write_at(root, "container/meshnode", &[("memory.max", "max")]);

        assert_eq!(memory_limit_under(&own, root), Some((2048, 512)));
    }

    /// The tightest bound wins, whichever level it is on: two limits on
    /// one path means the smaller is the one reached first.
    #[test]
    fn the_tightest_limit_on_the_path_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_at(root, ".", &[("memory.max", &(8 * GIB).to_string())]);
        write_at(root, "slice", &[("memory.max", &(4 * GIB).to_string())]);
        let own = write_at(
            root,
            "slice/meshnode",
            &[
                ("memory.max", &(6 * GIB).to_string()),
                ("memory.current", &GIB.to_string()),
            ],
        );
        assert_eq!(memory_limit_under(&own, root).map(|(l, _)| l), Some(4096));
    }

    /// An unlimited hierarchy is `None`, not a very large number: the
    /// caller takes a minimum with `/proc/meminfo`, and a bogus limit
    /// would silently become this node's advertised memory.
    #[test]
    fn no_limit_anywhere_reads_as_no_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_at(root, ".", &[("memory.max", "max")]);
        let own = write_at(root, "meshnode", &[("memory.max", "max")]);
        assert_eq!(memory_limit_under(&own, root), None);

        // cgroup v1 says the same thing with a sentinel near `u64::MAX`
        // instead of a word. A limit nobody could reach is not a limit.
        let v1 = write_at(root, "v1", &[("memory.max", "9223372036854771712")]);
        assert_eq!(memory_limit_under(&v1, root), None);
    }

    /// A cgroup with no `memory` controller at all — the file is simply
    /// absent — is not a limit either.
    #[test]
    fn a_hierarchy_without_the_memory_controller_is_not_a_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let own = write_at(root, "meshnode", &[("cpu.max", "max 100000")]);
        assert_eq!(memory_limit_under(&own, root), None);
    }

    /// The walk stops at the hierarchy root. `/proc/self/cgroup` is a file
    /// this process does not control on a host that has been tampered
    /// with, and reading `memory.max` up the whole filesystem would be a
    /// path traversal with extra steps.
    #[test]
    fn the_walk_never_climbs_above_the_hierarchy_root() {
        let tmp = tempfile::tempdir().unwrap();
        // A limit *outside* the hierarchy must not be found.
        write_at(tmp.path(), "outside", &[("memory.max", &GIB.to_string())]);
        let root = tmp.path().join("outside/hierarchy");
        let own = write_at(&root, "meshnode", &[("memory.max", "max")]);
        assert_eq!(memory_limit_under(&own, &root), None);
    }

    #[test]
    fn a_job_id_becomes_a_safe_path_segment() {
        assert_eq!(sanitize("job-7k1x8q"), "job-7k1x8q");
        assert_eq!(sanitize("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize("a/b"), "ab");
        assert_eq!(sanitize(""), "");
        assert_eq!(sanitize(&"x".repeat(200)).len(), 96);
    }

    #[test]
    fn a_cpu_quota_is_a_period_pair() {
        // `--cpus 2` must be two whole cores over the standard period, not
        // a percentage and not a share weight.
        assert_eq!(2 * CPU_PERIOD_US, 200_000);
        assert_eq!(CPU_PERIOD_US, 100_000);
    }

    #[test]
    fn probe_either_works_or_says_why() {
        // This runs on developer machines, in containers and in CI, and
        // must never panic in any of them. What it must do is give a
        // *reason* when it cannot enforce — an empty refusal sends an
        // operator nowhere.
        match Delegation::probe() {
            Ok(d) => {
                assert!(!d.controllers().is_empty());
                assert!(d.base.exists());
            }
            Err(why) => {
                assert!(!why.is_empty(), "a refusal must carry its reason");
                assert!(
                    why.len() > 20,
                    "the reason should be a sentence somebody can act on: {why}"
                );
            }
        }
    }
}
