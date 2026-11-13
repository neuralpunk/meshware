//! M5 scheduler behaviors (MASTERPLAN §10.1, §10.2, §11): capacity-derived
//! slots, bounded queues with priorities, Busy backpressure, and scored
//! placement steering work toward idle nodes.

use std::time::Duration;

use mesh_core::config::Limits;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, join_new_node, run_to_end, spawn_founder_with,
    stream_as, wait_until,
};

fn run_req(command: &str, on: Option<&str>, priority: JobPriority) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: on.map(String::from),
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority,
        max_parallel: None,
        halt_on_fail: None,
    }
}

/// Start a run stream and wait until its JobStarted frame arrives, keeping
/// the stream alive in a background drain so the job keeps flowing.
async fn start_job(node: &TestNode, command: &str, on: Option<&str>) -> mesh_proto::JobId {
    let mut rx = stream_as(
        node,
        founder_user_key(node),
        run_req(command, on, JobPriority::Normal),
    )
    .await;
    let mut job_id = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { job_id: id, .. } => {
                job_id = Some(id);
                break;
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                panic!("job did not start: {message}");
            }
            _ => {}
        }
    }
    // Keep draining in the background so the daemon-side stream isn't blocked.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    job_id.expect("JobStarted frame")
}

#[tokio::test]
async fn second_job_queues_when_slots_are_full() {
    let a = spawn_founder_with("alpha", "hyacinth", vec![], |c| {
        c.limits = Limits {
            slots: 1,
            queue: 8,
            min_free_mem_mb: 0,
        };
    })
    .await;

    let filler = start_job(&a, "sleep 2; echo filler", None).await;
    let queued = start_job(&a, "echo queued-then-ran", None).await;

    // While the filler occupies the only slot, the second job must be
    // journaled as Queued in replicated state (MASTERPLAN §10.2).
    let engine = a.node.engine().clone();
    let saw_queued = wait_until(Duration::from_secs(5), || {
        engine.read(|s| {
            s.jobs
                .get(&queued)
                .is_some_and(|j| j.state == JobState::Queued)
                && s.jobs
                    .get(&filler)
                    .is_some_and(|j| j.state == JobState::Running)
        })
    })
    .await;
    assert!(saw_queued, "second job should wait in the queue");

    // Once the filler finishes, the queued job runs to success.
    let both_done = wait_until(Duration::from_secs(20), || {
        engine.read(|s| {
            [filler, queued].iter().all(|id| {
                s.jobs
                    .get(id)
                    .is_some_and(|j| j.state == JobState::Succeeded)
            })
        })
    })
    .await;
    assert!(both_done, "both jobs should complete in order");

    a.node.shutdown();
}

#[tokio::test]
async fn high_priority_jumps_the_queue() {
    let a = spawn_founder_with("alpha", "hyacinth", vec![], |c| {
        c.limits = Limits {
            slots: 1,
            queue: 8,
            min_free_mem_mb: 0,
        };
    })
    .await;

    let _filler = start_job(&a, "sleep 2", None).await;
    // Low first, then high: both queue behind the filler; high must start
    // first when the slot frees (MASTERPLAN §10.2 priorities).
    let low = stream_as(
        &a,
        founder_user_key(&a),
        run_req("echo low", None, JobPriority::Low),
    )
    .await;
    let high = stream_as(
        &a,
        founder_user_key(&a),
        run_req("echo high", None, JobPriority::High),
    )
    .await;
    // Drain both streams concurrently to completion.
    let drain = |mut rx: tokio::sync::mpsc::Receiver<IpcResponse>| async move {
        let mut job = None;
        while let Some(frame) = rx.recv().await {
            match frame {
                IpcResponse::JobStarted { job_id, .. } => job = Some(job_id),
                IpcResponse::JobDone { state, .. } => {
                    assert_eq!(state, JobState::Succeeded);
                    break;
                }
                IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                    panic!("{message}")
                }
                _ => {}
            }
        }
        job.expect("job id")
    };
    let (low_id, high_id) = tokio::join!(drain(low), drain(high));

    let (low_started, high_started) = a.node.engine().read(|s| {
        (
            s.jobs[&low_id].started_at_micros.expect("low started"),
            s.jobs[&high_id].started_at_micros.expect("high started"),
        )
    });
    assert!(
        high_started < low_started,
        "high priority must start before low ({high_started} vs {low_started})"
    );

    a.node.shutdown();
}

#[tokio::test]
async fn saturated_node_refuses_and_work_lands_on_idle_peer() {
    // Founder with one slot and no queue: any second job must go elsewhere.
    let a = spawn_founder_with("alpha", "hyacinth", vec![], |c| {
        c.limits = Limits {
            slots: 1,
            queue: 0,
            min_free_mem_mb: 0,
        };
    })
    .await;
    let b = join_new_node("beta", &a, vec![]).await;

    let engine = a.node.engine().clone();
    let gossip = a.node.gossip().clone();
    let beta_id = b.node.node_id();
    let beta_addr = b.addr_string();
    let ready = wait_until(Duration::from_secs(15), || {
        engine.read(|s| {
            s.is_active_member(&beta_id) && s.members[&beta_id].info.addrs.contains(&beta_addr)
        }) && matches!(gossip.liveness_of(&beta_id), mesh_proto::Liveness::Alive)
    })
    .await;
    assert!(ready, "beta should be alive and addressable");

    // Saturate alpha.
    let _filler = start_job(&a, "sleep 3", Some("name=alpha")).await;

    // Pinned to alpha: the definitive Busy refusal surfaces as an error
    // because no other candidate matches the selector.
    match run_to_end(
        &a,
        founder_user_key(&a),
        run_req("echo never", Some("name=alpha"), JobPriority::Normal),
    )
    .await
    {
        RunOutcome::Error(message) => {
            assert!(
                message.contains("saturated"),
                "expected saturation error, got: {message}"
            );
        }
        other => panic!("expected saturation error, got {other:?}"),
    }

    // Unpinned: the scheduler routes around the saturated node (§10.1/§10.2).
    let mut rx = stream_as(
        &a,
        founder_user_key(&a),
        run_req("echo routed", None, JobPriority::Normal),
    )
    .await;
    let mut executor = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { executor: e, .. } => executor = Some(e),
            IpcResponse::JobDone { state, .. } => {
                assert_eq!(state, JobState::Succeeded);
                break;
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    assert_eq!(executor, Some(beta_id), "work must land on the idle peer");

    a.node.shutdown();
    b.node.shutdown();
}
