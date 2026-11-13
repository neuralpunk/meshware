//! ENG-15 — `systemctl restart` keeps the work a node has already taken on.
//!
//! A package upgrade ends in a restart, so the restart path is an upgrade
//! path. It used to lose both halves of a node's work at once: running jobs
//! were signalled by systemd directly, and *queued* jobs — work no process
//! had touched, whose spec was still intact on disk — were marked `failed`
//! by the orphan sweep on the way back up, because the sweep could not tell
//! "was running and cannot be resumed" from "never started".
//!
//! The queue is held shut here with the memory gate rather than with a long
//! job, so that the restart is a restart and not a race against something
//! still running: the gate is the one knob that stops promotion without
//! anything being in flight for it to stop.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    IpcRequest, IpcResponse, JobId, JobPayload, JobPriority, JobSpec, JobState, SignedIpc,
};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, spawn_founder_with};

/// A memory floor no machine can meet, so nothing is ever promoted.
const GATE_SHUT_MB: u64 = u64::MAX / 2;

fn envelope(user: &Keypair, command: &str, nonce: u64) -> SignedIpc {
    let req = IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(120),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let payload = postcard::to_stdvec(&req).unwrap();
    let at_micros = mesh_proto::now_micros();
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

/// A job dispatched to this node, as the executor's own accept path sees
/// it — no IPC stream, so nothing holds the store open across the restart.
fn accept(node: &TestNode, command: &str, nonce: u64) -> JobId {
    let user = founder_user_key(node);
    let job_id = JobId::generate();
    let spec = JobSpec {
        job_id,
        payload: JobPayload::Command(command.into()),
        env: vec![],
        timeout_secs: Some(120),
        priority: JobPriority::Normal,
        submitter: node.node.node_id(),
        submitter_user: user.user_id(),
        auth: envelope(user, command, nonce),
        fanout: None,
        created_at_micros: mesh_proto::now_micros(),
    };
    node.node.jobs().accept(spec).expect("accept the job");
    job_id
}

async fn states(node: &TestNode) -> Vec<JobState> {
    match ipc_as(node, founder_user_key(node), IpcRequest::Jobs).await {
        IpcResponse::Jobs(rows) => rows.into_iter().map(|r| r.state).collect(),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_work_survives_a_restart_and_then_runs() {
    let node = spawn_founder_with("NIM", "eng15-requeue", vec![], |c| {
        c.limits.slots = 2;
        c.limits.queue = 16;
        c.limits.min_free_mem_mb = GATE_SHUT_MB;
    })
    .await;

    for i in 0..3 {
        accept(&node, &format!("echo queued-{i}"), 100 + i);
    }
    assert_eq!(node.node.jobs().queued_count(), 3, "the gate should hold");
    assert_eq!(node.node.jobs().running_count(), 0);

    // The restart the operator actually types.
    let node = node.restart().await;

    // Back on the queue, not marked failed. The gate is still shut, so
    // this is the requeue and nothing else.
    assert_eq!(
        node.node.jobs().queued_count(),
        3,
        "queued work was lost by the restart"
    );
    let after = states(&node).await;
    assert!(
        after.iter().all(|s| *s == JobState::Queued),
        "a restart must not fail work that had not started: {after:?}"
    );

    // Open the gate the way a reload does, and the restored queue runs.
    let mut config = node.config().clone();
    config.limits.min_free_mem_mb = 0;
    node.node.apply_config(&config);

    let drained = mesh_testkit::wait_until(Duration::from_secs(60), || {
        let jobs = node.node.jobs();
        jobs.queued_count() == 0 && jobs.running_count() == 0
    })
    .await;
    let states = states(&node).await;
    assert!(drained, "the restored queue never ran: {states:?}");
    assert_eq!(
        states.iter().filter(|s| **s == JobState::Succeeded).count(),
        3,
        "every restored job should have run: {states:?}"
    );
}

/// The other half of the sweep still holds: a job that was *running* cannot
/// be resumed — its process went with the old daemon — and is failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_that_was_running_is_still_failed_by_a_restart() {
    let node = spawn_founder_with("RUN", "eng15-running", vec![], |c| {
        c.limits.slots = 2;
    })
    .await;

    // Bounded, not endless: until ENG-16 persists process groups, a job's
    // child outlives the daemon that started it, and a test must not leave
    // one on the machine.
    let id = accept(&node, "sleep 45", 7);
    let started = mesh_testkit::wait_until(Duration::from_secs(20), || {
        node.node.jobs().running_count() == 1
    })
    .await;
    assert!(started, "the job never started");

    // Kill the supervising task the way a stop does, then sweep on the way
    // back up. (The child outlives an in-process restart, which is why the
    // shipped unit leaves the SIGKILL to systemd.)
    node.node.jobs().forget(&id);
    let node = node.restart().await;

    let after = states(&node).await;
    assert_eq!(
        after.iter().filter(|s| **s == JobState::Failed).count(),
        1,
        "a job that was running should be failed by the restart: {after:?}"
    );
    assert_eq!(node.node.jobs().queued_count(), 0, "nothing to requeue");
}

/// The grace is what makes a stop worth waiting for; a node with nothing
/// running must not spend it. A stop that took 30 s on an idle node would
/// be a regression an operator notices immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_node_does_not_wait_out_its_shutdown_grace() {
    let node = spawn_founder_with("IDLE", "eng15-idle", vec![], |c| {
        c.shutdown.grace = Duration::from_secs(300);
    })
    .await;

    let began = std::time::Instant::now();
    let left = node.node.shutdown_grace().await;
    assert_eq!(left, 0, "nothing was running");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "an idle stop waited {:?}",
        began.elapsed()
    );
    node.stop().await;
}

/// The grace ends at its deadline and says what it left behind, rather than
/// hanging on a job that outlasts any deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_grace_is_bounded_and_reports_what_it_left_running() {
    let node = spawn_founder_with("SLOW", "eng15-bound", vec![], |c| {
        c.limits.slots = 2;
        c.shutdown.grace = Duration::from_secs(2);
    })
    .await;

    accept(&node, "sleep 45", 11);
    let started = mesh_testkit::wait_until(Duration::from_secs(20), || {
        node.node.jobs().running_count() == 1
    })
    .await;
    assert!(started, "the job never started");

    let began = std::time::Instant::now();
    let left = node.node.shutdown_grace().await;
    let waited = began.elapsed();
    assert_eq!(left, 1, "the long job should still be running");
    assert!(
        waited >= Duration::from_secs(2) && waited < Duration::from_secs(20),
        "the grace should end at its deadline, waited {waited:?}"
    );
    // Not admitting new work is the first thing the grace does.
    assert!(node.node.is_draining(), "the grace stops admitting work");

    node.stop().await;
}

/// The daemon's grace only means something if systemd is not killing every
/// job process itself. Asserted on the shipped unit, because that is the
/// file that has to carry it.
#[test]
fn the_shipped_unit_signals_the_daemon_alone() {
    let unit = include_str!("../../../packaging/systemd/meshnode.service");
    let directives: Vec<&str> = unit
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .collect();
    assert!(
        directives.contains(&"KillMode=mixed"),
        "without KillMode=mixed, systemd SIGTERMs every job process directly \
         and there is nothing left for the grace to wait for"
    );
    let stop_timeout = directives
        .iter()
        .find_map(|l| l.strip_prefix("TimeoutStopSec="))
        .and_then(|v| v.parse::<u64>().ok())
        .expect("the unit must bound its own stop");
    let grace = mesh_core::config::ShutdownConfig::default().grace.as_secs();
    assert!(
        stop_timeout > grace,
        "TimeoutStopSec ({stop_timeout}s) must sit above the default grace \
         ({grace}s), or systemd's SIGKILL lands first and the grace is only a delay"
    );
}
