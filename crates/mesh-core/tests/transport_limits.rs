//! **What a stranger can cost on the QUIC port is bounded** (SEC-10).
//!
//! The listener ran on quinn's defaults: no cap on connections, no deadline
//! on the handshake, a hundred streams a connection, each allowed a 4 MiB
//! frame before anything asked what it was for — and a probe handed the
//! mesh id to anyone who asked. These tests are the stranger: a bare
//! transport with a key the mesh never admitted.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_core::transport::{AdmissionCheck, BindOptions, LocalInfo, Transport};
use mesh_proto::{JobId, JobPayload, JobPriority, JobSpec, MeshId, Request, Response};
use mesh_testkit::{TestNode, join_new_node, spawn_founder};

async fn stranger(mesh_id: Option<MeshId>) -> Arc<Transport> {
    let key = Keypair::generate().unwrap();
    let local = LocalInfo {
        node_id: key.node_id(),
        node_name: "mallory".into(),
        mesh_id,
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
    std::mem::forget(rx);
    transport
}

fn mesh_of(node: &TestNode) -> MeshId {
    node.node.engine().genesis().body.mesh_id
}

async fn dial(from: &Transport, to: &TestNode) -> mesh_core::Result<quinn::Connection> {
    from.member_conn(to.node.node_id(), &[to.addr_string()])
        .await
}

fn job(env_bytes: usize) -> Request {
    let key = Keypair::generate().unwrap();
    let request = mesh_proto::IpcRequest::Jobs;
    Request::SubmitJob(JobSpec {
        job_id: JobId::generate(),
        payload: JobPayload::Command("true".into()),
        env: vec![("PAD".into(), "x".repeat(env_bytes))],
        timeout_secs: None,
        priority: JobPriority::Normal,
        submitter: key.node_id(),
        submitter_user: key.user_id(),
        auth: key.sign_request(&request).unwrap(),
        fanout: None,
        created_at_micros: mesh_proto::now_micros(),
    })
}

#[tokio::test]
async fn a_probe_learns_the_mesh_only_if_it_already_knows_it() {
    let founder = spawn_founder("NIM", "limits-probe").await;
    let member = join_new_node("beta", &founder, vec![]).await;

    // Somebody with no mesh, or another mesh, learns that there is one.
    for outsider in [
        stranger(None).await,
        stranger(Some(MeshId([7u8; 16]))).await,
    ] {
        let ping = outsider
            .ping(founder.node.listen_addr())
            .await
            .expect("a probe is answered");
        assert!(!ping.same_mesh);
        assert_eq!(ping.peer_mesh_id, None, "the mesh id was handed out");
        assert_eq!(ping.peer_mesh_name.as_deref(), Some(""));
        outsider.close();
    }

    // A member learns what it already knows.
    let ping = member
        .node
        .transport()
        .ping(founder.node.listen_addr())
        .await
        .unwrap();
    assert!(ping.same_mesh);
    assert_eq!(ping.peer_mesh_id, Some(mesh_of(&founder)));

    member.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn a_stranger_gets_small_frames_and_few_streams() {
    let founder = spawn_founder("NIM", "limits-frames").await;
    let outsider = stranger(Some(mesh_of(&founder))).await;
    let conn = dial(&outsider, &founder).await.expect("dial");

    // A small request is read, and refused for what it is.
    match outsider.request_one(&conn, &job(16)).await.unwrap() {
        Response::Error { message } => assert!(message.contains("not a member"), "{message}"),
        other => panic!("a stranger's job was not refused: {other:?}"),
    }
    // A megabyte is not read at all.
    assert!(
        outsider
            .request_one(&conn, &job(1024 * 1024))
            .await
            .is_err(),
        "a stranger's megabyte frame was accepted"
    );

    // Four streams at once, and the fifth waits.
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(conn.open_bi().await.unwrap());
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(500), conn.open_bi())
            .await
            .is_err(),
        "a stranger opened a fifth concurrent stream"
    );
    drop(held);

    outsider.close();
    founder.stop().await;
}

#[tokio::test]
async fn a_member_keeps_the_full_budget() {
    let founder = spawn_founder("NIM", "limits-member").await;
    let member = join_new_node("beta", &founder, vec![]).await;
    let conn = member
        .node
        .transport()
        .member_conn(founder.node.node_id(), &[founder.addr_string()])
        .await
        .unwrap();
    // Five hundred at once — five times quinn's default of a hundred,
    // which a member following that many jobs on one executor used to
    // exhaust (ENG-17).
    let mut held = Vec::new();
    for i in 0..500 {
        held.push(
            tokio::time::timeout(Duration::from_secs(5), conn.open_bi())
                .await
                .unwrap_or_else(|_| panic!("a member waited for stream credit at stream {i}"))
                .unwrap(),
        );
    }
    drop(held);
    member.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn one_address_cannot_hold_unlimited_stranger_connections() {
    let founder = spawn_founder("NIM", "limits-conns").await;
    let mesh = mesh_of(&founder);
    let mut outsiders = Vec::new();
    let mut refused = None;
    for i in 0..40 {
        let outsider = stranger(Some(mesh)).await;
        match dial(&outsider, &founder).await {
            Ok(_) => outsiders.push(outsider),
            Err(e) => {
                refused = Some((i, e.to_string()));
                outsider.close();
                break;
            }
        }
    }
    let (at, why) = refused.expect("forty stranger connections from one address were all held");
    assert_eq!(at, 32, "refused at the wrong count: {why}");
    assert!(why.contains("too many connections"), "{why}");

    // Closing one frees its place.
    outsiders.pop().unwrap().close();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let again = stranger(Some(mesh)).await;
    assert!(
        dial(&again, &founder).await.is_ok(),
        "a freed place was not reusable"
    );

    again.close();
    for o in outsiders {
        o.close();
    }
    founder.stop().await;
}
