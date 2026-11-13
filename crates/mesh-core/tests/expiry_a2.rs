//! M31 A2 — honest expiry.
//!
//! One rule, and it is §4 decision 9: **losing the record is unacceptable;
//! losing the payload is routine.** A run from 2024 whose output aged out
//! reads *"output expired"*. It does not read *"run not found"*, it does not
//! read *"failed"*, and it does not disappear.
//!
//! The bug this phase actually fixed is worse than the one it set out to
//! fix. `follow()` used to fabricate `JobState::Failed` with no exit code
//! when it could not read `status.json` — so the moment `[retention]`
//! removed a job's directory, **a job that had succeeded reported as
//! failed**. A wrong answer, which is worse than a missing one, and the
//! single most trust-damaging thing the product did.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{
    ExpiryReason, IpcRequest, IpcResponse, JobPriority, JobRow, JobState, NodeId,
    OutputAvailability,
};
use mesh_testkit::{RunOutcome, TestNode, founder_user_key, ipc_as, run_to_end, spawn_founder};

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

async fn until_terminal(node: &TestNode) -> Vec<JobRow> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let rows = jobs(node).await;
        if rows.iter().all(|r| r.state.is_terminal()) && !rows.is_empty() {
            return rows;
        }
        if std::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Delete a job's on-disk record the way `[retention]` does, without waiting
/// thirty days for it.
fn prune_on_disk(node: &TestNode, job: &mesh_proto::JobId) {
    let dir = node.node.jobs_for_test().job_dir_of(job);
    assert!(dir.exists(), "the fixture never had a record to prune");
    std::fs::remove_dir_all(&dir).expect("prune");
}

/// Collect a whole `JobLogs` stream.
async fn logs(node: &TestNode, job: mesh_proto::JobId) -> Vec<IpcResponse> {
    let mut rx = mesh_testkit::stream_as(
        node,
        founder_user_key(node),
        IpcRequest::JobLogs {
            job_id: job,
            follow: false,
        },
    )
    .await;
    let mut frames = Vec::new();
    while let Some(f) = rx.recv().await {
        let done = matches!(
            f,
            IpcResponse::JobDone { .. } | IpcResponse::Error { .. } | IpcResponse::Denied { .. }
        );
        frames.push(f);
        if done {
            break;
        }
    }
    frames
}

// ---------------------------------------------------------------------------
// The bug that mattered
// ---------------------------------------------------------------------------

/// **A successful job whose output was pruned is still a successful job.**
///
/// Before A2 this returned `failed` with no exit code, because a missing
/// `status.json` fell through to a fabricated `JobState::Failed`. An
/// operator told that a green deploy had failed does not go on to believe
/// anything else the product says.
#[tokio::test]
async fn a_pruned_job_still_reports_the_state_it_actually_finished_in() {
    let node = spawn_founder("n0", "a2-mesh").await;
    run(&node, "echo hello").await;
    let rows = until_terminal(&node).await;
    let job = rows[0].job_id;
    assert_eq!(rows[0].state, JobState::Succeeded);

    prune_on_disk(&node, &job);

    let frames = logs(&node, job).await;
    // No error, no fabricated failure.
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, IpcResponse::Error { .. })),
        "a pruned job produced an error: {frames:?}"
    );
    let done = frames
        .iter()
        .find_map(|f| match f {
            IpcResponse::JobDone { state, exit_code } => Some((*state, *exit_code)),
            _ => None,
        })
        .expect("the stream must still terminate with the job's real outcome");
    assert_eq!(
        done.0,
        JobState::Succeeded,
        "a successful job reported as {:?} once its log was pruned",
        done.0
    );
    assert_eq!(
        done.1,
        Some(0),
        "the exit code was thrown away with the log"
    );
}

/// And the stream says *why* there are no bytes, before it says the job is
/// done — so a reader is never left to conclude the job produced nothing.
#[tokio::test]
async fn a_pruned_job_says_its_output_expired_rather_than_going_silent() {
    let node = spawn_founder("n0", "a2-mesh").await;
    run(&node, "echo hello").await;
    let job = until_terminal(&node).await[0].job_id;
    prune_on_disk(&node, &job);

    let frames = logs(&node, job).await;
    let availability = frames
        .iter()
        .find_map(|f| match f {
            IpcResponse::LogsUnavailable { availability } => Some(availability.clone()),
            _ => None,
        })
        .expect("the stream must say why there is nothing to read");

    match availability {
        OutputAvailability::Expired { reason, .. } => {
            assert_eq!(reason, ExpiryReason::Retention);
        }
        other => panic!("expected expired, got {other:?}"),
    }
    // The one sentence, and the promise inside it.
    let msg = availability.message();
    assert!(msg.contains("output expired"), "{msg}");
    assert!(msg.contains("record below is complete"), "{msg}");
    assert!(
        !availability.may_return(),
        "expired must not read as retryable"
    );

    // And it comes *before* the terminal frame, so a renderer can print it
    // where the output would have been.
    let unavailable_at = frames
        .iter()
        .position(|f| matches!(f, IpcResponse::LogsUnavailable { .. }))
        .unwrap();
    let done_at = frames
        .iter()
        .position(|f| matches!(f, IpcResponse::JobDone { .. }))
        .unwrap();
    assert!(unavailable_at < done_at);
}

// ---------------------------------------------------------------------------
// Per reader — because this is exactly the rule that holds in three places
// and fails in the fourth
// ---------------------------------------------------------------------------

/// `meshnode job show` carries it on the record itself, so the answer is
/// there without running `job logs` and getting nothing.
#[tokio::test]
async fn the_job_record_itself_says_where_its_output_is() {
    let node = spawn_founder("n0", "a2-mesh").await;
    run(&node, "echo hello").await;
    let job = until_terminal(&node).await[0].job_id;

    // Available while the bytes are here.
    let row = jobs(&node)
        .await
        .into_iter()
        .find(|r| r.job_id == job)
        .unwrap();
    assert!(row.output.is_available(), "{:?}", row.output);
    assert_eq!(row.output.message(), "", "an available job needs no excuse");

    prune_on_disk(&node, &job);

    let row = jobs(&node)
        .await
        .into_iter()
        .find(|r| r.job_id == job)
        .expect("**the record survives the payload** — decision 9");
    assert_eq!(row.output.as_str(), "expired");
    assert_eq!(
        row.state,
        JobState::Succeeded,
        "the record must keep the outcome it had"
    );
    assert_eq!(row.exit_code, Some(0));
}

/// A job that has not finished has its output *ahead* of it, which is not
/// the same as having lost it. Calling that "expired" would be the first
/// lie, and every later one would be believed less.
#[tokio::test]
async fn a_job_that_has_not_finished_is_never_expired() {
    let node = spawn_founder("n0", "a2-mesh").await;
    let user = founder_user_key(&node).clone();
    let mut rx = mesh_testkit::stream_as(&node, &user, submission("sleep 5")).await;
    // Wait for it to start, then look at it mid-flight.
    let mut started = None;
    while let Some(f) = rx.recv().await {
        if let IpcResponse::JobStarted { job_id, .. } = f {
            started = Some(job_id);
            break;
        }
    }
    let job = started.expect("job started");
    let row = jobs(&node)
        .await
        .into_iter()
        .find(|r| r.job_id == job)
        .unwrap();
    assert!(
        row.output.is_available(),
        "a running job's output read as {:?}",
        row.output
    );
}

// ---------------------------------------------------------------------------
// Expired versus unavailable — the distinction that keeps "expired" credible
// ---------------------------------------------------------------------------

/// Output on a node that is merely offline is **unavailable**, not expired,
/// and the value says so in the API rather than only in the prose.
#[test]
fn offline_and_expired_are_distinguishable_in_the_api() {
    let offline = OutputAvailability::Unavailable {
        node: NodeId([7u8; 32]),
        node_name: "worker-7".into(),
    };
    let expired = OutputAvailability::Expired {
        when_micros: 1_780_790_400_000_000,
        reason: ExpiryReason::Retention,
    };

    // Different variants, different tags, different answers to the one
    // question a caller actually has.
    assert_ne!(offline, expired);
    assert_ne!(offline.as_str(), expired.as_str());
    assert!(offline.may_return());
    assert!(!expired.may_return());

    // A departed node is expired rather than unavailable: waiting will
    // never help, and sending an operator to look for a machine that no
    // longer exists is its own kind of dishonesty.
    let gone = OutputAvailability::Expired {
        when_micros: 0,
        reason: ExpiryReason::ProducerGone,
    };
    assert!(!gone.may_return());
    assert!(!gone.message().contains("offline"));

    // Round-trips over IPC as a value, not as a rendered string.
    let encoded = postcard::to_stdvec(&offline).unwrap();
    let back: OutputAvailability = postcard::from_bytes(&encoded).unwrap();
    assert_eq!(back, offline);
}

// ---------------------------------------------------------------------------
// Absence has a reason (item 4)
// ---------------------------------------------------------------------------

/// A query window reaching past what the mesh still keeps returns a **known
/// absence**, and one that simply matches nothing does not. They are
/// different values, asserted as such.
#[tokio::test]
async fn a_window_past_the_retention_horizon_is_a_known_absence_not_an_empty_result() {
    let node = spawn_founder("n0", "a2-mesh").await;
    run(&node, "echo hello").await;
    until_terminal(&node).await;

    // Let the index catch up — it is written on a timer, like everything
    // else that is a redb commit.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let page = history_jobs(&node, None).await;
        if !page.rows.is_empty() || std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A window that starts before anything this mesh has: known absence.
    let reached_past = history_jobs(&node, Some(1)).await;
    assert!(
        reached_past.absence.window_reached_past,
        "a window starting at the epoch did not register as reaching past"
    );
    let note = reached_past
        .absence
        .note()
        .expect("a reached-past window has a note");
    assert!(note.contains("known absence"), "{note}");
    assert!(note.contains("retention"), "{note}");

    // A window inside what we hold: no note, however few rows match.
    let inside = history_jobs(&node, Some(mesh_proto::now_micros())).await;
    assert!(!inside.absence.window_reached_past);
    assert_eq!(inside.absence.note(), None);

    // And the two are different values — the assertion the phase is for.
    assert_ne!(reached_past.absence, inside.absence);
}

async fn history_jobs(node: &TestNode, since: Option<u64>) -> mesh_proto::HistoryJobPage {
    let q = mesh_proto::HistoryQuery {
        since_micros: since,
        limit: 50,
        ..Default::default()
    };
    match ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::HistoryJobs(Box::new(q)),
    )
    .await
    {
        IpcResponse::HistoryJobs(page) => *page,
        other => panic!("unexpected: {other:?}"),
    }
}

/// A job this mesh has never heard of is still an error — *"not found"* has
/// to keep meaning something, or the honest states are noise.
#[tokio::test]
async fn a_job_that_never_existed_is_still_not_found() {
    let node = spawn_founder("n0", "a2-mesh").await;
    let frames = logs(&node, mesh_proto::JobId([9u8; 16])).await;
    let message = frames
        .iter()
        .find_map(|f| match f {
            IpcResponse::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("an unknown job is an error");
    // And even here it points at the thing that most often explains it.
    assert!(message.contains("retention"), "{message}");
}
