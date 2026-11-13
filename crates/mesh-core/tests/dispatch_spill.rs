//! ENG-6 — a submission goes elsewhere when its first node cannot be
//! reached, and a fan-out waits out a target that is restarting.
//!
//! Only a `Busy` answer used to move a job to the next candidate. A dial
//! that failed failed the job, although nothing had been delivered — so
//! during a rolling upgrade `run` failed instead of landing on a node that
//! was up, and a fan-out target that was restarting was simply lost.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState, Liveness};
use mesh_testkit::{
    TestNode, capacity, founder_user_key, join_new_node_sized, spawn_founder_sized, stream_as,
    wait_until,
};

fn run_on(on: Option<&str>, fanout: Option<u32>) -> IpcRequest {
    run_cmd("echo ok", on, fanout)
}

fn run_cmd(command: &str, on: Option<&str>, fanout: Option<u32>) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: on.map(String::from),
        env: vec![],
        timeout_secs: Some(60),
        fanout,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

async fn ready(from: &TestNode, peers: &[&TestNode]) {
    let ok = wait_until(Duration::from_secs(20), || {
        peers.iter().all(|p| {
            let id = p.node.node_id();
            let addr = p.addr_string();
            from.node.engine().read(|s| {
                s.is_active_member(&id)
                    && s.members[&id].info.addrs.contains(&addr)
                    && s.members[&id].info.tags.iter().any(|t| t == "pool")
            }) && matches!(from.node.gossip().liveness_of(&id), Liveness::Alive)
        })
    })
    .await;
    assert!(ok, "peers never became dialable");
}

/// Run one job and say where it ran and how it ended.
async fn run_once(entry: &TestNode, req: IpcRequest) -> Result<(String, JobState), String> {
    let mut rx = stream_as(entry, founder_user_key(entry), req).await;
    let mut ran_on = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { executor_name, .. } => ran_on = Some(executor_name),
            IpcResponse::JobDone { state, .. } => {
                return Ok((ran_on.unwrap_or_default(), state));
            }
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                return Err(message);
            }
            _ => {}
        }
    }
    Err("stream ended".into())
}

fn spills(node: &TestNode) -> u64 {
    node.node
        .metrics_text()
        .lines()
        .find_map(|l| l.strip_prefix("meshware_dispatch_spills_total "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_spills_past_a_candidate_that_cannot_be_reached() {
    // The entry node takes no part: it is not in the pool.
    let entry = spawn_founder_sized("entry", "eng6-spill", capacity(2, 2, 10)).await;
    let mut pool = Vec::new();
    for name in ["p1", "p2"] {
        pool.push(join_new_node_sized_tagged(name, &entry).await);
    }
    ready(&entry, &[&pool[0], &pool[1]]).await;

    // Occupy one pool node, so placement prefers the other: whichever
    // takes this job is where the next one has to end up.
    let busy_on = {
        let mut rx = stream_as(
            &entry,
            founder_user_key(&entry),
            run_cmd("sleep 30", Some("tag=pool"), None),
        )
        .await;
        let mut on = None;
        while let Some(frame) = rx.recv().await {
            if let IpcResponse::JobStarted { executor, .. } = frame {
                on = Some(executor);
                break;
            }
        }
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        on.expect("the long job never started")
    };
    let (busy, idle): (&TestNode, &TestNode) = if pool[0].node.node_id() == busy_on {
        (&pool[0], &pool[1])
    } else {
        (&pool[1], &pool[0])
    };
    assert!(
        wait_until(Duration::from_secs(15), || entry.node.engine().read(|s| {
            s.jobs
                .values()
                .any(|j| j.executor == busy_on && !j.state.is_terminal())
        }))
        .await,
        "the entry never learnt the pool node was busy"
    );

    // The idle node goes away without a word, and the entry node still
    // thinks it is alive: the next job is sent there first, cannot be
    // delivered, and lands on the busy one.
    idle.stop().await;
    assert!(matches!(
        entry.node.gossip().liveness_of(&idle.node.node_id()),
        Liveness::Alive
    ));
    let busy_name = busy.node.config().node_name.clone();
    let (ran_on, state) = run_once(&entry, run_on(Some("tag=pool"), None))
        .await
        .expect("the job failed instead of spilling");
    assert_eq!((ran_on, state), (busy_name, JobState::Succeeded));
    assert!(spills(&entry) >= 1, "no spill was counted");
    let metrics = entry.node.metrics_text();
    assert!(
        metrics.contains("meshware_dispatch_failures_total{reason=\"unreachable\"} 1"),
        "{metrics}"
    );
}

/// A pool node with two slots: one running job is half of it, which puts
/// the busy node clearly behind the idle one — ahead of the small penalty
/// the idle node picks up once its connection is gone and its round-trip
/// time is unknown.
async fn join_new_node_sized_tagged(name: &str, founder: &TestNode) -> TestNode {
    mesh_testkit::join_new_node_with(name, founder, vec!["pool".into()], |c| {
        mesh_testkit::apply_capacity_for_test(c, capacity(4, 4, 10));
        c.limits.slots = 2;
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fan_out_waits_out_a_target_that_is_restarting() {
    let entry = spawn_founder_sized("entry", "eng6-fan", capacity(2, 2, 10)).await;
    let target = join_new_node_sized("target", &entry, capacity(2, 2, 10)).await;
    let _ = ready_any(&entry, &target).await;

    // The target is down when the fan-out starts, and back three seconds
    // later — on a new port, which the entry learns from its announcement.
    target.stop().await;
    let entry_node = entry.node.clone();
    let user = founder_user_key(&entry).clone();
    let fan = tokio::spawn(async move {
        let env = user.sign_request(&run_on(None, Some(0))).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<IpcResponse>(64);
        let node = entry_node.clone();
        tokio::spawn(async move { node.handle_signed_stream(env, tx).await });
        while let Some(frame) = rx.recv().await {
            match frame {
                IpcResponse::FanoutDone {
                    succeeded, failed, ..
                } => return Ok((succeeded, failed)),
                IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                    return Err(message);
                }
                _ => {}
            }
        }
        Err("stream ended".to_string())
    });
    tokio::time::sleep(Duration::from_secs(3)).await;
    let target = target.restart().await;
    let (succeeded, failed) = tokio::time::timeout(Duration::from_secs(60), fan)
        .await
        .expect("the fan-out never finished")
        .unwrap()
        .unwrap();
    assert_eq!(
        (succeeded, failed),
        (2, 0),
        "the restarting target was not waited for"
    );
    drop(target);
}

async fn ready_any(from: &TestNode, peer: &TestNode) {
    let id = peer.node.node_id();
    let addr = peer.addr_string();
    let ok = wait_until(Duration::from_secs(20), || {
        from.node
            .engine()
            .read(|s| s.is_active_member(&id) && s.members[&id].info.addrs.contains(&addr))
            && matches!(from.node.gossip().liveness_of(&id), Liveness::Alive)
    })
    .await;
    assert!(ok, "peer never became dialable");
}
