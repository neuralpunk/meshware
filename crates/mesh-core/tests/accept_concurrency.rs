//! ENG-20 / **H-2** — accepting a job no longer holds the scheduler lock
//! across the disk.
//!
//! `JobManager::accept` used to keep the scheduler mutex from the admission
//! decision until after two fsynced file writes and an fsynced op commit.
//! Accepting one job froze the node's whole scheduler until the disk had
//! confirmed several writes, and log followers, status queries, cancels and
//! every other submission queued behind it — on virtualised storage, tens
//! of milliseconds each.
//!
//! The journalling now happens with the lock released, which opens two
//! windows that did not exist before. Both are what this file is about:
//!
//! - **Over-admission.** A second `accept` must not see capacity the first
//!   has already spent but not yet occupied.
//! - **Double acceptance.** Two re-dispatches of the same job must not both
//!   find no journal and both take it.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use mesh_core::error::CoreError;
use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, JobId, JobPayload, JobPriority, JobSpec, SignedIpc};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder_with};

fn envelope(user: &Keypair, command: &str, nonce: u64) -> SignedIpc {
    let req = IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(600),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let payload = postcard::to_stdvec(&req).unwrap();
    let at_micros = mesh_proto::now_micros();
    let signature = user
        .sign(&mesh_proto::ipc_signing_bytes(
            &user.user_id(),
            at_micros,
            nonce,
            &payload,
        ))
        .to_vec();
    SignedIpc {
        user: user.user_id(),
        at_micros,
        nonce,
        payload,
        signature,
    }
}

fn spec_for(node: &TestNode, job_id: JobId, command: &str, nonce: u64) -> JobSpec {
    let user = founder_user_key(node);
    JobSpec {
        job_id,
        payload: JobPayload::Command(command.into()),
        env: vec![],
        timeout_secs: Some(600),
        priority: JobPriority::Normal,
        submitter: node.node.node_id(),
        submitter_user: user.user_id(),
        auth: envelope(user, command, nonce),
        fanout: None,
        created_at_micros: mesh_proto::now_micros(),
    }
}

/// The resource ceiling must hold under concurrency.
///
/// With the journalling outside the lock, a job that has passed admission
/// but is not yet placed is in neither `running` nor `queued` — so the next
/// `accept` would see its cores as free. Each of these twenty submissions
/// asks for every core the node has, so exactly one can be admitted at a
/// time; without the in-flight reservation, all twenty race through the
/// same check and the node promises its hardware twenty times over.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn racing_submissions_cannot_promise_the_same_cores_twice() {
    const CORES: u32 = 4;
    let node = spawn_founder_with("RACE", "eng20-race", vec![], |c| {
        c.limits.slots = 8;
        c.limits.queue = 64;
        // Nothing may be promoted, so an admitted job stays where the test
        // can count it instead of finishing mid-assertion.
        c.limits.min_free_mem_mb = u64::MAX / 2;
        mesh_testkit::apply_capacity_for_test(c, mesh_testkit::capacity(CORES, 16, 100));
    })
    .await;

    let jobs: Arc<mesh_core::jobs::JobManager> = node.node.jobs().clone();
    let whole_machine = vec![(
        mesh_proto::capacity::REQ_CPUS.to_string(),
        CORES.to_string(),
    )];
    let specs: Vec<JobSpec> = (0..20u64)
        .map(|i| {
            let mut spec = spec_for(&node, JobId::generate(), "true", 400 + i);
            spec.env = whole_machine.clone();
            spec
        })
        .collect();

    let mut set = tokio::task::JoinSet::new();
    for spec in specs {
        let jobs = jobs.clone();
        set.spawn_blocking(move || match jobs.accept(spec) {
            Ok(()) => true,
            Err(CoreError::Busy { .. }) => false,
            Err(e) => panic!("unexpected refusal: {e}"),
        });
    }
    let mut accepted = 0usize;
    while let Some(result) = set.join_next().await {
        if result.unwrap() {
            accepted += 1;
        }
    }

    assert_eq!(
        accepted, 1,
        "the node has {CORES} cores and each job asked for all of them; admitting {accepted} promises hardware that does not exist"
    );
    assert_eq!(jobs.queued_count() + jobs.running_count(), 1);

    node.stop().await;
}

/// A job dispatched twice — a spill retry, an adoption re-dispatch — is
/// acknowledged twice and taken once. The stat on `spec.json` is only the
/// fast path now; the test that decides is in memory, under the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_same_job_dispatched_twice_at_once_is_taken_once() {
    let node = spawn_founder_with("TWICE", "eng20-twice", vec![], |c| {
        c.limits.slots = 4;
        c.limits.queue = 16;
        c.limits.min_free_mem_mb = u64::MAX / 2;
    })
    .await;

    let jobs: Arc<mesh_core::jobs::JobManager> = node.node.jobs().clone();
    let job_id = JobId::generate();

    let mut set = tokio::task::JoinSet::new();
    for i in 0..8u64 {
        let jobs = jobs.clone();
        // The same job id, the same command — what a re-dispatch is.
        let spec = spec_for(&node, job_id, "true", 500 + i);
        set.spawn_blocking(move || jobs.accept(spec));
    }
    while let Some(result) = set.join_next().await {
        result
            .unwrap()
            .expect("a re-dispatch is acknowledged, never refused");
    }

    assert_eq!(
        jobs.queued_count() + jobs.running_count(),
        1,
        "a job dispatched eight times must be held once"
    );

    node.stop().await;
}

/// A journal that cannot be written must not leave capacity reserved for a
/// job that will never exist. The jobs directory is made unwritable so the
/// spec write fails after admission has already succeeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_journal_gives_its_capacity_back() {
    let node = spawn_founder_with("FAIL", "eng20-fail", vec![], |c| {
        c.limits.slots = 1;
        c.limits.queue = 1;
        c.limits.min_free_mem_mb = u64::MAX / 2;
    })
    .await;
    let jobs: Arc<mesh_core::jobs::JobManager> = node.node.jobs().clone();

    let spool = node.config().data_dir.join("jobs");
    std::fs::create_dir_all(&spool).unwrap();
    let restore = std::fs::metadata(&spool).unwrap().permissions();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    let failed = jobs.accept(spec_for(&node, JobId::generate(), "true", 601));
    std::fs::set_permissions(&spool, restore).unwrap();
    assert!(failed.is_err(), "the journal write should have failed");

    // The capacity the failed acceptance held must be available again:
    // with slots=1 and queue=1 a leaked reservation would show up as the
    // next two submissions being refused.
    for i in 0..2u64 {
        jobs.accept(spec_for(&node, JobId::generate(), "true", 610 + i))
            .expect("capacity was not given back after a failed journal");
    }
    assert_eq!(jobs.queued_count() + jobs.running_count(), 2);

    node.stop().await;
}

/// The point of the change: the scheduler lock is available while a job is
/// being journalled. Measured through a lock-only operation — `queued_count`
/// takes the same mutex — while a batch of accepts is in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_scheduler_answers_while_a_job_is_being_journalled() {
    let node = spawn_founder_with("LOCK", "eng20-lock", vec![], |c| {
        c.limits.slots = 64;
        c.limits.queue = 256;
        c.limits.min_free_mem_mb = u64::MAX / 2;
    })
    .await;
    let jobs: Arc<mesh_core::jobs::JobManager> = node.node.jobs().clone();

    let mut set = tokio::task::JoinSet::new();
    for i in 0..40u64 {
        let jobs = jobs.clone();
        let spec = spec_for(&node, JobId::generate(), "true", 700 + i);
        set.spawn_blocking(move || {
            let _ = jobs.accept(spec);
        });
    }

    // While those are writing, the scheduler must still answer. Each of
    // these takes the same mutex `accept` used to hold across three fsyncs.
    let began = std::time::Instant::now();
    let mut answers = 0u32;
    while began.elapsed() < Duration::from_millis(500) {
        let _ = jobs.queued_count();
        answers += 1;
        tokio::task::yield_now().await;
    }
    while set.join_next().await.is_some() {}

    assert!(
        answers > 100,
        "the scheduler answered only {answers} times in half a second while \
         jobs were being journalled"
    );

    node.stop().await;
}
