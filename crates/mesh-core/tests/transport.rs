//! M0 exit criteria: two nodes handshake and ping over key-pinned QUIC —
//! plus the M7 (protocol 1.5) transport plumbing: inbound connections are
//! pooled and usable for reverse requests, `Observe` reports the
//! dialer's address, and a simulated 1.4 peer is neither pooled nor sent
//! 1.5 requests (a forced one fails stream-scoped, the connection lives).
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::transport::ConnKind;
use mesh_proto::{Ping, Request, Response, WAN_MINOR};
use mesh_testkit::{
    join_new_node, join_new_node_with, spawn_founder, spawn_founder_with, wait_until,
};

#[tokio::test]
async fn two_nodes_handshake_and_ping() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = spawn_founder("beta", "other-mesh").await;

    let result = a
        .node
        .transport()
        .ping(b.node.listen_addr())
        .await
        .expect("ping should succeed");

    assert_eq!(result.peer_node_id, b.node.node_id());
    assert_eq!(result.peer_node_name, "beta");
    // Different geneses -> different meshes.
    assert!(!result.same_mesh);
    assert!(result.rtt_micros > 0);

    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn ping_is_repeatable() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = spawn_founder("beta", "hyacinth-2").await;
    for _ in 0..5 {
        a.node
            .transport()
            .ping(b.node.listen_addr())
            .await
            .expect("repeat ping");
    }
    a.node.shutdown();
    b.node.shutdown();
}

/// MASTERPLAN M7: a 1.5 member's connection *to* us is pooled and serves
/// requests in the reverse direction — the mechanism by which a public
/// node reaches a NATed one that dialed it.
#[tokio::test]
async fn reverse_request_over_inbound_pooled_conn() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();

    // Beta's boot announcement dials the founder; the founder pools it.
    let ft = founder.node.transport().clone();
    let pooled = wait_until(Duration::from_secs(10), || {
        ft.pooled_conn(&beta_id).is_some_and(|c| c.inbound)
    })
    .await;
    assert!(pooled, "founder should pool beta's inbound connection");
    let entry = ft.pooled_conn(&beta_id).expect("pooled");
    assert_eq!(entry.minor, mesh_proto::PROTOCOL_MINOR);
    assert!(entry.minor >= WAN_MINOR, "WAN additions are negotiated");
    assert_eq!(entry.kind, ConnKind::Direct);
    assert_eq!(ft.peer_minor(&beta_id), Some(mesh_proto::PROTOCOL_MINOR));

    // No addresses at all: the pooled (inbound) connection must be used.
    let conn = ft
        .member_conn(beta_id, &[])
        .await
        .expect("pooled inbound conn serves member_conn");
    let resp = ft
        .request_one(
            &conn,
            &Request::Ping(Ping {
                nonce: 77,
                sent_at_micros: 0,
            }),
        )
        .await
        .expect("reverse ping");
    assert!(matches!(resp, Response::Pong(p) if p.nonce == 77));

    founder.node.shutdown();
    beta.node.shutdown();
}

/// Pinging a *member* goes through the dialer, so a dead candidate at the
/// head of the announced list — which is exactly where a peer's own
/// loopback sits — does not make a reachable node look unreachable.
#[tokio::test]
async fn ping_member_survives_a_dead_first_candidate() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();
    let ft = founder.node.transport().clone();

    // Force a real dial rather than reusing whatever join pooled.
    ft.drop_conn(&beta_id);

    let live = beta.addr_string();
    // 127.0.0.1:1 stands in for the loopback entry: syntactically fine,
    // and on this machine it answers for nobody.
    let addrs = vec!["127.0.0.1:1".to_string(), live.clone()];
    let (rtt, via) = ft
        .ping_member(beta_id, &addrs)
        .await
        .expect("a live candidate anywhere in the list is enough");
    assert!(rtt > 0);
    assert_eq!(via, live, "the ping reports the route that answered");

    founder.node.shutdown();
    beta.node.shutdown();
}

/// `Observe` answers with the address the connection appears to come from.
#[tokio::test]
async fn observe_reports_remote_address() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let founder_id = founder.node.node_id();
    let beta_id = beta.node.node_id();

    // Beta observes itself through the founder over its own dialed conn.
    let bt = beta.node.transport().clone();
    let ready = wait_until(Duration::from_secs(10), || {
        bt.pooled_conn(&founder_id).is_some()
    })
    .await;
    assert!(ready);
    let seen = bt
        .observe(&founder_id)
        .await
        .expect("observe ok")
        .expect("direct 1.5 conn → Some");
    assert_eq!(seen, beta.node.listen_addr(), "loopback: observed == bound");

    // The founder observes itself through beta over the inbound-pooled conn.
    let ft = founder.node.transport().clone();
    let ready = wait_until(Duration::from_secs(10), || {
        ft.pooled_conn(&beta_id).is_some()
    })
    .await;
    assert!(ready);
    let seen = ft
        .observe(&beta_id)
        .await
        .expect("observe ok")
        .expect("Some");
    assert_eq!(seen, founder.node.listen_addr());

    founder.node.shutdown();
    beta.node.shutdown();
}

/// Mixed-mesh gating: a peer advertising minor 4 is never pooled on the
/// inbound side (a 1.4 dialer never accepts streams), `observe` declines
/// to ask it, and a forced 1.5 request fails *stream-scoped* — the
/// connection keeps serving 1.4 requests.
#[tokio::test]
async fn simulated_1_4_peer_is_not_pooled_and_fails_stream_scoped() {
    // Facts off: since M18 a member that has published one cannot admit a
    // pre-1.7 joiner at all (F0 §3.3; `facts_m18.rs` covers that). What is
    // under test here is the transport's version gate, so the joiner has to
    // reach it.
    let founder =
        spawn_founder_with("alpha", "hyacinth", vec![], |c| c.facts.collect = false).await;
    let old = join_new_node_with("old", &founder, vec![], |c| {
        c.proto_minor_override = Some(4);
    })
    .await;
    let old_id = old.node.node_id();
    let founder_id = founder.node.node_id();

    // Let the old node's boot announcement land.
    let ot = old.node.transport().clone();
    assert!(
        wait_until(Duration::from_secs(10), || ot
            .pooled_conn(&founder_id)
            .is_some())
        .await,
        "old node dials the founder"
    );
    let ft = founder.node.transport().clone();
    // Whatever the founder pooled toward `old` must be a dialed (outbound)
    // connection negotiated at 4 — never the inbound one.
    if let Some(c) = ft.pooled_conn(&old_id) {
        assert!(!c.inbound, "1.4 inbound connections are not pooled");
        assert_eq!(c.minor, 4);
    }
    let addrs = founder
        .node
        .engine()
        .read(|s| s.members.get(&old_id).map(|m| m.info.addrs.clone()))
        .unwrap_or_default();
    let conn = ft.member_conn(old_id, &addrs).await.expect("dial old node");
    assert_eq!(ft.peer_minor(&old_id), Some(4));
    assert!(
        ft.observe(&old_id).await.expect("observe gated").is_none(),
        "never Observe a 1.4 peer"
    );

    // Force a 1.5 request anyway. Since ENG-24 it does not reach the wire
    // at all: `request_one` checks the negotiated minor against the
    // request's `min_minor()` and refuses locally, with a sentence naming
    // both. The symptom it replaces was a dropped stream — correct, and
    // indistinguishable at the sender from a peer that had gone away.
    //
    // **One check, not thirty.** Every sender-side `if minor >= SOME_MINOR`
    // is a place to forget one when a request variant is added, and
    // `min_minor()` already knows the answer for every variant in the enum.
    let forced = ft.request_one(&conn, &Request::Observe).await;
    let Err(e) = forced else {
        panic!("a 1.5 request to a 1.4 peer must fail");
    };
    assert!(
        matches!(e, mesh_core::error::CoreError::PeerTooOld(_)),
        "the refusal should name the version, not look like a transport fault: {e}"
    );
    let message = e.to_string();
    assert!(
        message.contains(&format!("1.{}", mesh_proto::WAN_MINOR)) && message.contains("1.4"),
        "the refusal should name both minors: {message}"
    );
    let pong = ft
        .request_one(
            &conn,
            &Request::Ping(Ping {
                nonce: 5,
                sent_at_micros: 0,
            }),
        )
        .await
        .expect("connection survives the dropped stream");
    assert!(matches!(pong, Response::Pong(p) if p.nonce == 5));

    founder.node.shutdown();
    old.node.shutdown();
}
