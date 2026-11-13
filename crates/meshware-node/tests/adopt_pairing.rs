//! `meshnode adopt`'s pairing half, against the real binary.
//!
//! Adopt is two mechanisms glued together: SSH, which is `ssh(1)` and needs
//! no test of ours, and **pairing driven by a program instead of a person**
//! — `join --pair --json`, its NDJSON frames, and the code comparison that
//! replaces a human's eyes with the SSH channel. That second half is the
//! part that is new and the part that carries the security argument, so it
//! is what this exercises.
//!
//! SSH is stood in for by an ordinary pipe. That is exactly the substitution
//! adopt makes on purpose: the transport's only job is to carry the joiner's
//! code back over a channel the pairing connection's attacker does not
//! control, and a pipe to a child process is that, trivially. What is real
//! here is everything else — a real `meshnode` process, a real mDNS-free
//! dial by address, a real commitment exchange, real Ed25519 identities, and
//! a real membership at the end.
//!
//! The claims:
//!
//! 1. a scripted joiner announces its port, then its code, as parseable
//!    NDJSON — the two frames `adopt` steers by;
//! 2. matching codes plus `yes` on stdin produce an actual member;
//! 3. `no` on stdin joins nothing, so a mismatch (which is what `no` stands
//!    in for) really is fatal rather than cosmetic;
//! 4. the code the joiner prints is the code the approver derived — the
//!    equality `adopt` tests, tested.
#![allow(clippy::unwrap_used)]

use std::process::Stdio;
use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, spawn_founder, stream_as};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// A `meshnode join --pair --json` child with its own empty home.
struct Joiner {
    child: Child,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stdin: tokio::process::ChildStdin,
    _home: tempfile::TempDir,
}

impl Joiner {
    /// `wait` is the joiner's own `--wait`. Short in the decline test on
    /// purpose: a declining approver closes the connection without sending
    /// the confirm round, so the joiner has nothing to react to and waits
    /// this out. `adopt` does not sit through that — it kills the SSH
    /// child, which takes the remote process with it (`Joiner::abandon`) —
    /// but the joiner's own behaviour is what this test drives directly.
    fn start(name: &str, wait: &str) -> Joiner {
        let home = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_meshnode"));
        cmd.arg("--data-dir")
            .arg(home.path())
            .arg("join")
            .arg("--pair")
            .arg("--json")
            .arg("--wait")
            .arg(wait)
            .arg("--listen")
            // An ephemeral port: several of these run at once under
            // `cargo test`, and a fixed one would make them flaky in a way
            // that looks like a pairing bug.
            .arg("127.0.0.1:0")
            .env("MESHWARE_NODE_NAME", name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn meshnode");
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        Joiner {
            child,
            lines: BufReader::new(stdout).lines(),
            stdin,
            _home: home,
        }
    }

    /// The next NDJSON object, ignoring anything that is not one.
    async fn frame(&mut self) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let line = tokio::time::timeout_at(deadline, self.lines.next_line())
                .await
                .expect("the joiner went quiet")
                .expect("reading the joiner")
                .expect("the joiner exited early");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                return v;
            }
        }
    }

    async fn answer(&mut self, accept: bool) {
        let verdict = if accept { "yes\n" } else { "no\n" };
        self.stdin.write_all(verdict.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }
}

impl Drop for Joiner {
    fn drop(&mut self) {
        self.child.start_kill().ok();
    }
}

/// Mint a single-use invite the way `adopt` and `invite approve` both do.
async fn invite(node: &TestNode) -> String {
    let user = founder_user_key(node);
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs,
    } = ipc_as(node, user, IpcRequest::InviteBootstrap).await
    else {
        panic!("expected InviteBootstrap");
    };
    let params = mesh_core::membership::InviteParams {
        ttl_secs: 600,
        uses: 1,
        tags: vec![],
        bootstrap: addrs,
    };
    let (token, public) =
        mesh_core::membership::build_invite(mesh_id, user, &params, node_id).unwrap();
    let IpcResponse::Ok = ipc_as(node, user, IpcRequest::InvitePublish { public }).await else {
        panic!("publishing the invite");
    };
    token.encode()
}

/// The joiner's `listening` frame gives adopt the port to dial.
#[tokio::test(flavor = "multi_thread")]
async fn a_scripted_joiner_announces_where_to_dial_it() {
    let mut joiner = Joiner::start("rack-1", "60s");
    let frame = joiner.frame().await;
    assert_eq!(frame["event"], "listening");
    let port = frame["port"].as_u64().expect("a port");
    assert!(port > 0, "a real bound port, not zero");
    assert_eq!(frame["node_name"], "rack-1");
}

/// Codes agree, `yes` is answered, and the machine is a member.
#[tokio::test(flavor = "multi_thread")]
async fn matching_codes_and_a_yes_produce_a_member() {
    let approver = spawn_founder("approver", "adopt-yes").await;
    let mut joiner = Joiner::start("rack-1", "60s");

    let listening = joiner.frame().await;
    let port = listening["port"].as_u64().unwrap();
    let addr = format!("127.0.0.1:{port}");

    let mut stream = stream_as(
        &approver,
        founder_user_key(&approver),
        IpcRequest::PairApprove {
            addr,
            token: invite(&approver).await,
        },
    )
    .await;

    // The joiner's code, off the "SSH" channel…
    let code_frame = joiner.frame().await;
    assert_eq!(code_frame["event"], "code");
    let joiner_code = code_frame["code"].as_str().unwrap().to_string();
    // …and the word pair travels with it, for the transcript.
    let words = code_frame["words"].as_str().expect("a word pair");
    assert_eq!(
        mesh_core::wordpair::code_for(words).as_deref(),
        Some(joiner_code.as_str()),
        "the words are the code, not a summary of it"
    );

    // …and the approver's own, off the pairing connection.
    let (session, our_code) = loop {
        match stream.recv().await.expect("a pairing frame") {
            IpcResponse::PairCode { session, code, .. } => break (session, code),
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                panic!("pairing failed: {message}")
            }
            _ => {}
        }
    };

    // The comparison adopt makes. Two independent derivations agreeing is
    // the whole of the security argument.
    assert_eq!(
        our_code, joiner_code,
        "both sides derived the same six digits"
    );

    joiner.answer(true).await;
    let IpcResponse::Ok = ipc_as(
        &approver,
        founder_user_key(&approver),
        IpcRequest::PairDecide {
            session,
            accept: true,
        },
    )
    .await
    else {
        panic!("PairDecide");
    };

    let done = loop {
        match stream.recv().await.expect("a pairing frame") {
            IpcResponse::PairDone { node_name, .. } => break node_name,
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                panic!("pairing failed after confirmation: {message}")
            }
            _ => {}
        }
    };
    assert_eq!(done, "rack-1");

    // The joiner agrees it joined, on its own channel.
    let joined = joiner.frame().await;
    assert_eq!(joined["event"], "joined", "{joined}");
    assert_eq!(joined["mesh_name"], "adopt-yes");
    assert_eq!(joined["members"], 2);

    // …and the mesh agrees, which is the only claim that really matters.
    let IpcResponse::Nodes(rows) =
        ipc_as(&approver, founder_user_key(&approver), IpcRequest::Nodes).await
    else {
        panic!("expected Nodes");
    };
    assert!(
        rows.iter().any(|r| r.name == "rack-1"),
        "the adopted machine is a member: {:?}",
        rows.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
}

/// A `no` — what adopt sends when the codes disagree — joins nothing.
///
/// The pinned property is that the refusal is *real*: a mismatch that
/// printed a warning and paired anyway would leave the security argument as
/// decoration.
#[tokio::test(flavor = "multi_thread")]
async fn a_declined_code_joins_nothing() {
    let approver = spawn_founder("approver", "adopt-no").await;
    let mut joiner = Joiner::start("rack-9", "5s");

    let listening = joiner.frame().await;
    let port = listening["port"].as_u64().unwrap();

    let mut stream = stream_as(
        &approver,
        founder_user_key(&approver),
        IpcRequest::PairApprove {
            addr: format!("127.0.0.1:{port}"),
            token: invite(&approver).await,
        },
    )
    .await;

    let code_frame = joiner.frame().await;
    assert_eq!(code_frame["event"], "code");
    let session = loop {
        match stream.recv().await.expect("a pairing frame") {
            IpcResponse::PairCode { session, .. } => break session,
            IpcResponse::Error { message } => panic!("pairing failed: {message}"),
            _ => {}
        }
    };

    joiner.answer(false).await;
    let _ = ipc_as(
        &approver,
        founder_user_key(&approver),
        IpcRequest::PairDecide {
            session,
            accept: false,
        },
    )
    .await;

    // The joiner reports a failure rather than exiting silently — adopt
    // turns this into the host's error line. (It arrives when the joiner's
    // own `--wait` runs out, because a declining approver closes the
    // connection without a confirm round for the joiner to answer; adopt
    // does not wait for it, it kills the process.)
    let ended = joiner.frame().await;
    assert_eq!(ended["event"], "error", "{ended}");

    // Nothing joined.
    let IpcResponse::Nodes(rows) =
        ipc_as(&approver, founder_user_key(&approver), IpcRequest::Nodes).await
    else {
        panic!("expected Nodes");
    };
    assert!(
        !rows.iter().any(|r| r.name == "rack-9"),
        "a declined pairing left a member behind: {:?}",
        rows.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
}
