//! Clearing job history: the Jobs screen's "Clear history" button.
//!
//! The claim is narrow and the two ways it can quietly be wrong are both
//! here:
//!
//! - **it stays cleared across a restart.** The op-log replays per author,
//!   not in global HLC order, so a clear that only emptied the map would be
//!   undone by the first `JobUpsert` replayed after it on the next boot.
//!   The watermark in the state is what makes forgetting stick.
//! - **it does not eat work in flight.** A job still running when the
//!   button was pressed finishes *after* the watermark and has to survive —
//!   which is why the rule reads `finished_at_micros` and not
//!   `created_at_micros`, the field that cannot tell the two apart.
//!
//! A third way, which needs a second author to show and so lives in
//! `state.rs`'s unit tests rather than here: the watermark has to be
//! applied to the *job*, not to each `JobUpsert` in isolation. See
//! `a_clear_forgets_a_job_whose_reports_replay_after_it`.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobId, JobPriority, JobRow, JobState};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, ipc_as, run_to_end, spawn_founder, stream_as,
};

fn submission(command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.to_string(),
        on: None,
        env: vec![],
        timeout_secs: Some(30),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

async fn run(node: &TestNode, command: &str) {
    match run_to_end(node, founder_user_key(node), submission(command)).await {
        RunOutcome::Done { state, .. } => assert!(state.is_terminal(), "{state:?}"),
        other => panic!("run did not complete: {other:?}"),
    }
}

async fn jobs(node: &TestNode) -> Vec<JobRow> {
    match ipc_as(node, founder_user_key(node), IpcRequest::Jobs).await {
        IpcResponse::Jobs(rows) => rows,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Poll until `f` sees what it is waiting for, or give up. Returned rather
/// than asserted so each caller can say what the absence meant.
async fn until(node: &TestNode, f: impl Fn(&[JobRow]) -> bool) -> Vec<JobRow> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let rows = jobs(node).await;
        if f(&rows) || std::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn clear(node: &TestNode) -> String {
    let before_micros = mesh_proto::now_micros();
    let user = founder_user_key(node);
    let by = user.user_id();
    let signature = user
        .sign(&mesh_proto::jobs_clear_signing_bytes(before_micros, &by))
        .to_vec();
    match ipc_as(
        node,
        user,
        IpcRequest::JobsClear {
            before_micros,
            by,
            signature,
        },
    )
    .await
    {
        IpcResponse::Notice { message } => message,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Cleared jobs are gone, and *stay* gone across the replay a restart does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clear_survives_replay() {
    let a = spawn_founder("a", "clear-replay").await;

    run(&a, "echo one").await;
    run(&a, "echo two").await;
    assert_eq!(jobs(&a).await.len(), 2);

    let jobs_dir = a.config().data_dir.join("jobs");
    assert!(std::fs::read_dir(&jobs_dir).unwrap().count() >= 2);

    let message = clear(&a).await;
    assert!(
        message.contains('2'),
        "count belongs in the notice: {message}"
    );
    assert!(jobs(&a).await.is_empty(), "the map should be empty");
    // The records on disk went with it.
    assert_eq!(
        std::fs::read_dir(&jobs_dir).map(|d| d.count()).unwrap_or(0),
        0,
        "job records outlived the clear"
    );

    // The whole point. Every `JobUpsert` for both jobs is still in the log
    // and is replayed after the clear, because the watermark's op and
    // theirs are in different sequences and nothing orders them.
    let a = a.restart().await;
    assert!(
        jobs(&a).await.is_empty(),
        "a cleared job came back on replay — the watermark is not being consulted"
    );

    a.node.shutdown();
}

/// A job still running when the button is pressed is not swallowed by it,
/// and is still listed once it finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clear_spares_work_in_flight() {
    let a = spawn_founder("a", "clear-inflight").await;

    run(&a, "echo done-already").await;

    // Launched and left running: `run_to_end` would wait for it.
    let mut stream = stream_as(&a, founder_user_key(&a), submission("sleep 4")).await;
    let rows = until(&a, |rows| rows.iter().any(|j| j.state == JobState::Running)).await;
    let running: JobId = rows
        .iter()
        .find(|j| j.state == JobState::Running)
        .expect("the sleep should be running")
        .job_id;

    let message = clear(&a).await;
    assert!(
        message.contains('1'),
        "only the finished job goes: {message}"
    );

    let rows = jobs(&a).await;
    assert_eq!(rows.len(), 1, "the running job was cleared: {rows:?}");
    assert_eq!(rows[0].job_id, running);

    // Drain the stream so the job reaches its terminal state. The
    // `JobUpsert` carrying `Succeeded` is authored *after* the watermark,
    // so the filter must let it through.
    while stream.recv().await.is_some() {}
    let rows = until(&a, |rows| rows.iter().all(|j| j.state.is_terminal())).await;
    assert_eq!(rows.len(), 1, "the job vanished when it finished: {rows:?}");
    assert_eq!(rows[0].job_id, running);
    assert!(rows[0].state.is_terminal(), "{:?}", rows[0].state);

    a.node.shutdown();
}
