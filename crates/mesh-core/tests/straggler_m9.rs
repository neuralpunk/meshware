//! M9 P7: straggler speculation (§5.6).
//!
//! At the tail of a parallel job, the wall clock *is* the slowest
//! outstanding chunk. One node that has gone quiet — swapping, throttling,
//! stopped — holds up work that everybody else finished minutes ago. The
//! answer is to run its share again somewhere else and take whichever copy
//! reports first.
//!
//! The precondition already held before M9: acceptance is idempotent, so
//! running an item twice is safe. What P7 adds is knowing *when* it is
//! worth it, and making sure the losing copy contributes nothing — not a
//! count, not a failure, not a line of output.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mesh_core::parallel::{DEFAULT_SPECULATE_AT, ParallelJob, Pull, Straggler};
use mesh_proto::{
    Capture, ChunkReport, ItemOutcome, JobId, NodeId, ParallelPlan, SignedIpc, UserId,
};

fn plan(items: usize, chunk: Option<u32>) -> Arc<ParallelPlan> {
    Arc::new(ParallelPlan {
        job: JobId::generate(),
        command: "echo {}".into(),
        exec: None,
        item_input: None,
        items: (0..items).map(|i| i.to_string()).collect(),
        env: vec![],
        timeout_secs: None,
        chunk,
        capture: Capture::Inline,
        inputs: vec![],
        workers: vec![],
        submitter_user: UserId([0; 32]),
        auth: SignedIpc {
            user: UserId([0; 32]),
            at_micros: 0,
            nonce: 0,
            payload: vec![],
            signature: vec![],
        },
    })
}

fn report(
    job: &ParallelJob,
    worker: NodeId,
    assignment: &mesh_proto::ChunkAssignment,
    micros: u64,
) {
    let outcomes = assignment
        .slice(job.items())
        .iter()
        .map(|item| ItemOutcome {
            item: item.clone(),
            exit_code: Some(0),
            stdout: format!("ran {item}"),
            stderr: String::new(),
            micros,
            output: None,
        })
        .collect();
    job.report(
        worker,
        &ChunkReport {
            job: job.id(),
            chunk: assignment.chunk,
            outcomes,
        },
    );
}

/// How long a chunk must sit before it is eligible to be raced. The
/// production floor is five seconds — a worker's first chunk legitimately
/// includes fetching a bundle — which no test should wait out, so these
/// state a short one instead of pretending the floor is not there.
const MIN_WAIT: Duration = Duration::from_millis(20);
const PAST_ELIGIBILITY: Duration = Duration::from_millis(60);
const ITEM_MICROS: u64 = 1_000;

fn racing() -> Option<Straggler> {
    Some(Straggler {
        at: DEFAULT_SPECULATE_AT,
        min_wait: MIN_WAIT,
    })
}

/// Drain everything the queue will hand `worker` right now, stopping at the
/// first chunk covering `avoid` (the stalled slice's copy) or at `Wait`.
fn drain_until_copy(
    job: &ParallelJob,
    worker: NodeId,
    name: &str,
    avoid: u32,
) -> (usize, Option<mesh_proto::ChunkAssignment>) {
    let mut done = 0;
    loop {
        match job.next_chunk(worker, name) {
            Pull::Work(a) if a.start == avoid => return (done, Some(*a)),
            Pull::Work(a) => {
                report(job, worker, &a, ITEM_MICROS);
                done += 1;
            }
            Pull::Wait | Pull::Done => return (done, None),
        }
    }
}

/// The exit property, in the queue where it is decided: one worker stops
/// answering at the tail, and the job still finishes — because its share is
/// handed to somebody else rather than waited on.
#[tokio::test]
async fn a_stalled_worker_at_the_tail_has_its_share_re_run_elsewhere() {
    // 20 items, one per chunk: 19 done is 95%, which is the tail.
    let (job, _results) = ParallelJob::own_with(plan(20, Some(1)), racing());
    let quick = NodeId([1; 32]);
    let stalled = NodeId([2; 32]);

    // The stalled worker takes one chunk and never reports it.
    let stuck = job
        .next_chunk(stalled, "stalled")
        .work()
        .expect("first chunk");
    // Everybody else finishes everything else. Nothing is speculated yet:
    // the stalled chunk has been outstanding for microseconds, and a chunk
    // handed out a moment ago is not evidence of anything.
    let (done, copy) = drain_until_copy(&job, quick, "quick", stuck.start);
    assert_eq!(done, 19, "everything but the stalled slice finished");
    assert!(
        copy.is_none(),
        "nothing is raced on the strength of no delay"
    );
    assert_eq!(job.progress().speculated, 0);

    // The queue does not retire the lane, though — there is outstanding
    // work that may yet come back to it.
    assert!(
        matches!(job.next_chunk(quick, "quick"), Pull::Wait),
        "a lane waits at the tail rather than retiring"
    );

    // Once the chunk has been outstanding long enough to look stuck, its
    // slice is offered to somebody else.
    tokio::time::sleep(PAST_ELIGIBILITY).await;
    let copy = job
        .next_chunk(quick, "quick")
        .work()
        .expect("the stalled slice is handed out again");
    assert_eq!(copy.start, stuck.start, "it covers the stalled slice");
    assert_ne!(copy.chunk, stuck.chunk, "and it is a distinct attempt");

    let p = job.progress();
    assert_eq!(p.done, 19);
    assert_eq!(p.speculated, 1, "the stalled slice was handed out again");
    assert!(!job.finished(), "one item is still outstanding");

    // The second copy finishes. That settles the item, and the job is done
    // *without* the stalled worker ever answering.
    report(&job, quick, &copy, ITEM_MICROS);
    assert!(job.finished(), "the job finished around the stalled worker");
    let p = job.progress();
    assert_eq!(p.done, 20, "every item counted exactly once");
    assert_eq!(p.failed, 0);
}

/// The loser of a race contributes nothing. This is what stops speculation
/// showing up as phantom results, double counts, or — worst — phantom
/// failures (§5.6).
#[tokio::test]
async fn the_losing_copy_of_a_race_is_not_counted_twice() {
    let (job, mut results) = ParallelJob::own_with(plan(20, Some(1)), racing());
    let slow = NodeId([1; 32]);
    let fast = NodeId([2; 32]);

    let stuck = job.next_chunk(slow, "slow").work().expect("first chunk");
    let (_, none) = drain_until_copy(&job, fast, "fast", stuck.start);
    assert!(none.is_none());
    tokio::time::sleep(PAST_ELIGIBILITY).await;
    let copy = job
        .next_chunk(fast, "fast")
        .work()
        .expect("a speculative copy was issued");

    // The copy wins.
    report(&job, fast, &copy, ITEM_MICROS);
    let after_win = job.progress();
    assert_eq!(after_win.done, 20);

    // The original finally reports — and changes nothing. Not the count,
    // not the failure tally, and no second line of output for the item.
    report(&job, slow, &stuck, 9_000_000);
    let after_loser = job.progress();
    assert_eq!(after_loser.done, 20, "the late copy did not double-count");
    assert_eq!(after_loser.failed, 0);

    // Exactly one result frame per item reached the stream.
    let mut streamed = Vec::new();
    while let Ok(result) = results.try_recv() {
        streamed.extend(result.outcomes.into_iter().map(|o| o.item));
    }
    streamed.sort();
    let unique = {
        let mut u = streamed.clone();
        u.dedup();
        u
    };
    assert_eq!(
        streamed.len(),
        unique.len(),
        "an item must be reported to the client exactly once"
    );
    assert_eq!(unique.len(), 20);
}

/// Speculation is a *tail* policy. Duplicating outstanding work in the
/// middle of a job spends a lane for nothing, so it does not happen.
#[tokio::test]
async fn nothing_is_speculated_before_the_tail() {
    let (job, _results) = ParallelJob::own_with(plan(100, Some(1)), racing());
    let slow = NodeId([1; 32]);
    let fast = NodeId([2; 32]);

    let held = job.next_chunk(slow, "slow").work().expect("first chunk");
    // Half the job, well short of the 95% threshold — and with plenty of
    // delay, so it is the threshold doing the work and not the timer.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(1)).await;
        let a = job.next_chunk(fast, "fast").work().expect("chunk");
        assert_ne!(
            a.start, held.start,
            "the held slice must not be re-issued mid-job"
        );
        report(&job, fast, &a, ITEM_MICROS);
    }
    assert_eq!(
        job.progress().speculated,
        0,
        "speculation is for the tail, not for every outstanding chunk"
    );
}

/// **The case the exit test found, and the plan did not.**
///
/// A frozen node keeps whatever share it had already taken. If that share
/// is bigger than the tail — 42 items of 400 in the run that surfaced this
/// — then `done` stalls *below* the 95% threshold and never reaches it, so
/// a fraction-only gate never opens and the job waits out the full chunk
/// grace instead. An exhausted item list is the second gate, and it is the
/// one that fires here.
#[tokio::test]
async fn a_node_holding_more_than_the_tail_is_still_raced() {
    // 40 items, one per chunk. The stalled worker takes 8 of them — 20% of
    // the job, four times the 5% the tail threshold allows for.
    let (job, _results) = ParallelJob::own_with(plan(40, Some(1)), racing());
    let quick = NodeId([1; 32]);
    let frozen = NodeId([2; 32]);

    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(job.next_chunk(frozen, "frozen").work().expect("chunk"));
    }
    // Everything else finishes. 32 of 40 is 80% — nowhere near the tail.
    while let Some(a) = job.next_chunk(quick, "quick").work() {
        if held.iter().any(|h| h.start == a.start) {
            break;
        }
        report(&job, quick, &a, ITEM_MICROS);
    }
    let stalled = job.progress();
    assert_eq!(stalled.done, 32);
    assert!(
        (stalled.done as f64 / 40.0) < DEFAULT_SPECULATE_AT,
        "the premise: a fraction gate alone would never open here"
    );

    // The list is exhausted, so the second gate opens and the frozen
    // node's share is raced — no waiting for a grace period to expire.
    tokio::time::sleep(PAST_ELIGIBILITY).await;
    let mut raced = 0;
    while let Some(a) = job.next_chunk(quick, "quick").work() {
        report(&job, quick, &a, ITEM_MICROS);
        raced += 1;
    }
    assert_eq!(raced, 8, "every held item was re-run somewhere else");
    assert!(job.finished(), "the job finished around the frozen node");
    let p = job.progress();
    assert_eq!(p.done, 40, "every item counted exactly once");
    assert_eq!(p.failed, 0);
    assert_eq!(p.speculated, 8);
}

/// An operator who turns speculation off gets a job that waits, which is
/// the correct behaviour for work that must not run twice for reasons the
/// mesh cannot see.
#[tokio::test]
async fn speculation_can_be_turned_off() {
    let (job, _results) = ParallelJob::own_with(plan(20, Some(1)), None);
    let slow = NodeId([1; 32]);
    let fast = NodeId([2; 32]);
    let held = job.next_chunk(slow, "slow").work().expect("first chunk");
    while let Some(a) = job.next_chunk(fast, "fast").work() {
        assert_ne!(a.start, held.start, "nothing is re-issued with it off");
        report(&job, fast, &a, ITEM_MICROS);
    }
    tokio::time::sleep(PAST_ELIGIBILITY).await;
    assert!(
        job.next_chunk(fast, "fast").work().is_none(),
        "nothing is re-issued with speculation off, however long it waits"
    );
    assert_eq!(job.progress().speculated, 0);
    assert!(!job.finished(), "the job waits for the held slice");
}

/// A worker never races itself: the copy exists to be run somewhere the
/// original is not.
#[tokio::test]
async fn a_worker_is_not_handed_a_copy_of_its_own_chunk() {
    let (job, _results) = ParallelJob::own_with(plan(20, Some(1)), racing());
    let one = NodeId([1; 32]);
    let two = NodeId([2; 32]);

    let held = job.next_chunk(one, "one").work().expect("first chunk");
    drain_until_copy(&job, two, "two", held.start);
    tokio::time::sleep(PAST_ELIGIBILITY).await;

    // The tail is reached and a copy is eligible — but asking as the
    // worker that already holds it gets nothing but a wait.
    assert!(
        matches!(job.next_chunk(one, "one"), Pull::Wait),
        "a node must not be handed a race against itself"
    );
    // Anyone else gets it.
    let copy = job
        .next_chunk(two, "two")
        .work()
        .expect("the other node gets it");
    assert_eq!(copy.start, held.start);
}

// ---------------------------------------------------------------------------
// The exit test, end to end
// ---------------------------------------------------------------------------

/// **P7's exit test**: stop one worker mid-job; the job still finishes near
/// nominal time rather than waiting out the stalled node's grace period.
///
/// A real `SIGSTOP` is simulated exactly as it behaves — a worker that took
/// work and will never answer — which is the only property the queue can
/// actually observe about a stopped process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_finishes_near_nominal_time_with_one_worker_stopped() {
    const ITEMS: usize = 200;
    const COST_MICROS: u64 = 5_000;

    let (job, _results) = ParallelJob::own_with(plan(ITEMS, Some(1)), racing());
    job.add_lanes(4);
    let stopped = NodeId([9; 32]);
    let stalled_chunks = Arc::new(AtomicBool::new(false));

    // One worker takes a chunk and stops dead, as SIGSTOP would.
    let stuck = job.next_chunk(stopped, "stopped").work().expect("chunk");
    stalled_chunks.store(true, Ordering::SeqCst);

    // Three healthy workers drain the rest, and — this is the part that
    // matters — *wait* at the tail rather than retiring, exactly as a real
    // lane does, so there is somebody left to run the re-run.
    let started = Instant::now();
    let mut handles = Vec::new();
    for id in 1..=3u8 {
        let job = job.clone();
        handles.push(tokio::spawn(async move {
            let worker = NodeId([id; 32]);
            let name = format!("worker-{id}");
            loop {
                match job.next_chunk(worker, &name) {
                    Pull::Work(a) => {
                        tokio::time::sleep(Duration::from_micros(COST_MICROS * u64::from(a.count)))
                            .await;
                        report(&job, worker, &a, COST_MICROS);
                    }
                    Pull::Wait => tokio::time::sleep(Duration::from_millis(5)).await,
                    Pull::Done => break,
                }
            }
        }));
    }
    for h in handles {
        h.await.expect("worker task");
    }
    let elapsed = started.elapsed();

    assert!(stalled_chunks.load(Ordering::SeqCst));
    assert!(
        job.finished(),
        "the job must complete without the stopped worker"
    );
    let p = job.progress();
    assert_eq!(p.done, ITEMS as u64, "every item finished exactly once");
    assert_eq!(p.failed, 0);
    assert!(p.speculated >= 1, "the stopped worker's share was re-run");

    // Nominal is 200 items over 3 workers at 5 ms each. The point of the
    // assertion is the *shape*: it finished in work-time rather than
    // waiting out the 30-second grace period a stalled chunk would
    // otherwise sit in.
    assert!(
        elapsed < Duration::from_secs(10),
        "finished in {elapsed:?} — a stalled chunk must not be waited out"
    );

    // And the stopped worker's late report still changes nothing.
    report(&job, stopped, &stuck, 30_000_000);
    assert_eq!(job.progress().done, ITEMS as u64);
}
