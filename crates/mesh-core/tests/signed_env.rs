//! **A job runs in the environment the user signed** (SEC-4).
//!
//! The executor compared a job's command with the user's envelope and
//! applied its environment as given. Fan-out plans carry the envelope to
//! every member for an hour, so any member could replay a user's command
//! with `PATH`, `LD_PRELOAD` or `BASH_FUNC_…` of its own — code execution
//! as that user. These tests replay a real envelope, lifted from a real
//! plan, with each of those changes, and expect each to be refused.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, JobId, JobPriority, JobSpec, Liveness, Request, Response};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, join_new_node, run_to_end, spawn_founder, stream_as,
    wait_until,
};

async fn submit_raw(from: &TestNode, target: &TestNode, spec: JobSpec) -> Response {
    let id = target.node.node_id();
    let addrs = from
        .node
        .engine()
        .read(|s| s.members.get(&id).map(|m| m.info.addrs.clone()))
        .unwrap_or_default();
    let conn = from
        .node
        .transport()
        .member_conn(id, &addrs)
        .await
        .expect("dial the target");
    from.node
        .transport()
        .request_one(&conn, &Request::SubmitJob(spec))
        .await
        .expect("a reply")
}

fn refused(resp: Response, why: &str) {
    match resp {
        Response::Error { message } => assert!(message.contains(why), "wanted {why:?}: {message}"),
        other => panic!("a tampered replay was not refused ({why}): {other:?}"),
    }
}

#[tokio::test]
async fn a_replayed_envelope_cannot_carry_a_different_environment() {
    let founder = spawn_founder("NIM", "signed-env").await;
    let rogue = join_new_node("mallory", &founder, vec![]).await;
    let victim = join_new_node("victim", &founder, vec![]).await;
    for node in [&rogue, &victim] {
        let id = node.node.node_id();
        assert!(
            wait_until(Duration::from_secs(20), || {
                founder.node.engine().read(|s| s.is_active_member(&id))
                    && matches!(founder.node.gossip().liveness_of(&id), Liveness::Alive)
            })
            .await
        );
    }

    // The founder fans a harmless command out to the victim, with an
    // environment and a deadline of their own.
    let fan = IpcRequest::Run {
        command: "echo hello".into(),
        on: Some(format!("name={}", victim.config().node_name)),
        env: vec![("GREETING".into(), "hi".into())],
        timeout_secs: Some(30),
        fanout: Some(0),
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let mut rx = stream_as(&founder, founder_user_key(&founder), fan).await;
    while rx.recv().await.is_some() {}

    // Every member has the plan, envelope and all.
    let mut plan = None;
    assert!(
        wait_until(Duration::from_secs(20), || {
            plan = rogue
                .node
                .engine()
                .read(|s| s.fanouts.values().next().map(|e| e.plan.clone()));
            plan.is_some()
        })
        .await
    );
    let plan = plan.unwrap();
    let replay = |env: Vec<(String, String)>, timeout_secs: Option<u64>| JobSpec {
        job_id: JobId::generate(),
        payload: plan.payload.clone(),
        env,
        timeout_secs,
        priority: plan.priority,
        submitter: rogue.node.node_id(),
        submitter_user: plan.submitter_user,
        auth: plan.auth.clone(),
        fanout: None,
        created_at_micros: mesh_proto::now_micros(),
    };
    let with = |extra: (&str, &str)| {
        let mut env = plan.env.clone();
        env.push((extra.0.into(), extra.1.into()));
        env
    };

    refused(
        submit_raw(
            &rogue,
            &victim,
            replay(with(("PATH", "/tmp/evil")), Some(30)),
        )
        .await,
        "environment does not match",
    );
    refused(
        submit_raw(&rogue, &victim, replay(Vec::new(), Some(30))).await,
        "environment does not match",
    );
    refused(
        submit_raw(
            &rogue,
            &victim,
            replay(with(("LD_PRELOAD", "/tmp/x.so")), Some(30)),
        )
        .await,
        "never accepted",
    );
    refused(
        submit_raw(
            &rogue,
            &victim,
            replay(with(("BASH_FUNC_echo%%", "() { id; }")), Some(30)),
        )
        .await,
        "never accepted",
    );
    refused(
        submit_raw(&rogue, &victim, replay(plan.env.clone(), None)).await,
        "timeout does not match",
    );
    // meshware's own keys are the origin's to add; they are not a way in.
    let reserved = with(("MESHWARE_TASK_PREEMPTIBLE", "1"));
    assert!(mesh_proto::env_matches_signed(&reserved, &plan.env));

    victim.stop().await;
    rogue.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn the_origin_says_so_before_anything_is_dispatched() {
    let founder = spawn_founder("NIM", "signed-env-origin").await;
    let req = IpcRequest::Run {
        command: "true".into(),
        on: None,
        env: vec![("LD_PRELOAD".into(), "/tmp/x.so".into())],
        timeout_secs: Some(10),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match run_to_end(&founder, founder_user_key(&founder), req).await {
        RunOutcome::Error(message) => {
            assert!(message.contains("LD_PRELOAD cannot be set"), "{message}")
        }
        other => panic!("LD_PRELOAD was accepted: {other:?}"),
    }
    // An ordinary environment still runs, and reaches the task.
    let req = IpcRequest::Run {
        command: "echo $GREETING".into(),
        on: None,
        env: vec![("GREETING".into(), "hi".into())],
        timeout_secs: Some(10),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match run_to_end(&founder, founder_user_key(&founder), req).await {
        RunOutcome::Done { stdout, .. } => assert_eq!(stdout.trim(), "hi"),
        other => panic!("run: {other:?}"),
    }
    founder.stop().await;
}
