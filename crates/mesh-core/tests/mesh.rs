//! M1 exit criteria: invite/join, gossip convergence, and remote job
//! execution with streaming + cancel.

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobState, Liveness};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder,
    spawn_founder_tagged, stream_as, wait_until,
};

#[tokio::test]
async fn join_via_invite_makes_a_member() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec!["gpu".into()]).await;

    // Beta knows it is a member immediately (join seeded its op-log).
    assert!(
        beta.node
            .engine()
            .read(|s| s.is_active_member(&beta.node.node_id()))
    );
    // Beta sees the founder too.
    assert!(
        beta.node
            .engine()
            .read(|s| s.is_active_member(&founder.node.node_id()))
    );

    // The founder learns about beta via gossip within a few seconds.
    let founder_engine = founder.node.engine().clone();
    let beta_id = beta.node.node_id();
    let converged = wait_until(Duration::from_secs(10), || {
        founder_engine.read(|s| s.is_active_member(&beta_id))
    })
    .await;
    assert!(converged, "founder should learn beta via gossip");

    founder.node.shutdown();
    beta.node.shutdown();
}

/// `ping <node>` names a member and lets the daemon choose the route, so
/// a frontend never has to pick an address out of the announced list (the
/// old shape of this call, which picked the peer's loopback and reported a
/// live node as unreachable).
#[tokio::test]
async fn ping_by_node_id_reaches_a_member() {
    let founder = spawn_founder("alpha", "hyacinth").await;
    let beta = join_new_node("beta", &founder, vec![]).await;
    let user = founder_user_key(&founder).clone();
    let beta_id = beta.node.node_id();

    // Wait for beta's own announcement, not just its membership: the
    // addresses are what the daemon is about to choose between.
    let founder_engine = founder.node.engine().clone();
    assert!(
        wait_until(Duration::from_secs(10), || founder_engine.read(|s| s
            .members
            .get(&beta_id)
            .is_some_and(|m| !m.info.addrs.is_empty())))
        .await
    );

    match ipc_as(&founder, &user, IpcRequest::PingNode { node: beta_id }).await {
        IpcResponse::Ping(p) => {
            assert_eq!(p.peer_node_id, beta_id);
            assert_eq!(p.peer_node_name, "beta");
            assert!(p.same_mesh);
            assert!(!p.via.is_empty(), "the answering route is reported");
        }
        other => panic!("expected a ping result, got {other:?}"),
    }

    // A node nobody has heard of is an error, not a timeout against some
    // address that happens to be lying around.
    let stranger = mesh_proto::NodeId([9u8; 32]);
    match ipc_as(&founder, &user, IpcRequest::PingNode { node: stranger }).await {
        IpcResponse::Error { message } => assert!(message.contains("unknown node"), "{message}"),
        other => panic!("expected an error, got {other:?}"),
    }

    founder.node.shutdown();
    beta.node.shutdown();
}

#[tokio::test]
async fn three_node_gossip_converges() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("beta", &a, vec![]).await;
    let c = join_new_node("gamma", &a, vec![]).await;

    // All three should eventually see all three as active members.
    for node in [&a, &b, &c] {
        let engine = node.node.engine().clone();
        let (ida, idb, idc) = (a.node.node_id(), b.node.node_id(), c.node.node_id());
        let ok = wait_until(Duration::from_secs(15), || {
            engine.read(|s| {
                s.is_active_member(&ida) && s.is_active_member(&idb) && s.is_active_member(&idc)
            })
        })
        .await;
        assert!(
            ok,
            "node {} should see all members",
            node.node.node_id().short()
        );
    }

    a.node.shutdown();
    b.node.shutdown();
    c.node.shutdown();
}

/// Collect a streamed run to completion as the founding admin:
/// returns (stdout, exit state, code).
async fn collect_run(
    node: &TestNode,
    command: &str,
    on: Option<String>,
) -> (String, JobState, Option<i32>) {
    let req = IpcRequest::Run {
        command: command.to_string(),
        on,
        env: vec![],
        timeout_secs: Some(30),
        fanout: None,
        priority: mesh_proto::JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match mesh_testkit::run_to_end(node, founder_user_key(node), req).await {
        RunOutcome::Done {
            stdout,
            state,
            code,
            ..
        } => (stdout, state, code),
        RunOutcome::Denied(m) => panic!("run denied: {m}"),
        RunOutcome::Error(m) => panic!("run error: {m}"),
    }
}

#[tokio::test]
async fn local_run_streams_output() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let (out, state, code) = collect_run(&a, "echo hello-mesh; exit 0", None).await;
    assert_eq!(state, JobState::Succeeded);
    assert_eq!(code, Some(0));
    assert!(out.contains("hello-mesh"), "stdout was {out:?}");
    a.node.shutdown();
}

#[tokio::test]
async fn remote_run_streams_from_executor() {
    let a = spawn_founder("alpha", "hyacinth").await;
    let b = join_new_node("beta", &a, vec!["worker".into()]).await;

    // Wait until alpha can see beta as alive so the selector resolves to it.
    let engine = a.node.engine().clone();
    let gossip = a.node.gossip().clone();
    let beta_id = b.node.node_id();
    let ready = wait_until(Duration::from_secs(10), || {
        engine.read(|s| s.is_active_member(&beta_id))
            && matches!(gossip.liveness_of(&beta_id), Liveness::Alive)
    })
    .await;
    assert!(ready, "alpha should see beta alive");

    // Run targeted at beta by tag; output must come back to alpha (submitter).
    let (out, state, code) = collect_run(
        &a,
        "echo from-$(hostname 2>/dev/null || echo beta)",
        Some("tag=worker".into()),
    )
    .await;
    assert_eq!(state, JobState::Succeeded, "stdout: {out}");
    assert_eq!(code, Some(0));
    assert!(out.starts_with("from-"), "stdout was {out:?}");

    // The job summary replicated back to alpha's state.
    let seen = wait_until(Duration::from_secs(5), || {
        a.node
            .engine()
            .read(|s| s.jobs.values().any(|j| j.executor == beta_id))
    })
    .await;
    assert!(seen, "alpha should see beta's job summary");

    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn cancel_stops_a_running_job() {
    let a = spawn_founder_tagged("alpha", "hyacinth", vec![]).await;

    // Long-runner; we cancel it mid-flight.
    let mut rx = stream_as(
        &a,
        founder_user_key(&a),
        IpcRequest::Run {
            command: "echo starting; sleep 60; echo done".into(),
            on: None,
            env: vec![],
            timeout_secs: None,
            fanout: None,
            priority: mesh_proto::JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;

    // Wait for the job to start and produce its first line.
    let mut job_id = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::JobStarted { job_id: id, .. } => job_id = Some(id),
            IpcResponse::LogChunk { .. } => break, // "starting" arrived: it's running
            other => panic!("unexpected before cancel: {other:?}"),
        }
    }
    let job_id = job_id.expect("job started");

    // Cancel it.
    let resp = ipc_as(
        &a,
        founder_user_key(&a),
        mesh_proto::IpcRequest::JobCancel { job_id },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "cancel response: {resp:?}");

    // The stream should terminate as Cancelled, not run the full 60s.
    let done = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(msg) = rx.recv().await {
            if let IpcResponse::JobDone { state, .. } = msg {
                return state;
            }
        }
        JobState::Failed
    })
    .await
    .expect("cancel should end the job well under the sleep");
    assert_eq!(done, JobState::Cancelled);

    a.node.shutdown();
}
