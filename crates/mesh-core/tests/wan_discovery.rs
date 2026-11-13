//! M7 `[wan]` + reflexive discovery (MASTERPLAN M7): nodes learn how the
//! mesh sees them by asking peers (`Observe`), operator-advertised
//! endpoints are announced from boot and count as verified public, the
//! relay role follows, `[wan]` hot-reloads, and the invite bootstrap set
//! is public-first.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::wan::Reachability;
use mesh_proto::{IpcRequest, IpcResponse, RELAY_TAG};
use mesh_testkit::{
    founder_user_key, ipc_as, join_new_node, spawn_founder, spawn_founder_with, wait_until,
};

#[tokio::test]
async fn loopback_nodes_observe_each_other_and_verify_public() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let gamma = join_new_node("gamma", &founder, vec![]).await;

    for node in [&founder, &beta, &gamma] {
        let wan = node.node.wan().clone();
        let ok = wait_until(Duration::from_secs(20), || {
            wan.reachability() == Reachability::Public
        })
        .await;
        assert!(
            ok,
            "{} should verify itself public on loopback (got {:?})",
            node.config().node_name,
            wan.reachability()
        );
        // Loopback observations are local: no reflexive candidate.
        assert!(wan.reflexive_addr().is_none());
        // Nothing relayed on a loopback mesh.
        assert_eq!(node.node.transport().meshsock().relayed_sent(), 0);
    }

    // Status carries the summary.
    let resp = ipc_as(&founder, founder_user_key(&founder), IpcRequest::Status).await;
    let IpcResponse::Status(s) = resp else {
        panic!("status")
    };
    let wan = s.wan.expect("wan summary present");
    assert_eq!(wan.reachability, "public");
    assert!(!wan.relay_server);

    founder.node.shutdown();
    beta.node.shutdown();
    gamma.node.shutdown();
}

#[tokio::test]
async fn advertise_is_announced_and_relay_role_follows() {
    let founder = spawn_founder_with("alpha", "hyacinth", vec![], |c| {
        c.wan.advertise = vec!["11.22.33.44:7777".into()];
        c.wan.relay = true;
    })
    .await;
    let me = founder.node.node_id();

    // The boot announcement already carries the advertised endpoint.
    let addrs = founder
        .node
        .engine()
        .read(|s| s.members.get(&me).map(|m| m.info.addrs.clone()))
        .expect("own record");
    assert!(addrs.contains(&"11.22.33.44:7777".to_string()));

    // Operator-declared public → Public without any observer; relay = on
    // → the hub is enabled and the reserved tag announced.
    let wan = founder.node.wan().clone();
    let engine = founder.node.engine().clone();
    let ok = wait_until(Duration::from_secs(10), || {
        wan.reachability() == Reachability::Public
            && wan.is_relay_server()
            && engine.read(|s| {
                s.members
                    .get(&me)
                    .is_some_and(|m| m.info.tags.iter().any(|t| t == RELAY_TAG))
            })
    })
    .await;
    assert!(ok, "advertise+relay=on should make a relay server");
    assert!(founder.node.transport().relay().is_enabled());

    // The invite rendezvous set is public-first and never `relay:`.
    let resp = ipc_as(
        &founder,
        founder_user_key(&founder),
        IpcRequest::InviteBootstrap,
    )
    .await;
    let IpcResponse::InviteBootstrap { addrs, .. } = resp else {
        panic!("bootstrap")
    };
    assert_eq!(addrs.first().map(String::as_str), Some("11.22.33.44:7777"));
    assert!(addrs.iter().all(|a| !a.starts_with("relay:")));
    assert!(addrs.iter().any(|a| a.starts_with("127.0.0.1:")));

    founder.node.shutdown();
}

#[tokio::test]
async fn wan_section_hot_reloads() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let me = founder.node.node_id();
    let mut new = founder.config().clone();
    new.wan.advertise = vec!["11.22.33.55:7777".into()];
    new.wan.relay = true;
    let notes = founder.node.apply_config(&new);
    assert!(
        notes.iter().any(|n| n.starts_with("wan:")),
        "notes: {notes:?}"
    );

    let engine = founder.node.engine().clone();
    let wan = founder.node.wan().clone();
    let ok = wait_until(Duration::from_secs(10), || {
        engine.read(|s| {
            s.members.get(&me).is_some_and(|m| {
                m.info.addrs.contains(&"11.22.33.55:7777".to_string())
                    && m.info.tags.iter().any(|t| t == RELAY_TAG)
            })
        }) && wan.is_relay_server()
    })
    .await;
    assert!(
        ok,
        "reload should re-announce the advertised endpoint + relay tag"
    );

    // And back off again: relay tag cleared, advertise gone.
    let mut off = founder.config().clone();
    off.wan.advertise = Vec::new();
    off.wan.relay = false;
    founder.node.apply_config(&off);
    let ok = wait_until(Duration::from_secs(10), || {
        engine.read(|s| {
            s.members.get(&me).is_some_and(|m| {
                !m.info.addrs.contains(&"11.22.33.55:7777".to_string())
                    && !m.info.tags.iter().any(|t| t == RELAY_TAG)
            })
        }) && !wan.is_relay_server()
    })
    .await;
    assert!(ok, "reload should withdraw the relay role");
    assert!(!founder.node.transport().relay().is_enabled());

    founder.node.shutdown();
}
