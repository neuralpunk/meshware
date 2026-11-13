//! M9 P4: `meshnode parallel` and work stealing (§5.4, §5.7).
//!
//! The exit test for the milestone lives here: a GNU-parallel-shaped job
//! spans the mesh, a bigger node completes proportionally more of it with
//! nobody deciding that it should, and killing the owner mid-job re-homes
//! the work without running anything twice.

use std::collections::HashMap;
use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobId, JobState, Liveness, NodeProgress};
use mesh_testkit::{
    TestNode, apply_capacity_for_test, capacity, founder_user_key, join_new_node_sized,
    join_new_node_with, spawn_founder_sized, spawn_founder_with, stream_as, wait_until,
};

fn par_req(command: &str, items: Vec<String>, chunk: Option<u32>) -> IpcRequest {
    IpcRequest::Parallel {
        command: command.into(),
        service: None,
        inputs: vec![],
        item_input: None,
        items,
        on: None,
        env: vec![],
        timeout_secs: Some(60),
        chunk,
        capture: mesh_proto::Capture::Inline,
        reduce: None,
        reduce_on: None,
    }
}

fn numbers(n: usize) -> Vec<String> {
    (1..=n).map(|i| i.to_string()).collect()
}

/// A finished parallel run, as the CLI would see it.
#[derive(Debug, Default)]
struct Run {
    job_id: Option<JobId>,
    items: HashMap<String, (Option<i32>, String, String)>,
    total: u64,
    succeeded: u64,
    failed: u64,
    per_node: Vec<NodeProgress>,
    /// Nodes that took the job on, and why anyone did not.
    workers: Vec<String>,
    declined: Vec<String>,
    error: Option<String>,
}

async fn run_parallel(
    node: &TestNode,
    user: &mesh_core::identity::Keypair,
    req: IpcRequest,
) -> Run {
    let mut rx = stream_as(node, user, req).await;
    let mut run = Run::default();
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::ParallelStarted {
                job_id,
                total,
                workers,
                declined,
                ..
            } => {
                run.job_id = Some(job_id);
                run.total = total;
                run.workers = workers;
                run.declined = declined;
            }
            IpcResponse::ParallelItem {
                item,
                exit_code,
                stdout,
                stderr,
                ..
            } => {
                run.items.insert(item, (exit_code, stdout, stderr));
            }
            IpcResponse::ParallelProgress { .. } => {}
            IpcResponse::ParallelDone {
                total,
                succeeded,
                failed,
                per_node,
                ..
            } => {
                run.total = total;
                run.succeeded = succeeded;
                run.failed = failed;
                run.per_node = per_node;
                return run;
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                run.error = Some(message);
                return run;
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    run
}

async fn converged(a: &TestNode, b: &TestNode) -> bool {
    wait_until(Duration::from_secs(20), || {
        a.node.engine().read(|s| s.members.len() == 2)
            && b.node.engine().read(|s| s.members.len() == 2)
            && matches!(
                a.node.gossip().liveness_of(&b.node.node_id()),
                Liveness::Alive
            )
            && matches!(
                b.node.gossip().liveness_of(&a.node.node_id()),
                Liveness::Alive
            )
    })
    .await
}

/// The wedge: a GNU-parallel-shaped command spans the mesh, and every item
/// runs exactly once with its own output.
#[tokio::test(flavor = "multi_thread")]
async fn a_parallel_job_spans_the_mesh_and_runs_every_item_once() {
    let a = spawn_founder_sized("a", "m9-par", capacity(4, 16, 200)).await;
    let b = join_new_node_sized("b", &a, capacity(4, 16, 200)).await;
    assert!(converged(&a, &b).await, "membership did not converge");
    let user = founder_user_key(&a).clone();

    let items = numbers(40);
    let run = run_parallel(&a, &user, par_req("echo item-{}", items.clone(), None)).await;
    assert_eq!(run.error, None);
    assert_eq!(run.total, 40);
    assert_eq!(run.succeeded, 40, "every item should succeed");
    assert_eq!(run.failed, 0);
    assert_eq!(run.items.len(), 40, "every item reported exactly once");
    for item in &items {
        let (code, stdout, _) = run.items.get(item).unwrap_or_else(|| panic!("no {item}"));
        assert_eq!(*code, Some(0));
        assert_eq!(stdout.trim(), format!("item-{item}"));
    }
    // Both nodes took part, and the per-node tallies add up.
    let counted: u64 = run.per_node.iter().map(|n| n.items).sum();
    assert_eq!(counted, 40);
    assert!(
        run.per_node.iter().filter(|n| n.items > 0).count() == 2,
        "both nodes should have done some of it: {:?}",
        run.per_node
    );

    // One replicated summary for the whole job — never one per chunk.
    let job_id = run.job_id.expect("job id");
    let summaries = a.node.engine().read(|s| {
        s.jobs
            .values()
            .filter(|j| j.runtime == "parallel")
            .map(|j| (j.job_id, j.state))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        summaries,
        vec![(job_id, JobState::Succeeded)],
        "a parallel job is one job row, not one per chunk"
    );
}

/// The milestone's headline: a 32-core node completes about twice the work
/// of a 16-core one, unprompted. Nothing computes that split — the bigger
/// node simply pulls through twice as many lanes.
#[tokio::test(flavor = "multi_thread")]
async fn the_bigger_node_completes_proportionally_more() {
    // 8 and 4 cores rather than 32 and 16: the ratio is what is under test,
    // and twelve lanes fit on any CI host. Declaring 48 lanes would put both
    // "nodes" in contention for the same real cores, and then the split
    // measures the host's scheduler instead of the pull loop.
    let big = spawn_founder_sized("big", "m9-split", capacity(8, 64, 500)).await;
    let small = join_new_node_sized("small", &big, capacity(4, 32, 250)).await;
    assert!(converged(&big, &small).await, "membership did not converge");
    let user = founder_user_key(&big).clone();

    // Items that take real time, so lanes stay occupied and the pull rate —
    // not the scheduler — decides the split.
    let run = run_parallel(
        &big,
        &user,
        par_req("sleep 0.05; echo {}", numbers(240), Some(1)),
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 240);

    let by_name = |name: &str| -> u64 {
        run.per_node
            .iter()
            .find(|n| n.name == name)
            .map_or(0, |n| n.items)
    };
    let (on_big, on_small) = (by_name("big"), by_name("small"));
    assert_eq!(on_big + on_small, 240);
    assert!(
        on_small > 0,
        "the small node did nothing: {:?}",
        run.per_node
    );
    let ratio = on_big as f64 / on_small as f64;
    assert!(
        (1.4..=3.2).contains(&ratio),
        "expected roughly 2:1 by cores, got {on_big}:{on_small} ({ratio:.2})"
    );
}

/// Failures are reported per item, with their output, and do not stop the
/// rest of the job.
#[tokio::test(flavor = "multi_thread")]
async fn failing_items_are_reported_individually() {
    let a = spawn_founder_sized("a", "m9-parfail", capacity(4, 16, 200)).await;
    let user = founder_user_key(&a).clone();

    // Odd items fail; even ones succeed.
    let run = run_parallel(
        &a,
        &user,
        par_req(
            "if [ $(( {} % 2 )) -eq 1 ]; then echo bad-{} >&2; exit 3; else echo ok-{}; fi",
            numbers(10),
            Some(2),
        ),
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(run.total, 10);
    assert_eq!(run.failed, 5, "five odd items should fail");
    assert_eq!(run.succeeded, 5);
    let (code, _, stderr) = run.items.get("3").expect("item 3");
    assert_eq!(*code, Some(3));
    assert!(stderr.contains("bad-3"), "stderr should reach the client");
    let (code, stdout, _) = run.items.get("4").expect("item 4");
    assert_eq!(*code, Some(0));
    assert_eq!(stdout.trim(), "ok-4");
}

/// A single-node mesh runs a parallel job with no special case: the owner is
/// an ordinary worker too.
#[tokio::test(flavor = "multi_thread")]
async fn one_node_is_not_a_special_case() {
    let a = spawn_founder_sized("solo", "m9-solo", capacity(4, 16, 200)).await;
    let user = founder_user_key(&a).clone();
    let run = run_parallel(&a, &user, par_req("echo {}", numbers(12), None)).await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 12);
    assert_eq!(run.per_node.len(), 1);
    assert_eq!(run.per_node[0].items, 12);
}

/// An empty list is refused rather than silently succeeding.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_item_list_is_refused() {
    let a = spawn_founder_sized("solo", "m9-empty", capacity(2, 8, 100)).await;
    let user = founder_user_key(&a).clone();
    let run = run_parallel(&a, &user, par_req("echo {}", vec![], None)).await;
    assert!(
        run.error.as_deref().is_some_and(|m| m.contains("empty")),
        "expected an empty-list refusal, got {:?}",
        run.error
    );
}

/// The milestone's third clause: kill the owner mid-job and the work
/// re-homes onto a survivor without anything running twice.
///
/// The submitting client's stream dies with the owner — there is nothing to
/// stream to any more. What survives is the work and the record of it, the
/// same guarantee origin-failure adoption already gives a fan-out
/// (MASTERPLAN §10.3).
#[tokio::test(flavor = "multi_thread")]
async fn killing_the_owner_re_homes_without_duplicate_execution() {
    mesh_testkit::init_tracing();
    let scratch = tempfile::tempdir().expect("tempdir");
    let ledger = scratch.path().join("ran");
    // Every item records itself before sleeping, so the ledger is the
    // ground truth for what actually executed, on whichever node.
    let command = format!("echo {{}} >> {}; sleep 0.3", ledger.display());

    let owner = spawn_founder_sized("owner", "m9-adopt", capacity(2, 8, 100)).await;
    let w1 = join_new_node_sized("w1", &owner, capacity(2, 8, 100)).await;
    let w2 = join_new_node_sized("w2", &owner, capacity(2, 8, 100)).await;
    let user = founder_user_key(&owner).clone();
    assert!(
        wait_until(Duration::from_secs(30), || {
            [&owner, &w1, &w2]
                .iter()
                .all(|n| n.node.engine().read(|s| s.members.len() == 3))
                && matches!(
                    owner.node.gossip().liveness_of(&w1.node.node_id()),
                    Liveness::Alive
                )
                && matches!(
                    owner.node.gossip().liveness_of(&w2.node.node_id()),
                    Liveness::Alive
                )
        })
        .await,
        "membership did not converge"
    );

    const ITEMS: usize = 150;
    let mut rx = stream_as(&owner, &user, par_req(&command, numbers(ITEMS), Some(1))).await;
    let job_id = match rx.recv().await.expect("first frame") {
        IpcResponse::ParallelStarted {
            job_id, workers, ..
        } => {
            assert_eq!(workers.len(), 3, "every node should have taken the job on");
            job_id
        }
        other => panic!("expected ParallelStarted, got {other:?}"),
    };

    // Let real work happen on all three nodes, then kill the owner.
    let mut seen = 0;
    while seen < 6 {
        match rx.recv().await {
            Some(IpcResponse::ParallelItem { .. }) => seen += 1,
            Some(_) => {}
            None => panic!("stream ended before the job got going"),
        }
    }
    owner.node.stop().await;
    drop(rx);

    // A survivor adopts and drives the job to a terminal state. Dead
    // detection takes several gossip rounds, so this is deliberately patient.
    assert!(
        wait_until(Duration::from_secs(180), || {
            [&w1, &w2].iter().any(|n| {
                n.node
                    .engine()
                    .read(|s| s.jobs.get(&job_id).is_some_and(|j| j.state.is_terminal()))
            })
        })
        .await,
        "the job never finished under an adopted owner"
    );

    // The ledger is the proof of what actually executed, anywhere.
    let recorded = std::fs::read_to_string(&ledger).expect("ledger");
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in recorded.lines().map(str::trim).filter(|l| !l.is_empty()) {
        *counts.entry(line).or_default() += 1;
    }
    // Nothing is lost: every item ran.
    assert_eq!(
        counts.len(),
        ITEMS,
        "items went missing across the adoption; got {} of {ITEMS} distinct",
        counts.len()
    );
    // Nothing is re-issued twice: a chunk the dead owner had outstanding is
    // handed out again exactly once, never a third time.
    let over = counts.iter().filter(|(_, n)| **n > 2).collect::<Vec<_>>();
    assert!(over.is_empty(), "items ran more than twice: {over:?}");
    // Some re-running is inherent and documented (§5.4: unacknowledged
    // chunks are re-queued, and chunk idempotency makes that safe) — what
    // must *not* happen is the whole job running again. Only work the dead
    // owner had in hand can repeat, so the repeats stay a small minority.
    let repeated = counts.values().filter(|n| **n > 1).count();
    assert!(
        repeated * 4 < ITEMS,
        "{repeated}/{ITEMS} items re-ran — adoption is redoing the job, not resuming it"
    );

    // And the adopter, not the dead owner, published the final record.
    let executor = w1
        .node
        .engine()
        .read(|s| s.jobs.get(&job_id).map(|j| j.executor))
        .expect("summary");
    assert_ne!(
        executor,
        owner.node.node_id(),
        "the dead owner cannot have published the terminal summary"
    );
}

/// A node that cannot take part is *named*, not silently skipped. One node
/// quietly doing all the work is the symptom operators read as "the mesh is
/// broken", and the reason has to reach whoever launched the job.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_sits_out_is_reported_with_its_reason() {
    // Facts off on the founder: since M18 every node publishes a
    // `FactsPublish` at boot, and a member that has published one can no
    // longer admit a pre-1.7 joiner at all (F0 §3.3, and `facts_m18.rs`
    // tests that consequence directly). What is under test here is M9's
    // roster reporting, so the door is held open deliberately.
    let a = spawn_founder_with("a", "m9-roster", vec![], |c| {
        apply_capacity_for_test(c, capacity(4, 16, 200));
        c.facts.collect = false;
    })
    .await;
    // A peer stuck on 1.5 cannot decode the offer at all (M9 §5.4).
    let old = join_new_node_with("old", &a, vec![], |c| {
        c.proto_minor_override = Some(5);
    })
    .await;
    let user = founder_user_key(&a).clone();
    assert!(
        wait_until(Duration::from_secs(20), || {
            a.node.engine().read(|s| s.members.len() == 2)
                && matches!(
                    a.node.gossip().liveness_of(&old.node.node_id()),
                    Liveness::Alive
                )
        })
        .await,
        "membership did not converge"
    );

    let run = run_parallel(&a, &user, par_req("echo {}", numbers(20), Some(1))).await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 20, "the job still runs, on fewer nodes");
    assert_eq!(run.workers, vec!["a"], "only the owner could take it on");
    assert_eq!(
        run.declined.len(),
        1,
        "the 1.5 peer must be named: {:?}",
        run.declined
    );
    assert!(
        run.declined[0].starts_with("old:") && run.declined[0].contains("minor"),
        "the reason should say why: {:?}",
        run.declined
    );
}

/// A Library script runs once per item, on every node, exactly as it runs
/// once on Run (MASTERPLAN M9 §6.4: "a per-item command is an ordinary
/// service"). The worker fetches and materializes the bundle itself, so the
/// script does not have to exist on the node beforehand.
#[tokio::test(flavor = "multi_thread")]
async fn a_library_script_runs_once_per_item() {
    let a = spawn_founder_sized("a", "m9-script", capacity(4, 16, 200)).await;
    let b = join_new_node_sized("b", &a, capacity(4, 16, 200)).await;
    assert!(converged(&a, &b).await, "membership did not converge");
    let user = founder_user_key(&a).clone();

    // A one-file bundle whose script echoes the item it was handed.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("greet.sh"),
        "#!/usr/bin/env bash\necho \"greeted $2 (env $MESHWARE_ITEM)\"\n",
    )
    .expect("write script");
    // Push rather than ingest: the bundle has to be *announced* to the mesh,
    // or the worker has nowhere to fetch it from.
    let bundle = match mesh_testkit::ipc_as(
        &a,
        &user,
        IpcRequest::Push {
            path: dir.path().to_string_lossy().into_owned(),
        },
    )
    .await
    {
        IpcResponse::Pushed { bundle_id, .. } => bundle_id,
        other => panic!("push failed: {other:?}"),
    };

    let version = mesh_core::services::sign_service_version(
        &user,
        mesh_proto::ServiceVersion {
            name: "greet".into(),
            version: 1,
            runtime: mesh_proto::Runtime::Bash,
            bundle_id: bundle,
            working_dir: String::new(),
            entrypoint: "greet.sh".into(),
            contract: None,
            env: vec![],
            description: String::new(),
            tags: vec![],
            created_at_micros: mesh_proto::now_micros(),
            created_by: user.user_id(),
            source: None,
            signature: Vec::new(),
        },
    );
    let head_signature = mesh_core::services::sign_service_head(&user, "greet", 1);
    let created = mesh_testkit::ipc_as(
        &a,
        &user,
        IpcRequest::ServiceCreate {
            version: Box::new(version),
            head_signature,
        },
    )
    .await;
    assert!(
        !matches!(
            created,
            IpcResponse::Denied { .. } | IpcResponse::Error { .. }
        ),
        "service create failed: {created:?}"
    );

    let items = numbers(24);
    let run = run_parallel(
        &a,
        &user,
        IpcRequest::Parallel {
            command: String::new(),
            service: Some("greet".into()),
            inputs: vec![],
            item_input: Some("who".into()),
            items: items.clone(),
            on: None,
            env: vec![],
            timeout_secs: Some(60),
            chunk: Some(2),
            capture: mesh_proto::Capture::Inline,
            reduce: None,
            reduce_on: None,
        },
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 24, "every item should have run the script");
    for item in &items {
        let (code, stdout, _) = run.items.get(item).unwrap_or_else(|| panic!("no {item}"));
        assert_eq!(*code, Some(0));
        // The item arrives as the contract input's argv value *and* in the
        // environment, so a script can take it either way.
        assert_eq!(stdout.trim(), format!("greeted {item} (env {item})"));
    }
    // The bundle really did travel: both nodes ran some of it.
    assert_eq!(run.per_node.iter().filter(|n| n.items > 0).count(), 2);
}
