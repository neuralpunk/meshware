//! M9 P2: live pressure over a gated request pair (§5.2).
//!
//! Pressure is the one wire addition in M9 — protocol minor 5 → 6. What
//! matters is that it converges through the ordinary gossip tick without a
//! second dial, that it is never persisted, and that a peer stuck on 1.5 is
//! never asked and never harmed by the asking.

use std::time::Duration;

use mesh_core::transport::ConnKind;
use mesh_proto::{IpcRequest, IpcResponse, Liveness, Request};
use mesh_testkit::nat::{NatProfile, TestNat};
use mesh_testkit::{
    capacity, founder_user_key, ipc_as, join_new_node_natted, join_new_node_sized,
    join_new_node_with, spawn_founder_sized, spawn_founder_with, wait_until,
};

async fn two_alive(a: &mesh_testkit::TestNode, b: &mesh_testkit::TestNode) -> bool {
    wait_until(Duration::from_secs(15), || {
        a.node.engine().read(|s| s.members.len() == 2)
            && matches!(
                a.node.gossip().liveness_of(&b.node.node_id()),
                Liveness::Alive
            )
    })
    .await
}

/// Pressure reaches both sides within a couple of gossip rounds, carrying
/// figures that describe the reporting node.
#[tokio::test(flavor = "multi_thread")]
async fn pressure_converges_over_the_gossip_tick() {
    let a = spawn_founder_sized("a", "m9-pressure", capacity(32, 128, 900)).await;
    let b = join_new_node_sized("b", &a, capacity(16, 64, 500)).await;
    assert!(two_alive(&a, &b).await, "membership did not converge");

    // Two gossip rounds is the budget: the probe rides the tick that syncs.
    let b_id = b.node.node_id();
    let a_id = a.node.node_id();
    assert!(
        wait_until(Duration::from_secs(10), || a
            .node
            .gossip()
            .pressure_of(&b_id)
            .is_some())
        .await,
        "a never learned b's pressure"
    );
    assert!(
        wait_until(Duration::from_secs(10), || b
            .node
            .gossip()
            .pressure_of(&a_id)
            .is_some())
        .await,
        "b never learned a's pressure"
    );

    let p = a.node.gossip().pressure_of(&b_id).expect("b's pressure");
    assert_eq!(p.running, 0, "an idle node reports no running work");
    assert_eq!(p.queued, 0);
    if cfg!(target_os = "linux") {
        assert!(p.load1.is_some(), "linux exposes /proc/loadavg");
        assert!(
            p.mem_available_mb.is_some_and(|m| m > 0),
            "linux exposes MemAvailable"
        );
        assert!(
            p.disk_free_mb.is_some_and(|d| d > 0),
            "the data dir lives on a real filesystem"
        );
    }
    // Load is only comparable per core across a heterogeneous mesh.
    if let Some(load) = p.load1 {
        assert_eq!(p.load_per_core(16), Some(load / 16.0));
        assert_eq!(p.load_per_core(0), None, "unknown cores is not a divide");
    }
}

/// Pressure is telemetry, not state: it never becomes an op, so the op-log
/// stays the size it was. This is the invariant §3.4 and §7 both turn on.
#[tokio::test(flavor = "multi_thread")]
async fn pressure_never_enters_the_op_log() {
    let a = spawn_founder_sized("a", "m9-nolog", capacity(8, 32, 200)).await;
    let b = join_new_node_sized("b", &a, capacity(4, 16, 100)).await;
    assert!(two_alive(&a, &b).await, "membership did not converge");
    assert!(
        wait_until(Duration::from_secs(10), || a
            .node
            .gossip()
            .pressure_of(&b.node.node_id())
            .is_some())
        .await,
        "a never learned b's pressure"
    );

    let before = a.node.engine().all_ops().expect("ops").len();
    // Several more gossip rounds: pressure refreshes every tick.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after = a.node.engine().all_ops().expect("ops").len();
    assert_eq!(
        before, after,
        "pressure authored ops — the state store is becoming a time-series DB"
    );
}

/// `meshnode top` surfaces pressure, including for the node you ask.
#[tokio::test(flavor = "multi_thread")]
async fn top_reports_own_and_peer_pressure() {
    let a = spawn_founder_sized("a", "m9-top", capacity(32, 128, 900)).await;
    let b = join_new_node_sized("b", &a, capacity(16, 64, 500)).await;
    assert!(two_alive(&a, &b).await, "membership did not converge");
    let user = founder_user_key(&a).clone();
    assert!(
        wait_until(Duration::from_secs(10), || a
            .node
            .gossip()
            .pressure_of(&b.node.node_id())
            .is_some())
        .await,
        "a never learned b's pressure"
    );

    let IpcResponse::Top(rows) = ipc_as(&a, &user, IpcRequest::Top).await else {
        panic!("expected Top rows");
    };
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert!(
            row.pressure.is_some(),
            "no pressure for {} (self={})",
            row.name,
            row.is_self
        );
    }
    assert_eq!(
        rows.iter().find(|r| r.name == "a").map(|r| r.cores),
        Some(32)
    );
    assert_eq!(
        rows.iter().find(|r| r.name == "b").map(|r| r.cores),
        Some(16)
    );
}

/// A peer stuck on 1.5 is never asked for pressure, and gossip with it
/// stays green — the whole point of gating on the negotiated minor.
#[tokio::test(flavor = "multi_thread")]
async fn a_1_5_peer_is_never_asked_and_gossip_stays_green() {
    // Facts off on the founder: since M18 a member that has published one
    // cannot admit a pre-1.7 joiner (F0 §3.3; `facts_m18.rs` covers that
    // consequence). This test is about pressure gossip across a version
    // boundary, so the joiner has to be able to get in.
    let a = spawn_founder_with("a", "m9-mixed", vec![], |c| {
        mesh_testkit::apply_capacity_for_test(c, capacity(8, 32, 200));
        c.facts.collect = false;
    })
    .await;
    let old = join_new_node_with("old", &a, vec![], |c| {
        c.proto_minor_override = Some(5);
    })
    .await;
    let old_id = old.node.node_id();
    assert!(two_alive(&a, &old).await, "membership did not converge");

    // Give gossip several rounds. State still converges, and liveness holds.
    assert!(
        wait_until(Duration::from_secs(15), || old
            .node
            .engine()
            .read(|s| s.members.len() == 2))
        .await,
        "a 1.5 peer must still converge"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        matches!(a.node.gossip().liveness_of(&old_id), Liveness::Alive),
        "gossip with a 1.5 peer went unhealthy"
    );
    assert_eq!(
        a.node.transport().peer_minor(&old_id),
        Some(5),
        "negotiated minor should be the older side's"
    );
    assert!(
        a.node.gossip().pressure_of(&old_id).is_none(),
        "pressure must never be collected from a 1.5 peer"
    );

    // And if one were forced anyway, the peer drops the stream — not the
    // connection — exactly as its decoder would.
    let addrs = a
        .node
        .engine()
        .read(|s| s.members.get(&old_id).map(|m| m.info.addrs.clone()))
        .unwrap_or_default();
    let conn = a
        .node
        .transport()
        .member_conn(old_id, &addrs)
        .await
        .expect("dial the 1.5 peer");
    assert!(
        a.node
            .transport()
            .request_one(&conn, &Request::Pressure)
            .await
            .is_err(),
        "a 1.6 request to a 1.5 peer must fail"
    );
    assert!(
        matches!(a.node.gossip().liveness_of(&old_id), Liveness::Alive),
        "the forced request must not cost the peer its liveness"
    );
}

/// A peer reachable only through a relay is never *probed* (M9 §3.5, §5.5)
/// — relays are soft traffic hubs and telemetry is advisory — but since
/// `TELEMETRY_MINOR` its reading still arrives, carried as data by a node
/// that measured it over a direct connection.
///
/// The distinction is the whole point of spreading readings instead of
/// pulling them. Before, a relayed peer's load was permanently unknown, so
/// placement was blind to exactly the nodes most likely to be somewhere
/// awkward. Now it is known, and it still costs no relay hop to ask.
///
/// What the assertions pin: the reading `a` holds for `b` is *the relay's*
/// reading of `b`, HLC for HLC. A probe would have produced a different,
/// later one.
#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_peer_is_learned_second_hand_never_probed() {
    let relay = spawn_founder_with("relay", "m9-relayed", vec![], |c| {
        c.wan.relay = true;
    })
    .await;
    let nat_a = TestNat::new(NatProfile::Symmetric);
    let nat_b = TestNat::new(NatProfile::Symmetric);
    let a = join_new_node_natted("a", &relay, &nat_a).await;
    let b = join_new_node_natted("b", &relay, &nat_b).await;
    let b_id = b.node.node_id();

    // Wait until a ↔ b is actually carried by the relay.
    assert!(
        wait_until(Duration::from_secs(40), || {
            a.node
                .transport()
                .pooled_conn(&b_id)
                .is_some_and(|c| c.kind == ConnKind::Relayed)
        })
        .await,
        "a ↔ b never fell back to the relay"
    );
    assert!(
        wait_until(Duration::from_secs(20), || matches!(
            a.node.gossip().liveness_of(&b_id),
            Liveness::Alive
        ))
        .await,
        "the relayed peer should still be alive"
    );

    // Several gossip ticks later: `a` knows what `b` is doing, and knows it
    // because the relay told it, not because it asked over the relay.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let heard = wait_until(Duration::from_secs(30), || {
        match (
            a.node.gossip().pressure_of(&b_id),
            relay.node.gossip().pressure_of(&b_id),
        ) {
            (Some(mine), Some(theirs)) => mine.hlc == theirs.hlc,
            _ => false,
        }
    })
    .await;
    assert!(
        heard,
        "a should hold the relay's reading of b, passed on rather than probed"
    );
    assert!(matches!(
        a.node.gossip().liveness_of(&b_id),
        Liveness::Alive
    ));

    // The relay itself is directly reachable, so it *is* probed — the gate
    // is about the path, not about WAN meshes as a whole.
    let relay_id = relay.node.node_id();
    assert!(
        wait_until(Duration::from_secs(20), || a
            .node
            .gossip()
            .pressure_of(&relay_id)
            .is_some())
        .await,
        "a directly-reachable peer must still report pressure"
    );
}
