//! **A terminal cannot read the node's key** (SEC-8), tested where a real
//! binary exists to host both shims.
//!
//! A shell session ran as the daemon with no ruleset, so granting somebody a
//! terminal handed them the node's identity and every secret sealed to it.
//! Now the PTY shim starts the sandbox shim, which confines the shell away
//! from the node's directory and the account's credential stores before the
//! shell runs — and leaves the rest of the machine to it, because a
//! terminal is for working on the machine.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tokio::net::UnixStream;

use mesh_proto::{IpcRequest, SessionKind, SessionServerFrame, read_frame};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder_with};

async fn node(mesh: &str) -> TestNode {
    let node = spawn_founder_with("alpha", mesh, vec![], |cfg| {
        cfg.pty_shim_override = Some(env!("CARGO_BIN_EXE_meshnode").into());
        cfg.runtime.sandbox.shim = Some(env!("CARGO_BIN_EXE_meshnode").into());
        cfg.sessions.accept = true;
    })
    .await;
    let admin = founder_user_key(&node);
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
        &node,
        admin,
        IpcRequest::SessionPolicyPut {
            policy: Box::new(policy),
        },
    )
    .await;
    assert!(matches!(resp, mesh_proto::IpcResponse::Ok), "{resp:?}");
    node
}

async fn run_in_terminal(node: &TestNode, command: &str) -> String {
    let req = IpcRequest::SessionOpen {
        node: "alpha".into(),
        kind: SessionKind::Shell {
            command: Some(command.to_string()),
            term: "dumb".into(),
            cols: 200,
            rows: 24,
        },
        reason: "SEC-8 test".into(),
    };
    let envelope = founder_user_key(node).sign_request(&req).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let n = node.node.clone();
    tokio::spawn(async move { n.serve_ipc_session(envelope, server).await });
    let mut out = Vec::new();
    let drained = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match read_frame::<_, SessionServerFrame>(&mut client).await {
                Ok(SessionServerFrame::Data { bytes, .. }) => out.extend_from_slice(&bytes),
                Ok(SessionServerFrame::Exit { .. }) | Err(_) => break,
                Ok(SessionServerFrame::Error { message }) => panic!("refused: {message}"),
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(drained.is_ok(), "the terminal hung");
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_cannot_read_the_node_identity() {
    let node = node("session-sandbox").await;
    let identity = node.config().data_dir.join("identity");
    assert!(identity.is_dir());
    // Somewhere to write that is not a directory holding the node's own:
    // Landlock grants whole subtrees, so a new file cannot be created
    // *directly* in a parent of the data directory — here `/tmp` itself,
    // because the test node lives there.
    let scratch = tempfile::tempdir().unwrap();
    let script = format!(
        "echo sandbox=$MESHWARE_SANDBOX; \
         if cat '{}'/* >/dev/null 2>&1; then echo KEY_READ; else echo KEY_REFUSED; fi; \
         if cat /etc/hostname >/dev/null 2>&1 || cat /etc/os-release >/dev/null 2>&1; then echo ETC_OK; else echo ETC_REFUSED; fi; \
         if touch '{}'/work 2>/dev/null; then echo WRITE_OK; else echo WRITE_REFUSED; fi; \
         echo END",
        identity.display(),
        scratch.path().display()
    );
    let text = run_in_terminal(&node, &script).await;
    assert!(text.contains("END"), "the script did not finish: {text:?}");
    let status = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("sandbox="))
        .unwrap_or("")
        .trim()
        .to_string();
    // Whether this machine can confine at all is asked of the sandbox
    // itself, not read off the terminal: an unconfined terminal reports no
    // status, and a test that took that as "no Landlock here" would pass
    // with the confinement removed.
    let policy = mesh_core::sandbox::Policy {
        shim: Some(env!("CARGO_BIN_EXE_meshnode").into()),
        ..Default::default()
    };
    let available = mesh_core::sandbox::unavailable_reason(&policy).is_none();
    match status.as_str() {
        _ if !available => {
            eprintln!("no Landlock here; the rules were not exercised");
        }
        "full" | "partial" => {
            assert!(
                text.contains("KEY_REFUSED"),
                "a terminal read the node key: {text:?}"
            );
            assert!(
                text.contains("ETC_OK"),
                "a terminal lost the system: {text:?}"
            );
            assert!(
                text.contains("WRITE_OK"),
                "a terminal cannot write its own files: {text:?}"
            );
            assert!(scratch.path().join("work").exists());
        }
        other => panic!(
            "this machine can confine a terminal and the terminal reported {other:?}: {text:?}"
        ),
    }
    node.node.shutdown();
}
