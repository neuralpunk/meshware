//! ENG-13 — a restored node picks up where its peers are, not where its
//! backup was.
//!
//! A backup brings back the node key and the op-log of the day it was
//! taken. The node's peers hold everything it wrote after that, so the
//! next sequence number it picked was one they already had — and they
//! dropped the new op as a duplicate, silently and for ever. A restored or
//! rebuilt node now writes nothing of its own until a peer has shown it its
//! own history.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mesh_core::config::NodeConfig;
use mesh_core::daemon::Node;
use mesh_core::membership;
use mesh_core::state::StateEngine;
use mesh_proto::{NodeId, OpBody};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder, test_config_for, wait_until};

/// A member whose data dir outlives it: the test decides when it goes.
async fn join_in(dir: &Path, name: &str, founder: &TestNode) -> (Node, NodeConfig) {
    let token = membership::create_invite(
        founder.node.engine(),
        founder_user_key(founder),
        membership::InviteParams {
            ttl_secs: 3600,
            uses: 10,
            tags: vec![],
            bootstrap: vec![founder.addr_string()],
        },
    )
    .unwrap();
    let config = test_config_for(dir, name, "ignored", vec![]);
    membership::join_mesh(&config, &token.encode(), &[])
        .await
        .unwrap();
    (Node::start(config.clone()).await.unwrap(), config)
}

/// Start a node on a data dir another node just left, once its store lock
/// has cleared.
async fn start_on(config: &NodeConfig) -> Node {
    let mut last = String::new();
    for _ in 0..40 {
        match Node::start(config.clone()).await {
            Ok(node) => return node,
            Err(e) => {
                last = e.to_string();
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    panic!("the node would not start: {last}");
}

fn applied(e: &Arc<StateEngine>, author: &NodeId) -> u64 {
    e.read(|s| s.applied.get(author).copied().unwrap_or(0))
}

/// The same member record under a new name — an op of this node's own that
/// every peer applies.
fn renamed(e: &Arc<StateEngine>, me: NodeId, name: &str) -> OpBody {
    let info = e.read(|s| s.members[&me].info.clone());
    OpBody::MemberUpdate {
        node_id: me,
        name: name.into(),
        addrs: info.addrs,
        tags: info.tags,
        slots: info.slots,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_node_catches_up_before_it_writes() {
    let founder = spawn_founder("hq", "eng13-restore").await;
    let dir = tempfile::tempdir().unwrap();
    let (b, b_config) = join_in(&dir.path().join("b"), "b", &founder).await;
    let me = b.node_id();
    b.engine()
        .author_op(renamed(b.engine(), me, "b-1"))
        .unwrap();

    // The backup, and then more history that only the peers keep.
    let backup = dir.path().join("b.mwb");
    mesh_core::backup::write_backup(
        &backup,
        &b_config.data_dir,
        "b",
        b.engine().genesis(),
        b.engine().all_ops().unwrap(),
    )
    .unwrap();
    let at_backup = applied(b.engine(), &me);
    for i in 0..3 {
        b.engine()
            .author_op(renamed(b.engine(), me, &format!("b-after-{i}")))
            .unwrap();
    }
    let latest = applied(b.engine(), &me);
    assert!(
        wait_until(Duration::from_secs(15), || applied(
            founder.node.engine(),
            &me
        ) >= latest)
        .await
    );
    b.stop().await;
    drop(b);

    // The machine is replaced, and the backup restored onto it.
    let restored = test_config_for(&dir.path().join("b-restored"), "b", "ignored", vec![]);
    let report = mesh_core::backup::restore(&backup, &restored.data_dir).unwrap();
    assert!(report.members >= 2);
    let c = start_on(&restored).await;
    assert!(
        c.engine().restore_hold().is_some(),
        "a restored node with peers started writing"
    );
    assert!(
        c.engine()
            .author_op(renamed(c.engine(), me, "too-early"))
            .is_err(),
        "a held node wrote an op"
    );

    // It fetches its own later history, and only then writes.
    assert!(
        wait_until(Duration::from_secs(20), || c
            .engine()
            .restore_hold()
            .is_none())
        .await,
        "the restored node never caught up"
    );
    assert!(applied(c.engine(), &me) >= latest);
    // Every one of its sequence numbers is the op its peers hold — no
    // second history under the same numbers.
    for seq in at_backup + 1..=latest {
        let ours = c.engine().op_at(&me, seq).unwrap().expect("missing op");
        let theirs = founder
            .node
            .engine()
            .op_at(&me, seq)
            .unwrap()
            .expect("peer missing op");
        assert_eq!(ours.signature, theirs.signature, "seq {seq} forked");
    }

    // What it writes now, its peers take.
    let op = c
        .engine()
        .author_op(renamed(c.engine(), me, "b-restored"))
        .unwrap();
    assert!(op.seq > latest);
    assert!(
        wait_until(Duration::from_secs(15), || founder
            .node
            .engine()
            .read(|s| s.members[&me].info.name == "b-restored"))
        .await,
        "the peer dropped the restored node's op"
    );
    // And the announcement the hold deferred went out: its new address.
    let port = format!(":{}", c.listen_addr().port());
    assert!(
        wait_until(Duration::from_secs(15), || founder.node.engine().read(
            |s| s.members[&me].info.addrs.iter().any(|a| a.ends_with(&port))
        ))
        .await,
        "the restored node's address never reached its peer"
    );
    c.stop().await;
    founder.stop().await;
}

/// With no other member there is nothing to wait for: the backup is the
/// only copy of the mesh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mesh_of_one_is_not_held() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config_for(&dir.path().join("solo"), "solo", "eng13-solo", vec![]);
    let node = Node::start(config.clone()).await.unwrap();
    let me = node.node_id();
    let backup = dir.path().join("solo.mwb");
    mesh_core::backup::write_backup(
        &backup,
        &config.data_dir,
        "solo",
        node.engine().genesis(),
        node.engine().all_ops().unwrap(),
    )
    .unwrap();
    node.stop().await;
    drop(node);

    let restored = test_config_for(&dir.path().join("again"), "solo", "eng13-solo", vec![]);
    mesh_core::backup::restore(&backup, &restored.data_dir).unwrap();
    let node = start_on(&restored).await;
    assert!(node.engine().restore_hold().is_none());
    node.engine()
        .author_op(renamed(node.engine(), me, "solo-2"))
        .unwrap();
    node.stop().await;
}

/// An op-log that will not open is rebuilt from a peer, and the node comes
/// back on it with its own history intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_oplog_is_rebuilt_from_a_peer() {
    let founder = spawn_founder("hq", "eng13-rebuild").await;
    let dir = tempfile::tempdir().unwrap();
    let (b, b_config) = join_in(&dir.path().join("b"), "b", &founder).await;
    let me = b.node_id();
    for i in 0..3 {
        b.engine()
            .author_op(renamed(b.engine(), me, &format!("b-{i}")))
            .unwrap();
    }
    let latest = applied(b.engine(), &me);
    assert!(
        wait_until(Duration::from_secs(15), || applied(
            founder.node.engine(),
            &me
        ) >= latest)
        .await
    );
    b.stop().await;
    drop(b);

    // Garbage where the op-log was.
    let oplog = b_config.data_dir.join("state").join("oplog.redb");
    std::fs::write(&oplog, vec![0x5au8; 64 * 1024]).unwrap();
    let broken = match Node::start(b_config.clone()).await {
        Ok(_) => panic!("a damaged op-log opened"),
        Err(e) => e.to_string(),
    };
    assert!(broken.contains("state rebuild"), "{broken}");

    // From a peer that is not there: nothing changes.
    let nowhere = membership::rebuild_oplog(&b_config, &["127.0.0.1:9".into()]).await;
    assert!(nowhere.is_err());
    assert_eq!(std::fs::read(&oplog).unwrap(), vec![0x5au8; 64 * 1024]);

    let report = membership::rebuild_oplog(&b_config, &[founder.addr_string()])
        .await
        .unwrap();
    assert_eq!(report.from, founder.node.node_id());
    let kept = report.kept.expect("the damaged file was not kept");
    assert_eq!(std::fs::read(&kept).unwrap(), vec![0x5au8; 64 * 1024]);

    let b = start_on(&b_config).await;
    assert!(applied(b.engine(), &me) >= latest);
    assert_eq!(b.engine().read(|s| s.members[&me].info.name.clone()), "b-2");
    assert!(
        wait_until(Duration::from_secs(20), || b
            .engine()
            .restore_hold()
            .is_none())
        .await
    );
    let op = b
        .engine()
        .author_op(renamed(b.engine(), me, "b-back"))
        .unwrap();
    assert!(op.seq > latest);
    assert!(
        wait_until(Duration::from_secs(15), || founder
            .node
            .engine()
            .read(|s| s.members[&me].info.name == "b-back"))
        .await
    );
    b.stop().await;
    founder.stop().await;
}
