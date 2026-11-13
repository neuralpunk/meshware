//! **The founder can dissolve the mesh, and nobody else can** (`meshnode
//! reset`).
//!
//! Every node that hears the reset stops its work, passes it on, moves its
//! mesh data aside and stops, waiting to be adopted into a new mesh — and a
//! reset home refuses to found a mesh of its own when its service restarts
//! it. The founder's key is checked against genesis on every node, so an
//! admin grant, a forged op or a reset of some other mesh changes nothing.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::CoreError;
use mesh_core::config::NodeConfig;
use mesh_core::daemon::{Node, RunHooks};
use mesh_core::identity::Keypair;
use mesh_core::membership::{self, InviteParams};
use mesh_proto::{
    Grant, GrantId, IpcRequest, IpcResponse, Liveness, MeshId, MeshReset, MeshResetOutcome,
    MeshResetReport, OpBody, Role, UserRecord,
};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, stream_as, test_config_for,
    wait_until,
};

fn signed_reset(user: &Keypair, mesh_id: MeshId) -> MeshReset {
    let mut reset = MeshReset {
        mesh_id,
        by: user.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    reset.signature = user.sign(&reset.signing_bytes()).to_vec();
    reset
}

async fn reset_as(node: &TestNode, user: &Keypair, reset: MeshReset) -> IpcResponse {
    let mut rx = stream_as(node, user, IpcRequest::MeshReset { reset }).await;
    tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .expect("an answer to the reset")
        .expect("a frame")
}

async fn alive(from: &TestNode, id: mesh_proto::NodeId) {
    assert!(
        wait_until(Duration::from_secs(20), || {
            from.node.engine().read(|s| s.is_active_member(&id))
                && matches!(from.node.gossip().liveness_of(&id), Liveness::Alive)
        })
        .await,
        "{} never saw {} alive",
        from.node.node_id().short(),
        id.short()
    );
}

/// A joined node whose home the test owns, so it can be run the way
/// `meshnode up` runs it — through `daemon::run`, where leaving happens.
async fn joined_home(founder: &TestNode, name: &str) -> (tempfile::TempDir, NodeConfig) {
    let token = membership::create_invite(
        founder.node.engine(),
        founder_user_key(founder),
        InviteParams {
            ttl_secs: 3600,
            uses: 1,
            tags: vec![],
            bootstrap: vec![founder.addr_string()],
        },
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = test_config_for(dir.path(), name, "ignored-will-join", vec![]);
    membership::join_mesh(&config, &token.encode(), &[])
        .await
        .unwrap();
    (dir, config)
}

#[tokio::test]
async fn the_founder_resets_every_node_and_each_waits_to_be_adopted() {
    let founder = spawn_founder("NIM", "reset-me").await;
    let mesh_id = founder.node.engine().genesis().body.mesh_id;

    // IVY runs as `meshnode up` does.
    let (_ivy_dir, ivy) = joined_home(&founder, "IVY").await;
    let ivy_home = ivy.layout();
    let ivy_run = tokio::spawn(mesh_core::daemon::run(ivy.clone(), RunHooks::default()));
    // APUS is an ordinary member; merope is a member that is down.
    let apus = join_new_node("APUS", &founder, vec![]).await;
    let merope = join_new_node("merope", &founder, vec![]).await;
    for id in [apus.node.node_id(), merope.node.node_id()] {
        alive(&founder, id).await;
    }
    assert!(
        wait_until(Duration::from_secs(20), || {
            founder.node.engine().read(|s| s.members.values().count()) == 4
        })
        .await
    );
    let ivy_id = founder
        .node
        .engine()
        .read(|s| {
            s.members
                .values()
                .find(|m| m.info.name == "IVY")
                .map(|m| m.info.node_id)
        })
        .unwrap();
    alive(&founder, ivy_id).await;
    merope.stop().await;

    let admin = founder_user_key(&founder);
    let report: MeshResetReport =
        match reset_as(&founder, admin, signed_reset(admin, mesh_id)).await {
            IpcResponse::MeshReset(report) => *report,
            other => panic!("reset: {other:?}"),
        };
    assert_eq!(report.mesh_name, "reset-me");
    let outcome = |name: &str| {
        report
            .peers
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.outcome.clone())
            .unwrap_or_else(|| panic!("{name} missing from the report: {report:?}"))
    };
    assert_eq!(outcome("IVY"), MeshResetOutcome::Delivered);
    assert_eq!(outcome("APUS"), MeshResetOutcome::Delivered);
    assert!(
        matches!(outcome("merope"), MeshResetOutcome::Unreachable(_)),
        "a node that is down is reported, not skipped: {report:?}"
    );

    // The founder's node and APUS leave on their own.
    for node in [&founder, &apus] {
        assert!(
            wait_until(Duration::from_secs(45), || node.node.left_mesh().is_some()).await,
            "{} never left",
            node.node.node_id().short()
        );
        assert!(node.node.is_draining(), "a leaving node takes no work");
    }

    // IVY, under `daemon::run`, stops and moves the mesh aside — with the
    // status its unit does not restart.
    let ended = tokio::time::timeout(Duration::from_secs(60), ivy_run)
        .await
        .expect("IVY's daemon stopped")
        .expect("IVY's daemon did not panic");
    assert!(
        matches!(ended, Err(CoreError::AwaitingAdoption(_))),
        "IVY stopped as waiting to be adopted: {ended:?}"
    );
    assert!(!ivy_home.genesis_file().exists(), "genesis moved aside");
    assert!(!ivy_home.oplog().exists(), "the op-log moved aside");
    assert!(
        ivy_home.identity_dir().join("node.key").exists(),
        "the machine keeps its key"
    );
    let marker = mesh_core::reset::marker(&ivy_home)
        .unwrap()
        .expect("marker");
    assert_eq!(marker.mesh_id, mesh_id);
    assert!(marker.aside.join("mesh").join("genesis.json").exists());
    assert!(marker.aside.join("state").join("oplog.redb").exists());

    // And the unit restarting it gets a refusal, not a new one-node mesh.
    match Node::start(ivy.clone()).await {
        Err(CoreError::AwaitingAdoption(why)) => assert!(why.contains("reset-me"), "{why}"),
        Err(e) => panic!("refused for the wrong reason: {e}"),
        Ok(_) => panic!("a reset home founded a mesh of its own"),
    }
    assert!(!ivy_home.genesis_file().exists());
}

#[tokio::test]
async fn only_the_founders_signature_resets_anything() {
    let founder = spawn_founder("NIM", "keep-me").await;
    let member = join_new_node("APUS", &founder, vec![]).await;
    alive(&founder, member.node.node_id()).await;
    let mesh_id = founder.node.engine().genesis().body.mesh_id;

    // An admin, granted by the founder, is still not the founder.
    let root = founder_user_key(&founder);
    let bob = Keypair::generate().unwrap();
    let mut record = UserRecord {
        user_id: bob.user_id(),
        username: "bob".into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: root.user_id(),
        signature: Vec::new(),
    };
    record.signature = root.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(&founder, root, IpcRequest::UserAdd { record }).await,
        IpcResponse::Ok
    ));
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id: bob.user_id(),
        role: Role::Admin,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: root.user_id(),
        signature: Vec::new(),
    };
    grant.signature = root.sign(&grant.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(&founder, root, IpcRequest::GrantAdd { grant }).await,
        IpcResponse::Ok
    ));

    match reset_as(&founder, &bob, signed_reset(&bob, mesh_id)).await {
        IpcResponse::Error { message } => assert!(message.contains("founder"), "{message}"),
        other => panic!("an admin reset the mesh: {other:?}"),
    }

    // Past the node's own check, straight into the log: a member publishing
    // an admin's reset, and one publishing the founder's reset of another
    // mesh. Neither may take anybody out.
    let forged = member
        .node
        .engine()
        .author_op(OpBody::MeshReset {
            reset: signed_reset(&bob, mesh_id),
        })
        .unwrap();
    let elsewhere = member
        .node
        .engine()
        .author_op(OpBody::MeshReset {
            reset: signed_reset(root, MeshId([7; 16])),
        })
        .unwrap();
    let last = forged.seq.max(elsewhere.seq);
    let author = member.node.node_id();
    assert!(
        wait_until(Duration::from_secs(20), || {
            founder
                .node
                .engine()
                .read(|s| s.applied.get(&author).copied().unwrap_or(0) >= last)
        })
        .await,
        "the founder never took the ops in"
    );
    // A watcher that did act would have within a second or two.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for node in [&founder, &member] {
        assert!(node.node.engine().read(|s| s.mesh_reset.is_none()));
        assert!(node.node.left_mesh().is_none());
        assert!(!node.node.is_draining());
    }
}
