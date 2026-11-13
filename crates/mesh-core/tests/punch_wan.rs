//! M7 coordinated punch + racing dialer (MASTERPLAN M7): two cone-NATed
//! members reach each other directly (the relay only coordinates), two
//! symmetric-NATed members fall back to the relay, and a black-holed first
//! candidate does not hold a dial hostage.
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use mesh_core::transport::ConnKind;
use mesh_core::wan::Reachability;
use mesh_proto::{IpcRequest, JobPriority, JobState, Liveness, RELAY_TAG};
use mesh_testkit::nat::{NatProfile, TestNat};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, join_new_node, join_new_node_natted, run_to_end,
    spawn_founder, spawn_founder_with, wait_until,
};

fn run_req(command: &str, on: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: Some(on.into()),
        env: vec![],
        timeout_secs: Some(120),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

async fn wait_registered(natted: &TestNode, relay: &TestNode) {
    let id = relay.node.node_id();
    let wan = natted.node.wan().clone();
    let ok = wait_until(Duration::from_secs(40), || wan.relays().contains(&id)).await;
    assert!(
        ok,
        "{} should register with relay (reachability {:?})",
        natted.config().node_name,
        wan.reachability()
    );
}

/// `viewer` sees `target` alive and knows a `relay:` candidate for it.
async fn wait_alive(viewer: &TestNode, target: &TestNode) {
    let id = target.node.node_id();
    let gossip = viewer.node.gossip().clone();
    let engine = viewer.node.engine().clone();
    let ok = wait_until(Duration::from_secs(40), || {
        engine.read(|s| {
            s.is_active_member(&id)
                && s.members
                    .get(&id)
                    .is_some_and(|m| m.info.addrs.iter().any(|a| a.starts_with("relay:")))
        }) && gossip.liveness_of(&id) == Liveness::Alive
    })
    .await;
    assert!(
        ok,
        "{} should see {} alive",
        viewer.config().node_name,
        target.config().node_name
    );
}

#[tokio::test]
async fn cone_to_cone_punch_yields_a_direct_path() {
    mesh_testkit::init_tracing();
    let relay = spawn_founder_with("relay", "hyacinth", vec![], |c| c.wan.relay = true).await;
    let nat_a = TestNat::new(NatProfile::Cone);
    let nat_b = TestNat::new(NatProfile::Cone);
    let a = join_new_node_natted("a", &relay, &nat_a).await;
    let b = join_new_node_natted("b", &relay, &nat_b).await;
    let a_id = a.node.node_id();
    let b_id = b.node.node_id();
    let key = founder_user_key(&relay);

    // Both discover their (stable) cone mappings and register with the relay.
    for n in [&a, &b] {
        let wan = n.node.wan().clone();
        assert!(
            wait_until(Duration::from_secs(40), || {
                wan.reachability() == Reachability::Natted && wan.reflexive_addr().is_some()
            })
            .await,
            "{} should be natted with a reflexive address",
            n.config().node_name
        );
    }
    wait_registered(&a, &relay).await;
    wait_registered(&b, &relay).await;
    let engine_a = a.node.engine().clone();
    assert!(
        wait_until(Duration::from_secs(30), || {
            engine_a.read(|s| {
                s.members.get(&b_id).is_some_and(|m| {
                    m.info.addrs.iter().any(|x| x.starts_with("relay:"))
                        && m.info.addrs.contains(
                            &nat_b
                                .external_addr(b.node.listen_addr())
                                .unwrap()
                                .to_string(),
                        )
                })
            })
        })
        .await,
        "a learns b's reflexive + relay candidates"
    );
    wait_alive(&a, &b).await;
    let relay_tagged = engine_a.read(|s| {
        s.members
            .values()
            .any(|m| m.info.tags.iter().any(|t| t == RELAY_TAG))
    });
    assert!(relay_tagged);

    // A → B: b's reflexive address is filtered until b sends toward a; the
    // punch (or the simultaneous open it triggers) opens both filters.
    let t = Instant::now();
    match run_to_end(&a, key, run_req("echo punched", "name=b")).await {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("punched"));
        }
        other => panic!("run after punch failed: {other:?}"),
    }
    let elapsed = t.elapsed();
    assert_eq!(
        a.node.transport().conn_kind(&b_id),
        Some(ConnKind::Direct),
        "the pooled path must be direct (took {elapsed:?})"
    );
    // Relay data plane stayed flat on both ends — the relay only carried
    // the punch request, never traffic.
    assert_eq!(a.node.transport().meshsock().relayed_sent(), 0);
    assert_eq!(b.node.transport().meshsock().relayed_sent(), 0);
    assert_eq!(relay.node.transport().relay().snapshot().frames_relayed, 0);
    assert!(
        elapsed < Duration::from_secs(20),
        "punch should be quick, took {elapsed:?}"
    );
    let _ = a_id;

    relay.node.shutdown();
    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn symmetric_to_symmetric_falls_back_to_relay() {
    mesh_testkit::init_tracing();
    let relay = spawn_founder_with("relay", "hyacinth", vec![], |c| c.wan.relay = true).await;
    let nat_a = TestNat::new(NatProfile::Symmetric);
    let nat_b = TestNat::new(NatProfile::Symmetric);
    let a = join_new_node_natted("a", &relay, &nat_a).await;
    let b = join_new_node_natted("b", &relay, &nat_b).await;
    let b_id = b.node.node_id();
    let key = founder_user_key(&relay);

    wait_registered(&a, &relay).await;
    wait_registered(&b, &relay).await;
    wait_alive(&a, &b).await;

    match run_to_end(&a, key, run_req("echo relayed", "name=b")).await {
        RunOutcome::Done { state, .. } => assert_eq!(state, JobState::Succeeded),
        other => panic!("relayed run failed: {other:?}"),
    }
    assert_eq!(a.node.transport().conn_kind(&b_id), Some(ConnKind::Relayed));
    assert!(a.node.transport().meshsock().relayed_sent() > 0);
    // The punch was tried (and could not help): the relay forwarded it.
    assert!(relay.node.transport().relay().snapshot().punches_forwarded >= 1);

    relay.node.shutdown();
    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn black_holed_first_candidate_is_beaten_quickly() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();
    // A bound-but-never-read socket: packets vanish, nothing answers.
    let black_hole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let hole = black_hole.local_addr().unwrap().to_string();
    let real = beta.node.listen_addr().to_string();

    // Make sure no pooled connection short-circuits the dial.
    let ft = founder.node.transport().clone();
    ft.drop_conn(&beta_id);
    let t = Instant::now();
    let conn = ft
        .member_conn(beta_id, &[hole, real])
        .await
        .expect("second candidate wins");
    let elapsed = t.elapsed();
    assert!(conn.close_reason().is_none());
    assert!(
        elapsed < Duration::from_secs(3),
        "racing dialer should beat the 2 s black hole, took {elapsed:?}"
    );
    assert_eq!(ft.conn_kind(&beta_id), Some(ConnKind::Direct));
    drop(black_hole);

    founder.node.shutdown();
    beta.node.shutdown();
}
