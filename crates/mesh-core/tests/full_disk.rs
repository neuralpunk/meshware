//! ENG-4 — a full disk makes a node refuse work and say so, not go
//! silently wrong.
//!
//! A node applied its own ops in memory and then wrote them. When the write
//! failed, peers saw sequence numbers it could never serve and waited behind
//! them for ever, redb refused every later write until a reopen nobody did,
//! the node blamed its peers for its own failed writes, and one chatty job
//! was enough to start it all.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::StateEngine;
use mesh_proto::{IpcRequest, JobPriority, JobState, Liveness, MemberInfo, NodeId, OpBody};
use mesh_testkit::{
    RunOutcome, founder_user_key, join_new_node, run_to_end, spawn_founder, spawn_founder_with,
    wait_until,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn rename(me: NodeId, name: &str) -> OpBody {
    OpBody::MemberUpdate {
        node_id: me,
        name: name.into(),
        addrs: vec![],
        tags: vec![],
        slots: 1,
    }
}

fn name_of(e: &Arc<StateEngine>, me: &NodeId) -> String {
    e.read(|s| s.members[me].info.name.clone())
}

fn applied(e: &Arc<StateEngine>, me: &NodeId) -> u64 {
    e.read(|s| s.applied.get(me).copied().unwrap_or(0))
}

#[test]
fn a_failed_write_spends_no_sequence_number_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let user = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("eng4", &user, node.node_id()).unwrap();
    let me = node.node_id();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(add_self(me)).unwrap();
    let before = applied(&e, &me);

    e.fill_disk_for_test(true);
    assert!(e.author_op(rename(me, "beta")).is_err());
    // Nothing applied, nothing spent.
    assert_eq!(applied(&e, &me), before);
    assert_eq!(name_of(&e, &me), "alpha");
    assert!(e.store_failure().is_some());
    assert!(
        e.write_failure().is_some(),
        "audit-worthy work is not refused"
    );

    // Space comes back, but the store stays refused until it is reopened —
    // which is how redb behaves after a failed commit.
    e.fill_disk_for_test(false);
    assert!(e.author_op(rename(me, "beta")).is_err());
    e.recover_store().unwrap();
    assert!(e.store_failure().is_none());
    assert!(e.write_failure().is_none());
    let op = e.author_op(rename(me, "gamma")).unwrap();
    assert_eq!(op.seq, before + 1, "a failed write spent a sequence number");
    assert_eq!(name_of(&e, &me), "gamma");

    // A restart agrees with what was said.
    drop(e);
    let e = StateEngine::open(dir.path(), node, genesis).unwrap();
    assert_eq!(applied(&e, &me), before + 1);
    assert_eq!(name_of(&e, &me), "gamma");
}

/// Writing first means an op that is then refused is already on the disk.
/// It is taken back: no sequence number spent, nothing for peers to fetch
/// and park behind, and a restart agrees.
#[test]
fn a_refused_op_is_taken_back_off_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let user = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("eng4-refused", &user, node.node_id()).unwrap();
    let me = node.node_id();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(add_self(me)).unwrap();
    let before = applied(&e, &me);
    assert_eq!(e.stored_ops_by(&me).unwrap(), before);

    // A retention policy signed by nobody the mesh knows does not apply.
    let stranger = Keypair::generate().unwrap();
    let mut policy = mesh_proto::RetentionPolicy::default();
    policy.set(
        mesh_proto::RetentionTable::Runs,
        mesh_proto::RetentionWindow {
            days: 1,
            floor_days: 0,
        },
    );
    let policy = mesh_core::retention::sign(&stranger, policy, 1_000);
    assert!(mesh_core::retention::put(&e, policy).is_err());
    assert_eq!(applied(&e, &me), before);
    assert_eq!(
        e.stored_ops_by(&me).unwrap(),
        before,
        "the refused op stayed on disk"
    );
    let (served, _) = e.missing_for(&Vec::new()).unwrap();
    assert!(served.iter().all(|op| op.author != me || op.seq <= before));

    // The number is reused, and a restart sees one history.
    let op = e.author_op(rename(me, "beta")).unwrap();
    assert_eq!(op.seq, before + 1);
    drop(e);
    let e = StateEngine::open(dir.path(), node, genesis).unwrap();
    assert_eq!(applied(&e, &me), before + 1);
    assert_eq!(e.stored_ops_by(&me).unwrap(), before + 1);
    assert_eq!(name_of(&e, &me), "beta");
}

fn add_self(me: NodeId) -> OpBody {
    OpBody::MemberAdd {
        member: MemberInfo {
            node_id: me,
            name: "alpha".into(),
            addrs: vec![],
            tags: vec![],
            slots: 1,
            joined_at_micros: mesh_proto::now_micros(),
        },
        invite: None,
    }
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out).await;
    out
}

fn run_on(on: &str, command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: Some(on.into()),
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_cannot_write_refuses_work_and_blames_nobody() {
    mesh_testkit::init_tracing();
    let full = spawn_founder_with("full", "eng4-node", vec![], |c| {
        c.http_addr = Some("127.0.0.1:0".parse().unwrap());
    })
    .await;
    let peer = join_new_node("peer", &full, vec![]).await;
    let admin = founder_user_key(&full).clone();
    let peer_id = peer.node.node_id();
    assert!(
        wait_until(Duration::from_secs(15), || matches!(
            full.node.gossip().liveness_of(&peer_id),
            Liveness::Alive
        ))
        .await
    );
    let http = full.node.http_addr().unwrap();

    // The disk fills, and the node finds out on its next write: a peer's
    // op, applied and then not stored.
    full.node.engine().fill_disk_for_test(true);
    let me = full.node.node_id();
    let peer_info = full
        .node
        .engine()
        .read(|s| s.members[&peer_id].info.clone());
    let full_info = peer.node.engine().read(|s| s.members[&me].info.clone());
    let again = |info: &MemberInfo| OpBody::MemberUpdate {
        node_id: info.node_id,
        name: info.name.clone(),
        addrs: info.addrs.clone(),
        tags: info.tags.clone(),
        slots: info.slots,
    };
    peer.node.engine().author_op(again(&peer_info)).unwrap();
    full.node
        .gossip()
        .sync_once_for_test(peer_info.clone())
        .await;
    assert!(full.node.engine().store_failure().is_some());
    // Its own writes fail from then on.
    assert!(full.node.engine().author_op(rename(me, "full")).is_err());

    let health = http_get(http, "/healthz").await;
    assert!(health.contains("503"), "{health}");
    assert!(health.contains("op-log cannot be written"), "{health}");

    // It takes no new work: sent to it from elsewhere, it answers Busy…
    match run_to_end(&peer, &admin, run_on("name=full", "echo here")).await {
        RunOutcome::Error(message) => assert!(message.contains("saturated"), "{message}"),
        other => panic!("a node that cannot write took a job: {other:?}"),
    }
    // …and asked directly, it will not do work it cannot record (SEC-6).
    match run_to_end(&full, &admin, run_on("name=full", "echo here")).await {
        RunOutcome::Error(message) => assert!(message.contains("cannot write"), "{message}"),
        other => panic!("a node that cannot write took a job: {other:?}"),
    }

    // The peer keeps writing, the full node keeps failing to store what it
    // is sent, and that is not held against the peer.
    for i in 0..3 {
        let RunOutcome::Done { state, .. } =
            run_to_end(&peer, &admin, run_on("name=peer", &format!("echo {i}"))).await
        else {
            panic!("the healthy node could not run work");
        };
        assert_eq!(state, JobState::Succeeded);
    }
    // Every round with the peer now fails here: what it has, this node
    // cannot store. Back to back, so the peer's own rounds cannot mask a
    // count building up against it, and each after something new.
    let held = full.node.transport().pooled_conn(&peer_id).unwrap().conn;
    for round in 0..20 {
        peer.node.engine().author_op(again(&peer_info)).unwrap();
        full.node
            .gossip()
            .sync_once_for_test(peer_info.clone())
            .await;
        assert_eq!(
            full.node.gossip().liveness_of(&peer_id),
            Liveness::Alive,
            "a local write failure was counted against the peer (round {round})"
        );
    }
    assert!(
        held.close_reason().is_none(),
        "the connection to the peer was dropped"
    );
    // The other way round, the full node refuses what it is pushed — and
    // it answered, so that is not held against it either: not by liveness,
    // and not by cutting the connection, and every stream on it, each round.
    let held = peer.node.transport().pooled_conn(&me).unwrap().conn;
    for round in 0..20 {
        peer.node.engine().author_op(again(&peer_info)).unwrap();
        peer.node
            .gossip()
            .sync_once_for_test(full_info.clone())
            .await;
        assert_eq!(
            peer.node.gossip().liveness_of(&me),
            Liveness::Alive,
            "a refusal was counted against the node that answered (round {round})"
        );
    }
    assert!(
        held.close_reason().is_none(),
        "the connection to the full node was dropped"
    );
    assert!(full.node.engine().store_failure().is_some());
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        full.node.gossip().liveness_of(&peer_id),
        Liveness::Alive,
        "a local write failure was counted against the peer"
    );
    // Nor is its refusal counted against it: it answered.
    assert_eq!(
        peer.node.gossip().liveness_of(&me),
        Liveness::Alive,
        "the peer counted a node that answered as failing"
    );
    // What it applied, it claims — and must be able to serve once it can.
    let claimed = full.node.engine().version_vector();

    // Space comes back: the node reopens its store on its own and works.
    full.node.engine().fill_disk_for_test(false);
    assert!(
        wait_until(Duration::from_secs(30), || full
            .node
            .engine()
            .write_failure()
            .is_none())
        .await,
        "the node never recovered its store"
    );
    let health = http_get(http, "/healthz").await;
    assert!(health.contains("200 OK"), "{health}");
    // Everything it claimed while full is in its store. Nothing is
    // compacted in a mesh this young, so every claimed op is a stored one.
    for (author, upto) in claimed {
        let stored = full.node.engine().stored_ops_by(&author).unwrap();
        assert!(
            stored >= upto,
            "{} claimed through {upto} but stores {stored} ops",
            author.short()
        );
    }
    let RunOutcome::Done { state, .. } =
        run_to_end(&full, &admin, run_on("name=full", "echo back")).await
    else {
        panic!("the recovered node could not run work");
    };
    assert_eq!(state, JobState::Succeeded);

    peer.stop().await;
    full.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chatty_job_is_capped_and_still_finishes() {
    let node = spawn_founder("NIM", "eng4-cap").await;
    let admin = founder_user_key(&node).clone();
    node.node.jobs().set_output_limit(64 * 1024);

    let outcome = run_to_end(
        &node,
        &admin,
        run_on(
            "name=NIM",
            "head -c 2000000 /dev/zero | tr '\\0' x; echo; echo done",
        ),
    )
    .await;
    let RunOutcome::Done { state, job_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded);
    let stdout = std::fs::read_to_string(
        node.node
            .jobs()
            .job_dir_of(&job_id.unwrap())
            .join("stdout.log"),
    )
    .unwrap();
    assert!(
        stdout.len() < 64 * 1024 + 1024,
        "kept {} bytes",
        stdout.len()
    );
    assert!(stdout.contains("job_output_max_mb"), "no note in the log");
    node.stop().await;
}
