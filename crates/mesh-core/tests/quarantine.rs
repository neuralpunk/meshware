//! **A quarantined node gets no new work and no new secrets** (SEC-16).
//!
//! Quarantine was checked during placement only. A job sent to a
//! quarantined node directly still ran there, and new secrets — and the
//! rewrap loop that heals late joiners — still sealed keys for it. A node
//! isolated because it may be compromised is the last machine to hand
//! either to.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{
    IpcRequest, IpcResponse, JobId, JobPayload, JobPriority, JobSpec, Liveness, NodeQuarantine,
    Request, Response,
};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, wait_until};

async fn quarantine(admin_node: &TestNode, target: &TestNode) {
    let admin = founder_user_key(admin_node);
    let mut q = NodeQuarantine {
        node_id: target.node.node_id(),
        quarantined: true,
        reason: "INC-9: unexpected outbound traffic".into(),
        by: admin.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    q.signature = admin.sign(&q.signing_bytes()).to_vec();
    let resp = ipc_as(
        admin_node,
        admin,
        IpcRequest::NodeQuarantine { quarantine: q },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "quarantine: {resp:?}");
    let id = target.node.node_id();
    assert!(
        wait_until(Duration::from_secs(20), || {
            target.node.engine().read(|s| s.is_quarantined(&id))
        })
        .await,
        "the quarantine never reached the node"
    );
}

#[tokio::test]
async fn a_quarantined_node_refuses_work_sent_straight_to_it() {
    let founder = spawn_founder("NIM", "quarantine-work").await;
    let suspect = join_new_node("suspect", &founder, vec![]).await;
    let id = suspect.node.node_id();
    assert!(
        wait_until(Duration::from_secs(20), || {
            founder.node.engine().read(|s| s.is_active_member(&id))
                && matches!(founder.node.gossip().liveness_of(&id), Liveness::Alive)
        })
        .await
    );
    quarantine(&founder, &suspect).await;

    // A properly signed job, sent past placement to the suspect itself.
    let admin = founder_user_key(&founder);
    let request = IpcRequest::Run {
        command: "echo ran".into(),
        on: None,
        env: vec![],
        timeout_secs: Some(10),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let spec = JobSpec {
        job_id: JobId::generate(),
        payload: JobPayload::Command("echo ran".into()),
        env: vec![],
        timeout_secs: Some(10),
        priority: JobPriority::Normal,
        submitter: founder.node.node_id(),
        submitter_user: admin.user_id(),
        auth: admin.sign_request(&request).unwrap(),
        fanout: None,
        created_at_micros: mesh_proto::now_micros(),
    };
    let addrs = founder
        .node
        .engine()
        .read(|s| s.members.get(&id).map(|m| m.info.addrs.clone()))
        .unwrap();
    let conn = founder
        .node
        .transport()
        .member_conn(id, &addrs)
        .await
        .unwrap();
    match founder
        .node
        .transport()
        .request_one(&conn, &Request::SubmitJob(spec))
        .await
        .unwrap()
    {
        Response::Error { message } => assert!(message.contains("quarantined"), "{message}"),
        other => panic!("a quarantined node accepted a job: {other:?}"),
    }

    suspect.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn a_quarantined_node_is_sealed_nothing_new() {
    let founder = spawn_founder("NIM", "quarantine-secrets").await;
    let suspect = join_new_node("suspect", &founder, vec![]).await;
    let late = join_new_node("late", &founder, vec![]).await;
    quarantine(&founder, &suspect).await;
    let admin = founder_user_key(&founder).clone();
    let suspect_id = suspect.node.node_id();
    let late_id = late.node.node_id();

    // The recipient rule the CLI and Director share leaves it out.
    let rows = match ipc_as(&founder, &admin, IpcRequest::Nodes).await {
        IpcResponse::Nodes(rows) => rows,
        other => panic!("nodes: {other:?}"),
    };
    let row = rows.iter().find(|r| r.node_id == suspect_id).unwrap();
    assert!(!row.receives_new_secrets());
    assert!(
        rows.iter()
            .filter(|r| r.node_id != suspect_id)
            .all(|r| r.receives_new_secrets())
    );

    // A client that seals for it anyway is refused.
    let seal = |recipients: &[mesh_proto::NodeId]| {
        let mut record = mesh_core::secrets::seal_secret(
            "db-password",
            1,
            b"hunter2",
            recipients,
            admin.user_id(),
        )
        .unwrap();
        record.signature = admin.sign(&record.signing_bytes()).to_vec();
        record
    };
    let founder_id = founder.node.node_id();
    let resp = ipc_as(
        &founder,
        &admin,
        IpcRequest::SecretPublish {
            record: seal(&[founder_id, suspect_id]),
        },
    )
    .await;
    match resp {
        IpcResponse::Error { message } => {
            assert!(message.contains("quarantined"), "{message}");
            assert!(message.contains("suspect"), "{message}");
        }
        other => panic!("a secret was sealed for a quarantined node: {other:?}"),
    }

    // Sealed for the founder alone, then healed by the rewrap loop: the
    // late joiner gets a wrap, the suspect does not.
    let resp = ipc_as(
        &founder,
        &admin,
        IpcRequest::SecretPublish {
            record: seal(&[founder_id]),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "{resp:?}");
    founder.node.rewrap_secrets();
    let wrapped = founder.node.engine().read(|s| {
        s.secrets["db-password"]
            .record
            .wraps
            .iter()
            .map(|w| w.node_id)
            .collect::<Vec<_>>()
    });
    assert!(wrapped.contains(&late_id), "{wrapped:?}");
    assert!(!wrapped.contains(&suspect_id), "{wrapped:?}");

    late.stop().await;
    suspect.stop().await;
    founder.stop().await;
}
