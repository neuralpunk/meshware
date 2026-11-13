//! Container engines, built in (T2 §4.4).
//!
//! ## Why this is not a plugin
//!
//! N4 made containers a subprocess plugin on the reasoning that `meshnode`
//! links no OCI client — four dependencies, no C, a static musl binary —
//! and that an in-tree image puller, layer unpacker, registry client and
//! TLS stack would spend all of it.
//!
//! The first half of that is true and load-bearing. The second half does
//! not follow, and confusing them cost the product its simplest path.
//! **Nomad's Docker driver does not implement OCI either** — it talks to a
//! daemon over a socket — and the reference plugin did not implement OCI
//! either: it ran `podman run`. Spawning a CLI that is already on the
//! machine costs zero dependencies and touches none of the property being
//! protected. So the indirection bought nothing for the common case, while
//! costing every operator a file to copy, a config line to write, and a
//! machine to write it on.
//!
//! This module is that CLI invocation, in Rust, where it can be tested, is
//! versioned with the binary, and declares the isolation controls from the
//! same code that passes the flags.
//!
//! ## What is *not* fixed by moving it in-binary
//!
//! `podman run` hands the workload to `conmon`, which reparents it out of
//! the executor's cgroup and out of any Landlock ruleset applied to the
//! client. That was true of the plugin and is true of this — it is a
//! property of container runtimes, not of the indirection. What a container
//! runtime has instead are its own equivalents, and [`Engine::controls`]
//! reports exactly the ones this code passes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::containers::{Controls, PrepareReply, PrepareRequest};
use crate::error::{CoreError, Result};

/// How long an engine gets to answer a probe.
///
/// Short, because this runs on the capacity refresh and a hung engine must
/// read as broken rather than stall a node's announcement. A container
/// runtime that cannot say its own version in three seconds is not one any
/// job should be dispatched to.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The engines this build can drive, in preference order.
///
/// **podman first, and it is a decision rather than alphabetical.** It is
/// rootless by default and has no daemon to be down, so a machine with both
/// almost certainly wants the one that does not require the node's user to
/// be in a group equivalent to root. An operator who disagrees says so with
/// `[runtime] container_engine`.
pub const PREFERENCE: [EngineKind; 2] = [EngineKind::Podman, EngineKind::Docker];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EngineKind {
    Podman,
    Docker,
}

impl EngineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Podman => "podman",
            EngineKind::Docker => "docker",
        }
    }

    pub fn parse(s: &str) -> Option<EngineKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "podman" => Some(EngineKind::Podman),
            "docker" => Some(EngineKind::Docker),
            _ => None,
        }
    }

    /// The program name, as found on `PATH`.
    fn program(&self) -> &'static str {
        self.as_str()
    }

    /// The arguments that make it print its server version and nothing else.
    ///
    /// `podman info` rather than `podman version`, because the question is
    /// not "is there a binary" — that is answered by finding it on PATH —
    /// but "does the thing behind it answer". `docker version` with a
    /// `--format` over `.Server` is the same question: it fails when the
    /// daemon is down, which is exactly the state worth distinguishing.
    fn probe_args(&self) -> &'static [&'static str] {
        match self {
            EngineKind::Podman => &["info", "--format", "{{.Version.Version}}"],
            EngineKind::Docker => &["version", "--format", "{{.Server.Version}}"],
        }
    }

    /// What to tell somebody who has neither. Read by [`report`], which
    /// builds the "install one of these" sentence from [`PREFERENCE`] so
    /// adding an engine adds it to the advice without a second edit.
    fn remedy(&self) -> &'static str {
        match self {
            EngineKind::Podman => "install podman (`dnf install podman`, `apt install podman`)",
            EngineKind::Docker => "install docker and start its daemon",
        }
    }

    /// Podman spells the flag bare; docker wants the `=true`.
    fn no_new_privileges(&self) -> &'static str {
        match self {
            EngineKind::Podman => "no-new-privileges",
            EngineKind::Docker => "no-new-privileges=true",
        }
    }
}

/// A container engine this node can actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    pub kind: EngineKind,
    /// The program to spawn. The bare name, resolved through `PATH` by the
    /// spawn — not an absolute path, so a `podman` shim earlier on the
    /// operator's `PATH` keeps working.
    pub program: String,
    /// The server version it reported.
    pub version: String,
    /// Running without root. Always true for ordinary podman; true for
    /// docker only in a rootless installation.
    pub rootless: bool,
    /// The socket of the podman service this node drives containers
    /// through, when it does — see [*Where containers are
    /// made*](#where-containers-are-made). `None` is podman or docker run
    /// directly, as a child of this process.
    pub remote: Option<PathBuf>,
    /// Whether the engine can enforce `--cpus` and `--memory`. Always true
    /// run directly, where the node's own delegated cgroup is what it
    /// uses; read from the engine when it is a service, whose controllers
    /// are whatever its user manager was delegated.
    pub cpu_limit: bool,
    pub memory_limit: bool,
}

impl Engine {
    /// The line a person reads: `podman 5.8.4 (rootless)`, and
    /// `podman 5.8.4 (rootless, via podman.socket)` when it is a service.
    pub fn detail(&self) -> String {
        let how = match (self.rootless, self.remote.is_some()) {
            (true, true) => " (rootless, via podman.socket)",
            (false, true) => " (via podman.socket)",
            (true, false) => " (rootless)",
            (false, false) => "",
        };
        format!("{} {}{how}", self.kind.as_str(), self.version)
    }

    /// The arguments every invocation of this engine starts with: none run
    /// directly, `--remote --url unix://…` through its service.
    pub fn global_args(&self) -> Vec<String> {
        remote_args(self.remote.as_deref())
    }

    /// [`Engine::global_args`] and then `args`: one invocation.
    fn invocation(&self, args: &[&str]) -> Vec<String> {
        let mut all = self.global_args();
        all.extend(args.iter().map(|a| (*a).to_string()));
        all
    }

    /// What this code actually enforces.
    ///
    /// **Declared by the function that passes the flags**, three screens
    /// below. When the plugin answered this it was a JSON blob a shell
    /// script emitted, which an operator had to trust a stranger to have
    /// kept honest; here the claim and the argument list cannot drift
    /// without the drift being visible in one file.
    pub fn controls(&self) -> Controls {
        Controls {
            memory: self.memory_limit,
            cpu: self.cpu_limit,
            readonly_rootfs: true,
            drop_capabilities: true,
            no_new_privileges: true,
            rootless: self.rootless,
        }
    }
}

/// Probe one engine.
///
/// `Ok(None)` means *no such program on this machine* — not an error, just
/// an engine this node does not have. `Err` means the program is there and
/// would not answer, which is the state worth telling somebody about.
///
/// The error is a **plain sentence, not a `CoreError`**, because its only
/// destination is a status table and an operator's eye. Wrapping it earned
/// a `job error: ` prefix on the one line meant to be read as English.
///
/// `service` is where this node's podman service listens, when it has one
/// ([`crate::layout::Layout::engine_socket`]). A socket there is used in
/// preference to running podman directly — see [*Where containers are
/// made*](#where-containers-are-made).
pub async fn probe(
    kind: EngineKind,
    service: Option<&Path>,
) -> std::result::Result<Option<Engine>, String> {
    let program = kind.program();
    let remote = match kind {
        EngineKind::Podman => service.filter(|s| is_socket(s)).map(Path::to_path_buf),
        EngineKind::Docker => None,
    };
    let global = remote_args(remote.as_deref());
    let out = match run_probe(program, &with_global(&global, kind.probe_args())).await {
        Ok(out) => out,
        // Not on PATH. Every other spawn failure is a real problem and is
        // reported as one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{program}: {e}")),
    };
    // **Run directly, podman cannot start a container here at all**, and
    // `podman info` answering says nothing about that — it mounts no
    // `/proc`. Checked before the answer is read, because under this
    // service's hardening the other checks below fail too, each with advice
    // to relax the unit, and that advice is the wrong fix.
    if kind == EngineKind::Podman
        && remote.is_none()
        && let Some(locked) = proc_locked(service)
    {
        return Err(locked);
    }
    let Some(out) = out else {
        return Err(format!(
            "{program} did not answer within {}s",
            PROBE_TIMEOUT.as_secs()
        ));
    };
    if !out.status.success() {
        let why = first_clause(&String::from_utf8_lossy(&out.stderr));
        return Err(match &remote {
            Some(socket) => format!(
                "this node's podman service at {} did not answer: {}",
                socket.display(),
                explain_failure(why)
            ),
            None => explain_failure(why),
        });
    }
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let rootless = rootless(kind, &global).await;
    // **Answering is not the same as being able to run anything (T2).**
    //
    // `podman info` answers perfectly well for a user with no subuid/subgid
    // range — it does not need one. The first `podman run` then fails,
    // deep in the image unpack, with
    //
    //   potentially insufficient UIDs or GIDs available in user namespace
    //   (requested 0:42 for /etc/shadow) … lchown /etc/shadow: invalid argument
    //
    // because `/etc/shadow` in a stock image is owned by `0:42` and there
    // is no gid 42 to map it to. So the node announced `container`, took
    // container work, and failed at spawn — which is the exact failure the
    // whole runtime-fingerprint track exists to prevent, one level deeper
    // than where it was first closed.
    if rootless {
        if let Some(short) = id_map_shortfall(kind, &global).await {
            return Err(short);
        }
        // A range is necessary and **not sufficient**: applying it is done
        // by `newuidmap`/`newgidmap`, which need privilege of their own to
        // change a namespace's mapping — and which can have it marked
        // wrongly in two opposite directions. Without it `podman info`
        // still reports the range — it reads `/etc/subuid` — and the first
        // `podman run` fails on a line that only names the helper it tried
        // to run. Checked here so the node says so instead of accepting
        // work and failing at spawn.
        if let Some(wrong) = id_helpers_unusable() {
            return Err(wrong);
        }
        // …and that *this process* is allowed to let them work. A
        // capability on the file is worth nothing if the kernel will not
        // grant it on exec, and meshnode's own systemd unit is the usual
        // reason it will not. Only run directly: a podman service execs the
        // helpers from its own process, whose privileges these are not.
        if remote.is_none()
            && let Some(forbidden) = namespace_mapping_forbidden()
        {
            return Err(forbidden);
        }
    }
    // Run directly, the node's own delegated cgroup is what enforces the
    // limits. A service has whatever controllers its user manager was
    // given, and a `--cpus` it cannot honour fails the container outright.
    let (cpu_limit, memory_limit) = match &remote {
        Some(_) => controllers(kind, &global).await,
        None => (true, true),
    };
    Ok(Some(Engine {
        kind,
        program: program.to_string(),
        version: if version.is_empty() {
            "unknown".into()
        } else {
            version
        },
        rootless,
        remote,
        cpu_limit,
        memory_limit,
    }))
}

/// How many sub-ids a rootless engine needs before stock images unpack.
///
/// 65536 is the conventional allocation and what `useradd` gives an
/// ordinary user. Images reach well past the low numbers — `nobody` is
/// 65534 — so a short range does not fail gracefully at the top, it fails
/// on whichever file happens to be owned above the limit.
const MIN_SUB_IDS: u64 = 65536;

/// How many ids an engine's reported mapping covers in total.
///
/// `None` when the engine reported nothing parseable, which is treated as
/// *unknown* rather than *too small* — see [`id_map_shortfall`].
///
/// The user's own id is always one entry of size 1; what decides whether an
/// image unpacks is the delegated range beside it, so the sum is the
/// number that matters. A user with no `/etc/subuid` entry reports exactly
/// `1`, which is the signature worth recognising.
fn mapped_ids(reported: &str) -> Option<u64> {
    let sizes: Vec<u64> = reported
        .split_whitespace()
        .filter_map(|n| n.parse::<u64>().ok())
        .collect();
    (!sizes.is_empty()).then(|| sizes.iter().sum())
}

/// `Some(reason)` when this engine's user namespace is too small to unpack
/// an ordinary image.
///
/// Best-effort: an engine that will not report its mappings is left alone
/// rather than declared broken, because a wrong refusal here takes a
/// working node out of the mesh.
async fn id_map_shortfall(kind: EngineKind, global: &[String]) -> Option<String> {
    let mut worst: Option<(&str, u64)> = None;
    for (what, field) in [("UID", "UIDMap"), ("GID", "GIDMap")] {
        let fmt = format!("{{{{range .Host.IDMappings.{field}}}}}{{{{.Size}}}} {{{{end}}}}");
        let args = with_global(global, &["info", "--format", &fmt]);
        let Ok(Some(out)) = run_probe(kind.program(), &args).await else {
            return None;
        };
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // `?` rather than a match: an engine that reports nothing
        // parseable is *unknown*, and unknown must not be reported as
        // broken — see this function's contract.
        let total = mapped_ids(&text)?;
        if total < MIN_SUB_IDS && worst.is_none_or(|(_, w)| total < w) {
            worst = Some((what, total));
        }
    }
    let (what, total) = worst?;
    Some(format!(
        concat!(
            "rootless {} has only {} {}s in its user namespace (needs {}), so stock ",
            "images cannot unpack — `podman run` fails on `lchown /etc/shadow`. The ",
            "user running meshnode has no /etc/sub{}id range. Fix it as root:\n",
            "  usermod --add-subuids 100000-165535 --add-subgids 100000-165535 <user>\n",
            "  loginctl enable-linger <user> && sudo -u <user> podman system migrate"
        ),
        kind.as_str(),
        total,
        what,
        MIN_SUB_IDS,
        what.to_lowercase(),
    ))
}

/// The sentence worth showing from an engine's complaint.
///
/// Container engines state the problem up front and then elaborate, and the
/// elaboration is a stack of socket paths and dial errors that says nothing
/// a status table can use. The first line, cut at its first `;`, is
/// reliably the part a person acts on:
///
/// > `failed to connect to the docker API at unix:///var/run/docker.sock`
///
/// rather than that plus two clauses restating it.
fn first_clause(stderr: &str) -> String {
    // **Keep every line that carries signal, drop the narration.**
    //
    // A container engine narrates before it fails, and the reason is spread
    // across the lines rather than living on any one of them. A rootless
    // podman failure reads:
    //
    //   time="…" level=error msg="running `/usr/bin/newuidmap 1 0 961 …`"
    //   newuidmap: write to uid_map failed: Operation not permitted
    //   Error: cannot set up namespace using "/usr/bin/newuidmap": exit status 1
    //
    // The first line names the attempt, the last names the *symptom*, and
    // the middle one is the only line that says **why**. Keeping one line
    // threw that away whichever one was chosen — first gave the attempt,
    // last gave the symptom. Both cost a round trip with somebody staring
    // at a log that named a helper and nothing else.
    //
    // So: unwrap the `time=… level=…` narration to the message inside it,
    // keep the rest. This is the *data*; the table in `meshnode runtimes`
    // truncates to its column and `doctor` and `--output json` show it
    // whole.
    //
    // **Unwrap, not drop.** This filter used to discard those lines
    // outright, on the reasoning that their `msg=` was only the command
    // being run and the reason followed on its own line. That is true of
    // one failure and false of the next: when `newuidmap` is refused the
    // open rather than the write, podman puts the command *and* the reason
    // in a single narrated line —
    //
    //   msg="running `/usr/bin/newuidmap …`: newuidmap: open of uid_map
    //        failed: Permission denied"
    //
    // — and dropping it left a log saying only `cannot set up namespace`,
    // which is the symptom with the diagnosis cut out of it. That cost a
    // round trip on a real machine.
    let owned: Vec<String> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // Layer-copy chatter from a pull that got as far as unpacking.
        .filter(|l| !l.starts_with("Copying ") && !l.starts_with("Trying to pull"))
        .filter(|l| *l != "Getting image source signatures")
        .map(|l| narrated_message(l).unwrap_or_else(|| l.to_string()))
        .filter(|l| !l.is_empty())
        .collect();
    let kept: Vec<&str> = owned.iter().map(String::as_str).collect();
    if kept.is_empty() {
        return "no reason given".to_string();
    }
    // Docker's socket error restates itself: `failed to connect …; check if
    // the path is correct …`, where everything after the semicolon is
    // advice we give better ourselves. Never cut a line whose reason is at
    // the end, which is most of them.
    let trimmed: Vec<&str> = kept
        .iter()
        .map(|l| match l.split_once(';') {
            Some((head, tail)) if tail.trim_start().starts_with("check ") => head.trim(),
            _ => *l,
        })
        .collect();
    // The last few, because a failure elaborates as it goes and the tail is
    // where the diagnosis lands.
    const MAX_LINES: usize = 4;
    let from = trimmed.len().saturating_sub(MAX_LINES);
    let joined = trimmed[from..].join(" / ");
    const CAP: usize = 600;
    if joined.chars().count() > CAP {
        let cut: String = joined.chars().take(CAP - 1).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

/// The message inside one of podman's narrated log lines, if it is one.
///
/// podman writes its own diagnostics through logrus, in two renderings
/// depending on whether stderr is a terminal:
///
/// ```text
/// ERRO[0000] running `/usr/bin/newuidmap …`: newuidmap: open of uid_map failed: …
/// time="…" level=error msg="running `/usr/bin/newuidmap …`: newuidmap: open of …"
/// ```
///
/// Both carry the same sentence, and under systemd it is always the second.
/// `None` for an ordinary line, which is then kept as it stands.
fn narrated_message(line: &str) -> Option<String> {
    // `ERRO[0000] …`, and its INFO/WARN/DEBU siblings.
    if let Some(rest) = line
        .strip_prefix("ERRO[")
        .or_else(|| line.strip_prefix("WARN["))
        .or_else(|| line.strip_prefix("INFO["))
        .or_else(|| line.strip_prefix("DEBU["))
    {
        return rest.split_once(']').map(|(_, msg)| msg.trim().to_string());
    }
    if !(line.starts_with("time=") && line.contains("level=")) {
        return None;
    }
    // `msg="…"`, with `\"` for a quote inside it. Anything before or after
    // is a key=value pair we have no use for — the timestamp, the level,
    // and podman's own source location.
    let after = line.split_once("msg=\"")?.1;
    let mut out = String::with_capacity(after.len());
    let mut chars = after.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    // An unterminated quote is still better read than dropped.
    Some(out)
}

/// How a sub-id helper is marked to gain the privilege it needs.
///
/// There are **two** valid markings and distributions disagree about which
/// to use. Fedora and the RHEL-likes ship `newuidmap` with a file
/// capability and no setuid bit; Debian and Ubuntu ship it setuid-root with
/// no capability. Both work. What does not work is both at once, which is
/// the whole reason this is a struct rather than a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marking {
    setuid_root: bool,
    file_cap: bool,
}

/// What a [`Marking`] means for whether the helper can do its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marked {
    /// Exactly one of the two markings. This is what a distribution ships.
    Correctly,
    /// Neither. The helper runs with the caller's own (nil) privilege.
    NotAtAll,
    /// **Both**, which the kernel refuses to honour. See [`Marked::why`].
    Both,
}

impl Marked {
    fn of(m: Marking) -> Self {
        match (m.setuid_root, m.file_cap) {
            (true, true) => Marked::Both,
            (false, false) => Marked::NotAtAll,
            _ => Marked::Correctly,
        }
    }
}

/// `Some(reason)` when the sub-id helpers cannot apply a mapping.
///
/// `newuidmap` and `newgidmap` write another process's `uid_map`, which
/// they can only do with privilege they do not inherit from the caller. A
/// distribution grants it one of two ways — a `cap_setuid` file capability
/// (Fedora, RHEL) or a setuid-root bit (Debian, Ubuntu) — and this reports
/// the two ways that goes wrong.
///
/// **Neither marking.** A restore from a backup, a `cp` that dropped
/// extended attributes, or a filesystem mounted `nosuid` takes it away and
/// nothing else on the machine notices. `podman run` then fails with
/// `write to uid_map failed: Operation not permitted` — an `EPERM` from the
/// write, because the helper opened the file as its caller (who owns it)
/// and then lacked `CAP_SETUID` to fill it in.
///
/// **Both markings**, which is the one that cost a real fleet an evening.
/// `capabilities(7)` is explicit that a setuid-root binary which *also*
/// carries effective file capabilities does not get the root capability
/// set — the kernel says so in the log, once per boot:
///
/// ```text
/// warning: `/usr/bin/newuidmap' has both setuid-root and effective
/// capabilities. Therefore not raising all capabilities.
/// ```
///
/// So the helper runs with euid 0 and `cap_setuid` alone — no
/// `CAP_DAC_OVERRIDE` — and euid 0 grants nothing by itself under DAC.
/// `/proc/<pid>/uid_map` is mode 0644 owned by the *caller*, so root
/// without `CAP_DAC_OVERRIDE` cannot open it for writing and the failure
/// moves one step earlier, to an `EACCES` from the open:
///
/// ```text
/// newuidmap: open of uid_map failed: Permission denied
/// ```
///
/// That open-versus-write distinction is the diagnosis: `EPERM` on the
/// write means no privilege, `EACCES` on the open means the wrong kind.
///
/// **This function is also an apology.** meshware's own documentation used
/// to say "run `setcap cap_setuid+ep /usr/bin/newuidmap` if `getcap` prints
/// nothing" without qualifying it by distribution, which on Ubuntu adds a
/// capability to a binary that is already setuid-root and breaks a machine
/// that was working. Advice that is right on one distribution and harmful
/// on another has to be checked before it is given, and now it is.
///
/// Best-effort in both directions: no `getcap` means the capability is
/// unknown, and unknown is never a refusal.
fn id_helpers_unusable() -> Option<String> {
    let caps = std::process::Command::new("getcap")
        .args(["/usr/bin/newuidmap", "/usr/bin/newgidmap"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase())?;

    // Both helpers, each with its own capability. One wrong without the
    // other fails just as completely, and naming only one sends somebody
    // to fix half of it.
    let mut unmarked = Vec::new();
    let mut both = Vec::new();
    for (tool, cap) in [("newuidmap", "cap_setuid"), ("newgidmap", "cap_setgid")] {
        let path = format!("/usr/bin/{tool}");
        let marking = Marking {
            setuid_root: is_setuid_root(&path),
            file_cap: caps.lines().any(|l| l.contains(tool) && l.contains(cap)),
        };
        match Marked::of(marking) {
            Marked::Correctly => {}
            Marked::NotAtAll => unmarked.push(tool),
            Marked::Both => both.push(tool),
        }
    }

    // The mixed marking is named first when both faults are present: it is
    // the one that looks fixed, because `getcap` prints the capability the
    // other message asks for.
    if !both.is_empty() {
        // `setcap -r` takes one path. Given two it reads the second as a
        // capability name and says "File 'setcap' has no capablity to
        // remove" — its own typo, and a message that sounds like a refusal
        // to act rather than a usage error. So: one command per file.
        let fix = both
            .iter()
            .map(|t| format!("  sudo setcap -r /usr/bin/{t}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Some(format!(
            concat!(
                "{} is marked setuid-root *and* capability-marked, a combination the ",
                "kernel refuses\n\n",
                "`capabilities(7)`: a setuid-root binary that also carries effective file ",
                "capabilities is not given the root capability set. The kernel says so ",
                "once per boot — `dmesg | grep 'both setuid-root'`. The helper then runs ",
                "as root with cap_setuid alone, and root without CAP_DAC_OVERRIDE ",
                "overrides no file permission at all, so it cannot open the caller's own ",
                "mode-0644 /proc/<pid>/uid_map and every `podman run` dies on ",
                "`newuidmap: open of uid_map failed: Permission denied`.\n\n",
                "Debian and Ubuntu ship these setuid-root and need no capability; adding ",
                "one is what breaks them. Remove it, one file per command:\n{}"
            ),
            both.join(" and "),
            fix,
        ));
    }

    if unmarked.is_empty() {
        return None;
    }
    Some(format!(
        concat!(
            "{} is neither setuid-root nor capability-marked, so it cannot apply a ",
            "user-namespace mapping\n\n",
            "The sub-id range is fine; the helper that applies it is not, and every ",
            "`podman run` fails on a line naming the helper rather than the reason.\n\n",
            "The safe repair is to reinstall the package that owns it — `shadow-utils` on ",
            "Fedora/RHEL, `uidmap` on Debian/Ubuntu — which restores whichever marking ",
            "this distribution intends. By hand it is one of these, never both:\n",
            "  sudo setcap cap_setuid+ep /usr/bin/newuidmap   # Fedora, RHEL\n",
            "  sudo chmod u+s /usr/bin/newuidmap              # Debian, Ubuntu"
        ),
        unmarked.join(" and "),
    ))
}

/// Is this file setuid-root?
///
/// The setuid bit alone is not enough — a file owned by somebody other than
/// root gains that user, not privilege — so the owner is checked with it.
fn is_setuid_root(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.uid() == 0 && m.permissions().mode() & 0o4000 != 0)
}

/// Which of `needed` a bounding set does not contain.
///
/// `CapBnd` in `/proc/self/status` is a hex bitmask over the capability
/// numbers, so this is one shift per capability — pulled out of its caller
/// only so it can be tested against a real mask from a real machine
/// instead of whichever one the test happens to run on.
fn missing_from_bounding(set: u64, needed: &[(u8, &'static str)]) -> Vec<&'static str> {
    needed
        .iter()
        .filter(|(bit, _)| set & (1u64 << bit) == 0)
        .map(|(_, name)| *name)
        .collect()
}

/// `Some(reason)` when *this process* cannot let the sub-id helpers work.
///
/// **The trap that `getcap` cannot see.** `newuidmap` carries
/// `cap_setuid=ep` as a file capability, and two things in this process's
/// own state can make the kernel ignore it on exec:
///
/// - **`no_new_privs`**, which makes `execve` grant no file capabilities at
///   all. That is the entire point of the flag, and it is exactly what
///   `NoNewPrivileges=true` in `meshnode.service` sets.
/// - **an empty capability bounding set**, which caps what any exec may
///   ever acquire. `CapabilityBoundingSet=` in the same unit empties it.
///
/// Both are *correct* hardening for a daemon that only ever spawns
/// unprivileged work, and both were correct for meshware right up until it
/// started driving rootless podman itself — because `newuidmap` exists to
/// change user ids, which is precisely what they forbid.
///
/// The symptom is maddening without this check: `getcap` shows the
/// capability present, `/etc/subuid` shows the range allocated, and every
/// `podman run` fails on a line that names the helper. So the node reads
/// its own `/proc/self/status` and says which of its own restrictions is
/// responsible.
/// Capability bit numbers, from `linux/capability.h`.
///
/// Three of forty; the rest have no bearing on mapping a user namespace.
const CAP_DAC_OVERRIDE: u8 = 1;
const CAP_SETGID: u8 = 6;
const CAP_SETUID: u8 = 7;
const CAP_SYS_ADMIN: u8 = 21;

/// What the sub-id helpers need *this process* to allow them, given how
/// this machine's helpers are marked.
///
/// **The two markings need different things, which is the whole point.**
///
/// A capability-marked helper (Fedora, RHEL) execs into euid 0's *absence*:
/// it stays the calling user and gains exactly `CAP_SETUID`. The
/// `/proc/<pid>/uid_map` it opens is owned by that same user, so ordinary
/// file permissions let it in and no `CAP_DAC_OVERRIDE` is involved.
///
/// A setuid-root helper (Debian, Ubuntu) execs into euid 0, and a
/// setuid-root exec takes its permitted set from **the bounding set** —
/// `cap_bprm_creds_from_file` sets `pP' = bset | pI`. So under a narrow
/// ceiling it is root with *only* what the ceiling allows, and euid 0 is
/// not by itself permission to do anything. It needs two more:
///
/// - `CAP_DAC_OVERRIDE`, because the `/proc/<pid>/uid_map` it opens is mode
///   0644 owned by the **service user**, not by root, and under file
///   permissions the kernel checks the capability rather than the uid.
/// - `CAP_SYS_ADMIN`, which is the one nobody guesses, for a reason that is
///   worth stating exactly. `map_write` requires `CAP_SYS_ADMIN` **over the
///   namespace being mapped**, and `cap_capable` grants every capability in
///   a user namespace to *the euid that owns it*. A capability-marked
///   helper keeps the caller's euid, so it **is** the owner and gets
///   `CAP_SYS_ADMIN` there for free. A setuid-root helper has euid 0, which
///   is not the owner — 997 is — so the shortcut does not apply and it must
///   hold the capability outright, in the initial namespace.
///
/// That asymmetry is the whole reason the two markings need different
/// ceilings, and it is why Debian ships these setuid-root rather than
/// capability-marked: the set they need is close enough to root that
/// naming it was never the simpler option.
fn caps_the_helpers_need() -> Vec<(u8, &'static str)> {
    let mut needed = vec![(CAP_SETUID, "CAP_SETUID"), (CAP_SETGID, "CAP_SETGID")];
    if is_setuid_root("/usr/bin/newuidmap") || is_setuid_root("/usr/bin/newgidmap") {
        needed.push((CAP_DAC_OVERRIDE, "CAP_DAC_OVERRIDE"));
        needed.push((CAP_SYS_ADMIN, "CAP_SYS_ADMIN"));
    }
    needed
}

/// The drop-in this machine actually needs, ready to paste.
///
/// Generated rather than referenced. Telling an operator to `cp` a file out
/// of a checkout assumes they have the checkout on the node, and a fleet
/// where that is true is not the fleet this is for. It is also how a stale
/// copy of the file survives a fix to it — which is how a machine ended up
/// with a two-capability bounding set after the helper on it needed three.
fn containers_dropin(also: &[&str]) -> String {
    let mut caps: Vec<&str> = caps_the_helpers_need().iter().map(|(_, n)| *n).collect();
    // Whatever was found missing, even if this machine's markings say it
    // should not be needed. The two readings disagreeing is possible —
    // a helper somewhere other than /usr/bin, a `podman` that shells out
    // to a different one — and a paste-able block that omits the
    // capability the sentence above it just named would be worse than
    // useless: it would read as a contradiction and be doubted.
    for cap in also {
        if !caps.contains(cap) {
            caps.push(cap);
        }
    }
    format!(
        concat!(
            "  sudo mkdir -p /etc/systemd/system/meshnode.service.d\n",
            "  sudo tee /etc/systemd/system/meshnode.service.d/containers.conf >/dev/null <<'EOF'\n",
            "[Service]\n",
            "NoNewPrivileges=false\n",
            "CapabilityBoundingSet={}\n",
            "EOF\n",
            "  sudo systemctl daemon-reload && sudo systemctl restart meshnode"
        ),
        caps.join(" ")
    )
}

/// `Some(reason)` when *this process* cannot let the sub-id helpers work.
///
/// **The trap that `getcap` cannot see.** The helper's own marking can be
/// perfect and still be worth nothing, because a marking is a request the
/// kernel grants at `execve` and this process's own state decides whether
/// it will:
///
/// - `NoNewPrivileges=true` — the kernel grants **no** file capability and
///   honours **no** setuid bit on exec. That is the entire point of the
///   flag, and it defeats both markings equally.
/// - `CapabilityBoundingSet=` — a ceiling on what any exec may ever hold.
///   Empty stops everything; *narrower than the helper needs* stops it just
///   as completely and is much harder to see, because the set looks
///   deliberate and mostly is.
///
/// Read from `/proc/self/status`, which is this process's observed state
/// rather than a unit file's stated intent — the distinction this whole
/// track keeps paying for. Best-effort: a `/proc` that will not answer is
/// unknown, and unknown is never a refusal.
fn namespace_mapping_forbidden() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| {
        status.lines().find_map(|l| {
            l.strip_prefix(name)
                .and_then(|v| v.strip_prefix(':'))
                .map(|v| v.trim().to_string())
        })
    };
    let no_new_privs = field("NoNewPrivs").is_some_and(|v| v != "0");
    let bounding = field("CapBnd").and_then(|v| u64::from_str_radix(v.trim(), 16).ok());
    // Unreadable or unparseable is unknown, and unknown grants everything:
    // refusing on a field we could not read would take working nodes out
    // of the mesh.
    let missing: Vec<&str> = bounding
        .map(|set| missing_from_bounding(set, &caps_the_helpers_need()))
        .unwrap_or_default();
    if !no_new_privs && missing.is_empty() {
        return None;
    }
    Some(mapping_forbidden_text(no_new_privs, &missing))
}

/// The sentence itself, given what was found.
///
/// Separated from the reading of `/proc/self/status` so that the wording
/// can be tested on a machine that is not in the state it describes —
/// which is every machine a test runs on, and is why two versions of this
/// message shipped saying something that was not true of the node reading
/// it.
fn mapping_forbidden_text(no_new_privs: bool, missing: &[&str]) -> String {
    // The headline is one line because two places can only print one: a
    // log entry and a table cell. Everything after the blank line is for
    // the place that has room.
    let headline = match (no_new_privs, missing) {
        (true, _) => "this unit forbids user-id mapping: NoNewPrivileges=true, so the kernel \
                      honours no setuid bit and no file capability on exec"
            .to_string(),
        (false, [one]) => format!(
            "this unit's CapabilityBoundingSet has no {one}, which the sub-id helpers on \
             this machine need"
        ),
        (false, many) => format!(
            "this unit's CapabilityBoundingSet has no {}, which the sub-id helpers on \
             this machine need",
            many.join(" or ")
        ),
    };

    // Why the missing piece is missing, in the machine's own terms.
    let because = if no_new_privs {
        String::new()
    } else if missing.contains(&"CAP_DAC_OVERRIDE") || missing.contains(&"CAP_SYS_ADMIN") {
        concat!(
            "`/usr/bin/newuidmap` here is setuid-root, so it runs as root — and a ",
            "setuid-root exec takes its capabilities *from the bounding set*, so a narrow ",
            "ceiling leaves it root with almost nothing. It needs two capabilities a ",
            "capability-marked helper does not:\n\n",
            "  CAP_DAC_OVERRIDE  the /proc/<pid>/uid_map it opens is mode 0644 owned by ",
            "this service's user, not by root, and under file permissions the kernel ",
            "checks the capability rather than the uid. Missing, the *open* is refused: ",
            "`open of uid_map failed: Permission denied`.\n",
            "  CAP_SYS_ADMIN     writing a map needs CAP_SYS_ADMIN over the namespace ",
            "being mapped, and the kernel grants every capability in a user namespace to ",
            "the euid that owns it. A capability-marked helper keeps the caller's euid, so ",
            "it *is* the owner and gets this for free; a setuid-root one is euid 0, which ",
            "is not the owner, so it must hold the capability outright. Missing, the ",
            "*write* is refused: `write to uid_map failed: Operation not permitted`.\n\n",
            "That asymmetry is why the two markings need different ceilings, and why ",
            "Debian ships these setuid-root rather than capability-marked — the set they ",
            "need is close enough to root that naming it was never the simpler option.\n\n",
        )
        .to_string()
    } else {
        concat!(
            "The helper carries the capability and /etc/subuid carries the range; this ",
            "process is simply not allowed to use them.\n\n",
        )
        .to_string()
    };

    format!(
        "{headline}\n\n{because}Grant exactly what containers need on this machine, and \
         nothing else:\n{}\n{}",
        containers_dropin(missing),
        cost_of_the_dropin(missing)
    )
}

/// What the operator gives up, said plainly and in proportion.
///
/// **`CAP_SYS_ADMIN` is not a footnote.** On a node whose helpers are
/// setuid-root the ceiling has to include it, and that deserves a sentence
/// of its own rather than the same reassurance a two-capability set gets —
/// and it deserves the narrower alternative named beside it, because there
/// is one and an operator who cares should be told.
fn cost_of_the_dropin(missing: &[&str]) -> String {
    let base = "\nJobs are unaffected either way: the sandbox shim sets PR_SET_NO_NEW_PRIVS on \
                every sandboxed job itself, so relaxing the unit does not hand it to the work. \
                What changes is what *this daemon's own* execs may acquire.";
    if !caps_the_helpers_need()
        .iter()
        .any(|(_, n)| *n == "CAP_SYS_ADMIN")
        && !missing.contains(&"CAP_SYS_ADMIN")
    {
        return format!("{base} An exec cannot acquire anything outside the list above.");
    }
    format!(
        concat!(
            "{}\n\n",
            "CAP_SYS_ADMIN is a large capability and this is the honest price of a ",
            "setuid-root `newuidmap`: it is in the ceiling, so any setuid-root binary this ",
            "daemon execs could acquire it. The narrower option is to give this machine's ",
            "helpers the marking Fedora and RHEL use, after which two capabilities are ",
            "enough:\n",
            "  sudo chmod u-s /usr/bin/newuidmap /usr/bin/newgidmap\n",
            "  sudo setcap cap_setuid+ep /usr/bin/newuidmap\n",
            "  sudo setcap cap_setgid+ep /usr/bin/newgidmap\n",
            "  # then CapabilityBoundingSet=CAP_SETUID CAP_SETGID is sufficient\n",
            "Both markings at once is the one combination that does not work, so remove ",
            "the setuid bit in the same breath as adding the capability — and note that a ",
            "`uidmap` package upgrade puts the setuid bit back and drops the capability, ",
            "which `meshnode runtimes` will name when it happens."
        ),
        base
    )
}

/// Say the most specific true thing this node can about a failed engine.
///
/// **Observed state first, pattern-matching last.** A rootless-podman
/// failure has several causes that print the *same line*, so the message
/// cannot be read for the answer — it can only be read for the *shape* of
/// the question. What separates them is state this node can go and look at:
/// its own `/proc/self/status`, and the marking on the helper binaries. So
/// those are checked in order, and only if both come back clean is a hint
/// offered about something this node cannot verify.
///
/// That ordering is the correction for a real mistake. An earlier version
/// appended the AppArmor hint to any failure that mentioned a namespace,
/// on a machine where the sysctl happened to be on — and sent somebody to
/// write a profile that was already there, twice, while the actual cause
/// (a capability clamped out of the unit's bounding set) sat in
/// `/proc/self/status` unread. A guess printed beside an unchecked fact is
/// worse than no guess: it is read as the diagnosis.
fn explain_failure(why: String) -> String {
    if !looks_like_userns(&why) {
        return why;
    }
    // This process's own state, then the files it would exec. Both are
    // definite; either one being wrong is sufficient on its own.
    if let Some(reason) = namespace_mapping_forbidden() {
        return format!("{reason}\n\nThe engine itself reported: {why}");
    }
    if let Some(reason) = id_helpers_unusable() {
        return format!("{reason}\n\nThe engine itself reported: {why}");
    }
    with_userns_hint(why)
}

/// Is this failure about mapping a user namespace at all?
///
/// Deliberately broad. The cost of a false positive is a paragraph about
/// user namespaces under an unrelated error; the cost of a false negative
/// is the diagnosis never being printed.
fn looks_like_userns(why: &str) -> bool {
    why.contains("uid_map")
        || why.contains("gid_map")
        || why.contains("newuidmap")
        || why.contains("newgidmap")
        || why.contains("namespace")
}

/// Attach the AppArmor explanation when nothing definite explains it (T2).
///
/// **An explanation, never a refusal, and now a last resort.** Ubuntu 24.04
/// and later default `kernel.apparmor_restrict_unprivileged_userns=1`,
/// which gates unprivileged user namespaces behind an AppArmor profile: a
/// process that creates one without a profile permitting `userns` is
/// transitioned into the restricted `unprivileged_userns` profile and is
/// then denied its own `uid_map`. Fedora and the RHEL-likes use SELinux and
/// have no equivalent, so in a mixed fleet it is the Ubuntu box and only
/// the Ubuntu box.
///
/// Reached only from [`explain_failure`], after the two things this node
/// can check for itself have come back clean — and phrased as a suspect
/// with a way to confirm it, because AppArmor audits every refusal and the
/// absence of a `DENIED` line is real evidence rather than an inconclusive
/// test.
///
/// Consulted only on a failure, never up front: the sysctl being on is not
/// a fault. A machine with a podman profile installed has it on and works
/// perfectly, and refusing on the sysctl alone would take working nodes out
/// of the mesh.
fn with_userns_hint(why: String) -> String {
    if !looks_like_userns(&why) || !apparmor_restricts_userns() {
        return why;
    }
    userns_hint_text(&why)
}

/// The explanation itself, split out so it can be read and tested on a
/// machine that is not the one it describes.
fn userns_hint_text(why: &str) -> String {
    format!(
        concat!(
            "{}\n\n",
            "Nothing on this node is misconfigured that it can see: the sub-id helpers are ",
            "marked correctly and this unit allows them. What is left is one thing it ",
            "cannot verify — this kernel has `kernel.apparmor_restrict_unprivileged_userns=1` ",
            "(the Ubuntu 24.04+ default), which gates unprivileged user namespaces behind an ",
            "AppArmor profile.\n\n",
            "Confirm before acting, because AppArmor audits every refusal and this cause ",
            "leaves nothing else behind:\n",
            "  sudo journalctl -k --since -10min | grep -i 'apparmor=\"DENIED\"'\n\n",
            "A line naming podman, newuidmap or unprivileged_userns means it is this. Then:\n",
            "  printf 'abi <abi/4.0>,\\ninclude <tunables/global>\\n",
            "profile podman /usr/bin/podman flags=(unconfined) {{\\n  userns,\\n}}\\n' ",
            "| sudo tee /etc/apparmor.d/podman >/dev/null\n",
            "  sudo apparmor_parser -r /etc/apparmor.d/podman\n",
            "  sudo systemctl restart meshnode\n\n",
            "No DENIED line means it is not this, however plausible the sysctl looks. Then ",
            "the engine's own message above is the only evidence there is, and it is worth ",
            "reproducing by hand as the service user: ",
            "`sudo su -s /bin/sh -l -c 'podman run --rm alpine true' <user>`."
        ),
        why
    )
}

/// Does this kernel gate unprivileged user namespaces behind AppArmor?
///
/// Absent on every non-Ubuntu machine, which reads as *no* — the file not
/// existing is the ordinary case and not something to report.
fn apparmor_restricts_userns() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() != "0")
}

/// Is this engine running without root?
///
/// Best-effort and never fatal: podman answers directly, docker has no
/// equivalent one-liner that works across versions, so it is treated as
/// rootless only when the daemon itself is in a rootless installation —
/// which its context endpoint gives away. A wrong answer here costs a word
/// in a status line and one field in the declared controls, never a
/// placement.
async fn rootless(kind: EngineKind, global: &[String]) -> bool {
    let args: &[&str] = match kind {
        EngineKind::Podman => &["info", "--format", "{{.Host.Security.Rootless}}"],
        EngineKind::Docker => &["info", "--format", "{{.SecurityOptions}}"],
    };
    let Ok(Some(out)) = run_probe(kind.program(), &with_global(global, args)).await else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    match kind {
        EngineKind::Podman => text.trim() == "true",
        EngineKind::Docker => text.contains("rootless"),
    }
}

/// Spawn a probe with a timeout. `Ok(None)` = it did not finish in time.
async fn run_probe<S: AsRef<std::ffi::OsStr>>(
    program: &str,
    args: &[S],
) -> std::io::Result<Option<std::process::Output>> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(out) => out.map(Some),
        Err(_) => Ok(None),
    }
}

/// What a detection attempt found, in enough detail to explain itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The engine to use, if any is usable.
    pub engine: Option<Engine>,
    /// Engines whose program is installed but would not answer, and why.
    ///
    /// **The state worth distinguishing**: "docker is installed but its
    /// daemon is stopped" is a different sentence, with a different fix,
    /// from "docker is not installed". Collapsing them is how somebody
    /// spends an afternoon installing what they already have.
    pub broken: Vec<(EngineKind, String)>,
}

/// Find a usable container engine on this machine.
///
/// Tries [`PREFERENCE`] in order and takes the first that answers. A
/// `prefer` that names an engine moves it to the front — it never restricts
/// the search, because a preference that turned into an outage the day that
/// engine stopped would be a worse setting than no setting.
pub async fn detect(prefer: Option<EngineKind>, service: Option<&Path>) -> Detected {
    let mut order: Vec<EngineKind> = PREFERENCE.to_vec();
    if let Some(first) = prefer {
        order.retain(|k| *k != first);
        order.insert(0, first);
    }
    let mut broken = Vec::new();
    let mut engine = None;
    for kind in order {
        match probe(kind, service).await {
            Ok(Some(found)) if engine.is_none() => engine = Some(found),
            // Keep probing even once one is found: the others' state is
            // what `doctor` reports, and a machine with a broken docker
            // beside a working podman is worth a sentence.
            Ok(_) => {}
            Err(e) => broken.push((kind, e.to_string())),
        }
    }
    Detected { engine, broken }
}

/// The runtime report for containers on this node (T1).
pub fn report(found: &Detected, plugin: Option<&str>) -> mesh_proto::runtimes::RuntimeReport {
    use mesh_proto::Runtime;
    use mesh_proto::runtimes::RuntimeReport;

    // An explicit plugin wins the resolution order, so it wins the report:
    // saying "no engine found" on a node that runs containers through a
    // plugin would be the status line contradicting the machine.
    if let Some(plugin) = plugin.filter(|p| !p.trim().is_empty()) {
        return RuntimeReport::ready(Runtime::Container, format!("plugin {plugin}"));
    }
    match (&found.engine, found.broken.first()) {
        (Some(engine), _) => RuntimeReport::ready(Runtime::Container, engine.detail()),
        (None, Some((kind, why))) => RuntimeReport::broken(
            Runtime::Container,
            if why.starts_with(PROC_LOCKED_HEADLINE) {
                format!(
                    "{} cannot run containers from inside this service",
                    kind.as_str()
                )
            } else {
                format!("{} is installed but not answering", kind.as_str())
            },
            // Only suggest the alternative engine when the reason does not
            // already end in what to do. "…restart meshnode — fix it, or
            // install podman, which needs no daemon" is advice contradicting
            // the three commands directly above it, about an engine that is
            // already installed.
            //
            // A multi-line reason is a headline with an explanation under
            // it (see `RuntimeReport::remedy`), and appending to that would
            // land the advice at the bottom of the explanation instead of
            // on the headline where it belongs.
            if why.contains('\n') || why.contains("sudo ") || why.contains("setcap ") {
                why.clone()
            } else {
                format!("{why} — fix it, or install podman, which needs no daemon")
            },
        ),
        (None, None) => RuntimeReport::absent(
            Runtime::Container,
            format!(
                "no container engine on this machine — {}. meshware uses \
                 whichever is present and needs no further configuration",
                PREFERENCE
                    .iter()
                    .map(|k| k.remedy())
                    .collect::<Vec<_>>()
                    .join(", or ")
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// Where containers are made
// ---------------------------------------------------------------------------
//
// **Not inside `meshnode.service`, on a machine where that unit is the
// packaged one.** It sets `ProtectKernelTunables=` and `ProtectKernelLogs=`,
// which put read-only mounts over parts of `/proc` — `/proc/sys`,
// `/proc/sysrq-trigger`, `/proc/kmsg` and more — and a rootless container
// needs a `/proc` of its own. The kernel refuses to mount one inside a user
// namespace while any of the existing `/proc` is covered
// (`mount_too_revealing`): a fresh procfs would reveal what the covering
// hid. So every rootless container started from inside the unit failed,
// the first two ways found on a real fleet being
//
//   open `/proc/sys/net/ipv4/ping_group_range`: Read-only file system
//   mount `proc` to `proc`: Operation not permitted
//
// — the second being what is left once the first is cleared. Nothing
// inside the unit can undo it: the covering mounts are locked into every
// user namespace made below them. Lifting the two directives would, and
// would weaken the daemon that faces the network in order to suit a child
// process that does not.
//
// So a system node drives containers through **its account's own podman
// service**: podman's stock `podman.socket`, in the account's systemd user
// manager (lingering, which the installer already turns on for the
// sub-id setup), with its socket moved to `run/podman.sock` in the node's
// home — the one place both ends can reach, since `ProtectHome=` hides
// `/run/user` from the unit. podman there runs exactly as podman is
// designed to run rootless, with a `/proc` of its own and a user session
// for its cgroups, and the node runs `podman --remote`. Docker has always
// worked this way: the daemon makes the container, not the client.
//
// What that buys the unit is the reverse of a relaxation: it no longer
// needs `NoNewPrivileges=false` and four capabilities in its bounding set
// for `newuidmap`, because it never execs `newuidmap`. The installer
// removes that drop-in.
//
// Run directly is still right everywhere the node is not so confined — a
// person's own node under `systemctl --user`, a machine with no systemd —
// and is what the node does when there is no socket.

/// `--remote --url unix://…` for a podman service, nothing run directly.
fn remote_args(remote: Option<&Path>) -> Vec<String> {
    match remote {
        Some(socket) => vec![
            "--remote".into(),
            "--url".into(),
            format!("unix://{}", socket.display()),
        ],
        None => Vec::new(),
    }
}

/// `global` and then `args`.
fn with_global(global: &[String], args: &[&str]) -> Vec<String> {
    global
        .iter()
        .cloned()
        .chain(args.iter().map(|a| (*a).to_string()))
        .collect()
}

/// Is there a socket at `path` — something listening, or able to be
/// activated, rather than a stale file or nothing?
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

/// Which of `--cpus` and `--memory` a podman service can enforce, read
/// from the cgroup controllers it reports. Unreadable is *both*: a guess
/// that turns out wrong costs one container's start, and a wrong refusal
/// would drop a limit somebody asked for.
async fn controllers(kind: EngineKind, global: &[String]) -> (bool, bool) {
    let args = with_global(global, &["info", "--format", "{{.Host.CgroupControllers}}"]);
    let Ok(Some(out)) = run_probe(kind.program(), &args).await else {
        return (true, true);
    };
    if !out.status.success() {
        return (true, true);
    }
    parse_controllers(&String::from_utf8_lossy(&out.stdout))
}

/// `[cpu io memory pids]`, podman's rendering of a list, to (cpu, memory).
fn parse_controllers(reported: &str) -> (bool, bool) {
    let names: Vec<&str> = reported
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split_whitespace()
        .collect();
    if names.is_empty() {
        return (true, true);
    }
    (names.contains(&"cpu"), names.contains(&"memory"))
}

/// Mount points under `/proc` that cover nothing, and so do not stop a new
/// `/proc` being mounted beside them: the kernel's permanently empty
/// directories, made to be mounted on.
const EMPTY_PROC_MOUNTPOINTS: [&str; 2] = ["/proc/sys/fs/binfmt_misc", "/proc/fs/nfsd"];

/// The mounts over parts of `/proc` in this process's view, from
/// `/proc/self/mountinfo` — its fifth field is the mount point.
fn covered_proc_paths(mountinfo: &str) -> Vec<String> {
    let mut covered: Vec<String> = mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .filter(|p| p.starts_with("/proc/") && !EMPTY_PROC_MOUNTPOINTS.contains(p))
        .map(str::to_string)
        .collect();
    covered.sort();
    covered.dedup();
    covered
}

/// The first line of [`proc_locked`]'s reason, which [`report`] reads to
/// say something truer than "not answering".
const PROC_LOCKED_HEADLINE: &str = "podman cannot start a container from inside this service, and this node has no podman \
     service of its own to start them";

/// `Some(reason)` when podman run directly by this process cannot start a
/// rootless container at all, because parts of its `/proc` are covered.
///
/// Read from the mounts rather than inferred from a unit file, like
/// [`namespace_mapping_forbidden`]. Unreadable is *not locked*: unknown is
/// never a refusal.
fn proc_locked(service: Option<&Path>) -> Option<String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let covered = covered_proc_paths(&mountinfo);
    if covered.is_empty() {
        return None;
    }
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "meshnode".into());
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
    Some(proc_locked_text(&covered, service, &user, home.as_deref()))
}

/// The sentence itself, separate so it can be read and tested on a machine
/// that is not in the state it describes.
fn proc_locked_text(
    covered: &[String],
    service: Option<&Path>,
    user: &str,
    home: Option<&str>,
) -> String {
    const SHOWN: usize = 3;
    let mut shown = covered
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if covered.len() > SHOWN {
        shown.push_str(&format!(" and {} more", covered.len() - SHOWN));
    }
    let why = format!(
        "This service has read-only or hidden mounts over parts of /proc ({shown}) — \
         ProtectKernelTunables= and ProtectKernelLogs= put them there in the packaged unit. A \
         rootless container needs a /proc of its own, and the kernel refuses to mount one while \
         any of /proc is covered, so every container started from in here fails with `mount \
         proc to proc: Operation not permitted`. No setting inside the service changes that."
    );
    let fix = match (service, home) {
        (Some(socket), Some(home)) => {
            let socket = socket.display();
            format!(
                "\n\nmeshnode starts containers through its account's own podman service instead, \
                 outside this one, listening at {socket} — and nothing listens there yet. Update \
                 this machine over SSH to set it up (Director: Mesh › Updates › Update over SSH, \
                 with Reinstall if it is already on this version), or do it as root:\n  \
                 loginctl enable-linger {user}\n  \
                 mkdir -p {home}/.config/systemd/user/podman.socket.d\n  \
                 printf '[Socket]\\nListenStream=\\nListenStream={socket}\\nSocketMode=0600\\n' \
                 > {home}/.config/systemd/user/podman.socket.d/50-meshware.conf\n  \
                 chown -R {user}: {home}/.config\n  \
                 systemctl --user -M {user}@ daemon-reload\n  \
                 systemctl --user -M {user}@ enable --now podman.socket\n  \
                 systemctl restart meshnode"
            )
        }
        _ => String::new(),
    };
    format!("{PROC_LOCKED_HEADLINE}\n\n{why}{fix}")
}

// ---------------------------------------------------------------------------
// Running one
// ---------------------------------------------------------------------------

/// Where a bundle is mounted inside the container.
pub use crate::containers::DEFAULT_BUNDLE_MOUNT;

/// The variables a container engine needs from the daemon's own environment.
///
/// **meshware runs every job with `env_clear()`** — deliberately, so a job
/// sees exactly what it was given and nothing the daemon happened to be
/// holding. Rootless podman does not survive that: without
/// `XDG_RUNTIME_DIR` it cannot find its own runtime directory and the first
/// run fails with `creating events dirs: permission denied`.
///
/// So the exceptions are named here, one short list, rather than by
/// widening the executor. The job still sees nothing it was not given; the
/// *engine* sees what it needs to start.
const ENGINE_ENV: [&str; 5] = [
    "XDG_RUNTIME_DIR",
    "HOME",
    "PATH",
    "CONTAINERS_CONF",
    "CONTAINERS_STORAGE_CONF",
];

/// The label that says which node started a container (N6's reaper).
pub const NODE_LABEL: &str = "meshware.node";

/// The container name for a job: `meshware-<job id>`.
pub fn container_name(job_id: &str) -> String {
    format!("meshware-{job_id}")
}

impl Engine {
    /// Build the command that runs this container.
    ///
    /// Ported from the reference plugin, argument for argument, with every
    /// reason it gives kept — those reasons were each paid for by a stock
    /// image failing on a live mesh.
    pub fn plan(&self, req: &PrepareRequest) -> Result<PrepareReply> {
        if req.image.trim().is_empty() {
            return Err(CoreError::Job("no image to run".into()));
        }
        if !crate::containers::is_plausible_image(&req.image) {
            return Err(CoreError::Job(format!(
                "{:?} is not a usable image reference",
                req.image
            )));
        }
        let mut args: Vec<String> = self.global_args();
        let push = |args: &mut Vec<String>, s: &str| args.push(s.to_string());

        // `--rm` so a finished container leaves nothing behind: meshware
        // owns the job's lifecycle and its logs, and a stopped container
        // nobody reconciles is state nobody reconciles.
        push(&mut args, "run");
        push(&mut args, "--rm");
        push(&mut args, "--init");

        // **Published ports when the job declared `to`, host networking
        // otherwise**, and this is the difference between running a stock
        // image and rewriting one.
        //
        // `to = 80` says *my app listens on 80 inside the container*, so
        // the node's allocated host port is published to it and
        // `nginx:alpine` runs completely unmodified. Without the mapping,
        // host networking means the only way to use an allocated port is
        // for the workload to read `$MESHWARE_PORT_<LABEL>` and bind it —
        // which off-the-shelf images do not do and cannot be made to do
        // without a command override.
        //
        // Host networking stays the default when nothing declares `to`,
        // because it is right for the case it was chosen for: a job
        // meshware itself built, which reads its port from the environment,
        // and for which a NAT between the registry's address and the
        // listener would be a lie.
        let mapped: Vec<&crate::containers::PreparePort> =
            req.ports.iter().filter(|p| p.to > 0).collect();
        if mapped.is_empty() {
            push(&mut args, "--network=host");
        } else {
            for p in &mapped {
                push(&mut args, "--publish");
                args.push(format!("{}:{}", p.host, p.to));
            }
        }

        if !req.job_id.trim().is_empty() {
            push(&mut args, "--name");
            args.push(container_name(&req.job_id));
        }
        // Whose container this is. The reaper filters on it, so a machine
        // running two nodes never has one removing the other's work.
        if !req.node.trim().is_empty() {
            push(&mut args, "--label");
            args.push(format!("{NODE_LABEL}={}", req.node));
        }

        // The equivalents of the controls that cannot reach in.
        push(&mut args, "--read-only");
        push(&mut args, "--security-opt");
        push(&mut args, self.kind.no_new_privileges());

        // Drop everything, then add back the five an ordinary image needs
        // to *start* and then give up privileges.
        //
        // `--cap-drop=ALL` alone is the stricter claim and it makes stock
        // images unrunnable: `nginx:alpine` fails with
        //
        //   nginx: [emerg] chown("/var/cache/nginx/client_temp", 101)
        //          failed (1: Operation not permitted)
        //
        // because its entry point chowns its cache and drops to an
        // unprivileged user — which is the image doing the *right* thing
        // and needing CHOWN, SETUID and SETGID to do it. NET_BIND_SERVICE
        // is for the reason a web-server image exists at all: it listens on
        // 80 inside the container.
        //
        // What stays dropped is the set that matters: SYS_ADMIN,
        // SYS_PTRACE, NET_RAW, NET_ADMIN, SYS_MODULE, MKNOD and the rest.
        push(&mut args, "--cap-drop=ALL");
        for cap in [
            "CHOWN",
            "SETUID",
            "SETGID",
            "DAC_OVERRIDE",
            "NET_BIND_SERVICE",
        ] {
            args.push(format!("--cap-add={cap}"));
        }

        // Writable tmpfs for the paths a read-only rootfs actually needs.
        //
        // `/tmp` alone is not enough for stock images, and that mattered
        // more than it sounds: `nginx:alpine` — the most ordinary container
        // anybody will try first — dies at startup with
        //
        //   nginx: [emerg] mkdir() "/var/cache/nginx/client_temp" failed
        //          (30: Read-only file system)
        //
        // A hardened rootfs that only works for images written to expect
        // one is a hardened rootfs nobody keeps switched on. These five are
        // what a conventional read-only container setup grants, they are
        // all tmpfs so nothing survives the container, and `nosuid,nodev`
        // keeps them boring.
        for (path, size) in [
            ("/tmp", "64m"),
            ("/run", "16m"),
            ("/var/run", "16m"),
            ("/var/cache", "64m"),
            ("/var/log", "32m"),
        ] {
            push(&mut args, "--tmpfs");
            args.push(format!("{path}:rw,nosuid,nodev,size={size}"));
        }

        // Only what the engine can enforce: `controls()` says which, and a
        // `--cpus` a podman service's user manager was never delegated the
        // controller for fails the container rather than being ignored.
        if req.cpus > 0 && self.cpu_limit {
            push(&mut args, "--cpus");
            args.push(req.cpus.to_string());
        }
        if req.mem_mb > 0 && self.memory_limit {
            push(&mut args, "--memory");
            args.push(format!("{}m", req.mem_mb));
        }
        // `--cgroup-parent`, but **only when the engine will accept it**.
        //
        // meshware delegates a cgroupfs path (`…/meshnode.service/jobs`),
        // and podman under the systemd cgroup manager demands a `.slice`
        // name instead — passing the path gets `did not receive systemd
        // slice as cgroup parent`, and the job fails before the image is
        // consulted.
        //
        // So it is passed only when it is already a slice. Where it is not,
        // the container lands in the engine's own tree and the ceilings
        // still apply: `--memory` and `--cpus` are enforced either way,
        // which is what `controls()` claims. What is lost is the accounting
        // being in one tree, not the limit.
        if let Some(parent) = req
            .cgroup_parent
            .as_deref()
            .filter(|p| p.ends_with(".slice"))
        {
            push(&mut args, "--cgroup-parent");
            args.push(parent.to_string());
        }

        // The bundle is the *code*; the image is the base. Read-only, and
        // `:Z` so SELinux relabels it where SELinux is enforcing.
        if let Some(root) = req.bundle_root.as_deref().filter(|r| !r.is_empty()) {
            let mount = if req.bundle_mount.trim().is_empty() {
                DEFAULT_BUNDLE_MOUNT
            } else {
                req.bundle_mount.as_str()
            };
            push(&mut args, "-v");
            args.push(format!("{root}:{mount}:ro,Z"));
        }

        // Volumes this job has **already claimed** (N5). The claim is a row
        // in the node's own lease table, taken before this ran; this mounts
        // what it is given and never claims — which is what keeps the claim
        // and the mount from being two facts that can disagree.
        for v in &req.volumes {
            push(&mut args, "-v");
            args.push(format!(
                "{}:{}:{},Z",
                v.source,
                v.target,
                if v.read_only { "ro" } else { "rw" }
            ));
        }

        if !req.workdir.trim().is_empty() {
            push(&mut args, "--workdir");
            args.push(req.workdir.clone());
        }

        // The command override, which both engines spell `--entrypoint`.
        //
        // Nomad's `config { command = … }` and the same semantics: absent
        // means the image runs whatever it was built to run, which is right
        // for an image built to be run and useless for a general-purpose
        // base like alpine.
        if let Some(cmd) = req.command.as_deref().filter(|c| !c.trim().is_empty()) {
            push(&mut args, "--entrypoint");
            args.push(cmd.to_string());
        }

        // The allocated host port, in the environment, **either way**.
        //
        // Under host networking it is the port the job must bind. Under a
        // published mapping the job binds its own `to` and does not need
        // it — but something inside still often wants to know the address
        // it is reachable at, so it is passed regardless rather than being
        // a variable that exists in one mode and not the other.
        for p in &req.ports {
            push(&mut args, "--env");
            args.push(format!(
                "{}{}={}",
                mesh_proto::PORT_ENV_PREFIX,
                mesh_proto::ports::label_key_suffix(&p.label),
                p.host
            ));
        }
        // Each volume's variable names where it is mounted **inside**, not
        // the host directory the job's environment holds: that path does
        // not exist in the container.
        for v in &req.volumes {
            push(&mut args, "--env");
            args.push(format!(
                "{}{}={}",
                mesh_proto::VOLUME_ENV_PREFIX,
                mesh_proto::volumes::key_suffix(&v.name),
                v.target
            ));
        }

        // Everything else the job was given goes in here, by name — see
        // `PrepareReply::forward_env_at`.
        let forward_env_at = Some(args.len());
        args.push(req.image.clone());
        args.extend(req.args.iter().cloned());

        Ok(PrepareReply {
            program: self.program.clone(),
            args,
            env: engine_env(),
            workdir: String::new(),
            controls: self.controls(),
            forward_env_at,
        })
    }
}

/// The engine's own environment needs, read from this process.
pub fn engine_env() -> Vec<(String, String)> {
    ENGINE_ENV
        .iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| ((*k).to_string(), v))
        })
        .collect()
}

/// Remove one container, by job id. Idempotent: already gone is success.
pub async fn stop(engine: &Engine, job_id: &str) -> Result<()> {
    let name = container_name(job_id);
    let args: Vec<&str> = match engine.kind {
        // `--ignore` makes a container that is already gone a success,
        // which is the common case — this runs on every exit, and a cleanly
        // finished `--rm` container removed itself already.
        EngineKind::Podman => vec!["rm", "--force", "--ignore", &name],
        // Docker has no `--ignore`; a missing container is an error there,
        // and is filtered out below rather than reported.
        EngineKind::Docker => vec!["rm", "--force", &name],
    };
    let out = run_probe(&engine.program, &engine.invocation(&args))
        .await
        .map_err(|e| CoreError::Job(format!("{}: {e}", engine.program)))?
        .ok_or_else(|| CoreError::Job(format!("{} timed out removing {name}", engine.program)))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    // "No such container" is the idempotent case, spelled differently by
    // each engine. Treating it as a failure would make every ordinary job
    // exit log an error.
    if stderr.contains("no such container") || stderr.contains("not found") {
        return Ok(());
    }
    Err(CoreError::Job(format!(
        "{} could not remove {name}: {}",
        engine.program,
        stderr.trim()
    )))
}

/// Run a deployment's `exec` health check **inside** its container
/// (Phase A, `docs/NOMAD-ENHANCEMENTS.md` §2.5b).
///
/// The check used to run `sh -c` on the host, which for a container asks
/// the wrong machine: the host has none of the image's files, a different
/// `curl` or none, and a process table where the service is somebody
/// else's child. Running it in the container's own namespaces is the only
/// vantage point from which the answer is about the service — the same
/// place Nomad's script check runs. It needs a shell in the image, which
/// the failure detail says when there is not one.
pub async fn exec_check(
    engine: &Engine,
    job_id: &str,
    command: &str,
    timeout: std::time::Duration,
) -> (bool, String) {
    let name = container_name(job_id);
    let child = tokio::process::Command::new(&engine.program)
        .args(engine.invocation(&["exec", name.as_str(), "sh", "-c", command]))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return (false, format!("exec: {}: {e}", engine.program)),
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => (true, String::new()),
        Ok(Ok(out)) => {
            let code = out.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let first = stderr
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("");
            // 125–127 are the engine saying the *exec* failed — no such
            // container, no `sh` in the image — which is a different
            // sentence from the check itself answering no, and the one a
            // person needs to see to know the check never ran.
            if (125..=127).contains(&code) && !first.is_empty() {
                (false, format!("exec: {first}"))
            } else {
                (false, format!("exec: exit {code}"))
            }
        }
        Ok(Err(e)) => (false, format!("exec: {e}")),
        Err(_) => (false, "exec: timed out".into()),
    }
}

/// Job ids for containers this node started that are still running.
///
/// Filtered on [`NODE_LABEL`], which is what makes it safe: two nodes on
/// one machine each see the other's containers, and a reap that went by
/// name alone would have one node remove the other's work on every restart.
pub async fn list_jobs(engine: &Engine, node: &str) -> Result<Vec<String>> {
    let filter = format!("label={NODE_LABEL}={node}");
    let args = ["ps", "--filter", filter.as_str(), "--format", "{{.Names}}"];
    let out = run_probe(&engine.program, &engine.invocation(&args))
        .await
        .map_err(|e| CoreError::Job(format!("{}: {e}", engine.program)))?
        .ok_or_else(|| CoreError::Job(format!("{} timed out listing", engine.program)))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(CoreError::Job(format!(
            "{} cannot list containers: {}",
            engine.program,
            stderr.lines().next().unwrap_or("no reason given").trim()
        )));
    }
    Ok(parse_names(&String::from_utf8_lossy(&out.stdout)))
}

/// Container names to job ids, dropping anything that is not ours.
///
/// Split out because the parsing is the part that can be wrong, and it is
/// wrong in a way nothing downstream would catch: a blank line becoming an
/// empty job id, and `stop` then removing a container literally named
/// `meshware-`.
fn parse_names(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter_map(|n| n.strip_prefix("meshware-"))
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// Remove every container this node left behind, before it places anything.
///
/// The engine half of [`crate::containers::reap_orphans`]: `stop` covers the
/// job that ends, and nothing covered the *node* that ends. Kill `meshnode`
/// and conmon keeps every container it started alive — holding its host
/// ports, answering to nobody, and invisible to a ledger correctly rebuilt
/// from an op log that says those jobs are over.
///
/// Best-effort and loud. Refusing to start over a stale container would
/// turn a tidy-up into an outage; staying quiet about one would leave a
/// host port held by nothing anybody can find.
pub async fn reap_orphans(engine: &Engine, node: &str) {
    let orphans = match list_jobs(engine, node).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                "could not check for containers left by a previous run: {e} — \
                 any that exist still hold their ports; `{} ps` will show them",
                engine.program
            );
            return;
        }
    };
    if orphans.is_empty() {
        return;
    }
    tracing::warn!(
        "{} container(s) outlived the last run of this node; removing them",
        orphans.len()
    );
    for job in &orphans {
        match stop(engine, job).await {
            Ok(()) => tracing::info!(job = %job, "removed a container left by a previous run"),
            Err(e) => tracing::warn!(job = %job, "could not remove it: {e}"),
        }
    }
}

/// A map of every engine's state, for `doctor` and the runtimes table.
pub fn states(found: &Detected) -> BTreeMap<EngineKind, String> {
    let mut out = BTreeMap::new();
    if let Some(engine) = &found.engine {
        out.insert(engine.kind, engine.detail());
    }
    for (kind, why) in &found.broken {
        out.entry(*kind).or_insert_with(|| format!("broken: {why}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::{PreparePort, PrepareVolume};

    fn req() -> PrepareRequest {
        PrepareRequest {
            version: crate::containers::CONTRACT_VERSION,
            image: "docker.io/library/nginx:alpine".into(),
            command: None,
            args: Vec::new(),
            workdir: String::new(),
            bundle_root: None,
            bundle_mount: DEFAULT_BUNDLE_MOUNT.into(),
            job_dir: "/var/lib/meshware/jobs/x".into(),
            cpus: 0,
            mem_mb: 0,
            ports: Vec::new(),
            volumes: Vec::new(),
            cgroup_parent: None,
            job_id: "job-abc".into(),
            node: "n-1".into(),
        }
    }

    fn podman() -> Engine {
        Engine {
            kind: EngineKind::Podman,
            program: "podman".into(),
            version: "5.8.4".into(),
            rootless: true,
            remote: None,
            cpu_limit: true,
            memory_limit: true,
        }
    }

    /// The same podman, reached through a system node's own service.
    fn podman_service() -> Engine {
        Engine {
            remote: Some(PathBuf::from("/var/lib/meshware/run/podman.sock")),
            ..podman()
        }
    }

    /// The case the whole N-track alignment was about: a port with `to` set
    /// publishes a mapping, so a stock image runs unmodified.
    #[test]
    fn a_declared_port_is_published_not_host_networked() {
        let mut r = req();
        r.ports = vec![PreparePort {
            label: "http".into(),
            host: 24000,
            to: 80,
        }];
        let plan = podman().plan(&r).expect("plan");
        assert!(plan.args.contains(&"--publish".to_string()));
        assert!(plan.args.contains(&"24000:80".to_string()));
        assert!(
            !plan.args.iter().any(|a| a == "--network=host"),
            "a mapped port and host networking are contradictory"
        );
        // …and the allocated port is still in the environment, because
        // something inside often wants to know the address it answers at.
        assert!(plan.args.contains(&"MESHWARE_PORT_HTTP=24000".to_string()));
    }

    /// A label with a `-` names the same variable the executor sets and the
    /// registry reads, not `MESHWARE_PORT_ADMIN-API`, which no shell can
    /// read. Volumes are named by where they are *inside*. And the job's
    /// own environment goes in before the image, where `run` reads flags.
    #[test]
    fn the_container_is_told_what_the_job_was_told() {
        let mut r = req();
        r.ports = vec![PreparePort {
            label: "admin-api".into(),
            host: 24001,
            to: 0,
        }];
        r.volumes = vec![crate::containers::PrepareVolume {
            name: "hot-cache".into(),
            source: "/var/lib/meshware/volumes/hot-cache".into(),
            target: "/meshware/volumes/hot-cache".into(),
            read_only: false,
        }];
        r.args = vec!["-c".into(), "env".into()];
        let plan = podman().plan(&r).expect("plan");
        assert!(
            plan.args
                .contains(&"MESHWARE_PORT_ADMIN_API=24001".to_string())
        );
        assert!(
            plan.args
                .contains(&"MESHWARE_VOLUME_HOT_CACHE=/meshware/volumes/hot-cache".to_string())
        );
        let at = plan
            .forward_env_at
            .expect("the built-in engine forwards the environment");
        assert_eq!(plan.args[at], r.image, "flags go in just before the image");
    }

    /// …and with nothing declared, host networking, because that is right
    /// for a job meshware built which reads its port from the environment.
    #[test]
    fn no_declared_port_means_host_networking() {
        let mut r = req();
        r.ports = vec![PreparePort {
            label: "http".into(),
            host: 24000,
            to: 0,
        }];
        let plan = podman().plan(&r).expect("plan");
        assert!(plan.args.contains(&"--network=host".to_string()));
        assert!(!plan.args.iter().any(|a| a == "--publish"));
        assert!(plan.args.contains(&"MESHWARE_PORT_HTTP=24000".to_string()));
    }

    /// The five capabilities that make stock images work, and the read-only
    /// rootfs plus tmpfs set that makes `nginx:alpine` survive it. Both
    /// were paid for by a live mesh.
    #[test]
    fn the_hardening_matches_what_controls_claims() {
        let plan = podman().plan(&req()).expect("plan");
        assert!(plan.args.contains(&"--read-only".to_string()));
        assert!(plan.args.contains(&"--cap-drop=ALL".to_string()));
        for cap in [
            "CHOWN",
            "SETUID",
            "SETGID",
            "DAC_OVERRIDE",
            "NET_BIND_SERVICE",
        ] {
            assert!(
                plan.args.contains(&format!("--cap-add={cap}")),
                "{cap} is needed for a stock image to start and drop privileges"
            );
        }
        for path in ["/tmp", "/run", "/var/run", "/var/cache", "/var/log"] {
            assert!(
                plan.args
                    .iter()
                    .any(|a| a.starts_with(&format!("{path}:rw,nosuid,nodev"))),
                "{path} must be writable or stock images die on a read-only rootfs"
            );
        }
        // The claim and the flags come from one file; this is the test that
        // keeps them from drifting inside it.
        let c = plan.controls;
        assert!(c.readonly_rootfs && c.drop_capabilities && c.no_new_privileges);
        assert!(c.memory && c.cpu && c.rootless);
    }

    #[test]
    fn limits_volumes_and_the_bundle_are_passed() {
        let mut r = req();
        r.cpus = 2;
        r.mem_mb = 512;
        r.bundle_root = Some("/var/lib/meshware/jobs/x/checkout".into());
        r.volumes = vec![PrepareVolume {
            name: "data".into(),
            source: "/srv/data".into(),
            target: "/data".into(),
            read_only: true,
        }];
        let plan = podman().plan(&r).expect("plan");
        let joined = plan.args.join(" ");
        assert!(joined.contains("--cpus 2"));
        assert!(joined.contains("--memory 512m"));
        assert!(joined.contains("/var/lib/meshware/jobs/x/checkout:/meshware/bundle:ro,Z"));
        assert!(joined.contains("/srv/data:/data:ro,Z"));
    }

    /// A cgroup *path* is dropped and a `.slice` is passed. Passing the
    /// path gets `did not receive systemd slice as cgroup parent` and the
    /// job fails before the image is consulted.
    #[test]
    fn only_a_slice_reaches_cgroup_parent() {
        let mut r = req();
        r.cgroup_parent = Some("/sys/fs/cgroup/meshnode.service/jobs".into());
        assert!(
            !podman()
                .plan(&r)
                .expect("plan")
                .args
                .iter()
                .any(|a| a == "--cgroup-parent")
        );
        r.cgroup_parent = Some("meshware.slice".into());
        let plan = podman().plan(&r).expect("plan");
        assert!(plan.args.contains(&"meshware.slice".to_string()));
    }

    /// The image is the last argument before the job's own, so nothing the
    /// job passes can be read as a flag to the engine.
    #[test]
    fn the_image_comes_last_and_job_args_follow_it() {
        let mut r = req();
        r.args = vec!["--verbose".into(), "run".into()];
        let plan = podman().plan(&r).expect("plan");
        let at = plan
            .args
            .iter()
            .position(|a| a == "docker.io/library/nginx:alpine")
            .expect("the image");
        assert_eq!(
            &plan.args[at + 1..],
            &["--verbose".to_string(), "run".to_string()]
        );
    }

    #[test]
    fn an_implausible_image_is_refused() {
        let mut r = req();
        r.image = "nginx; rm -rf /".into();
        assert!(podman().plan(&r).is_err());
        r.image = String::new();
        assert!(podman().plan(&r).is_err());
    }

    /// Docker spells the flag with a value; podman does not. Getting this
    /// wrong makes every container fail on one engine and nothing on the
    /// other, which is the worst kind of portability bug to find in
    /// production.
    #[test]
    fn each_engine_gets_its_own_spelling() {
        let mut docker = podman();
        docker.kind = EngineKind::Docker;
        docker.program = "docker".into();
        assert!(
            docker
                .plan(&req())
                .expect("plan")
                .args
                .contains(&"no-new-privileges=true".to_string())
        );
        assert!(
            podman()
                .plan(&req())
                .expect("plan")
                .args
                .contains(&"no-new-privileges".to_string())
        );
    }

    /// The complaint an operator reads, not the dial stack behind it.
    #[test]
    fn only_the_useful_clause_survives() {
        let docker = "failed to connect to the docker API at unix:///var/run/docker.sock; \
                      check if the path is correct and if the daemon is running: dial unix \
                      /var/run/docker.sock: connect: no such file or directory";
        assert_eq!(
            first_clause(docker),
            "failed to connect to the docker API at unix:///var/run/docker.sock"
        );
        assert_eq!(first_clause(""), "no reason given");
        assert_eq!(first_clause("\n\n  boom  \n"), "boom");
        // One pathological line cannot grow without bound: the cap is on
        // the data, and the table truncates again for display.
        let long = "x".repeat(2000);
        assert!(first_clause(&long).chars().count() <= 600);
    }

    /// **The reason is on none of the lines — it is spread across them.**
    ///
    /// A rootless podman failure names the attempt first, the symptom last,
    /// and the actual cause in the middle. Keeping one line threw the cause
    /// away whichever one was picked, and that cost two round trips with
    /// somebody staring at a log that named `newuidmap` and nothing else.
    #[test]
    fn every_line_that_carries_signal_is_kept() {
        let podman = concat!(
            "time=\"2026-09-13T05:06:03-05:00\" level=error msg=\"running ",
            "`/usr/bin/newuidmap 171547 0 961 1 1 2000000 65536`\"\n",
            "newuidmap: write to uid_map failed: Operation not permitted\n",
            "Error: cannot set up namespace using \"/usr/bin/newuidmap\": ",
            "exit status 1"
        );
        let got = first_clause(podman);
        // The narration goes…
        assert!(!got.contains("level=error"), "{got}");
        // …the symptom stays…
        assert!(got.contains("cannot set up namespace"), "{got}");
        // …and so does the only line that says *why*, which is the whole
        // point of this function.
        assert!(got.contains("Operation not permitted"), "{got}");

        // Pull chatter from a failure that got as far as unpacking layers
        // is narration too.
        let pulling = concat!(
            "Trying to pull docker.io/library/nginx:alpine...\n",
            "Getting image source signatures\n",
            "Copying blob sha256:5190…\n",
            "Error: unpacking failed: potentially insufficient UIDs"
        );
        let got = first_clause(pulling);
        assert!(!got.contains("Copying blob"), "{got}");
        assert!(got.contains("insufficient UIDs"), "{got}");

        // A semicolon inside a reason is not a restatement and must not be
        // cut — only docker's `…; check if …` advice is.
        let keep = "Error: something failed; the reason is here";
        assert_eq!(first_clause(keep), keep);

        assert_eq!(first_clause(""), "no reason given");
    }

    /// A range with no way to apply it is the trap this catches:
    /// `podman info` reports the mapping either way, because it reads
    /// `/etc/subuid` rather than trying to use it.
    #[test]
    fn the_mapping_helpers_are_checked_by_name() {
        // Whatever this machine's state, the answer must be a sentence
        // naming both the tool and the fix, or nothing at all.
        if let Some(reason) = id_helpers_unusable() {
            assert!(reason.contains("newuidmap") || reason.contains("newgidmap"));
            assert!(
                reason.contains("setcap") || reason.contains("reinstall"),
                "the remedy must be in the message: {reason}"
            );
        }
    }

    /// **Two valid markings, and the harm is in having both.**
    ///
    /// Fedora ships `newuidmap` with `cap_setuid=ep` and no setuid bit;
    /// Debian and Ubuntu ship it setuid-root with no capability. Either
    /// works. Both together is the state `capabilities(7)` refuses to
    /// honour, and it is reachable by following advice that was written
    /// for the other distribution — which is exactly how a working Ubuntu
    /// node was broken by meshware's own documentation.
    #[test]
    fn a_helper_may_be_setuid_or_capable_but_never_both() {
        let m = |setuid_root, file_cap| {
            Marked::of(Marking {
                setuid_root,
                file_cap,
            })
        };
        assert_eq!(m(false, true), Marked::Correctly, "the Fedora marking");
        assert_eq!(m(true, false), Marked::Correctly, "the Debian marking");
        assert_eq!(m(true, true), Marked::Both);
        assert_eq!(m(false, false), Marked::NotAtAll);
    }

    /// **The drop-in this repository shipped was wrong on half the fleet.**
    ///
    /// `CapabilityBoundingSet=CAP_SETUID CAP_SETGID` is exactly right for a
    /// capability-marked helper and one capability short for a setuid-root
    /// one, which becomes root and then finds that euid 0 overrides no file
    /// permission without CAP_DAC_OVERRIDE. Both failures print the same
    /// line, which is why this is arithmetic rather than a message.
    #[test]
    fn a_bounding_set_is_measured_against_the_marking_it_serves() {
        let capability_marked = [(CAP_SETUID, "CAP_SETUID"), (CAP_SETGID, "CAP_SETGID")];
        let setuid_root = [
            (CAP_SETUID, "CAP_SETUID"),
            (CAP_SETGID, "CAP_SETGID"),
            (CAP_DAC_OVERRIDE, "CAP_DAC_OVERRIDE"),
            (CAP_SYS_ADMIN, "CAP_SYS_ADMIN"),
        ];
        // What the first drop-in granted: bits 6 and 7.
        let two = (1 << CAP_SETGID) | (1 << CAP_SETUID);
        assert!(missing_from_bounding(two, &capability_marked).is_empty());
        assert_eq!(
            missing_from_bounding(two, &setuid_root),
            vec!["CAP_DAC_OVERRIDE", "CAP_SYS_ADMIN"],
            "the Debian marking needs two capabilities the first drop-in clamped away"
        );

        // The second one. This is the mask a real APUS printed as `CapBnd:
        // 00000000000000c2` while podman still failed — right about the
        // open, still wrong about the write.
        let three = two | (1 << CAP_DAC_OVERRIDE);
        assert_eq!(three, 0xc2);
        assert!(missing_from_bounding(three, &capability_marked).is_empty());
        assert_eq!(
            missing_from_bounding(three, &setuid_root),
            vec!["CAP_SYS_ADMIN"],
            "a fix that moves the failure from the open to the write is not a fix"
        );

        // What it grants now.
        let four = three | (1 << CAP_SYS_ADMIN);
        assert_eq!(four, 0x2000c2);
        assert!(missing_from_bounding(four, &setuid_root).is_empty());
        assert!(missing_from_bounding(four, &capability_marked).is_empty());

        // The shipped unit's empty set, and a full one.
        assert_eq!(missing_from_bounding(0, &setuid_root).len(), 4);
        assert!(missing_from_bounding(u64::MAX, &setuid_root).is_empty());
    }

    /// **What APUS will read.** Printed rather than only asserted: two
    /// versions of this message shipped saying something untrue of the node
    /// reading them, and the only way that gets caught is by looking at it.
    #[test]
    fn the_bounding_set_message_names_the_capability_and_the_fix() {
        let text = mapping_forbidden_text(false, &["CAP_DAC_OVERRIDE", "CAP_SYS_ADMIN"]);
        println!("\n{text}\n");
        let (headline, body) = text.split_once("\n\n").expect("headline, then body");
        assert!(!headline.contains('\n'));
        assert!(headline.contains("CAP_DAC_OVERRIDE"), "{headline}");
        assert!(headline.contains("CAP_SYS_ADMIN"), "{headline}");
        assert!(headline.chars().count() < 180, "{headline}");
        // The body explains *why* those capabilities and not others, names
        // the distinct symptom each one produces, and ends in something
        // paste-able.
        assert!(body.contains("setuid-root"), "{body}");
        assert!(body.contains("open of uid_map failed"), "{body}");
        assert!(body.contains("write to uid_map failed"), "{body}");
        assert!(body.contains("systemctl daemon-reload"), "{body}");
        // CAP_SYS_ADMIN is large enough that saying so is part of saying
        // what to do, and the narrower alternative has to be offered.
        assert!(
            body.contains("CAP_SYS_ADMIN is a large capability"),
            "{body}"
        );
        assert!(body.contains("chmod u-s"), "{body}");

        // The no-new-privs case says nothing about DAC_OVERRIDE, which is
        // not what stops it.
        let nnp = mapping_forbidden_text(true, &[]);
        assert!(
            nnp.starts_with("this unit forbids user-id mapping"),
            "{nnp}"
        );
        assert!(!nnp.contains("euid 0 is not by itself"), "{nnp}");
    }

    /// The drop-in is generated for this machine, so it must actually name
    /// the capabilities this machine's helpers need — a paste-able block
    /// that grants two where three are wanted is the bug it exists to fix.
    #[test]
    fn the_dropin_grants_what_this_machine_needs() {
        let text = containers_dropin(&[]);
        assert!(text.contains("NoNewPrivileges=false"), "{text}");
        for (_, name) in caps_the_helpers_need() {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
        assert!(text.contains("daemon-reload"), "{text}");
    }

    /// **podman puts the reason inside the narration.** Dropping those
    /// lines left a log that said `cannot set up namespace` and nothing
    /// about why, which cost a round trip with somebody reading it.
    #[test]
    fn a_narrated_line_is_unwrapped_not_discarded() {
        let journald = r#"time="2026-09-13T07:25:30Z" level=error msg="running `/usr/bin/newuidmap 35061 0 1000 1 1 100000 65536`: newuidmap: open of uid_map failed: Permission denied""#;
        let got = narrated_message(journald).expect("a narrated line");
        assert!(got.contains("open of uid_map failed"), "{got}");
        assert!(!got.contains("level=error"), "{got}");

        // The terminal rendering of the same thing.
        let tty = "ERRO[0000] running `/usr/bin/newuidmap 35061 0 1000`: newuidmap: open of uid_map failed: Permission denied";
        let got = narrated_message(tty).expect("a tagged line");
        assert!(got.starts_with("running `"), "{got}");

        // An ordinary line is left exactly as it is.
        assert_eq!(narrated_message("Error: cannot set up namespace"), None);

        // …and end to end, the reason survives into what gets reported.
        let whole = format!(
            "{journald}\nError: cannot set up namespace using \"/usr/bin/newuidmap\": exit status 1\n"
        );
        let clause = first_clause(&whole);
        assert!(
            clause.contains("open of uid_map failed"),
            "the only line that says why was dropped: {clause}"
        );
    }

    /// A setuid bit on a file somebody else owns is not privilege, and
    /// treating it as one would report a broken helper as fine.
    #[test]
    fn setuid_means_setuid_root_specifically() {
        // /usr/bin/passwd is setuid-root on every distribution this runs
        // on; /bin/sh is on none of them. Both are read-only facts about
        // the machine, so this asserts the reading, not the marking.
        assert!(!is_setuid_root("/bin/sh"));
        assert!(!is_setuid_root("/nonexistent/newuidmap"));
    }

    /// **The failure a real fleet hit.** Three nodes installed from the
    /// package announced `container ready`, took the work, and died in the
    /// image unpack on `lchown /etc/shadow: invalid argument` — because
    /// `useradd --system` allocates no sub-id range and `podman info`
    /// answers perfectly well without one.
    #[test]
    fn a_user_namespace_too_small_for_an_image_is_recognised() {
        // A user with no /etc/subuid entry at all: one mapping, size 1.
        assert_eq!(mapped_ids("1 "), Some(1));
        assert!(mapped_ids("1 ").is_some_and(|n| n < MIN_SUB_IDS));

        // The ordinary allocation `useradd` gives a real user.
        assert_eq!(mapped_ids("1 65536 "), Some(65537));
        assert!(mapped_ids("1 65536 ").is_some_and(|n| n >= MIN_SUB_IDS));

        // A short range is still short — it does not fail gracefully at the
        // top, it fails on whichever file is owned above the limit.
        assert!(mapped_ids("1 1000 ").is_some_and(|n| n < MIN_SUB_IDS));

        // Nothing parseable is *unknown*, never "too small": a wrong
        // refusal takes a working node out of the mesh.
        assert_eq!(mapped_ids(""), None);
        assert_eq!(mapped_ids("   \n "), None);
    }

    /// **An explanation, never a refusal.** The sysctl being on is not a
    /// fault — a machine with a podman profile installed has it on and
    /// works — so the hint attaches only to a failure that looks like it,
    /// and never turns a working node into a broken one.
    #[test]
    fn the_apparmor_hint_attaches_only_where_it_applies() {
        let userns = "newuidmap: open of uid_map failed: Permission denied".to_string();
        let unrelated = "failed to connect to the docker API".to_string();

        // Whatever this machine is, an unrelated failure is never
        // decorated with a user-namespace explanation.
        assert_eq!(with_userns_hint(unrelated.clone()), unrelated);

        // …and the original reason is always preserved, hint or not: the
        // line an operator searches for must survive.
        assert!(with_userns_hint(userns.clone()).starts_with(&userns));

        // And the explanation itself carries the fix — checked directly,
        // so this holds on a machine that is not the one it describes.
        let hint = userns_hint_text(&userns);
        assert!(hint.starts_with(&userns), "the original reason comes first");
        assert!(
            hint.contains("apparmor_restrict_unprivileged_userns"),
            "{hint}"
        );
        assert!(hint.contains("apparmor_parser -r"), "{hint}");
        assert!(
            hint.contains("userns,"),
            "the profile must permit it: {hint}"
        );
        println!("---- what an Ubuntu node reports ----\n{hint}");
    }

    #[test]
    fn names_parse_to_job_ids() {
        assert_eq!(
            parse_names("meshware-job-abc\nmeshware-job-def\n"),
            vec!["job-abc".to_string(), "job-def".to_string()]
        );
        // A container that is not ours, and a blank line that must not
        // become an empty job id — `stop ""` would remove a container
        // literally named `meshware-`.
        assert!(parse_names("some-other-container\n\nmeshware-\n").is_empty());
    }

    /// A preference reorders the search; it never restricts it. A setting
    /// that turned into an outage the day that engine stopped would be
    /// worse than no setting.
    #[test]
    fn a_preference_reorders_but_does_not_restrict() {
        let mut order: Vec<EngineKind> = PREFERENCE.to_vec();
        let first = EngineKind::Docker;
        order.retain(|k| *k != first);
        order.insert(0, first);
        assert_eq!(order, vec![EngineKind::Docker, EngineKind::Podman]);
    }

    /// The report the operator reads, in each of its three shapes.
    #[test]
    fn the_report_says_which_kind_of_no() {
        use mesh_proto::runtimes::Detection;
        let none = Detected {
            engine: None,
            broken: Vec::new(),
        };
        let absent = report(&none, None);
        assert_eq!(absent.detection, Detection::Absent);
        // Every engine this build can drive is named in the advice, so a
        // sixth one added to PREFERENCE cannot be left out of the sentence
        // that tells somebody what to install.
        for kind in PREFERENCE {
            assert!(
                absent.remedy.contains(kind.as_str()),
                "the advice must name {}",
                kind.as_str()
            );
        }

        let down = Detected {
            engine: None,
            broken: vec![(EngineKind::Docker, "daemon is not running".into())],
        };
        let r = report(&down, None);
        assert_eq!(r.detection, Detection::Broken);
        assert!(r.remedy.contains("daemon is not running"));

        let ok = Detected {
            engine: Some(podman()),
            broken: Vec::new(),
        };
        assert_eq!(report(&ok, None).detail, "podman 5.8.4 (rootless)");

        // A configured plugin wins the resolution order, so it wins the
        // report: "no engine found" on a node that runs containers through
        // a plugin would be the status line contradicting the machine.
        assert!(
            report(&none, Some("/usr/libexec/mw-runc"))
                .detection
                .is_ready()
        );
    }

    // -- where containers are made --------------------------------------

    /// `/proc/self/mountinfo` inside a unit with `ProtectKernelTunables=`
    /// and `ProtectKernelLogs=`, as systemd 258 writes it — captured, not
    /// composed.
    const HARDENED_MOUNTINFO: &str = "\
1191 1190 0:34 / /proc/sys/fs/binfmt_misc rw,relatime shared:977 master:15 - autofs systemd-1 rw,fd=41
1192 1191 0:109 / /proc/sys/fs/binfmt_misc rw,nosuid,nodev,noexec,relatime shared:978 master:339 - binfmt_misc binfmt_misc rw
1207 1190 0:25 /acpi /proc/acpi ro,nosuid,nodev,noexec,relatime shared:979 master:13 - proc proc rw
1210 1190 0:25 /fs /proc/fs ro,nosuid,nodev,noexec,relatime shared:982 master:13 - proc proc rw
1212 1190 0:115 /systemd/inaccessible/reg /proc/kallsyms ro,nosuid,nodev,noexec,relatime shared:984 master:1340 - tmpfs tmpfs rw
1214 1190 0:115 /systemd/inaccessible/reg /proc/kmsg ro,nosuid,nodev,noexec,relatime shared:986 master:1340 - tmpfs tmpfs rw
1218 1190 0:25 /sys /proc/sys ro,nosuid,nodev,noexec,relatime shared:990 master:13 - proc proc rw
1220 1219 0:109 / /proc/sys/fs/binfmt_misc ro,nosuid,nodev,noexec,relatime shared:992 master:339 - binfmt_misc binfmt_misc rw
1221 1190 0:25 /sysrq-trigger /proc/sysrq-trigger ro,nosuid,nodev,noexec,relatime shared:993 master:13 - proc proc rw
";

    /// …and on an ordinary machine, where only the kernel's own empty
    /// mount point is under `/proc`.
    const ORDINARY_MOUNTINFO: &str = "\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
27 22 0:34 / /proc/sys/fs/binfmt_misc rw,relatime shared:15 - autofs systemd-1 rw,fd=41
577 27 0:109 / /proc/sys/fs/binfmt_misc rw,nosuid,nodev,noexec,relatime shared:339 - binfmt_misc binfmt_misc rw
";

    #[test]
    fn a_hardened_units_proc_is_recognised_and_an_ordinary_one_is_not() {
        let covered = covered_proc_paths(HARDENED_MOUNTINFO);
        assert_eq!(
            covered,
            [
                "/proc/acpi",
                "/proc/fs",
                "/proc/kallsyms",
                "/proc/kmsg",
                "/proc/sys",
                "/proc/sysrq-trigger"
            ]
        );
        assert!(covered_proc_paths(ORDINARY_MOUNTINFO).is_empty());
    }

    #[test]
    fn a_locked_proc_is_explained_with_the_service_that_fixes_it() {
        let covered = covered_proc_paths(HARDENED_MOUNTINFO);
        let socket = Path::new("/var/lib/meshware/run/podman.sock");
        let text = proc_locked_text(
            &covered,
            Some(socket),
            "meshnode",
            Some("/var/lib/meshware"),
        );
        assert_eq!(text.lines().next(), Some(PROC_LOCKED_HEADLINE));
        assert!(
            text.contains("/proc/acpi, /proc/fs, /proc/kallsyms and 3 more"),
            "{text}"
        );
        assert!(
            text.contains("mount proc to proc: Operation not permitted"),
            "{text}"
        );
        assert!(text.contains("Update over SSH"), "{text}");
        assert!(
            text.contains("ListenStream=/var/lib/meshware/run/podman.sock"),
            "{text}"
        );
        assert!(text.contains("systemctl --user -M meshnode@ enable --now podman.socket"));
        // Never the old advice: relaxing the unit does not fix this one.
        assert!(!text.contains("NoNewPrivileges=false"), "{text}");

        // With nowhere to point, it explains and does not invent a fix.
        let bare = proc_locked_text(&covered, None, "meshnode", None);
        assert!(!bare.contains("systemctl"), "{bare}");

        // And the table says what is wrong, not "not answering".
        let found = Detected {
            engine: None,
            broken: vec![(EngineKind::Podman, text)],
        };
        let r = report(&found, None);
        assert!(
            r.detail
                .contains("cannot run containers from inside this service"),
            "{}",
            r.detail
        );
    }

    #[test]
    fn a_podman_service_is_driven_remotely_on_every_invocation() {
        let e = podman_service();
        assert_eq!(
            e.global_args(),
            [
                "--remote",
                "--url",
                "unix:///var/lib/meshware/run/podman.sock"
            ]
        );
        let plan = e.plan(&req()).expect("plan");
        assert_eq!(
            &plan.args[..4],
            [
                "--remote",
                "--url",
                "unix:///var/lib/meshware/run/podman.sock",
                "run"
            ]
        );
        assert_eq!(e.detail(), "podman 5.8.4 (rootless, via podman.socket)");
        // The job's own variables still go in after the flags and before
        // the image, where `run` reads them.
        let at = plan.forward_env_at.expect("forwarded");
        assert_eq!(plan.args[at], "docker.io/library/nginx:alpine");

        // Run directly, nothing is prepended.
        let local = podman().plan(&req()).expect("plan");
        assert_eq!(local.args[0], "run");
        assert_eq!(podman().detail(), "podman 5.8.4 (rootless)");
    }

    #[test]
    fn a_limit_the_service_cannot_enforce_is_not_passed_and_not_claimed() {
        assert_eq!(parse_controllers("[cpu io memory pids]\n"), (true, true));
        assert_eq!(parse_controllers("[memory pids]"), (false, true));
        assert_eq!(parse_controllers(""), (true, true), "unknown is both");

        let mut r = req();
        r.cpus = 2;
        r.mem_mb = 512;
        let e = Engine {
            cpu_limit: false,
            ..podman_service()
        };
        let plan = e.plan(&r).expect("plan");
        assert!(!plan.args.iter().any(|a| a == "--cpus"));
        assert!(plan.args.iter().any(|a| a == "--memory"));
        assert!(!e.controls().cpu);
        assert!(e.controls().memory);
    }
}
