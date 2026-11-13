//! `meshnode adopt ops@rack-{1..10}` — a rack in one command.
//!
//! The pitch has always been that SSH is what the mesh replaces. This is the
//! same claim, made literal: use the SSH access people already have **once**
//! — install the binary, set up the service, pair, start — and from then on
//! the mesh is the transport and SSH is never needed again.
//!
//! ## What it does per host
//!
//! 1. `ssh host` and read back what it is: OS, architecture, whether
//!    meshnode is already there, whether it runs systemd, and what the
//!    account can reach (`mesh_core::deploy::probe_script`).
//! 2. Push a binary for that architecture (this machine's own when it
//!    matches; `--binary` otherwise), stage it, *run* it, install it.
//! 3. Write the packaged systemd unit — the same file a `.deb` ships —
//!    with the service account and data directory, plus the opt-in
//!    fleet-upgrade drop-in and a firewall rule where a firewall runs.
//! 4. Start `meshnode join --pair --json` there **as the service
//!    account**, over the same SSH session, so the identity pairing makes
//!    is the identity the service will run with.
//! 5. Dial its pairing endpoint from *this* node's daemon, run the
//!    ordinary exchange, and confirm.
//! 6. Start the service, and report which one it was.
//!
//! Every remote command comes from `mesh_core::deploy`, which Director's
//! **Add machines** uses too, so a node installed from either ends up the
//! same node.
//!
//! ## The confirmation is not skipped
//!
//! `mesh_core::pairing`'s security model is a **numeric comparison**: two
//! parties derive the same six digits from a commitment exchange bound to
//! two TLS-pinned node keys, and both must confirm. An attacker bridging
//! two pairing connections shows each side an independent uniformly-random
//! code, so agreement means there is nobody in the middle.
//!
//! That argument does not care that the comparer is a human. It cares that
//! the code crosses a **second channel the attacker does not control**.
//! Across a room, that channel is a person's eyes. Here it is the SSH
//! session: host-key verified, user authenticated, already trusted enough
//! that this command is about to install a binary through it. So the joiner
//! prints its derived code on SSH stdout, this side compares it with what
//! its own daemon derived, and answers on SSH stdin.
//!
//! Every code is still printed. A mismatch is **not** a retry — it is the
//! one signal the exchange exists to produce, so the host is abandoned
//! loudly and the run's exit code says so.
//!
//! ## Privilege
//!
//! `adopt` runs `ssh` in BatchMode and never prompts, so the only sudo it
//! can use is one that asks no password. Root, or passwordless sudo, gets
//! the server shape: `/usr/local/bin`, a `meshnode` service account, a
//! system unit. Anything less gets the account's own home, a user unit,
//! and a note saying so — `--sudo` turns that fallback into an error for
//! people who meant to have root. Director, which can carry a password,
//! has no such limit.
//!
//! ## What it deliberately is not
//!
//! Not a configuration manager. It does not template a `meshnode.toml`
//! or come back later to converge anything — Ansible owns machines the
//! mesh reaches but does not run on (MASTERPLAN, the anti-roadmap), and
//! this is the one command that turns a machine into one the mesh *does*
//! run on. After it, `meshnode upgrade` is how binaries move.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use mesh_core::deploy::{self, InstallPlan, Privilege, Probe, Scope};
use mesh_proto::{IpcRequest, IpcResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// How long a probe (`uname`, a version check) may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an install step may take — the binary is ~20 MB, and a slow
/// link to a far rack is the normal case, not the exception.
const STEP_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for the joiner to announce its pairing port.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the pairing exchange once both sides are up.
const PAIR_TIMEOUT: Duration = Duration::from_secs(180);

/// Everything `meshnode adopt` was asked to do.
#[derive(Debug, Clone)]
pub struct Options {
    pub targets: Vec<String>,
    /// Hosts adopted at once.
    pub parallel: usize,
    /// Push this binary rather than resolving one.
    pub binary: Option<PathBuf>,
    /// Where the binary goes on the remote host; the scope's default when
    /// not given.
    pub install_path: Option<String>,
    /// Insist on root or passwordless sudo: refuse rather than fall back
    /// to a user install.
    pub sudo: bool,
    /// Never escalate, even where passwordless sudo would work.
    pub no_sudo: bool,
    /// Write the fleet-upgrade drop-in (M24 §4).
    pub fleet_upgrade: bool,
    /// Open the mesh port where firewalld/ufw is running.
    pub open_firewall: bool,
    /// Tags for the adopted nodes.
    pub tags: Vec<String>,
    /// Extra `ssh -o` options (repeatable).
    pub ssh_options: Vec<String>,
    /// Dial the joiner at this host instead of the SSH host — for a machine
    /// whose SSH name is not the address its pairing port answers on.
    pub via: Option<String>,
    /// Report what would happen and change nothing.
    pub dry_run: bool,
    /// Re-install and re-pair a host that already runs meshnode.
    pub force: bool,
}

/// One host's outcome, for the summary table.
#[derive(Debug)]
pub struct Outcome {
    pub target: String,
    pub result: Result<Adopted>,
}

#[derive(Debug, Clone)]
pub struct Adopted {
    pub node_name: String,
    pub node_id: String,
    pub code: String,
    /// The binary was already there at the right version; nothing pushed.
    pub already_installed: bool,
    /// `system` or `user`.
    pub scope: &'static str,
    /// What is keeping the daemon up: `systemd`, `systemd-user`, `nohup`.
    pub service: String,
    /// Anything the operator should hear once, in a sentence.
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// SSH
// ---------------------------------------------------------------------------

/// The system `ssh`, and deliberately not a Rust SSH client.
///
/// The whole premise is "the access people already have": their
/// `~/.ssh/config`, their `ProxyJump` through a bastion, their agent, their
/// `known_hosts`, their hardware key. A library reimplements a fraction of
/// that and gets the host-key policy — the thing this command's security
/// argument rests on — subtly different. Shelling out inherits all of it,
/// exactly as the operator has already tested it.
///
/// (Director carries its own client, because a desktop app has to be able
/// to take a password from a person. That is the one place the two doors
/// differ, and it is documented there.)
fn ssh_command(target: &str, options: &[String]) -> Command {
    let mut cmd = Command::new("ssh");
    // Never prompt. An adopt across ten machines that stops on machine four
    // waiting for a password nobody is watching for is worse than a
    // failure, because it looks like a hang.
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o").arg("ConnectTimeout=10");
    // `ask` rather than `accept-new` or `no`: this command is about to
    // install a binary and carry a pairing code, and an unknown host key is
    // exactly when a person should be asked. With BatchMode on, `ask`
    // *fails* rather than prompting — which is the right outcome here, with
    // an error the caller turns into an actionable sentence.
    cmd.arg("-o").arg("StrictHostKeyChecking=ask");
    for opt in options {
        cmd.arg("-o").arg(opt);
    }
    cmd.arg(target);
    cmd
}

/// Turn an `ssh` failure into a sentence that sends people somewhere.
fn ssh_failure(target: &str, stderr: &[u8]) -> anyhow::Error {
    let err = String::from_utf8_lossy(stderr).trim().to_string();
    if err.contains("Host key verification failed") {
        return anyhow!(
            "{target}: host key not in known_hosts. `meshnode adopt` never accepts an unknown \
             key on your behalf — it is about to install a binary there. SSH to it once by \
             hand, then re-run"
        );
    }
    if err.contains("Permission denied") {
        return anyhow!(
            "{target}: SSH refused the key. `adopt` runs with BatchMode, so there is no \
             password prompt — load your key into an agent, or name one with \
             `-o IdentityFile=…`"
        );
    }
    anyhow!(
        "{target}: {}",
        if err.is_empty() {
            "ssh failed".into()
        } else {
            err
        }
    )
}

/// Run one command on the host and return its stdout, refusing on failure.
async fn ssh_capture(target: &str, options: &[String], script: &str) -> Result<String> {
    let mut cmd = ssh_command(target, options);
    cmd.arg(script);
    cmd.stdin(Stdio::null());
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("timed out talking to {target}"))?
        .with_context(|| {
            format!("could not run ssh — is it installed and on PATH? (target {target})")
        })?;
    if !output.status.success() {
        return Err(ssh_failure(target, &output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run one command on the host with `stdin` piped in, and return its
/// stdout. Fails with the remote stderr when the command did.
async fn ssh_feed(
    target: &str,
    options: &[String],
    command: &str,
    stdin_bytes: Vec<u8>,
) -> Result<String> {
    let mut cmd = ssh_command(target, options);
    cmd.arg(command);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning ssh")?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no ssh stdin"))?;
    let write = tokio::spawn(async move {
        let r = stdin.write_all(&stdin_bytes).await;
        let _ = stdin.shutdown().await;
        r
    });
    let output = tokio::time::timeout(STEP_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{target}: timed out"))?
        .context("ssh")?;
    write.await.ok();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        // The script's own `mw.error=` line, when it wrote one, beats a
        // stderr full of `set -e` noise.
        if let Some(e) = deploy::kv_value(&stdout, "error") {
            bail!("{target}: {e}");
        }
        // And a diagnosis the script gathered on the spot beats both —
        // systemd's `203/EXEC` describes a label problem as a missing
        // executable, which sends the reader looking at the file.
        if let Some(why) = deploy::start_failure_hint(&stdout) {
            bail!("{target}: {why}");
        }
        return Err(ssh_failure(target, &output.stderr));
    }
    Ok(stdout)
}

/// The command that runs an inline POSIX script under `sh`, whatever the
/// account's login shell is.
fn sh_c(script: &str) -> String {
    format!("sh -c {}", deploy::shell_quote(script))
}

/// The binary to push to a host of this shape.
fn binary_for(probe: &Probe, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if !probe.os.eq_ignore_ascii_case("linux") {
        bail!(
            "{} is not Linux, and only Linux peers execute jobs (MASTERPLAN D6). Adopt it by \
             hand if you want it as a non-executing member",
            probe.os
        );
    }
    let want = deploy::normalise_arch(&probe.arch);
    if want != deploy::normalise_arch(std::env::consts::ARCH) {
        bail!(
            "this machine is {} and that one is {want} — point `--binary` at a {want} meshnode \
             (a release download, or a cross build). Fetching one per architecture is not \
             something adopt does behind your back: the binary it installs is the binary you \
             chose",
            std::env::consts::ARCH
        );
    }
    std::env::current_exe().context("cannot find this meshnode binary to copy")
}

// ---------------------------------------------------------------------------
// Pairing over the SSH channel
// ---------------------------------------------------------------------------

/// One NDJSON frame from `meshnode join --pair --json`.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum JoinerFrame {
    Listening {
        port: u16,
        #[allow(dead_code)]
        addr: String,
        node_name: String,
    },
    Code {
        code: String,
        #[allow(dead_code)]
        words: Option<String>,
        #[allow(dead_code)]
        mesh_name: String,
    },
    Joined {
        #[allow(dead_code)]
        mesh_name: String,
        #[allow(dead_code)]
        accepted_by: String,
        #[allow(dead_code)]
        members: u64,
    },
    Error {
        message: String,
    },
}

/// The joiner process on the far end of an SSH pipe.
struct Joiner {
    child: Child,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin: tokio::process::ChildStdin,
}

impl Joiner {
    async fn start(target: &str, options: &[String], command: &str) -> Result<Joiner> {
        let mut cmd = ssh_command(target, options);
        cmd.arg(command);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawning the remote joiner")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no ssh stdout"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no ssh stdin"))?;
        Ok(Joiner {
            child,
            lines: BufReader::new(stdout).lines(),
            stdin,
        })
    }

    /// The next NDJSON frame, skipping anything that is not one.
    ///
    /// A remote shell profile that prints a banner is not a protocol error,
    /// it is Tuesday — so unparseable lines are ignored rather than fatal.
    async fn next_frame(&mut self, within: Duration) -> Result<JoinerFrame> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let line = tokio::time::timeout_at(deadline, self.lines.next_line())
                .await
                .map_err(|_| anyhow!("the remote joiner went quiet"))?
                .context("reading from the remote joiner")?;
            let Some(line) = line else {
                bail!("the remote joiner exited before pairing");
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(frame) = serde_json::from_str::<JoinerFrame>(trimmed) {
                return Ok(frame);
            }
        }
    }

    async fn answer(&mut self, accept: bool) -> Result<()> {
        let verdict = if accept { "yes\n" } else { "no\n" };
        self.stdin.write_all(verdict.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// End the remote joiner now.
    ///
    /// A declining *approver* closes the pairing connection and never sends
    /// the confirm round, so the joiner has nothing to react to and waits
    /// out its own `--wait` — minutes of a process sitting on a machine
    /// that was not adopted. A person can press Ctrl-C; a rack of ten
    /// cannot, and ten of those in series is the difference between a
    /// command that failed and a command that hung.
    ///
    /// Adopt started this process, so adopt ends it. Killing the local
    /// `ssh` closes the channel, which is what takes the remote command
    /// down with it.
    async fn abandon(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Whatever the remote wrote to stderr, for an error message.
    async fn stderr_tail(&mut self) -> String {
        let Some(mut err) = self.child.stderr.take() else {
            return String::new();
        };
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut err, &mut buf).await;
        String::from_utf8_lossy(&buf).trim().to_string()
    }
}

/// The shape a host gets, from what the probe said and what the operator
/// asked for.
fn plan_for(opts: &Options, probe: &Probe) -> Result<(InstallPlan, Privilege, Option<String>)> {
    let priv_ = probe.privilege.unwrap_or(Privilege::None);
    let mut note = None;
    let elevate = match priv_ {
        Privilege::Root => Privilege::Root,
        Privilege::SudoNoPassword if !opts.no_sudo => Privilege::SudoNoPassword,
        Privilege::SudoNoPassword => Privilege::None,
        Privilege::SudoPassword | Privilege::None => {
            if opts.sudo {
                bail!(
                    "{} is not root and sudo there needs a password. `adopt` never prompts \
                     (BatchMode), so give that account passwordless sudo, or drop --sudo to \
                     install inside its home instead — or use Director → Add machines, which \
                     can carry a password",
                    probe.user
                );
            }
            note = Some(format!(
                "{} cannot reach root without a password, so the node lives in its home and \
                 runs as that account (stops at logout unless `loginctl enable-linger` is on)",
                probe.user
            ));
            Privilege::None
        }
    };
    let mut plan = if elevate.can_elevate() {
        InstallPlan::system(opts.install_path.as_deref())
    } else {
        let mut p = InstallPlan::user(&probe.home, &probe.user);
        if let Some(path) = opts
            .install_path
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            p.install_path = path.to_string();
        }
        p
    };
    plan.fleet_upgrade = opts.fleet_upgrade;
    plan.open_firewall = opts.open_firewall && plan.scope == Scope::System;
    // Every field of this plan is about to be interpolated into a systemd
    // unit and a root-run script (S-M5). `install_path` is a flag somebody
    // typed; `data_dir`, `config_home` and `service_user` are derived from
    // that or from what the *remote machine* said its home and account are,
    // which is not the same as trustworthy.
    plan.validate().map_err(|e| anyhow::anyhow!(e))?;
    Ok((plan, elevate, note))
}

/// Adopt one host, start to finish. `pb` is that host's line on the
/// terminal; every stage says where it is.
async fn adopt_one(
    ctx: &crate::Ctx,
    opts: &Options,
    target: &str,
    pb: &ProgressBar,
) -> Result<Adopted> {
    pb.set_message("connecting");
    let probe_path = opts
        .install_path
        .clone()
        .unwrap_or_else(|| "/usr/local/bin/meshnode".into());
    let out = ssh_capture(
        target,
        &opts.ssh_options,
        &sh_c(&deploy::probe_script(&probe_path, "/var/lib/meshware")),
    )
    .await?;
    let probe = deploy::parse_probe(&out).with_context(|| target.to_string())?;
    if probe.existing_mesh.is_some() && !opts.force {
        bail!(
            "{target} already belongs to a mesh. Adopting it again would pair a second identity \
             on the same machine; pass --force if that is really what you want, or \
             `meshnode upgrade` if you only meant to move its binary"
        );
    }
    let (plan, elevate, note) = plan_for(opts, &probe)?;
    pb.set_message(format!(
        "{} · {} {}",
        probe.hostname, probe.distro, probe.arch
    ));

    let already_installed = probe
        .version
        .as_deref()
        .is_some_and(|v| v.contains(env!("CARGO_PKG_VERSION")))
        && probe.exe.as_deref() == Some(plan.install_path.as_str());
    let scope = match plan.scope {
        Scope::System => "system",
        Scope::User => "user",
    };
    if opts.dry_run {
        return Ok(Adopted {
            node_name: target.to_string(),
            node_id: String::new(),
            code: String::new(),
            already_installed,
            scope,
            service: if probe.systemd {
                if plan.scope == Scope::System {
                    "systemd".into()
                } else {
                    "systemd-user".into()
                }
            } else {
                "nohup".into()
            },
            note,
        });
    }

    // A password never travels here (BatchMode), so the privileged
    // wrappers never need one on stdin.
    let sh_s = elevate.wrap(None, "sh -s");

    if opts.force && probe.existing_mesh.is_some() {
        ssh_feed(
            target,
            &opts.ssh_options,
            &sh_s,
            plan.reset_script().into_bytes(),
        )
        .await
        .context("resetting the previous install")?;
    }

    // Stage, then install. Two steps because they run as two accounts: the
    // stage is the SSH user's own, and the install is the one privileged
    // thing this command does.
    let staged = if already_installed {
        None
    } else {
        let binary = binary_for(&probe, opts.binary.as_deref())?;
        let bytes = tokio::fs::read(&binary)
            .await
            .with_context(|| format!("reading {}", binary.display()))?;
        pb.set_message(format!(
            "sending meshnode ({:.1} MB)",
            bytes.len() as f64 / 1_048_576.0
        ));
        let out = ssh_feed(
            target,
            &opts.ssh_options,
            &sh_c(&InstallPlan::stage_script()),
            bytes,
        )
        .await
        .context("staging the binary")?;
        Some(
            deploy::kv_value(&out, "staged")
                .ok_or_else(|| {
                    anyhow!("{target}: the stage step did not say where it put the binary")
                })?
                .to_string(),
        )
    };
    pb.set_message(match plan.scope {
        Scope::System => "installing: service account, unit, firewall",
        Scope::User => "installing: user service",
    });
    let out = ssh_feed(
        target,
        &opts.ssh_options,
        &sh_s,
        plan.install_script(staged.as_deref()).into_bytes(),
    )
    .await
    .context("installing")?;
    if deploy::kv_value(&out, "installed") != Some("ok") {
        bail!("{target}: the install step did not finish: {}", out.trim());
    }
    if deploy::kv_value(&out, "enabled") == Some("no") {
        // Not fatal — the node still joins and runs. It just will not be
        // there after the machine reboots, which is worth one line now
        // rather than a mystery in a month.
        eprintln!(
            "{target}: the service could not be enabled, so it will not start after a reboot \
             (`systemctl enable meshnode` there fixes it)"
        );
    }

    // Start the joiner and learn the port it bound.
    pb.set_message("pairing");
    let join_cmd = elevate.wrap(
        plan.run_as(),
        &plan.join_command(&opts.tags, PAIR_TIMEOUT.as_secs(), None),
    );
    let mut joiner = Joiner::start(target, &opts.ssh_options, &join_cmd).await?;
    let (port, announced_name) = match joiner.next_frame(LISTEN_TIMEOUT).await {
        Ok(JoinerFrame::Listening {
            port, node_name, ..
        }) => (port, node_name),
        Ok(JoinerFrame::Error { message }) => bail!("{target}: {message}"),
        Ok(other) => bail!("{target}: unexpected first frame from the joiner: {other:?}"),
        Err(e) => {
            let err = joiner.stderr_tail().await;
            if err.is_empty() {
                return Err(e);
            }
            bail!("{target}: the remote joiner failed to start: {err}");
        }
    };

    // Where *this* node dials it. The SSH hostname is the one address known
    // to reach the machine — it is how we are talking to it — so it leads,
    // and `--via` overrides for the split-horizon case where the SSH name
    // and the data-plane address are different boxes' idea of the same host.
    let host = match &opts.via {
        Some(v) => v.clone(),
        None => deploy::Target::parse(target)?.host,
    };
    let addr = deploy::dial_addr(&host, port);

    // Mint a single-use invite for provenance, exactly as `invite approve`
    // does — built and signed here, because only this machine holds the
    // admin's user key (MASTERPLAN §7.1).
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs,
    } = ctx.ipc(IpcRequest::InviteBootstrap).await?
    else {
        bail!("unexpected response to InviteBootstrap");
    };
    let params = mesh_core::membership::InviteParams {
        ttl_secs: 600,
        uses: 1,
        tags: opts.tags.clone(),
        bootstrap: addrs,
    };
    let (token, public) =
        mesh_core::membership::build_invite(mesh_id, &ctx.user, &params, node_id)?;
    ctx.ipc(IpcRequest::InvitePublish { public }).await?;

    let mut stream = ctx
        .stream(IpcRequest::PairApprove {
            addr: addr.clone(),
            token: token.encode(),
        })
        .await?;

    // Read the joiner's code and our own daemon's, then compare.
    let joiner_code = match joiner.next_frame(PAIR_TIMEOUT).await? {
        JoinerFrame::Code { code, .. } => code,
        JoinerFrame::Error { message } => bail!("{target}: {message}"),
        other => bail!("{target}: expected a code frame, got {other:?}"),
    };

    let mut session = None;
    let mut our_code = None;
    while our_code.is_none() {
        let frame = tokio::time::timeout(PAIR_TIMEOUT, mesh_proto::read_frame(&mut stream))
            .await
            .map_err(|_| anyhow!("{target}: this node's daemon never produced a pairing code"))?
            .context("pairing stream")?;
        match frame {
            IpcResponse::PairCode {
                session: s,
                code,
                joiner_name,
                ..
            } => {
                // The joiner named itself twice — once over SSH before the
                // exchange, once inside it. They should agree, and a
                // disagreement means the endpoint we dialled is not the
                // machine we installed on.
                if joiner_name != announced_name {
                    let _ = joiner.answer(false).await;
                    joiner.abandon().await;
                    bail!(
                        "{target}: the machine at {addr} calls itself {joiner_name:?}, but the \
                         one reached over SSH said {announced_name:?}. Nothing was joined"
                    );
                }
                session = Some(s);
                our_code = Some(code);
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                let _ = joiner.answer(false).await;
                joiner.abandon().await;
                bail!("{target}: {message}");
            }
            _ => {}
        }
    }
    let our_code = our_code.expect("loop exits with a code");
    let session = session.expect("a code frame carries its session");

    // The comparison. Both sides answer honestly either way — a decline has
    // to reach the joiner too, or it sits there until its own timeout.
    let matched = our_code == joiner_code;
    pb.set_message(format!(
        "code {} · {}",
        mesh_core::pairing::format_code(&our_code),
        if matched {
            "matched, joining"
        } else {
            "MISMATCH"
        }
    ));
    joiner.answer(matched).await.ok();
    let _ = ctx
        .ipc(IpcRequest::PairDecide {
            session,
            accept: matched,
        })
        .await;
    if !matched {
        joiner.abandon().await;
        // Not a retry. Two independently-derived codes disagreeing is the
        // single outcome this exchange exists to detect, and treating it as
        // a transient failure would throw away the whole security argument.
        bail!(
            "{target}: PAIRING CODES DO NOT MATCH — this node derived {our_code}, {target} \
             derived {joiner_code}. Somebody is between them. Nothing was joined; do not \
             re-run until you know why"
        );
    }

    // Both sides confirmed: wait for the membership to land.
    let (node_id, node_name) = loop {
        let frame = tokio::time::timeout(PAIR_TIMEOUT, mesh_proto::read_frame(&mut stream))
            .await
            .map_err(|_| anyhow!("{target}: pairing stalled after confirmation"))?
            .context("pairing stream")?;
        match frame {
            IpcResponse::PairDone { node_id, node_name } => {
                break (node_id.to_display_string(), node_name);
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                bail!("{target}: {message}")
            }
            _ => {}
        }
    };
    // Let the joiner write its identity and exit before the service starts
    // on the same data directory.
    let _ = tokio::time::timeout(Duration::from_secs(20), joiner.child.wait()).await;

    // Start it, and say what is keeping it up.
    pb.set_message("starting the service");
    let out = ssh_feed(
        target,
        &opts.ssh_options,
        &sh_s,
        plan.start_script().into_bytes(),
    )
    .await
    .context("starting the service")?;
    let service = deploy::kv_value(&out, "started")
        .unwrap_or("none")
        .to_string();
    let note = match (note, deploy::kv_value(&out, "linger")) {
        (Some(n), Some("on")) => Some(n.replace(
            " (stops at logout unless `loginctl enable-linger` is on)",
            "; lingering is on, so it survives logout",
        )),
        (n, _) => n,
    };

    Ok(Adopted {
        node_name,
        node_id,
        code: our_code,
        already_installed,
        scope,
        service,
        note,
    })
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Adopt every target, `parallel` at a time.
pub async fn run(ctx: &crate::Ctx, opts: Options) -> Result<Vec<Outcome>> {
    let targets = deploy::expand_all(&opts.targets)?;
    if targets.is_empty() {
        bail!("no targets");
    }

    let opts = Arc::new(opts);
    let mut outcomes = Vec::with_capacity(targets.len());

    // One line per host, live, on stderr — and only on a terminal: piped
    // or logged, indicatif draws nothing, and the summary table at the end
    // is the record. Every host gets its line up front, so a rack of ten
    // shows ten lines with "waiting" on eight of them rather than lines
    // appearing as their turn comes.
    let bars = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
    let width = targets.iter().map(|t| t.len()).max().unwrap_or(8).min(40);
    let style = ProgressStyle::with_template("{spinner:.cyan} {prefix} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner());
    let lines: Vec<ProgressBar> = targets
        .iter()
        .map(|t| {
            let pb = bars.add(ProgressBar::new_spinner());
            pb.set_style(style.clone());
            pb.set_prefix(format!("{t:<width$}"));
            pb.set_message("waiting");
            pb
        })
        .collect();

    // Bounded, and the bound matters: each host holds an SSH connection, a
    // remote process and a QUIC pairing session open at once, and a rack of
    // fifty adopted flat-out is fifty of each against one daemon.
    for (chunk, pbs) in targets
        .chunks(opts.parallel.max(1))
        .zip(lines.chunks(opts.parallel.max(1)))
    {
        let mut running = Vec::new();
        for (target, pb) in chunk.iter().zip(pbs) {
            running.push(async {
                pb.enable_steady_tick(Duration::from_millis(100));
                let result = adopt_one(ctx, &opts, target, pb).await;
                match &result {
                    Ok(a) => pb.finish_with_message(format!("adopted · {}", a.service)),
                    Err(e) => pb.abandon_with_message(format!("failed: {e}")),
                }
                Outcome {
                    target: target.clone(),
                    result,
                }
            });
        }
        outcomes.extend(futures::future::join_all(running).await);
    }
    let _ = bars.clear();
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux(arch: &str, privilege: Privilege) -> Probe {
        Probe {
            os: "Linux".into(),
            arch: arch.into(),
            user: "ops".into(),
            home: "/home/ops".into(),
            privilege: Some(privilege),
            ..Probe::default()
        }
    }

    fn opts() -> Options {
        Options {
            targets: vec![],
            parallel: 1,
            binary: None,
            install_path: None,
            sudo: false,
            no_sudo: false,
            fleet_upgrade: true,
            open_firewall: true,
            tags: vec![],
            ssh_options: vec![],
            via: None,
            dry_run: false,
            force: false,
        }
    }

    #[test]
    fn a_foreign_architecture_asks_for_a_binary_rather_than_guessing() {
        let other = if std::env::consts::ARCH == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        let e = binary_for(&linux(other, Privilege::Root), None).expect_err("cross-arch");
        assert!(e.to_string().contains("--binary"), "{e}");
    }

    #[test]
    fn a_non_linux_host_is_refused_with_the_reason() {
        let mut p = linux("arm64", Privilege::Root);
        p.os = "Darwin".into();
        let e = binary_for(&p, None).expect_err("macOS does not execute");
        assert!(e.to_string().contains("Linux"), "{e}");
    }

    #[test]
    fn root_or_passwordless_sudo_gets_the_server_shape() {
        let (plan, elev, note) =
            plan_for(&opts(), &linux("x86_64", Privilege::Root)).expect("plan");
        assert_eq!(plan.scope, Scope::System);
        assert_eq!(elev, Privilege::Root);
        assert!(note.is_none());
        assert!(plan.open_firewall);

        let (plan, elev, _) =
            plan_for(&opts(), &linux("x86_64", Privilege::SudoNoPassword)).expect("plan");
        assert_eq!(plan.scope, Scope::System);
        assert_eq!(elev, Privilege::SudoNoPassword);
    }

    #[test]
    fn sudo_that_wants_a_password_falls_back_to_the_home_and_says_so() {
        let (plan, elev, note) =
            plan_for(&opts(), &linux("x86_64", Privilege::SudoPassword)).expect("plan");
        assert_eq!(plan.scope, Scope::User);
        assert_eq!(elev, Privilege::None);
        assert_eq!(plan.install_path, "/home/ops/.local/bin/meshnode");
        assert!(!plan.open_firewall, "no root, no firewall rule");
        assert!(note.expect("a note").contains("password"));

        // …unless --sudo said root was the point.
        let mut o = opts();
        o.sudo = true;
        let e = plan_for(&o, &linux("x86_64", Privilege::SudoPassword)).expect_err("refused");
        assert!(e.to_string().contains("Director"), "{e}");
    }

    #[test]
    fn no_sudo_never_escalates() {
        let mut o = opts();
        o.no_sudo = true;
        let (plan, elev, _) =
            plan_for(&o, &linux("x86_64", Privilege::SudoNoPassword)).expect("plan");
        assert_eq!(plan.scope, Scope::User);
        assert_eq!(elev, Privilege::None);
    }

    #[test]
    fn inline_scripts_run_under_sh_whatever_the_login_shell_is() {
        assert_eq!(sh_c("echo 'hi'"), "sh -c 'echo '\\''hi'\\'''");
    }

    #[test]
    fn joiner_frames_parse() {
        let listening: JoinerFrame = serde_json::from_str(
            r#"{"event":"listening","port":41234,"addr":"0.0.0.0:41234","node_name":"rack-1"}"#,
        )
        .expect("listening");
        assert!(matches!(
            listening,
            JoinerFrame::Listening { port: 41234, .. }
        ));

        let code: JoinerFrame = serde_json::from_str(
            r#"{"event":"code","code":"421733","words":"harbor lantern","mesh_name":"home"}"#,
        )
        .expect("code");
        match code {
            JoinerFrame::Code { code, .. } => assert_eq!(code, "421733"),
            other => panic!("{other:?}"),
        }

        let err: JoinerFrame =
            serde_json::from_str(r#"{"event":"error","message":"nope"}"#).expect("error");
        assert!(matches!(err, JoinerFrame::Error { .. }));
    }
}
