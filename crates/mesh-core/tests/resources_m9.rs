//! M9 P3: resource-aware admission (§5.3).
//!
//! Tasks declare what they need; placement becomes bin-packing rather than
//! slot counting. Three properties matter: a task that cannot fit a node is
//! *filtered out* rather than ranked low, work spreads in proportion to the
//! hardware behind each node, and the executor itself is the authority —
//! refusing with the `Busy` the origin already knows how to spill on.

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState, Liveness, Requirements};
use mesh_testkit::{
    RunOutcome, capacity, founder_user_key, join_new_node_sized, run_to_end, spawn_founder_sized,
    stream_as, wait_until,
};

fn run_req(command: &str, reqs: Requirements) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env: reqs.to_env(),
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

async fn two_alive(a: &mesh_testkit::TestNode, b: &mesh_testkit::TestNode) -> bool {
    wait_until(Duration::from_secs(15), || {
        a.node.engine().read(|s| s.members.len() == 2)
            && matches!(
                a.node.gossip().liveness_of(&b.node.node_id()),
                Liveness::Alive
            )
            && b.node.engine().read(|s| s.members.len() == 2)
    })
    .await
}

/// A task larger than a node's RAM is not a low-scoring candidate there —
/// it is not a candidate at all, and lands on the node that can hold it.
#[tokio::test(flavor = "multi_thread")]
async fn a_big_task_never_lands_on_a_small_node() {
    let small = spawn_founder_sized("small", "m9-fit", capacity(4, 1, 200)).await;
    let big = join_new_node_sized("big", &small, capacity(8, 64, 900)).await;
    assert!(two_alive(&small, &big).await, "membership did not converge");
    let user = founder_user_key(&small).clone();

    // Submitted from the *small* node, which would otherwise win on RTT.
    let reqs = Requirements {
        cpus: 1,
        mem_mb: 4 * 1024,
        disk_mb: 0,
    };
    let outcome = run_to_end(&small, &user, run_req("echo placed", reqs)).await;
    let job_id = match outcome {
        RunOutcome::Done { state, job_id, .. } => {
            assert_eq!(state, JobState::Succeeded);
            job_id.expect("job id")
        }
        other => panic!("4 GB task did not run: {other:?}"),
    };
    let executor = small
        .node
        .engine()
        .read(|s| s.jobs.get(&job_id).map(|j| j.executor))
        .expect("job summary");
    assert_eq!(
        executor,
        big.node.node_id(),
        "a 4 GB task must land on the 64 GB node, not the 1 GB one"
    );
}

/// When nothing in the mesh is big enough, the refusal says so — a sizing
/// problem must not read as "no node matches your selector".
#[tokio::test(flavor = "multi_thread")]
async fn an_unsatisfiable_task_is_refused_by_size_not_by_selector() {
    let small = spawn_founder_sized("small", "m9-toobig", capacity(4, 16, 200)).await;
    let user = founder_user_key(&small).clone();
    let reqs = Requirements {
        cpus: 0,
        mem_mb: 512 * 1024,
        disk_mb: 0,
    };
    match run_to_end(&small, &user, run_req("echo nope", reqs)).await {
        RunOutcome::Error(message) => {
            assert!(
                message.contains("512 GB RAM") && message.contains("room"),
                "unhelpful refusal: {message}"
            );
        }
        other => panic!("expected a sizing refusal, got {other:?}"),
    }
}

/// Work spreads in proportion to the hardware behind each node: a 32-core
/// node takes roughly twice what a 16-core one does, with no operator input.
#[tokio::test(flavor = "multi_thread")]
async fn work_spreads_in_proportion_to_cores() {
    let big = spawn_founder_sized("big", "m9-spread", capacity(32, 128, 900)).await;
    let small = join_new_node_sized("small", &big, capacity(16, 64, 500)).await;
    assert!(two_alive(&big, &small).await, "membership did not converge");
    let user = founder_user_key(&big).clone();

    // 48 single-core tasks, all in flight at once, submitted from `big`.
    const TASKS: usize = 48;
    let reqs = Requirements {
        cpus: 1,
        mem_mb: 0,
        disk_mb: 0,
    };
    let mut streams = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        streams.push(stream_as(&big, &user, run_req("sleep 3", reqs)).await);
    }
    // Wait until every task has been placed.
    for rx in &mut streams {
        let first = rx.recv().await.expect("a frame");
        assert!(
            matches!(first, IpcResponse::JobStarted { .. }),
            "task not placed: {first:?}"
        );
    }

    // Counted from *replicated* state, so the tasks placed on `small`
    // arrive here by gossip rather than by the acknowledgement that already
    // came back. `JobStarted` says the executor accepted the job; it does
    // not say this node has seen the summary yet, and reading immediately
    // makes the assertion a race the loaded machine loses.
    let counts = || {
        big.node.engine().read(|s| {
            let count = |id| s.jobs.values().filter(|j| j.executor == id).count();
            (count(big.node.node_id()), count(small.node.node_id()))
        })
    };
    assert!(
        wait_until(Duration::from_secs(20), || {
            let (b, s) = counts();
            b + s == TASKS
        })
        .await,
        "not every task became visible: {:?}",
        counts()
    );
    let (on_big, on_small) = counts();
    assert_eq!(on_big + on_small, TASKS, "every task should be placed");
    assert!(
        on_small > 0,
        "the small node must get work: {on_big} / {on_small}"
    );
    let ratio = on_big as f64 / on_small as f64;
    assert!(
        (1.5..=3.0).contains(&ratio),
        "expected roughly 2:1 by cores, got {on_big}:{on_small} ({ratio:.2})"
    );
}

/// The executor is the authority. A node whose cores are already committed
/// refuses more work with `Busy` — the same definitive refusal the origin
/// has always spilled on — rather than accepting what it cannot run.
#[tokio::test(flavor = "multi_thread")]
async fn cores_are_bin_packed_and_overflow_refuses_with_busy() {
    let node = spawn_founder_sized("solo", "m9-pack", capacity(4, 32, 200)).await;
    let user = founder_user_key(&node).clone();

    // Two 2-core tasks fill a 4-core node.
    let two_cores = Requirements {
        cpus: 2,
        mem_mb: 0,
        disk_mb: 0,
    };
    let mut running = Vec::new();
    for _ in 0..2 {
        running.push(stream_as(&node, &user, run_req("sleep 3", two_cores)).await);
    }
    for rx in &mut running {
        assert!(matches!(
            rx.recv().await.expect("frame"),
            IpcResponse::JobStarted { .. }
        ));
    }
    assert!(
        wait_until(Duration::from_secs(5), || node
            .node
            .jobs_for_test()
            .running_count()
            == 2)
        .await,
        "both 2-core tasks should be running"
    );

    // A third 2-core task does not fit: the node's four cores are already
    // promised. It is refused rather than queued, so the origin can spill
    // it to a node with room — here there is none, so the run says so.
    match run_to_end(&node, &user, run_req("echo third", two_cores)).await {
        RunOutcome::Error(message) => assert!(
            message.contains("saturated"),
            "expected a saturation refusal, got {message}"
        ),
        other => panic!("a fifth core does not exist on a 4-core node: {other:?}"),
    }
    assert_eq!(
        node.node.jobs_for_test().running_count(),
        2,
        "the refusal must not have started anything"
    );
    assert_eq!(node.node.jobs_for_test().queued_count(), 0);

    // A task bigger than the whole node is refused too, and by size rather
    // than by saturation — it could not run here even on an empty node.
    let impossible = Requirements {
        cpus: 64,
        mem_mb: 0,
        disk_mb: 0,
    };
    match run_to_end(&node, &user, run_req("echo never", impossible)).await {
        RunOutcome::Error(message) => assert!(
            message.contains("room"),
            "expected a sizing refusal, got {message}"
        ),
        other => panic!("a 64-core task on a 4-core node should be refused: {other:?}"),
    }
}

/// Unstated requirements behave exactly as they did before M9 existed: no
/// reserved env, no filtering, no admission change.
#[tokio::test(flavor = "multi_thread")]
async fn unstated_requirements_are_unchanged_behaviour() {
    let node = spawn_founder_sized("solo", "m9-plain", capacity(2, 8, 100)).await;
    let user = founder_user_key(&node).clone();
    assert!(Requirements::default().to_env().is_empty());
    match run_to_end(&node, &user, run_req("echo plain", Requirements::default())).await {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("plain"));
        }
        other => panic!("a plain job must still run: {other:?}"),
    }
}
