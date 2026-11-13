//! Per-job filesystem sandboxing, unprivileged (MASTERPLAN §7.5, the
//! part that does not need root).
//!
//! Landlock is a Linux mechanism (5.13+) by which a process restricts
//! **itself**: it builds a ruleset naming the directories it may still
//! reach, applies it, and from then on neither it nor anything it execs
//! can open a path outside that set. No root, no namespaces, no daemon.
//! What it buys a node is the promise the executor could never make
//! before: a job writes inside its own directory, `/tmp`, and a handful of
//! places its tools need, and **nowhere else** — not the node's state, not
//! its content store, not another job's checkout — and on a system install
//! it cannot even read the node's identity key.
//!
//! ## The shim
//!
//! A ruleset restricts the process that applies it, so it has to be
//! applied in the child, after the fork and before the exec. `std`'s only
//! seam there is `pre_exec`, which is `unsafe`, and this workspace denies
//! `unsafe_code` on purpose. The PTY host solved the same problem the same
//! way (`sessions::pty_shim`): the child is a re-exec of this very binary
//! with [`SHIM_ARG`] first, which applies the ruleset as an ordinary
//! process in `main` and then `exec`s the real program — every step safe.
//! Every binary that hosts a node calls [`try_run`] first thing in `main`,
//! exactly as it calls the PTY shim.
//!
//! ## What a job may reach
//!
//! Read and execute: the system (`/usr`, `/etc`, `/opt`, …), the node's
//! Python environments, and the daemon's home directory **entry by entry,
//! minus the node's own** — `identity`, `state`, `mesh`, `content`, `jobs`
//! and the rest of [`crate::layout`]'s names — **and minus the credential
//! stores** ([`HOME_SECRETS`]: `.ssh`, `.gnupg`, `.aws`, …). Landlock is an
//! allowlist, so "the home except the key" is spelled as "each thing in the
//! home that is not the key", read from the directory at spawn time.
//!
//! ## What it still reaches, said plainly (S-M10)
//!
//! Two grants in [`SYSTEM_READ`] are wider than the summary above sounds,
//! and both are deliberate:
//!
//! - **`/proc`.** Landlock mediates paths, so a job can open
//!   `/proc/<daemon-pid>/environ` and `/proc/<pid>/fd/*` for any process of
//!   the same uid. There is no way to express "my own `/proc` only" in a
//!   Landlock ruleset — `/proc/self` is a per-pid symlink and the ruleset is
//!   fixed before the pid exists — and revoking `/proc` outright breaks
//!   ordinary programs (`/proc/self/exe`, `/proc/self/maps`, every runtime
//!   that reads its own limits). Per-job *user* separation is what actually
//!   closes this, and `SECURITY.md` already discloses its absence.
//! - **`/run`.** `/etc/resolv.conf` is a symlink into it on every
//!   systemd-resolved machine, so removing it takes DNS away from jobs. The
//!   node's IPC socket lives there too — and a job reaching it is *by
//!   design*: that is how a task calls back into the mesh with its job
//!   credential. Landlock does not mediate `connect()` on a Unix socket in
//!   any ABI, so removing the path grant would cost DNS and buy nothing.
//!
//! There is also **no network confinement**: only `AccessFs` rules are
//! built, and Landlock's `AccessNet` (ABI 4+) is unused. Most jobs exist to
//! reach something, so it would have to be opt-in per service before it
//! could be switched on, and that is a feature rather than a fix.
//!
//! Write: the job's own directory (its checkout, its work dir, its secret
//! spool, and a `tmp` of its own that `TMPDIR` points at), `/dev` (for
//! `/dev/null` and `/dev/shm`), and the caches its tools insist on in
//! `$HOME` — `.cache`, `.ansible`, `.local/share`, `.config`. **Not
//! `/tmp`**: a node whose data lives under `/tmp` (every test harness, some
//! containers) would otherwise be writable by its own jobs, and every tool
//! that matters honours `TMPDIR`. Anything a deployment needs beyond that
//! is `[runtime] sandbox_read` / `sandbox_write` in `meshnode.toml`.
//!
//! ## Degrading
//!
//! `sandbox = "auto"` (the default) sandboxes when it can: Linux, a kernel
//! with Landlock, and a host binary that knows the shim. A kernel without
//! Landlock, a macOS Director, or a test harness runs jobs exactly as
//! before, and says so once in the log. `"on"` refuses to run a job it
//! cannot sandbox; `"off"` never tries. The child sees which it got in
//! `MESHWARE_SANDBOX` (`full`, `partial`, `none`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The argv[1] sentinel that turns this binary into the sandbox shim.
pub const SHIM_ARG: &str = "__meshware-sandbox";

/// Override for which binary the host re-execs as the shim (the same
/// reason `sessions::pty_shim::SHIM_ENV` exists: tests, and repackagings).
pub const SHIM_ENV: &str = "MESHWARE_SANDBOX_SHIM";

/// What the child is told about its own confinement.
pub const STATUS_ENV: &str = "MESHWARE_SANDBOX";

/// Whether to sandbox at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Sandbox when this machine can; run plainly, and say so, when not.
    #[default]
    Auto,
    /// Sandbox or refuse the job.
    On,
    /// Never.
    Off,
}

impl Mode {
    /// The word this mode is spelled with in `meshnode.toml`, so a message
    /// about it names the thing the operator would actually edit.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::On => "on",
            Mode::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Mode::Auto),
            "on" | "true" | "landlock" => Some(Mode::On),
            "off" | "false" | "none" => Some(Mode::Off),
            _ => None,
        }
    }
}

/// The resolved `[runtime] sandbox*` settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Policy {
    pub mode: Mode,
    /// Extra paths a job may read and execute from.
    pub read: Vec<PathBuf>,
    /// Extra paths a job may also write to.
    pub write: Vec<PathBuf>,
    /// The binary to re-exec as the shim, when it is not this process:
    /// a test harness pointing at a real `meshnode`. Never from a file.
    #[serde(skip)]
    pub shim: Option<PathBuf>,
}

/// System prefixes every job may read from and execute out of.
const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/bin",
    "/sbin",
    "/etc",
    "/opt",
    "/run",
    "/proc",
    "/sys",
    "/snap",
    "/nix",
    "/srv",
    "/mnt",
    "/media",
    "/var/cache",
    "/tmp",
    "/var/tmp",
];

/// Places every job may write. `/tmp` is deliberately absent — see the
/// module docs; the job gets a `TMPDIR` of its own instead.
const SYSTEM_WRITE: &[&str] = &["/dev"];

/// The job's own scratch directory, under its job dir.
const JOB_TMP: &str = "tmp";

/// Entries of the daemon's home a job must not see: the node's own.
///
/// Every directory [`crate::layout::Layout`] creates, plus the legacy
/// names from before the consolidation. A job that could read `identity`
/// could sign as the node; one that could write `state` could rewrite
/// history.
const NODE_OWNED: &[&str] = &[
    "identity",
    "mesh",
    "state",
    "content",
    "jobs",
    "objects",
    "history",
    "archive",
    "upgrade-staging",
    "secrets",
    "director",
    "meshnode.sock",
    "known_hosts",
    "data",
];

/// `$HOME` subdirectories a job's tools write to, created if missing so
/// the first Ansible or pip run on a fresh node does not fail on `mkdir`.
const HOME_WRITE: &[&str] = &[".cache", ".ansible", ".local/share", ".config"];

/// Entries of the daemon's home a job must not see because of what is in
/// them, rather than because they are the node's (S-M10).
///
/// `home_entries` grants "everything in the home that is not ours", and
/// `~/.ssh` was one of the things it granted — a test asserted it. On a
/// fleet-management node those are frequently the keys that reach the rest
/// of the estate, which makes "a job cannot read the node's identity" true
/// and beside the point.
///
/// A denylist inside an allowlist, and that is the right shape here: the
/// home is *someone's home*, its contents are not enumerable in advance, and
/// the alternative — name every directory a job may read — is a rule nobody
/// can keep current. These are the well-known credential stores; anything
/// else worth hiding is `NODE_OWNED`, a different account, or
/// `[runtime] sandbox_read` deliberately widened.
const HOME_SECRETS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".password-store",
    ".azure",
    ".terraformrc",
];

/// Extra paths one specific job may reach, derived from what its service
/// declared it reads and writes (R4 P1).
///
/// **Why this exists.** Getting a job's output onto shared storage used to
/// mean adding the directory to `[runtime] sandbox_write` in
/// `meshnode.toml` **on every node**, then `SIGHUP`ing each one. That is
/// three of the thirteen steps it took to run one script, it is the only
/// part that scales with the size of your mesh, and it grants far more than
/// the job needs: every job on every node gets permanent write access to the
/// whole directory, forever, so that one run could write one file.
///
/// A grant derived per job is *narrower* than the workaround it replaces.
///
/// **Where they come from, and why not from the job's environment.** These
/// are resolved from the **service definition** — signed by whoever
/// published it, replicated, audited — and never from anything the
/// submitter attaches to a single run. A submitter who could name their own
/// grants could ask for `~/.ssh` and the sandbox would hand it over, which
/// would make it decorative.
///
/// **The volume fields are a different kind of grant** and are kept apart
/// for that reason. `read`/`write` are derived from a *service author's*
/// declaration and a *submitter's* argument, so they are filtered by
/// [`grantable`]. `leased_read`/`leased_write` are directories **this
/// node's own operator** declared lendable in their own `meshnode.toml`
/// and which this job has already been handed an exclusive lease on — the
/// same authority as `[runtime] sandbox_write`, and taken as written for
/// the same reason. Nothing a submitter sends can put a path in them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    /// Volumes leased to this job read-only (N5).
    pub leased_read: Vec<PathBuf>,
    /// Volumes leased to this job for writing (N5).
    pub leased_write: Vec<PathBuf>,
}

impl Grants {
    pub fn is_empty(&self) -> bool {
        self.read.is_empty()
            && self.write.is_empty()
            && self.leased_read.is_empty()
            && self.leased_write.is_empty()
    }
}

/// Whether a derived grant may cover this path at all.
///
/// The service author chooses *which argument* is a path; the submitter
/// chooses *which* path; this chooses what is never grantable however the
/// other two are set. Two things are refused, and they are the two the
/// unprivileged sandbox exists to protect:
///
///   * the node's own directory — identity, op-log, content, other jobs;
///   * the credential stores under the daemon's home ([`HOME_SECRETS`]),
///     which `home_entries` already carves out of the ordinary grant. A
///     derived grant that could put `~/.ssh` back would quietly undo S-M10.
///
/// A refusal is not an error: the path is dropped and the job runs without
/// it, failing on its own terms if it truly needed it. Refusing to *start*
/// would turn a tightening of the sandbox into an outage.
pub fn grantable(path: &Path, data_dir: &Path, home: Option<&Path>) -> bool {
    // Relative paths cannot be reasoned about here — the job's working
    // directory is not this process's — so they are never granted.
    if !path.is_absolute() {
        return false;
    }
    if path.starts_with(data_dir) || data_dir.starts_with(path) {
        return false;
    }
    if let Some(home) = home {
        for secret in HOME_SECRETS {
            let p = home.join(secret);
            if path.starts_with(&p) || p.starts_with(path) {
                return false;
            }
        }
        // A grant of the home itself would sweep the credential stores back
        // in wholesale.
        if path == home {
            return false;
        }
    }
    // `/` would grant everything, and is always a mistake rather than an
    // intent.
    path.parent().is_some()
}

/// Confine the calling **thread** to reading `read` and writing `write`
/// (SEC-3). Returns what [`try_run`] would tell a job: `full`, `partial` or
/// `none`.
///
/// Landlock restricts the task that applies it, and on Linux a task is a
/// thread, so the rest of the daemon is untouched. This is for the one kind
/// of work the daemon does with a path a caller chose — reading a tree into
/// the content store — done on a thread of its own that ends with the work.
/// **Never call it on a runtime worker**: the restriction cannot be lifted.
pub fn confine_this_thread(read: &[PathBuf], write: &[PathBuf]) -> &'static str {
    let read: Vec<PathBuf> = read.iter().filter(|p| p.exists()).cloned().collect();
    let write: Vec<PathBuf> = write.iter().filter(|p| p.exists()).cloned().collect();
    restrict(&read, &write)
}

/// The credential stores under `home` — what a job never reads, and what
/// the daemon will not read for anybody (SEC-3).
pub fn home_secrets(home: &Path) -> Vec<PathBuf> {
    HOME_SECRETS.iter().map(|s| home.join(s)).collect()
}

/// What `git` must not see when the daemon runs it for a watched source
/// (SEC-21): the node's own directory — or, where that directory is also
/// the account's home (a service install), the node's entries in it, so
/// the account's SSH keys and git configuration stay where git and ssh
/// look for them. Fetching a private repository needs exactly those.
pub fn node_dirs(data_dir: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let data = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    match home {
        Some(home) if home_is_data_dir(home, &data) => {
            NODE_OWNED.iter().map(|n| data.join(n)).collect()
        }
        _ => vec![data],
    }
}

/// Places a confined helper may always write: `/dev` (`/dev/null`, a tty
/// it will not get). The rest it is given by name.
pub fn system_write() -> Vec<PathBuf> {
    SYSTEM_WRITE.iter().map(PathBuf::from).collect()
}

/// Everything on this machine except `hidden` and what lies beneath it, as
/// a Landlock allowlist (SEC-3).
///
/// Landlock can only grant, so "all but the node's own directory" is spelled
/// the way [`home_entries`] spells "the home but the key": walk from `/`
/// down each hidden path's ancestors and grant every sibling on the way.
/// Entries are judged by where they lead, so a symlink into a hidden tree —
/// or onto one of its ancestors — is left out rather than granted.
pub fn everything_but(hidden: &[PathBuf]) -> Vec<PathBuf> {
    let hidden: Vec<PathBuf> = hidden
        .iter()
        .map(|h| std::fs::canonicalize(h).unwrap_or_else(|_| h.clone()))
        .collect();
    let mut out = Vec::new();
    grant_around(Path::new("/"), &hidden, &mut out, 0);
    out
}

fn grant_around(dir: &Path, hidden: &[PathBuf], out: &mut Vec<PathBuf>, depth: usize) {
    // Deeper than any real path; a loop would be the only way to get here.
    if depth > 64 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let inside = |p: &Path| hidden.iter().any(|h| p.starts_with(h));
    let above = |p: &Path| hidden.iter().any(|h| h.starts_with(p));
    for entry in entries.flatten() {
        let path = entry.path();
        let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if inside(&path) || inside(&real) {
            continue;
        }
        if above(&path) || above(&real) {
            // An ancestor of something hidden: grant its other children
            // instead. Only a real directory is walked; a link onto an
            // ancestor is dropped whole, and the real one is reached from
            // `/` in its own right.
            if real == path && entry.file_type().is_ok_and(|t| t.is_dir()) {
                grant_around(&path, hidden, out, depth + 1);
            }
            continue;
        }
        out.push(path);
    }
}

/// The paths one job may reach, computed in the parent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
}

impl Rules {
    /// What a job in `job_dir`, on a node whose data lives in `data_dir`
    /// and whose account's home is `home`, may reach.
    pub fn for_job(
        policy: &Policy,
        job_dir: &Path,
        data_dir: &Path,
        home: Option<&Path>,
        grants: &Grants,
    ) -> Rules {
        let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
        let mut write: Vec<PathBuf> = SYSTEM_WRITE.iter().map(PathBuf::from).collect();
        write.push(job_dir.to_path_buf());
        let _ = std::fs::create_dir_all(job_dir.join(JOB_TMP));
        // The node's Python environments: executed, never written — the
        // daemon built them at prepare time.
        read.push(data_dir.join("envs"));
        if let Some(home) = home {
            read.extend(home_entries(home, data_dir));
            for sub in HOME_WRITE {
                let p = home.join(sub);
                let _ = std::fs::create_dir_all(&p);
                write.push(p);
            }
        }
        read.extend(policy.read.iter().cloned());
        write.extend(policy.write.iter().cloned());
        // N5 — the volumes this job holds a lease on.
        //
        // Beside the operator's standing policy rather than below with the
        // derived grants, and unfiltered for the same reason: the path came
        // from a `[[volume]]` block in this machine's own config, and the
        // lease was taken in this node's own ledger before the job started.
        //
        // **Without this the claim is a lie for the one runtime most people
        // try first.** A container gets a real bind mount from its plugin,
        // so it never noticed; a bash job gets the host path in
        // `MESHWARE_VOLUME_<NAME>` and — until this line — a ruleset that
        // did not include it. `echo > $MESHWARE_VOLUME_DATA/f` answered
        // `Permission denied` on a volume the ledger said it was writing.
        read.extend(grants.leased_read.iter().cloned());
        write.extend(grants.leased_write.iter().cloned());
        // Derived last, and filtered: standing policy is an operator's
        // decision and is taken as written, a derived grant is not.
        read.extend(
            grants
                .read
                .iter()
                .filter(|p| grantable(p, data_dir, home))
                .cloned(),
        );
        write.extend(
            grants
                .write
                .iter()
                .filter(|p| grantable(p, data_dir, home))
                .cloned(),
        );
        // Only what exists: a rule for a missing path is an error in the
        // kernel and a no-op in intent.
        read.retain(|p| p.exists());
        write.retain(|p| p.exists());
        read.sort();
        read.dedup();
        write.sort();
        write.dedup();
        Rules { read, write }
    }

    /// The environment a sandboxed job gets on top of its own: a private
    /// `TMPDIR` (and its spellings) inside the job directory.
    pub fn env(job_dir: &Path) -> Vec<(String, String)> {
        let tmp = job_dir.join(JOB_TMP).to_string_lossy().into_owned();
        vec![
            ("TMPDIR".into(), tmp.clone()),
            ("TMP".into(), tmp.clone()),
            ("TEMP".into(), tmp),
        ]
    }

    /// The shim's argv: `__meshware-sandbox [--cgroup D] [--ro P]… [--rw P]… -- program args…`.
    pub fn argv(&self, program: &str, args: &[String]) -> Vec<OsString> {
        self.argv_in(program, args, None)
    }

    /// [`Rules::argv`], with a cgroup for the shim to join first (E2.1).
    pub fn argv_in(
        &self,
        program: &str,
        args: &[String],
        cgroup: Option<&std::path::Path>,
    ) -> Vec<OsString> {
        let mut out: Vec<OsString> = vec![SHIM_ARG.into()];
        if let Some(dir) = cgroup {
            out.push("--cgroup".into());
            out.push(dir.as_os_str().to_os_string());
        }
        for p in &self.read {
            out.push("--ro".into());
            out.push(p.clone().into_os_string());
        }
        for p in &self.write {
            out.push("--rw".into());
            out.push(p.clone().into_os_string());
        }
        out.push("--".into());
        out.push(program.into());
        out.extend(args.iter().map(OsString::from));
        out
    }
}

/// Everything in `home` that is not the node's own, as individual rules.
///
/// The home *is* the data directory on a service install (`HOME` for the
/// `meshnode` account is `/var/lib/meshware`), and contains it on a user
/// install (`~/.meshware`). Either way the answer is the same: grant each
/// entry that is not one of ours.
fn home_entries(home: &Path, data_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let data_dir = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        // The data directory itself (a user install's `~/.meshware`), and
        // on a service install — where home == data dir — its contents.
        if canon == data_dir
            || (home_is_data_dir(home, &data_dir) && NODE_OWNED.contains(&name.as_str()))
        {
            continue;
        }
        // And the credential stores, wherever the home is (S-M10).
        if HOME_SECRETS.contains(&name.as_str()) {
            continue;
        }
        out.push(path);
    }
    out
}

fn home_is_data_dir(home: &Path, data_dir: &Path) -> bool {
    std::fs::canonicalize(home)
        .map(|h| h == data_dir)
        .unwrap_or(false)
}

/// Why this machine cannot sandbox, in one sentence — or `None`.
pub fn unavailable_reason(policy: &Policy) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("job sandboxing needs Linux (Landlock)".into());
    }
    if !host_knows_shim(policy) {
        return Some(
            "this process cannot re-exec itself as the sandbox shim (set MESHWARE_SANDBOX_SHIM \
             to a meshnode binary)"
                .into(),
        );
    }
    kernel_reason()
}

#[cfg(target_os = "linux")]
fn kernel_reason() -> Option<String> {
    use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
    // The crate exposes no "which ABI is this kernel" probe; asking for
    // the first ABI as a hard requirement and seeing whether a ruleset can
    // be created is the same question. The fd it makes is dropped here.
    let probe = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|r| r.create());
    match probe {
        Ok(_) => None,
        Err(e) => Some(format!(
            "this kernel has no Landlock ({e}; needs 5.13+ with CONFIG_SECURITY_LANDLOCK and \
             `landlock` in lsm=)"
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn kernel_reason() -> Option<String> {
    Some("job sandboxing needs Linux (Landlock)".into())
}

/// Can the current process be re-executed as the shim? The binaries that
/// call [`try_run`] can; a test harness cannot, unless it points
/// [`SHIM_ENV`] at one that does.
fn host_knows_shim(policy: &Policy) -> bool {
    if policy.shim.is_some() || std::env::var_os(SHIM_ENV).is_some() {
        return true;
    }
    self_exe_name().is_some_and(|n| is_shim_host_name(&n))
}

/// What Linux appends to `/proc/self/exe` once the running binary's file
/// has been replaced or removed.
const DELETED_SUFFIX: &str = " (deleted)";

/// This binary's file name, as it was before any in-place swap (ENG-10).
///
/// After `upgrade --self`, a fleet upgrade, or a package upgrade that has
/// not restarted the daemon yet, the kernel reports the running image as
/// `meshnode (deleted)`. Matched as it stood, that name was not a shim host,
/// so under `sandbox = "auto"` every job quietly ran unsandboxed until the
/// restart — and under `"on"` every job failed.
fn self_exe_name() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some(undeleted(&name).to_string())
}

/// A file name as it was before its file was swapped out.
fn undeleted(name: &str) -> &str {
    name.strip_suffix(DELETED_SUFFIX).unwrap_or(name)
}

/// This process's own binary, as something that can be executed now
/// (ENG-10).
///
/// The resolved path while it still exists. Once the file has been swapped
/// out from under the running daemon, `/proc/<pid>/exe`: a link the kernel
/// keeps pointing at the image that is running, whether or not it still has
/// a name — so a shim re-executed through it is the same code as the daemon
/// that spawned it, which is what its arguments were written for.
pub fn self_exe() -> Option<PathBuf> {
    match std::env::current_exe() {
        Ok(path) if path.is_file() => Some(path),
        _ => {
            let magic = PathBuf::from(format!("/proc/{}/exe", std::process::id()));
            magic.exists().then_some(magic)
        }
    }
}

/// Whether a binary with this file name is one that calls [`try_run`].
///
/// The exact-match version of this was a silent security hole. Releases ship
/// as `meshnode-v0.7.6-x86_64-unknown-linux-musl`, and a node run straight
/// from that artefact — which is what the install instructions produce, and
/// what a person testing a release does — matched none of the names here.
/// `sandbox = "auto"` then degraded to "no sandbox" for every job on that
/// node, while `doctor` reported Landlock as available. Nothing was logged
/// at a level anyone would see.
///
/// The binary is the same binary whatever it has been renamed to: it knows
/// [`SHIM_ARG`], so it can host the shim. Matching the stem rather than the
/// whole name is what makes that true in practice. A test harness is still
/// not a host, because a test binary is named after its test.
fn is_shim_host_name(name: &str) -> bool {
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    stem == "meshnode"
        || stem == "meshware-director"
        || stem.starts_with("meshnode-")
        || stem.starts_with("meshware-director-")
}

/// The binary to re-exec as the shim.
pub fn shim_binary(policy: &Policy) -> Option<PathBuf> {
    if let Some(p) = &policy.shim {
        return Some(p.clone());
    }
    match std::env::var_os(SHIM_ENV) {
        Some(p) => Some(PathBuf::from(p)),
        None => self_exe(),
    }
}

/// Does a job spawned under `policy` run inside the shim? The same
/// decision [`wrap_granted`] makes, for a caller that needs to know what a
/// job's processes can reach without spawning one (SEC-3).
pub fn applies(policy: &Policy) -> bool {
    !matches!(policy.mode, Mode::Off)
        && unavailable_reason(policy).is_none()
        && shim_binary(policy).is_some()
}

/// What to spawn for a sandboxed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    /// The shim binary.
    pub program: PathBuf,
    /// Its arguments, ending in the real program and arguments.
    pub args: Vec<OsString>,
    /// Environment the job gets on top of its own (`TMPDIR`).
    pub env: Vec<(String, String)>,
}

/// The program and arguments to actually spawn for a job, given what it
/// was going to run.
///
/// `Ok(None)` means run it plainly: the policy is off, or auto on a
/// machine that cannot. `Err` is `on` on such a machine.
pub fn wrap(
    policy: &Policy,
    job_dir: &Path,
    data_dir: &Path,
    program: &str,
    args: &[String],
) -> Result<Option<Wrapped>, String> {
    wrap_in(policy, job_dir, data_dir, program, args, None)
}

/// [`wrap`], additionally handing the shim a cgroup to join first (E2.1).
///
/// When this returns `None` — sandboxing off or unavailable — there is no
/// shim, and the caller is responsible for moving the spawned child into
/// the cgroup itself. That path has a small window in which the job is
/// running unconstrained; the shim path does not, which is why it is the
/// one that matters.
pub fn wrap_in(
    policy: &Policy,
    job_dir: &Path,
    data_dir: &Path,
    program: &str,
    args: &[String],
    cgroup: Option<&Path>,
) -> Result<Option<Wrapped>, String> {
    wrap_granted(
        policy,
        job_dir,
        data_dir,
        program,
        args,
        cgroup,
        &Grants::default(),
    )
}

/// [`wrap_in`], plus the paths this job's service declared it needs (R4 P1).
#[allow(clippy::too_many_arguments)]
pub fn wrap_granted(
    policy: &Policy,
    job_dir: &Path,
    data_dir: &Path,
    program: &str,
    args: &[String],
    cgroup: Option<&Path>,
    grants: &Grants,
) -> Result<Option<Wrapped>, String> {
    match policy.mode {
        Mode::Off => return Ok(None),
        Mode::Auto | Mode::On => {}
    }
    if let Some(why) = unavailable_reason(policy) {
        return match policy.mode {
            Mode::On => Err(format!(
                "[runtime] sandbox = \"on\", but {why} — set it to \"auto\" to run jobs unsandboxed \
                 here"
            )),
            _ => Ok(None),
        };
    }
    let Some(shim) = shim_binary(policy) else {
        return Ok(None);
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let rules = Rules::for_job(policy, job_dir, data_dir, home.as_deref(), grants);
    Ok(Some(Wrapped {
        program: shim,
        args: rules.argv_in(program, args, cgroup),
        env: Rules::env(job_dir),
    }))
}

/// If this process was launched as the sandbox shim, confine itself and
/// become the job. Otherwise return immediately.
///
/// Call it as one of the first statements of `main`, beside the PTY shim.
/// Drop every capability in this process's ambient set.
///
/// Called first thing in `main`, before any thread or child exists, because
/// capabilities are per thread and are copied into each thread at its
/// creation. A unit that grants the node a capability through systemd's
/// `AmbientCapabilities=` — the retired mesh proxy's `proxy.conf` did, for
/// `CAP_NET_BIND_SERVICE`, and a machine not yet re-deployed still has it —
/// grants it in the ambient set, which is precisely what survives `exec`:
/// without this, every job, shell and container engine the node starts
/// could bind any low port. Clearing it leaves the capability where it was
/// granted, in this process's permitted and effective sets, and out of
/// everything the process runs.
#[cfg(target_os = "linux")]
pub fn drop_ambient_capabilities() {
    // A kernel too old to have ambient capabilities has nothing to clear.
    let _ = rustix::thread::clear_ambient_capability_set();
}

#[cfg(not(target_os = "linux"))]
pub fn drop_ambient_capabilities() {}

pub fn try_run() {
    let mut args = std::env::args_os();
    let _exe = args.next();
    match args.next() {
        Some(first) if first == SHIM_ARG => {}
        _ => return,
    }
    let mut read = Vec::new();
    let mut write = Vec::new();
    let mut cgroup: Option<PathBuf> = None;
    let mut program: Option<OsString> = None;
    let mut rest: Vec<OsString> = Vec::new();
    while let Some(a) = args.next() {
        if a == "--cgroup" {
            if let Some(p) = args.next() {
                cgroup = Some(PathBuf::from(p));
            }
        } else if a == "--ro" {
            if let Some(p) = args.next() {
                read.push(PathBuf::from(p));
            }
        } else if a == "--rw" {
            if let Some(p) = args.next() {
                write.push(PathBuf::from(p));
            }
        } else if a == "--" {
            program = args.next();
            rest = args.collect();
            break;
        } else {
            eprintln!("meshware: {SHIM_ARG}: unexpected argument {a:?}");
            std::process::exit(127);
        }
    }
    let Some(program) = program else {
        eprintln!("meshware: {SHIM_ARG} takes `-- program args…`");
        std::process::exit(127);
    };
    // **Order is the point** (E2.1). Join the cgroup first: a process moved
    // into one after it starts has already had a window in which no limit
    // applied. Then restrict the filesystem — Landlock would otherwise
    // block the write to `cgroup.procs`. Then become the job, which is
    // therefore inside its ceiling from its first instruction.
    if let Some(dir) = &cgroup
        && let Err(e) = crate::cgroup::join(dir)
    {
        // Not fatal. A job that runs unconstrained is worse than one that
        // runs constrained and better than one that does not run at all —
        // and the daemon already logged whether this node enforces.
        eprintln!("meshware: could not join the job cgroup ({e}); running unconstrained");
    }
    // **No-new-privs, set here rather than inherited from the unit.**
    //
    // It used to come only from `NoNewPrivileges=true` in
    // `meshnode.service`, which quietly made that directive load-bearing
    // for every job — and therefore impossible to relax for the one thing
    // that needs it relaxed. Rootless podman execs `newuidmap`, whose
    // entire purpose is to gain `CAP_SETUID` from a file capability, and
    // the kernel ignores file capabilities under no-new-privs. So a node
    // hardened that way could never run a container: `getcap` showed the
    // capability present and every `podman run` failed regardless.
    //
    // Setting it in the shim puts the protection where it belongs — on the
    // job — so a unit relaxed to let podman run directly does not take it
    // from any sandboxed job. (The packaged unit no longer is: containers
    // are made by the account's podman service — `engines.rs`, *Where
    // containers are made*.) A container never reaches this code: it
    // bypasses the shim entirely (`jobs.rs`, `via_plugin`), which is the
    // same reason Landlock does not reach it and is documented as such.
    //
    // After the cgroup join, before `restrict`: it costs one syscall and
    // cannot fail for lack of privilege — setting it is always permitted —
    // so an error here means a kernel that does not know the option, and a
    // job that runs without it is still a job that runs.
    if let Err(e) = rustix::thread::set_no_new_privs(true) {
        eprintln!("meshware: could not set no-new-privs for this job ({e})");
    }
    let status = restrict(&read, &write);
    exec(program, rest, status)
}

/// Apply the ruleset to this process. Returns what the child is told.
#[cfg(target_os = "linux")]
fn restrict(read: &[PathBuf], write: &[PathBuf]) -> &'static str {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, path_beneath_rules,
    };
    // The newest ABI this code knows; best-effort compatibility narrows it
    // to what the running kernel has and reports `PartiallyEnforced`.
    let abi = ABI::V5;
    let built = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(read, AccessFs::from_read(abi))))
        .and_then(|r| r.add_rules(path_beneath_rules(write, AccessFs::from_all(abi))))
        .and_then(|r| r.restrict_self());
    match built {
        Ok(status) => match status.ruleset {
            RulesetStatus::FullyEnforced => "full",
            RulesetStatus::PartiallyEnforced => "partial",
            RulesetStatus::NotEnforced => "none",
        },
        Err(e) => {
            // Refusing to run the job would turn a kernel quirk into an
            // outage; the daemon chose `auto`, and the child is told.
            eprintln!("meshware: sandbox not applied: {e}");
            "none"
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn restrict(_read: &[PathBuf], _write: &[PathBuf]) -> &'static str {
    "none"
}

#[cfg(unix)]
fn exec(program: OsString, args: Vec<OsString>, status: &'static str) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&program)
        .args(&args)
        .env(STATUS_ENV, status)
        .exec();
    eprintln!(
        "meshware: cannot run {}: {err}",
        Path::new(&program).display()
    );
    std::process::exit(127);
}

#[cfg(not(unix))]
fn exec(program: OsString, args: Vec<OsString>, status: &'static str) -> ! {
    let code = std::process::Command::new(&program)
        .args(&args)
        .env(STATUS_ENV, status)
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(127);
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse("ON"), Some(Mode::On));
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse("maybe"), None);
    }

    #[test]
    fn a_job_may_write_its_own_directory_and_read_the_system_but_not_the_node() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A service install: home == data dir, with the node's own entries
        // beside things a job's tools want.
        let home = tmp.path();
        for d in [
            "identity", "state", "mesh", "jobs", ".ssh", "envs", "scripts",
        ] {
            std::fs::create_dir_all(home.join(d)).expect("mkdir");
        }
        std::fs::write(home.join("notes.txt"), "x").expect("write");
        let job = home.join("jobs").join("job-1");
        std::fs::create_dir_all(&job).expect("mkdir");

        let rules = Rules::for_job(
            &Policy::default(),
            &job,
            home,
            Some(home),
            &Grants::default(),
        );
        let has = |v: &[PathBuf], p: &Path| v.iter().any(|x| x == p);
        assert!(has(&rules.write, &job));
        assert!(
            !has(&rules.write, Path::new("/tmp")),
            "/tmp is never writable"
        );
        assert!(has(&rules.read, Path::new("/tmp")));
        assert!(job.join(JOB_TMP).is_dir(), "the job's own scratch exists");
        assert!(
            Rules::env(&job)
                .iter()
                .any(|(k, v)| k == "TMPDIR" && v.ends_with("/tmp"))
        );
        assert!(has(&rules.write, &home.join(".cache")), "created on demand");
        // S-M10: the credential stores are not the node's, and a job may
        // not read them either. On a fleet-management node `~/.ssh` is
        // frequently the keys that reach the rest of the estate.
        for secret in HOME_SECRETS {
            assert!(
                !has(&rules.read, &home.join(secret)),
                "{secret} must not be readable"
            );
            assert!(!has(&rules.write, &home.join(secret)));
        }
        assert!(has(&rules.read, &home.join("scripts")));
        assert!(has(&rules.read, &home.join("notes.txt")));
        assert!(has(&rules.read, &home.join("envs")));
        for owned in ["identity", "state", "mesh", "jobs"] {
            assert!(
                !has(&rules.read, &home.join(owned)),
                "{owned} must not be readable"
            );
            assert!(!has(&rules.write, &home.join(owned)));
        }
        // No rule for a path that does not exist on this box.
        assert!(rules.read.iter().all(|p| p.exists()));
    }

    #[test]
    fn a_user_install_hides_the_data_directory_and_the_credentials() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let data = home.join(".meshware");
        for d in [".meshware/identity", ".meshware/jobs/j", "projects", ".ssh"] {
            std::fs::create_dir_all(home.join(d)).expect("mkdir");
        }
        let job = data.join("jobs").join("j");
        let rules = Rules::for_job(
            &Policy::default(),
            &job,
            &data,
            Some(home),
            &Grants::default(),
        );
        let has = |v: &[PathBuf], p: &Path| v.iter().any(|x| x == p);
        assert!(has(&rules.read, &home.join("projects")));
        assert!(
            !has(&rules.read, &home.join(".ssh")),
            "a user install hides the person's SSH keys too (S-M10)"
        );
        assert!(
            !has(&rules.read, &data),
            "the data dir is hidden whichever install this is"
        );
        assert!(has(&rules.write, &job));
    }

    #[test]
    fn the_shim_argv_carries_every_rule_then_the_program() {
        let rules = Rules {
            read: vec![PathBuf::from("/usr")],
            write: vec![PathBuf::from("/tmp")],
        };
        let argv = rules.argv("bash", &["-c".into(), "echo hi".into()]);
        let strs: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            strs,
            [
                SHIM_ARG, "--ro", "/usr", "--rw", "/tmp", "--", "bash", "-c", "echo hi"
            ]
        );
    }

    #[test]
    fn a_derived_grant_can_never_reach_the_node_or_the_credentials() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let data = home.join(".meshware");
        std::fs::create_dir_all(data.join("identity")).expect("mk");
        std::fs::create_dir_all(home.join(".ssh")).expect("mk");
        std::fs::create_dir_all(home.join("Videos")).expect("mk");

        let ok = |p: &str| grantable(Path::new(p), &data, Some(home));
        // The whole point: a shared output directory becomes grantable
        // without anybody editing meshnode.toml on every node.
        assert!(ok("/srv/media/encoded"));
        assert!(ok(&home.join("Videos").display().to_string()));

        // The node's own state, from any angle.
        assert!(!ok(&data.display().to_string()));
        assert!(!ok(&data.join("identity").display().to_string()));
        // A grant of an ancestor would sweep the data dir in with it.
        assert!(!ok(&home.display().to_string()));

        // The credential stores S-M10 deliberately carved out. A derived
        // grant that could put these back would make the carve-out
        // decorative.
        assert!(!ok(&home.join(".ssh").display().to_string()));
        assert!(!ok(&home.join(".ssh/id_ed25519").display().to_string()));
        assert!(!ok(&home.join(".aws").display().to_string()));

        // Not paths at all.
        assert!(!ok("relative/path"));
        assert!(!ok("/"));
    }

    #[test]
    fn a_declared_grant_reaches_the_rules_and_a_refused_one_does_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path();
        let data = home.join(".meshware");
        let job = data.join("jobs").join("job-1");
        std::fs::create_dir_all(&job).expect("mk");
        std::fs::create_dir_all(home.join(".ssh")).expect("mk");
        let wanted = home.join("Videos");
        std::fs::create_dir_all(&wanted).expect("mk");

        let grants = Grants {
            read: vec![wanted.clone(), home.join(".ssh")],
            write: vec![wanted.clone(), data.clone()],
            ..Default::default()
        };
        let rules = Rules::for_job(&Policy::default(), &job, &data, Some(home), &grants);
        assert!(rules.read.contains(&wanted), "declared read was dropped");
        assert!(rules.write.contains(&wanted), "declared write was dropped");
        assert!(!rules.read.contains(&home.join(".ssh")));
        assert!(!rules.write.contains(&data));
    }

    /// A leased volume reaches the ruleset (N5).
    ///
    /// The bug this pins: nothing put the volume's path in, so a bash job
    /// holding a writable lease on `data` got `Permission denied` writing
    /// to the directory the ledger said it was writing. A container never
    /// saw it — its plugin does a real bind mount — so the whole phase
    /// demoed green while the commonest runtime could not use the feature.
    #[test]
    fn a_leased_volume_is_in_the_ruleset() {
        let tmp = tempfile::tempdir().expect("tmp");
        let job = tmp.path().join("job");
        let data = tmp.path().join("data");
        let rw = tmp.path().join("vol-rw");
        let ro = tmp.path().join("vol-ro");
        for d in [&job, &data, &rw, &ro] {
            std::fs::create_dir_all(d).expect("mk");
        }

        let grants = Grants {
            leased_read: vec![ro.clone()],
            leased_write: vec![rw.clone()],
            ..Default::default()
        };
        let rules = Rules::for_job(&Policy::default(), &job, &data, None, &grants);
        assert!(rules.write.contains(&rw), "a writable lease was dropped");
        // Readable, and *not* writable: the mode on the claim is the whole
        // point, and a read-only volume that arrived writable would make
        // `read_only = true` decorative.
        assert!(rules.read.contains(&ro), "a read-only lease was dropped");
        assert!(
            !rules.write.contains(&ro),
            "a read-only lease became writable"
        );
    }

    #[test]
    fn a_renamed_release_artefact_still_hosts_the_shim() {
        // The exact filename that shipped, and that ran every job
        // unconfined while doctor said Landlock was available.
        assert!(is_shim_host_name(
            "meshnode-v0.7.6-x86_64-unknown-linux-musl"
        ));
        assert!(is_shim_host_name("meshnode"));
        assert!(is_shim_host_name("meshnode.exe"));
        assert!(is_shim_host_name("meshware-director"));
        assert!(is_shim_host_name("meshware-director.exe"));
        assert!(is_shim_host_name("meshware-director-0.7.6"));
        // A test binary is named after its test, not after the product.
        assert!(!is_shim_host_name("sandbox-1a2b3c4d"));
        // After an in-place upgrade, the running image keeps its host name
        // (ENG-10).
        assert!(is_shim_host_name(undeleted("meshnode (deleted)")));
        assert!(is_shim_host_name(undeleted(
            "meshnode-v0.7.6-x86_64-unknown-linux-musl (deleted)"
        )));
        assert!(!is_shim_host_name(undeleted("sandbox-1a2b3c4d (deleted)")));
        assert!(!is_shim_host_name("meshnodeish"));
        assert!(!is_shim_host_name("cargo"));
    }

    #[test]
    fn off_never_wraps_and_auto_degrades_without_a_shim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let off = Policy {
            mode: Mode::Off,
            ..Policy::default()
        };
        assert!(
            wrap(&off, tmp.path(), tmp.path(), "true", &[])
                .expect("ok")
                .is_none()
        );
        // A test harness is not a shim host (unless MESHWARE_SANDBOX_SHIM
        // says so), so auto runs plainly and `on` says why it cannot.
        if std::env::var_os(SHIM_ENV).is_none() {
            let auto = Policy::default();
            assert!(
                wrap(&auto, tmp.path(), tmp.path(), "true", &[])
                    .expect("ok")
                    .is_none()
            );
            let on = Policy {
                mode: Mode::On,
                ..Policy::default()
            };
            let e = wrap(&on, tmp.path(), tmp.path(), "true", &[]).expect_err("refused");
            assert!(e.contains("sandbox"), "{e}");
        }
        // A policy that names a shim is a host that knows one; whether the
        // kernel plays along is the only remaining question.
        let named = Policy {
            shim: Some(PathBuf::from("/usr/bin/true")),
            ..Policy::default()
        };
        if let Ok(Some(w)) = wrap(&named, tmp.path(), tmp.path(), "true", &[]) {
            assert_eq!(w.program, PathBuf::from("/usr/bin/true"));
            assert_eq!(w.args[0], OsString::from(SHIM_ARG));
            assert!(w.env.iter().any(|(k, _)| k == "TMPDIR"));
        }
    }
}
