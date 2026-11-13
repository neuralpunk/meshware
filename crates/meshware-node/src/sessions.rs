//! The client end of the fabric (M17): `shell`.
//!
//! M17 v1 ships **the terminal and nothing else**. `exec`, `cp` and
//! `forward` were built, worked, and are deliberately not exposed: a first
//! release whose whole security story is "an authorized operator can get a
//! prompt on an authorized node, and every one is audited" is a story that
//! fits in a sentence. Their session kinds are still on the wire (appended
//! variants keep their indices), and the host refuses them by name, so
//! turning one back on later is deleting a refusal rather than a protocol
//! change.
//!
//! Everything here talks to the local daemon over the Unix socket and then
//! stops treating it as a request/response channel — [`ipc::open_session`]
//! keeps both halves, and from that point the conversation is
//! [`SessionFrame`] out and [`SessionServerFrame`] in. The daemon splices
//! those frames to the host node without interpreting them, so what this
//! module writes is what the host reads.
//!
//! The one genuinely fiddly part is `shell`: a terminal has to be put in
//! raw mode so that Ctrl-C reaches the *far* shell instead of killing this
//! one, and it has to come back out on every exit path — including a panic.
//! [`RawMode`] is a guard type for exactly that reason, and deliberately
//! not a `Drop` on a future, which is not guaranteed to run.

use anyhow::{Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use mesh_core::ipc;
use mesh_proto::{
    STREAM_STDERR, SessionFrame, SessionKind, SessionServerFrame, read_frame, write_frame,
};

use crate::Ctx;

/// Bytes read from local stdin before a frame is sent. Small, because a
/// keystroke that waits for a buffer to fill is a keystroke the user
/// notices.
const STDIN_CHUNK: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Raw mode
// ---------------------------------------------------------------------------

/// Puts the terminal in raw mode and takes it back out when dropped.
///
/// Raw mode is what makes a remote shell a shell: without it the *local*
/// terminal interprets Ctrl-C, echoes what you type, and buffers by line,
/// so the far end sees nothing until you press enter and never sees an
/// interrupt at all.
///
/// Restoring it is not optional and not best-effort. A process that exits
/// without restoring leaves the user's terminal with no echo and no line
/// editing, which reads as a hung machine — so this is a guard on the
/// stack, running on every return path and on unwind.
pub struct RawMode {
    saved: Option<rustix::termios::Termios>,
}

impl RawMode {
    /// Enter raw mode on stdin, or do nothing if stdin is not a terminal
    /// (a pipe, a CI runner) — where there is nothing to make raw and
    /// nothing to restore.
    pub fn enter() -> RawMode {
        let stdin = std::io::stdin();
        let Ok(saved) = rustix::termios::tcgetattr(&stdin) else {
            return RawMode { saved: None };
        };
        let mut raw = saved.clone();
        raw.make_raw();
        match rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw) {
            Ok(()) => RawMode { saved: Some(saved) },
            Err(_) => RawMode { saved: None },
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            let _ = rustix::termios::tcsetattr(
                std::io::stdin(),
                rustix::termios::OptionalActions::Now,
                &saved,
            );
        }
    }
}

/// The terminal's size, or a sane default when there is no terminal.
fn window() -> (u16, u16) {
    match rustix::termios::tcgetwinsize(std::io::stdout()) {
        Ok(w) if w.ws_col > 0 && w.ws_row > 0 => (w.ws_col, w.ws_row),
        _ => (80, 24),
    }
}

fn term_name() -> String {
    std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into())
}

// ---------------------------------------------------------------------------
// shell / exec
// ---------------------------------------------------------------------------

/// `meshnode shell <node> [-- command]` — an interactive PTY on a member.
pub async fn shell(ctx: &Ctx, node: &str, reason: &str, command: Vec<String>) -> Result<i32> {
    let (cols, rows) = window();
    let kind = SessionKind::Shell {
        command: (!command.is_empty()).then(|| command.join(" ")),
        term: term_name(),
        cols,
        rows,
    };
    interactive(ctx, node, kind, reason).await
}

/// A terminal inside one container instance on `node` (Phase B, E3) —
/// `meshnode deployment exec`. The same session, gate and audit as
/// [`shell`]; only what the far node spawns differs.
pub async fn instance(
    ctx: &Ctx,
    node: &str,
    job: &str,
    reason: &str,
    command: Vec<String>,
) -> Result<i32> {
    let (cols, rows) = window();
    let kind = SessionKind::Instance {
        job: job.to_string(),
        command: (!command.is_empty()).then(|| command.join(" ")),
        term: term_name(),
        cols,
        rows,
    };
    interactive(ctx, node, kind, reason).await
}

/// The half every terminal shares: raw mode, keystrokes and window size up,
/// output down, and the far process's exit code back.
async fn interactive(ctx: &Ctx, node: &str, kind: SessionKind, reason: &str) -> Result<i32> {
    let stream =
        ipc::open_session(&ctx.config.socket_path(), &ctx.user, node, kind, reason).await?;
    let (mut from_daemon, to_daemon) = stream.into_split();

    // Raw mode goes on *before* the first byte is read and comes off in
    // this frame's scope, whichever way we leave it.
    let _raw = RawMode::enter();

    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<SessionFrame>(64);

    // stdin → frames. A blocking-file stdin cannot be cancelled, so this
    // task is detached and the process exit is what ends it.
    let stdin_tx = frames_tx.clone();
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; STDIN_CHUNK];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx
                        .send(SessionFrame::Data(buf[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = stdin_tx.send(SessionFrame::Eof).await;
    });

    // A resized window is not cosmetic: a full-screen editor draws to the
    // size it was told, and a stale size makes it draw over itself.
    let winch_tx = frames_tx.clone();
    tokio::spawn(async move {
        let Ok(mut winch) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        else {
            return;
        };
        while winch.recv().await.is_some() {
            let (cols, rows) = window();
            if winch_tx
                .send(SessionFrame::Resize { cols, rows })
                .await
                .is_err()
            {
                return;
            }
        }
    });
    drop(frames_tx);

    let writer = tokio::spawn(pump_frames(to_daemon, frames_rx));
    let code = read_session(&mut from_daemon, true).await;
    writer.abort();
    code
}

/// Render a session's output until it ends, and return the exit code.
///
/// `merged` is the PTY case: a terminal has one stream, and splitting it
/// would interleave two file descriptors that the far side never split.
async fn read_session<R>(reader: &mut R, merged: bool) -> Result<i32>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    loop {
        let frame: SessionServerFrame = match read_frame(reader).await {
            Ok(f) => f,
            // The daemon went away without a terminal frame. Not an error
            // to invent an exit code for: 255 is the "connection ended"
            // convention every remote-shell tool uses.
            Err(_) => return Ok(255),
        };
        match frame {
            SessionServerFrame::Opened => {}
            SessionServerFrame::Data { stream, bytes } => {
                if merged || stream != STREAM_STDERR {
                    stdout.write_all(&bytes).await?;
                    stdout.flush().await?;
                } else {
                    stderr.write_all(&bytes).await?;
                    stderr.flush().await?;
                }
            }
            SessionServerFrame::Exit { code, signal } => {
                let _ = stdout.flush().await;
                // The shell convention: a process killed by signal N exits
                // 128+N, so a script can tell "failed" from "was killed".
                return Ok(match (code, signal) {
                    (Some(code), _) => code,
                    (None, Some(sig)) => 128 + sig,
                    _ => 0,
                });
            }
            SessionServerFrame::Error { message } => {
                let _ = stdout.flush().await;
                bail!("{message}");
            }
            SessionServerFrame::Meta { .. } | SessionServerFrame::Digest(_) => {}
        }
    }
}

async fn pump_frames<W>(mut writer: W, mut rx: tokio::sync::mpsc::Receiver<SessionFrame>)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if write_frame(&mut writer, &frame).await.is_err() {
            return;
        }
    }
}
