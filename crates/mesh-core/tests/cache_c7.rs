//! C7 — the action cache (C0–C9 §11).
//!
//! §14.4 names one gate for this milestone and it is unusual in being about
//! a *pure function* rather than about a race: **key stability across
//! irrelevant changes and key change across relevant ones**, because this
//! is the milestone where a subtle bug is invisible. A key that misses a
//! field hands back last week's answer for work that changed, and the
//! symptom is not an error — it is a haunting.
//!
//! That table lives in `mesh_proto::cache`, where the derivation does. What
//! lives here is everything the table cannot prove: that a real submission
//! through a real mesh misses once, publishes, and hits the second time;
//! that a hit is *visible*; that changing the work changes the answer; and
//! that the refusals which keep the cache honest — ad-hoc bash, fan-out —
//! actually refuse.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{Capture, IpcRequest, IpcResponse, JobPriority};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, spawn_founder, stream_as, wait_until,
};

/// A `map` that asks to be cached. `map` rather than `run --cmd`, because
/// §11.2 draws exactly that line: an ad-hoc command is not hermetic and
/// must never be assumed to be, while a `map` item is a declared,
/// data-parallel computation whose author is claiming per-item purity.
fn cached_map(command: &str, items: Vec<String>) -> IpcRequest {
    IpcRequest::Parallel {
        command: command.into(),
        service: None,
        inputs: vec![],
        item_input: None,
        items,
        on: None,
        env: vec![(mesh_proto::REQ_CACHE.to_string(), "1".into())],
        timeout_secs: Some(60),
        chunk: Some(1),
        capture: Capture::Inline,
        reduce: None,
        reduce_on: None,
    }
}

fn plain_run(command: &str, env: Vec<(String, String)>, fanout: Option<u32>) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env,
        timeout_secs: Some(60),
        fanout,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

/// Drain a parallel submission, returning each item's stdout.
async fn map_to_end(node: &TestNode, as_user: &TestNode, req: IpcRequest) -> Vec<(String, String)> {
    let mut rx = stream_as(node, founder_user_key(as_user), req).await;
    let mut items = Vec::new();
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::ParallelItem { item, stdout, .. } => items.push((item, stdout)),
            IpcResponse::ParallelDone { .. } | IpcResponse::Error { .. } => break,
            _ => {}
        }
    }
    items
}

async fn stats(node: &TestNode, as_user: &TestNode) -> mesh_proto::CacheStats {
    match ipc_as(node, founder_user_key(as_user), IpcRequest::CacheStats).await {
        IpcResponse::CacheStats(stats) => *stats,
        other => panic!("unexpected response to CacheStats: {other:?}"),
    }
}

/// The demo P2 ends on, at the smallest scale that can prove it: run the
/// same work twice and watch the second one not happen.
#[tokio::test]
async fn the_second_run_of_unchanged_work_does_not_run() {
    let founder = spawn_founder("cache-a", "mesh-cache").await;

    let first = map_to_end(
        &founder,
        &founder,
        cached_map("echo item-{}", vec!["1".into(), "2".into()]),
    )
    .await;
    assert_eq!(first.len(), 2, "both items ran the first time");
    assert!(
        first
            .iter()
            .all(|(item, out)| out.contains(&format!("item-{item}")))
    );

    // Publication is off the job's own path — §11.2's rule that a miss
    // costs nothing applies to the publish side too — so the entry lands
    // on the history task's tick rather than before the stream ends.
    let published = wait_until(Duration::from_secs(20), || {
        founder.node.engine().read(|_| true) && !founder.node.action_cache_is_empty()
    })
    .await;
    assert!(published, "a finished cacheable item must publish an entry");

    let before = stats(&founder, &founder).await;
    let second = map_to_end(
        &founder,
        &founder,
        cached_map("echo item-{}", vec!["1".into(), "2".into()]),
    )
    .await;
    let after = stats(&founder, &founder).await;

    assert_eq!(
        second.len(),
        2,
        "a cached run must produce every item a real one does"
    );
    for (item, out) in &second {
        assert!(
            out.contains(&format!("item-{item}")),
            "a hit must return the bytes the original run produced, got {out:?}"
        );
    }
    assert!(
        after.hits > before.hits,
        "the second run of unchanged work must hit: {before:?} -> {after:?}"
    );
    assert!(
        after.bytes_saved > before.bytes_saved,
        "a hit has to account for the bytes it saved, or `cache stats` is decoration"
    );
}

/// The other half of the same property, and the one that keeps the cache
/// honest: change the work and the answer must change with it.
#[tokio::test]
async fn changing_the_work_changes_the_answer() {
    let founder = spawn_founder("cache-b", "mesh-cache").await;

    let first = map_to_end(
        &founder,
        &founder,
        cached_map("echo v1-{}", vec!["a".into()]),
    )
    .await;
    assert!(first[0].1.contains("v1-a"));
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;

    // One character different in the command is a different action.
    let second = map_to_end(
        &founder,
        &founder,
        cached_map("echo v2-{}", vec!["a".into()]),
    )
    .await;
    assert!(
        second[0].1.contains("v2-a"),
        "a changed command must not be served the old command's output: {:?}",
        second[0].1
    );
    // …and so is a different item, through the same mechanism: the item is
    // substituted into the command before the key is derived.
    let third = map_to_end(
        &founder,
        &founder,
        cached_map("echo v1-{}", vec!["b".into()]),
    )
    .await;
    assert!(third[0].1.contains("v1-b"));
}

/// §11.2: *a hit is visible or it is untrustworthy*. An invisible cache is
/// a bug report factory, because "it didn't run" and "it broke" look
/// identical from outside.
#[tokio::test]
async fn a_hit_is_counted_and_a_miss_is_counted() {
    let founder = spawn_founder("cache-c", "mesh-cache").await;

    let cold = stats(&founder, &founder).await;
    assert_eq!(
        (cold.hits, cold.misses),
        (0, 0),
        "a cold node has done nothing"
    );
    assert!(cold.enabled);

    map_to_end(
        &founder,
        &founder,
        cached_map("echo once-{}", vec!["x".into()]),
    )
    .await;
    let after_miss = stats(&founder, &founder).await;
    assert_eq!(after_miss.misses, 1, "the first run of an action is a miss");
    assert_eq!(after_miss.hits, 0);

    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;
    map_to_end(
        &founder,
        &founder,
        cached_map("echo once-{}", vec!["x".into()]),
    )
    .await;

    let after_hit = stats(&founder, &founder).await;
    assert_eq!(after_hit.hits, 1, "the second run is a hit");
    assert!(
        after_hit
            .by_service
            .iter()
            .any(|r| r.service == "(map)" && r.hits == 1),
        "a hit has to be attributable to something: {:?}",
        after_hit.by_service
    );
}

/// An entry published on one node must be usable from another. Otherwise
/// the cache scales with a *machine* rather than with use, and the one
/// claim the milestone makes — the mesh gets faster as it gets bigger —
/// is false.
#[tokio::test]
async fn an_entry_published_anywhere_is_usable_everywhere() {
    let founder = spawn_founder("cache-d1", "mesh-cache").await;
    let second = join_new_node("cache-d2", &founder, vec![]).await;
    let joined = wait_until(Duration::from_secs(30), || {
        founder.node.engine().read(|s| s.members.len() == 2)
            && second.node.engine().read(|s| s.members.len() == 2)
    })
    .await;
    assert!(
        joined,
        "both nodes must see the mesh before this means anything"
    );

    map_to_end(
        &founder,
        &founder,
        cached_map("echo shared-{}", vec!["1".into()]),
    )
    .await;
    let published = wait_until(Duration::from_secs(30), || {
        !founder.node.action_cache_is_empty() || !second.node.action_cache_is_empty()
    })
    .await;
    assert!(
        published,
        "the entry must reach one of the two directory replicas"
    );

    // Submitted from the *other* node: the directory is a rendezvous
    // ranking, so whichever node asks computes the same two replicas and
    // finds the same entry.
    let before = stats(&second, &founder).await;
    map_to_end(
        &second,
        &founder,
        cached_map("echo shared-{}", vec!["1".into()]),
    )
    .await;
    let after = stats(&second, &founder).await;
    assert!(
        after.hits > before.hits,
        "a second node must find the first node's entry: {before:?} -> {after:?}"
    );
}

/// Publish a one-file bash service, optionally tagged `cacheable`.
fn publish_service(node: &TestNode, name: &str, script: &str, tags: Vec<String>) {
    let bundle = tempfile::tempdir().expect("tempdir");
    std::fs::write(bundle.path().join("run.sh"), script).expect("write");
    let engine = node.node.engine();
    let content =
        mesh_core::content::ContentStore::open(node.config().data_dir.as_path()).expect("content");
    let push = mesh_core::services::push_dir(engine, &content, bundle.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(node),
        name.to_string(),
        mesh_proto::Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        String::new(),
        tags,
    )
    .expect("create service");
}

async fn run_service(node: &TestNode, req: IpcRequest) -> (String, bool) {
    let mut rx = stream_as(node, founder_user_key(node), req).await;
    let mut stdout = String::new();
    let mut cached = false;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::CacheHit(_) => cached = true,
            IpcResponse::LogChunk { stream: 1, data } => {
                stdout.push_str(&String::from_utf8_lossy(&data));
            }
            IpcResponse::JobDone { .. } => break,
            IpcResponse::Error { message } => panic!("service run failed: {message}"),
            _ => {}
        }
    }
    (stdout, cached)
}

/// **A declared output file must come back**, and it must come back to the
/// directory the process would have written it in.
///
/// Here because it was wrong first: the publisher globbed `work/`, which is
/// where a *command* runs — a service runs in its bundle's checkout under
/// its `working_dir`, so `--cache-outputs` on a service found nothing and
/// cached an empty output list. Silently, which is the worst way for this
/// to be wrong: the hit still succeeded, still printed the right stdout,
/// and left the artefact missing.
#[tokio::test]
async fn a_cached_service_brings_its_declared_files_with_it() {
    let founder = spawn_founder("cache-k", "mesh-cache").await;
    // Writes a file whose contents differ on every real execution, so a
    // second run that produces the *same* bytes can only have been served.
    publish_service(
        &founder,
        "build",
        "#!/bin/sh\nmkdir -p out\necho \"artifact $$-$(date +%s%N)\" > out/result.txt\necho built\n",
        vec![],
    );
    let run = || IpcRequest::RunService {
        name: "build".into(),
        on: None,
        inputs: vec![
            (mesh_proto::REQ_CACHE.to_string(), "1".to_string()),
            (
                mesh_proto::REQ_CACHE_OUTPUTS.to_string(),
                "out/*.txt".to_string(),
            ),
        ],
        inventory: None,
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };

    let (first, cached) = run_service(&founder, run()).await;
    assert!(first.contains("built"));
    assert!(!cached, "the first run of an action cannot be a hit");
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;

    let (second, cached) = run_service(&founder, run()).await;
    assert!(cached, "the second run must be served from the cache");
    assert!(second.contains("built"));

    // Both job directories must hold the artefact, and it must be the
    // *same* artefact — a regenerated one would carry a different pid and
    // timestamp.
    let jobs = founder.config().data_dir.join("jobs");
    let mut found: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&jobs).expect("jobs dir").flatten() {
        let path = entry.path().join("checkout").join("out").join("result.txt");
        if let Ok(text) = std::fs::read_to_string(&path) {
            found.push(text);
        }
    }
    assert_eq!(
        found.len(),
        2,
        "a cached run must leave the same files behind a real one would"
    );
    assert_eq!(
        found[0], found[1],
        "the file must be materialised from the cache, not regenerated"
    );
}

/// A service version tagged `cacheable` opts every run of it in, with no
/// flag at the call site — which is the point: hermeticity is a property of
/// the code, claimed by its author.
#[tokio::test]
async fn the_cacheable_tag_opts_a_service_in() {
    let founder = spawn_founder("cache-l", "mesh-cache").await;
    publish_service(
        &founder,
        "pure",
        "#!/bin/sh\necho deterministic\n",
        vec![mesh_proto::CACHEABLE_TAG.to_string()],
    );
    let run = || IpcRequest::RunService {
        name: "pure".into(),
        on: None,
        // No `--cacheable` anywhere: the tag is the whole opt-in.
        inputs: vec![],
        inventory: None,
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let (_, cached) = run_service(&founder, run()).await;
    assert!(!cached);
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;
    let (out, cached) = run_service(&founder, run()).await;
    assert!(cached, "a `cacheable` service hits without anyone asking");
    assert!(out.contains("deterministic"));
}

/// `map --service X --cacheable`: both halves of §11.2's "per service or
/// per `map`" at once, and the case that would most easily have been left
/// as a silent no-op — a `--cacheable` that quietly did nothing for exactly
/// the payload class the milestone says is *most* cacheable.
#[tokio::test]
async fn a_cacheable_map_over_a_service_caches_per_item() {
    let founder = spawn_founder("cache-m", "mesh-cache").await;
    publish_service(&founder, "shout", "#!/bin/sh\necho \"ran $*\"\n", vec![]);

    let map_service = || IpcRequest::Parallel {
        command: String::new(),
        service: Some("shout".into()),
        inputs: vec![],
        item_input: Some("word".into()),
        items: vec!["alpha".into(), "beta".into()],
        on: None,
        env: vec![(mesh_proto::REQ_CACHE.to_string(), "1".into())],
        timeout_secs: Some(60),
        chunk: Some(1),
        capture: Capture::Inline,
        reduce: None,
        reduce_on: None,
    };

    let first = map_to_end(&founder, &founder, map_service()).await;
    assert_eq!(first.len(), 2);
    assert!(
        first
            .iter()
            .any(|(item, out)| item == "alpha" && out.contains("alpha")),
        "each item must reach the script as its own input: {first:?}"
    );
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;

    let before = stats(&founder, &founder).await;
    let second = map_to_end(&founder, &founder, map_service()).await;
    let after = stats(&founder, &founder).await;
    assert!(
        after.hits >= before.hits + 2,
        "both items must hit the second time: {before:?} -> {after:?}"
    );
    // Two items are two actions, and each gets back its own answer — a key
    // that collided across items would return one item's output for both.
    for (item, out) in &second {
        assert!(
            out.contains(item),
            "item {item} was served {out:?}, which is another item's answer"
        );
    }
}

/// The refusals, which are as load-bearing as the feature. A `--cacheable`
/// that quietly did nothing would be worse than one that says why.
#[tokio::test]
async fn the_things_that_cannot_be_cached_say_so() {
    let founder = spawn_founder("cache-e", "mesh-cache").await;
    let user = founder_user_key(&founder);
    let cacheable = vec![(mesh_proto::REQ_CACHE.to_string(), "1".to_string())];

    // Ad-hoc bash: not hermetic, cannot be declared so.
    let mut rx = stream_as(&founder, user, plain_run("date", cacheable.clone(), None)).await;
    let mut refusal = None;
    while let Some(frame) = rx.recv().await {
        if let IpcResponse::Error { message } = frame {
            refusal = Some(message);
            break;
        }
    }
    let message = refusal.expect("an ad-hoc cacheable command must be refused, not quietly run");
    assert!(
        message.contains("ad-hoc") && message.contains("service"),
        "the refusal has to say why and what to do instead: {message}"
    );

    // A fan-out: N targets are N different actions, because each one ran on
    // and observed a different machine.
    let mut rx = stream_as(&founder, user, plain_run("date", cacheable, Some(0))).await;
    let mut refusal = None;
    while let Some(frame) = rx.recv().await {
        if let IpcResponse::Error { message } = frame {
            refusal = Some(message);
            break;
        }
    }
    let message = refusal.expect("a cacheable fan-out must be refused");
    assert!(
        message.contains("fan-out") && message.contains("map"),
        "the refusal has to point at the thing that does work per item: {message}"
    );
}

/// The epoch: one integer, bumped, and every entry in the mesh stops
/// matching — no migration, no purge sweep, no window in which half the
/// mesh believes different things.
#[tokio::test]
async fn bumping_the_epoch_invalidates_everything() {
    let founder = mesh_testkit::spawn_founder_with("cache-f", "mesh-cache", vec![], |c| {
        c.cache.epoch = 0;
    })
    .await;
    map_to_end(
        &founder,
        &founder,
        cached_map("echo epoch-{}", vec!["1".into()]),
    )
    .await;
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;
    assert_eq!(stats(&founder, &founder).await.epoch, 0);

    // A node on a later epoch does not see the earlier one's work — not as
    // an error, and not as a stale hit. It simply misses.
    let bumped = mesh_testkit::spawn_founder_with("cache-g", "mesh-cache", vec![], |c| {
        c.cache.epoch = 1;
    })
    .await;
    map_to_end(
        &bumped,
        &bumped,
        cached_map("echo epoch-{}", vec!["1".into()]),
    )
    .await;
    let after = stats(&bumped, &bumped).await;
    assert_eq!(after.epoch, 1);
    assert_eq!(
        after.hits, 0,
        "an entry from a superseded epoch is not a hit"
    );
}

/// A `map` item's answer is an **object**, not text, and a hit has to come
/// back as one — otherwise `--out` on a fully cached run writes nothing and
/// the second run of a pipeline silently loses its data.
///
/// This is here because it was wrong first: publication ran before
/// `capture_objects`, so the entry carried the item's stdout and a hit
/// returned no output at all.
#[tokio::test]
async fn a_cached_map_item_comes_back_as_an_object() {
    let founder = spawn_founder("cache-i", "mesh-cache").await;
    let user = founder_user_key(&founder);

    let object_map = |command: &str| IpcRequest::Parallel {
        command: command.into(),
        service: None,
        inputs: vec![],
        item_input: None,
        items: vec!["1".into(), "2".into()],
        on: None,
        env: vec![(mesh_proto::REQ_CACHE.to_string(), "1".into())],
        timeout_secs: Some(60),
        chunk: Some(1),
        // `Capture::Object` is what `meshnode map` sends, and the whole
        // difference between `map` and `parallel`.
        capture: Capture::Object,
        reduce: None,
        reduce_on: None,
    };

    let mut first: Vec<mesh_proto::ObjectRef> = Vec::new();
    let mut rx = stream_as(&founder, user, object_map("echo obj-{}")).await;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::ParallelItem { output, .. } => first.extend(output),
            IpcResponse::ParallelDone { .. } | IpcResponse::Error { .. } => break,
            _ => {}
        }
    }
    assert_eq!(first.len(), 2, "every item of a map produces an object");
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;

    let before = stats(&founder, &founder).await;
    let mut second: Vec<mesh_proto::ObjectRef> = Vec::new();
    let mut rx = stream_as(&founder, user, object_map("echo obj-{}")).await;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::ParallelItem { output, .. } => second.extend(output),
            IpcResponse::ParallelDone { .. } | IpcResponse::Error { .. } => break,
            _ => {}
        }
    }
    let after = stats(&founder, &founder).await;
    assert!(after.hits > before.hits, "the second run must hit");
    assert_eq!(
        second.len(),
        2,
        "a cached item must still hand back an object, or `--out` writes nothing"
    );
    let mut a: Vec<_> = first.iter().map(|o| o.id).collect();
    let mut b: Vec<_> = second.iter().map(|o| o.id).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b, "the same work must name the same objects");
}

/// The key must not carry facts about *this dispatch*.
///
/// Also here because it was wrong first: `MESHWARE_OUT` names a scratch
/// path under the job id, so a `map --cacheable` run twice published two
/// entries and hit neither — a silent performance bug that looked exactly
/// like a working cache.
#[tokio::test]
async fn a_second_dispatch_of_identical_work_derives_the_same_key() {
    let founder = spawn_founder("cache-j", "mesh-cache").await;
    map_to_end(
        &founder,
        &founder,
        cached_map("echo stable-{}", vec!["1".into()]),
    )
    .await;
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;
    let after_first = stats(&founder, &founder).await;
    assert_eq!(after_first.entries, 1);

    map_to_end(
        &founder,
        &founder,
        cached_map("echo stable-{}", vec!["1".into()]),
    )
    .await;
    let after_second = stats(&founder, &founder).await;
    assert_eq!(
        after_second.entries, 1,
        "a second dispatch of identical work must reuse the key, not mint a new one"
    );
    assert_eq!(after_second.hits, 1);
}

/// `cache purge` makes the mesh redo work it has already done, which is
/// exactly what somebody who suspects the cache wants.
#[tokio::test]
async fn a_purge_makes_the_next_run_run() {
    let founder = spawn_founder("cache-h", "mesh-cache").await;
    let user = founder_user_key(&founder);

    map_to_end(
        &founder,
        &founder,
        cached_map("echo purge-{}", vec!["1".into()]),
    )
    .await;
    wait_until(Duration::from_secs(20), || {
        !founder.node.action_cache_is_empty()
    })
    .await;
    assert!(stats(&founder, &founder).await.entries > 0);

    let resp = ipc_as(
        &founder,
        user,
        IpcRequest::CachePurge {
            scope: mesh_proto::PurgeScope::All,
        },
    )
    .await;
    assert!(
        matches!(resp, IpcResponse::Notice { .. }),
        "a purge reports what it did: {resp:?}"
    );
    assert_eq!(stats(&founder, &founder).await.entries, 0);

    let before = stats(&founder, &founder).await;
    map_to_end(
        &founder,
        &founder,
        cached_map("echo purge-{}", vec!["1".into()]),
    )
    .await;
    assert_eq!(
        stats(&founder, &founder).await.hits,
        before.hits,
        "after a purge the work is done again rather than served"
    );
}
