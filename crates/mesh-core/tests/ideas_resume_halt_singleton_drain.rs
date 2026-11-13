//! The four daemon-side behaviours added on 2026-09-05: a parallel run's
//! record (`--resume`), the `--halt-on-fail` breaker, `--singleton`, and
//! a drain with an end (`drain --until`).

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobId, JobPriority, JobState};
use mesh_testkit::{
    RunOutcome, TestNode, capacity, founder_user_key, ipc_as, run_to_end, spawn_founder_sized,
    stream_as, wait_until,
};

fn par_req(command: &str, items: Vec<String>, env: Vec<(String, String)>) -> IpcRequest {
    IpcRequest::Parallel {
        command: command.into(),
        service: None,
        inputs: vec![],
        item_input: None,
        items,
        on: None,
        env,
        timeout_secs: Some(60),
        chunk: Some(1),
        capture: mesh_proto::Capture::Inline,
        reduce: None,
        reduce_on: None,
    }
}

fn run_req(command: &str, env: Vec<(String, String)>) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env,
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

#[derive(Debug, Default)]
struct Done {
    job: Option<JobId>,
    succeeded: u64,
    failed: u64,
    total: u64,
    halted: Option<String>,
    error: Option<String>,
}

async fn parallel_to_end(node: &TestNode, req: IpcRequest) -> Done {
    let mut rx = stream_as(node, founder_user_key(node), req).await;
    let mut done = Done::default();
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::ParallelStarted { job_id, .. } => done.job = Some(job_id),
            IpcResponse::ParallelDone {
                total,
                succeeded,
                failed,
                halted,
                ..
            } => {
                done.total = total;
                done.succeeded = succeeded;
                done.failed = failed;
                done.halted = halted;
                return done;
            }
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                done.error = Some(message);
                return done;
            }
            _ => {}
        }
    }
    done
}

async fn record(node: &TestNode, job: JobId) -> mesh_proto::parallel::ParallelRecord {
    match ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::ParallelRecord { job_id: job },
    )
    .await
    {
        IpcResponse::ParallelRecord(r) => *r,
        other => panic!("no record: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 01 — the record, and what a resume would re-run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_run_records_every_item_and_names_the_unfinished_ones() {
    let a = spawn_founder_sized("a", "ideas-record", capacity(4, 16, 200)).await;
    let items: Vec<String> = (1..=8).map(|i| i.to_string()).collect();
    // Item 3 fails; everything else succeeds.
    let done = parallel_to_end(&a, par_req("test {} -ne 3", items.clone(), vec![])).await;
    assert_eq!(done.error, None);
    assert_eq!((done.succeeded, done.failed), (7, 1));
    let job = done.job.expect("job id");

    let rec = record(&a, job).await;
    assert_eq!(rec.items, items);
    assert_eq!(rec.outcomes.len(), 8, "one outcome per item");
    assert!(rec.finished_at_micros.is_some());
    assert_eq!(rec.halted, None);
    assert_eq!(rec.unfinished_items(), vec!["3".to_string()]);
    assert_eq!((rec.failed(), rec.missing()), (1, 0));
    assert_eq!(rec.capture, mesh_proto::Capture::Inline);
    // Two identical answers carry the same digest; a different one differs.
    let by_item = |i: &str| rec.outcomes.iter().find(|o| o.item == i).expect("outcome");
    assert_eq!(by_item("1").stdout_digest, by_item("2").stdout_digest);

    // A resume: the CLI resubmits with only the unfinished items and the
    // link in the environment. The new run's record carries the link.
    let resumed = parallel_to_end(
        &a,
        par_req(
            "test {} -ne 3",
            rec.unfinished_items(),
            vec![(
                mesh_proto::parallel::REQ_RESUMES.to_string(),
                job.to_display_string(),
            )],
        ),
    )
    .await;
    let rec2 = record(&a, resumed.job.expect("resumed job")).await;
    assert_eq!(rec2.resumes, Some(job));
    assert_eq!(rec2.items, vec!["3".to_string()]);
    // The record keeps the request verbatim, link included — it is what a
    // second resume would resubmit.
    assert!(
        rec2.env
            .iter()
            .any(|(k, _)| k == mesh_proto::parallel::REQ_RESUMES)
    );
    assert_eq!(rec2.unfinished_items(), vec!["3".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_nobody_owned_here_says_so() {
    let a = spawn_founder_sized("a", "ideas-norecord", capacity(2, 8, 100)).await;
    let resp = ipc_as(
        &a,
        founder_user_key(&a),
        IpcRequest::ParallelRecord {
            job_id: JobId::generate(),
        },
    )
    .await;
    let IpcResponse::Error { message } = resp else {
        panic!("expected an error, got {resp:?}");
    };
    assert!(message.contains("no parallel run"), "{message}");
}

// ---------------------------------------------------------------------------
// 02 — the breaker
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn halt_on_fail_stops_handing_out_work_and_says_why() {
    let a = spawn_founder_sized("a", "ideas-halt", capacity(2, 8, 100)).await;
    let items: Vec<String> = (1..=200).map(|i| i.to_string()).collect();
    // Every item fails, slowly enough that the breaker trips well before
    // the list is exhausted.
    let done = parallel_to_end(
        &a,
        par_req(
            "sleep 0.05; false",
            items,
            vec![(
                mesh_proto::parallel::REQ_HALT_ON_FAIL.to_string(),
                "5".into(),
            )],
        ),
    )
    .await;
    assert_eq!(done.error, None);
    let why = done.halted.clone().expect("the breaker should trip");
    assert!(why.contains("--halt-on-fail 5"), "{why}");
    assert!(done.failed >= 5, "{done:?}");
    assert!(
        done.succeeded + done.failed < 200,
        "most of the list should be untouched: {done:?}"
    );
    let rec = record(&a, done.job.expect("job")).await;
    assert_eq!(rec.halted.as_deref(), Some(why.as_str()));
    assert!(
        rec.missing() > 100,
        "untouched items are missing from the record"
    );
    assert_eq!(
        rec.unfinished_items().len(),
        200,
        "nothing succeeded, so a resume re-runs all of it"
    );
    // The job summary says failed, not succeeded.
    assert!(
        wait_until(Duration::from_secs(5), || {
            a.node
                .engine()
                .read(|s| s.jobs.get(&rec.job).map(|j| j.state) == Some(JobState::Failed))
        })
        .await
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_percentage_threshold_is_of_the_whole_list() {
    let a = spawn_founder_sized("a", "ideas-halt-pct", capacity(2, 8, 100)).await;
    let items: Vec<String> = (1..=10).map(|i| i.to_string()).collect();
    // 50% of ten is five failures; only four items fail, so no trip.
    let done = parallel_to_end(
        &a,
        par_req(
            "test {} -gt 4",
            items,
            vec![(
                mesh_proto::parallel::REQ_HALT_ON_FAIL.to_string(),
                "50%".into(),
            )],
        ),
    )
    .await;
    assert_eq!(done.halted, None, "{done:?}");
    assert_eq!((done.succeeded, done.failed), (6, 4));
}

// ---------------------------------------------------------------------------
// 06 — singleton
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_singleton_name_is_held_while_its_job_runs_and_free_after() {
    let a = spawn_founder_sized("a", "ideas-singleton", capacity(4, 8, 100)).await;
    let user = founder_user_key(&a).clone();
    let env = vec![(mesh_proto::REQ_SINGLETON.to_string(), "nightly".into())];

    let mut first = stream_as(&a, &user, run_req("sleep 2", env.clone())).await;
    assert!(
        wait_until(Duration::from_secs(10), || {
            a.node
                .engine()
                .read(|s| s.singletons.contains_key("nightly"))
        })
        .await,
        "the claim should be authored on dispatch"
    );

    // A second copy is refused while the first runs.
    match run_to_end(&a, &user, run_req("echo second", env.clone())).await {
        RunOutcome::Error(message) => {
            assert!(
                message.contains("singleton \"nightly\" is held by"),
                "{message}"
            );
        }
        other => panic!("the second copy should be refused: {other:?}"),
    }

    // Let the first finish.
    let mut state = None;
    while let Some(frame) = first.recv().await {
        if let IpcResponse::JobDone { state: s, .. } = frame {
            state = Some(s);
            break;
        }
    }
    assert_eq!(state, Some(JobState::Succeeded));
    let claimed = a
        .node
        .engine()
        .read(|s| s.singletons.get("nightly").map(|e| e.job))
        .expect("claim");
    assert!(
        wait_until(Duration::from_secs(10), || {
            a.node
                .engine()
                .read(|s| s.jobs.get(&claimed).is_some_and(|j| j.state.is_terminal()))
        })
        .await
    );

    // The name is free again the moment the job is terminal — no release.
    match run_to_end(&a, &user, run_req("echo third", env)).await {
        RunOutcome::Done { state, stdout, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("third"));
        }
        other => panic!("the name should be free: {other:?}"),
    }
    // The executor never saw the reserved key.
    let leaked = run_to_end(
        &a,
        &user,
        run_req(
            "echo singleton=${MESHWARE_TASK_SINGLETON:-unset}",
            vec![(mesh_proto::REQ_SINGLETON.to_string(), "probe".into())],
        ),
    )
    .await;
    match leaked {
        RunOutcome::Done { stdout, .. } => assert!(stdout.contains("singleton=unset"), "{stdout}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_singleton_fan_out_is_refused_with_a_reason() {
    let a = spawn_founder_sized("a", "ideas-singleton-fan", capacity(2, 8, 100)).await;
    let mut req = run_req(
        "true",
        vec![(mesh_proto::REQ_SINGLETON.to_string(), "all".into())],
    );
    if let IpcRequest::Run { fanout, .. } = &mut req {
        *fanout = Some(0);
    }
    match run_to_end(&a, founder_user_key(&a), req).await {
        RunOutcome::Error(message) => {
            assert!(message.contains("applies to a single run"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 20 — a drain with an end
// ---------------------------------------------------------------------------

async fn status(node: &TestNode) -> mesh_proto::StatusInfo {
    match ipc_as(node, founder_user_key(node), IpcRequest::Status).await {
        IpcResponse::Status(s) => s,
        other => panic!("{other:?}"),
    }
}

fn tagged_draining(node: &TestNode) -> bool {
    let me = node.node.node_id();
    node.node.engine().read(|s| {
        s.members
            .get(&me)
            .is_some_and(|m| m.info.tags.iter().any(|t| t == mesh_proto::DRAINING_TAG))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_drain_refuses_work_and_lifts_itself() {
    let a = spawn_founder_sized("a", "ideas-drain", capacity(2, 8, 100)).await;
    let user = founder_user_key(&a).clone();
    let until = mesh_proto::now_micros() + 2_500_000;
    let resp = ipc_as(
        &a,
        &user,
        IpcRequest::DrainTimed {
            until_micros: Some(until),
            reason: "kernel update".into(),
            move_pending: false,
        },
    )
    .await;
    let IpcResponse::Notice { message } = resp else {
        panic!("{resp:?}");
    };
    assert!(message.contains("draining until"), "{message}");

    let s = status(&a).await;
    assert!(s.draining);
    assert_eq!(
        s.drain
            .as_ref()
            .map(|d| (d.until_micros, d.reason.as_str())),
        Some((until, "kernel update"))
    );
    assert!(wait_until(Duration::from_secs(5), || tagged_draining(&a)).await);

    // Work submitted now is refused: the only node is draining.
    match run_to_end(&a, &user, run_req("echo no", vec![])).await {
        RunOutcome::Error(message) => assert!(message.contains("no live node"), "{message}"),
        other => panic!("a draining node took work: {other:?}"),
    }

    // …and the node rejoins on its own.
    assert!(
        wait_until(Duration::from_secs(10), || !a.node.is_draining()).await,
        "the drain should lift at its deadline"
    );
    assert!(wait_until(Duration::from_secs(5), || !tagged_draining(&a)).await);
    assert_eq!(status(&a).await.drain, None);
    match run_to_end(&a, &user, run_req("echo back", vec![])).await {
        RunOutcome::Done { state, .. } => assert_eq!(state, JobState::Succeeded),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_drain_can_be_lifted_early_and_a_past_deadline_is_refused() {
    let a = spawn_founder_sized("a", "ideas-drain-lift", capacity(2, 8, 100)).await;
    let user = founder_user_key(&a).clone();
    let past = ipc_as(
        &a,
        &user,
        IpcRequest::DrainTimed {
            until_micros: Some(mesh_proto::now_micros().saturating_sub(1)),
            reason: String::new(),
            move_pending: false,
        },
    )
    .await;
    assert!(matches!(past, IpcResponse::Error { .. }), "{past:?}");

    // Nothing to lift yet.
    let none = ipc_as(
        &a,
        &user,
        IpcRequest::DrainTimed {
            until_micros: None,
            reason: String::new(),
            move_pending: false,
        },
    )
    .await;
    assert!(
        matches!(&none, IpcResponse::Notice { message } if message == "not draining"),
        "{none:?}"
    );

    let _ = ipc_as(
        &a,
        &user,
        IpcRequest::DrainTimed {
            until_micros: Some(mesh_proto::now_micros() + 60_000_000),
            reason: "an hour".into(),
            move_pending: false,
        },
    )
    .await;
    assert!(a.node.is_draining());
    let lifted = ipc_as(
        &a,
        &user,
        IpcRequest::DrainTimed {
            until_micros: None,
            reason: String::new(),
            move_pending: false,
        },
    )
    .await;
    assert!(
        matches!(&lifted, IpcResponse::Notice { message } if message.contains("lifted")),
        "{lifted:?}"
    );
    assert!(!a.node.is_draining());
    assert!(wait_until(Duration::from_secs(5), || !tagged_draining(&a)).await);
}
