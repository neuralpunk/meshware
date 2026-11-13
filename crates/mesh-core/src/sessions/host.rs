//! The host end of a session (M17 §3.5–§3.8).
//!
//! Authorization has already happened by the time anything here runs — the
//! daemon re-verified the initiator's envelope against its own materialized
//! state and checked the reserved tags (§3.4, §3.9). What is left is doing
//! the thing, and doing it with the same conventions a job gets: a cleared
//! environment plus the six inherited variables, its own process group, and
//! a kill that takes the whole tree.
//!
//! Every kind is the same three-part shape — a frame reader, a payload
//! writer, and something in the middle — so the interesting differences are
//! small and local:
//!
//! | Kind | Middle | Terminates on |
//! |---|---|---|
//! | `Exec` | piped child | child exit |
//! | `Shell` | PTY + child | child exit |
//! | `CpTo` | a temp file, renamed on a matching digest | `Eof` |
//! | `CpFrom` | a file read | end of file |
//! | `Forward` | a TCP connection | either half closing |

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use mesh_proto::{
    MAX_SESSION_FRAME, SESSION_CHUNK, STREAM_STDERR, STREAM_STDOUT, SessionFrame, SessionKind,
    SessionServerFrame, SessionSignal, read_frame, write_frame,
};

use super::{Ending, Guard};

/// How long a `Forward` waits for the far port to answer. A tunnel that
/// hangs on connect is indistinguishable from a broken mesh, and the
/// difference is the whole diagnosis.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest file `CpTo` will accept, and `CpFrom` will offer.
///
/// Not a content limit — 8 GiB of image is a legitimate thing to copy — but
/// a declared one, so a host can refuse *before* it starts writing rather
/// than when the disk fills. `cp` streams and does not ingest (§3.7), so
/// nothing here is chunked, hashed into the bundle store, or kept.
pub const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// What the host needs to know that is not in the [`SessionKind`].
#[derive(Debug, Clone)]
pub struct Opts {
    /// Where to write a transcript, when `[sessions] record` is on. `None`
    /// is the default and means the session leaves only its audit entry.
    pub record_dir: Option<PathBuf>,
    /// The name of this node, for messages the initiator reads.
    pub node_name: String,
    /// Which binary to re-exec as the PTY shim; `None` is `current_exe()`
    /// (see [`super::pty_shim`]).
    pub shim: Option<PathBuf>,
    /// For a [`SessionKind::Instance`]: what to spawn instead of a login
    /// shell, resolved by the daemon — which alone knows this node's
    /// container engine and which instances it runs — after authorizing the
    /// session. `None` for every other kind.
    pub instance: Option<InstanceTarget>,
    /// What a session may not reach (SEC-8). `None` only where the operator
    /// turned sandboxing off.
    pub confine: Option<Confine>,
}

/// The boundary a terminal, an `exec` and a copy are held to (SEC-8).
///
/// They used to run as the daemon with no ruleset at all, and `cp` read any
/// path — so granting somebody a terminal handed them the node's identity
/// and every secret sealed to it. They still run as the daemon's account;
/// what they cannot reach is the node's own directory and the account's
/// credential stores, which is what a job cannot reach either.
#[derive(Debug, Clone)]
pub struct Confine {
    /// Never reachable: the data directory, `~/.ssh` and its kin.
    pub hidden: Vec<PathBuf>,
    /// The sandbox shim to start a shell or a command under — or why there
    /// is none. `Ok(None)` is a node that degrades (`sandbox = "auto"` where
    /// Landlock or the shim is missing); `Err` is one that refuses
    /// (`sandbox = "on"`).
    pub shim: Result<Option<PathBuf>, String>,
}

impl Confine {
    fn rules(&self) -> crate::sandbox::Rules {
        let reachable = crate::sandbox::everything_but(&self.hidden);
        crate::sandbox::Rules {
            read: reachable.clone(),
            write: reachable,
        }
    }

    /// `program args…`, as the command line that runs it confined.
    fn wrap(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<(PathBuf, Vec<std::ffi::OsString>), String> {
        match &self.shim {
            Ok(Some(shim)) => Ok((shim.clone(), self.rules().argv(program, args))),
            Ok(None) => Ok((
                PathBuf::from(program),
                args.iter().map(std::ffi::OsString::from).collect(),
            )),
            Err(why) => Err(why.clone()),
        }
    }

    /// Is `path` — resolved — one of the hidden ones, or inside one?
    fn hides(&self, path: &Path) -> bool {
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.hidden.iter().any(|h| {
            let h = std::fs::canonicalize(h).unwrap_or_else(|_| h.clone());
            real.starts_with(&h)
        })
    }
}

/// Run `f` on a thread of its own, under the session's Landlock ruleset
/// (SEC-8). File work for a copy happens here — opening, renaming,
/// removing — so a directory swapped for a symlink between a check and a
/// use reaches nothing the ruleset hides. A pool thread is never confined:
/// the restriction outlives the call.
async fn confined<T: Send + 'static>(
    confine: &Option<Confine>,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let rules = confine.as_ref().map(Confine::rules);
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        if let Some(rules) = rules {
            crate::sandbox::confine_this_thread(&rules.read, &rules.write);
        }
        let _ = tx.send(f());
    });
    rx.await.expect("the confined thread ends with an answer")
}

/// The container `exec` an instance session runs (Phase B, E3).
#[derive(Debug, Clone)]
pub struct InstanceTarget {
    /// The engine binary: `podman` or `docker`.
    pub program: String,
    /// What every invocation of it starts with: `--remote --url …` for a
    /// podman service, nothing run directly (`Engine::global_args`).
    pub global_args: Vec<String>,
    /// `meshware-<job>`.
    pub container: String,
    /// What the engine needs beyond a session's own environment —
    /// `XDG_RUNTIME_DIR` for rootless podman above all, which a session's
    /// cleared environment would otherwise strip.
    pub env: Vec<(String, String)>,
}

/// Run one authorized session to its end.
///
/// Writes [`SessionServerFrame::Opened`] first, so a client can tell
/// "connected and quiet" from "still connecting" — for a port-forward
/// listener that is the difference between holding a TCP connection open
/// and closing it.
pub async fn run<R, W>(kind: SessionKind, reader: R, writer: W, guard: &Guard, opts: Opts) -> Ending
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // One task owns the write half and drains a channel, so every producer
    // — the child's stdout, its stderr, an error path — can emit frames
    // without sharing a lock on the stream.
    let (tx, rx) = mpsc::channel::<SessionServerFrame>(64);
    let pump = tokio::spawn(write_pump(writer, rx));

    let ending = match &kind {
        SessionKind::Shell { .. } | SessionKind::Exec { .. } | SessionKind::Instance { .. } => {
            spawned(kind, reader, tx.clone(), guard, &opts).await
        }
        SessionKind::CpTo { path, mode, size } => {
            cp_to(
                path.clone(),
                *mode,
                *size,
                reader,
                tx.clone(),
                guard,
                &opts.confine,
            )
            .await
        }
        SessionKind::CpFrom { path } => {
            cp_from(path.clone(), reader, tx.clone(), guard, &opts.confine).await
        }
        SessionKind::Forward { host, port } => {
            forward(host.clone(), *port, reader, tx.clone(), guard).await
        }
    };

    // A refusal or a failure is the last thing the initiator hears, and it
    // has to arrive before the stream closes or it reads as a hang.
    match &ending {
        Ending::Failed { message } => {
            let _ = tx
                .send(SessionServerFrame::Error {
                    message: message.clone(),
                })
                .await;
        }
        Ending::Evicted { reason, .. } => {
            let _ = tx
                .send(SessionServerFrame::Error {
                    message: format!("{}: {reason}", opts.node_name),
                })
                .await;
        }
        Ending::Exited { .. } | Ending::Closed => {}
    }
    drop(tx);
    let _ = pump.await;
    ending
}

async fn write_pump<W>(mut writer: W, mut rx: mpsc::Receiver<SessionServerFrame>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if write_frame(&mut writer, &frame).await.is_err() {
            return;
        }
    }
    let _ = writer.flush().await;
}

// ---------------------------------------------------------------------------
// Shell and exec
// ---------------------------------------------------------------------------

/// The six variables a job inherits, and the only ones (`jobs.rs`).
const INHERITED: [&str; 5] = ["PATH", "HOME", "LANG", "LC_ALL", "USER"];

/// What a `Shell` runs when the initiator named no command.
fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

async fn spawned<R>(
    kind: SessionKind,
    reader: R,
    tx: mpsc::Sender<SessionServerFrame>,
    guard: &Guard,
    opts: &Opts,
) -> Ending
where
    R: AsyncRead + Unpin + Send + 'static,
{
    match kind {
        SessionKind::Shell {
            command,
            term,
            cols,
            rows,
        } => pty_session(command, term, cols, rows, reader, tx, guard, opts).await,
        SessionKind::Exec { command } => exec_session(command, reader, tx, guard, opts).await,
        // Phase B, E3. The same PTY, the same shim, the same teardown — the
        // only difference is the line it runs: this node's own engine
        // `exec`ing into the container, with the engine's environment put
        // back (a session's is cleared, and rootless podman cannot find its
        // runtime directory without `XDG_RUNTIME_DIR`).
        SessionKind::Instance {
            command,
            term,
            cols,
            rows,
            ..
        } => {
            // Resolved by the daemon from the same state it authorized
            // against; absent means that check no longer holds.
            let Some(target) = opts.instance.clone() else {
                return Ending::Failed {
                    message: format!(
                        "that instance is not a running container on {}",
                        opts.node_name
                    ),
                };
            };
            let inner = match &command {
                Some(c) => format!("sh -c {}", sh_quote(c)),
                None => "sh".to_string(),
            };
            let env: String = target
                .env
                .iter()
                .map(|(k, v)| format!("{k}={} ", sh_quote(v)))
                .collect();
            let global: String = target
                .global_args
                .iter()
                .map(|a| format!("{} ", sh_quote(a)))
                .collect();
            let line = format!(
                "{env}exec {} {global}exec -it {} {inner}",
                sh_quote(&target.program),
                sh_quote(&target.container)
            );
            pty_session(Some(line), term, cols, rows, reader, tx, guard, opts).await
        }
        _ => Ending::Failed {
            message: "not a spawning session".into(),
        },
    }
}

/// One argument for `bash -lc`, single-quoted so nothing in it is expanded.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Base environment for a session's child: cleared, plus the six a job
/// inherits. A session is not a job, but the *environment contract* is the
/// same one, and two answers to "what does my command see" would be one
/// too many.
fn base_env(cmd: &mut tokio::process::Command, term: Option<&str>) {
    cmd.env_clear();
    for var in INHERITED {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    // `TERM` comes from the *client's* terminal, not this machine's, so
    // curses programs agree with the thing actually rendering them.
    match term {
        Some(t) if !t.trim().is_empty() => {
            cmd.env("TERM", t);
        }
        _ => {
            if let Ok(v) = std::env::var("TERM") {
                cmd.env("TERM", v);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.current_dir(home);
    }
}

async fn exec_session<R>(
    command: String,
    mut reader: R,
    tx: mpsc::Sender<SessionServerFrame>,
    guard: &Guard,
    opts: &Opts,
) -> Ending
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let args = vec!["-lc".to_string(), command.clone()];
    let (program, argv) = match &opts.confine {
        Some(confine) => match confine.wrap("bash", &args) {
            Ok(wrapped) => wrapped,
            Err(message) => return Ending::Failed { message },
        },
        None => (
            PathBuf::from("bash"),
            args.iter().map(std::ffi::OsString::from).collect(),
        ),
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(argv);
    base_env(&mut cmd, None);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        // A session does not outlive the task hosting it — a daemon shutting
        // down included (SEC-8).
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ending::Failed {
                message: format!("cannot start a shell to run that: {e}"),
            };
        }
    };
    let pid = child.id().unwrap_or(0);
    let _ = tx.send(SessionServerFrame::Opened).await;

    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    let out_tx = tx.clone();
    let out = tokio::spawn(async move {
        let mut total = 0usize;
        if let Some(mut s) = stdout.take() {
            let mut buf = vec![0u8; SESSION_CHUNK];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        total += n;
                        if out_tx
                            .send(SessionServerFrame::Data {
                                stream: STREAM_STDOUT,
                                bytes: buf[..n].to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
        total
    });
    let err_tx = tx.clone();
    let err = tokio::spawn(async move {
        let mut total = 0usize;
        if let Some(mut s) = stderr.take() {
            let mut buf = vec![0u8; SESSION_CHUNK];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        total += n;
                        if err_tx
                            .send(SessionServerFrame::Data {
                                stream: STREAM_STDERR,
                                bytes: buf[..n].to_vec(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
        total
    });

    let ending = loop {
        tokio::select! {
            frame = read_frame::<_, SessionFrame>(&mut reader) => {
                match frame {
                    Ok(SessionFrame::Data(bytes)) => {
                        guard.count_in(bytes.len());
                        if let Some(s) = stdin.as_mut()
                            && s.write_all(&bytes).await.is_err() {
                                stdin = None;
                            }
                    }
                    Ok(SessionFrame::Eof) => {
                        // Dropping the handle closes the pipe, which is how
                        // `tar -xf -` on the far end learns to finish.
                        stdin = None;
                    }
                    Ok(SessionFrame::Signal(sig)) => signal_group(pid, sig),
                    // A resize means nothing without a terminal, and
                    // pretending otherwise would make `exec` output depend
                    // on the client's window — which is exactly what `exec`
                    // exists to avoid.
                    Ok(SessionFrame::Resize { .. }) | Ok(SessionFrame::Digest(_)) => {}
                    Err(_) => {
                        // The initiator is gone. Kill the group rather than
                        // leaving an orphan holding the machine's CPU.
                        signal_group(pid, SessionSignal::Term);
                        break Ending::Closed;
                    }
                }
            }
            status = child.wait() => break exit_of(status),
            end = guard.closed() => {
                signal_group(pid, SessionSignal::Term);
                break Ending::Evicted { end, reason: closing_reason(end, guard) };
            }
        }
    };

    if !matches!(ending, Ending::Exited { .. }) {
        make_sure_it_ends(&mut child, pid).await;
    }
    // Drain what the child already wrote before reporting the exit: a
    // command whose last line arrives after its exit code has lost the
    // line, and that line is usually the answer.
    let produced = out.await.unwrap_or(0) + err.await.unwrap_or(0);
    guard.count_out(produced);
    if let Ending::Exited { code, signal } = ending {
        let _ = tx.send(SessionServerFrame::Exit { code, signal }).await;
    }
    ending
}

#[allow(clippy::too_many_arguments)]
async fn pty_session<R>(
    command: Option<String>,
    term: String,
    cols: u16,
    rows: u16,
    mut reader: R,
    tx: mpsc::Sender<SessionServerFrame>,
    guard: &Guard,
    opts: &Opts,
) -> Ending
where
    R: AsyncRead + Unpin + Send + 'static,
{
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use rustix::termios::{Winsize, tcsetwinsize};

    let master = match openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC) {
        Ok(fd) => fd,
        Err(e) => {
            return Ending::Failed {
                message: format!("no pseudoterminal available on {}: {e}", opts.node_name),
            };
        }
    };
    if let Err(e) = grantpt(&master).and_then(|()| unlockpt(&master)) {
        return Ending::Failed {
            message: format!("cannot prepare a terminal on {}: {e}", opts.node_name),
        };
    }
    let slave_path = match ptsname(&master, Vec::new()) {
        Ok(name) => {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(name.into_bytes()))
        }
        Err(e) => {
            return Ending::Failed {
                message: format!("cannot name the terminal on {}: {e}", opts.node_name),
            };
        }
    };
    let _ = tcsetwinsize(
        &master,
        Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    );

    // The slave becomes the child's stdin/stdout/stderr through ordinary
    // `Stdio`. Nothing here is unsafe; the shim does the two calls that
    // would otherwise have had to happen inside a fork (see `pty_shim`).
    let slave = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&slave_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Ending::Failed {
                message: format!("cannot open the terminal on {}: {e}", opts.node_name),
            };
        }
    };
    let (slave_out, slave_err) = match (slave.try_clone(), slave.try_clone()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => {
            return Ending::Failed {
                message: "cannot duplicate the terminal".into(),
            };
        }
    };

    let exe = match opts
        .shim
        .clone()
        .or_else(|| std::env::var_os(super::pty_shim::SHIM_ENV).map(PathBuf::from))
    {
        Some(path) => path,
        // The running image, even after an in-place upgrade (ENG-10).
        None => match crate::sandbox::self_exe() {
            Some(p) => p,
            None => {
                return Ending::Failed {
                    message: "cannot locate this binary to start a terminal".into(),
                };
            }
        },
    };
    let (program, args) = match &command {
        Some(c) => ("bash".to_string(), vec!["-lc".to_string(), c.clone()]),
        None => (login_shell(), vec!["-l".to_string()]),
    };
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg(super::pty_shim::SHIM_ARG);
    // The sandbox shim sits between the PTY shim and the shell (SEC-8) —
    // except for a container's terminal, whose engine client a ruleset
    // would only break, and whose boundary is the container's.
    match opts.confine.as_ref().filter(|_| opts.instance.is_none()) {
        Some(confine) => match confine.wrap(&program, &args) {
            Ok((wrapper, argv)) => {
                cmd.arg(wrapper).args(argv);
            }
            Err(message) => return Ending::Failed { message },
        },
        None => {
            cmd.arg(program).args(args);
        }
    }
    base_env(&mut cmd, Some(&term));
    cmd.stdin(Stdio::from(slave))
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err))
        .kill_on_drop(true);
    // Deliberately **not** `process_group(0)`: the shim calls `setsid`,
    // which fails if the caller is already a process-group leader. The
    // group `kill_process_group` signals is the one `setsid` makes, and its
    // id is the shim's pid either way.

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ending::Failed {
                message: format!("cannot start a terminal on {}: {e}", opts.node_name),
            };
        }
    };
    let pid = child.id().unwrap_or(0);
    let _ = tx.send(SessionServerFrame::Opened).await;

    // Non-blocking, because `AsyncFd` polls readiness and then expects the
    // read itself not to block.
    if let Err(e) = rustix::fs::fcntl_setfl(
        &master,
        rustix::fs::fcntl_getfl(&master).unwrap_or(rustix::fs::OFlags::RDWR)
            | rustix::fs::OFlags::NONBLOCK,
    ) {
        tracing::debug!("pty master non-blocking failed: {e}");
    }
    let master = match tokio::io::unix::AsyncFd::new(master) {
        Ok(fd) => std::sync::Arc::new(fd),
        Err(e) => {
            let _ = child.kill().await;
            return Ending::Failed {
                message: format!("cannot watch the terminal: {e}"),
            };
        }
    };

    let mut recorder = Recorder::open(opts.record_dir.as_deref());

    // Master → initiator. A PTY has one stream: stderr on the far side is
    // the same file description as stdout, and claiming otherwise would be
    // a fiction the terminal cannot honour.
    let out_tx = tx.clone();
    let out_fd = master.clone();
    let mut out_recorder = recorder.take_output();
    let out = tokio::spawn(async move {
        let mut total = 0usize;
        loop {
            let mut ready = match out_fd.readable().await {
                Ok(r) => r,
                Err(_) => break,
            };
            let mut buf = vec![0u8; SESSION_CHUNK];
            match ready
                .try_io(|fd| rustix::io::read(fd.get_ref(), &mut buf).map_err(std::io::Error::from))
            {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    total += n;
                    if let Some(w) = out_recorder.as_mut() {
                        let _ = std::io::Write::write_all(w, &buf[..n]);
                    }
                    if out_tx
                        .send(SessionServerFrame::Data {
                            stream: STREAM_STDOUT,
                            bytes: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // Spurious readiness: clear it and go round again.
                Err(_would_block) => continue,
            }
        }
        total
    });

    let ending = loop {
        tokio::select! {
            frame = read_frame::<_, SessionFrame>(&mut reader) => {
                match frame {
                    Ok(SessionFrame::Data(bytes)) => {
                        guard.count_in(bytes.len());
                        recorder.input(&bytes);
                        if write_all_fd(&master, &bytes).await.is_err() {
                            break Ending::Closed;
                        }
                    }
                    Ok(SessionFrame::Resize { cols, rows }) => {
                        let _ = tcsetwinsize(
                            master.get_ref(),
                            Winsize {
                                ws_row: rows.max(1),
                                ws_col: cols.max(1),
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            },
                        );
                    }
                    Ok(SessionFrame::Signal(sig)) => signal_group(pid, sig),
                    // A terminal has no half-close: the far end learns the
                    // session is over from the stream closing, and `exit`
                    // is how a human says it.
                    Ok(SessionFrame::Eof) | Ok(SessionFrame::Digest(_)) => {}
                    Err(_) => {
                        signal_group(pid, SessionSignal::Term);
                        break Ending::Closed;
                    }
                }
            }
            status = child.wait() => break exit_of(status),
            end = guard.closed() => {
                signal_group(pid, SessionSignal::Term);
                break Ending::Evicted { end, reason: closing_reason(end, guard) };
            }
        }
    };

    if !matches!(ending, Ending::Exited { .. }) {
        make_sure_it_ends(&mut child, pid).await;
    }
    // Give the shell's last output — the "logout", the final prompt — a
    // moment to cross before the exit frame, then stop waiting regardless:
    // a session that will not close is worse than one that loses a line.
    let produced = match tokio::time::timeout(Duration::from_millis(250), out).await {
        Ok(Ok(n)) => n,
        _ => 0,
    };
    guard.count_out(produced);
    if let Ending::Exited { code, signal } = ending {
        let _ = tx.send(SessionServerFrame::Exit { code, signal }).await;
    }
    ending
}

/// Write every byte to a non-blocking fd through `AsyncFd`.
async fn write_all_fd(
    fd: &tokio::io::unix::AsyncFd<rustix::fd::OwnedFd>,
    mut bytes: &[u8],
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let mut ready = fd.writable().await?;
        match ready
            .try_io(|fd| rustix::io::write(fd.get_ref(), bytes).map_err(std::io::Error::from))
        {
            Ok(Ok(0)) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(Ok(n)) => bytes = &bytes[n..],
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

/// The sentence the operator sees when the host ends their session.
///
/// A timeout that says only "closed" is a bug report; one that names the
/// setting is a configuration change somebody can make.
fn closing_reason(end: mesh_proto::SessionEnd, guard: &Guard) -> String {
    use mesh_proto::SessionEnd;
    match end {
        SessionEnd::IdleTimeout => {
            "closed after the idle timeout ([sessions] idle_timeout)".to_string()
        }
        SessionEnd::MaxDuration => {
            "closed at the maximum session duration ([sessions] max_duration)".to_string()
        }
        _ => guard.eviction_reason(),
    }
}

fn exit_of(status: std::io::Result<std::process::ExitStatus>) -> Ending {
    match status {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            Ending::Exited {
                code: status.code(),
                signal: status.signal(),
            }
        }
        Err(e) => Ending::Failed {
            message: format!("lost track of the process: {e}"),
        },
    }
}

/// Signal the session's whole process group, the way job cancel does
/// (`jobs.rs`) — a shell's children are the point of a shell.
/// How long a session's processes get to leave after being asked.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// End a session's process tree after it was told to go (SEC-8).
///
/// The group was sent `TERM` and nothing followed, so a shell that
/// ignored it (`trap '' TERM`) outlived its idle timeout, its maximum
/// duration, an operator closing it and a drain — running on as the daemon
/// with nobody attached. After a grace period the group gets `KILL`, and
/// so does the child itself, in case it never became a group leader.
async fn make_sure_it_ends(child: &mut tokio::process::Child, pid: u32) {
    if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_ok() {
        return;
    }
    signal_group(pid, SessionSignal::Kill);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(TERM_GRACE, child.wait()).await;
}

fn signal_group(pid: u32, sig: SessionSignal) {
    let Ok(raw) = i32::try_from(pid) else { return };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return;
    };
    let signal = match sig {
        SessionSignal::Int => rustix::process::Signal::INT,
        SessionSignal::Term => rustix::process::Signal::TERM,
        SessionSignal::Kill => rustix::process::Signal::KILL,
    };
    let _ = rustix::process::kill_process_group(pid, signal);
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

async fn cp_to<R>(
    path: String,
    mode: u32,
    size: u64,
    mut reader: R,
    tx: mpsc::Sender<SessionServerFrame>,
    guard: &Guard,
    confine: &Option<Confine>,
) -> Ending
where
    R: AsyncRead + Unpin + Send + 'static,
{
    if size > MAX_COPY_BYTES {
        return Ending::Failed {
            message: format!("{size} bytes is over this node's {MAX_COPY_BYTES}-byte copy limit"),
        };
    }
    // A destination that is a directory means "into it", which is what
    // every copy tool does and what `meshnode cp x hermes:/tmp/` reads as.
    let target = match resolve_destination(&path) {
        Ok(t) => t,
        Err(message) => return Ending::Failed { message },
    };
    // Written beside the target and renamed, so an interrupted copy never
    // leaves a half-file where a whole one used to be. Same directory, so
    // the rename is atomic rather than a cross-device move.
    if let Some(c) = confine
        && target.parent().is_some_and(|dir| c.hides(dir))
    {
        return Ending::Failed {
            message: format!(
                "{} is inside this node's own directory or a credential store, which a \
                 session does not reach",
                target.display()
            ),
        };
    }
    let part = partial_path(&target);
    let created = {
        let part = part.clone();
        confined(confine, move || std::fs::File::create(&part)).await
    };
    let mut file = match created {
        Ok(f) => tokio::fs::File::from_std(f),
        Err(e) => {
            return Ending::Failed {
                message: format!("cannot write {}: {e}", target.display()),
            };
        }
    };
    let _ = tx.send(SessionServerFrame::Opened).await;

    let mut hasher = blake3::Hasher::new();
    let mut written: u64 = 0;
    let mut claimed: Option<[u8; 32]> = None;

    let ending = loop {
        tokio::select! {
            frame = read_frame::<_, SessionFrame>(&mut reader) => {
                match frame {
                    Ok(SessionFrame::Data(bytes)) => {
                        written += bytes.len() as u64;
                        if written > MAX_COPY_BYTES {
                            break Ending::Failed {
                                message: format!(
                                    "the sender exceeded this node's {MAX_COPY_BYTES}-byte copy limit"
                                ),
                            };
                        }
                        guard.count_in(bytes.len());
                        hasher.update(&bytes);
                        if let Err(e) = file.write_all(&bytes).await {
                            break Ending::Failed {
                                message: format!("cannot write {}: {e}", target.display()),
                            };
                        }
                    }
                    Ok(SessionFrame::Digest(d)) => claimed = Some(d),
                    Ok(SessionFrame::Eof) => break Ending::Closed,
                    Ok(_) => {}
                    Err(_) => {
                        break Ending::Failed {
                            message: "the copy ended before the file did".into(),
                        };
                    }
                }
            }
            end = guard.closed() => {
                break Ending::Evicted { end, reason: closing_reason(end, guard) };
            }
        }
    };

    let finish = async {
        file.flush().await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        drop(file);
        // The digest is the whole point of streaming rather than ingesting:
        // nothing content-addressed this file, so this is the only chance
        // either end gets to find out the bytes are the bytes (§3.7).
        let actual = *hasher.finalize().as_bytes();
        match claimed {
            Some(d) if d != actual => {
                return Err("the copy did not survive the wire: digests differ".to_string());
            }
            None => return Err("the sender never claimed a digest".to_string()),
            _ => {}
        }
        if written != size {
            return Err(format!("expected {size} bytes and got {written}"));
        }
        let (from, to) = (part.clone(), target.clone());
        confined(confine, move || {
            set_mode(&from, mode)?;
            std::fs::rename(&from, &to).map_err(|e| format!("cannot place {}: {e}", to.display()))
        })
        .await?;
        Ok::<(), String>(())
    };

    match ending {
        Ending::Closed => match finish.await {
            Ok(()) => {
                let _ = tx
                    .send(SessionServerFrame::Exit {
                        code: Some(0),
                        signal: None,
                    })
                    .await;
                Ending::Exited {
                    code: Some(0),
                    signal: None,
                }
            }
            Err(message) => {
                remove_confined(confine, &part).await;
                Ending::Failed { message }
            }
        },
        other => {
            remove_confined(confine, &part).await;
            other
        }
    }
}

async fn remove_confined(confine: &Option<Confine>, part: &Path) {
    let part = part.to_path_buf();
    confined(confine, move || {
        let _ = std::fs::remove_file(&part);
    })
    .await;
}

async fn cp_from<R>(
    path: String,
    mut reader: R,
    tx: mpsc::Sender<SessionServerFrame>,
    guard: &Guard,
    confine: &Option<Confine>,
) -> Ending
where
    R: AsyncRead + Unpin + Send + 'static,
{
    if confine.as_ref().is_some_and(|c| c.hides(Path::new(&path))) {
        return Ending::Failed {
            message: format!(
                "{path} is inside this node's own directory or a credential store, which a \
                 session does not reach"
            ),
        };
    }
    // Opened under the ruleset, and read through the handle it returns
    // (SEC-8): what was checked is what is read.
    let opened = {
        let path = path.clone();
        confined(confine, move || {
            let file = std::fs::File::open(&path)?;
            let meta = file.metadata()?;
            Ok::<_, std::io::Error>((file, meta))
        })
        .await
    };
    let (file, meta) = match opened {
        Ok(pair) => pair,
        Err(e) => {
            return Ending::Failed {
                message: format!("cannot read {path}: {e}"),
            };
        }
    };
    if meta.is_dir() {
        // Refused with the fix named, which is why recursive `cp` is a
        // convenience rather than a requirement (§3.7).
        return Ending::Failed {
            message: format!(
                "{path} is a directory — `meshnode exec <node> -- tar -cf - {path} | tar -xf -` \
                 copies one"
            ),
        };
    }
    if meta.len() > MAX_COPY_BYTES {
        return Ending::Failed {
            message: format!("{path} is over the {MAX_COPY_BYTES}-byte copy limit"),
        };
    }
    let mut file = tokio::fs::File::from_std(file);
    let _ = tx.send(SessionServerFrame::Opened).await;
    let _ = tx
        .send(SessionServerFrame::Meta {
            mode: mode_of(&meta),
            size: meta.len(),
        })
        .await;

    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; SESSION_CHUNK];
    loop {
        let read = tokio::select! {
            n = file.read(&mut buf) => n,
            end = guard.closed() => {
                return Ending::Evicted { end, reason: guard.eviction_reason() };
            }
            // A `cp` has nothing to say back, but a closed stream still has
            // to end the read rather than filling a channel nobody drains.
            frame = read_frame::<_, SessionFrame>(&mut reader) => {
                match frame {
                    Ok(_) => continue,
                    Err(_) => return Ending::Closed,
                }
            }
        };
        match read {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                guard.count_out(n);
                if tx
                    .send(SessionServerFrame::Data {
                        stream: STREAM_STDOUT,
                        bytes: buf[..n].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    return Ending::Closed;
                }
            }
            Err(e) => {
                return Ending::Failed {
                    message: format!("cannot read {path}: {e}"),
                };
            }
        }
    }
    let _ = tx
        .send(SessionServerFrame::Digest(*hasher.finalize().as_bytes()))
        .await;
    let _ = tx
        .send(SessionServerFrame::Exit {
            code: Some(0),
            signal: None,
        })
        .await;
    Ending::Exited {
        code: Some(0),
        signal: None,
    }
}

fn resolve_destination(path: &str) -> std::result::Result<PathBuf, String> {
    let p = PathBuf::from(path);
    if path.ends_with('/') || p.is_dir() {
        return Err(format!(
            "{path} is a directory — name the file to write, e.g. {}",
            Path::new(path).join("file").display()
        ));
    }
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() && !dir.is_dir() => {
            Err(format!("{} is not a directory", dir.display()))
        }
        _ => Ok(p),
    }
}

fn partial_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".meshware-part");
    target.with_file_name(name)
}

fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

fn set_mode(path: &Path, mode: u32) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    // Only the permission bits: a sender's file *type* bits are not ours to
    // reproduce, and 0 means "whatever the umask says".
    let bits = mode & 0o7777;
    if bits == 0 {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
        .map_err(|e| format!("cannot set mode on {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Forward
// ---------------------------------------------------------------------------

async fn forward<R>(
    host: String,
    port: u16,
    mut reader: R,
    tx: mpsc::Sender<SessionServerFrame>,
    guard: &Guard,
) -> Ending
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let target = format!("{host}:{port}");
    let stream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(&target),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Ending::Failed {
                message: format!("cannot reach {target} from there: {e}"),
            };
        }
        Err(_) => {
            return Ending::Failed {
                message: format!(
                    "cannot reach {target} from there: no answer in {}s",
                    CONNECT_TIMEOUT.as_secs()
                ),
            };
        }
    };
    let _ = stream.set_nodelay(true);
    let (mut tcp_read, mut tcp_write) = stream.into_split();
    let _ = tx.send(SessionServerFrame::Opened).await;

    let out_tx = tx.clone();
    let mut out = tokio::spawn(async move {
        let mut total = 0usize;
        let mut buf = vec![0u8; SESSION_CHUNK];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n;
                    if out_tx
                        .send(SessionServerFrame::Data {
                            stream: STREAM_STDOUT,
                            bytes: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        total
    });

    let ending = loop {
        tokio::select! {
            frame = read_frame::<_, SessionFrame>(&mut reader) => {
                match frame {
                    Ok(SessionFrame::Data(bytes)) => {
                        guard.count_in(bytes.len());
                        if tcp_write.write_all(&bytes).await.is_err() {
                            break Ending::Closed;
                        }
                    }
                    Ok(SessionFrame::Eof) => {
                        let _ = tcp_write.shutdown().await;
                    }
                    Ok(_) => {}
                    Err(_) => break Ending::Closed,
                }
            }
            // The far side hung up. A forward whose upstream closed has to
            // close too, or the local listener holds a dead connection open
            // forever waiting for a reply that is never coming.
            produced = &mut out => {
                guard.count_out(produced.unwrap_or(0));
                break Ending::Exited { code: Some(0), signal: None };
            }
            end = guard.closed() => {
                break Ending::Evicted { end, reason: closing_reason(end, guard) };
            }
        }
    };
    out.abort();
    if matches!(ending, Ending::Exited { .. } | Ending::Closed) {
        let _ = tx
            .send(SessionServerFrame::Exit {
                code: Some(0),
                signal: None,
            })
            .await;
    }
    ending
}

// ---------------------------------------------------------------------------
// Recording (§3.10)
// ---------------------------------------------------------------------------

/// A transcript, when `[sessions] record` is on.
///
/// Off by default and not a substitute for anything: recording a human's
/// terminal is a policy decision an organisation makes, and the *audit
/// entry* is written either way.
struct Recorder {
    input: Option<std::fs::File>,
    output: Option<std::fs::File>,
}

impl Recorder {
    fn open(dir: Option<&Path>) -> Recorder {
        let Some(dir) = dir else {
            return Recorder {
                input: None,
                output: None,
            };
        };
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("session recording disabled: {e}");
            return Recorder {
                input: None,
                output: None,
            };
        }
        Recorder {
            input: std::fs::File::create(dir.join("input")).ok(),
            output: std::fs::File::create(dir.join("output")).ok(),
        }
    }

    fn take_output(&mut self) -> Option<std::fs::File> {
        self.output.take()
    }

    fn input(&mut self, bytes: &[u8]) {
        if let Some(f) = self.input.as_mut() {
            let _ = std::io::Write::write_all(f, bytes);
        }
    }
}

/// Frames larger than the protocol allows are a peer bug or an attack, and
/// either way the answer is the same.
pub fn oversized(len: usize) -> bool {
    len > MAX_SESSION_FRAME
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_lands_beside_its_target_so_the_rename_is_atomic() {
        let p = partial_path(Path::new("/tmp/x/patch.sh"));
        assert_eq!(p, Path::new("/tmp/x/patch.sh.meshware-part"));
        assert_eq!(p.parent(), Path::new("/tmp/x/patch.sh").parent());
    }

    #[test]
    fn a_directory_destination_is_refused_with_the_name_to_use() {
        let e = resolve_destination("/tmp/").expect_err("refused");
        assert!(e.contains("/tmp/file"), "{e}");
    }

    #[test]
    fn a_missing_parent_is_refused_before_anything_is_written() {
        let e = resolve_destination("/nonexistent-dir-xyz/f").expect_err("refused");
        assert!(e.contains("not a directory"), "{e}");
    }

    #[test]
    fn only_permission_bits_are_reproduced() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        // 0o100644 is a regular file with 0644 — the type bits must not
        // reach `set_permissions`.
        set_mode(&f, 0o100_644).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }

    /// Phase B, E3: an instance session's line is run by `bash -lc`, so each
    /// piece of it is one single-quoted word — a command with quotes, `$` or
    /// `;` in it stays one argument and expands nothing.
    #[test]
    fn a_quoted_word_expands_nothing() {
        assert_eq!(sh_quote("sh"), "'sh'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        let word = "it's $HOME; echo not-run";
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("printf %s {}", sh_quote(word)))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), word);
    }
}
