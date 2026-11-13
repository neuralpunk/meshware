//! The PTY half of M17 (§3.6), tested where a real binary exists.
//!
//! A pseudoterminal session re-execs its host binary as the child so that
//! `setsid()` + `TIOCSCTTY` can run in an ordinary `main` instead of an
//! `unsafe` pre-exec closure (`mesh_core::sessions::pty_shim`). That makes
//! this the one part of the milestone an in-process test in `mesh-core`
//! cannot reach: *its* `current_exe()` is a test harness that has never
//! heard of the shim argument. Here `CARGO_BIN_EXE_meshnode` is a binary
//! whose `main` calls `pty_shim::try_run()` as its first statement, so the
//! whole path is real.
//!
//! Two claims are worth the trouble:
//!
//! 1. **It is a terminal.** `tty` inside the session names a pts, which is
//!    what makes `isatty` true and readline behave.
//! 2. **It is a *controlling* terminal.** The child is its own session
//!    leader, which is what gives the line discipline a process group to
//!    signal — without it Ctrl-C does nothing, and a shell where Ctrl-C
//!    does nothing is not a shell.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tokio::net::UnixStream;

use mesh_proto::{
    IpcRequest, SessionFrame, SessionKind, SessionServerFrame, read_frame, write_frame,
};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder_with};

/// A node whose PTY sessions re-exec the real `meshnode` binary, with
/// terminals switched on and a policy that permits one.
///
/// Both are needed and neither is a default: `[sessions] accept` is off out
/// of the box, and a session policy has to grant the user a shell on this
/// node. A test that only did one of them would pass for the wrong reason
/// the day the other regressed.
async fn node(mesh: &str) -> TestNode {
    let node = spawn_founder_with("alpha", mesh, vec![], |cfg| {
        cfg.pty_shim_override = Some(env!("CARGO_BIN_EXE_meshnode").into());
        cfg.sessions.accept = true;
    })
    .await;
    permit_shell(&node).await;
    node
}

/// Publish an admin-signed policy granting everyone a terminal everywhere.
async fn permit_shell(node: &TestNode) {
    let admin = founder_user_key(node);
    let mut policy = mesh_proto::SessionPolicy {
        id: "test".into(),
        description: String::new(),
        users: "*".into(),
        nodes: "any".into(),
        allow: vec![mesh_proto::SessionPermission::Shell],
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        signature: Vec::new(),
    };
    policy.signature = admin.sign(&policy.signing_bytes()).to_vec();
    let resp = mesh_testkit::ipc_as(
        node,
        admin,
        IpcRequest::SessionPolicyPut {
            policy: Box::new(policy),
        },
    )
    .await;
    assert!(
        matches!(resp, mesh_proto::IpcResponse::Ok),
        "policy put: {resp:?}"
    );
}

async fn shell(node: &TestNode, command: &str, cols: u16, rows: u16) -> UnixStream {
    let req = IpcRequest::SessionOpen {
        node: "alpha".into(),
        kind: SessionKind::Shell {
            command: Some(command.to_string()),
            term: "xterm-256color".into(),
            cols,
            rows,
        },
        reason: "M17 test".into(),
    };
    let envelope = founder_user_key(node).sign_request(&req).unwrap();
    let (client, server) = UnixStream::pair().unwrap();
    let n = node.node.clone();
    tokio::spawn(async move {
        n.serve_ipc_session(envelope, server).await;
    });
    client
}

/// Everything the session printed, until it ends.
async fn transcript(stream: &mut UnixStream) -> String {
    let mut out = Vec::new();
    loop {
        match read_frame::<_, SessionServerFrame>(stream).await {
            Ok(SessionServerFrame::Data { bytes, .. }) => out.extend_from_slice(&bytes),
            Ok(SessionServerFrame::Exit { .. }) => break,
            Ok(SessionServerFrame::Error { message }) => {
                panic!("session refused: {message}")
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// §3.6: the child runs on a real pseudoterminal.
#[tokio::test]
async fn a_pty_session_runs_on_a_terminal() {
    let node = node("pty-tty").await;
    let mut s = shell(&node, "tty", 80, 24).await;
    let text = tokio::time::timeout(Duration::from_secs(30), transcript(&mut s))
        .await
        .expect("the session must not hang");
    assert!(
        text.contains("/dev/pts/"),
        "expected a pts, got {text:?} — the child is not on a terminal"
    );
}

/// **The reason the shim exists.** The child is its own session leader with
/// the pts as its controlling terminal, so the line discipline has a
/// process group to signal. Without `setsid` + `TIOCSCTTY` the pts's `pgrp`
/// is never set and Ctrl-C reaches nobody.
///
/// `ps -o sid=,pgid=` of the shell reports its session and process-group
/// ids; a session leader has `sid == pid`, and `ps -o tty=` names the
/// controlling terminal rather than `?`.
#[tokio::test]
async fn the_child_owns_its_terminal_so_ctrl_c_has_somewhere_to_go() {
    let node = node("pty-ctty").await;
    let mut s = shell(&node, "ps -o sid=,pid=,tty= -p $$", 80, 24).await;
    let text = tokio::time::timeout(Duration::from_secs(30), transcript(&mut s))
        .await
        .expect("the session must not hang");

    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.contains("pts/"))
        .unwrap_or_else(|| panic!("no ps output naming a pts in {text:?}"));
    let fields: Vec<&str> = line.split_whitespace().collect();
    assert!(fields.len() >= 3, "unexpected ps output {line:?}");
    assert_eq!(
        fields[0], fields[1],
        "the shell is not a session leader (sid != pid) in {line:?} — \
         setsid did not run, so Ctrl-C would reach nobody"
    );
    assert!(
        fields[2].contains("pts/"),
        "the shell has no controlling terminal in {line:?} — TIOCSCTTY did not run"
    );
}

/// The window size the client asked for is the size the far program sees,
/// and a later `Resize` changes it. A full-screen editor draws to the size
/// it was told, and a stale one makes it draw over itself.
#[tokio::test]
async fn the_window_size_crosses_and_a_resize_changes_it() {
    let node = node("pty-size").await;
    // Two readings from one shell: the size at start, then the size after a
    // resize frame lands mid-session.
    let mut s = shell(&node, "stty size; read _ ; stty size", 120, 40).await;

    // Give the shell a moment to print the first reading and reach `read`,
    // then resize and let it print again.
    tokio::time::sleep(Duration::from_millis(400)).await;
    write_frame(
        &mut s,
        &SessionFrame::Resize {
            cols: 100,
            rows: 30,
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    write_frame(&mut s, &SessionFrame::Data(b"\n".to_vec()))
        .await
        .unwrap();

    let text = tokio::time::timeout(Duration::from_secs(30), transcript(&mut s))
        .await
        .expect("the session must not hang");
    assert!(
        text.contains("40 120"),
        "the initial window never crossed: {text:?}"
    );
    assert!(
        text.contains("30 100"),
        "the resize never reached the terminal: {text:?}"
    );
}

/// A PTY has one stream: stderr on the far side is the same file
/// description as stdout, and claiming otherwise would be a fiction the
/// terminal cannot honour.
#[tokio::test]
async fn a_terminal_has_one_stream() {
    let node = node("pty-one-stream").await;
    let mut s = shell(&node, "echo out; echo err >&2", 80, 24).await;

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(30), read_frame(&mut s))
            .await
            .expect("the session must not hang")
        {
            Ok(SessionServerFrame::Data { stream, bytes }) => {
                if stream == mesh_proto::STREAM_STDERR {
                    stderr_bytes.extend_from_slice(&bytes);
                } else {
                    stdout_bytes.extend_from_slice(&bytes);
                }
            }
            Ok(SessionServerFrame::Exit { .. }) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let text = String::from_utf8_lossy(&stdout_bytes);
    assert!(text.contains("out") && text.contains("err"), "{text:?}");
    assert!(
        stderr_bytes.is_empty(),
        "a PTY session must not claim a separate stderr"
    );
}

/// The far command's exit code comes back, so `meshnode shell node -- cmd`
/// is usable in a script.
#[tokio::test]
async fn the_exit_code_comes_back() {
    let node = node("pty-exit").await;
    let mut s = shell(&node, "exit 7", 80, 24).await;
    let code = loop {
        match tokio::time::timeout(Duration::from_secs(30), read_frame(&mut s))
            .await
            .expect("the session must not hang")
        {
            Ok(SessionServerFrame::Exit { code, .. }) => break code,
            Ok(SessionServerFrame::Error { message }) => panic!("refused: {message}"),
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    assert_eq!(code, Some(7));
}

/// The Build farm panel's line, through the session that will actually run
/// it (`apps/director/src-tauri/src/lib.rs`, `farm_build_line`).
///
/// The panel composes one string — `cd <project> && export MESHWARE_HOME=…
/// CC=… CXX=… && <build command>` — and hands it to a PTY session on this
/// node. Three things in that shape can quietly fail, and all three would
/// look like "the build farm doesn't work" rather than like a quoting bug:
///
///   * the `cd` has to land, or the build runs in `$HOME` (a session's
///     child starts there — `sessions/host.rs`, `base_env`);
///   * the exports have to survive into the command, because
///     `MESHWARE_HOME` is what points the wrapper at *this* node's daemon
///     rather than a standalone `meshnode`'s home;
///   * a project path with a space in it has to stay one path.
#[tokio::test]
async fn the_build_farm_line_lands_in_the_project_with_its_environment() {
    let node = node("pty-farm-line").await;
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("my project");
    std::fs::create_dir(&project).expect("project dir");
    let project = project.canonicalize().expect("canonical");

    // Quoted exactly as `sh_quote` does it on the Director side.
    let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let line = format!(
        "cd {} && export MESHWARE_HOME={} CC={} && pwd && echo \"[$MESHWARE_HOME][$CC]\"",
        q(&project.to_string_lossy()),
        q("/home/someone/.meshware/director"),
        q("/usr/bin/meshware-director cc gcc"),
    );

    let mut s = shell(&node, &line, 80, 24).await;
    let text = tokio::time::timeout(Duration::from_secs(30), transcript(&mut s))
        .await
        .expect("the session must not hang");

    assert!(
        text.contains(&project.to_string_lossy().to_string()),
        "the build did not start in the project directory; got {text:?}"
    );
    assert!(
        text.contains("[/home/someone/.meshware/director][/usr/bin/meshware-director cc gcc]"),
        "the exports did not reach the build command; got {text:?}"
    );
}
