//! M4 engine behavior: LAN pairing (code confirmation, decline paths) and
//! fan-out execution with the no-execute capability.
//!
//! Pairing runs mDNS-free here (the joiner reports its bound endpoint via
//! `on_listening`); discovery is exercised by the CLI/Director on real LANs.

use std::time::Duration;

use mesh_core::config::{NodeConfig, Overrides};
use mesh_core::daemon::Node;
use mesh_core::membership::{self, InviteParams};
use mesh_core::pairing::{self, JoinerUx, PairEvent};
use mesh_proto::{IpcRequest, IpcResponse, JobState, Liveness};
use mesh_testkit::{TestNode, founder_user_key, join_new_node, spawn_founder, wait_until};
use tokio::sync::{mpsc, oneshot};

fn joiner_config(dir: &std::path::Path, name: &str) -> NodeConfig {
    NodeConfig::load(
        None,
        &Overrides {
            data_dir: Some(dir.to_path_buf()),
            listen: Some("127.0.0.1:0".to_string()),
            node_name: Some(name.to_string()),
            mesh_name: Some("ignored-will-pair".to_string()),
            tags: Some(vec!["paired".into()]),
            advertise: None,
        },
    )
    .expect("joiner config")
}

fn mint_pair_invite(founder: &TestNode) -> mesh_proto::InviteToken {
    membership::create_invite(
        founder.node.engine(),
        founder_user_key(founder),
        InviteParams {
            ttl_secs: 600,
            uses: 1,
            tags: vec![],
            bootstrap: vec![founder.addr_string()],
        },
    )
    .expect("mint invite")
}

/// Drive both sides of a pairing exchange with scripted human verdicts.
/// Returns (joiner result, approver result, the codes both sides displayed).
async fn run_pairing(
    founder: &TestNode,
    joiner_name: &str,
    joiner_says: bool,
    approver_says: bool,
) -> (
    mesh_core::Result<membership::JoinReport>,
    mesh_core::Result<pairing::PairOutcome>,
    Option<(String, String)>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = joiner_config(dir.path(), joiner_name);

    let (code_tx, mut code_rx) = mpsc::channel(1);
    let (jdec_tx, jdec_rx) = oneshot::channel();
    let (listen_tx, listen_rx) = oneshot::channel();
    let joiner = tokio::spawn({
        let config = config.clone();
        async move {
            pairing::pair_join(
                &config,
                JoinerUx {
                    on_code: code_tx,
                    decision: jdec_rx,
                    on_listening: Some(listen_tx),
                    person: None,
                    on_announced: None,
                },
                Some(Duration::from_secs(30)),
            )
            .await
        }
    });
    let addr = listen_rx.await.expect("joiner reports its endpoint");

    let token = mint_pair_invite(founder);
    let (ev_tx, mut ev_rx) = mpsc::channel(4);
    let (adec_tx, adec_rx) = oneshot::channel();
    let approver = tokio::spawn({
        let transport = founder.node.transport().clone();
        let engine = founder.node.engine().clone();
        async move { pairing::pair_approve(&transport, &engine, addr, token, ev_tx, adec_rx).await }
    });

    // Both sides display a code; the scripted humans answer.
    let joiner_prompt = tokio::time::timeout(Duration::from_secs(10), code_rx.recv())
        .await
        .expect("joiner code within 10s");
    let approver_event = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
        .await
        .expect("approver code within 10s");
    let codes = match (&joiner_prompt, &approver_event) {
        (Some(prompt), Some(PairEvent::Code { code, .. })) => {
            Some((prompt.code.clone(), code.clone()))
        }
        _ => None,
    };
    let _ = adec_tx.send(approver_says);
    let _ = jdec_tx.send(joiner_says);

    let approver_result = approver.await.expect("approver task");
    let joiner_result = joiner.await.expect("joiner task");
    // Keep the tempdir alive until both sides finished.
    drop(dir);
    (joiner_result, approver_result, codes)
}

#[tokio::test]
async fn pairing_with_matching_codes_joins_the_mesh() {
    let founder = spawn_founder("alpha", "pairmesh").await;
    let (joiner, approver, codes) = run_pairing(&founder, "beta", true, true).await;

    let (jcode, acode) = codes.expect("both sides displayed a code");
    assert_eq!(jcode, acode, "both humans must see the same 6 digits");
    assert_eq!(jcode.len(), 6);

    let outcome = approver.expect("approver side succeeds");
    let report = joiner.expect("joiner side succeeds");
    assert_eq!(report.mesh_name, "pairmesh");
    assert_eq!(report.accepted_by, founder.node.node_id());

    // The founder's state has the new member, tagged from its config.
    let member = founder
        .node
        .engine()
        .read(|s| s.members.get(&outcome.node_id).map(|m| m.info.clone()))
        .expect("paired node is in state");
    assert_eq!(member.name, "beta");
    assert!(member.tags.contains(&"paired".to_string()));

    founder.node.shutdown();
}

#[tokio::test]
async fn pairing_declined_by_joiner_admits_nothing() {
    let founder = spawn_founder("alpha", "pairmesh").await;
    let before = founder.node.engine().read(|s| s.members.len());
    let (joiner, approver, _) = run_pairing(&founder, "beta", false, true).await;

    assert!(joiner.is_err(), "joiner must not join after declining");
    assert!(approver.is_err(), "approver must fail when joiner declines");
    let after = founder.node.engine().read(|s| s.members.len());
    assert_eq!(before, after, "no membership was authored");

    founder.node.shutdown();
}

#[tokio::test]
async fn pairing_declined_by_approver_admits_nothing() {
    let founder = spawn_founder("alpha", "pairmesh").await;
    let before = founder.node.engine().read(|s| s.members.len());
    let (joiner, approver, _) = run_pairing(&founder, "beta", true, false).await;

    assert!(approver.is_err(), "approver declined");
    assert!(joiner.is_err(), "joiner must not end up joined");
    let after = founder.node.engine().read(|s| s.members.len());
    assert_eq!(before, after, "no membership was authored");

    founder.node.shutdown();
}

/// Start a cancellable joiner (DIR-6). Returns the task, its endpoint, the
/// code it will show, the decision it is waiting on (kept alive, so only a
/// cancel can answer it), and the cancel itself.
#[allow(clippy::type_complexity)]
async fn cancellable_joiner(
    config: &NodeConfig,
) -> (
    tokio::task::JoinHandle<mesh_core::Result<membership::JoinReport>>,
    std::net::SocketAddr,
    mpsc::Receiver<pairing::PairPrompt>,
    oneshot::Sender<bool>,
    oneshot::Sender<()>,
) {
    let (code_tx, code_rx) = mpsc::channel(1);
    let (jdec_tx, jdec_rx) = oneshot::channel();
    let (listen_tx, listen_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let joiner = tokio::spawn({
        let config = config.clone();
        async move {
            pairing::pair_join_until(
                &config,
                JoinerUx {
                    on_code: code_tx,
                    decision: jdec_rx,
                    on_listening: Some(listen_tx),
                    person: None,
                    on_announced: None,
                },
                Some(Duration::from_secs(30)),
                async move {
                    let _ = cancel_rx.await;
                },
            )
            .await
        }
    });
    let addr = listen_rx.await.expect("joiner reports its endpoint");
    (joiner, addr, code_rx, jdec_tx, cancel_tx)
}

#[tokio::test]
async fn a_cancelled_wait_ends_at_once_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = joiner_config(dir.path(), "beta");
    let (joiner, addr, _code_rx, _jdec_tx, cancel) = cancellable_joiner(&config).await;

    let _ = cancel.send(());
    let result = tokio::time::timeout(Duration::from_secs(5), joiner)
        .await
        .expect("a cancel ends the wait promptly, not at its deadline")
        .expect("joiner task");
    let err = result.expect_err("a cancelled pairing does not join");
    assert!(err.to_string().contains("cancelled"), "{err}");

    // Nothing was written, so any other way in still works on this dir…
    assert!(
        mesh_core::genesis::Genesis::load(&config.data_dir)
            .expect("read genesis")
            .is_none()
    );
    // …and the pairing endpoint let go of its port.
    let freed = wait_until(Duration::from_secs(5), || {
        std::net::UdpSocket::bind(addr).is_ok()
    })
    .await;
    assert!(freed, "the pairing endpoint still holds {addr}");
}

#[tokio::test]
async fn cancelling_with_the_code_on_screen_declines_the_approver() {
    let founder = spawn_founder("alpha", "pairmesh").await;
    let before = founder.node.engine().read(|s| s.members.len());
    let dir = tempfile::tempdir().expect("tempdir");
    let config = joiner_config(dir.path(), "beta");
    let (joiner, addr, mut code_rx, _jdec_tx, cancel) = cancellable_joiner(&config).await;

    let token = mint_pair_invite(&founder);
    let (ev_tx, mut ev_rx) = mpsc::channel(4);
    let (adec_tx, adec_rx) = oneshot::channel();
    let approver = tokio::spawn({
        let transport = founder.node.transport().clone();
        let engine = founder.node.engine().clone();
        async move { pairing::pair_approve(&transport, &engine, addr, token, ev_tx, adec_rx).await }
    });
    tokio::time::timeout(Duration::from_secs(10), code_rx.recv())
        .await
        .expect("joiner code within 10s")
        .expect("a code");
    tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
        .await
        .expect("approver code within 10s");

    // The admin approves; the person at the joiner walks away instead.
    let _ = adec_tx.send(true);
    let _ = cancel.send(());

    let approver = tokio::time::timeout(Duration::from_secs(10), approver)
        .await
        .expect("the approver hears back")
        .expect("approver task");
    let joiner = tokio::time::timeout(Duration::from_secs(10), joiner)
        .await
        .expect("the joiner ends")
        .expect("joiner task");
    assert!(approver.is_err(), "the approver must not see a join");
    assert!(joiner.is_err(), "the joiner must not end up joined");
    let after = founder.node.engine().read(|s| s.members.len());
    assert_eq!(before, after, "no membership was authored");

    founder.node.shutdown();
}

#[tokio::test]
async fn paired_node_participates_after_up() {
    let founder = spawn_founder("alpha", "pairmesh").await;

    let dir = tempfile::tempdir().expect("tempdir");
    let config = joiner_config(dir.path(), "beta");
    let (code_tx, mut code_rx) = mpsc::channel(1);
    let (jdec_tx, jdec_rx) = oneshot::channel();
    let (listen_tx, listen_rx) = oneshot::channel();
    let joiner = tokio::spawn({
        let config = config.clone();
        async move {
            pairing::pair_join(
                &config,
                JoinerUx {
                    on_code: code_tx,
                    decision: jdec_rx,
                    on_listening: Some(listen_tx),
                    person: None,
                    on_announced: None,
                },
                Some(Duration::from_secs(30)),
            )
            .await
        }
    });
    let addr = listen_rx.await.expect("endpoint");
    let token = mint_pair_invite(&founder);
    let (ev_tx, mut ev_rx) = mpsc::channel(4);
    let (adec_tx, adec_rx) = oneshot::channel();
    let transport = founder.node.transport().clone();
    let engine = founder.node.engine().clone();
    let approver = tokio::spawn(async move {
        pairing::pair_approve(&transport, &engine, addr, token, ev_tx, adec_rx).await
    });
    let _ = code_rx.recv().await;
    let _ = ev_rx.recv().await;
    let _ = adec_tx.send(true);
    let _ = jdec_tx.send(true);
    approver.await.expect("task").expect("approve ok");
    joiner.await.expect("task").expect("join ok");

    // The paired data dir boots as a full member and gossips.
    let node = Node::start(config).await.expect("paired node starts");
    let founder_gossip = founder.node.gossip().clone();
    let beta_id = node.node_id();
    let alive = wait_until(Duration::from_secs(10), || {
        matches!(founder_gossip.liveness_of(&beta_id), Liveness::Alive)
    })
    .await;
    assert!(alive, "paired node should gossip as alive after `up`");

    node.shutdown();
    founder.node.shutdown();
}

// ---------------------------------------------------------------------------
// Fan-out (MASTERPLAN §9.3, M4 slice)
// ---------------------------------------------------------------------------

/// Drive a fan-out run to completion; returns (per-index stdout, per-index
/// terminal state, FanoutDone counts).
async fn collect_fanout(
    node: &TestNode,
    command: &str,
    on: Option<String>,
    fanout: u32,
) -> (
    std::collections::BTreeMap<u32, String>,
    std::collections::BTreeMap<u32, JobState>,
    (u32, u32),
    Vec<String>,
) {
    let req = IpcRequest::Run {
        command: command.to_string(),
        on,
        env: vec![],
        timeout_secs: Some(60),
        fanout: Some(fanout),
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let mut rx = mesh_testkit::stream_as(node, founder_user_key(node), req).await;
    let mut stdout: std::collections::BTreeMap<u32, String> = Default::default();
    let mut states: std::collections::BTreeMap<u32, JobState> = Default::default();
    let mut names: Vec<String> = Vec::new();
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::FanoutStarted { targets, .. } => {
                names = targets.into_iter().map(|t| t.node_name).collect();
            }
            IpcResponse::FanChunk {
                index,
                stream: 1,
                data,
            } => {
                stdout
                    .entry(index)
                    .or_default()
                    .push_str(&String::from_utf8_lossy(&data));
            }
            IpcResponse::FanChunk { index, data, .. } => {
                eprintln!("[fan {index} stderr] {}", String::from_utf8_lossy(&data));
            }
            IpcResponse::FanJobDone { index, state, .. } => {
                states.insert(index, state);
            }
            IpcResponse::FanoutDone {
                succeeded, failed, ..
            } => {
                return (stdout, states, (succeeded, failed), names);
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                panic!("fan-out failed: {message}");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    panic!("stream ended without FanoutDone");
}

/// Wait until the submitter can actually dispatch to both workers: alive
/// *and* their real (post-`up`) addresses have gossiped in — a joiner's
/// record has no dialable address until its boot-time MemberUpdate lands.
async fn wait_three_alive(a: &TestNode, b: &TestNode, c: &TestNode) {
    let engine = a.node.engine().clone();
    let gossip = a.node.gossip().clone();
    let (idb, idc) = (b.node.node_id(), c.node.node_id());
    let (addr_b, addr_c) = (b.addr_string(), c.addr_string());
    let ok = wait_until(Duration::from_secs(15), || {
        engine.read(|s| {
            s.is_active_member(&idb)
                && s.is_active_member(&idc)
                && s.members[&idb].info.addrs.contains(&addr_b)
                && s.members[&idc].info.addrs.contains(&addr_c)
        }) && matches!(gossip.liveness_of(&idb), Liveness::Alive)
            && matches!(gossip.liveness_of(&idc), Liveness::Alive)
    })
    .await;
    assert!(
        ok,
        "all members should be alive and addressable at the submitter"
    );
}

#[tokio::test]
async fn fanout_all_runs_on_every_node_with_attribution() {
    let a = spawn_founder("alpha", "fanmesh").await;
    let b = join_new_node("beta", &a, vec![]).await;
    let c = join_new_node("gamma", &a, vec![]).await;
    wait_three_alive(&a, &b, &c).await;

    let (stdout, states, (succeeded, failed), names) =
        collect_fanout(&a, "echo fan-hello", None, 0).await;

    assert_eq!(names.len(), 3, "all three nodes are targets");
    assert_eq!(succeeded, 3, "states were {states:?}, stdout {stdout:?}");
    assert_eq!(failed, 0);
    for index in 0..3u32 {
        assert_eq!(states.get(&index), Some(&JobState::Succeeded));
        assert!(
            stdout.get(&index).is_some_and(|s| s.contains("fan-hello")),
            "target {index} produced {stdout:?}"
        );
    }

    // Every sub-job carries the shared group in replicated state.
    let grouped = wait_until(Duration::from_secs(5), || {
        a.node.engine().read(|s| {
            let groups: std::collections::BTreeSet<_> = s
                .jobs
                .values()
                .filter_map(|j| j.fanout.map(|f| f.group))
                .collect();
            groups.len() == 1 && s.jobs.values().filter(|j| j.fanout.is_some()).count() == 3
        })
    })
    .await;
    assert!(grouped, "three sub-jobs share one FanId in state");

    a.node.shutdown();
    b.node.shutdown();
    c.node.shutdown();
}

#[tokio::test]
async fn fanout_count_limits_targets() {
    let a = spawn_founder("alpha", "fanmesh").await;
    let b = join_new_node("beta", &a, vec![]).await;
    let c = join_new_node("gamma", &a, vec![]).await;
    wait_three_alive(&a, &b, &c).await;

    let (_stdout, states, (succeeded, failed), names) = collect_fanout(&a, "true", None, 2).await;
    assert_eq!(names.len(), 2, "--fanout 2 targets exactly two nodes");
    assert_eq!(states.len(), 2);
    assert_eq!((succeeded, failed), (2, 0));

    a.node.shutdown();
    b.node.shutdown();
    c.node.shutdown();
}

// ---------------------------------------------------------------------------
// The no-execute capability (MASTERPLAN D6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_execute_nodes_are_never_placement_candidates() {
    let a = spawn_founder("alpha", "capmesh").await;
    let b = join_new_node("beta", &a, vec![mesh_proto::NO_EXECUTE_TAG.into()]).await;
    wait_until(Duration::from_secs(15), || {
        let idb = b.node.node_id();
        a.node.engine().read(|s| s.is_active_member(&idb))
            && matches!(a.node.gossip().liveness_of(&idb), Liveness::Alive)
    })
    .await;

    // Fan-out over everything: only alpha executes.
    let (_out, _states, (succeeded, failed), names) =
        collect_fanout(&a, "echo capable", None, 0).await;
    assert_eq!(names, vec!["alpha".to_string()]);
    assert_eq!((succeeded, failed), (1, 0));

    // Targeting the no-execute node directly finds no candidate.
    let req = IpcRequest::Run {
        command: "echo nope".into(),
        on: Some("name=beta".into()),
        env: vec![],
        timeout_secs: Some(10),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match mesh_testkit::run_to_end(&a, founder_user_key(&a), req).await {
        mesh_testkit::RunOutcome::Error(message) => {
            assert!(
                message.contains("no live node matches"),
                "unexpected error: {message}"
            );
        }
        other => panic!("expected an error, got {other:?}"),
    }

    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn tags_update_toggles_no_execute_live() {
    let a = spawn_founder("alpha", "togglemesh").await;

    // Toggle contribute-compute off (Director's switch does exactly this).
    let resp = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::TagsUpdate {
            add: vec![mesh_proto::NO_EXECUTE_TAG.into()],
            remove: vec![],
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "tags update: {resp:?}");

    let req = IpcRequest::Run {
        command: "echo blocked".into(),
        on: None,
        env: vec![],
        timeout_secs: Some(10),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match mesh_testkit::run_to_end(&a, founder_user_key(&a), req).await {
        mesh_testkit::RunOutcome::Error(message) => {
            assert!(message.contains("no live node matches"));
        }
        other => panic!("expected no candidates, got {other:?}"),
    }

    // Toggle back on: execution resumes.
    let resp = mesh_testkit::ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::TagsUpdate {
            add: vec![],
            remove: vec![mesh_proto::NO_EXECUTE_TAG.into()],
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok));
    let req = IpcRequest::Run {
        command: "echo unblocked".into(),
        on: None,
        env: vec![],
        timeout_secs: Some(10),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match mesh_testkit::run_to_end(&a, founder_user_key(&a), req).await {
        mesh_testkit::RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("unblocked"));
        }
        other => panic!("expected success, got {other:?}"),
    }

    a.node.shutdown();
}

/// The pairing endpoint binds the *configured* listen address, so the one
/// firewall rule a node needs for its daemon also admits the pairing dial —
/// a WAN pairing costs no temporary hole on an ephemeral port. When the port
/// is genuinely taken it falls back rather than refusing to pair at all.
#[tokio::test]
async fn pairing_binds_the_configured_listen_port() {
    /// A port nothing is listening on, released before we name it.
    fn free_port() -> u16 {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").expect("probe bind");
        s.local_addr().expect("probe addr").port()
    }

    /// Start a joiner and report the endpoint it actually bound.
    async fn bound_port(config: NodeConfig) -> u16 {
        let (code_tx, _code_rx) = mpsc::channel(1);
        let (_jdec_tx, jdec_rx) = oneshot::channel();
        let (listen_tx, listen_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            pairing::pair_join(
                &config,
                JoinerUx {
                    on_code: code_tx,
                    decision: jdec_rx,
                    on_listening: Some(listen_tx),
                    person: None,
                    on_announced: None,
                },
                Some(Duration::from_secs(5)),
            )
            .await
        });
        let addr = tokio::time::timeout(Duration::from_secs(10), listen_rx)
            .await
            .expect("joiner reports its endpoint")
            .expect("listen channel");
        task.abort();
        addr.port()
    }

    let wanted = free_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = joiner_config(dir.path(), "fixed-port");
    config.listen = format!("127.0.0.1:{wanted}").parse().expect("listen addr");
    assert_eq!(
        bound_port(config).await,
        wanted,
        "pairing should bind the configured port, not an ephemeral one"
    );

    // Same config, but something else already owns the port.
    let squatter = std::net::UdpSocket::bind("127.0.0.1:0").expect("squatter bind");
    let taken = squatter.local_addr().expect("squatter addr").port();
    let dir2 = tempfile::tempdir().expect("tempdir");
    let mut config2 = joiner_config(dir2.path(), "fallback-port");
    config2.listen = format!("127.0.0.1:{taken}").parse().expect("listen addr");
    let got = bound_port(config2).await;
    assert_ne!(got, taken, "should have fallen back off the taken port");
    assert_ne!(got, 0, "fallback still has to be dialable");
}

/// SEC-9: a name that could repaint a terminal never becomes a member's
/// name — refused at the join before the invite is spent, cleaned if an
/// op carries one anyway, and ignored in a re-announcement.
#[tokio::test]
async fn a_hazardous_name_is_refused_cleaned_or_ignored() {
    let founder = spawn_founder("NIM", "pair-names").await;
    let engine = founder.node.engine();
    let evil = "laptop\u{1b}[2A\r  123 456\u{202e}";

    // A join asking for it is refused, and the single-use invite survives.
    let token = mint_pair_invite(&founder);
    let joiner = mesh_core::identity::Keypair::generate().expect("key");
    let req = |name: &str| mesh_proto::JoinRequest {
        token: token.clone(),
        node_name: name.to_string(),
        listen_port: 7777,
        addrs: vec![],
        extra_tags: vec![],
        slots: 1,
    };
    let refused = membership::handle_join(
        engine,
        joiner.node_id(),
        "127.0.0.1".parse().expect("ip"),
        &req(evil),
    );
    assert!(refused.is_err(), "a hazardous name joined");
    membership::handle_join(
        engine,
        joiner.node_id(),
        "127.0.0.1".parse().expect("ip"),
        &req("laptop"),
    )
    .expect("the invite was spent by the refused join");

    // An op that carries one anyway (an acceptor that did not check) is
    // cleaned at merge, identically everywhere.
    let other = mesh_core::identity::Keypair::generate().expect("key");
    engine
        .author_op(mesh_proto::OpBody::MemberAdd {
            member: mesh_proto::MemberInfo {
                node_id: other.node_id(),
                name: evil.to_string(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
                joined_at_micros: mesh_proto::now_micros(),
            },
            invite: None,
        })
        .expect("add");
    let stored = engine.read(|s| s.members[&other.node_id()].info.name.clone());
    assert!(mesh_proto::text::is_clean(
        &stored,
        mesh_proto::text::MAX_NAME_CHARS
    ));

    // A member re-announcing itself with one keeps its old name.
    let me = founder.node.node_id();
    let before = engine.read(|s| s.members[&me].info.clone());
    engine
        .author_op(mesh_proto::OpBody::MemberUpdate {
            node_id: me,
            name: evil.to_string(),
            addrs: before.addrs.clone(),
            tags: before.tags.clone(),
            slots: before.slots,
        })
        .expect("update");
    assert_eq!(
        engine.read(|s| s.members[&me].info.name.clone()),
        before.name
    );

    // And a node is never configured with one in the first place.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = joiner_config(dir.path(), evil);
    assert!(config.check().is_err());

    founder.stop().await;
}

/// **An approver that goes quiet does not hold the joiner** (SEC-23).
///
/// Anyone on the LAN could dial a waiting joiner, send `PairInit`, and say
/// nothing more — and the real approver was refused as "already in
/// progress" for the whole ten-minute wait. An unconfirmed session now
/// gives way after thirty seconds.
#[tokio::test]
async fn a_silent_approver_gives_way_after_its_time() {
    use mesh_core::transport::{AdmissionCheck, BindOptions, LocalInfo, Transport};
    use mesh_proto::{PairInit, Request, Response};

    let founder = spawn_founder("alpha", "pair-quiet").await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = joiner_config(dir.path(), "beta");
    let (code_tx, mut code_rx) = mpsc::channel(4);
    let (jdec_tx, jdec_rx) = oneshot::channel();
    let (listen_tx, listen_rx) = oneshot::channel();
    let joiner = tokio::spawn({
        let config = config.clone();
        async move {
            pairing::pair_join(
                &config,
                JoinerUx {
                    on_code: code_tx,
                    decision: jdec_rx,
                    on_listening: Some(listen_tx),
                    person: None,
                    on_announced: None,
                },
                Some(Duration::from_secs(120)),
            )
            .await
        }
    });
    let addr = listen_rx.await.expect("joiner reports its endpoint");

    // Mallory starts a pairing and says nothing more.
    let key = mesh_core::identity::Keypair::generate().expect("key");
    let admission: AdmissionCheck = std::sync::Arc::new(|_, _| Ok(()));
    let (mallory, _events) = Transport::bind(
        "127.0.0.1:0".parse().expect("addr"),
        key.clone(),
        LocalInfo {
            node_id: key.node_id(),
            node_name: "mallory".into(),
            mesh_id: None,
            mesh_name: None,
        },
        admission,
        BindOptions::default(),
    )
    .await
    .expect("bind");
    let (conn, _) = mallory.connect_for_pair(addr).await.expect("dial");
    let init = Request::PairInit(PairInit {
        commitment: [0u8; 32],
        mesh_name: "not-yours".into(),
        approver_name: "mallory".into(),
    });
    assert!(matches!(
        mallory.request_one(&conn, &init).await.expect("init"),
        Response::PairAccept(_)
    ));

    // The real approver is turned away while the session is fresh…
    let (ev_tx, _ev_rx) = mpsc::channel(4);
    let (_adec_tx, adec_rx) = oneshot::channel();
    let err = pairing::pair_approve(
        founder.node.transport(),
        founder.node.engine(),
        addr,
        mint_pair_invite(&founder),
        ev_tx,
        adec_rx,
    )
    .await
    .expect_err("a second approver was let in at once");
    assert!(err.to_string().contains("already in progress"), "{err}");

    // …and let in once it has gone unconfirmed past its time.
    tokio::time::sleep(Duration::from_secs(31)).await;
    let (ev_tx, mut ev_rx) = mpsc::channel(4);
    let (adec_tx, adec_rx) = oneshot::channel();
    let approver = tokio::spawn({
        let transport = founder.node.transport().clone();
        let engine = founder.node.engine().clone();
        let token = mint_pair_invite(&founder);
        async move { pairing::pair_approve(&transport, &engine, addr, token, ev_tx, adec_rx).await }
    });
    let prompt = tokio::time::timeout(Duration::from_secs(10), code_rx.recv())
        .await
        .expect("joiner code within 10s")
        .expect("a code");
    assert_eq!(prompt.approver_id, founder.node.node_id());
    let event = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
        .await
        .expect("approver code within 10s");
    assert!(matches!(event, Some(PairEvent::Code { .. })), "{event:?}");
    let _ = adec_tx.send(true);
    let _ = jdec_tx.send(true);
    approver
        .await
        .expect("approver task")
        .expect("the real approver pairs");
    let report = joiner.await.expect("joiner task").expect("joined");
    assert_eq!(report.accepted_by, founder.node.node_id());

    mallory.close();
    drop(dir);
    founder.node.shutdown();
}
