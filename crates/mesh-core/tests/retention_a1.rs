//! M31 A1 — a retention window for the replicated tier.
//!
//! C0 bounded the half of the op-log that something *supersedes*. A finished
//! job's last `JobUpsert` supersedes nothing and is superseded by nothing, so
//! it survived for ever — one op per job, on every node, permanently. That is
//! the wall M31 §1 measures, and this is the phase that puts a window in it.
//!
//! Three properties, in the order they matter:
//!
//! 1. A closed job past the window leaves the log; one inside it, and one
//!    that never finished, do not.
//! 2. **Two nodes with different node config and the same replicated policy
//!    drop exactly the same ops.** This is the C9 test and it is the one that
//!    matters — a history that depends on who you ask is not one.
//! 3. A node with no code for `RetentionPut` stores, verifies and forwards it
//!    without wedging (F0), and simply keeps everything.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::StateEngine;
use mesh_proto::{
    AuditEntry, AuditId, Hlc, JobId, JobState, JobSummary, MemberInfo, NodeId, OpBody,
    RetentionPolicy, RetentionTable, RetentionWindow, UserId,
};

const DAY: u64 = 86_400 * 1_000_000;

fn member_info(key: &Keypair, name: &str) -> MemberInfo {
    MemberInfo {
        node_id: key.node_id(),
        name: name.into(),
        addrs: vec!["127.0.0.1:7777".into()],
        tags: vec![],
        slots: 8,
        joined_at_micros: mesh_proto::now_micros(),
    }
}

/// A job in one state, with an explicit finish instant when terminal.
fn job(executor: NodeId, id: u8, state: JobState, at: u64) -> OpBody {
    OpBody::JobUpsert {
        summary: JobSummary {
            job_id: JobId([id; 16]),
            cmd_preview: "echo hi".into(),
            runtime: "bash".into(),
            submitter: executor,
            submitter_user: UserId([7u8; 32]),
            executor,
            state,
            exit_code: None,
            created_at_micros: at.saturating_sub(1_000_000),
            started_at_micros: Some(at.saturating_sub(500_000)),
            finished_at_micros: state.is_terminal().then_some(at),
            fanout: None,
            updated_hlc: Hlc {
                wall_micros: at,
                logical: 0,
            },
        },
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    node: Keypair,
    founder: Keypair,
    genesis: Genesis,
}

fn setup() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let founder = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("a1-mesh", &founder, node.node_id()).unwrap();
    Fixture {
        _dir: dir,
        node,
        founder,
        genesis,
    }
}

fn engine(fx: &Fixture, dir: &std::path::Path) -> Arc<StateEngine> {
    let e = StateEngine::open(dir, fx.node.clone(), fx.genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&fx.node, "alpha"),
        invite: None,
    })
    .unwrap();
    e
}

/// Register the founder as an admin user so admin-signed ops apply.
fn admit_founder(e: &Arc<StateEngine>, founder: &Keypair) {
    // The genesis founder is an admin by construction — `check_user_signed`
    // recognises the founding key without a `UserAdd`. Nothing to do, and
    // saying so here saves the next reader the same investigation.
    let _ = (e, founder);
}

fn policy_with(runs_days: u32) -> RetentionPolicy {
    let mut p = RetentionPolicy::default();
    p.set(
        RetentionTable::Runs,
        RetentionWindow {
            days: runs_days,
            floor_days: 0,
        },
    );
    p
}

// ---------------------------------------------------------------------------
// The policy travels, and it is what the compactor reads
// ---------------------------------------------------------------------------

#[test]
fn a_signed_policy_replicates_and_becomes_what_compaction_reads() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());
    admit_founder(&e, &fx.founder);

    // Before anything is signed, the shipped defaults apply — which is what
    // makes a fresh mesh (and an upgraded one) bounded before anybody reads
    // a release note.
    assert_eq!(e.retention().window(RetentionTable::Runs).days, 90);
    assert!(!e.retention().is_signed());

    let signed = mesh_core::retention::sign(&fx.founder, policy_with(30), mesh_proto::now_micros());
    mesh_core::retention::put(&e, signed).unwrap();

    let live = e.retention();
    assert_eq!(live.window(RetentionTable::Runs).days, 30);
    assert!(live.is_signed(), "the record must carry who decided");
    assert_eq!(live.created_by, fx.founder.user_id());
}

/// The exit demo, at test scale: a mesh doing real volume settles at the
/// size the projection predicted instead of growing for ever.
#[test]
fn a_log_of_closed_jobs_settles_instead_of_growing_forever() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());
    let me = fx.node.node_id();
    let now = 400 * DAY;

    // A year of jobs, one a day, each queued → running → finished.
    for day in 1..=365u64 {
        let at = day * DAY;
        for state in [JobState::Queued, JobState::Running, JobState::Succeeded] {
            e.author_op(job(me, (day % 250) as u8 + 1, state, at))
                .unwrap();
        }
    }

    mesh_core::retention::put(
        &e,
        mesh_core::retention::sign(&fx.founder, policy_with(90), now),
    )
    .unwrap();

    let plan = e.compaction_plan(0).unwrap();
    assert!(
        plan.retired > 0,
        "nothing was retired, so the window did nothing"
    );

    // And through a **real pass**, not just a plan: the demo is that the
    // log gets smaller, not that a function said it would.
    let before = e.all_ops().unwrap().len();
    let (dropped, checkpoint) = e.compact_self(0).unwrap();
    let after = e.all_ops().unwrap().len();
    assert!(dropped > 0 && checkpoint > 0);
    assert!(
        after < before,
        "a year of jobs did not shrink the log: {before} → {after}"
    );
    // A year of daily work settles to roughly the window's worth of jobs
    // plus the permanent entities, rather than to a year's worth.
    assert!(
        after < 300,
        "the log did not settle: {after} ops survive a year of work"
    );

    // The point of the whole phase, stated as the number an operator reads.
    //
    // Every synthetic job here is dated in 1970–71 — day 1 through 365 of
    // the epoch — while `compact_self` runs against the *real* clock. So
    // all of them are decades past a 90-day window, and the correct result
    // is that **no `JobUpsert` op is left at all**. Before A1 every one of
    // them would have survived for ever, which is the wall §1 measures.
    let census = e.census().unwrap();
    assert!(
        census.by_kind.iter().all(|k| k.kind != "JobUpsert"),
        "closed jobs survived the window: {:?}",
        census.by_kind
    );
    // What is left is what *should* be: the member record, the policy, and
    // the checkpoint that vouches for the holes. All permanent, all tiny.
    assert_eq!(
        census.surviving_total, census.ops_total,
        "something left in the log is not permanent: {:?}",
        census.by_kind
    );

    // A second pass with nothing new to drop is a no-op (ENG-5). It used to
    // swap the checkpoint for an identical new one every time, so a settled
    // log still wrote — and every peer kept — one op an hour for ever. The
    // op count is the other half: C0's
    // `repeated_passes_do_not_leak_a_checkpoint_each_time` guards the swap
    // when there *is* something new.
    assert_eq!(e.compact_self(0).unwrap(), (0, 0));
    assert_eq!(
        e.all_ops().unwrap().len(),
        after,
        "a second pass changed the size of a settled log"
    );
}

// ---------------------------------------------------------------------------
// C9 — the test that matters
// ---------------------------------------------------------------------------

/// **Two nodes with different node config and the same replicated policy
/// drop exactly the same ops.**
///
/// The horizon that governs *what anyone reads* is replicated; the one that
/// governs *how long a node keeps bytes* is local. Only the first may change
/// an answer, and this asserts that the second cannot.
#[test]
fn two_nodes_with_different_config_and_one_policy_drop_the_same_ops() {
    let fx = setup();
    let now = 400 * DAY;

    let mut plans = Vec::new();
    // The two nodes differ in the one node-local knob that reaches the
    // compaction path at all.
    for horizon_secs in [0u64, 30 * 24 * 3600] {
        let dir = tempfile::tempdir().unwrap();
        let e = engine(&fx, dir.path());
        let me = fx.node.node_id();
        for day in [10u64, 100, 200, 300, 395] {
            let at = day * DAY;
            e.author_op(job(me, day as u8, JobState::Succeeded, at))
                .unwrap();
        }
        mesh_core::retention::put(
            &e,
            mesh_core::retention::sign(&fx.founder, policy_with(90), now),
        )
        .unwrap();
        plans.push(e.compaction_plan(horizon_secs).unwrap());
    }

    assert_eq!(
        plans[0].retired, plans[1].retired,
        "two differently-configured nodes retired different numbers of ops"
    );
    assert!(
        plans[0].retired > 0,
        "the fixture must actually retire something for this to mean anything"
    );
}

// ---------------------------------------------------------------------------
// What must never be dropped
// ---------------------------------------------------------------------------

/// A non-terminal job is never dropped whatever its age — the
/// `sweep_retention` rule, restated on the replicated side. This is the one
/// that would be data loss rather than a policy question.
#[test]
fn a_job_still_running_is_never_retired_however_old() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());
    let me = fx.node.node_id();

    e.author_op(job(me, 1, JobState::Running, DAY)).unwrap();
    mesh_core::retention::put(
        &e,
        mesh_core::retention::sign(&fx.founder, policy_with(1), 10_000 * DAY),
    )
    .unwrap();

    let plan = e.compaction_plan(0).unwrap();
    assert_eq!(plan.retired, 0);
    // The running job's op is still there after a real pass.
    e.compact_self(0).unwrap();
    let bodies: Vec<OpBody> = e
        .all_ops()
        .unwrap()
        .iter()
        .filter_map(|o| o.decode_body())
        .collect();
    assert!(
        bodies.iter().any(|b| matches!(
            b,
            OpBody::JobUpsert { summary } if summary.state == JobState::Running
        )),
        "a running job's record was compacted away"
    );
}

/// `AuditAppend` is never dropped, at any window, including one that tries
/// to say zero. The single most important assertion in this file.
#[test]
fn the_audit_log_survives_every_window_including_zero() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());
    let me = fx.node.node_id();

    for i in 1..=5u8 {
        e.author_op(OpBody::AuditAppend {
            entry: AuditEntry {
                audit_id: AuditId([i; 16]),
                at_micros: u64::from(i),
                user: UserId([7u8; 32]),
                username: "ada".into(),
                action: "secret.resolve".into(),
                target: format!("job-{i}"),
                node: me,
                outcome: "ok".into(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        })
        .unwrap();
    }

    for days in [1u32, 90, 0] {
        mesh_core::retention::put(
            &e,
            mesh_core::retention::sign(&fx.founder, policy_with(days), 10_000 * DAY),
        )
        .unwrap();
        e.compact_self(0).unwrap();
        let audits = e
            .all_ops()
            .unwrap()
            .iter()
            .filter(|o| matches!(o.decode_body(), Some(OpBody::AuditAppend { .. })))
            .count();
        assert_eq!(audits, 5, "an audit entry was dropped at runs={days}");
    }
}

/// A `RetentionPut` below the audit floor is refused, and the refusal names
/// the floor. Refused **before** signing, and again at merge time on every
/// node — a floor the writer enforces is a floor, one every applier
/// enforces is a rule.
#[test]
fn a_policy_below_the_audit_floor_is_refused_and_names_the_floor() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let mut bad = RetentionPolicy::default();
    bad.set(
        RetentionTable::Audit,
        RetentionWindow {
            days: 30,
            floor_days: mesh_proto::AUDIT_FLOOR_DAYS,
        },
    );

    let err =
        mesh_core::retention::put(&e, mesh_core::retention::sign(&fx.founder, bad.clone(), 0))
            .expect_err("must be refused");
    assert!(err.to_string().contains("365"), "{err}");

    // And if one reaches the log anyway — a peer running a build with a
    // lower floor, say — every applier refuses it and the live policy is
    // unchanged.
    e.author_op(OpBody::RetentionPut {
        policy: Box::new(mesh_core::retention::sign(&fx.founder, bad, 0)),
    })
    .unwrap();
    assert!(
        e.retention().window(RetentionTable::Audit).is_forever(),
        "a policy below the floor was applied"
    );
}

// ---------------------------------------------------------------------------
// F0 — a node that has never heard of retention
// ---------------------------------------------------------------------------

/// A pre-`RetentionPut` node stores, verifies and forwards the op without
/// wedging, and simply enforces nothing.
///
/// The asymmetry runs the *safe* way round, and it is worth naming: a node
/// that cannot decode the policy keeps **more** than it was told to. A mixed
/// mesh converges on storage more slowly and never incorrectly — nobody
/// drops anything they were not told to drop.
#[test]
fn a_node_with_no_code_for_retention_carries_the_op_and_keeps_everything() {
    use mesh_testkit::ops::{future_body, now_hlc, seal_raw};

    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    // An op from a build one variant past this one: exactly the shape a
    // future `RetentionPut` successor has to this node.
    let seq = e.read(|s| s.applied.get(&fx.node.node_id()).copied().unwrap_or(0)) + 1;
    let op = seal_raw(
        &fx.node,
        seq,
        now_hlc(),
        future_body(9_999, b"a policy this build cannot read"),
    );
    assert_eq!(
        e.ingest(vec![op.clone()]).unwrap(),
        1,
        "an authentic future op must advance the watermark rather than park"
    );

    // Carried, not interpreted: the live policy is untouched.
    assert!(!e.retention().is_signed());
    assert!(
        e.all_ops()
            .unwrap()
            .iter()
            .any(|o| o.author == op.author && o.seq == op.seq),
        "the op must be stored so it can be forwarded"
    );

    // And the log still compacts, because an op it cannot classify is
    // carried rather than guessed at.
    e.compact_self(0).unwrap();
    assert!(
        e.all_ops()
            .unwrap()
            .iter()
            .any(|o| o.author == op.author && o.seq == op.seq),
        "an undecodable op was dropped by a pass that could not classify it"
    );
}

// ---------------------------------------------------------------------------
// The read side (C9's other half)
// ---------------------------------------------------------------------------

/// Compaction removes the op; `expire_closed` removes the entry. Without
/// the second, the node that authored the op keeps showing the job until it
/// restarts, while a node that *did* restart replays a log the op is gone
/// from — and the two answer "show me that job" differently.
#[test]
fn expiring_closed_jobs_keeps_memory_agreeing_with_the_log() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());
    let me = fx.node.node_id();
    let now = 400 * DAY;

    e.author_op(job(me, 1, JobState::Succeeded, 10 * DAY))
        .unwrap();
    e.author_op(job(me, 2, JobState::Succeeded, 395 * DAY))
        .unwrap();
    e.author_op(job(me, 3, JobState::Running, 10 * DAY))
        .unwrap();
    assert_eq!(e.read(|s| s.jobs.len()), 3);

    mesh_core::retention::put(
        &e,
        mesh_core::retention::sign(&fx.founder, policy_with(90), now),
    )
    .unwrap();

    let released = e.expire_closed(now);
    assert_eq!(released, 1, "only the 390-day-old closed job goes");
    let left = e.read(|s| s.jobs.len());
    assert_eq!(left, 2, "the recent job and the running one both stay");

    // `forever` releases nothing, because an operator typed it.
    mesh_core::retention::put(
        &e,
        mesh_core::retention::sign(
            &fx.founder,
            {
                let mut p = RetentionPolicy::default();
                p.set(RetentionTable::Runs, RetentionWindow::forever());
                p
            },
            now + 1,
        ),
    )
    .unwrap();
    assert_eq!(e.expire_closed(now), 0);
}

/// A reset returns the mesh to the shipped defaults — which are finite, so
/// it loosens a window rather than removing one.
#[test]
fn a_reset_returns_to_the_shipped_defaults_and_they_are_finite() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    mesh_core::retention::put(
        &e,
        mesh_core::retention::sign(&fx.founder, policy_with(7), 1_000),
    )
    .unwrap();
    assert_eq!(e.retention().window(RetentionTable::Runs).days, 7);

    let at = 2_000;
    e.author_op(OpBody::RetentionReset {
        by: fx.founder.user_id(),
        at_micros: at,
        signature: mesh_core::retention::sign_reset(&fx.founder, at),
    })
    .unwrap();

    let live = e.retention();
    assert_eq!(
        live.window(RetentionTable::Runs).days,
        90,
        "back to the default"
    );
    assert!(!live.is_signed());
    assert!(
        !live.window(RetentionTable::Runs).is_forever(),
        "a reset must not make the log unbounded again"
    );
}

/// A policy signed by a key nobody has admitted does not take effect.
///
/// The realistic route: it arrives from a peer, sealed by a real member's
/// node key, carrying a *user* signature from somebody the mesh has never
/// heard of. It is held rather than applied — and the live policy is
/// unchanged, which is the only thing that matters.
#[test]
fn a_policy_from_an_unknown_key_does_not_change_what_the_mesh_keeps() {
    use mesh_testkit::ops::{now_hlc, seal};

    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let stranger = Keypair::generate().unwrap();
    let seq = e.read(|s| s.applied.get(&fx.node.node_id()).copied().unwrap_or(0)) + 1;
    let op = seal(
        &fx.node,
        seq,
        now_hlc(),
        &OpBody::RetentionPut {
            policy: Box::new(mesh_core::retention::sign(&stranger, policy_with(1), 1_000)),
        },
    );
    // Ingesting must not error: a hostile or merely unknown signer is an
    // ordinary event, and an op that made the whole batch fail would let
    // any member stall a peer's sync.
    e.ingest(vec![op]).unwrap();

    assert_eq!(
        e.retention().window(RetentionTable::Runs).days,
        90,
        "an unknown key rewrote the mesh's retention policy"
    );
}

/// The daemon refuses to *author* a policy the local user cannot sign for,
/// which is the cheaper half of the same guarantee: the refusal happens
/// before the op exists rather than on every peer that receives it.
#[test]
fn a_non_admin_cannot_author_a_retention_policy_at_all() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let stranger = Keypair::generate().unwrap();
    let signed = mesh_core::retention::sign(&stranger, policy_with(1), 1_000);
    assert!(
        mesh_core::retention::put(&e, signed).is_err(),
        "an unknown key authored a retention policy"
    );
    assert_eq!(e.retention().window(RetentionTable::Runs).days, 90);
}
