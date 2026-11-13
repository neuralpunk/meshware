//! M17 the fabric (`docs/M17-M21-FABRIC-KNOWLEDGE-AND-AGENTS.md` §3): a
//! session, not a job.
//!
//! **v1 is the terminal and nothing else**, and it is off until three
//! independent things say yes:
//!
//! 1. the node's operator turned terminals on (`[sessions] accept`),
//! 2. the caller holds the runner grant they would need to `meshnode run`
//!    there, and
//! 3. a replicated, admin-signed **session policy** names that user and
//!    that node.
//!
//! Most of what is asserted below is the *absence* of a terminal, because
//! that is what a security model is: each of those three is tested by
//! removing it and watching the prompt not appear. The tests that check the
//! pipe matter too — every other request in the system closes its write
//! half before the first response (R1), so a test that only read output
//! would pass against a broken pipe.
//!
//! The `setsid` + `TIOCSCTTY` shim that makes Ctrl-C work is tested in
//! `meshware-node`, where a real binary exists to re-exec. Here a stand-in
//! shim gives a genuine pseudoterminal without one.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tokio::net::UnixStream;

use mesh_core::identity::Keypair;
use mesh_proto::{
    Grant, GrantId, IpcRequest, IpcResponse, Liveness, Role, SessionFrame, SessionKind,
    SessionPermission, SessionPolicy, SessionServerFrame, UserRecord, read_frame, write_frame,
};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder_with, wait_until,
};

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A stand-in for the PTY shim.
///
/// The real one re-execs the host binary so `setsid` + `TIOCSCTTY` can run
/// in an ordinary `main`; an in-process test's `current_exe()` is a test
/// harness that has never heard of it. This honours the same contract —
/// `<exe> __meshware-pty <program> <args…>` — minus the controlling
/// terminal, which is the part `meshware-node`'s tests cover with a real
/// binary. Everything here needs a *pseudoterminal*, and gets one.
fn stand_in_shim(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("shim.sh");
    std::fs::write(&path, "#!/bin/sh\nshift\nexec \"$@\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A node plus the tempdir holding its stand-in shim.
///
/// The field is `inner`, not `node`, so `f.node` derefs through to
/// `TestNode::node` — naming it `node` would shadow the thing every test
/// actually wants.
struct Fixture {
    inner: TestNode,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Fixture {
    type Target = TestNode;
    fn deref(&self) -> &TestNode {
        &self.inner
    }
}

/// A founder with nothing turned on — the out-of-the-box posture.
async fn bare(
    mesh: &str,
    tags: Vec<String>,
    mutate: impl FnOnce(&mut mesh_core::config::NodeConfig),
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let shim = stand_in_shim(dir.path());
    let node = spawn_founder_with("alpha", mesh, tags, |cfg| {
        cfg.pty_shim_override = Some(shim);
        mutate(cfg);
    })
    .await;
    Fixture {
        inner: node,
        _dir: dir,
    }
}

/// A founder with terminals enabled and a policy that permits them.
async fn permissive(mesh: &str) -> Fixture {
    let f = bare(mesh, vec![], |cfg| cfg.sessions.accept = true).await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    f
}

/// Publish an admin-signed session policy.
async fn permit(node: &TestNode, id: &str, users: &str, nodes: &str, allow: &[SessionPermission]) {
    let admin = founder_user_key(node);
    let mut policy = SessionPolicy {
        id: id.into(),
        description: String::new(),
        users: users.into(),
        nodes: nodes.into(),
        allow: allow.to_vec(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        signature: Vec::new(),
    };
    policy.signature = admin.sign(&policy.signing_bytes()).to_vec();
    let resp = ipc_as(
        node,
        admin,
        IpcRequest::SessionPolicyPut {
            policy: Box::new(policy),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "policy put: {resp:?}");
}

fn terminal(command: &str) -> SessionKind {
    SessionKind::Shell {
        command: Some(command.to_string()),
        term: "xterm-256color".into(),
        cols: 80,
        rows: 24,
    }
}

/// Open a session over a real socket pair — the local leg exactly as
/// `meshnode` drives it (R4).
async fn open_as(
    node: &TestNode,
    user: &Keypair,
    target: &str,
    kind: SessionKind,
    reason: &str,
) -> UnixStream {
    let req = IpcRequest::SessionOpen {
        node: target.to_string(),
        kind,
        reason: reason.to_string(),
    };
    let envelope = user.sign_request(&req).expect("sign");
    let (client, server) = UnixStream::pair().expect("socket pair");
    let n = node.node.clone();
    tokio::spawn(async move {
        n.serve_ipc_session(envelope, server).await;
    });
    client
}

async fn open(node: &TestNode, target: &str, kind: SessionKind) -> UnixStream {
    open_as(node, founder_user_key(node), target, kind, "INC-1").await
}

/// Everything the session printed, and how it ended.
async fn drain(stream: &mut UnixStream) -> (String, std::result::Result<i32, String>) {
    let mut out = Vec::new();
    loop {
        match read_frame::<_, SessionServerFrame>(stream).await {
            Ok(SessionServerFrame::Data { bytes, .. }) => out.extend_from_slice(&bytes),
            Ok(SessionServerFrame::Exit { code, .. }) => {
                return (
                    String::from_utf8_lossy(&out).into_owned(),
                    Ok(code.unwrap_or(0)),
                );
            }
            Ok(SessionServerFrame::Error { message }) => {
                return (String::from_utf8_lossy(&out).into_owned(), Err(message));
            }
            Ok(_) => {}
            Err(_) => return (String::from_utf8_lossy(&out).into_owned(), Ok(255)),
        }
    }
}

/// The refusal a session ends with, or a panic naming what happened instead.
async fn refusal(stream: &mut UnixStream) -> String {
    match drain(stream).await {
        (_, Err(message)) => message,
        (out, Ok(code)) => panic!("expected a refusal, got exit {code} with {out:?}"),
    }
}

async fn add_user(node: &TestNode, user: &Keypair, username: &str, role: Role) {
    let admin = founder_user_key(node);
    let mut record = UserRecord {
        user_id: user.user_id(),
        username: username.into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::UserAdd { record }).await,
        IpcResponse::Ok
    ));
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id: user.user_id(),
        role,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::GrantAdd { grant }).await,
        IpcResponse::Ok
    ));
}

async fn wait_reachable(origin: &TestNode, peer: &TestNode) {
    let engine = origin.node.engine().clone();
    let gossip = origin.node.gossip().clone();
    let id = peer.node.node_id();
    let addr = peer.addr_string();
    assert!(
        wait_until(Duration::from_secs(20), || {
            engine.read(|s| s.is_active_member(&id) && s.members[&id].info.addrs.contains(&addr))
                && matches!(gossip.liveness_of(&id), Liveness::Alive)
        })
        .await,
        "peer never became reachable"
    );
}

// ---------------------------------------------------------------------------
// The pipe
// ---------------------------------------------------------------------------

/// **The milestone in one assertion.** Bytes written *after* the request
/// frame reach the far shell, and its output comes back.
///
/// Every other request in the system finishes its write half before the
/// first response arrives (R1, `transport.rs` `request_stream`) — which is
/// why "keystrokes have nowhere to go" and why the first design on paper
/// was unimplementable rather than merely awkward.
#[tokio::test]
async fn keystrokes_written_after_the_request_reach_the_far_shell() {
    let f = permissive("fabric-pipe").await;
    let mut s = open(&f, "alpha", terminal("read line; echo \"got:$line\"")).await;
    write_frame(&mut s, &SessionFrame::Data(b"hello fabric\n".to_vec()))
        .await
        .unwrap();
    let (out, outcome) = drain(&mut s).await;
    assert_eq!(outcome, Ok(0));
    assert!(out.contains("got:hello fabric"), "{out:?}");
}

/// It is a real terminal, which is what makes `vim`, `top` and job control
/// work rather than merely `echo`.
#[tokio::test]
async fn the_far_command_runs_on_a_pseudoterminal() {
    let f = permissive("fabric-tty").await;
    let mut s = open(&f, "alpha", terminal("tty; stty size")).await;
    let (out, outcome) = drain(&mut s).await;
    assert_eq!(outcome, Ok(0));
    assert!(out.contains("/dev/pts/"), "{out:?}");
    assert!(
        out.contains("24 80"),
        "the window size never crossed: {out:?}"
    );
}

/// A terminal on a *remote* member, over the wire leg rather than the local
/// one — the same handler, reached through QUIC.
#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_opens_on_another_member() {
    let dir = tempfile::tempdir().unwrap();
    let shim = stand_in_shim(dir.path());
    let f = bare("fabric-remote", vec![], |cfg| cfg.sessions.accept = true).await;
    let beta = mesh_testkit::join_new_node_with("beta", &f, vec![], |cfg| {
        cfg.pty_shim_override = Some(shim);
        cfg.sessions.accept = true;
    })
    .await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    wait_reachable(&f, &beta).await;

    let mut s = open(&f, "beta", terminal("echo remote-prompt")).await;
    let (out, outcome) = drain(&mut s).await;
    assert_eq!(outcome, Ok(0), "{out:?}");
    assert!(out.contains("remote-prompt"), "{out:?}");
    drop(beta);
}

/// **Enforcement is on the node being shelled into, not the one asking.**
///
/// The origin here has terminals on and a policy that covers everything;
/// the target has terminals off. The refusal comes from the target, which
/// is the MASTERPLAN's "enforcement happens on the executing node" applied
/// to a prompt — and it means a compromised origin cannot talk a node into
/// opening one.
#[tokio::test(flavor = "multi_thread")]
async fn the_target_decides_not_the_origin() {
    let f = bare("fabric-enforce", vec![], |cfg| cfg.sessions.accept = true).await;
    let beta = join_new_node("beta", &f, vec![]).await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    wait_reachable(&f, &beta).await;

    let mut s = open(&f, "beta", terminal("id")).await;
    let message = refusal(&mut s).await;
    assert!(message.contains("beta"), "{message}");
    assert!(message.contains("does not accept terminals"), "{message}");
    drop(beta);
}

/// SEC-11: an envelope opens a terminal only on the machine it names.
///
/// Each node keeps its own nonce cache, so an envelope signed for a shell on
/// beta — captured by any node it passed through — was fresh on alpha too,
/// and alpha never asked which machine it was for. A stranger's session was
/// not even checked for membership.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_envelope_opens_only_the_node_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let shim = stand_in_shim(dir.path());
    let f = permissive("fabric-target").await;
    let beta = mesh_testkit::join_new_node_with("beta", &f, vec![], |cfg| {
        cfg.pty_shim_override = Some(shim);
        cfg.sessions.accept = true;
    })
    .await;
    wait_reachable(&f, &beta).await;
    wait_reachable(&beta, &f).await;

    // The founder asks for a shell on beta; beta replays it at alpha.
    let kind = terminal("echo replayed");
    let auth = founder_user_key(&f)
        .sign_request(&IpcRequest::SessionOpen {
            node: "beta".into(),
            kind: kind.clone(),
            reason: "INC-2".into(),
        })
        .unwrap();
    let spec = mesh_proto::SessionSpec { kind, auth };
    let replay = async |from: &mesh_core::transport::Transport, spec| {
        let alpha = f.node.node_id();
        let conn = from
            .member_conn(alpha, &[f.addr_string()])
            .await
            .expect("dial alpha");
        let (_send, mut recv) = from.open_session(&conn, spec).await.expect("open");
        match read_frame::<_, SessionServerFrame>(&mut recv).await {
            Ok(SessionServerFrame::Error { message }) => message,
            other => panic!("a replayed session was not refused: {other:?}"),
        }
    };
    let message = replay(beta.node.transport(), spec.clone()).await;
    assert!(message.contains("not this node"), "{message}");

    // A key the mesh never admitted, with the mesh id and a valid envelope.
    let key = Keypair::generate().unwrap();
    let local = mesh_core::transport::LocalInfo {
        node_id: key.node_id(),
        node_name: "mallory".into(),
        mesh_id: Some(f.node.engine().genesis().body.mesh_id),
        mesh_name: None,
    };
    let (stranger, rx) = mesh_core::transport::Transport::bind(
        "127.0.0.1:0".parse().unwrap(),
        key,
        local,
        std::sync::Arc::new(|_, _| Ok(())),
        Default::default(),
    )
    .await
    .unwrap();
    std::mem::forget(rx);
    let auth = founder_user_key(&f)
        .sign_request(&IpcRequest::SessionOpen {
            node: "alpha".into(),
            kind: terminal("echo stranger"),
            reason: "INC-3".into(),
        })
        .unwrap();
    let message = replay(
        &stranger,
        mesh_proto::SessionSpec {
            kind: terminal("echo stranger"),
            auth,
        },
    )
    .await;
    assert!(message.contains("not a member"), "{message}");

    // And the ordinary path still opens a terminal on beta.
    let mut s = open(&f, "beta", terminal("echo named")).await;
    let (out, outcome) = drain(&mut s).await;
    assert_eq!(outcome, Ok(0), "{out:?}");
    assert!(out.contains("named"), "{out:?}");
    stranger.close();
    drop(beta);
}

/// SEC-11: a name two machines share opens neither.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_name_is_refused_at_the_origin() {
    let f = permissive("fabric-shared-name").await;
    let twin = join_new_node("alpha", &f, vec![]).await;
    wait_reachable(&f, &twin).await;
    let mut s = open(&f, "alpha", terminal("id")).await;
    let message = refusal(&mut s).await;
    assert!(message.contains("2 machines"), "{message}");
    // By id it is unambiguous.
    let id = f.node.node_id().to_display_string();
    let mut s = open(&f, &id, terminal("echo by-id")).await;
    let (out, outcome) = drain(&mut s).await;
    assert_eq!(outcome, Ok(0), "{out:?}");
    assert!(out.contains("by-id"), "{out:?}");
    drop(twin);
}

/// The exit code comes back, so `meshnode shell node -- cmd` is scriptable.
#[tokio::test]
async fn the_exit_code_comes_back() {
    let f = permissive("fabric-exit").await;
    let mut s = open(&f, "alpha", terminal("exit 7")).await;
    assert_eq!(drain(&mut s).await.1, Ok(7));
}

// ---------------------------------------------------------------------------
// Off unless three separate things say yes
// ---------------------------------------------------------------------------

/// **The out-of-the-box posture.** An agent installed to run jobs is not an
/// agent somebody can get a prompt through.
#[tokio::test]
async fn a_node_refuses_terminals_until_its_operator_enables_them() {
    let f = bare("fabric-default-off", vec![], |_| {}).await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    let mut s = open(&f, "alpha", terminal("id")).await;
    let message = refusal(&mut s).await;
    assert!(message.contains("does not accept terminals"), "{message}");
    assert!(message.contains("accept"), "{message}");
}

/// The switch is live (ENG-11): a reload that turns terminals off refuses
/// the next one, and one that turns them on again accepts it. The check read
/// the boot config, so an operator who switched terminals off and reloaded
/// kept granting them until the next restart.
#[tokio::test]
async fn a_reload_turns_terminals_off_and_on_again() {
    let f = permissive("fabric-reload").await;
    let mut s = open(&f, "alpha", terminal("echo in")).await;
    assert_eq!(drain(&mut s).await.1, Ok(0));

    let mut off = (*f.node.live_config()).clone();
    off.sessions.accept = false;
    f.node.apply_config(&off);
    let mut s = open(&f, "alpha", terminal("id")).await;
    let message = refusal(&mut s).await;
    assert!(message.contains("does not accept terminals"), "{message}");

    let mut on = (*f.node.live_config()).clone();
    on.sessions.accept = true;
    f.node.apply_config(&on);
    let mut s = open(&f, "alpha", terminal("echo back")).await;
    assert_eq!(drain(&mut s).await.1, Ok(0));
}

/// Enabling the node is not enough. Without a policy nobody has a terminal
/// — which is what makes the entity fail *closed*: a node that cannot
/// decode a newer policy grants nothing, and grants are the only thing a
/// policy carries.
#[tokio::test]
async fn enabling_a_node_grants_nobody_a_terminal_without_a_policy() {
    let f = bare("fabric-no-policy", vec![], |cfg| cfg.sessions.accept = true).await;
    let mut s = open(&f, "alpha", terminal("id")).await;
    assert!(refusal(&mut s).await.contains("no session policy"));
}

/// A policy scoped to other nodes does not reach this one.
#[tokio::test]
async fn a_policy_that_does_not_cover_this_node_grants_nothing_here() {
    let f = bare("fabric-policy-nodes", vec![], |cfg| {
        cfg.sessions.accept = true
    })
    .await;
    permit(&f, "dev-only", "*", "tag=dev", &[SessionPermission::Shell]).await;
    let mut s = open(&f, "alpha", terminal("id")).await;
    assert!(refusal(&mut s).await.contains("no session policy"));
}

/// …nor to other people, and the same policy does reach somebody it names.
#[tokio::test]
async fn a_policy_reaches_only_the_users_it_names() {
    let f = bare("fabric-policy-users", vec![], |cfg| {
        cfg.sessions.accept = true
    })
    .await;
    permit(&f, "ops", "ops-*", "any", &[SessionPermission::Shell]).await;

    let ada = Keypair::generate().unwrap();
    add_user(&f, &ada, "ada", Role::Operator).await;
    let mut s = open_as(&f, &ada, "alpha", terminal("id"), "INC-1").await;
    assert!(refusal(&mut s).await.contains("no session policy"));

    let bot = Keypair::generate().unwrap();
    add_user(&f, &bot, "ops-bot", Role::Operator).await;
    let mut s = open_as(&f, &bot, "alpha", terminal("echo in"), "INC-1").await;
    assert_eq!(drain(&mut s).await.1, Ok(0));
}

/// **A policy is necessary, not sufficient** (§3.4): a viewer permitted by
/// a wide-open policy still cannot get a prompt, because they could not run
/// a command here either. Nobody gains a terminal by this entity existing.
#[tokio::test]
async fn a_policy_cannot_hand_a_terminal_to_someone_who_cannot_run_here() {
    let f = permissive("fabric-grant").await;
    let vera = Keypair::generate().unwrap();
    add_user(&f, &vera, "vera", Role::Viewer).await;
    let mut s = open_as(&f, &vera, "alpha", terminal("id"), "INC-1").await;
    let message = refusal(&mut s).await;
    assert!(message.contains("runner grant"), "{message}");
}

/// An expired policy stops granting, without anyone having to retire it.
#[tokio::test]
async fn an_expired_policy_stops_granting() {
    let f = bare("fabric-expired", vec![], |cfg| cfg.sessions.accept = true).await;
    let admin = founder_user_key(&f);
    let mut policy = SessionPolicy {
        id: "yesterday".into(),
        description: String::new(),
        users: "*".into(),
        nodes: "any".into(),
        allow: vec![SessionPermission::Shell],
        expires_at_micros: Some(mesh_proto::now_micros() - 1_000_000),
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        signature: Vec::new(),
    };
    policy.signature = admin.sign(&policy.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(
            &f,
            admin,
            IpcRequest::SessionPolicyPut {
                policy: Box::new(policy)
            }
        )
        .await,
        IpcResponse::Ok
    ));
    let mut s = open(&f, "alpha", terminal("id")).await;
    assert!(refusal(&mut s).await.contains("no session policy"));
}

/// A retired policy stops granting too.
#[tokio::test]
async fn a_retired_policy_stops_granting() {
    let f = permissive("fabric-retire").await;
    let admin = founder_user_key(&f);
    let by = admin.user_id();
    let signature = admin
        .sign(&mesh_proto::session_policy_remove_signing_bytes("all", &by))
        .to_vec();
    assert!(matches!(
        ipc_as(
            &f,
            admin,
            IpcRequest::SessionPolicyRemove {
                id: "all".into(),
                by,
                signature
            }
        )
        .await,
        IpcResponse::Ok
    ));
    let mut s = open(&f, "alpha", terminal("id")).await;
    assert!(refusal(&mut s).await.contains("no session policy"));
}

/// A policy that permits nothing is refused when it is written, rather than
/// puzzled over later.
#[tokio::test]
async fn an_inert_policy_is_refused_when_it_is_written() {
    let f = permissive("fabric-inert").await;
    let admin = founder_user_key(&f);
    let mut policy = SessionPolicy {
        id: "empty".into(),
        description: String::new(),
        users: "*".into(),
        nodes: "any".into(),
        allow: Vec::new(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        created_by: admin.user_id(),
        signature: Vec::new(),
    };
    policy.signature = admin.sign(&policy.signing_bytes()).to_vec();
    match ipc_as(
        &f,
        admin,
        IpcRequest::SessionPolicyPut {
            policy: Box::new(policy),
        },
    )
    .await
    {
        IpcResponse::Error { message } => assert!(message.contains("permits nothing"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Only an admin may write one — a policy an operator could publish is a
/// policy an operator could grant themselves a prompt with.
#[tokio::test]
async fn only_an_admin_may_write_a_session_policy() {
    let f = permissive("fabric-policy-rbac").await;
    let ada = Keypair::generate().unwrap();
    add_user(&f, &ada, "ada", Role::Operator).await;
    let mut policy = SessionPolicy {
        id: "mine".into(),
        description: String::new(),
        users: "ada".into(),
        nodes: "any".into(),
        allow: vec![SessionPermission::Shell],
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        created_by: ada.user_id(),
        signature: Vec::new(),
    };
    policy.signature = ada.sign(&policy.signing_bytes()).to_vec();
    match ipc_as(
        &f,
        &ada,
        IpcRequest::SessionPolicyPut {
            policy: Box::new(policy),
        },
    )
    .await
    {
        IpcResponse::Denied { .. } => {}
        other => panic!("an operator must not write a session policy, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The reason, and the audit trail
// ---------------------------------------------------------------------------

/// A terminal without a stated reason is refused. It is the first column an
/// auditor reads, so it cannot be optional.
#[tokio::test]
async fn a_terminal_without_a_reason_is_refused() {
    let f = permissive("fabric-reason").await;
    let mut s = open_as(&f, founder_user_key(&f), "alpha", terminal("id"), "   ").await;
    assert!(refusal(&mut s).await.contains("needs a reason"));
}

/// **There is one audit log, and a terminal is in it** — with who, where,
/// which OS account, which session, and why.
#[tokio::test]
async fn a_terminal_is_audited_with_its_reason_and_os_account() {
    let f = permissive("fabric-audit").await;
    let mut s = open_as(
        &f,
        founder_user_key(&f),
        "alpha",
        terminal("true"),
        "INC-4821",
    )
    .await;
    let _ = drain(&mut s).await;

    let found = wait_until(Duration::from_secs(10), || {
        f.node.engine().read(|st| {
            st.audit.values().any(|e| {
                e.action == "shell.open"
                    && e.detail.contains("INC-4821")
                    && e.detail.contains("as ")
                    && e.detail.contains("session ")
            }) && st.audit.values().any(|e| e.action == "shell.close")
        })
    })
    .await;
    assert!(found, "the terminal left no usable audit trail");
}

/// A refusal is audited too. An auditor who sees only the sessions that
/// succeeded is watching half the story.
#[tokio::test]
async fn a_refused_terminal_is_audited_as_denied() {
    let f = permissive("fabric-audit-deny").await;
    let vera = Keypair::generate().unwrap();
    add_user(&f, &vera, "vera", Role::Viewer).await;
    let mut s = open_as(&f, &vera, "alpha", terminal("id"), "INC-1").await;
    let _ = refusal(&mut s).await;

    let logged = wait_until(Duration::from_secs(10), || {
        f.node.engine().read(|st| {
            st.audit
                .values()
                .any(|e| e.action == "shell.open" && e.outcome.starts_with("denied"))
        })
    })
    .await;
    assert!(logged, "a refusal left no audit trail");
}

// ---------------------------------------------------------------------------
// Node-side controls
// ---------------------------------------------------------------------------

/// `no-shell` is one notch narrower than `no-execute`: this node still runs
/// jobs, it is simply not a machine people get prompts on. Replicated, so
/// an admin carves out production from anywhere rather than editing a TOML
/// file on six hundred boxes.
#[tokio::test]
async fn a_no_shell_node_still_runs_jobs_and_refuses_prompts() {
    let f = bare(
        "fabric-no-shell",
        vec![mesh_proto::NO_SHELL_TAG.to_string()],
        |cfg| cfg.sessions.accept = true,
    )
    .await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    let mut s = open(&f, "alpha", terminal("id")).await;
    let message = refusal(&mut s).await;
    assert!(message.contains("no-shell"), "{message}");
    assert!(
        f.node.engine().read(|st| !st.members[&f.node.node_id()]
            .info
            .tags
            .iter()
            .any(|t| t == mesh_proto::NO_EXECUTE_TAG)),
        "no-shell must not imply no-execute"
    );
}

/// `no-execute` refuses a prompt too — it does not run processes at all.
#[tokio::test]
async fn a_no_execute_node_refuses_a_prompt() {
    let f = bare(
        "fabric-no-exec",
        vec![mesh_proto::NO_EXECUTE_TAG.to_string()],
        |cfg| cfg.sessions.accept = true,
    )
    .await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    let mut s = open(&f, "alpha", terminal("id")).await;
    assert!(refusal(&mut s).await.contains("no-execute"));
}

/// An unknown target is named rather than hung on.
#[tokio::test]
async fn an_unknown_member_is_refused_by_name() {
    let f = permissive("fabric-unknown").await;
    let mut s = open(&f, "nowhere", terminal("true")).await;
    assert!(refusal(&mut s).await.contains("nowhere"));
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// A terminal nobody is using does not stay open forever, and the refusal
/// names the setting rather than merely saying "closed".
#[tokio::test]
async fn the_idle_timeout_closes_a_quiet_terminal_and_says_so() {
    let f = bare("fabric-idle", vec![], |cfg| {
        cfg.sessions.accept = true;
        cfg.sessions.limits = mesh_core::sessions::Limits {
            idle: Some(Duration::from_millis(600)),
            max: None,
        };
    })
    .await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;

    let mut s = open(&f, "alpha", terminal("sleep 30")).await;
    let (_, outcome) = tokio::time::timeout(Duration::from_secs(20), drain(&mut s))
        .await
        .expect("an idle terminal must not hang");
    let message = outcome.expect_err("an idle terminal ends with a message");
    assert!(message.contains("idle_timeout"), "{message}");
}

/// …and a busy one does not stay open forever either. The command below is
/// chatty, so the idle clock would never fire even if it were set — this is
/// the maximum-duration cap and nothing else.
#[tokio::test]
async fn the_maximum_duration_closes_even_a_busy_terminal() {
    let f = bare("fabric-max", vec![], |cfg| {
        cfg.sessions.accept = true;
        cfg.sessions.limits = mesh_core::sessions::Limits {
            idle: None,
            max: Some(Duration::from_millis(700)),
        };
    })
    .await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;

    let mut s = open(
        &f,
        "alpha",
        terminal("while true; do echo tick; sleep 0.1; done"),
    )
    .await;
    let (_, outcome) = tokio::time::timeout(Duration::from_secs(20), drain(&mut s))
        .await
        .expect("a capped terminal must not hang");
    let message = outcome.expect_err("a capped terminal ends with a message");
    assert!(message.contains("max_duration"), "{message}");
}

/// SEC-8: a shell that ignores `TERM` does not outlive its session. It
/// used to be sent `TERM` and nothing else, so `trap '' TERM` kept it
/// running as the daemon after its maximum duration — and after every
/// other way a session is closed.
#[tokio::test]
async fn a_shell_that_ignores_term_still_ends() {
    let f = bare("fabric-trap", vec![], |cfg| {
        cfg.sessions.accept = true;
        cfg.sessions.limits = mesh_core::sessions::Limits {
            idle: None,
            max: Some(Duration::from_millis(700)),
        };
    })
    .await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let command = format!(
        "trap '' TERM; echo $$ > '{}'; while true; do echo tick; sleep 0.1; done",
        pidfile.display()
    );
    let mut s = open(&f, "alpha", terminal(&command)).await;
    let (_, outcome) = tokio::time::timeout(Duration::from_secs(20), drain(&mut s))
        .await
        .expect("a capped terminal must not hang");
    assert!(outcome.is_err(), "the cap did not close the session");
    let pid = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .to_string();
    let proc_dir = std::path::PathBuf::from(format!("/proc/{pid}"));
    assert!(
        wait_until(Duration::from_secs(10), || !proc_dir.exists()).await,
        "the shell that ignored TERM is still running as pid {pid}"
    );
}

/// The per-peer cap refuses and names the setting.
#[tokio::test]
async fn the_per_peer_cap_refuses_and_names_the_setting() {
    let f = bare("fabric-cap", vec![], |cfg| {
        cfg.sessions.accept = true;
        cfg.sessions.max_per_peer = 1;
    })
    .await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;

    let mut first = open(&f, "alpha", terminal("sleep 30")).await;
    assert!(
        wait_until(Duration::from_secs(10), || f.node.session_rows().len() == 1).await,
        "the first terminal never started"
    );
    let mut second = open(&f, "alpha", terminal("true")).await;
    assert!(refusal(&mut second).await.contains("max_per_peer"));
    let _ = write_frame(
        &mut first,
        &SessionFrame::Signal(mesh_proto::SessionSignal::Kill),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Live sessions, and what a drain does to them
// ---------------------------------------------------------------------------

/// A live terminal is visible, carries its reason and OS account, and is
/// **not** a job: no `JobSummary` replicates for it and no executor slot is
/// held.
#[tokio::test]
async fn a_live_terminal_is_listed_with_its_reason_and_is_not_a_job() {
    let f = permissive("fabric-list").await;
    let jobs_before = f.node.engine().read(|s| s.jobs.len());

    let mut s = open_as(
        &f,
        founder_user_key(&f),
        "alpha",
        terminal("sleep 4"),
        "INC-9",
    )
    .await;
    assert!(
        wait_until(Duration::from_secs(10), || f.node.session_rows().len() == 1).await,
        "the terminal never appeared in the registry"
    );

    let rows = f.node.session_rows();
    assert_eq!(rows[0].kind, "shell");
    assert!(rows[0].hosting);
    assert_eq!(rows[0].reason, "INC-9");
    assert!(!rows[0].os_user.is_empty(), "the OS account is recorded");
    assert_eq!(f.node.engine().read(|s| s.jobs.len()), jobs_before);

    // And it deregisters when the far shell ends — the guard's `Drop`, on
    // every exit path.
    let _ = drain(&mut s).await;
    assert!(
        wait_until(Duration::from_secs(15), || f.node.session_rows().is_empty()).await,
        "the terminal outlived its stream"
    );
}

/// §3.9: **a drain does not wait for terminals.** It closes them with a
/// message naming the node and the reason — recorded here because the
/// opposite behaviour is the tempting one, and it turns `meshnode drain`
/// into a command that sometimes never returns.
#[tokio::test]
async fn a_drain_closes_a_live_terminal_instead_of_waiting_for_it() {
    let f = permissive("fabric-drain").await;
    let mut s = open(&f, "alpha", terminal("sleep 30")).await;
    assert!(
        wait_until(Duration::from_secs(10), || f.node.session_rows().len() == 1).await,
        "the terminal never started"
    );

    assert_eq!(
        f.node
            .sessions()
            .close_hosted("alpha is draining and closed this session"),
        1
    );
    let (_, outcome) = tokio::time::timeout(Duration::from_secs(10), drain(&mut s))
        .await
        .expect("a closed terminal must not hang");
    assert!(
        outcome
            .expect_err("ends with a message")
            .contains("draining")
    );
}

/// An operator can end somebody else's terminal.
#[tokio::test]
async fn an_operator_can_close_a_live_terminal() {
    let f = permissive("fabric-close").await;
    let mut s = open(&f, "alpha", terminal("sleep 30")).await;
    assert!(
        wait_until(Duration::from_secs(10), || f.node.session_rows().len() == 1).await,
        "the terminal never started"
    );
    let id = f.node.session_rows()[0].id;
    assert!(matches!(
        ipc_as(&f, founder_user_key(&f), IpcRequest::SessionClose { id }).await,
        IpcResponse::Ok
    ));
    let (_, outcome) = tokio::time::timeout(Duration::from_secs(10), drain(&mut s))
        .await
        .expect("a closed terminal must not hang");
    assert!(
        outcome.is_err(),
        "a terminated terminal ends with a message"
    );
}

// ---------------------------------------------------------------------------
// Scope, and compatibility
// ---------------------------------------------------------------------------

/// **v1 is the terminal and nothing else.** The other kinds are still on the
/// wire at their existing indices — removing a variant is a protocol change
/// and re-adding one later would be a second — and the host refuses them by
/// name.
#[tokio::test]
async fn the_deferred_session_kinds_are_refused_by_name() {
    let f = permissive("fabric-scope").await;
    for kind in [
        SessionKind::Exec {
            command: "id".into(),
        },
        SessionKind::CpFrom {
            path: "/etc/hostname".into(),
        },
        SessionKind::Forward {
            host: "localhost".into(),
            port: 80,
        },
    ] {
        let mut s = open(&f, "alpha", kind.clone()).await;
        let message = refusal(&mut s).await;
        assert!(message.contains("terminal only"), "{kind:?}: {message}");
    }
}

/// A peer below 1.9 is **named**, not hung on. A 1.8 node would fail to
/// decode `SessionOpen` and drop the stream, which from the initiator's end
/// is indistinguishable from a peer that never answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_19_peer_is_refused_by_version() {
    let f = bare("fabric-skew", vec![], |cfg| cfg.sessions.accept = true).await;
    permit(&f, "all", "*", "any", &[SessionPermission::Shell]).await;
    let old = mesh_testkit::join_new_node_with("beta", &f, vec![], |cfg| {
        cfg.proto_minor_override = Some(mesh_proto::INVENTORY_MINOR);
    })
    .await;
    wait_reachable(&f, &old).await;

    let mut s = open(&f, "beta", terminal("true")).await;
    let message = refusal(&mut s).await;
    assert!(message.contains("1.9"), "{message}");
    assert!(message.contains("beta"), "{message}");
    drop(old);
}

/// The signature covers **what the user asked for, and why** (§3.4).
/// Without this check a compromised origin could forward a valid envelope
/// and open a different session with it, or put a different reason in the
/// operator's mouth.
#[test]
fn a_session_spec_must_match_the_envelope_it_carries() {
    let user = Keypair::generate().unwrap();
    let asked = terminal("id");
    let envelope = user
        .sign_request(&IpcRequest::SessionOpen {
            node: "hermes".into(),
            kind: asked.clone(),
            reason: "INC-1".into(),
        })
        .unwrap();
    let decoded: IpcRequest = postcard::from_bytes(&envelope.payload).unwrap();
    let IpcRequest::SessionOpen { kind, reason, .. } = decoded else {
        panic!("not a session request");
    };
    assert_eq!(kind, asked);
    assert_eq!(reason, "INC-1");
}

// ---------------------------------------------------------------------------
// Terminal readiness is visible from anywhere
// ---------------------------------------------------------------------------

/// **A node's terminal acceptance is its own statement, and every other
/// node can read it.**
///
/// `[sessions] accept` lives in one machine's config file, which made "can
/// anyone get a prompt on that box" a question you could only answer by
/// logging into the box — the exact thing the feature exists to avoid. So a
/// node announces it as a runtime-managed `cap.` tag, the same family and
/// the same guarantees as `cap.cores`: self-described, replicated, and
/// refused from hand-editing.
#[tokio::test(flavor = "multi_thread")]
async fn a_nodes_terminal_acceptance_is_visible_from_other_nodes() {
    let f = bare("fabric-cap-term", vec![], |cfg| cfg.sessions.accept = true).await;
    let beta = mesh_testkit::join_new_node_with("beta", &f, vec![], |cfg| {
        cfg.sessions.accept = false;
    })
    .await;
    wait_reachable(&f, &beta).await;

    let rows = match ipc_as(&f, founder_user_key(&f), IpcRequest::Nodes).await {
        IpcResponse::Nodes(rows) => rows,
        other => panic!("unexpected: {other:?}"),
    };
    let row = |name: &str| {
        rows.iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no row for {name}: {rows:?}"))
            .clone()
    };
    assert!(row("alpha").accepts_terminals, "alpha has terminals on");
    assert!(!row("beta").accepts_terminals, "beta has them off");

    // …and it is not in the user-facing tag list, because nobody set it by
    // hand and nobody should be able to.
    assert!(
        !row("alpha").tags.iter().any(|t| t.starts_with("cap.")),
        "cap tags are parsed out of the user tag list: {:?}",
        row("alpha").tags
    );
    drop(beta);
}

/// An operator cannot assert it on their own behalf — it is a `cap.` tag,
/// and those are refused from `nodes tag` exactly as `cap.cores` is.
#[tokio::test]
async fn a_node_cannot_hand_edit_its_terminal_acceptance() {
    let f = bare("fabric-cap-forge", vec![], |_| {}).await;
    match ipc_as(
        &f,
        founder_user_key(&f),
        IpcRequest::TagsUpdate {
            add: vec![mesh_proto::term_tag()],
            remove: Vec::new(),
        },
    )
    .await
    {
        IpcResponse::Error { message } => assert!(message.contains("cap."), "{message}"),
        other => panic!("a cap tag must be refused by hand, got {other:?}"),
    }
    // The node still says what it really is: terminals are off here.
    let rows = match ipc_as(&f, founder_user_key(&f), IpcRequest::Nodes).await {
        IpcResponse::Nodes(rows) => rows,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(!rows[0].accepts_terminals);
}

/// `no-shell` is an ordinary tag, so it *can* be set by the node's own
/// operator — and that is the point: it is the machine's veto, replicated
/// so every other node honours it without anybody editing a file there.
#[tokio::test]
async fn no_shell_is_settable_by_the_node_and_replicates() {
    let f = permissive("fabric-noshell-tag").await;
    let mut s = open(&f, "alpha", terminal("echo before")).await;
    assert_eq!(
        drain(&mut s).await.1,
        Ok(0),
        "a terminal works to begin with"
    );

    assert!(matches!(
        ipc_as(
            &f,
            founder_user_key(&f),
            IpcRequest::TagsUpdate {
                add: vec![mesh_proto::NO_SHELL_TAG.to_string()],
                remove: Vec::new(),
            },
        )
        .await,
        IpcResponse::Status(_) | IpcResponse::Ok
    ));

    let tagged = wait_until(Duration::from_secs(10), || {
        f.node.engine().read(|st| {
            st.members[&f.node.node_id()]
                .info
                .tags
                .iter()
                .any(|t| t == mesh_proto::NO_SHELL_TAG)
        })
    })
    .await;
    assert!(tagged, "the tag never landed");

    let mut s = open(&f, "alpha", terminal("echo after")).await;
    assert!(refusal(&mut s).await.contains("no-shell"));
}
