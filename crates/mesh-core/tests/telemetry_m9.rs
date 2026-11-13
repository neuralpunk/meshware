//! Live system telemetry (`TELEMETRY_MINOR`): what a machine is actually
//! doing, and how far that travels.
//!
//! The two properties this file exists for, both of which the pressure
//! model lacked:
//!
//! 1. **A busy machine reads as busy.** `load1` is a one-minute average of
//!    the run queue and cgroup PSI counts tasks *waiting* for a CPU; on a
//!    16-core box with every core pegged those report 0.20-per-core and
//!    2.6%, which every surface renders as idle. `cpu_pct` is utilisation.
//! 2. **A reading reaches the whole mesh**, not just whichever peer this
//!    node's gossip tick happened to pick.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse};
use mesh_testkit::{founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until};

#[tokio::test(flavor = "multi_thread")]
async fn a_node_measures_what_its_machine_is_doing() {
    let founder = spawn_founder("alpha", "telemetry").await;

    // The first sample of a node's life has no previous reading to
    // difference against, so utilisation is honestly unknown rather than
    // dishonestly zero. It becomes known on the next one.
    let measured = wait_until(Duration::from_secs(20), || {
        founder.node.system().cpu_pct.is_some()
    })
    .await;
    assert!(
        measured,
        "cpu utilisation should be measurable after a tick"
    );

    let s = founder.node.system();
    let cpu = s.cpu_pct.unwrap();
    assert!(
        (0.0..=100.0).contains(&cpu),
        "utilisation is a percentage, got {cpu}"
    );
    assert!(s.mem_total_mb.unwrap_or(0) > 0, "memory total should read");
    assert!(s.disk_total_mb.unwrap_or(0) > 0, "disk total should read");
    assert!(s.uptime_secs.is_some(), "uptime should read");
    // Throughput is a rate, so like `cpu_pct` it needs a second sample —
    // and it is deliberately *not* a percentage, so there is no bound to
    // assert beyond it existing and being a number.
    let measured_net = wait_until(Duration::from_secs(20), || {
        founder.node.system().net_rx_bps.is_some()
    })
    .await;
    assert!(measured_net, "network throughput should be measurable");
    assert!(
        founder.node.system().net_total_bps().is_some(),
        "the combined figure should follow from the halves"
    );
    // The one thing that must stay true: throughput never reaches `busy`.
    // A rate with no denominator cannot be a fraction of anything, and a
    // scheduler acting on an invented one would move real work.
    let quiet = mesh_proto::SystemSnapshot {
        cpu_pct: Some(1.0),
        net_rx_bps: Some(10_000_000_000),
        net_tx_bps: Some(10_000_000_000),
        ..founder.node.system()
    };
    assert!(
        quiet.busy().is_some_and(|b| b < 0.5),
        "a node saturating its NIC while idle is still idle to placement"
    );
    // The derived views are the ones surfaces use, so they are the ones
    // worth pinning.
    let mem = s.mem_used_pct().expect("memory percentage");
    assert!((0.0..=100.0).contains(&mem), "memory percent, got {mem}");
    assert!(
        s.busy().is_some(),
        "a machine that can be measured is busy-able"
    );

    founder.node.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_reaches_a_node_that_never_asked_for_it() {
    // The property the pull model could not have. `gamma` learns about
    // `beta` without either of them being each other's gossip partner at
    // the moment that matters — the reading crosses through `alpha`.
    let founder = spawn_founder("alpha", "telemetry-spread").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let gamma = join_new_node("gamma", &founder, vec![]).await;
    let beta_id = beta.node.node_id();

    let spread = wait_until(Duration::from_secs(30), || {
        gamma
            .node
            .gossip()
            .system_of(&beta_id)
            .is_some_and(|s| s.cpu_pct.is_some())
    })
    .await;
    assert!(
        spread,
        "gamma should learn beta's telemetry through the mesh"
    );

    // And it is beta's own measurement, not something invented in transit.
    let here = gamma.node.gossip().system_of(&beta_id).unwrap();
    let there = beta.node.system();
    assert!(
        here.mem_total_mb == there.mem_total_mb,
        "the reading that arrived should be the one beta took"
    );

    founder.node.shutdown();
    beta.node.shutdown();
    gamma.node.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn top_carries_the_live_picture_for_every_node() {
    let founder = spawn_founder("alpha", "telemetry-top").await;
    let beta = join_new_node("beta", &founder, vec![]).await;

    let ready = wait_until(Duration::from_secs(30), || {
        let IpcResponse::Top(rows) = futures::executor::block_on(ipc_as(
            &founder,
            founder_user_key(&founder),
            IpcRequest::Top,
        )) else {
            return false;
        };
        rows.len() == 2 && rows.iter().all(|r| r.system.is_some())
    })
    .await;
    assert!(ready, "both rows should carry live telemetry");

    let IpcResponse::Top(rows) =
        ipc_as(&founder, founder_user_key(&founder), IpcRequest::Top).await
    else {
        panic!("top")
    };
    for r in &rows {
        let s = r.system.as_ref().unwrap();
        assert!(s.mem_total_mb.is_some(), "{} should report memory", r.name);
        assert!(
            s.base.hlc.wall_micros > 0,
            "{} should carry a timestamped reading",
            r.name
        );
    }

    founder.node.shutdown();
    beta.node.shutdown();
}

/// A reading that stops arriving stops being reported. Absent, never stale
/// — the rule every other measurement here follows, and the reason a
/// surface can say "unknown" rather than quietly showing yesterday's load.
#[tokio::test(flavor = "multi_thread")]
async fn a_departed_nodes_reading_is_forgotten_not_frozen() {
    let founder = spawn_founder("alpha", "telemetry-ttl").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();

    assert!(
        wait_until(Duration::from_secs(30), || founder
            .node
            .gossip()
            .system_of(&beta_id)
            .is_some())
        .await,
        "the founder should hold beta's reading while beta is up"
    );

    beta.node.shutdown();
    // The founder keeps serving what it last heard until the TTL retires
    // it; what matters is that it retires rather than persisting forever.
    // `retain_pressure` also drops entries for members gossip no longer
    // lists, so either path is a pass.
    let forgotten = wait_until(Duration::from_secs(60), || {
        founder.node.gossip().system_of(&beta_id).is_none()
    })
    .await;
    assert!(forgotten, "a reading with nobody behind it should age out");

    founder.node.shutdown();
}
