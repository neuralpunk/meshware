//! **A key the mesh never admitted gets no say in who is alive** (SEC-2).
//!
//! Admission lets an unknown key open a member connection on purpose — a
//! node whose `MemberAdd` is still in flight has to be able to reach the
//! gossip that admits it. The daemon then marked that connection's key
//! alive *before* checking whether it was a member, the liveness table was
//! never pruned, and every sync carried the whole table. About 100,000 junk
//! keys pushed each sync past the 4 MiB frame cap, and a mesh whose syncs
//! all fail decides every node is dead. All it took was UDP 7777 and a mesh
//! id, which a probe hands out.
//!
//! These tests pin the three halves of the fix: a stranger's traffic is not
//! evidence of life, a member's hearsay about a stranger is not stored, and
//! whatever does get in is pruned and never repeated in a sync.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_core::transport::{AdmissionCheck, BindOptions, LocalInfo, Transport};
use mesh_proto::{Hlc, Liveness, LivenessEntry, NodeId, Ping, Request, SyncRequestV2};
use mesh_testkit::{TestNode, join_new_node, spawn_founder, wait_until};

/// A bare transport holding the victim mesh's id and a key the mesh has
/// never admitted — exactly what a probe and a keygen buy an attacker.
async fn stranger(victim: &TestNode) -> (Arc<Transport>, NodeId) {
    let key = Keypair::generate().unwrap();
    let id = key.node_id();
    let local = LocalInfo {
        node_id: id,
        node_name: "mallory".into(),
        mesh_id: Some(victim.node.engine().genesis().body.mesh_id),
        mesh_name: None,
    };
    let admission: AdmissionCheck = Arc::new(|_, _| Ok(()));
    let (transport, rx) = Transport::bind(
        "127.0.0.1:0".parse().unwrap(),
        key,
        local,
        admission,
        BindOptions::default(),
    )
    .await
    .unwrap();
    // Nothing is ever sent to the stranger; keep the receiver alive anyway
    // so its accept loop has somewhere to put what arrives.
    std::mem::forget(rx);
    (transport, id)
}

fn entry(node_id: NodeId, liveness: Liveness) -> LivenessEntry {
    LivenessEntry {
        node_id,
        liveness,
        observed_hlc: Hlc {
            wall_micros: mesh_proto::now_micros(),
            logical: 0,
        },
    }
}

fn in_snapshot(node: &TestNode, id: &NodeId) -> bool {
    node.node
        .gossip()
        .snapshot()
        .iter()
        .any(|e| e.node_id == *id)
}

#[tokio::test]
async fn a_strangers_traffic_is_not_evidence_of_life() {
    let founder = spawn_founder("NIM", "liveness-stranger").await;
    let (mallory, mallory_id) = stranger(&founder).await;
    let founder_id = founder.node.node_id();
    let conn = mallory
        .member_conn(founder_id, &[founder.addr_string()])
        .await
        .expect("admission lets an unknown key connect");

    // A ping and a sync, the second carrying claims about more strangers.
    let fakes: Vec<NodeId> = (0..50)
        .map(|_| Keypair::generate().unwrap().node_id())
        .collect();
    let _ = mallory
        .request_one(
            &conn,
            &Request::Ping(Ping {
                nonce: 1,
                sent_at_micros: mesh_proto::now_micros(),
            }),
        )
        .await;
    let _ = mallory
        .request_one(
            &conn,
            &Request::SyncV2(SyncRequestV2 {
                vv: Vec::new(),
                liveness: fakes.iter().map(|f| entry(*f, Liveness::Alive)).collect(),
            }),
        )
        .await;
    // Give a wrongly-recorded entry every chance to show up.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let gossip = founder.node.gossip();
    assert_eq!(
        gossip.liveness_of(&mallory_id),
        Liveness::Suspect,
        "a stranger's connection was taken as a live member"
    );
    assert!(!in_snapshot(&founder, &mallory_id));
    for fake in &fakes {
        assert_eq!(gossip.liveness_of(fake), Liveness::Suspect);
        assert!(!in_snapshot(&founder, fake));
    }

    mallory.close();
    founder.stop().await;
}

#[tokio::test]
async fn hearsay_about_strangers_is_dropped_and_members_still_merge() {
    let founder = spawn_founder("NIM", "liveness-hearsay").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let beta_id = beta.node.node_id();
    let gossip = founder.node.gossip();
    assert!(
        wait_until(Duration::from_secs(15), || {
            matches!(gossip.liveness_of(&beta_id), Liveness::Alive)
        })
        .await,
        "beta never came up"
    );

    // Hearsay about a key nobody admitted: not stored.
    let fake = Keypair::generate().unwrap().node_id();
    gossip.merge_remote(&[entry(fake, Liveness::Alive)]);
    assert_eq!(gossip.liveness_of(&fake), Liveness::Suspect);
    assert!(!in_snapshot(&founder, &fake));

    // Hearsay about a member: still merged, freshest wins.
    gossip.merge_remote(&[entry(beta_id, Liveness::Dead)]);
    assert_eq!(gossip.liveness_of(&beta_id), Liveness::Dead);
    assert!(in_snapshot(&founder, &beta_id));

    beta.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn whatever_gets_in_is_pruned_and_never_repeated() {
    let founder = spawn_founder("NIM", "liveness-prune").await;
    let gossip = founder.node.gossip();

    // However an entry got there, a sync never carries it…
    let fake = Keypair::generate().unwrap().node_id();
    gossip.mark_alive(&fake);
    assert!(!in_snapshot(&founder, &fake));
    // …and the next gossip tick forgets it.
    assert!(
        wait_until(Duration::from_secs(10), || {
            gossip.liveness_of(&fake) == Liveness::Suspect
        })
        .await,
        "a non-member's entry outlived the gossip tick"
    );

    founder.stop().await;
}

/// **A claim of life is dated by the life** (ENG-7).
///
/// A node two failed probes into a streak still counts its peer alive, and
/// it used to say so dated *now* — the time of its latest failure. Every
/// node that heard it took that as fresh evidence and reset its own count,
/// so a dead peer could go unnoticed for as long as the nodes kept telling
/// each other it was fine.
#[tokio::test]
async fn a_failing_peer_is_not_vouched_for_by_the_failures() {
    let founder = spawn_founder("NIM", "liveness-dated").await;
    let gossip = founder.node.gossip();
    // A member with no address: the gossip loops leave it alone, so only
    // this test says anything about it.
    let quiet = Keypair::generate().unwrap();
    founder
        .node
        .engine()
        .author_op(mesh_proto::OpBody::MemberAdd {
            member: mesh_proto::MemberInfo {
                node_id: quiet.node_id(),
                name: "quiet".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
                joined_at_micros: mesh_proto::now_micros(),
            },
            invite: None,
        })
        .unwrap();
    let id = quiet.node_id();
    let claim = |g: &mesh_core::gossip::Gossip| {
        g.snapshot()
            .into_iter()
            .find(|e| e.node_id == id)
            .expect("the member is in the snapshot")
    };

    gossip.mark_alive(&id);
    let alive = claim(gossip);
    gossip.mark_failure_for_test(&id);
    gossip.mark_failure_for_test(&id);
    assert_eq!(gossip.liveness_of(&id), Liveness::Alive);
    let after_failures = claim(gossip);
    assert_eq!(after_failures.liveness, Liveness::Alive);
    assert_eq!(
        after_failures.observed_hlc, alive.observed_hlc,
        "the claim of life moved with the failures"
    );

    // Hearing that claim back is not news: the third failure still counts.
    gossip.merge_remote(&[after_failures]);
    gossip.mark_failure_for_test(&id);
    assert_eq!(gossip.liveness_of(&id), Liveness::Suspect);

    // Life seen after the failures is: it clears the streak.
    gossip.merge_remote(&[entry(id, Liveness::Alive)]);
    assert_eq!(gossip.liveness_of(&id), Liveness::Alive);
    gossip.mark_failure_for_test(&id);
    assert_eq!(gossip.liveness_of(&id), Liveness::Alive);

    founder.stop().await;
}
