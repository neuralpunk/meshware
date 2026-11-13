//! **An ordinary member cannot read or stop other people's work** (SEC-15).
//!
//! The legacy `CancelJob` was served to any member with no user check, a
//! job's output was streamed to any member that asked.
//! These tests are a member that is neither the job's origin nor anybody's
//! courier.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse, JobId, JobPriority, Liveness, Request, Response};
use mesh_testkit::{
    TestNode, founder_user_key, join_new_node, spawn_founder, stream_as, wait_until,
};

async fn alive(from: &TestNode, nodes: &[&TestNode]) {
    for n in nodes {
        let id = n.node.node_id();
        assert!(
            wait_until(Duration::from_secs(20), || {
                from.node.engine().read(|s| s.is_active_member(&id))
                    && matches!(from.node.gossip().liveness_of(&id), Liveness::Alive)
            })
            .await
        );
    }
}

async fn ask(from: &TestNode, to: &TestNode, req: Request) -> Response {
    let id = to.node.node_id();
    let addrs = from
        .node
        .engine()
        .read(|s| s.members[&id].info.addrs.clone());
    let conn = from.node.transport().member_conn(id, &addrs).await.unwrap();
    from.node
        .transport()
        .request_one(&conn, &req)
        .await
        .unwrap()
}

/// A long job on `exec`, dispatched by `origin`, and its id once it runs.
async fn long_job(
    origin: &TestNode,
    exec: &TestNode,
) -> (JobId, tokio::sync::mpsc::Receiver<IpcResponse>) {
    let req = IpcRequest::Run {
        command: "echo secret-output; sleep 30".into(),
        on: Some(format!("name={}", exec.config().node_name)),
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let mut rx = stream_as(origin, founder_user_key(origin), req).await;
    let job = loop {
        match rx.recv().await.unwrap() {
            IpcResponse::JobStarted { job_id, .. } => break job_id,
            IpcResponse::Notice { .. } => {}
            other => panic!("{other:?}"),
        }
    };
    (job, rx)
}

#[tokio::test]
async fn a_bystander_cannot_read_or_cancel_a_job_it_did_not_place() {
    let founder = spawn_founder("NIM", "member-reads").await;
    let exec = join_new_node("exec", &founder, vec![]).await;
    let bystander = join_new_node("bystander", &founder, vec![]).await;
    alive(&founder, &[&exec, &bystander]).await;
    alive(&bystander, &[&exec]).await;
    alive(&exec, &[&founder, &bystander]).await;
    let (job, _rx) = long_job(&founder, &exec).await;
    assert!(
        wait_until(Duration::from_secs(20), || exec
            .node
            .jobs()
            .is_running(&job))
        .await
    );

    // The legacy read and cancel, from a member that placed nothing.
    let follow = Request::FollowJob {
        job_id: job,
        stdout_offset: 0,
        stderr_offset: 0,
        follow: false,
    };
    match ask(&bystander, &exec, follow.clone()).await {
        Response::Error { message } => assert!(message.contains("dispatched it"), "{message}"),
        other => panic!("a bystander read the output: {other:?}"),
    }
    match ask(&bystander, &exec, Request::CancelJob { job_id: job }).await {
        Response::Error { message } => assert!(message.contains("unsigned cancel"), "{message}"),
        other => panic!("a bystander cancelled the job: {other:?}"),
    }
    assert!(exec.node.jobs().is_running(&job), "the job was stopped");

    // A signed read, but of a different job, or by a key nobody admitted.
    let admin = founder_user_key(&founder);
    let other_job = admin
        .sign_request(&IpcRequest::JobLogs {
            job_id: JobId::generate(),
            follow: false,
        })
        .unwrap();
    let stranger = Keypair::generate()
        .unwrap()
        .sign_request(&IpcRequest::JobLogs {
            job_id: job,
            follow: false,
        })
        .unwrap();
    for env in [other_job, stranger] {
        let req = Request::FollowJobV2 {
            job_id: job,
            stdout_offset: 0,
            stderr_offset: 0,
            follow: false,
            env: Box::new(env),
        };
        match ask(&bystander, &exec, req).await {
            Response::Error { .. } => {}
            other => panic!("a bad signed read was served: {other:?}"),
        }
    }

    // A person asking the bystander for the logs is served — through V2.
    assert!(
        wait_until(Duration::from_secs(20), || {
            bystander.node.engine().read(|s| s.jobs.contains_key(&job))
        })
        .await
    );
    let resp = run_to_end_logs(&bystander, admin, job).await;
    assert!(resp.contains("secret-output"), "{resp:?}");

    // The origin still reads and cancels as it always has.
    match ask(&founder, &exec, follow).await {
        Response::LogChunk { .. } | Response::JobEnd { .. } => {}
        other => panic!("the origin was refused its own job's output: {other:?}"),
    }
    match ask(&founder, &exec, Request::CancelJob { job_id: job }).await {
        Response::Ok => {}
        other => panic!("the origin could not cancel: {other:?}"),
    }

    bystander.stop().await;
    exec.stop().await;
    founder.stop().await;
}

async fn run_to_end_logs(node: &TestNode, user: &Keypair, job: JobId) -> String {
    let mut rx = stream_as(
        node,
        user,
        IpcRequest::JobLogs {
            job_id: job,
            follow: false,
        },
    )
    .await;
    let mut out = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        match frame {
            IpcResponse::LogChunk { data, .. } => out.push_str(&String::from_utf8_lossy(&data)),
            IpcResponse::Error { message } => return format!("error: {message}"),
            _ => {}
        }
        if out.contains("secret-output") {
            break;
        }
    }
    out
}
