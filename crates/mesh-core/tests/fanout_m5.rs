//! M5 fan-out at depth (MASTERPLAN §9.3, §10.3): halt-on-fail with
//! max-parallel windows, and the milestone exit demo — kill a fan-out's
//! origin node mid-run and watch the job complete under adopted ownership.

use std::time::Duration;

use mesh_proto::{FanId, HaltPolicy, IpcRequest, IpcResponse, JobPriority, JobState, Liveness};
use mesh_testkit::{
    TestNode, founder_user_key, join_new_node, spawn_founder, stream_as, wait_until,
};

fn fanout_req(
    command: &str,
    max_parallel: Option<u32>,
    halt_on_fail: Option<HaltPolicy>,
) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(120),
        fanout: Some(0), // all matching nodes
        priority: JobPriority::Normal,
        max_parallel,
        halt_on_fail,
    }
}

/// Wait until the submitter can dispatch to both workers (alive + real
/// addresses gossiped in).
async fn wait_three_ready(a: &TestNode, b: &TestNode, c: &TestNode) {
    let engine = a.node.engine().clone();
    let gossip = a.node.gossip().clone();
    let (idb, idc) = (b.node.node_id(), c.node.node_id());
    let (addr_b, addr_c) = (b.addr_string(), c.addr_string());
    let ok = wait_until(Duration::from_secs(15), || {
        engine.read(|s| {
            s.is_active_member(&idb)
                && s.is_active_member(&idc)
                && s.members[&idb].info.addrs.contains(&addr_b)
                && s.members[&idc].info.addrs.contains(&addr_c)
        }) && matches!(gossip.liveness_of(&idb), Liveness::Alive)
            && matches!(gossip.liveness_of(&idc), Liveness::Alive)
    })
    .await;
    assert!(ok, "all members should be alive and addressable");
}

#[tokio::test]
async fn halt_on_fail_skips_remaining_targets() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("beta", &a, vec![]).await;
    let c = join_new_node("gamma", &a, vec![]).await;
    wait_three_ready(&a, &b, &c).await;

    // Every target fails; with a window of one and a threshold of one, the
    // first failure must stop dispatch and skip the remaining two targets.
    let mut rx = stream_as(
        &a,
        founder_user_key(&a),
        fanout_req("exit 7", Some(1), Some(HaltPolicy::Count(1))),
    )
    .await;
    let mut done = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::FanoutDone {
                succeeded,
                failed,
                skipped,
            } => {
                done = Some((succeeded, failed, skipped));
                break;
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    let (succeeded, failed, skipped) = done.expect("FanoutDone");
    assert_eq!(succeeded, 0);
    assert_eq!(failed, 1, "the window admits exactly one failure");
    assert_eq!(skipped, 2, "the other targets must be skipped");

    // The replicated group record carries the same outcome.
    let group_closed = a.node.engine().read(|s| {
        s.fanouts
            .values()
            .any(|e| e.closed.is_some_and(|o| o.failed == 1 && o.skipped == 2))
    });
    assert!(group_closed, "FanoutClosed must replicate the outcome");

    a.node.shutdown();
    b.node.shutdown();
    c.node.shutdown();
}

/// The M5 exit demo (MASTERPLAN §17): kill a fan-out's origin mid-run; a
/// successor adopts the plan and completes the remaining dispatches.
#[tokio::test]
async fn origin_death_mid_fanout_is_adopted_and_completed() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("beta", &a, vec![]).await;
    let c = join_new_node("gamma", &a, vec![]).await;
    wait_three_ready(&a, &b, &c).await;

    // A window of one serializes dispatch: when we kill the origin during
    // the first (slow) sub-job, the remaining targets are still pending —
    // only adoption can dispatch them.
    let mut rx = stream_as(
        &a,
        founder_user_key(&a),
        fanout_req("sleep 4; echo adopted-ok", Some(1), None),
    )
    .await;
    let mut group: Option<FanId> = None;
    let mut targets = Vec::new();
    while let Some(frame) = rx.recv().await {
        if let IpcResponse::FanoutStarted {
            group: g,
            targets: t,
        } = frame
        {
            group = Some(g);
            targets = t;
            break;
        }
    }
    let group = group.expect("FanoutStarted");
    assert_eq!(targets.len(), 3);

    // The plan must have replicated to the survivors before we pull the plug.
    let (be, ce) = (b.node.engine().clone(), c.node.engine().clone());
    let plan_replicated = wait_until(Duration::from_secs(10), || {
        be.read(|s| s.fanouts.contains_key(&group)) && ce.read(|s| s.fanouts.contains_key(&group))
    })
    .await;
    assert!(plan_replicated, "fan-out plan should replicate via gossip");

    // Kill the origin mid-fan-out.
    drop(rx);
    a.node.shutdown();

    // Failure detection (~16s) + adoption + serialized dispatch of the
    // remaining sleep-4 targets: allow a generous window.
    let closed = wait_until(Duration::from_secs(120), || {
        be.read(|s| s.fanouts.get(&group).is_some_and(|e| e.closed.is_some()))
    })
    .await;
    assert!(closed, "a successor must adopt and close the fan-out");

    let (outcome, adopters) = be.read(|s| {
        let e = &s.fanouts[&group];
        (e.closed.expect("closed"), e.adopters.clone())
    });
    assert!(
        !adopters.is_empty(),
        "adoption must be recorded in replicated state"
    );
    assert!(
        !adopters.contains(&a.node.node_id()),
        "the dead origin cannot be the adopter"
    );
    // The two surviving targets ran to success under adopted ownership; the
    // dead origin's own target is failed or skipped, never left open.
    assert_eq!(outcome.succeeded, 2, "outcome: {outcome:?}");
    assert_eq!(outcome.failed + outcome.skipped, 1, "outcome: {outcome:?}");

    // Both survivors' sub-jobs are terminal and successful in mesh state.
    let survivor_jobs_ok = be.read(|s| {
        let plan = &s.fanouts[&group].plan;
        plan.targets
            .iter()
            .filter(|t| t.node_id != a.node.node_id())
            .all(|t| {
                s.jobs
                    .get(&t.job_id)
                    .is_some_and(|j| j.state == JobState::Succeeded)
            })
    });
    assert!(survivor_jobs_ok, "surviving targets must have succeeded");

    b.node.shutdown();
    c.node.shutdown();
}
