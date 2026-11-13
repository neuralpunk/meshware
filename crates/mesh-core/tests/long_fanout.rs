//! ENG-9 — a fan-out that runs longer than an hour can still finish.
//!
//! Every target of a fan-out carries the user's one signed envelope, and an
//! executor refused an envelope more than an hour old. So
//! `run --on tag=prod --max-parallel 5` across a few hundred machines failed
//! every target dispatched after minute sixty, and so did a fan-out a
//! successor adopted. The replicated plan now vouches for its own targets.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{
    FanId, FanoutControls, FanoutInfo, FanoutPlanRecord, IpcRequest, JobId, JobPayload,
    JobPriority, JobSpec, OpBody, PlannedTarget, Request, Response, SignedIpc,
};
use mesh_testkit::{TestNode, founder_user_key, join_new_node, spawn_founder, wait_until};

const HOUR: u64 = 3_600_000_000;

/// The request a user signed two hours ago.
fn old_envelope(user: &mesh_core::identity::Keypair) -> SignedIpc {
    let req = IpcRequest::Run {
        command: "echo late".into(),
        on: None,
        env: vec![],
        timeout_secs: Some(60),
        fanout: Some(0),
        priority: JobPriority::Normal,
        max_parallel: Some(1),
        halt_on_fail: None,
    };
    let payload = postcard::to_stdvec(&req).unwrap();
    let at_micros = mesh_proto::now_micros() - 2 * HOUR;
    let nonce = 7;
    let signature = user
        .sign(&mesh_proto::ipc_signing_bytes(
            &user.user_id(),
            at_micros,
            nonce,
            &payload,
        ))
        .to_vec();
    SignedIpc {
        user: user.user_id(),
        at_micros,
        nonce,
        payload,
        signature,
    }
}

fn spec(plan: &FanoutPlanRecord, job_id: JobId, index: u32) -> JobSpec {
    JobSpec {
        job_id,
        payload: plan.payload.clone(),
        env: plan.env.clone(),
        timeout_secs: plan.timeout_secs,
        priority: plan.priority,
        submitter: plan.submitter,
        submitter_user: plan.submitter_user,
        auth: plan.auth.clone(),
        fanout: Some(FanoutInfo {
            group: plan.group,
            index,
            total: u32::try_from(plan.targets.len()).unwrap(),
        }),
        created_at_micros: plan.created_at_micros,
    }
}

async fn submit(from: &TestNode, to: &TestNode, spec: JobSpec) -> Response {
    let conn = from
        .node
        .transport()
        .member_conn(to.node.node_id(), &[to.addr_string()])
        .await
        .unwrap();
    from.node
        .transport()
        .request_one(&conn, &Request::SubmitJob(spec))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_planned_target_runs_after_its_envelope_is_an_hour_old() {
    let origin = spawn_founder("origin", "eng9-late").await;
    let target = join_new_node("target", &origin, vec![]).await;
    let other = join_new_node("other", &origin, vec![]).await;
    let user = founder_user_key(&origin).clone();

    let planned = JobId::generate();
    let plan = FanoutPlanRecord {
        group: FanId::generate(),
        payload: JobPayload::Command("echo late".into()),
        env: vec![],
        timeout_secs: Some(60),
        priority: JobPriority::Normal,
        submitter: origin.node.node_id(),
        submitter_user: user.user_id(),
        auth: old_envelope(&user),
        targets: vec![PlannedTarget {
            job_id: planned,
            node_id: target.node.node_id(),
            node_name: "target".into(),
        }],
        controls: FanoutControls {
            max_parallel: Some(1),
            halt_on_fail: None,
        },
        created_at_micros: mesh_proto::now_micros(),
    };
    origin
        .node
        .engine()
        .author_op(OpBody::FanoutPlan {
            plan: Box::new(plan.clone()),
        })
        .unwrap();
    for node in [&target, &other] {
        assert!(
            wait_until(Duration::from_secs(15), || node
                .node
                .engine()
                .read(|s| s.fanouts.contains_key(&plan.group)))
            .await,
            "the plan never reached {:?}",
            node
        );
    }

    let expired = |resp: &Response| matches!(resp, Response::Error { message } if message.contains("expired"));

    // Not a job the plan names: the envelope is just old.
    let stray = submit(&origin, &target, spec(&plan, JobId::generate(), 0)).await;
    assert!(expired(&stray), "{stray:?}");
    // The planned job, on a node the plan does not name: old too.
    let elsewhere = submit(&origin, &other, spec(&plan, planned, 0)).await;
    assert!(expired(&elsewhere), "{elsewhere:?}");
    // The planned job on the planned node: the plan vouches for it.
    let accepted = submit(&origin, &target, spec(&plan, planned, 0)).await;
    assert!(
        matches!(accepted, Response::JobAccepted { .. }),
        "{accepted:?}"
    );

    // Once the group is closed, nothing more rides on it.
    origin
        .node
        .engine()
        .author_op(OpBody::FanoutClosed {
            group: plan.group,
            succeeded: 1,
            failed: 0,
            skipped: 0,
        })
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(15), || target.node.engine().read(|s| s
            .fanouts
            .get(&plan.group)
            .is_some_and(|e| e.closed.is_some())))
        .await
    );
    let after_close = submit(&origin, &target, spec(&plan, planned, 0)).await;
    assert!(expired(&after_close), "{after_close:?}");

    other.stop().await;
    target.stop().await;
    origin.stop().await;
}
