//! Founder repair of a member's recorded addresses (MASTERPLAN M7).
//!
//! `MemberUpdate`'s merge rule admits an op about node N from N *or from
//! the founder*. The founder half exists for one situation: every recorded
//! address in the mesh went stale at once — a re-addressed VLAN — so no
//! node can dial any other, and the ordinary self-healing path (each node
//! re-announcing itself on boot) never runs because it has nowhere to run
//! to. One repaired record breaks the symmetry.
//!
//! The repair is deliberately weak, and the last test here is the one that
//! says so: the subject overwrites it with its own account of itself the
//! moment the two can talk again.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, NodeId};
use mesh_testkit::{founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until};

fn addrs_of(node: &mesh_testkit::TestNode, id: NodeId) -> Vec<String> {
    node.node
        .engine()
        .read(|s| s.members.get(&id).map(|m| m.info.addrs.clone()))
        .unwrap_or_default()
}

/// A joined node announces its real address in a `MemberUpdate` at boot,
/// which has to gossip its way over. Every assertion about a *recorded*
/// address has to start after that lands, or it races the join.
async fn announced_addrs(at: &mesh_testkit::TestNode, id: NodeId) -> Vec<String> {
    let ok = wait_until(Duration::from_secs(30), || !addrs_of(at, id).is_empty()).await;
    assert!(ok, "{} never announced an address", id.short());
    addrs_of(at, id)
}

async fn repair(
    at: &mesh_testkit::TestNode,
    signer: &mesh_testkit::TestNode,
    node_id: NodeId,
    addrs: &[&str],
) -> IpcResponse {
    ipc_as(
        at,
        founder_user_key(signer),
        IpcRequest::NodeAddrRepair {
            node_id,
            addrs: addrs.iter().map(|a| (*a).to_string()).collect(),
        },
    )
    .await
}

#[tokio::test]
async fn founder_repairs_a_members_recorded_address() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();

    announced_addrs(&founder, beta_id).await;

    let resp = repair(
        &founder,
        &founder,
        beta_id,
        &["10.20.30.11:7777", "merope.lan:7777", "[2001:db8::1]:7777"],
    )
    .await;
    assert!(
        matches!(resp, IpcResponse::Ok),
        "founder repair should be accepted, got {resp:?}"
    );
    assert_eq!(
        addrs_of(&founder, beta_id),
        vec![
            "10.20.30.11:7777".to_string(),
            "merope.lan:7777".to_string(),
            "[2001:db8::1]:7777".to_string(),
        ],
        "the repair replaces the recorded set: literal, hostname and IPv6"
    );

    founder.node.shutdown();
    beta.node.shutdown();
}

#[tokio::test]
async fn only_the_founder_may_speak_for_another_node() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let gamma = join_new_node("gamma", &founder, vec![]).await;
    let gamma_id = gamma.node.node_id();

    let before = announced_addrs(&beta, gamma_id).await;

    // Beta is an admin everywhere — RBAC is not what stops this. The
    // membership rule is, and the daemon refuses up front rather than
    // authoring an op every replica would ignore.
    let resp = repair(&beta, &founder, gamma_id, &["10.20.30.12:7777"]).await;
    let IpcResponse::Error { message } = resp else {
        panic!("a non-founder repair must be refused, got {resp:?}");
    };
    assert!(
        message.contains("founder"),
        "the refusal should say where to run it: {message}"
    );
    assert_eq!(
        addrs_of(&beta, gamma_id),
        before,
        "nothing was authored on refusal"
    );

    founder.node.shutdown();
    beta.node.shutdown();
    gamma.node.shutdown();
}

#[tokio::test]
async fn a_node_still_speaks_for_itself() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let me = founder.node.node_id();

    let resp = repair(&founder, &founder, me, &["10.20.30.10:7777"]).await;
    let IpcResponse::Error { message } = resp else {
        panic!("self-repair must be refused, got {resp:?}");
    };
    assert!(
        message.contains("advertise"),
        "the refusal should point at the setting that does work: {message}"
    );

    founder.node.shutdown();
}

#[tokio::test]
async fn repair_refuses_what_it_cannot_dial() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();
    let before = announced_addrs(&founder, beta_id).await;

    let relay = mesh_proto::relay_candidate(&beta_id);
    // The short id is what an operator copies out of `nodes ls`, so the
    // relay refusal has to survive an id that does not parse — otherwise
    // the likelier typo gets the message for the rarer one.
    let short_relay = format!("relay:{}", beta_id.short());

    for (addrs, expect) in [
        // A relay route is something a node earns and announces.
        (vec![relay.as_str()], "a node announces its own relays"),
        (
            vec![short_relay.as_str()],
            "a node announces its own relays",
        ),
        // Typos that would sit in replicated state looking plausible.
        (vec!["10.20.30.11"], "must be host:port"),
        // A valid SocketAddr and a permanently undialable record.
        (vec!["10.20.30.11:0"], "port 0"),
        (vec!["10.20.30.11:notaport"], "must end in a port number"),
        (vec![":7777"], "has no host"),
        (vec!["2001:db8::1:7777"], "bracketed"),
        // Nothing at all is not a way to unlist a node.
        (vec![], "at least one"),
        (vec!["   "], "at least one"),
    ] {
        let resp = repair(&founder, &founder, beta_id, &addrs).await;
        let IpcResponse::Error { message } = resp else {
            panic!("{addrs:?} should be refused, got {resp:?}");
        };
        assert!(
            message.contains(expect),
            "{addrs:?}: expected a message about {expect:?}, got {message}"
        );
    }
    assert_eq!(addrs_of(&founder, beta_id), before, "no partial writes");

    founder.node.shutdown();
    beta.node.shutdown();
}

#[tokio::test]
async fn the_subject_overwrites_the_repair_with_its_own_account() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();
    let real = announced_addrs(&founder, beta_id).await;

    // A repair is a first-contact address, not a source of truth. Assert
    // it lands...
    let resp = repair(&founder, &founder, beta_id, &["10.20.30.11:7777"]).await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");
    assert_eq!(addrs_of(&founder, beta_id), vec!["10.20.30.11:7777"]);

    // ...and then that beta takes its record back. Beta's WAN task
    // re-announces whenever replicated state disagrees with the candidates
    // it actually has, so the operator's guess survives exactly as long as
    // it takes the subject to answer for itself.
    let healed = wait_until(Duration::from_secs(30), || {
        addrs_of(&founder, beta_id) == real
    })
    .await;
    assert!(
        healed,
        "beta should reclaim its own record; founder still holds {:?}",
        addrs_of(&founder, beta_id)
    );

    founder.node.shutdown();
    beta.node.shutdown();
}
