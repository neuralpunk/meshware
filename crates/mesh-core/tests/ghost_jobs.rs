//! Ghost jobs: rows that come back at `running` after the operator has
//! already got rid of them, every time the Director restarts.
//!
//! Two independent holes made the same ghost, and a mesh could have either
//! one. Both are end-to-end here because both were invisible to the unit
//! tests that already covered the pieces:
//!
//! - **the executor forgets, the mesh does not.** Only the executor may
//!   author a job's summary — `JobUpsert` is dropped on apply from any
//!   other author. So a node that loses a job's local record while the
//!   summary still says `running` strands a row that literally nobody is
//!   allowed to close. `jobs clear` will not take it either: the watermark
//!   forgets jobs that *finished*, and this one never did.
//! - **the cure did not survive the cleanup.** Force-cancel is what closes
//!   such a row by hand, and the clear that followed threw the override
//!   away — so the next boot replayed the executor's `running` report onto
//!   an empty override map and the row came back.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{Hlc, IpcRequest, IpcResponse, JobId, JobRow, JobState, JobSummary, OpBody};
use mesh_testkit::{founder_user_key, ipc_as, spawn_founder};

async fn jobs(node: &mesh_testkit::TestNode) -> Vec<JobRow> {
    match ipc_as(node, founder_user_key(node), IpcRequest::Jobs).await {
        IpcResponse::Jobs(rows) => rows,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Put the node in the state the two real executors were found in: the
/// mesh credits it with a job it is running, and it has no record of that
/// job at all. Authored through the engine because that is the only way a
/// summary can exist — the executor signs its own.
fn strand_a_running_job(node: &mesh_testkit::TestNode) -> (JobId, Hlc) {
    let engine = node.node.engine();
    let job_id = JobId::generate();
    let now = mesh_proto::now_micros();
    let stranded_at = engine.clock().now();
    engine
        .author_op(OpBody::JobUpsert {
            summary: JobSummary {
                job_id,
                cmd_preview: "complex-math (bash/complex-math.sh)".into(),
                runtime: "bash".into(),
                submitter: engine.node_id(),
                submitter_user: founder_user_key(node).user_id(),
                executor: engine.node_id(),
                state: JobState::Running,
                exit_code: None,
                created_at_micros: now,
                started_at_micros: Some(now),
                finished_at_micros: None,
                fanout: None,
                updated_hlc: stranded_at,
            },
        })
        .expect("author the stranded summary");
    assert!(
        !node.node.jobs_for_test().has_record(&job_id),
        "the fixture is only a fixture if the record really is gone"
    );
    (job_id, stranded_at)
}

/// A one-second script listed as running a day later, on a node that has
/// nothing left to run: the boot that finds the row closes it.
///
/// Without this the row is permanent. Retention and `jobs clear` both skip
/// non-terminal jobs, the orphan sweep only sees jobs whose record is
/// still on disk, and no other node's `JobUpsert` for it would even apply.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_closes_a_job_the_executor_no_longer_has() {
    let a = spawn_founder("a", "ghost-strand").await;
    let (job_id, stranded_at) = strand_a_running_job(&a);

    let rows = jobs(&a).await;
    let ghost = rows.iter().find(|r| r.job_id == job_id).expect("ghost");
    assert_eq!(ghost.state, JobState::Running, "fixture did not take");

    let a = a.restart().await;

    let rows = jobs(&a).await;
    let row = rows
        .iter()
        .find(|r| r.job_id == job_id)
        .expect("the row should still exist, just finished");
    assert!(
        row.state.is_terminal(),
        "the ghost came back running after a restart: {:?}",
        row.state
    );

    // **And it closed the row without outranking anyone.** Losing the
    // local record is also what a handoff looks like from here: `drain
    // --move` and preemption delete the journal precisely so the job can
    // run elsewhere, and the summary they leave behind is this node's,
    // non-terminal, with no record — indistinguishable from a ghost at
    // boot. A close stamped `now` would carry a fresh HLC and beat the new
    // executor's real report on merge, turning a job that succeeded
    // somewhere else into a mesh-wide `failed`. One tick past the summary
    // it closes, it is the last word only where there is no other word.
    let closed_at = a
        .node
        .engine()
        .read(|s| s.jobs.get(&job_id).map(|j| j.updated_hlc))
        .expect("summary");
    assert!(
        closed_at > stranded_at,
        "the close has to win against the summary it closes"
    );
    assert_eq!(
        closed_at,
        Hlc {
            wall_micros: stranded_at.wall_micros,
            logical: stranded_at.logical + 1,
        },
        "the close took a boot-time HLC and would outrank the node the job was handed to"
    );
    a.stop().await;
}

/// And once it is terminal it is ordinary history, so the button that is
/// supposed to forget it does.
#[tokio::test(flavor = "multi_thread")]
async fn the_closed_ghost_clears_and_stays_cleared() {
    let a = spawn_founder("a", "ghost-clear").await;
    let (job_id, _) = strand_a_running_job(&a);
    let a = a.restart().await;

    // The clear is stamped after the restart closed the job, so the
    // watermark is past its finish.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let user = founder_user_key(&a).clone();
    let before_micros = mesh_proto::now_micros();
    let by = user.user_id();
    let signature = user
        .sign(&mesh_proto::jobs_clear_signing_bytes(before_micros, &by))
        .to_vec();
    match ipc_as(
        &a,
        &user,
        IpcRequest::JobsClear {
            before_micros,
            by,
            signature,
        },
    )
    .await
    {
        IpcResponse::Notice { .. } => {}
        other => panic!("clear failed: {other:?}"),
    }
    assert!(
        !jobs(&a).await.iter().any(|r| r.job_id == job_id),
        "the clear did not forget the closed ghost"
    );

    // The restart is the whole point: this is where it used to come back.
    let a = a.restart().await;
    assert!(
        !jobs(&a).await.iter().any(|r| r.job_id == job_id),
        "the cleared ghost came back on the next start"
    );
    a.stop().await;
}

/// The operator's own route, in the order the Director offers it: Cancel
/// (a force, because the executor has nothing to cancel) and then Clear
/// history. It has to survive the restart that follows.
///
/// The restart here is one node's, so its own ops replay in sequence and
/// the pair cannot land out of order — which is exactly why the assertion
/// at the end is about the *override* and not only about the row. On a
/// real mesh the executor is somebody else, its log replays wherever its
/// node id sorts, and the surviving override is the only thing that
/// settles its `running` report when it lands after the clear. Drop the
/// override and the ghost is back on whichever nodes sort the wrong way;
/// the replay orders themselves are pinned in `state.rs`'s
/// `a_forced_cancel_survives_the_clear_that_forgets_its_job`.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_then_clear_survives_a_restart() {
    let a = spawn_founder("a", "ghost-cancel").await;
    let (job_id, _) = strand_a_running_job(&a);
    let user = founder_user_key(&a).clone();

    let by = user.user_id();
    let signature = user
        .sign(&mesh_proto::job_cancel_force_signing_bytes(&job_id, &by))
        .to_vec();
    match ipc_as(
        &a,
        &user,
        IpcRequest::JobCancelForce {
            job_id,
            by,
            signature,
        },
    )
    .await
    {
        IpcResponse::Notice { .. } | IpcResponse::Ok => {}
        other => panic!("cancel failed: {other:?}"),
    }
    assert!(
        jobs(&a)
            .await
            .iter()
            .find(|r| r.job_id == job_id)
            .is_some_and(|r| r.state.is_terminal()),
        "cancel left the job running"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let before_micros = mesh_proto::now_micros();
    let by = user.user_id();
    let signature = user
        .sign(&mesh_proto::jobs_clear_signing_bytes(before_micros, &by))
        .to_vec();
    match ipc_as(
        &a,
        &user,
        IpcRequest::JobsClear {
            before_micros,
            by,
            signature,
        },
    )
    .await
    {
        IpcResponse::Notice { .. } => {}
        other => panic!("clear failed: {other:?}"),
    }
    assert!(
        !jobs(&a).await.iter().any(|r| r.job_id == job_id),
        "clear did not forget the cancelled job"
    );

    let a = a.restart().await;
    assert!(
        !jobs(&a).await.iter().any(|r| r.job_id == job_id),
        "the job came back after Cancel, Clear history, and a restart — \
         which is the whole complaint"
    );
    assert!(
        a.node
            .engine()
            .read(|s| s.jobs_force_cancelled.contains_key(&job_id)),
        "the clear threw away the force that finished the job, so the executor's \
         `running` report has nothing left to settle it when it replays"
    );
    a.stop().await;
}
