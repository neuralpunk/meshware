//! The testkit NAT (MASTERPLAN M7, §16): a node behind a userspace NAT has
//! no reachable address of its own; reflexive discovery classifies the
//! NAT and publishes exactly what is dialable.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::wan::Reachability;
use mesh_proto::{IpcRequest, JobPriority, JobState};
use mesh_testkit::nat::{NatProfile, TestNat};
use mesh_testkit::{
    RunOutcome, founder_user_key, join_new_node, join_new_node_natted, run_to_end, spawn_founder,
    wait_until,
};

fn run_req(command: &str, on: &str) -> IpcRequest {
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

#[tokio::test]
async fn cone_nat_reflexive_consensus_equals_external_addr() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let nat = TestNat::new(NatProfile::Cone);
    let natted = join_new_node_natted("natted", &founder, &nat).await;
    let natted_id = natted.node.node_id();

    // Two public observers see the same external mapping → Natted, with
    // the consensus address published as a candidate.
    let wan = natted.node.wan().clone();
    let ok = wait_until(Duration::from_secs(30), || {
        wan.reachability() == Reachability::Natted && wan.reflexive_addr().is_some()
    })
    .await;
    assert!(
        ok,
        "cone NAT should classify as natted (got {:?})",
        wan.reachability()
    );
    let external = nat
        .external_addr(natted.node.listen_addr())
        .expect("cone mapping exists");
    assert_eq!(wan.reflexive_addr(), Some(external));

    // The founder learns the reflexive candidate through gossip; the
    // node's own (black-holed) listen address is still first.
    let engine = founder.node.engine().clone();
    let ok = wait_until(Duration::from_secs(20), || {
        engine.read(|s| {
            s.members
                .get(&natted_id)
                .is_some_and(|m| m.info.addrs.contains(&external.to_string()))
        })
    })
    .await;
    assert!(ok, "reflexive candidate should be announced");
    let addrs = engine
        .read(|s| s.members.get(&natted_id).map(|m| m.info.addrs.clone()))
        .unwrap_or_default();
    assert_eq!(
        addrs.first().map(String::as_str),
        Some(natted.node.listen_addr().to_string().as_str())
    );

    // The NATed node dialed the founder; the founder runs work on it over
    // that inbound-pooled connection (no dial toward the black hole).
    let ft = founder.node.transport().clone();
    assert!(
        wait_until(Duration::from_secs(10), || ft
            .pooled_conn(&natted_id)
            .is_some())
        .await,
        "founder should hold the NATed node's inbound connection"
    );
    match run_to_end(
        &founder,
        founder_user_key(&founder),
        run_req("echo via-nat", "name=natted"),
    )
    .await
    {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("via-nat"));
        }
        other => panic!("run on natted node failed: {other:?}"),
    }

    founder.node.shutdown();
    beta.node.shutdown();
    natted.node.shutdown();
}

#[tokio::test]
async fn symmetric_nat_publishes_no_reflexive_candidate() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let nat = TestNat::new(NatProfile::Symmetric);
    let natted = join_new_node_natted("natted", &founder, &nat).await;
    let natted_id = natted.node.node_id();

    let wan = natted.node.wan().clone();
    let ok = wait_until(Duration::from_secs(30), || {
        wan.reachability() == Reachability::SymmetricNat
    })
    .await;
    assert!(
        ok,
        "two observers with different mappings → symmetric (got {:?})",
        wan.reachability()
    );
    assert!(wan.reflexive_addr().is_none());
    // Give a couple of ticks; nothing external may ever be announced.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let externals: Vec<String> = nat
        .external_addrs(natted.node.listen_addr())
        .into_iter()
        .map(|a| a.to_string())
        .collect();
    assert!(externals.len() >= 2, "symmetric NAT mapped per destination");
    let addrs = founder
        .node
        .engine()
        .read(|s| s.members.get(&natted_id).map(|m| m.info.addrs.clone()))
        .unwrap_or_default();
    assert!(
        addrs.iter().all(|a| !externals.contains(a)),
        "no per-destination mapping may be published: {addrs:?}"
    );

    founder.node.shutdown();
    beta.node.shutdown();
    natted.node.shutdown();
}
