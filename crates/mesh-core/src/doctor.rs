//! Health probes behind `meshnode doctor` and Director's Diagnostics
//! (MASTERPLAN §12): connectivity, clock skew, disk, permissions, and
//! sandbox capability probes — the checks an operator would otherwise run
//! by hand. Rendering is the caller's business (CLI table, GUI list).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::config::NodeConfig;
use crate::identity::Keypair;
use crate::{genesis::Genesis, ipc};
use mesh_proto::{IpcRequest, IpcResponse, Liveness};

/// How long one probe waits for an answer.
///
/// **`doctor` must never inherit the hang it was run to diagnose.** Every
/// question here goes to a node on this machine over a unix socket, so a
/// slow answer is not a slow network — it is a wedged node, and that is
/// the finding, not a reason to stop reporting. Without a bound, one stuck
/// handler took the whole report with it: Director's Diagnostics button
/// sat on "Probing…" for ever and said nothing at all, which is strictly
/// worse than saying "the daemon did not answer".
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on a `--version` probe of a runtime binary. Same argument: a
/// binary that will not answer in five seconds is a finding.
const BINARY_TIMEOUT: Duration = Duration::from_secs(5);

/// One bounded IPC question. A timeout reads as an ordinary failure to
/// every caller that only cares whether it got its answer; the two callers
/// that want to *say* "timed out" wrap the call themselves.
async fn probe(
    socket: &Path,
    user: &Keypair,
    req: IpcRequest,
) -> crate::error::Result<IpcResponse> {
    match tokio::time::timeout(PROBE_TIMEOUT, ipc::request(socket, user, req)).await {
        Ok(r) => r,
        Err(_) => Err(crate::error::CoreError::Ipc(format!(
            "the node did not answer within {}s",
            PROBE_TIMEOUT.as_secs()
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, serde::Serialize)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

fn ok(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        verdict: Verdict::Ok,
        detail: detail.into(),
    }
}
fn warn(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        verdict: Verdict::Warn,
        detail: detail.into(),
    }
}
fn fail(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        verdict: Verdict::Fail,
        detail: detail.into(),
    }
}

/// What `doctor --fix` did about one finding.
#[derive(Debug, serde::Serialize)]
pub struct FixOutcome {
    /// The check this answers, by its name.
    pub name: &'static str,
    /// The fix was applied. `false` means it was not, and `detail` says
    /// why and what a person can do instead.
    pub applied: bool,
    pub detail: String,
}

/// `meshnode doctor --fix`: apply the safe fixes the checks already know,
/// and say honestly which findings have none.
///
/// The rule for what is *safe*: the fix is the one the check's own message
/// recommends, it is idempotent, and it cannot make a healthy node less
/// healthy. Creating the data dir, resuming sinks held by a restore, and
/// restarting a daemon that systemd manages qualify. Installing packages,
/// touching kernel settings, or pausing somebody's collector do not — those
/// are named as "cannot fix" with the reason, which is the other half of
/// the feature.
///
/// Only findings that were not `ok` produce an outcome.
pub async fn fix(config: &NodeConfig, user: &Keypair, checks: &[Check]) -> Vec<FixOutcome> {
    let mut out = Vec::new();
    let no = |name: &'static str, detail: String| FixOutcome {
        name,
        applied: false,
        detail,
    };
    for check in checks.iter().filter(|c| c.verdict != Verdict::Ok) {
        let outcome = match check.name {
            "home" => match std::fs::create_dir_all(&config.data_dir) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &config.data_dir,
                            std::fs::Permissions::from_mode(0o700),
                        );
                    }
                    match config.check() {
                        Ok(_) => FixOutcome {
                            name: "home",
                            applied: true,
                            detail: format!("created {} (mode 0700)", config.data_dir.display()),
                        },
                        Err(e) => no("home", format!("created the directory, but: {e}")),
                    }
                }
                Err(e) => no(
                    "home",
                    format!(
                        "cannot create {}: {e} — pick a writable --home",
                        config.data_dir.display()
                    ),
                ),
            },
            // Both findings have the same recommendation, verbatim: restart
            // the daemon so it runs this build. Only under systemd, because
            // that is the one case where "restart" is a known, supervised
            // action rather than a kill.
            "daemon" | "protocol" if check.detail.contains("restart") => {
                restart_under_systemd(check.name).await
            }
            "sinks" if check.detail.contains("restored from a backup") => {
                match probe(&config.socket_path(), user, IpcRequest::SinksResume).await {
                    Ok(IpcResponse::Ok) | Ok(IpcResponse::Notice { .. }) => FixOutcome {
                        name: "sinks",
                        applied: true,
                        detail: "sinks resumed — exporting again from where they left off".into(),
                    },
                    Ok(IpcResponse::Denied { message }) => no(
                        "sinks",
                        format!("not allowed to resume sinks as this user: {message}"),
                    ),
                    Ok(IpcResponse::Error { message }) => no("sinks", message),
                    Ok(_) | Err(_) => no("sinks", "the daemon did not answer the resume".into()),
                }
            }
            "runtime dir" => no(
                "runtime dir",
                "set XDG_RUNTIME_DIR in the daemon's environment (a systemd unit gets one from \
                 RuntimeDirectory=); the fallback socket in the data dir works, it is only \
                 the path that is long"
                    .into(),
            ),
            "identity" | "genesis" => no(
                check.name,
                "created by `meshnode up` (or `meshnode join <token>`) — nothing to repair".into(),
            ),
            "bash" => no(
                "bash",
                "install bash; every runtime here shells through it".into(),
            ),
            "git" | "ansible-playbook" | "uv" | "python env" => no(
                check.name,
                "an optional runtime that is not installed; install it if you use it".into(),
            ),
            "sandbox" => no(
                check.name,
                "run the node from a binary named `meshnode` (a renamed release artefact \
                 cannot re-exec itself as the Landlock shim), or set MESHWARE_SANDBOX_SHIM \
                 to one that can"
                    .into(),
            ),
            "landlock" | "cgroups v2" => no(
                check.name,
                "a kernel feature; nothing on this node can turn it on".into(),
            ),
            "resource limits" => no(
                check.name,
                "add `Delegate=yes` to the systemd unit and restart, or accept that --cpus \
                 and --mem are scheduling inputs on this node"
                    .into(),
            ),
            "clock" | "schedules" => no(
                check.name,
                "fix NTP on the member whose clock is off".into(),
            ),
            "disk" => no(
                "disk",
                "free space at the data dir; `meshnode state compact` and \
                                  retention (`meshnode retention`) are the levers"
                    .into(),
            ),
            "drain" => no(
                "drain",
                "a drain is an operator's decision; `meshnode drain --lift` ends a timed one"
                    .into(),
            ),
            "listen port" => no(
                "listen port",
                "another process holds the port; stop it or change [listen] port".into(),
            ),
            _ => no(check.name, "no automatic fix for this one".into()),
        };
        out.push(outcome);
    }
    out
}

/// `systemctl restart meshnode`, when systemd manages the daemon here.
async fn restart_under_systemd(name: &'static str) -> FixOutcome {
    let managed = Path::new("/run/systemd/system").exists()
        && tokio::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "meshnode"])
            .status()
            .await
            .is_ok_and(|s| s.success());
    if !managed {
        return FixOutcome {
            name,
            applied: false,
            detail: "the daemon is not a systemd unit here — `meshnode drain`, then start it \
                     again on the new binary"
                .into(),
        };
    }
    match tokio::process::Command::new("systemctl")
        .args(["restart", "meshnode"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => FixOutcome {
            name,
            applied: true,
            detail: "restarted meshnode under systemd; it comes back on this build".into(),
        },
        Ok(o) => FixOutcome {
            name,
            applied: false,
            detail: format!(
                "systemctl refused: {} — run `sudo systemctl restart meshnode`",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        },
        Err(e) => FixOutcome {
            name,
            applied: false,
            detail: format!("cannot run systemctl: {e}"),
        },
    }
}

/// Can this node host terminals at all, and what will it refuse?
///
/// Off is not a failure — it is the default and somebody has to choose
/// otherwise — so this reports rather than complains. What it does do is
/// say the quiet part: enabling the node is only *one* of the three things
/// a terminal needs, and "I turned it on and it still says no" is the
/// support ticket this line exists to prevent.
fn sessions_check(config: &NodeConfig) -> Check {
    let s = config.sessions;
    if !s.accept {
        return ok(
            "terminals",
            "off — this node serves no interactive sessions ([sessions] accept = \"on\" \
             to enable)",
        );
    }
    if config.tags.iter().any(|t| t == mesh_proto::NO_SHELL_TAG) {
        return ok(
            "terminals",
            format!(
                "enabled here, but this node is tagged {} — it runs jobs and refuses prompts",
                mesh_proto::NO_SHELL_TAG
            ),
        );
    }
    if config.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG) {
        return ok(
            "terminals",
            "enabled here, but this node is no-execute — it does not run processes",
        );
    }
    let idle = s
        .limits
        .idle
        .map(|d| humantime::format_duration(d).to_string())
        .unwrap_or_else(|| "off".into());
    let max = s
        .limits
        .max
        .map(|d| humantime::format_duration(d).to_string())
        .unwrap_or_else(|| "off".into());
    let recording = if s.record { ", recording on" } else { "" };
    ok(
        "terminals",
        format!(
            "on — still needs a session policy and a runner grant per user \
             (idle {idle}, max {max}, {} per peer{recording})",
            s.max_per_peer
        ),
    )
}

/// Is this daemon handing out root prompts?
///
/// The packaged unit runs as an unprivileged `meshnode` user, so the
/// ordinary answer is "not applicable". It is a **warning** when the daemon
/// is uid 0 and root terminals are enabled, because that combination is
/// almost always somebody who ran `meshnode up` as root for convenience and
/// did not think about what a terminal on it means.
fn root_shell_check(config: &NodeConfig) -> Check {
    #[cfg(unix)]
    let is_root = rustix::process::getuid().is_root();
    #[cfg(not(unix))]
    let is_root = false;

    if !is_root {
        return ok(
            "terminal user",
            "the daemon is not root, so neither is a terminal",
        );
    }
    if !config.sessions.accept {
        return ok(
            "terminal user",
            "the daemon runs as root — terminals are off, so nothing is exposed",
        );
    }
    if config.sessions.allow_root {
        warn(
            "terminal user",
            "the daemon runs as root and root terminals are enabled — every prompt here \
             is a root prompt ([sessions] allow_root)",
        )
    } else {
        ok(
            "terminal user",
            "the daemon runs as root, so terminals are refused ([sessions] allow_root = \
             \"on\" to permit root prompts)",
        )
    }
}

/// Can this node actually allocate a pseudoterminal, and can it find the
/// binary it re-execs as the shim?
///
/// Both fail in ways that only show up when somebody opens a terminal: a
/// container without `/dev/ptmx` refuses at `openpt`, and a binary that is
/// no longer where the running daemon was started from refuses at `spawn`.
///
/// The second is not exotic — **replacing the binary in place under a
/// running daemon does it every time**, because `current_exe()` then
/// resolves to a deleted inode. Everything else keeps working, so the
/// symptom is "terminals stopped opening this afternoon" with no other
/// evidence. The fix is the one `doctor` already advises for a version
/// mismatch: restart the daemon to finish the upgrade.
fn pty_check(config: &NodeConfig) -> Check {
    if !config.sessions.accept {
        return ok("pty", "not needed — terminals are off on this node");
    }
    let shim = config
        .pty_shim_override
        .clone()
        .or_else(|| std::env::var_os(crate::sessions::pty_shim::SHIM_ENV).map(Into::into))
        .or_else(|| std::env::current_exe().ok());
    match shim {
        Some(path) if !path.exists() => {
            return fail(
                "pty",
                format!(
                    "{} does not exist — no terminal can start (restart the daemon if the \
                     binary was replaced in place)",
                    path.display()
                ),
            );
        }
        None => {
            return warn(
                "pty",
                "cannot locate this binary — a terminal may not start",
            );
        }
        Some(_) => {}
    }
    match rustix::pty::openpt(
        rustix::pty::OpenptFlags::RDWR
            | rustix::pty::OpenptFlags::NOCTTY
            | rustix::pty::OpenptFlags::CLOEXEC,
    ) {
        Ok(_) => ok("pty", "pseudoterminals available"),
        Err(e) => warn(
            "pty",
            format!("cannot allocate a pseudoterminal ({e}) — terminals will be refused"),
        ),
    }
}

/// Report anything a node has left outside its home.
/// Report anything a node has left outside its home.
///
/// A warning rather than a failure: a node whose data is split across two
/// places still works, it is just no longer backed up by copying one
/// directory — which is the property the layout exists to provide.
/// What this machine's accelerator probes say (C5 P6).
///
/// Three distinguishable answers, because they have three different fixes.
/// No vendor tool at all is the ordinary case on the ordinary machine and
/// reads as information rather than a warning — most machines in most
/// meshes have no accelerator, and a mesh full of yellow lines about it
/// would train people to stop reading `doctor`. A tool that is present and
/// reports nothing is a driver problem. A device the driver has taken out
/// of service is a hardware problem, and it is named.
/// The container runtime, and **what it does not enforce** (N4).
///
/// N4's landmine: Landlock and cgroups already exist and must keep
/// What this node can run, one line per runtime (T4 §5).
///
/// **The check that answers "why did my job not run here."** Every other
/// entry in `doctor` describes a way the node might be degraded; this one
/// describes what it can do at all, which is the question somebody actually
/// arrives with.
///
/// A runtime the machine simply does not have is `ok`, not a warning. Most
/// nodes run none of ansible, python or containers, and a `doctor` that
/// warned about each would be a `doctor` whose warnings nobody reads. What
/// earns a warning is **installed and not working**, because that is the
/// state somebody meant to have working.
/// What this node's **daemon** says it can run (T4).
///
/// **Asked over IPC, not probed here.** `doctor` is a separate process, and
/// some of what decides whether a runtime works is a property of *the
/// process asking*: a daemon started by systemd under
/// `NoNewPrivileges=true`, or with a `CapabilityBoundingSet=` narrower than
/// `newuidmap` needs, cannot let the helper gain its privilege at all —
/// while a CLI run from a login shell can. Probing locally therefore
/// reported a cheerful `container ready` on precisely the node whose
/// containers could not start — a check answering for the wrong process,
/// which is worse than no check at all.
///
/// Falls back to a local probe when there is no daemon to ask, because
/// `doctor` runs against a stopped node too. What that cannot see is
/// anything about the daemon's own restrictions, which is the case this
/// exists for — so it is a fallback, not an equivalent.
async fn runtime_reports_for_doctor(
    config: &NodeConfig,
    user: &Keypair,
) -> Vec<mesh_proto::runtimes::RuntimeReport> {
    if let Ok(IpcResponse::Runtimes(view)) = probe(
        &config.socket_path(),
        user,
        IpcRequest::Runtimes { node: None },
    )
    .await
    {
        return view.local;
    }
    let opts = crate::runtimes::ContainerOptions {
        plugin: config.runtime.container_plugin.clone(),
        prefer: config
            .runtime
            .container_engine
            .as_deref()
            .and_then(crate::engines::EngineKind::parse),
        engine_socket: Some(config.layout().engine_socket()),
    };
    crate::runtimes::detect(&opts).await
}

fn runtime_checks(reports: &[mesh_proto::runtimes::RuntimeReport]) -> Vec<Check> {
    use mesh_proto::runtimes::Detection;
    reports
        .iter()
        .cloned()
        .map(|r| {
            // A stable name per runtime, because `doctor --json` is
            // something people filter on and a name that moved between
            // releases would break their filter silently.
            let name: &'static str = match r.runtime {
                mesh_proto::Runtime::Exec => "runtime exec",
                mesh_proto::Runtime::Bash => "runtime bash",
                mesh_proto::Runtime::Python => "runtime python",
                mesh_proto::Runtime::Ansible => "runtime ansible",
                mesh_proto::Runtime::Container => "runtime container",
            };
            match r.detection {
                Detection::Ready => ok(name, r.detail),
                Detection::Absent => ok(
                    name,
                    format!("not on this machine — {} if you want it here", r.remedy),
                ),
                Detection::Broken => warn(name, format!("{} — {}", r.detail, r.remedy)),
            }
        })
        .collect()
}

/// applying, or `doctor` must report the degradation by name. They do not
/// fully apply — `podman run` hands the workload to `conmon`, which
/// reparents it out of the executor's cgroup and out of any Landlock
/// ruleset on the client process — so this is where an operator finds out,
/// rather than from an incident.
///
/// The plugin **declares** what it enforces (`<plugin> info`), and this
/// reports exactly that. A plugin that claims nothing is reported as
/// enforcing nothing; one that does not answer `info` is reported as
/// present but silent, because the contract's required verb is `prepare`
/// and refusing to run a mesh's containers over an optional introspection
/// verb would be the wrong trade.
async fn container_check(
    config: &NodeConfig,
    reports: &[mesh_proto::runtimes::RuntimeReport],
) -> Check {
    // **Isolation is only a question once the thing can run.** Reporting
    // the controls of an engine this node cannot actually use said `ok`
    // beside a `runtime container` saying the opposite — two checks about
    // one node, disagreeing, which teaches an operator to trust neither.
    if reports
        .iter()
        .any(|r| r.runtime == mesh_proto::Runtime::Container && !r.detection.is_ready())
    {
        return ok(
            "containers",
            "nothing to isolate until containers run here — see `runtime container`",
        );
    }
    let Some(plugin) = config.runtime.container_plugin.as_deref() else {
        // T2. No plugin is now the *ordinary* case rather than a
        // limitation: the node drives whatever engine it found. What this
        // line used to say — "this node runs no container work" — became
        // false the day detection landed, and a `doctor` that says a
        // working node is not working is worse than one that says nothing.
        return match crate::engines::detect(
            config
                .runtime
                .container_engine
                .as_deref()
                .and_then(crate::engines::EngineKind::parse),
            Some(&config.layout().engine_socket()),
        )
        .await
        .engine
        {
            // **Isolation, not availability.** `runtime container` above
            // already says whether containers can run here; this check
            // exists for the N4 landmine, which is what a container job
            // actually gets instead of Landlock and this node's cgroup. Two
            // checks saying the same thing would train an operator to read
            // neither.
            Some(engine) => {
                let missing = engine.controls().missing();
                let head = format!("{} (built in)", engine.detail());
                if missing.is_empty() {
                    ok(
                        "containers",
                        format!(
                            "{head} — memory, cpu, read-only rootfs, dropped capabilities, \
                             no-new-privileges, rootless"
                        ),
                    )
                } else {
                    warn(
                        "containers",
                        format!("{head} — does not enforce: {}", missing.join(", ")),
                    )
                }
            }
            None => ok(
                "containers",
                "no container engine here, so nothing to isolate — see `runtime container`",
            ),
        };
    };
    // Can it be asked what it left running? Probed with a node id nothing
    // will match, so a plugin that has the verb answers with an empty list
    // and one that does not fails — which is the only difference being
    // looked for here.
    let reaps = crate::containers::list_jobs(plugin, "doctor-probe")
        .await
        .is_ok();
    match crate::containers::info(plugin).await {
        None => warn(
            "containers",
            format!(
                concat!(
                    "plugin {:?} did not answer `info` — it may still work (only ",
                    "`prepare` is required), but nothing here can say what it enforces",
                ),
                plugin
            ),
        ),
        Some(reply) => {
            let missing = reply.controls.missing();
            let head = format!(
                "{} via {plugin:?}{}",
                reply.name,
                if reply.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", reply.detail)
                }
            );
            if reply.controls.claims_nothing() {
                // The loudest case, and the one the landmine is about: a
                // container job on this node runs with whatever the
                // runtime's defaults happen to be, and the node's own
                // Landlock and cgroup controls do not reach it.
                fail(
                    "containers",
                    format!(
                        concat!(
                            "{} — claims NO isolation controls. Landlock and this ",
                            "node's cgroup do not reach a container's processes, so a ",
                            "container job here runs with the runtime's defaults and ",
                            "nothing else.",
                        ),
                        head
                    ),
                )
            } else if !reaps {
                // Enforcement is fine; survival of a restart is not. A
                // plugin with no `list` cannot be asked what it left
                // running, so a container that outlived a killed node keeps
                // its host port and nothing will ever take it back — the
                // next deployment handed that port crash-loops on `Address
                // already in use`, and the ledger says the port is free.
                warn(
                    "containers",
                    format!(
                        concat!(
                            "{} — does not answer `list`, so containers left behind by a ",
                            "crash or a kill are never reaped. They keep their host ports. ",
                            "Check with `podman ps` after an unclean stop.",
                        ),
                        head
                    ),
                )
            } else if missing.is_empty() {
                ok(
                    "containers",
                    format!(
                        "{head} — memory, cpu, read-only rootfs, dropped capabilities, no-new-privileges, rootless"
                    ),
                )
            } else {
                warn(
                    "containers",
                    format!("{head} — does not enforce: {}", missing.join(", ")),
                )
            }
        }
    }
}

/// Ports and DNS: what this node lends, and what answers `.mesh` (N2/N3).
///
/// Two things worth saying out loud, because both are invisible until
/// somebody is already debugging:
///
/// - **A node that advertises no port range takes no port work.** That is
///   a refusal, not an unknown, and the likeliest cause is a typo in
///   `[network] dynamic_ports` — which fails closed rather than falling
///   back to the default, precisely so it surfaces here.
/// - **Whether a responder is actually there, and whether it is ours**
///   (N7). Reporting the configured bind address said nothing: a node whose
///   responder failed to start looks identical, and on a machine running
///   two meshnodes the second one's never starts at all.
/// - **Whether anything forwards `.mesh` to it.** A responder on 8600 that
///   no resolver asks looks exactly like a mesh where nothing is
///   registered — and `curl web.mesh` fails while every other surface
///   correctly reports the service healthy.
///
/// `known` is one service this node's own catalogue says is registered, for
/// the responder probe to check an answer against. `None` when nothing is
/// registered, which is not a fault — there is simply nothing to verify the
/// responder's answers with.
fn network_check(config: &NodeConfig, known: Option<String>) -> Check {
    let net = &config.network;
    let Some(range) = &net.dynamic else {
        return warn(
            "network",
            "this node advertises no dynamic port range, so it will take no work \
             that needs a port — check `[network] dynamic_ports`, which \
             advertises nothing rather than falling back to the default when \
             it cannot be parsed",
        );
    };
    let mut detail = format!("ports {}-{}", range.min, range.max);
    if !net.reserved.is_empty() {
        detail.push_str(&format!(", {} reserved", net.reserved.len()));
    }
    match (net.dns, net.dns_bind) {
        (false, _) => {
            detail.push_str("; DNS off");
            ok("network", detail)
        }
        (true, None) => {
            detail.push_str("; DNS enabled but `[network] dns_bind` is not a usable address");
            warn("network", detail)
        }
        (true, Some(bind)) => {
            detail.push_str(&format!("; DNS on {bind} for .{}", mesh_proto::DNS_SUFFIX));
            // **Is a responder actually there, and is it ours? (N7)**
            //
            // Reporting the configured address and calling it `ok` says
            // nothing: a node whose responder failed to start looks
            // identical. And on a machine running two meshnodes — a
            // Director beside a daemon is the ordinary case — the first to
            // start owns the port and the second logs `Address already in
            // use` to a journal nobody reads, so `.mesh` answers come from
            // a different mesh entirely.
            //
            // The sentence is `dns::probe_problem`'s, shared with the DNS
            // status that Director's Registry banner shows (Phase A,
            // §2.5d), so the two cannot disagree about one port.
            let found = crate::dns::probe(bind, known.as_deref());
            if let Some(problem) = crate::dns::probe_problem(&found, bind, known.as_deref()) {
                detail.push_str(&format!(" — but {problem}"));
                return fail("network", detail);
            }
            // **Is anything on this machine actually asking it?**
            //
            // This used to report `ok` on the strength of the responder
            // existing, which is not the question. A responder on 8600 that
            // no resolver forwards to looks exactly like a mesh where
            // nothing is registered — `curl web.mesh` says *Could not
            // resolve host* and every surface in the product says the
            // service is healthy. The operator is left holding two true
            // statements that cannot both be right.
            if bind.port() != 53 && !resolver_routes_mesh(bind) {
                detail.push_str(&format!(
                    concat!(
                        " — but nothing on this machine forwards .{} to it, so ",
                        "`<name>.{}` will not resolve. Fix it with ",
                        "`sudo meshnode dns install --address {}`, which writes the ",
                        "systemd-resolved drop-in."
                    ),
                    mesh_proto::DNS_SUFFIX,
                    mesh_proto::DNS_SUFFIX,
                    bind,
                ));
                return warn("network", detail);
            }
            ok("network", detail)
        }
    }
}

/// Does this machine's resolver send `.mesh` to the responder?
///
/// Two ways a Linux box can be set up to do it, and both are checked
/// because an operator may have used either:
///
/// - **systemd-resolved**, where `resolvectl domain` lists `~mesh` against
///   the link that carries our address. This is the ordinary answer, and
///   the one the drop-in in the docs produces.
/// - **A plain `resolv.conf`** naming our bind address as a nameserver,
///   which is what a container or a minimal image usually has.
///
/// Best-effort by construction: a machine using dnsmasq, unbound or a
/// corporate resolver may be perfectly well configured and answer `false`
/// here. So the check *warns* rather than failing, and its message says
/// what to do rather than asserting the machine is broken — a wrong
/// warning costs a sentence, and a missing one costs an afternoon.
pub fn resolver_routes_mesh(bind: std::net::SocketAddr) -> bool {
    let suffix = format!("~{}", mesh_proto::DNS_SUFFIX);
    // `resolvectl domain` prints the routing domains per link. The tilde
    // form is the one that means "send this domain here and nowhere else".
    if let Ok(out) = std::process::Command::new("resolvectl")
        .arg("domain")
        .stdin(std::process::Stdio::null())
        .output()
        && out.status.success()
        && String::from_utf8_lossy(&out.stdout).contains(&suffix)
    {
        return true;
    }
    // The static file, for a machine with no resolved. Matched on the
    // address as well as the port: `nameserver 127.0.0.1` alone is the
    // default loopback resolver on plenty of boxes and means nothing here.
    let ip = bind.ip().to_string();
    std::fs::read_to_string("/etc/resolv.conf").is_ok_and(|text| {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .any(|l| l.starts_with("nameserver") && l.contains(&ip))
    })
}

/// How long a peer's last sync request may be, before it is a peer that
/// has stopped asking rather than one between rounds (ENG-26). A gossip
/// round is seconds even on a large mesh.
const SYNC_STALL_AFTER: Duration = Duration::from_secs(300);

/// Open-file descriptors this node should have headroom for (ENG-26).
///
/// One per pooled connection (the pool alone reaches 1 024 on a large
/// mesh), per followed log, per running job's two output files, per chunk
/// in flight, plus the op-log, the index and the content store. The
/// default 1 024 on many distributions is under the pool's own ceiling.
const NOFILE_WANTED: u64 = 8192;

fn nofile_check() -> Check {
    #[cfg(unix)]
    {
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let soft = limit.current.unwrap_or(u64::MAX);
        let hard = limit.maximum.unwrap_or(u64::MAX);
        if soft >= NOFILE_WANTED {
            return ok("open files", format!("{soft} (limit)"));
        }
        // A raisable limit is a one-line fix; a hard ceiling this low is a
        // different conversation, and saying which is the whole value of
        // the check.
        let fix = if hard >= NOFILE_WANTED {
            "raise it with `LimitNOFILE=65536` in the unit — which is what the packaged unit \
             already sets, so this machine is running meshnode some other way"
        } else {
            "the hard limit is this low too — raise it in /etc/security/limits.conf or the unit's LimitNOFILE"
        };
        warn(
            "open files",
            format!(
                "{soft} is below the {NOFILE_WANTED} this node wants: one descriptor per pooled connection, per followed log, per running job and per chunk being served. When it runs out, connections fail with `Too many open files` several layers from anything that mentions a limit — {fix}"
            ),
        )
    }
    #[cfg(not(unix))]
    {
        ok("open files", "not applicable")
    }
}

/// Is *this machine's* clock disciplined (ENG-26)?
///
/// The existing `clock` check compares this node against its peers, which
/// answers a different question: a fleet whose clocks are all wrong
/// together passes it. This one asks whether anything is steering the
/// clock at all — and it matters because SEC-5 bounds how far an op's
/// timestamp may be from local time, so a node with a free-running clock
/// has its work quietly parked by everyone else.
fn clock_sync_check() -> Check {
    // systemd-timesyncd creates this the moment it first synchronises, and
    // it is the cheapest true answer available.
    if std::path::Path::new("/run/systemd/timesync/synchronized").exists() {
        return ok("clock sync", "synchronised (systemd-timesyncd)");
    }
    // chrony and ntpd do not, so look for the daemon itself rather than
    // reporting a machine with chrony on it as unsynchronised.
    for (daemon, name) in [
        ("chronyd", "chrony"),
        ("ntpd", "ntpd"),
        ("systemd-timesyncd", "systemd-timesyncd"),
        ("ntpsec", "ntpsec"),
    ] {
        if process_running(daemon) {
            return ok("clock sync", format!("{name} is running"));
        }
    }
    warn(
        "clock sync",
        "nothing appears to be disciplining this machine's clock. Ops carry timestamps, and a clock that drifts far enough has this node's work parked by every other member — install chrony or enable systemd-timesyncd",
    )
}

/// Is a process with this name running? Reads `/proc`, which is where the
/// answer is; shelling out to `pgrep` would be a process to answer a
/// question about processes.
fn process_running(name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            && std::fs::read_to_string(e.path().join("comm")).is_ok_and(|comm| comm.trim() == name)
    })
}

/// Has the op-log store had anything go wrong in it (ENG-26)?
///
/// Two readings, and they fail in opposite directions. A **write failure**
/// is loud and current: the node cannot record anything until it clears.
/// A **contained panic** is silent and historical — SEC-22 catches a merge
/// that panics so one malformed op cannot take the node down, which is
/// right, and means the only record that it happened is a counter nobody
/// reads. Anything but zero there is a bug worth reporting, and this is
/// where somebody would see it.
fn store_integrity_check(status: &mesh_proto::StatusInfo) -> Check {
    if let Some(why) = &status.store_failure {
        return warn(
            "store integrity",
            format!("the op-log is refusing writes: {why}"),
        );
    }
    match status.contained_panics {
        0 => ok("store integrity", "no contained merge failures"),
        n => warn(
            "store integrity",
            format!(
                "{n} op(s) panicked while being merged and were contained. The node is fine and those ops did not apply here — but this is a bug, and the journal around each one is what a report needs"
            ),
        ),
    }
}

fn devices_check() -> Check {
    let probes = crate::devices::probes_available();
    if probes.is_empty() {
        return ok(
            "accelerators",
            "no vendor tools (nvidia-smi, rocm-smi) — this machine advertises no devices",
        );
    }
    let devices = crate::devices::detect();
    if devices.is_empty() {
        return warn(
            "accelerators",
            format!(
                "{} installed but reports no devices — check the driver is loaded",
                probes.join(", ")
            ),
        );
    }
    let unhealthy: Vec<String> = devices
        .iter()
        .filter(|d| !d.health.schedulable())
        .map(|d| {
            format!(
                "{} ({})",
                d.summary(),
                d.health.reason().unwrap_or("unavailable")
            )
        })
        .collect();
    if unhealthy.is_empty() {
        ok(
            "accelerators",
            format!(
                "{} device(s): {}",
                devices.len(),
                devices
                    .iter()
                    .map(mesh_proto::Device::summary)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    } else {
        warn(
            "accelerators",
            format!(
                "{} of {} device(s) not schedulable: {}",
                unhealthy.len(),
                devices.len(),
                unhealthy.join(", ")
            ),
        )
    }
}

fn layout_check(config: &NodeConfig) -> Check {
    let layout = config.layout();
    let mut strays = Vec::new();
    for (label, path, node_needs) in layout.describe() {
        // A packaged node's config is in /etc by design (DOC-5).
        let by_design = label == "config" && layout.is_system_home();
        if node_needs && !by_design && path.exists() && !path.starts_with(layout.root()) {
            strays.push(format!("{label} at {}", path.display()));
        }
    }
    // The pre-consolidation locations, which an un-adopted node still uses.
    for (label, path) in [
        ("old data dir", dirs::data_dir().map(|d| d.join("meshware"))),
        ("old config", Some(crate::config::legacy_config_path())),
    ] {
        if let Some(path) = path
            && path.exists()
            && !path.starts_with(layout.root())
        {
            strays.push(format!("{label} still at {}", path.display()));
        }
    }
    // A packaged node used to be told to move its config into the data
    // directory. With /etc/meshware/meshnode.toml present, that copy is
    // no longer read, and saying nothing would leave an operator editing
    // the file that does not count.
    let in_home = layout.root().join("meshnode.toml");
    if layout.is_system_home() && in_home.exists() && layout.config_file().exists() {
        strays.push(format!(
            "old config still at {} (not read: {} wins)",
            in_home.display(),
            layout.config_file().display()
        ));
    }
    if strays.is_empty() {
        ok(
            "layout",
            format!("everything under {}", layout.root().display()),
        )
    } else {
        // Adoption only runs for the *default* home, so the advice has to
        // depend on which home this is — telling someone with an explicit
        // `--home` to "restart and it will move" would be a lie.
        let advice = if layout.is_system_home() {
            // Nothing is adopted for a packaged node: its old config is in
            // /etc, which the unit mounts read-only (DOC-5).
            "this is a packaged node, so its config belongs in /etc/meshware/meshnode.toml — \
             move the old file there by hand"
        } else if layout.root() == crate::layout::default_home() {
            "start the node once and it will be adopted automatically"
        } else {
            "this node has an explicit home, so nothing is adopted for it — \
             move what you want by hand, or delete it"
        };
        warn(
            "layout",
            format!("data outside the home: {} — {advice}", strays.join("; ")),
        )
    }
}

/// Run every probe. Returns the checks in display order.
pub async fn run(config: &NodeConfig, user: &Keypair) -> Vec<Check> {
    let mut checks = Vec::new();

    // -- local environment ---------------------------------------------------
    checks.push(match config.config_path.as_ref() {
        Some(p) => ok("config", format!("loaded {}", p.display())),
        None => ok("config", "defaults (no config file)"),
    });

    checks.push(match config.check() {
        Ok(_) => ok("home", format!("{} writable", config.data_dir.display())),
        Err(e) => fail("home", e.to_string()),
    });

    // "Where is my data" should be answerable from `doctor` alone, without
    // anyone having to know the layout. Before consolidation this question
    // had three answers and the doctor reported one of them.
    checks.push(layout_check(config));

    checks.push(disk_check(&config.data_dir, config.disk));

    let id_dir = crate::identity::Identity::identity_dir(&config.data_dir);
    checks.push(
        if id_dir.join("node.key").exists() && id_dir.join("user.key").exists() {
            ok("identity", "node + user keys present")
        } else {
            warn(
                "identity",
                "no identity yet (created on first `meshnode up`)",
            )
        },
    );

    checks.push(match Genesis::load(&config.data_dir) {
        Ok(Some(g)) => match g.verify() {
            Ok(()) => ok(
                "genesis",
                format!("mesh '{}' — signature valid", g.body.mesh_name),
            ),
            Err(e) => fail("genesis", format!("signature INVALID: {e}")),
        },
        Ok(None) => warn(
            "genesis",
            "not in a mesh yet (`up` bootstraps, `join` joins)",
        ),
        Err(e) => fail("genesis", e.to_string()),
    });

    // -- runtimes (MASTERPLAN §9.2) -------------------------------------------
    checks.push(binary_check("bash", "bash", true).await);
    checks.push(binary_check("git", "git (Git sources)", false).await);
    checks.push(binary_check("ansible-playbook", "ansible-playbook (inventory)", false).await);
    let uv = binary_check("uv", "uv", false).await;
    if uv.verdict == Verdict::Ok {
        checks.push(uv);
    } else {
        let py = binary_check("python3", "python3 (uv fallback)", false).await;
        checks.push(Check {
            name: "python env",
            verdict: py.verdict,
            detail: format!("uv not found; {}", py.detail),
        });
    }

    // -- the machine's own limits (ENG-26) -------------------------------------
    //
    // **Descriptors are what a mesh node runs out of first.** One per
    // pooled connection, per open log a follower holds, per job's two
    // output files, per chunk being served, plus the stores. The ceiling
    // is a number nobody looks at until connections start failing, and
    // when they do the error is `Too many open files` several layers from
    // anything that mentions a limit.
    checks.push(nofile_check());
    checks.push(clock_sync_check());

    // -- sandbox capability probes (MASTERPLAN §7.5) ---------------------------
    checks.push(if Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        ok("cgroups v2", "available")
    } else {
        warn("cgroups v2", "not detected — resource accounting degraded")
    });
    // E2.1: whether `--cpus` and `--mem` are ceilings or only scheduling
    // inputs on this node. A *warning* rather than a note, deliberately:
    // the docs say declared limits are enforced, and a node where they are
    // not is a node whose operator is being misled until they read this.
    checks.push(match crate::cgroup::Delegation::probe() {
        Ok(d) => ok(
            "resource limits",
            format!("enforced ({})", d.controllers().join(", ")),
        ),
        Err(why) => warn("resource limits", format!("NOT enforced — {why}")),
    });
    // What bounds *this node*, as opposed to what it enforces on a job
    // (ENG-23). Worth a line whenever a cgroup limit is in force, because
    // the alternative is an operator looking at a 512 GB machine whose
    // node advertises 2 GB with nothing anywhere saying why.
    if let Some((limit_mb, _)) = crate::cgroup::memory_limit() {
        let machine = crate::capacity::machine_mem_total_mb();
        checks.push(ok(
            "memory limit",
            match machine {
                Some(machine) if machine < limit_mb => format!(
                    "{limit_mb} MB by cgroup, above this machine's {machine} MB — the machine is the bound"
                ),
                _ => format!(
                    "{limit_mb} MB by cgroup — this node advertises that, not the machine's memory. From a container limit or the unit's MemoryMax="
                ),
            },
        ));
    }
    // What a job on this node would *actually* get, not what the kernel is
    // capable of.
    //
    // This check used to read `/sys/kernel/security/lsm` and report
    // "available", which is true and beside the point: sandboxing also needs
    // this process to be able to re-exec itself as the shim, and when it
    // cannot, `sandbox = "auto"` degrades to running every job unconfined.
    // A node shipped as `meshnode-v0.7.6-x86_64-unknown-linux-musl` did
    // exactly that while this line said `ok landlock available` — the worst
    // shape a security check can have.
    checks.push({
        let policy = &config.runtime.sandbox;
        match crate::sandbox::Mode::Off == policy.mode {
            true => warn(
                "sandbox",
                "off by configuration — jobs run unconfined ([runtime] sandbox)",
            ),
            false => match crate::sandbox::unavailable_reason(policy) {
                None => ok(
                    "sandbox",
                    format!(
                        "applied to every job ([runtime] sandbox = \"{}\")",
                        policy.mode.as_str()
                    ),
                ),
                // `on` refuses the job outright, which is loud and safe.
                // `auto` runs it anyway, which is quiet and is the thing
                // worth shouting about here.
                Some(why) if policy.mode == crate::sandbox::Mode::On => {
                    warn("sandbox", format!("jobs will be REFUSED — {why}"))
                }
                Some(why) => fail(
                    "sandbox",
                    format!("NOT applied — jobs run unconfined. {why}"),
                ),
            },
        }
    });
    checks.push(match std::fs::read_to_string("/sys/kernel/security/lsm") {
        Ok(lsm) if lsm.contains("landlock") => ok("landlock", "the kernel supports it"),
        Ok(_) => warn("landlock", "kernel LSM list does not include landlock"),
        Err(_) => warn("landlock", "cannot read /sys/kernel/security/lsm"),
    });
    checks.push(if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        ok("runtime dir", "XDG_RUNTIME_DIR set (IPC socket home)")
    } else {
        warn(
            "runtime dir",
            "XDG_RUNTIME_DIR unset — IPC socket falls back into the data dir",
        )
    });

    // -- accelerators (C5 P6) -----------------------------------------------
    //
    // The check answers the question a person with a GPU box and an empty
    // `nodes --devices` actually has, which is *"is it me or is it them"*:
    // no driver tools installed, tools installed but reporting nothing, or
    // a card the driver has taken out of service.
    // Off the async workers (ENG-26): `nvidia-smi` on a wedged driver
    // blocks for the full probe timeout, and `doctor` is the command an
    // operator runs *because* something is wrong.
    checks.push(
        tokio::task::spawn_blocking(devices_check)
            .await
            .unwrap_or_else(|_| warn("accelerators", "the device probe panicked")),
    );
    // One registered name, for the DNS probe to check an answer against.
    // Asked over IPC like every other live fact here: `doctor` holds no
    // engine, deliberately, so that it works against a daemon it did not
    // start.
    let known_service = match probe(&config.socket_path(), user, IpcRequest::Registry).await {
        Ok(IpcResponse::Registry(rows)) => rows.into_iter().next().map(|r| r.service),
        _ => None,
    };
    checks.push(network_check(config, known_service));
    // **Probed once, and from the daemon.** Both checks below describe the
    // same node, so they must come from the same answer — and that answer
    // has to be the daemon's, because some of what decides whether a
    // runtime works is a property of the process asking. See
    // `runtime_reports_for_doctor`.
    let reports = runtime_reports_for_doctor(config, user).await;
    checks.push(container_check(config, &reports).await);
    checks.extend(runtime_checks(&reports));

    // -- the fabric (M17 §3.12 P5) ------------------------------------------
    //
    // A shell that cannot open is diagnosed here rather than at the prompt.
    // Both checks answer a question a failed `meshnode shell` raises and
    // cannot answer for itself: *is it me or is it them?*
    checks.push(sessions_check(config));
    checks.push(root_shell_check(config));
    checks.push(pty_check(config));

    // -- daemon & mesh --------------------------------------------------------
    let status = tokio::time::timeout(
        PROBE_TIMEOUT,
        probe(&config.socket_path(), user, IpcRequest::Status),
    )
    .await;
    match status {
        Ok(Ok(IpcResponse::Status(s))) => {
            checks.push(store_integrity_check(&s));
            let cli_version = env!("CARGO_PKG_VERSION");
            checks.push(if s.version == cli_version {
                ok(
                    "daemon",
                    format!("up {}s, version {}", s.uptime_secs, s.version),
                )
            } else {
                warn(
                    "daemon",
                    format!(
                        "daemon runs {} but this CLI is {} — restart the daemon to finish the upgrade",
                        s.version, cli_version
                    ),
                )
            });
            // Protocol skew (MASTERPLAN M7): a daemon speaking an older
            // minor than this CLI is the same unfinished upgrade, seen
            // from the wire; a newer one means the CLI is stale.
            if s.proto_minor != mesh_proto::PROTOCOL_MINOR
                || s.proto_major != mesh_proto::PROTOCOL_MAJOR
            {
                checks.push(warn(
                    "protocol",
                    format!(
                        "daemon speaks {}.{} but this CLI speaks {}.{} — restart the daemon (or reinstall the CLI) so both run the same build",
                        s.proto_major,
                        s.proto_minor,
                        mesh_proto::PROTOCOL_MAJOR,
                        mesh_proto::PROTOCOL_MINOR
                    ),
                ));
            } else {
                checks.push(ok(
                    "protocol",
                    format!("{}.{}", s.proto_major, s.proto_minor),
                ));
            }
            if let Some(w) = &s.wan {
                let mut detail = w.reachability.clone();
                if let Some(r) = &w.reflexive_addr {
                    detail.push_str(&format!(" (seen as {r})"));
                }
                if w.relay_server {
                    detail.push_str(&format!(", relaying for {} client(s)", w.relay_clients));
                }
                if !w.relays.is_empty() {
                    detail.push_str(&format!(", via relays {}", w.relays.join(",")));
                }
                checks.push(match w.reachability.as_str() {
                    "symmetric-nat" if w.relays.is_empty() => warn(
                        "wan",
                        format!("{detail} — behind a symmetric NAT with no relay: only peers you dial can reach you; add a relay-capable member (`[wan] relay = \"on\"` on a public node)"),
                    ),
                    _ => ok("wan", detail),
                });
            }
            if s.draining {
                checks.push(warn("drain", "node is draining — it accepts no new work"));
            }
            // Named first: a member far enough ahead that its ops are held
            // here (SEC-5) is the actionable case, and the one the skew
            // figure below can no longer see — held ops are not observed.
            let held = match probe(&config.socket_path(), user, IpcRequest::ClockAhead).await {
                Ok(IpcResponse::ClockAhead(rows)) => rows,
                _ => Vec::new(),
            };
            let skew_secs = s.hlc_wall_ahead_micros / 1_000_000;
            checks.push(if !held.is_empty() {
                let who: Vec<String> = held
                    .iter()
                    .map(|r| format!("{} (~{}s ahead)", r.name, r.ahead_micros / 1_000_000))
                    .collect();
                fail(
                    "clock",
                    format!(
                        "{} dated ops more than {}s ahead of this node, so this node is holding \
                         them until its clock catches up — fix NTP on that machine (or on this one)",
                        who.join(", "),
                        crate::hlc::MAX_AHEAD_MICROS / 1_000_000
                    ),
                )
            } else if skew_secs > 30 {
                warn(
                    "clock",
                    format!(
                        "a mesh member's clock runs ~{skew_secs}s ahead of this node — \
                         fix NTP before it hits the 300s auth window"
                    ),
                )
            } else {
                ok("clock", "no significant skew observed in the mesh")
            });

            checks.push(oplog_check(config, user).await);
            if let Some(c) = archive_check(config, archive_window(config, user).await) {
                checks.push(c);
            }
            checks.extend(backpressure_checks(config, user).await);
            checks.extend(shed_checks(config, user).await);
            checks.extend(sink_checks(config, user).await);
            checks.extend(peer_checks(config, user).await);
            checks.extend(fact_checks(config, user).await);
        }
        // Answering slowly is not the same as not being there, and the
        // fix is not the same either: this is a daemon that is up and
        // stuck, and it is the one state where "is `meshnode up` running?"
        // sends an operator looking in exactly the wrong place.
        Err(_) => {
            checks.push(fail(
                "daemon",
                format!(
                    "listening, but it did not answer within {}s — the node is up and wedged.                      Check its log, then `meshnode drain` or restart it",
                    PROBE_TIMEOUT.as_secs()
                ),
            ));
        }
        // A daemon that answers but cannot be decoded is an older build
        // than this CLI (the IPC schema grew): finish the upgrade.
        Ok(Err(crate::CoreError::Proto(mesh_proto::ProtoError::Codec(_)))) => {
            checks.push(warn(
                "daemon",
                "answers, but its replies do not decode — the daemon is older than this CLI; \
                 restart it (`meshnode drain`, or `systemctl restart meshnode`) to finish the upgrade",
            ));
        }
        _ => {
            checks.push(warn(
                "daemon",
                "not reachable on the local socket (is `meshnode up` running?)",
            ));
            // With no daemon holding it, the configured port should be free.
            checks.push(match std::net::UdpSocket::bind(config.listen) {
                Ok(_) => ok("listen port", format!("{} is free", config.listen)),
                Err(e) => warn(
                    "listen port",
                    format!(
                        "{} not bindable ({e}) — another process holds it",
                        config.listen
                    ),
                ),
            });
        }
    }

    checks
}

/// What the replicated log costs, what it is made of, and how fast it grows
/// (M31 P0).
///
/// The point of putting it in `doctor` rather than only in `state show` is
/// that the operator who most needs this number is the one who has never
/// read a design document. `meshware` replicates its op-log to every node,
/// so the storage cost of the mesh is `O(surviving history) × O(nodes)` —
/// and the number that predicts next year is the *surviving* one, not the
/// total, because C0 already drops the rest.
///
/// It warns on a projected size rather than on a current one. A 200 MB log
/// is fine; a 200 MB log growing 400 MB a year on a fleet of a thousand
/// machines is the thing worth saying out loud before it is 70 GB.
async fn oplog_check(config: &NodeConfig, user: &Keypair) -> Check {
    let census = match probe(&config.socket_path(), user, IpcRequest::OpCensus).await {
        Ok(IpcResponse::OpCensus(c)) => *c,
        // An older daemon has no census. Say which half is missing rather
        // than reporting a health problem that is really a version skew —
        // the daemon check above already names the real fix.
        _ => {
            return ok(
                "op-log",
                "not reported by this daemon (older than the CLI — restart it)",
            );
        }
    };

    let per_day = census.surviving_bytes_per_day();
    let year = per_day.saturating_mul(365);
    let top: Vec<String> = census
        .top_kinds(3)
        .iter()
        .map(|k| format!("{} {}", k.kind, k.ops))
        .collect();
    let detail = format!(
        "{} on disk, {} ops ({} permanent){}{}",
        bytes_human(census.disk_bytes),
        census.ops_total,
        census.surviving_total,
        if per_day > 0 {
            format!(
                "; growing ~{}/day permanently, ~{}/year",
                bytes_human(per_day),
                bytes_human(year)
            )
        } else {
            String::new()
        },
        if top.is_empty() {
            String::new()
        } else {
            format!(" — {}", top.join(", "))
        }
    );

    // -- where it settles (M31 A3 item 3) ---------------------------------
    //
    // **The same projection Director shows**, from the same place, so an
    // operator reading `doctor` and an operator reading Settings see one
    // number rather than two that nearly agree. A1's whole design produces
    // this figure; a health check that reported growth without it would be
    // telling half the story, and the missing half is the reassuring one.
    let settle = match probe(&config.socket_path(), user, IpcRequest::RetentionShow).await {
        Ok(IpcResponse::Retention(v)) => Some(*v),
        _ => None,
    };
    let detail = match &settle {
        Some(v) => match v.settles_at_bytes {
            Some(at) => format!("{detail}; settles around {}", bytes_human(at)),
            None => format!("{detail}; does not settle — `runs` is kept forever"),
        },
        None => detail,
    };

    match oplog_verdict(
        year,
        settle.as_ref().map(|v| v.settles_at_bytes),
        settle
            .as_ref()
            .map(|v| v.retirable_bytes_per_day)
            .unwrap_or(0),
    ) {
        OplogVerdict::Unbounded => warn(
            "op-log",
            format!(
                "{detail}. Nothing retires closed records, so this grows for as long as the \
                 mesh runs — set a window with `meshnode retention set runs=90d`"
            ),
        ),
        OplogVerdict::SettlesHigh | OplogVerdict::GrowsFast => warn(
            "op-log",
            format!(
                "{detail}. Every node replicates all of it, and a joiner replicates it before \
                 it is useful — a shorter `runs` window is the lever"
            ),
        ),
        OplogVerdict::Fine => ok("op-log", detail),
    }
}

/// What the op-log projection is worth warning about (M31 A3 item 3).
///
/// Pure, because the interesting part is *which* threshold fires and in
/// what order, and that is a decision worth pinning rather than a shape
/// worth eyeballing.
///
/// Three cases rather than one, because they have different fixes:
/// *"nothing bounds this"* is a policy nobody set, *"bounded, at a big
/// number"* is a policy that is too generous, and *"growing fast"* is the
/// pre-A1 measurement still worth reporting when no projection is
/// available. **Unbounded outranks everything**: a mesh with no window is
/// the one case where the number on screen will keep being wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OplogVerdict {
    Fine,
    /// `runs` is `forever` while closed records still accrue.
    Unbounded,
    /// It settles, but at a size a joiner will notice.
    SettlesHigh,
    /// No projection available; the raw yearly growth is large.
    GrowsFast,
}

/// A gigabyte a year is where a new node's first sync stops being instant
/// and starts being something an operator schedules.
const OPLOG_YEAR_WARN_BYTES: u64 = 1024 * 1024 * 1024;

/// Four gigabytes settled. Nothing is broken at this size — the mesh is
/// working exactly as designed, and the design has a wall in it — so this
/// warns rather than fails, and it warns on the *projection*, well before
/// the log is anywhere near it.
const OPLOG_SETTLE_WARN_BYTES: u64 = 4 * 1024 * 1024 * 1024;

fn oplog_verdict(
    year_bytes: u64,
    settles_at: Option<Option<u64>>,
    retirable_per_day: u64,
) -> OplogVerdict {
    match settles_at {
        // A projection exists and says it never settles — but only when
        // something is actually accruing. `forever` on an idle mesh is a
        // setting, not a problem.
        Some(None) if retirable_per_day > 0 => OplogVerdict::Unbounded,
        Some(Some(at)) if at >= OPLOG_SETTLE_WARN_BYTES => OplogVerdict::SettlesHigh,
        // A projection that says it settles small is the reassuring case,
        // and it **overrides** raw growth: "growing 2 GB a year and
        // settling at 300 MB" is a healthy mesh with a busy week, and
        // warning about it is how a check gets ignored.
        Some(Some(_)) | Some(None) => OplogVerdict::Fine,
        None if year_bytes >= OPLOG_YEAR_WARN_BYTES => OplogVerdict::GrowsFast,
        None => OplogVerdict::Fine,
    }
}

/// The mesh's `archive` retention window, in days — if somebody chose it.
///
/// **Only a signed policy counts.** The shipped defaults carry an
/// `archive` window too, so counting them made every new node's first
/// `doctor` warn that "this mesh keeps 5y of archived history and nothing
/// has ever written an archive here" about an archive nobody asked for —
/// the nag `archive_check` exists not to be (DOC-12). An archive that
/// *does* exist is still checked for staleness either way.
fn archive_window_of(view: &mesh_proto::RetentionView) -> Option<u32> {
    view.signed_by.as_ref()?;
    Some(view.policy.window(mesh_proto::RetentionTable::Archive).days).filter(|d| *d > 0)
}

async fn archive_window(config: &NodeConfig, user: &Keypair) -> Option<u32> {
    match probe(&config.socket_path(), user, IpcRequest::RetentionShow).await {
        Ok(IpcResponse::Retention(v)) => archive_window_of(&v),
        _ => None,
    }
}

/// Is anything actually filling the archive? (M31 B4 item 4.)
///
/// **The dangerous combination is the one worth naming**: a retention
/// policy that says `archive: 5y` and an archive nothing has written to in
/// a month is five years of nothing — and it reads, on every screen, as a
/// mesh that keeps five years of history. A window is a promise about a
/// tier; this check is whether the tier exists.
///
/// Silent on a mesh with no archive directory *and* no archive window,
/// which is the ordinary case: the archive is opt-in and a `doctor` that
/// nags about features nobody turned on is a `doctor` people stop reading.
/// It speaks up in exactly two situations — a window with no archive, and
/// an archive that has gone stale.
fn archive_check(config: &NodeConfig, retention_days: Option<u32>) -> Option<Check> {
    let root = config.layout().archive_dir();
    let manifest = root.join("manifest.json");
    let wants_archive = retention_days.is_some_and(|d| d > 0);

    if !manifest.exists() {
        if !wants_archive {
            return None;
        }
        return Some(warn(
            "archive",
            format!(
                "this mesh keeps {} of archived history and nothing has ever written an \
                 archive here — that is a window over a tier that does not exist. Install \
                 `meshware-archive` and `systemctl enable --now meshware-archive.timer`",
                mesh_proto::retention::describe_days(retention_days.unwrap_or(0))
            ),
        ));
    }

    // How long since anybody ingested. Read from the manifest's mtime
    // rather than from a field inside it: every ingest rewrites the
    // manifest, including one that wrote no rows, so the file's own
    // timestamp is the honest answer to "did the timer run" — which is a
    // different question from "was there anything to copy".
    let stale_days = std::fs::metadata(&manifest)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);

    if stale_days >= ARCHIVE_STALE_DAYS {
        return Some(warn(
            "archive",
            format!(
                "nothing has been archived here for {stale_days} days{}. \
                 `systemctl status meshware-archive.timer`, or run \
                 `meshware-archive ingest` by hand",
                match retention_days {
                    Some(d) if d > 0 => format!(
                        " and this mesh keeps {} of it",
                        mesh_proto::retention::describe_days(d)
                    ),
                    _ => String::new(),
                }
            ),
        ));
    }
    Some(ok(
        "archive",
        format!("last ingest {stale_days} day(s) ago at {}", root.display()),
    ))
}

/// How long an archive may go unwritten before `doctor` says so.
///
/// Two days. The shipped timer is hourly, so two days is roughly fifty
/// missed runs — comfortably past "the machine was off for the weekend"
/// and comfortably short of "we lost a month and nobody noticed".
const ARCHIVE_STALE_DAYS: u64 = 2;

/// What this node is refusing, and what is churning (M31 A3).
///
/// Empty on a healthy mesh, and that is deliberate: a `doctor` that reports
/// on controls nobody has tripped is a `doctor` people stop reading. The
/// line appears the day something loops, which is the day somebody is
/// looking for it.
/// The mesh-wide shed, when one is in force (N6 §12).
///
/// **Silent on a mesh shedding nothing**, on the rule the backpressure
/// check below already follows: a `doctor` that reports on controls nobody
/// has tripped is a `doctor` people stop reading.
///
/// When one *is* in force it says two things, and the second is the one
/// that matters. Every other capability gate in meshware leaves an older
/// node doing **less**; this one leaves it doing **more** — a node that
/// cannot read the record keeps taking work — so a half-upgraded mesh
/// silently sheds only half. That is a `fail`, not a `warn`: the operator
/// believes the mesh has stopped, and it has not.
async fn shed_checks(config: &NodeConfig, user: &Keypair) -> Vec<Check> {
    let view = match probe(&config.socket_path(), user, IpcRequest::ShedStatus).await {
        Ok(IpcResponse::Shed(v)) => *v,
        _ => return Vec::new(),
    };
    if !view.submissions && !view.reconcile {
        return Vec::new();
    }
    let what = match (view.submissions, view.reconcile) {
        (true, true) => "new submissions and reconciliation are",
        (true, false) => "new submissions are",
        (false, true) => "deployment reconciliation and schedule firing are",
        (false, false) => unreachable!("filtered above"),
    };
    let by = view
        .by_name
        .as_deref()
        .map(|n| format!(", set by {n}"))
        .unwrap_or_default();
    let reason = if view.reason.trim().is_empty() {
        String::new()
    } else {
        format!(" — {}", view.reason.trim())
    };
    let mut checks = vec![warn(
        "load shed",
        format!(
            "{what} shed mesh-wide{by}{reason}. Running jobs are unaffected. \
             `meshnode shed clear` lifts it."
        ),
    )];
    if let Some(w) = view.readiness.warning() {
        checks.push(fail("load shed reach", w));
    }
    checks
}

async fn backpressure_checks(config: &NodeConfig, user: &Keypair) -> Vec<Check> {
    let view = match probe(&config.socket_path(), user, IpcRequest::Backpressure).await {
        Ok(IpcResponse::Backpressure(v)) => *v,
        _ => return Vec::new(),
    };
    let mut checks = Vec::new();

    if !view.entities.is_empty() {
        let paused = view
            .entities
            .iter()
            .filter(|e| e.paused_until_micros > 0)
            .count();
        let worst = &view.entities[0];
        checks.push(warn(
            "op rate",
            format!(
                "{} entit{} over this node's limit of {}/min — worst is `{}` at {}/min, {} ops \
                 refused{}. Refused here, where they were produced; `meshnode audit \
                 --action state.backpressure` has the record",
                view.entities.len(),
                if view.entities.len() == 1 { "y" } else { "ies" },
                view.limit_per_minute,
                worst.entity,
                worst.observed_per_minute,
                worst.refused_total,
                if paused > 0 {
                    format!(", {paused} auto-paused (it expires by itself)")
                } else {
                    String::new()
                }
            ),
        ));
    }

    if !view.flapping.is_empty() {
        checks.push(warn(
            "fact churn",
            format!(
                "{} collector(s) held back for changing again inside the publish window: {}. \
                 A collector that reports a different value every run is measuring noise",
                view.flapping.len(),
                summarize(&view.flapping.iter().map(String::as_str).collect::<Vec<_>>())
            ),
        ));
    }
    checks
}

/// What every sink is doing, and whether anything is being lost (M31 B1).
///
/// Nothing here fires on a mesh with no sinks, which is nearly every mesh:
/// the check returns an empty list rather than an "ok — no sinks" line,
/// because a `doctor` that reports on features nobody turned on is a
/// `doctor` people stop reading.
async fn sink_checks(config: &NodeConfig, user: &Keypair) -> Vec<Check> {
    let rows = match probe(&config.socket_path(), user, IpcRequest::Sinks).await {
        Ok(IpcResponse::Sinks(rows)) => rows,
        _ => return Vec::new(),
    };
    if rows.is_empty() {
        return Vec::new();
    }
    let mut checks = Vec::new();

    // The latch first: everything else reads as broken while it is on, and
    // the answer is one command rather than a network investigation.
    if rows.iter().any(|r| r.paused_by_restore) {
        checks.push(warn(
            "sinks",
            format!(
                "{} sink(s) held: this node's data was restored from a backup, so nothing is \
                 exporting. Review them, then `meshnode sinks resume`",
                rows.len()
            ),
        ));
        return checks;
    }

    for row in &rows {
        if !row.enabled {
            continue;
        }
        // A sink nobody can export is the failure that looks like nothing.
        // It is what `run_on` gets wrong in a real deployment: the tag was
        // set, and no live node carries it.
        if row.elected.is_empty() {
            checks.push(warn(
                "sink",
                format!(
                    "{}: no live node is eligible to export it — check its `run_on`, and that \
                     an eligible node is running a build new enough to have sinks",
                    row.name
                ),
            ));
            continue;
        }
        // A permanent export gap outranks everything: it is the one state
        // here that means history was deleted before it reached the
        // destination, deliberately, because the sink's own ceiling passed.
        if row.forced_gaps > 0 {
            checks.push(warn(
                "sink",
                format!(
                    "{}: {} record(s) were deleted before this sink took them — its {}-day \
                     hold expired while the destination was unreachable. The export gap is \
                     permanent",
                    row.name, row.forced_gaps, row.hold_for_export_days
                ),
            ));
            continue;
        }
        // Dropping is the one thing that is actually lost, so it outranks
        // lag however far behind the sink is.
        if row.dropped > 0 {
            checks.push(warn(
                "sink",
                format!(
                    "{}: {} batch(es) dropped — the spool filled while `{}` was failing. \
                     Those rows are gone and will not be re-sent",
                    row.name, row.dropped, row.last_status
                ),
            ));
            continue;
        }
        if row.lag_secs >= SINK_LAG_WARN_SECS {
            checks.push(warn(
                "sink",
                format!(
                    "{}: {} behind, exporting from {}{}",
                    row.name,
                    humantime::format_duration(std::time::Duration::from_secs(row.lag_secs)),
                    row.elected,
                    if row.last_status.is_empty() {
                        String::new()
                    } else {
                        format!(" (last: {})", row.last_status)
                    }
                ),
            ));
            continue;
        }
        checks.push(ok(
            "sink",
            format!(
                "{}: {} → {}, {} sent, exporting from {}",
                row.name, row.kind, row.target, row.sent, row.elected
            ),
        ));
    }
    checks
}

/// How far behind a sink may fall before `doctor` says so.
///
/// An hour. Short enough that a warehouse that went down over lunch is
/// noticed; long enough that the five-minute settle lag and a slow batch
/// never trip it.
const SINK_LAG_WARN_SECS: u64 = 3_600;

/// Bytes as an operator reads them. Local to this module rather than shared
/// with the CLI's formatter: `doctor` runs where `meshware-node` does not.
fn bytes_human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 6] = [
        ("PB", 1024 * 1024 * 1024 * 1024 * 1024),
        ("TB", 1024 * 1024 * 1024 * 1024),
        ("GB", 1024 * 1024 * 1024),
        ("MB", 1024 * 1024),
        ("KB", 1024),
        ("B", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return if scale == 1 {
                format!("{bytes} B")
            } else {
                format!("{:.1} {unit}", bytes as f64 / scale as f64)
            };
        }
    }
    "0 B".to_string()
}

/// Free space against **this node's own thresholds** (M31 A2).
///
/// The numbers used to be literals here — 200 MB and 2 GB — which made the
/// doctor's verdict and the daemon's behaviour two unrelated opinions about
/// the same disk. A node configured to reclaim below 20 GB would have been
/// deleting bundles for a week while `doctor` said `ok`. Now there is one
/// set of numbers, `[disk]` owns it, and the check reports what the node
/// will actually *do* rather than a guess about what is comfortable.
fn disk_check(data_dir: &Path, disk: crate::config::DiskConfig) -> Check {
    let Some(free_mb) = crate::capacity::disk_free_mb(data_dir) else {
        return warn("disk", "free-space probe unsupported on this platform");
    };
    let free = free_mb.saturating_mul(1024 * 1024);
    let floor = disk.min_free_bytes(data_dir);
    let warn_at = disk.warn_bytes(data_dir);
    if free < floor {
        fail(
            "disk",
            format!(
                "{} free at the data dir, under the {} floor — this node is \
                 reclaiming its content store and will refuse new bundles",
                bytes_human(free),
                bytes_human(floor)
            ),
        )
    } else if free < warn_at {
        warn(
            "disk",
            format!(
                "{} free at the data dir, heading for the {} floor",
                bytes_human(free),
                bytes_human(floor)
            ),
        )
    } else {
        ok(
            "disk",
            format!("{} free at the data dir", bytes_human(free)),
        )
    }
}

/// Facts (M18 §4.6, R-3 and R-5).
///
/// Two questions, and the first is the one this milestone must be able to
/// answer about itself: *is any collector flapping?* A collector that
/// publishes a measurement instead of a property would write an unbounded,
/// uncompactable op-log, so the flap guard holds it back — and this is
/// where a person finds out, **with the keys that moved named**. "Your
/// collector is wrong" is a complaint; "your collector emits `uptime`" is a
/// fix.
///
/// The second is R-5: a `fact.*` selector inside a live policy matches
/// *less* on a node that has no code for it, and matching less is the
/// unsafe direction for a policy. The same warning covers grants, where the
/// direction is safe but the surprise is identical.
async fn fact_checks(config: &NodeConfig, user: &Keypair) -> Vec<Check> {
    let socket = config.socket_path();
    let collectors = match probe(&socket, user, IpcRequest::FactCollectors).await {
        Ok(IpcResponse::FactCollectors(rows)) => rows,
        _ => return Vec::new(),
    };
    let mut checks = Vec::new();

    let flapping: Vec<String> = collectors
        .iter()
        .filter(|c| !c.flapping_keys.is_empty())
        .map(|c| format!("{} (changed: {})", c.name, c.flapping_keys.join(", ")))
        .collect();
    let subjects: u32 = collectors.iter().map(|c| c.subjects).sum();
    checks.push(if flapping.is_empty() {
        ok(
            "fact collectors",
            format!(
                "{} collector(s), {subjects} subject document(s), none flapping",
                collectors.len()
            ),
        )
    } else {
        fail(
            "fact collectors",
            format!(
                "{} collector(s) publishing faster than once an hour: {} — a fact is a \
                 property of the machine, not a measurement of it; the named keys are the \
                 ones changing when nothing changed",
                flapping.len(),
                flapping.join("; ")
            ),
        )
    });

    checks
}

/// Name a few, count the rest. A diagnosis nobody can read is not one.
fn summarize(names: &[&str]) -> String {
    const SHOWN: usize = 3;
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, +{} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

async fn binary_check(bin: &'static str, label: &str, required: bool) -> Check {
    // stdin is closed, not inherited: `doctor` runs inside a desktop app as
    // often as in a terminal, and a runtime that decides to read from a
    // terminal it was handed would stall the whole report on a probe that
    // was supposed to take milliseconds.
    let probe = tokio::process::Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let found = matches!(
        tokio::time::timeout(BINARY_TIMEOUT, probe).await,
        Ok(Ok(s)) if s.success()
    );
    if found {
        ok(bin, format!("{label} found"))
    } else if required {
        fail(bin, format!("{label} NOT found on PATH"))
    } else {
        warn(bin, format!("{label} not found on PATH"))
    }
}

/// Reachability of live members, through the daemon's transport.
async fn peer_checks(config: &NodeConfig, user: &Keypair) -> Vec<Check> {
    let mut checks = Vec::new();
    let Ok(IpcResponse::Nodes(rows)) = probe(&config.socket_path(), user, IpcRequest::Nodes).await
    else {
        return checks;
    };
    // What this node is *to the mesh*, before anything about its peers.
    // Both of these look identical from the outside — an idle machine
    // nothing is ever scheduled on — and have completely different fixes,
    // so an operator running `doctor` on the box should not have to infer
    // which one they are in.
    if let Some(me) = rows.iter().find(|r| r.is_self)
        && !me.contributes
    {
        checks.push(warn(
            "fleet",
            format!(
                "an administrator of this mesh has not allowed this node to run work{}. \
                     It is still a full member — state, submission and terminals are \
                     unaffected — and an admin can reverse it with \
                     `meshnode nodes contribute {} --on`",
                if me.contribute_reason.trim().is_empty() {
                    String::new()
                } else {
                    format!(" ({})", me.contribute_reason)
                },
                me.name
            ),
        ));
    }
    let peers: Vec<_> = rows
        .iter()
        .filter(|r| !r.is_self && !r.revoked && !r.left)
        .collect();
    let dead = peers
        .iter()
        .filter(|r| r.liveness == Liveness::Dead)
        .count();
    if dead > 0 {
        checks.push(warn(
            "members",
            format!("{dead} member(s) observed dead (of {})", peers.len()),
        ));
    } else if !peers.is_empty() {
        checks.push(ok("members", format!("{} peer member(s)", peers.len())));
    }

    // -- the gaps ENG-26 named --------------------------------------------
    //
    // **Suspect is not dead, and that is the point.** A dead peer is a
    // decided fact an operator can act on. A *suspect* one is the mesh
    // saying it has missed this node for a while and has not yet given up
    // — which is the state a flapping link, a machine that swaps itself to
    // a halt, or a firewall rule somebody half-applied actually sits in,
    // sometimes for days, while every dashboard shows the fleet as healthy
    // because nothing has crossed the line into dead.
    let suspect = peers
        .iter()
        .filter(|r| r.liveness == Liveness::Suspect)
        .count();
    if suspect > 0 {
        checks.push(warn(
            "peers suspect",
            format!(
                "{suspect} member(s) are neither alive nor declared dead — this node has missed \
                 them for long enough to notice and not long enough to give up. A link that \
                 flaps sits here indefinitely; `meshnode nodes` names them"
            ),
        ));
    }

    // **A peer that has stopped asking for ops.** Behind is normal — a
    // gossip round is seconds and every peer is behind between rounds.
    // What is not normal is a reading that is *old*: the peer has not
    // asked at all, so nothing is flowing to it and nothing will say so,
    // because it is alive and answering pings the whole time.
    let stalled: Vec<&str> = peers
        .iter()
        .filter(|r| r.liveness == Liveness::Alive)
        .filter(|r| {
            r.sync
                .is_some_and(|s| s.behind > 0 && s.age_micros > SYNC_STALL_AFTER.as_micros() as u64)
        })
        .map(|r| r.name.as_str())
        .collect();
    if !stalled.is_empty() {
        let worst = peers
            .iter()
            .filter_map(|r| r.sync.map(|s| s.behind))
            .max()
            .unwrap_or(0);
        checks.push(warn(
            "sync lag",
            format!(
                "{} peer(s) last asked this node for ops more than {}s ago and are still behind \
                 (worst: {worst} ops): {}. They are alive, so nothing else reports this",
                stalled.len(),
                SYNC_STALL_AFTER.as_secs(),
                stalled.join(", ")
            ),
        ));
    }

    // Rolling-upgrade visibility (F0 §3.3). A pre-1.7 peer is reachable
    // and healthy but cannot be sent ops whose bodies it has no code for,
    // so it is deliberately behind on those entity kinds. Warn rather than
    // fail: this is the expected middle of an upgrade window, and it ends
    // when the peer upgrades.
    let stale: Vec<_> = peers
        .iter()
        .filter_map(|r| r.stale.map(|s| (r, s)))
        .collect();
    if stale.is_empty() {
        if !peers.is_empty() {
            checks.push(ok(
                "op-log compatibility",
                "every peer speaks the forward-compatible op-log",
            ));
        }
    } else {
        let ops: u64 = stale.iter().map(|(_, s)| s.ops_withheld).sum();
        let names: Vec<String> = stale
            .iter()
            .map(|(r, s)| format!("{} (1.{})", r.name, s.minor))
            .collect();
        checks.push(warn(
            "op-log compatibility",
            format!(
                "{} peer(s) cannot receive {ops} op(s): {}. They predate protocol 1.{} \
                 and have no code for those entity kinds, so they are stably behind on \
                 them — not wedged, and not losing anything they could render. \
                 Upgrade them to clear it.",
                stale.len(),
                names.join(", "),
                mesh_proto::OPLOG_MINOR,
            ),
        ));
    }
    for row in peers
        .iter()
        .filter(|r| r.liveness == Liveness::Alive)
        .take(5)
    {
        if row.addrs.is_empty() {
            checks.push(warn("peer", format!("{}: no dialable address", row.name)));
            continue;
        }
        // By id, not by an address picked off the announcement: the daemon
        // races every candidate and knows the relay routes, so a reachable
        // peer never reads as unreachable here because its first announced
        // address happens to be its own loopback.
        let result = probe(
            &config.socket_path(),
            user,
            IpcRequest::PingNode { node: row.node_id },
        )
        .await;
        checks.push(match result {
            Ok(IpcResponse::Ping(p)) => ok(
                "peer",
                format!(
                    "{} reachable via {} ({} µs rtt)",
                    row.name, p.via, p.rtt_micros
                ),
            ),
            Ok(IpcResponse::Error { message }) => fail("peer", format!("{}: {message}", row.name)),
            _ => fail("peer", format!("{}: unreachable", row.name)),
        });
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped defaults are nobody's decision to keep an archive, so a
    /// new mesh's `doctor` does not warn about one; a signed policy is.
    #[test]
    fn only_a_signed_policy_asks_for_an_archive() {
        let mut view = mesh_proto::RetentionView {
            policy: mesh_proto::retention::RetentionPolicy::default(),
            windows: Vec::new(),
            signed_by: None,
            signed_at_micros: 0,
            oplog_bytes: 0,
            permanent_bytes_per_day: 0,
            retirable_bytes_per_day: 0,
            settles_at_bytes: None,
        };
        assert_eq!(
            archive_window_of(&view),
            None,
            "the defaults asked for nothing"
        );
        view.signed_by = Some("ada".into());
        assert!(archive_window_of(&view).is_some(), "a signed policy did");
    }

    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;

    /// **The check warns on the projection, before anything is critical.**
    ///
    /// That is the whole point of having one: a 200 MB log is fine, and a
    /// 200 MB log heading for 70 GB across a fleet is worth saying while it
    /// is still cheap to change. So the thresholds are asserted rather than
    /// eyeballed — including the order, because "unbounded" and "bounded at
    /// a big number" have different fixes and the first must win.
    #[test]
    fn the_oplog_check_warns_on_where_it_is_heading_not_on_where_it_is() {
        // Small now, unbounded policy, records still accruing: the warning
        // that matters, and it fires while the log is tiny.
        assert_eq!(
            oplog_verdict(10 * MB, Some(None), 3 * MB),
            OplogVerdict::Unbounded
        );
        // `forever` with nothing accruing is a setting, not a problem.
        assert_eq!(oplog_verdict(10 * MB, Some(None), 0), OplogVerdict::Fine);
        // Bounded, but at a size a joiner will notice.
        assert_eq!(
            oplog_verdict(10 * MB, Some(Some(9 * GB)), 3 * MB),
            OplogVerdict::SettlesHigh
        );
        // **A projection that settles small overrides raw growth.** A busy
        // week on a bounded mesh is not a health problem, and a check that
        // cried about it is a check people stop reading.
        assert_eq!(
            oplog_verdict(4 * GB, Some(Some(300 * MB)), 12 * MB),
            OplogVerdict::Fine
        );
        // No projection (an older daemon): fall back to raw growth.
        assert_eq!(oplog_verdict(4 * GB, None, 0), OplogVerdict::GrowsFast);
        assert_eq!(oplog_verdict(100 * MB, None, 0), OplogVerdict::Fine);
    }

    /// The boundary itself, so a refactor cannot quietly move it.
    #[test]
    fn the_thresholds_are_where_they_say_they_are() {
        assert_eq!(
            oplog_verdict(OPLOG_YEAR_WARN_BYTES - 1, None, 0),
            OplogVerdict::Fine
        );
        assert_eq!(
            oplog_verdict(OPLOG_YEAR_WARN_BYTES, None, 0),
            OplogVerdict::GrowsFast
        );
        assert_eq!(
            oplog_verdict(0, Some(Some(OPLOG_SETTLE_WARN_BYTES - 1)), MB),
            OplogVerdict::Fine
        );
        assert_eq!(
            oplog_verdict(0, Some(Some(OPLOG_SETTLE_WARN_BYTES)), MB),
            OplogVerdict::SettlesHigh
        );
    }
}
