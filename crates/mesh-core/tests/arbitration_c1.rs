//! C1 — arbitration: accounting, fair share, preemption (C0–C9 §5).
//!
//! The mesh answered *who may* with real rigour and had no answer at all to
//! *whose turn*. These tests pin the three things that change that, in the
//! order §5.2 locks them: accounting first, because it is the input the
//! other two read. (C1 P3, quotas, was removed; so was the usage report
//! that read the same counters.)
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use mesh_core::accounting::{Charge, charge_for};
use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::StateEngine;
use mesh_core::usage::{Totals, UsageMeter, dominant_share_permille, weighted_shares};
use mesh_proto::{
    Grant, GrantId, IpcRequest, IpcResponse, JobPriority, MemberInfo, OpBody, Requirements, Role,
    UserId, UserRecord,
};
use mesh_testkit::{RunOutcome, TestNode, founder_user_key, ipc_as, run_to_end, spawn_founder};

fn engine_with_member() -> (tempfile::TempDir, Arc<StateEngine>, Keypair) {
    let dir = tempfile::tempdir().unwrap();
    let user = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("c1-mesh", &user, node.node_id()).unwrap();
    let e = StateEngine::open(dir.path(), node.clone(), genesis).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: MemberInfo {
            node_id: node.node_id(),
            name: "alpha".into(),
            addrs: vec![],
            tags: vec![],
            slots: 8,
            joined_at_micros: mesh_proto::now_micros(),
        },
        invite: None,
    })
    .unwrap();
    (dir, e, node)
}

// ---------------------------------------------------------------------------
// P1 — accounting
// ---------------------------------------------------------------------------

#[test]
fn a_share_is_dominant_not_averaged() {
    // The property DRF is named for, and the reason it fits a mesh that has
    // no exchange rate between a core and a gigabyte. A principal on 20% of
    // the CPU and 60% of the memory holds 60% of the cluster, because the
    // memory is what everybody else cannot have.
    let mine = Totals {
        cpu_secs: 20,
        mem_mb_secs: 60,
        ..Default::default()
    };
    let whole = Totals {
        cpu_secs: 100,
        mem_mb_secs: 100,
        ..Default::default()
    };
    assert_eq!(dominant_share_permille(&mine, &whole), 600);

    // An unused resource never drags a share down — a max, not a mean.
    let whole = Totals {
        cpu_secs: 100,
        mem_mb_secs: 100,
        gpu_secs: 1_000,
        ..Default::default()
    };
    assert_eq!(dominant_share_permille(&mine, &whole), 600);

    // Nobody using anything is nobody's share, not a division by zero.
    assert_eq!(
        dominant_share_permille(&Totals::default(), &Totals::default()),
        0
    );
}

#[test]
fn cancelled_work_is_charged_because_it_held_the_slot() {
    // A principal who cancels a four-hour job at hour three has spent three
    // hours of the cluster. Forgiving that would make cancellation a way to
    // run for free.
    let who = UserId([1u8; 32]);
    let reqs = Requirements {
        cpus: 4,
        mem_mb: 512,
        disk_mb: 0,
    };
    let c = charge_for(who, reqs, Some(0), Some(3_600 * 1_000_000), None).unwrap();
    assert_eq!(c.charge.cpu_secs, 3_600 * 4);
    assert_eq!(c.charge.mem_mb_secs, 3_600 * 512);
}

#[test]
fn the_meter_and_the_state_agree_after_a_flush() {
    let (_d, engine, node) = engine_with_member();
    let meter = UsageMeter::new(engine.clone());
    let who = UserId([5u8; 32]);
    let window = mesh_proto::usage_window(mesh_proto::now_micros());
    for _ in 0..25 {
        meter.charge(&mesh_core::accounting::Chargeable {
            principal: who,
            window,
            charge: Charge {
                wall_secs: 10,
                cpu_secs: 40,
                mem_mb_secs: 5_120,
                gpu_secs: 0,
                measured_cpu_secs: Some(9),
            },
        });
    }
    meter.flush();
    let now = mesh_proto::now_micros();
    assert_eq!(
        engine.read(|s| s.usage_of(&who, now)),
        (25 * 40, 25 * 5_120, 0)
    );
    assert_eq!(
        engine
            .read(|s| s.usage.get(&(node.node_id(), who, window)).copied())
            .unwrap()
            .jobs,
        25
    );
}

// ---------------------------------------------------------------------------
// P1 end to end — through the surface an operator touches
// ---------------------------------------------------------------------------

async fn add_user(node: &TestNode, user: &Keypair, username: &str, role: Role) {
    let admin = founder_user_key(node);
    let mut record = UserRecord {
        user_id: user.user_id(),
        username: username.into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: admin.user_id(),
        signature: Vec::new(),
    };
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::UserAdd { record }).await,
        IpcResponse::Ok
    ));
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id: user.user_id(),
        role,
        nodes: "any".into(),
        services: "*".into(),
        expires_at_micros: None,
        created_at_micros: mesh_proto::now_micros(),
        granted_by: admin.user_id(),
        signature: Vec::new(),
    };
    grant.signature = admin.sign(&grant.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(node, admin, IpcRequest::GrantAdd { grant }).await,
        IpcResponse::Ok
    ));
}

fn run_req(command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mesh_keeps_count_of_what_it_did_and_for_whom() {
    // C1 P1's demo, in one test: two principals run work, and the node
    // holds who used what — as replicated counters, with no accounting
    // server anywhere. Fair share reads nothing else.
    let node = spawn_founder("alpha", "c1-mesh").await;
    let ada = Keypair::generate().unwrap();
    add_user(&node, &ada, "ada", Role::Operator).await;

    // The founder runs one job; ada runs three.
    let founder = founder_user_key(&node);
    assert!(matches!(
        run_to_end(&node, founder, run_req("true")).await,
        RunOutcome::Done { .. }
    ));
    for _ in 0..3 {
        assert!(matches!(
            run_to_end(&node, &ada, run_req("true")).await,
            RunOutcome::Done { .. }
        ));
    }

    // Nothing is visible until a flush — accounting must not cost one op
    // per job, which is the whole reason C0 came first.
    let meter = node.node.jobs_for_test().meter();
    assert!(meter.flush() > 0, "the meter had nothing to publish");

    let jobs_of = |who: UserId| {
        node.node.engine().read(|s| {
            s.usage
                .iter()
                .filter(|((_, principal, _), _)| *principal == who)
                .map(|(_, partial)| partial.jobs)
                .sum::<u64>()
        })
    };
    assert_eq!(jobs_of(ada.user_id()), 3);
    assert_eq!(jobs_of(founder.user_id()), 1);

    // Three of four jobs is the larger share, so ada is the one who waits.
    let now = mesh_proto::now_micros();
    let usage = node.node.engine().read(|s| s.usage_by_principal(now));
    assert!(usage[&ada.user_id()].0 > 0);
    let shares = weighted_shares(&usage);
    assert!(
        shares[&ada.user_id()] >= shares[&founder.user_id()],
        "{shares:?}"
    );
}

// ---------------------------------------------------------------------------
// P2 — live counters, agreed across the mesh
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn every_node_agrees_on_every_principals_share() {
    // C1 P2's demo, and the claim that makes DRF possible here at all:
    // **no node asks another node anything.** Each computes every
    // principal's share from replicated counters, so two nodes ordering
    // their own queues arrive at the same ordering — which is what makes a
    // leaderless fair-share scheduler coherent rather than merely
    // decentralised.
    use mesh_testkit::{join_new_node, wait_until};

    let alpha = spawn_founder("alpha", "c1-share").await;
    let beta = join_new_node("beta", &alpha, vec![]).await;

    let ada = Keypair::generate().unwrap();
    add_user(&alpha, &ada, "ada", Role::Operator).await;
    wait_until(Duration::from_secs(10), || {
        beta.node
            .engine()
            .read(|s| s.users.contains_key(&ada.user_id()))
    })
    .await;

    // Work on both machines, for both principals, so neither node has the
    // whole picture from its own execution alone.
    for _ in 0..3 {
        assert!(matches!(
            run_to_end(&alpha, &ada, run_req("true")).await,
            RunOutcome::Done { .. }
        ));
    }
    assert!(matches!(
        run_to_end(&beta, founder_user_key(&alpha), run_req("true")).await,
        RunOutcome::Done { .. }
    ));

    alpha.node.jobs_for_test().meter().flush();
    beta.node.jobs_for_test().meter().flush();

    // One gossip round is all it takes; nothing is asked for, only merged.
    let want = ada.user_id();
    wait_until(Duration::from_secs(15), || {
        let now = mesh_proto::now_micros();
        let a = alpha.node.engine().read(|s| s.usage_of(&want, now));
        let b = beta.node.engine().read(|s| s.usage_of(&want, now));
        a == b && a.0 > 0
    })
    .await;

    // And the *shares* agree, not merely one principal's counters — which
    // is what two nodes ordering their own queues actually compare.
    wait_until(Duration::from_secs(15), || {
        let now = mesh_proto::now_micros();
        let a = weighted_shares(&alpha.node.engine().read(|s| s.usage_by_principal(now)));
        let b = weighted_shares(&beta.node.engine().read(|s| s.usage_by_principal(now)));
        a == b && a.len() == 2
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_counter_survives_the_node_that_wrote_it_going_quiet() {
    // A partial is a running total, not a delta: once it has gossiped, the
    // mesh keeps it whether or not its author says anything again. That is
    // what makes accounting survive a node leaving mid-window, and it is
    // the property a delta-based counter could not have without
    // exactly-once delivery.
    use mesh_testkit::{join_new_node, wait_until};

    let alpha = spawn_founder("alpha", "c1-quiet").await;
    let beta = join_new_node("beta", &alpha, vec![]).await;
    let who = founder_user_key(&alpha).user_id();

    assert!(matches!(
        run_to_end(&beta, founder_user_key(&alpha), run_req("true")).await,
        RunOutcome::Done { .. }
    ));
    beta.node.jobs_for_test().meter().flush();

    wait_until(Duration::from_secs(15), || {
        alpha
            .node
            .engine()
            .read(|s| s.usage_of(&who, mesh_proto::now_micros()))
            .0
            > 0
    })
    .await;

    let before = alpha
        .node
        .engine()
        .read(|s| s.usage_of(&who, mesh_proto::now_micros()));
    drop(beta);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        alpha
            .node
            .engine()
            .read(|s| s.usage_of(&who, mesh_proto::now_micros())),
        before,
        "a departed node's usage vanished from the mesh"
    );
}

// ---------------------------------------------------------------------------
// P4 — DRF fair share
// ---------------------------------------------------------------------------

#[test]
fn a_mesh_with_no_usage_keeps_arrival_order() {
    // The property that makes this safe to ship on a running mesh: with
    // nothing to arbitrate, nothing changes.
    let usage = std::collections::BTreeMap::new();
    assert!(weighted_shares(&usage).is_empty());
}

#[test]
fn every_principal_is_weighed_equally() {
    // With quotas gone there is no entitlement to weigh against: each
    // principal is owed the same slice, so whoever has used less runs
    // first, and two principals who have used the same tie.
    let ada = UserId([1u8; 32]);
    let bob = UserId([2u8; 32]);
    let mut usage = std::collections::BTreeMap::new();
    usage.insert(ada, (300u64, 0u64, 0u64));
    usage.insert(bob, (700u64, 0u64, 0u64));
    let shares = weighted_shares(&usage);
    assert!(shares[&ada] < shares[&bob]);

    usage.insert(bob, (300u64, 0u64, 0u64));
    let shares = weighted_shares(&usage);
    assert_eq!(shares[&ada], shares[&bob]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_users_flood_no_longer_starves_another_users_small_job() {
    // C1 P4's demo. One principal fills the queue; a second submits one
    // job well behind it in arrival order, and runs before the flood
    // drains — because the flood's owner is far ahead of their share.
    use mesh_testkit::{spawn_founder_with, stream_as};

    // One slot, so everything after the first submission queues and the
    // ordering is the only thing that decides what runs next.
    let node = spawn_founder_with("alpha", "c1-drf", vec![], |c| {
        c.limits.slots = 1;
        c.limits.queue = 200;
    })
    .await;
    let admin = founder_user_key(&node);
    let hog = Keypair::generate().unwrap();
    let ada = Keypair::generate().unwrap();
    add_user(&node, &hog, "hog", Role::Operator).await;
    add_user(&node, &ada, "ada", Role::Operator).await;

    // The hog has already used the mesh heavily this hour — the state a
    // 100k-item parallel reaches within seconds of starting.
    let window = mesh_proto::usage_window(mesh_proto::now_micros());
    node.node
        .engine()
        .publish_usage(mesh_proto::UsagePartial {
            principal: hog.user_id(),
            window,
            cpu_secs: 100_000,
            mem_mb_secs: 0,
            gpu_secs: 0,
            wall_secs: 100_000,
            jobs: 10_000,
        })
        .unwrap();

    // Fill the queue with the hog's work, then add one job for ada, last.
    let mut streams = Vec::new();
    for _ in 0..12 {
        streams.push(stream_as(&node, &hog, run_req("sleep 20")).await);
    }
    mesh_testkit::wait_until(Duration::from_secs(10), || {
        node.node.jobs_for_test().queued_count() >= 8
    })
    .await;
    streams.push(stream_as(&node, &ada, run_req("sleep 20")).await);

    // Ada arrived last and is first in line, because she is furthest behind.
    mesh_testkit::wait_until(Duration::from_secs(10), || {
        node.node
            .jobs_for_test()
            .queue_rows()
            .first()
            .is_some_and(|r| r.username == "ada")
    })
    .await;

    let queue = node.node.jobs_for_test().queue_rows();
    let ada_row = queue.iter().find(|r| r.username == "ada").unwrap();
    let hog_row = queue.iter().find(|r| r.username == "hog").unwrap();
    assert_eq!(ada_row.position, 1, "{queue:#?}");
    assert!(
        ada_row.share_permille < hog_row.share_permille,
        "ada {} vs hog {}",
        ada_row.share_permille,
        hog_row.share_permille
    );
    drop(streams);
    let _ = admin;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn priority_still_outranks_a_share() {
    // A share answers "whose turn"; a priority answers "this first". A
    // scheduler that let the first quietly overrule the second would
    // surprise the operator who set the priority.
    use mesh_testkit::{spawn_founder_with, stream_as};

    let node = spawn_founder_with("alpha", "c1-prio", vec![], |c| {
        c.limits.slots = 1;
        c.limits.queue = 100;
    })
    .await;
    let hog = Keypair::generate().unwrap();
    let ada = Keypair::generate().unwrap();
    add_user(&node, &hog, "hog", Role::Operator).await;
    add_user(&node, &ada, "ada", Role::Operator).await;

    let window = mesh_proto::usage_window(mesh_proto::now_micros());
    node.node
        .engine()
        .publish_usage(mesh_proto::UsagePartial {
            principal: hog.user_id(),
            window,
            cpu_secs: 100_000,
            mem_mb_secs: 0,
            gpu_secs: 0,
            wall_secs: 100_000,
            jobs: 10_000,
        })
        .unwrap();

    let mut streams = vec![stream_as(&node, &ada, run_req("sleep 5")).await];
    mesh_testkit::wait_until(Duration::from_secs(10), || {
        node.node.jobs_for_test().running_count() >= 1
    })
    .await;
    // Ada is far behind her share, but the hog's job is marked high.
    streams.push(stream_as(&node, &ada, run_req("sleep 0.1")).await);
    let mut high = run_req("sleep 0.1");
    if let IpcRequest::Run { priority, .. } = &mut high {
        *priority = JobPriority::High;
    }
    streams.push(mesh_testkit::stream_as(&node, &hog, high).await);

    mesh_testkit::wait_until(Duration::from_secs(10), || {
        node.node.jobs_for_test().queue_rows().len() >= 2
    })
    .await;
    let queue = node.node.jobs_for_test().queue_rows();
    assert_eq!(
        queue[0].username, "hog",
        "a share overruled an explicit priority: {queue:#?}"
    );
    drop(streams);
}

// ---------------------------------------------------------------------------
// P5 — preemption
// ---------------------------------------------------------------------------

fn preemptible_req(command: &str) -> IpcRequest {
    let mut req = run_req(command);
    if let IpcRequest::Run { env, .. } = &mut req {
        env.push((mesh_proto::REQ_PREEMPTIBLE.to_string(), "1".into()));
    }
    req
}

#[test]
fn preemptible_is_opt_in_and_never_a_default() {
    // The single most important property of this phase. Silently killing
    // somebody's four-hour run to satisfy a share is how a scheduler loses
    // a user permanently.
    assert!(!mesh_proto::is_preemptible(&[]));
    assert!(!mesh_proto::is_preemptible(&[(
        "MESHWARE_TASK_CPUS".into(),
        "8".into()
    )]));
    assert!(mesh_proto::is_preemptible(&[(
        mesh_proto::REQ_PREEMPTIBLE.into(),
        "1".into()
    )]));
    // A value that is not an affirmative is not an affirmative.
    assert!(!mesh_proto::is_preemptible(&[(
        mesh_proto::REQ_PREEMPTIBLE.into(),
        "0".into()
    )]));
}

#[test]
fn suspend_is_not_a_preempt_mode() {
    // §5.2: cancel and requeue only. A `suspend` that means SIGSTOP holds
    // the memory it was meant to release, and shipping it would be worse
    // than not having it.
    assert_eq!(mesh_proto::PreemptMode::parse("suspend"), None);
    assert_eq!(
        mesh_proto::PreemptMode::parse("requeue"),
        Some(mesh_proto::PreemptMode::Requeue)
    );
    assert_eq!(
        mesh_proto::PreemptMode::parse("cancel"),
        Some(mesh_proto::PreemptMode::Cancel)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_preempted_when_nobody_is_waiting() {
    // Reclamation exists to give somebody a slot. With no queue there is
    // nobody to give one to, and killing work anyway would be churn
    // dressed up as fairness.
    use mesh_testkit::{spawn_founder_with, stream_as};
    let node = spawn_founder_with("alpha", "c1-noreclaim", vec![], |c| {
        c.limits.slots = 4;
    })
    .await;
    let ada = Keypair::generate().unwrap();
    add_user(&node, &ada, "ada", Role::Operator).await;

    let held = stream_as(&node, &ada, preemptible_req("sleep 20")).await;
    mesh_testkit::wait_until(Duration::from_secs(10), || {
        !node.node.jobs_for_test().preemptible_running().is_empty()
    })
    .await;
    assert!(node.node.jobs_for_test().waiting_principals().is_empty());

    node.node.reclaim_pass_for_test().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        node.node.jobs_for_test().preemptible_running().len(),
        1,
        "work was preempted with nobody waiting for the slot"
    );
    drop(held);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preemptible_slot_is_reclaimed_for_the_principal_who_is_behind() {
    // C1 P5's demo: a slot held by volunteered work is reclaimed and the
    // preempted task requeues.
    use mesh_testkit::{spawn_founder_with, stream_as};
    let node = spawn_founder_with("alpha", "c1-reclaim", vec![], |c| {
        c.limits.slots = 1;
        c.limits.queue = 20;
    })
    .await;
    let hog = Keypair::generate().unwrap();
    let ada = Keypair::generate().unwrap();
    add_user(&node, &hog, "hog", Role::Operator).await;
    add_user(&node, &ada, "ada", Role::Operator).await;

    // The hog is far ahead of their share.
    let window = mesh_proto::usage_window(mesh_proto::now_micros());
    node.node
        .engine()
        .publish_usage(mesh_proto::UsagePartial {
            principal: hog.user_id(),
            window,
            cpu_secs: 100_000,
            mem_mb_secs: 0,
            gpu_secs: 0,
            wall_secs: 100_000,
            jobs: 10_000,
        })
        .unwrap();

    // The hog holds the only slot, with work it volunteered as preemptible.
    let hog_stream = stream_as(&node, &hog, preemptible_req("sleep 30")).await;
    mesh_testkit::wait_until(Duration::from_secs(10), || {
        !node.node.jobs_for_test().preemptible_running().is_empty()
    })
    .await;
    let victim = node.node.jobs_for_test().preemptible_running()[0].0;

    // Ada, who has used nothing, waits behind it.
    let ada_stream = stream_as(&node, &ada, run_req("true")).await;
    mesh_testkit::wait_until(Duration::from_secs(10), || {
        node.node.jobs_for_test().queued_count() >= 1
    })
    .await;

    node.node.reclaim_pass_for_test().await;

    // The hog's job leaves the slot, and ada's runs.
    mesh_testkit::wait_until(Duration::from_secs(20), || {
        !node.node.jobs_for_test().is_running(&victim)
    })
    .await;
    // And the preemption is in the audit log, as its own action — a
    // reclamation nobody can find afterwards is a silent kill.
    mesh_testkit::wait_until(Duration::from_secs(10), || {
        node.node
            .engine()
            .read(|s| s.audit.values().any(|e| e.action == "preemption"))
    })
    .await;
    let entry = node
        .node
        .engine()
        .read(|s| s.audit.values().find(|e| e.action == "preemption").cloned());
    let entry = entry.unwrap();
    assert!(entry.detail.contains("ada"), "{}", entry.detail);
    assert_eq!(entry.outcome, "requeue");
    drop((hog_stream, ada_stream));
}
