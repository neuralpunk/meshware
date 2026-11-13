//! **An op's timestamp has to be believable** (SEC-5).
//!
//! Ops are authorized — and last-writer-wins races decided — at the time
//! their author put on them. The logical clock accepted any remote
//! timestamp, so a member could date a grant to a moment its signer still
//! held admin, and one far-future timestamp dragged every node's clock
//! forward for good. These tests forge ops as a real member, with the
//! timestamps an attacker (or a dead CMOS battery) would choose.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    Grant, GrantId, Hlc, IpcRequest, IpcResponse, NodeId, OpBody, OpEnvelope, Role, UserRecord,
};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until};

/// An op by `node`, at its next sequence number, dated `hlc`.
fn forge(node: &TestNode, body: &OpBody, hlc: Hlc) -> OpEnvelope {
    let author = node.node.node_id();
    let seq = node
        .node
        .engine()
        .read(|s| s.applied.get(&author).copied().unwrap_or(0))
        + 1;
    forge_as(node.node.node_key_for_test(), author, seq, body, hlc)
}

fn forge_as(key: &Keypair, author: NodeId, seq: u64, body: &OpBody, hlc: Hlc) -> OpEnvelope {
    let encoded = postcard::to_stdvec(body).unwrap();
    let signature = key
        .sign(&mesh_proto::op_signing_bytes_raw(
            &author, seq, &hlc, &encoded,
        ))
        .to_vec();
    OpEnvelope {
        author,
        seq,
        hlc,
        body: encoded,
        signature,
    }
}

fn at(wall_micros: u64) -> Hlc {
    Hlc {
        wall_micros,
        logical: 0,
    }
}

fn rename(node: &TestNode, name: &str) -> OpBody {
    let info = node
        .node
        .engine()
        .read(|s| s.members.get(&node.node.node_id()).unwrap().info.clone());
    OpBody::MemberUpdate {
        node_id: info.node_id,
        name: name.into(),
        addrs: info.addrs,
        tags: info.tags,
        slots: info.slots,
    }
}

async fn caught_up(on: &TestNode, author: &TestNode) {
    let id = author.node.node_id();
    let want = author
        .node
        .engine()
        .read(|s| s.applied.get(&id).copied().unwrap_or(0));
    assert!(
        wait_until(Duration::from_secs(20), || {
            on.node
                .engine()
                .read(|s| s.applied.get(&id).copied().unwrap_or(0))
                >= want
        })
        .await,
        "never caught up on {}",
        author.config().node_name
    );
}

#[tokio::test]
async fn an_op_from_the_future_waits_and_its_author_is_named() {
    let founder = spawn_founder("NIM", "clock-ahead").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    caught_up(&founder, &beta).await;
    let beta_id = beta.node.node_id();
    let engine = founder.node.engine();

    let hour = 3_600_000_000;
    let op = forge(
        &beta,
        &rename(&beta, "from-the-future"),
        at(mesh_proto::now_micros() + hour),
    );
    assert_eq!(
        engine.ingest(vec![op]).unwrap(),
        0,
        "a far-future op applied"
    );
    let name = engine.read(|s| s.members.get(&beta_id).unwrap().info.name.clone());
    assert_eq!(name, "beta");

    // This node's clock did not follow it…
    let next = engine.clock().now();
    assert!(next.wall_micros <= mesh_proto::now_micros() + mesh_core::hlc::MAX_AHEAD_MICROS);

    // …and the node whose clock is wrong is named, over IPC, for `doctor`.
    let admin = founder_user_key(&founder).clone();
    match ipc_as(&founder, &admin, IpcRequest::ClockAhead).await {
        IpcResponse::ClockAhead(rows) => {
            let row = rows
                .iter()
                .find(|r| r.node_id == beta_id)
                .expect("beta named");
            assert_eq!(row.name, "beta");
            assert!(row.ahead_micros > hour - 60_000_000);
        }
        other => panic!("ClockAhead: {other:?}"),
    }

    // A modest lead is ordinary skew, and applies.
    let op = forge(
        &beta,
        &rename(&beta, "a-bit-ahead"),
        at(mesh_proto::now_micros() + 30_000_000),
    );
    assert_eq!(engine.ingest(vec![op]).unwrap(), 1);

    beta.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn a_lapsed_admin_cannot_be_backdated_into_a_grant() {
    let founder = spawn_founder("NIM", "clock-backdate").await;
    let rogue = join_new_node("mallory", &founder, vec![]).await;
    let admin = founder_user_key(&founder).clone();

    // A person with admin for two seconds.
    let temp = Keypair::generate().unwrap();
    let mut record = UserRecord {
        user_id: temp.user_id(),
        username: "temp-admin".into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(&founder, &admin, IpcRequest::UserAdd { record }).await,
        IpcResponse::Ok
    ));
    let expires = mesh_proto::now_micros() + 2_000_000;
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id: temp.user_id(),
        role: Role::Admin,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: Some(expires),
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(&founder, &admin, IpcRequest::GrantAdd { grant }).await,
        IpcResponse::Ok
    ));
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    // The grant has lapsed, and the rogue node has said something since.
    rogue
        .node
        .engine()
        .author_op(rename(&rogue, "mallory"))
        .unwrap();
    caught_up(&founder, &rogue).await;

    // Now it dates a fresh, permanent admin grant — signed by the lapsed
    // admin — to a moment before the lapse.
    let mut forever = Grant {
        grant_id: GrantId::generate(),
        user_id: temp.user_id(),
        role: Role::Admin,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: expires - 1_000_000,
        granted_by: temp.user_id(),
        signature: Vec::new(),
    };
    forever.signature = temp.sign(&forever.signing_bytes()).to_vec();
    let forever_id = forever.grant_id;
    let op = forge(
        &rogue,
        &OpBody::GrantAdd {
            grant: forever.clone(),
        },
        at(expires - 1_000_000),
    );
    // Carried (the rogue's sequence must not wedge) but without effect,
    // on the rogue itself and on the founder alike.
    assert_eq!(rogue.node.engine().ingest(vec![op.clone()]).unwrap(), 1);
    assert_eq!(founder.node.engine().ingest(vec![op]).unwrap(), 1);
    for node in [&founder, &rogue] {
        assert!(
            node.node
                .engine()
                .read(|s| !s.grants.contains_key(&forever_id)),
            "{} accepted a backdated grant",
            node.config().node_name
        );
    }
    // …and the rogue's next honest op still lands.
    let next = rogue
        .node
        .engine()
        .author_op(rename(&rogue, "mallory-2"))
        .unwrap();
    assert_eq!(founder.node.engine().ingest(vec![next]).unwrap(), 1);

    rogue.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn a_new_members_first_op_cannot_predate_its_admission() {
    let founder = spawn_founder("NIM", "clock-admission").await;
    let engine = founder.node.engine();

    // A member admitted now…
    let key = Keypair::generate().unwrap();
    let id = key.node_id();
    let mut info = engine.read(|s| s.members.get(&founder.node.node_id()).unwrap().info.clone());
    info.node_id = id;
    info.name = "newcomer".into();
    engine
        .author_op(OpBody::MemberAdd {
            member: info.clone(),
            invite: None,
        })
        .unwrap();
    let admitted = mesh_proto::now_micros();
    let update = OpBody::MemberUpdate {
        node_id: id,
        name: "renamed".into(),
        addrs: info.addrs.clone(),
        tags: info.tags.clone(),
        slots: info.slots,
    };

    // …whose first op claims to be from an hour before: held.
    let early = forge_as(&key, id, 1, &update, at(admitted - 3_600_000_000));
    assert_eq!(engine.ingest(vec![early]).unwrap(), 0);
    // The same op, dated after: applied.
    let later = forge_as(&key, id, 1, &update, at(mesh_proto::now_micros()));
    assert_eq!(engine.ingest(vec![later]).unwrap(), 1);
    assert_eq!(
        engine.read(|s| s.members.get(&id).unwrap().info.name.clone()),
        "renamed"
    );

    founder.stop().await;
}
