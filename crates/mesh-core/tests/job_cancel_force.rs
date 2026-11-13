//! Cancel means cancel, even with nobody to deliver it to.
//!
//! Only the executor may report a job's state — that rule is what makes a
//! summary trustworthy, and it has one failure mode: an executor that
//! published `running` and then vanished for good leaves a row that reads
//! `running` forever, with a cancel that has nobody to talk to. The
//! Director's 2026-09 field report was exactly this — two jobs "running"
//! on executors that no longer existed, and a cancel button that flashed
//! an error and changed nothing.
//!
//! `JobCancelForce` is the override for that silence: the user's signature
//! carries the authority, every peer applies it alone, and a late report
//! from an executor that turns out to be alive still wins by HLC. The
//! state-level laws (commutation with the upsert it overrides, the late
//! report winning, unauthorized forces parking) live in `state.rs` unit
//! tests; this test is the operator's-eye view through the daemon.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    Grant, GrantId, IpcRequest, IpcResponse, JobId, JobPriority, JobRow, JobState, Liveness, Role,
    UserRecord,
};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, stream_as, wait_until,
};

fn run_req(command: &str, on: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: Some(on.into()),
        env: vec![],
        timeout_secs: Some(120),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

async fn jobs(node: &TestNode) -> Vec<JobRow> {
    match ipc_as(node, founder_user_key(node), IpcRequest::Jobs).await {
        IpcResponse::Jobs(rows) => rows,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Register a user + one unscoped grant through the founder's signed path.
async fn add_user(node: &TestNode, user: &Keypair, username: &str, role: Role) {
    let admin = founder_user_key(node);
    let mut record = UserRecord {
        user_id: user.user_id(),
        username: username.into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    let resp = ipc_as(node, admin, IpcRequest::UserAdd { record }).await;
    assert!(matches!(resp, IpcResponse::Ok), "user add: {resp:?}");
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id: user.user_id(),
        role,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    let resp = ipc_as(node, admin, IpcRequest::GrantAdd { grant }).await;
    assert!(matches!(resp, IpcResponse::Ok), "grant add: {resp:?}");
}

fn force_req(user: &Keypair, job_id: JobId) -> IpcRequest {
    let by = user.user_id();
    let signature = user
        .sign(&mesh_proto::job_cancel_force_signing_bytes(&job_id, &by))
        .to_vec();
    IpcRequest::JobCancelForce {
        job_id,
        by,
        signature,
    }
}

async fn until(node: &TestNode, f: impl Fn(&[JobRow]) -> bool) -> Vec<JobRow> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let rows = jobs(node).await;
        if f(&rows) || std::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn force_cancel_settles_a_job_whose_executor_vanished() {
    let a = spawn_founder("alpha", "force-cancel").await;
    let b = join_new_node("beta", &a, vec![]).await;
    let user = founder_user_key(&a);

    // Placement needs a live candidate, not merely a joined one.
    let beta = b.node.node_id();
    let gossip = a.node.gossip().clone();
    let engine = a.node.engine().clone();
    let ok = wait_until(Duration::from_secs(40), || {
        engine.read(|s| s.is_active_member(&beta)) && gossip.liveness_of(&beta) == Liveness::Alive
    })
    .await;
    assert!(ok, "alpha should see beta alive");

    // A long job placed on beta. The stream stays bound and undrained —
    // the job is meant to still be in flight when its executor goes.
    let _stream = stream_as(&a, user, run_req("sleep 30", "beta")).await;
    let rows = until(&a, |rows| rows.iter().any(|j| j.state == JobState::Running)).await;
    let job_id: JobId = rows
        .iter()
        .find(|j| j.state == JobState::Running)
        .expect("the sleep should be running on beta")
        .job_id;

    // Beta vanishes without a word: no graceful stop, no terminal report,
    // and — the case this op exists for — no way for it to ever send one.
    b.node.shutdown();

    // The ordinary cancel has nobody to talk to; the forced one does not
    // need anybody. The daemon tries the executor first and says which of
    // the two happened, so the assertion is on the *force* having landed.
    let by = user.user_id();
    let signature = user
        .sign(&mesh_proto::job_cancel_force_signing_bytes(&job_id, &by))
        .to_vec();
    match ipc_as(
        &a,
        user,
        IpcRequest::JobCancelForce {
            job_id,
            by,
            signature,
        },
    )
    .await
    {
        IpcResponse::Notice { message } => assert!(
            message.contains("could not be reached"),
            "the executor is gone, so the force path should have fired: {message}"
        ),
        other => panic!("unexpected: {other:?}"),
    }

    let rows = until(&a, |rows| {
        rows.iter()
            .any(|j| j.job_id == job_id && j.state.is_terminal())
    })
    .await;
    let row = rows.iter().find(|j| j.job_id == job_id).expect("job row");
    assert_eq!(
        row.state,
        JobState::Cancelled,
        "cancel means cancel: {row:?}"
    );
    assert!(
        row.finished_at_micros.is_some(),
        "a forced cancel must finish the job, or a jobs clear can never forget it"
    );

    a.node.shutdown();
}

/// The escalation is a separate power from the cancel: a developer whose
/// *own* job is stranded on a vanished executor passes the ordinary gate,
/// and is refused only at the point of rewriting the record — with the
/// refusal saying both what failed and who can finish it. An admin then
/// can, for anybody's job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_developer_is_refused_the_escalation_an_admin_gets() {
    let a = spawn_founder("alpha", "force-scope").await;
    let b = join_new_node("beta", &a, vec![]).await;

    let beta = b.node.node_id();
    let gossip = a.node.gossip().clone();
    let engine = a.node.engine().clone();
    let ok = wait_until(Duration::from_secs(40), || {
        engine.read(|s| s.is_active_member(&beta)) && gossip.liveness_of(&beta) == Liveness::Alive
    })
    .await;
    assert!(ok, "alpha should see beta alive");

    let dana = Keypair::generate().unwrap();
    add_user(&a, &dana, "dana", Role::Developer).await;
    // Beta must know dana before it will run her work.
    let b_engine = b.node.engine().clone();
    let dana_id = dana.user_id();
    assert!(
        wait_until(Duration::from_secs(10), || {
            b_engine.read(|s| {
                s.users.contains_key(&dana_id)
                    && s.grants.values().any(|g| g.grant.user_id == dana_id)
            })
        })
        .await,
        "dana should replicate to beta"
    );

    // Dana's own long job, placed on beta — hers to cancel.
    let _stream = stream_as(&a, &dana, run_req("sleep 30", "beta")).await;
    let rows = until(&a, |rows| rows.iter().any(|j| j.state == JobState::Running)).await;
    let job_id = rows
        .iter()
        .find(|j| j.state == JobState::Running)
        .expect("the sleep should be running on beta")
        .job_id;

    b.node.shutdown();

    // Her cancel passes the ownership gate, cannot reach the executor, and
    // is refused exactly at the escalation — told what failed and who can.
    match ipc_as(&a, &dana, force_req(&dana, job_id)).await {
        IpcResponse::Denied { message } => {
            assert!(
                message.contains("could not be reached"),
                "what failed: {message}"
            );
            assert!(
                message.contains("operator or admin"),
                "who can finish it: {message}"
            );
        }
        other => panic!("dana must be refused the escalation: {other:?}"),
    }
    // Refused means untouched: still running, for the admin to end.
    let engine = a.node.engine().clone();
    assert!(
        engine.read(|s| s.jobs.get(&job_id).is_some_and(|j| !j.state.is_terminal())),
        "a refused escalation must not rewrite the record"
    );

    // Admins have full power over anybody's job.
    let admin = founder_user_key(&a);
    match ipc_as(&a, admin, force_req(admin, job_id)).await {
        IpcResponse::Notice { message } => assert!(
            message.contains("recorded as cancelled"),
            "the force path should have fired: {message}"
        ),
        other => panic!("the admin's force must land: {other:?}"),
    }
    let rows = until(&a, |rows| {
        rows.iter()
            .any(|j| j.job_id == job_id && j.state.is_terminal())
    })
    .await;
    let row = rows.iter().find(|j| j.job_id == job_id).expect("job row");
    assert_eq!(row.state, JobState::Cancelled, "{row:?}");

    a.node.shutdown();
}
