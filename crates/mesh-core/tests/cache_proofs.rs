//! **A cache replica indexes only work the publisher ran** (SEC-18).
//!
//! Any member could publish any entry: nothing checked that it ran the job
//! the entry cited, or that the job derived the key, so any member could
//! plant outputs that other people's cached runs would then reuse. These
//! tests are that member, talking to a replica directly.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    ActionKey, ActionSpec, CacheEntry, CacheProof, Capture, IpcRequest, IpcResponse, JobId,
    JobPriority, ParallelPlan, Request, Response,
};
use mesh_testkit::{
    TestNode, founder_user_key, ipc_as, join_new_node, run_to_end, spawn_founder, wait_until,
};

async fn ask(from: &TestNode, to: &TestNode, req: Request) -> Response {
    let id = to.node.node_id();
    let addrs = from
        .node
        .engine()
        .read(|s| s.members[&id].info.addrs.clone());
    let conn = from.node.transport().member_conn(id, &addrs).await.unwrap();
    from.node
        .transport()
        .request_one(&conn, &req)
        .await
        .unwrap()
}

async fn alive(a: &TestNode, b: &TestNode) {
    let (ia, ib) = (a.node.node_id(), b.node.node_id());
    assert!(
        wait_until(Duration::from_secs(30), || {
            a.node.engine().read(|s| s.is_active_member(&ib))
                && b.node.engine().read(|s| s.is_active_member(&ia))
                && matches!(
                    a.node.gossip().liveness_of(&ib),
                    mesh_proto::Liveness::Alive
                )
        })
        .await
    );
}

async fn epoch(node: &TestNode) -> u64 {
    match ipc_as(node, founder_user_key(node), IpcRequest::CacheStats).await {
        IpcResponse::CacheStats(stats) => stats.epoch,
        other => panic!("CacheStats: {other:?}"),
    }
}

async fn entries(node: &TestNode, admin: &TestNode) -> u64 {
    match ipc_as(node, founder_user_key(admin), IpcRequest::CacheStats).await {
        IpcResponse::CacheStats(stats) => stats.entries,
        other => panic!("CacheStats: {other:?}"),
    }
}

fn entry(key: ActionKey, job: JobId, epoch: u64) -> CacheEntry {
    CacheEntry {
        key,
        exit_code: Some(0),
        stdout: None,
        stdout_len: 0,
        stderr: None,
        stderr_len: 0,
        outputs: vec![],
        producer: Keypair::generate().unwrap().node_id(),
        job,
        epoch,
        hlc: Default::default(),
        at_micros: mesh_proto::now_micros(),
    }
}

fn publish(entry: CacheEntry, proof: CacheProof) -> Request {
    Request::CachePublishV2 {
        entry: Box::new(entry),
        proof: Box::new(proof),
    }
}

fn refused(resp: Response, why: &str) {
    match resp {
        Response::Error { message } => assert!(message.contains(why), "{message}"),
        other => panic!("expected a refusal naming {why:?}, got {other:?}"),
    }
}

/// Run a plain command on `on`, as the founder's user, and return its id.
async fn ran_on(founder: &TestNode, on: &TestNode) -> JobId {
    let outcome = run_to_end(
        founder,
        founder_user_key(founder),
        IpcRequest::Run {
            command: "echo hello".into(),
            on: Some(format!("name={}", on.config().node_name)),
            env: vec![],
            timeout_secs: Some(60),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    let mesh_testkit::RunOutcome::Done {
        job_id: Some(job), ..
    } = outcome
    else {
        panic!("run: {outcome:?}");
    };
    assert!(
        wait_until(Duration::from_secs(20), || {
            founder.node.engine().read(|s| {
                s.jobs
                    .get(&job)
                    .is_some_and(|j| j.state == mesh_proto::JobState::Succeeded)
            })
        })
        .await
    );
    job
}

#[tokio::test]
async fn a_member_cannot_index_a_job_it_did_not_run_or_a_key_it_did_not_derive() {
    let founder = spawn_founder("NIM", "cache-proof-jobs").await;
    let mallory = join_new_node("mallory", &founder, vec![]).await;
    alive(&founder, &mallory).await;
    let epoch = epoch(&founder).await;
    let key = ActionKey([7u8; 32]);

    // The request every older node sends is refused outright.
    let bare = Request::CachePublish {
        entry: Box::new(entry(key, JobId::generate(), epoch)),
    };
    refused(ask(&mallory, &founder, bare).await, "CachePublishV2");

    // Somebody else's job.
    let theirs = ran_on(&founder, &founder).await;
    let spec = founder.node.jobs().spec_of(&theirs).unwrap();
    refused(
        ask(
            &mallory,
            &founder,
            publish(entry(key, theirs, epoch), CacheProof::Job(Box::new(spec))),
        )
        .await,
        "did not run job",
    );

    // Its own job, but one that derives no key — even with the key written
    // into its environment, where the origin would put it.
    let mine = ran_on(&founder, &mallory).await;
    let mut spec = mallory.node.jobs().spec_of(&mine).unwrap();
    let cited = publish(
        entry(key, mine, epoch),
        CacheProof::Job(Box::new(spec.clone())),
    );
    refused(ask(&mallory, &founder, cited).await, "does not derive");
    spec.env
        .push((mesh_proto::CACHE_KEY.into(), key.to_string()));
    let cited = publish(
        entry(key, mine, epoch),
        CacheProof::Job(Box::new(spec.clone())),
    );
    refused(ask(&mallory, &founder, cited).await, "does not derive");

    // The wrong job id for the spec, and the wrong epoch.
    refused(
        ask(
            &mallory,
            &founder,
            publish(
                entry(key, JobId::generate(), epoch),
                CacheProof::Job(Box::new(spec.clone())),
            ),
        )
        .await,
        "different job",
    );
    refused(
        ask(
            &mallory,
            &founder,
            publish(entry(key, mine, epoch + 1), CacheProof::Job(Box::new(spec))),
        )
        .await,
        "epoch",
    );
    assert_eq!(entries(&founder, &founder).await, 0);

    mallory.stop().await;
    founder.stop().await;
}

/// The key a worker derives for `item` of a plain command `map`.
fn item_key(plan: &ParallelPlan, item: &str, epoch: u64) -> ActionKey {
    let mut env: Vec<(String, String)> = plan.env.clone();
    env.push((mesh_proto::parallel::ITEM_ENV.into(), item.into()));
    let mut action = ActionSpec {
        runtime: "bash".into(),
        entrypoint: "(map item)".into(),
        argv: vec![plan.command_for(item)],
        env,
        bundle_root: None,
        inputs: vec![],
        source_digest: None,
        outputs: vec![],
        sandbox_class: mesh_core::cache::SANDBOX_PROCESS.into(),
        arch: mesh_core::cache::arch(),
        epoch,
    };
    action.canonicalise();
    action.key()
}

#[tokio::test]
async fn a_map_item_is_indexed_only_for_a_worker_of_a_signed_plan() {
    let founder = spawn_founder("NIM", "cache-proof-items").await;
    let mallory = join_new_node("mallory", &founder, vec![]).await;
    alive(&founder, &mallory).await;
    let epoch = epoch(&founder).await;
    let admin = founder_user_key(&founder);

    let env = vec![(mesh_proto::REQ_CACHE.to_string(), "1".to_string())];
    let items = vec!["a".to_string(), "b".to_string()];
    let request = IpcRequest::Parallel {
        command: "echo {}".into(),
        service: None,
        inputs: vec![],
        item_input: None,
        items: items.clone(),
        on: None,
        env: env.clone(),
        timeout_secs: Some(60),
        chunk: Some(1),
        capture: Capture::Inline,
        reduce: None,
        reduce_on: None,
    };
    let job = JobId::generate();
    let plan = ParallelPlan {
        job,
        command: "echo {}".into(),
        exec: None,
        item_input: None,
        items,
        env,
        timeout_secs: Some(60),
        chunk: Some(1),
        capture: Capture::Inline,
        inputs: vec![],
        workers: vec![founder.node.node_id(), mallory.node.node_id()],
        submitter_user: admin.user_id(),
        auth: admin.sign_request(&request).unwrap(),
    };
    let good = item_key(&plan, "a", epoch);
    let item = |name: &str, plan: Option<&ParallelPlan>| CacheProof::Item {
        item: name.into(),
        plan: plan.map(|p| Box::new(p.clone())),
    };

    // The plan is asked for once, then remembered.
    match ask(
        &mallory,
        &founder,
        publish(entry(good, job, epoch), item("a", None)),
    )
    .await
    {
        Response::CachePlanNeeded => {}
        other => panic!("expected CachePlanNeeded: {other:?}"),
    }

    // Not a worker; a forged signature; an item the plan does not have; a
    // key the item does not derive.
    let mut outsider = plan.clone();
    outsider.workers = vec![founder.node.node_id()];
    refused(
        ask(
            &mallory,
            &founder,
            publish(entry(good, job, epoch), item("a", Some(&outsider))),
        )
        .await,
        "not a worker",
    );
    let mut forged = plan.clone();
    forged.command = "echo planted {}".into();
    refused(
        ask(
            &mallory,
            &founder,
            publish(entry(good, job, epoch), item("a", Some(&forged))),
        )
        .await,
        "does not match",
    );
    refused(
        ask(
            &mallory,
            &founder,
            publish(entry(good, job, epoch), item("zzz", Some(&plan))),
        )
        .await,
        "not one of the job's",
    );
    let wrong = item_key(&plan, "b", epoch);
    refused(
        ask(
            &mallory,
            &founder,
            publish(entry(wrong, job, epoch), item("a", None)),
        )
        .await,
        "does not derive",
    );
    assert_eq!(entries(&founder, &founder).await, 0);

    // The real thing, without the plan now that the replica has it.
    match ask(
        &mallory,
        &founder,
        publish(entry(good, job, epoch), item("a", None)),
    )
    .await
    {
        Response::Ok => {}
        other => panic!("a genuine item entry was refused: {other:?}"),
    }
    assert_eq!(entries(&founder, &founder).await, 1);

    mallory.stop().await;
    founder.stop().await;
}

#[tokio::test]
async fn real_entries_reach_the_replica_that_did_not_compute_them() {
    let founder = spawn_founder("NIM", "cache-proof-real").await;
    let second = join_new_node("second", &founder, vec![]).await;
    let (f, b) = (founder.node.node_id(), second.node.node_id());
    assert!(
        wait_until(Duration::from_secs(30), || {
            founder.node.engine().read(|s| s.is_active_member(&b))
                && second.node.engine().read(|s| s.is_active_member(&f))
                && matches!(
                    founder.node.gossip().liveness_of(&b),
                    mesh_proto::Liveness::Alive
                )
                && matches!(
                    second.node.gossip().liveness_of(&f),
                    mesh_proto::Liveness::Alive
                )
        })
        .await
    );
    // Two members and two replicas: every entry belongs on both, and each
    // node computed only some of them.
    let map = IpcRequest::Parallel {
        command: "echo proof-{}".into(),
        service: None,
        inputs: vec![],
        item_input: None,
        items: (0..6).map(|i| i.to_string()).collect(),
        on: None,
        env: vec![(mesh_proto::REQ_CACHE.to_string(), "1".into())],
        timeout_secs: Some(60),
        chunk: Some(1),
        capture: Capture::Inline,
        reduce: None,
        reduce_on: None,
    };
    let mut rx = mesh_testkit::stream_as(&founder, founder_user_key(&founder), map).await;
    while let Some(frame) = rx.recv().await {
        if matches!(
            frame,
            IpcResponse::ParallelDone { .. } | IpcResponse::Error { .. }
        ) {
            break;
        }
    }
    let mut both = (0, 0);
    let indexed = {
        let mut ok = false;
        for _ in 0..40 {
            both = (
                entries(&founder, &founder).await,
                entries(&second, &founder).await,
            );
            if both == (6, 6) {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        ok
    };
    assert!(indexed, "entries per replica: {both:?}");

    second.stop().await;
    founder.stop().await;
}
