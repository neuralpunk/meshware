//! M9 P6: the ephemeral object store (§5.5).
//!
//! The claim under test is the one that makes rung 3 possible: a parallel
//! job's intermediate data stays on the node that produced it, and any node
//! that needs it fetches it from there — never through the node that owns
//! the job. Without that, every byte funnels through the origin and the
//! origin becomes the central bottleneck the project exists to avoid (G4).

use std::collections::HashMap;
use std::time::Duration;

use mesh_proto::{Capture, IpcRequest, IpcResponse, Liveness, NodeId, ObjectRef};
use mesh_testkit::{
    TestNode, capacity, founder_user_key, ipc_as, join_new_node_sized, spawn_founder_sized,
    stream_as, wait_until,
};

fn map_req(command: &str, items: Vec<String>, chunk: Option<u32>) -> IpcRequest {
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
        capture: Capture::Object,
        reduce: None,
        reduce_on: None,
    }
}

fn numbers(n: usize) -> Vec<String> {
    (1..=n).map(|i| i.to_string()).collect()
}

#[derive(Debug, Default)]
struct MapRun {
    /// item -> (object, the node that produced it)
    outputs: HashMap<String, (ObjectRef, NodeId)>,
    total: u64,
    succeeded: u64,
    failed: u64,
    reduce: Option<(String, String)>,
    error: Option<String>,
}

async fn run_map(node: &TestNode, user: &mesh_core::identity::Keypair, req: IpcRequest) -> MapRun {
    let mut rx = stream_as(node, user, req).await;
    let mut run = MapRun::default();
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::ParallelStarted { total, .. } => run.total = total,
            IpcResponse::ParallelItem {
                item,
                output: Some(obj),
                node,
                ..
            } => {
                run.outputs.insert(item, (obj, node));
            }
            IpcResponse::ParallelReduced {
                node_name,
                stdout,
                stderr,
                exit_code,
                inputs,
                ..
            } => {
                assert_eq!(
                    exit_code,
                    Some(0),
                    "the reduce failed on {node_name} over {inputs} input(s): {stderr}"
                );
                run.reduce = Some((node_name, stdout));
            }
            IpcResponse::ParallelDone {
                succeeded, failed, ..
            } => {
                run.succeeded = succeeded;
                run.failed = failed;
            }
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                run.error = Some(message);
            }
            _ => {}
        }
    }
    run
}

/// Every object the node holds right now.
async fn objects_on(
    node: &TestNode,
    user: &mesh_core::identity::Keypair,
) -> Vec<mesh_proto::ObjectRow> {
    match ipc_as(node, user, IpcRequest::Objects).await {
        IpcResponse::Objects(rows) => rows,
        other => panic!("expected an object list, got {other:?}"),
    }
}

/// Fetch an object through the daemon, exactly as `meshnode objects get`
/// would — no hints, so the lookup goes through the rendezvous directory.
async fn fetch_object(
    node: &TestNode,
    user: &mesh_core::identity::Keypair,
    id: mesh_proto::ObjectId,
    hints: Vec<NodeId>,
) -> Result<Vec<u8>, String> {
    let mut rx = stream_as(node, user, IpcRequest::ObjectGet { id, hints }).await;
    let mut bytes = Vec::new();
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::ObjectChunk { data } => bytes.extend_from_slice(&data),
            IpcResponse::ObjectEnd { size } => {
                assert_eq!(size, bytes.len() as u64, "the declared size must match");
                return Ok(bytes);
            }
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                return Err(message);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    Err("the object stream ended without a terminal frame".into())
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

// ---------------------------------------------------------------------------

/// **The P6 exit property.** A map's output lives where it was computed.
/// The origin sees references; it does not see, store or forward bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_maps_output_stays_on_the_node_that_produced_it() {
    let origin = spawn_founder_sized("origin", "objects", capacity(2, 8, 200)).await;
    let worker = join_new_node_sized("worker", &origin, capacity(2, 8, 200)).await;
    assert!(converged(&origin, &worker).await, "mesh must converge");
    let user = founder_user_key(&origin);

    // Each item prints something unique and large enough to be worth
    // caring about where it lives.
    let run = run_map(
        &origin,
        user,
        map_req("printf 'result-%s\\n' {}", numbers(40), Some(2)),
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 40, "every item must succeed");
    assert_eq!(run.outputs.len(), 40, "every item must produce an object");

    // The origin holds only what its own lanes produced. Every other
    // object is on the worker, and the origin never saw its bytes.
    let here = objects_on(&origin, user).await;
    let mine: Vec<_> = run
        .outputs
        .values()
        .filter(|(_, node)| *node == origin.node.node_id())
        .collect();
    let theirs: Vec<_> = run
        .outputs
        .values()
        .filter(|(_, node)| *node == worker.node.node_id())
        .collect();
    assert!(!theirs.is_empty(), "the worker must have done some of it");
    for (obj, _) in &theirs {
        assert!(
            !here.iter().any(|row| row.id == obj.id),
            "an object the worker produced must not be sitting on the origin"
        );
    }
    for (obj, _) in &mine {
        assert!(
            here.iter().any(|row| row.id == obj.id),
            "the origin must hold what its own lanes produced"
        );
    }
    // And the reference names the producer, which is what lets a fetcher
    // skip the directory in the common case.
    for (obj, node) in run.outputs.values() {
        assert_eq!(obj.holder, *node, "a reference names its producer");
        assert!(obj.size > 0, "an item that printed produced bytes");
    }
}

/// Anyone can read an object they never produced, through the
/// rendezvous-hashed directory — with no hint, and with no help from the
/// node that owned the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn any_node_can_fetch_an_object_it_never_produced() {
    let origin = spawn_founder_sized("origin", "objects", capacity(2, 8, 200)).await;
    let worker = join_new_node_sized("worker", &origin, capacity(2, 8, 200)).await;
    assert!(converged(&origin, &worker).await, "mesh must converge");
    let user = founder_user_key(&origin);

    let run = run_map(
        &origin,
        user,
        map_req("printf 'payload-%s' {}", numbers(20), Some(1)),
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 20);

    // Pick something the *worker* produced, and read it from the origin
    // with no hint at all: the only way to find it is the directory.
    let (item, (obj, producer)) = run
        .outputs
        .iter()
        .find(|(_, (_, node))| *node == worker.node.node_id())
        .expect("the worker must have produced at least one object");
    assert_ne!(*producer, origin.node.node_id());

    let bytes = fetch_object(&origin, user, obj.id, vec![])
        .await
        .expect("the directory must be able to find it");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        format!("payload-{item}"),
        "the fetched bytes must be what the item printed"
    );

    // The same read in the other direction, this time with the hint the
    // item's own report carried — the path that costs no lookup.
    let (item, (obj, _)) = run
        .outputs
        .iter()
        .find(|(_, (_, node))| *node == origin.node.node_id())
        .expect("the origin must have produced at least one object");
    let bytes = fetch_object(&worker, user, obj.id, vec![origin.node.node_id()])
        .await
        .expect("a hinted fetch must work");
    assert_eq!(String::from_utf8_lossy(&bytes), format!("payload-{item}"));
}

/// An item that writes to `$MESHWARE_OUT` has *that* stored, not its
/// stdout — which is what lets a real workload produce a file rather than
/// print one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_may_write_its_output_to_a_file() {
    let node = spawn_founder_sized("solo", "objects", capacity(2, 8, 200)).await;
    let user = founder_user_key(&node);
    let run = run_map(
        &node,
        user,
        map_req(
            "echo 'this goes nowhere'; printf 'file-%s' {} > \"$MESHWARE_OUT\"",
            numbers(4),
            Some(1),
        ),
    )
    .await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 4);
    for (item, (obj, _)) in &run.outputs {
        let bytes = fetch_object(&node, user, obj.id, vec![])
            .await
            .expect("fetch");
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            format!("file-{item}"),
            "the file wins over stdout when the item wrote one"
        );
    }
}

/// A failed item produces no object: half-written output is not something
/// a reduce should be handed, and the operator wants the bytes on screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_item_produces_no_object() {
    let node = spawn_founder_sized("solo", "objects", capacity(2, 8, 200)).await;
    let user = founder_user_key(&node);
    let run = run_map(
        &node,
        user,
        map_req("printf 'partial-%s' {} && exit 3", numbers(3), Some(1)),
    )
    .await;
    assert_eq!(run.failed, 3, "every item failed");
    assert!(
        run.outputs.is_empty(),
        "a failure must not leave an object behind"
    );
    assert!(
        objects_on(&node, user).await.is_empty(),
        "and nothing must be sitting in the store either"
    );
}

/// Objects are refcounted to the job that made them, and reclaimed when it
/// is over — the "ephemeral" half of the class (§5.5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn objects_are_reclaimed_when_their_job_is_over() {
    // Zero TTL: "terminal" means reclaimable now, so the sweep is
    // observable without waiting out fifteen minutes.
    let node = mesh_testkit::spawn_founder_with("solo", "objects", vec![], |c| {
        c.limits.slots = 2;
        c.capacity_override = Some(capacity(2, 8, 200));
        c.compute.object_ttl = Duration::ZERO;
    })
    .await;
    let user = founder_user_key(&node);
    let run = run_map(&node, user, map_req("echo {}", numbers(6), Some(2))).await;
    assert_eq!(run.error, None);
    assert_eq!(run.outputs.len(), 6);
    assert!(!objects_on(&node, user).await.is_empty());

    // The job's summary is replicated and terminal, so the sweep can
    // decide this alone — no release message from anyone.
    assert!(
        wait_until(Duration::from_secs(10), || {
            node.node.sweep_objects_for_test();
            node.node.object_counts_for_test().0 == 0
        })
        .await,
        "objects must be reclaimed once their job is terminal"
    );
    assert!(objects_on(&node, user).await.is_empty());
}

/// The reduce is a task placed where its inputs already are — rung 3, in
/// one sentence (§5.5, §5.6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reduce_merges_the_map_outputs_where_most_of_them_live() {
    let origin = spawn_founder_sized("origin", "objects", capacity(2, 8, 200)).await;
    let worker = join_new_node_sized("worker", &origin, capacity(2, 8, 200)).await;
    assert!(converged(&origin, &worker).await, "mesh must converge");
    let user = founder_user_key(&origin);

    let mut req = map_req("echo {}", numbers(20), Some(2));
    if let IpcRequest::Parallel { reduce, .. } = &mut req {
        // Sum every map output. The reduce reads its inputs from the
        // directory it is handed, having fetched them straight from the
        // nodes that produced them.
        *reduce = Some("cat \"$MESHWARE_INPUTS\"/* | awk '{n += $1} END {print n}'".into());
    }
    let run = run_map(&origin, user, req).await;
    assert_eq!(run.error, None);
    assert_eq!(run.succeeded, 20);

    let (node_name, stdout) = run.reduce.expect("the reduce must report a result");
    assert_eq!(
        stdout.trim(),
        (1..=20).sum::<u32>().to_string(),
        "the reduce ran over every map output, from {node_name}"
    );
    assert!(
        node_name == "origin" || node_name == "worker",
        "the reduce ran on a mesh member, not nowhere"
    );
}

/// A reduce is refused on `parallel`, where there is nothing to reduce
/// over: the outputs went inline, not into objects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reduce_without_objects_is_refused() {
    let node = spawn_founder_sized("solo", "objects", capacity(2, 8, 200)).await;
    let user = founder_user_key(&node);
    let mut req = map_req("echo {}", numbers(4), Some(1));
    if let IpcRequest::Parallel {
        capture, reduce, ..
    } = &mut req
    {
        *capture = Capture::Inline;
        *reduce = Some("cat".into());
    }
    let run = run_map(&node, user, req).await;
    assert!(
        run.error.is_some_and(|e| e.contains("map")),
        "a reduce over inline output should say so, and name the fix"
    );
}
