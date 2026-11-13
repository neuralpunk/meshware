//! M7 exit demo (MASTERPLAN §17): **a home laptop behind CGNAT joins a
//! cloud mesh with one token** — and is then fully usable: the founder and
//! another public member dispatch jobs to it and stream its logs.
//!
//! On loopback: the "cloud" founder relays (`[wan] relay = "on"`), the
//! laptop sits behind a symmetric userspace NAT (the harshest profile — its
//! own address is a black hole, nothing it publishes is dialable, punches
//! cannot help), it joins with a single token, converges, registers with
//! the founder's relay, and work flows to it.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::transport::ConnKind;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState, Liveness, RELAY_TAG};
use mesh_testkit::nat::{NatProfile, TestNat};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, ipc_as, join_new_node, join_new_node_natted,
    run_to_end, spawn_founder_with, stream_as, wait_until,
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

async fn wait_alive(viewer: &TestNode, target: &TestNode) {
    let id = target.node.node_id();
    let gossip = viewer.node.gossip().clone();
    let engine = viewer.node.engine().clone();
    let ok = wait_until(Duration::from_secs(40), || {
        engine.read(|s| s.is_active_member(&id)) && gossip.liveness_of(&id) == Liveness::Alive
    })
    .await;
    assert!(
        ok,
        "{} should see {} alive",
        viewer.config().node_name,
        target.config().node_name
    );
}

#[tokio::test]
async fn home_laptop_behind_cgnat_joins_cloud_mesh_with_one_token() {
    mesh_testkit::init_tracing();
    // The cloud: a founder that relays, plus one more public member.
    let cloud = spawn_founder_with("cloud", "hyacinth", vec![], |c| {
        c.wan.relay = true;
    })
    .await;
    let worker = join_new_node("worker", &cloud, vec![]).await;
    let key = founder_user_key(&cloud);

    // The laptop: symmetric NAT (CGNAT-like), one token, done.
    let cgnat = TestNat::new(NatProfile::Symmetric);
    let laptop = join_new_node_natted("laptop", &cloud, &cgnat).await;
    let laptop_id = laptop.node.node_id();
    let cloud_id = cloud.node.node_id();

    // Converges: everyone sees everyone alive.
    wait_alive(&cloud, &laptop).await;
    wait_alive(&worker, &laptop).await;
    wait_alive(&laptop, &cloud).await;
    wait_alive(&laptop, &worker).await;

    // The founder verified itself public and relays; the laptop registered
    // with it and announces the relay candidate.
    let engine_cloud = cloud.node.engine().clone();
    let laptop_wan = laptop.node.wan().clone();
    let ok = wait_until(Duration::from_secs(40), || {
        engine_cloud.read(|s| {
            s.members
                .get(&cloud_id)
                .is_some_and(|m| m.info.tags.iter().any(|t| t == RELAY_TAG))
        }) && laptop_wan.relays().contains(&cloud_id)
    })
    .await;
    assert!(
        ok,
        "laptop registers with the cloud relay (wan {:?})",
        laptop_wan.reachability()
    );

    // The founder runs a job on the laptop with streamed logs. It reaches
    // the laptop over the laptop's own connection to it (inbound pooling):
    // no relay traffic needed between a public node and a NATed one.
    match run_to_end(
        &cloud,
        key,
        run_req("echo hello-from-laptop; hostname >/dev/null", "name=laptop"),
    )
    .await
    {
        RunOutcome::Done {
            stdout,
            state,
            code,
            ..
        } => {
            assert_eq!(state, JobState::Succeeded);
            assert_eq!(code, Some(0));
            assert!(stdout.contains("hello-from-laptop"), "stdout: {stdout}");
        }
        other => panic!("cloud → laptop run failed: {other:?}"),
    }
    assert_eq!(
        cloud.node.transport().conn_kind(&laptop_id),
        Some(ConnKind::Direct)
    );

    // Streaming: follow a multi-line job as it runs.
    let mut rx = stream_as(
        &cloud,
        key,
        run_req(
            "for i in 1 2 3; do echo line-$i; sleep 0.2; done",
            "name=laptop",
        ),
    )
    .await;
    let mut lines = 0;
    let mut done = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::LogChunk { stream: 1, data } => {
                lines += String::from_utf8_lossy(&data).matches("line-").count();
            }
            IpcResponse::JobDone { state, .. } => {
                done = Some(state);
                break;
            }
            IpcResponse::Error { message } => panic!("stream error: {message}"),
            _ => {}
        }
    }
    assert_eq!(done, Some(JobState::Succeeded));
    assert_eq!(lines, 3, "all streamed lines arrive");

    // Another public member reaches the laptop too: either the laptop's
    // gossip already dialed it, or the punch makes the laptop dial back
    // (the worker is public, so the dial-back lands) — never a relay.
    match run_to_end(&worker, key, run_req("echo from-worker", "name=laptop")).await {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("from-worker"));
        }
        other => panic!("worker → laptop run failed: {other:?}"),
    }
    assert_eq!(
        worker.node.transport().conn_kind(&laptop_id),
        Some(ConnKind::Direct)
    );

    // And the laptop dispatches to the cloud like any member.
    match run_to_end(
        &laptop,
        key,
        run_req("echo from-laptop-to-cloud", "name=cloud"),
    )
    .await
    {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("from-laptop-to-cloud"));
        }
        other => panic!("laptop → cloud run failed: {other:?}"),
    }

    // The laptop's status shows what it learned.
    let resp = ipc_as(&laptop, key, IpcRequest::Status).await;
    let IpcResponse::Status(s) = resp else {
        panic!("status")
    };
    let wan = s.wan.expect("wan summary");
    assert!(wan.relays.iter().any(|r| *r == cloud_id.short()));
    assert_ne!(wan.reachability, "public");

    cloud.node.shutdown();
    worker.node.shutdown();
    laptop.node.shutdown();
}
