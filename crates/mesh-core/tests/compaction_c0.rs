//! C0 — op-log compaction (C0–C9 §4).
//!
//! The property under test is not "the log got smaller". It is that the log
//! got smaller **and nothing changed**: a node that never sees a single
//! dropped op must materialize byte-identical state, and must not park
//! waiting for ops that no longer exist.
//!
//! The second half is the whole reason the checkpoint op exists. Without
//! it, dropping an op is indistinguishable from losing one, and the gapless
//! per-author rule parks every op behind the hole forever — which is a
//! quieter and worse failure than the unbounded log it was meant to fix.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::{MeshState, StateEngine};
use mesh_proto::{
    AuditEntry, AuditId, Hlc, JobId, JobState, JobSummary, MemberInfo, NodeId, OpBody, OpEnvelope,
    UserId,
};

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

fn job(executor: NodeId, id: u8, state: JobState, hlc: Hlc) -> OpBody {
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
            created_at_micros: 0,
            started_at_micros: None,
            finished_at_micros: None,
            fanout: None,
            updated_hlc: hlc,
        },
    }
}

/// Everything compaction must not perturb.
fn summarize(s: &MeshState) -> String {
    let mut out = String::new();
    for (id, m) in &s.members {
        out.push_str(&format!("member {} {}\n", id.short(), m.info.name));
    }
    for (id, j) in &s.jobs {
        out.push_str(&format!(
            "job {} state={:?} hlc={}\n",
            id.short(),
            j.state,
            j.updated_hlc.wall_micros
        ));
    }
    for (id, a) in &s.audit {
        out.push_str(&format!("audit {} {}\n", id.short(), a.action));
    }
    out
}

/// A founder engine with itself as a member, plus `n` finished jobs each
/// written through the three-op lifecycle a real executor writes.
fn seeded(dir: &std::path::Path, node: &Keypair, genesis: &Genesis, jobs: u8) -> Arc<StateEngine> {
    let e = StateEngine::open(dir, node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(node, "alpha"),
        invite: None,
    })
    .unwrap();
    let me = node.node_id();
    for i in 1..=jobs {
        for (n, state) in [
            (1u64, JobState::Queued),
            (2, JobState::Running),
            (3, JobState::Succeeded),
        ] {
            let hlc = Hlc {
                wall_micros: u64::from(i) * 10 + n,
                logical: 0,
            };
            e.author_op(job(me, i, state, hlc)).unwrap();
        }
        // One audit line per job — the thing that must survive all of this.
        e.author_op(OpBody::AuditAppend {
            entry: AuditEntry {
                audit_id: AuditId([i; 16]),
                at_micros: u64::from(i),
                user: UserId([7u8; 32]),
                username: "ada".into(),
                action: "job.run".into(),
                target: format!("job-{i}"),
                node: me,
                outcome: "ok".into(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        })
        .unwrap();
    }
    e
}

fn setup() -> (tempfile::TempDir, Keypair, Genesis) {
    let dir = tempfile::tempdir().unwrap();
    let founder_user = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("c0-mesh", &founder_user, node.node_id()).unwrap();
    (dir, node, genesis)
}

/// A signed deployment put, as the daemon authors one.
fn deployment_put(user: &Keypair, name: &str, count: u32) -> OpBody {
    let src = format!(
        "[deployment]\nname    = \"{name}\"\nservice = \"web@1\"\ncount   = {count}\ntargets = \"any\"\n"
    );
    let parsed = mesh_core::deployments::parse(&src, user.user_id()).unwrap();
    OpBody::DeploymentPut {
        deployment: Box::new(mesh_core::deployments::sign(user, parsed.deployment)),
    }
}

fn deployment_remove(user: &Keypair, name: &str) -> OpBody {
    OpBody::DeploymentRemove {
        name: name.into(),
        by: user.user_id(),
        signature: mesh_core::deployments::sign_remove(user, name),
    }
}

/// A deployment deleted and made again, compacted, then reopened
/// (2026-09-14).
///
/// Compaction keeps a deployment's newest put and every removal, so after
/// put → remove → put the first put is gone, and on the next start the
/// removal replays against a name with no entry yet. It used to *park*
/// there — and replay is gapless per author, so every op this node wrote
/// afterwards parked behind it: the deployments map came back empty, and the
/// next op the node authored would have reused a sequence number already on
/// disk.
#[test]
fn a_deployment_deleted_and_made_again_survives_compaction_and_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let user = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("c0-deployments", &user, node.node_id()).unwrap();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    e.author_op(deployment_put(&user, "web", 3)).unwrap();
    e.author_op(deployment_remove(&user, "web")).unwrap();
    e.author_op(deployment_put(&user, "web", 2)).unwrap();
    // Written after all of it, and must not be stranded behind it.
    e.author_op(job(
        node.node_id(),
        1,
        JobState::Succeeded,
        Hlc {
            wall_micros: 1,
            logical: 0,
        },
    ))
    .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, _) = e.compact_self(0).unwrap();
    assert!(
        dropped >= 1,
        "the first put should have been compacted away"
    );
    let top = e.all_ops().unwrap().iter().map(|o| o.seq).max().unwrap();
    drop(e);

    let e = StateEngine::open(dir.path(), node.clone(), genesis).unwrap();
    assert_eq!(
        e.read(|s| s.applied.get(&node.node_id()).copied()),
        Some(top),
        "replay parked behind a removal whose put was compacted away"
    );
    assert_eq!(
        e.read(|s| s
            .deployments
            .get("web")
            .map(|d| (d.removed, d.deployment.count.as_display()))),
        Some((false, "2".to_string())),
        "the deployment made again is the one that stands"
    );
    assert_eq!(e.read(|s| s.jobs.len()), 1, "the op after it was stranded");
}

#[test]
fn compaction_drops_superseded_ops_and_changes_no_state() {
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 20);

    let before_ops = e.all_ops().unwrap().len();
    let before_state = e.read(summarize);

    // Horizon 0 with a beat of daylight: every superseded op is older than
    // "now", nothing that won is.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, checkpoint_seq) = e.compact_self(0).unwrap();

    assert!(checkpoint_seq > 0, "a pass that drops must vouch for it");
    // 20 jobs × 2 superseded upserts.
    assert_eq!(dropped, 40, "exactly the superseded job upserts");

    let after_ops = e.all_ops().unwrap().len();
    // -40 dropped, +1 checkpoint.
    assert_eq!(after_ops, before_ops - 40 + 1);
    assert_eq!(
        e.read(summarize),
        before_state,
        "compaction changed materialized state"
    );
}

#[test]
fn the_audit_log_survives_compaction_untouched() {
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 12);
    let audit_before = e.read(|s| s.audit.len());
    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();
    assert_eq!(e.read(|s| s.audit.len()), audit_before);
    // And the ops themselves are still on disk: tamper-evidence you cannot
    // reconstruct is not tamper-evidence.
    let audit_ops = e
        .all_ops()
        .unwrap()
        .iter()
        .filter(|o| matches!(o.decode_body(), Some(OpBody::AuditAppend { .. })))
        .count();
    assert_eq!(audit_ops, audit_before);
}

#[test]
fn a_peer_bootstrapping_from_a_compacted_log_reaches_identical_state() {
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 20);
    let expected = e.read(summarize);

    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();

    // A joiner sees only what survived — it never had the dropped ops and
    // never will.
    let surviving: Vec<OpEnvelope> = e.all_ops().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    b.ingest(surviving.clone()).unwrap();

    assert_eq!(b.read(summarize), expected, "a compacted log rebuilt wrong");

    // And it is *caught up*, not merely correct: the watermark stepped over
    // the holes, so the next op the author writes applies immediately
    // instead of parking behind ops that no longer exist.
    assert_eq!(
        b.version_vector(),
        e.version_vector(),
        "the joiner is still waiting for ops that were compacted away"
    );

    let next = e
        .author_op(job(
            node.node_id(),
            99,
            JobState::Succeeded,
            Hlc {
                wall_micros: 9_999,
                logical: 0,
            },
        ))
        .unwrap();
    assert_eq!(b.ingest(vec![next]).unwrap(), 1, "the joiner parked");
}

#[test]
fn delivery_order_does_not_matter_to_a_checkpoint() {
    // The checkpoint usually arrives *last* — it sits at the top of the log
    // — but anti-entropy is free to hand it over first, or alone. Both must
    // land the same way.
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 15);
    let expected = e.read(summarize);
    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();

    let mut ops = e.all_ops().unwrap();
    ops.reverse();

    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    b.ingest(ops).unwrap();
    assert_eq!(b.read(summarize), expected);
    assert_eq!(b.version_vector(), e.version_vector());
}

#[test]
fn a_hole_nobody_vouched_for_still_parks() {
    // The safety half of the design. Compaction advances a watermark past a
    // gap *because an author signed for it*; a gap from packet loss must
    // still park, or a lost op would be silently skipped forever.
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 3);
    let mut ops = e.all_ops().unwrap();
    assert!(ops.len() > 4);
    // Remove one op in the middle without any checkpoint to explain it.
    ops.remove(3);
    let missing_from = ops[2].seq + 1;

    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    b.ingest(ops).unwrap();

    let applied = b
        .read(|s| s.applied.get(&node.node_id()).copied())
        .unwrap_or(0);
    assert_eq!(
        applied,
        missing_from - 1,
        "an unexplained gap was skipped instead of parked"
    );
}

#[test]
fn repeated_passes_do_not_leak_a_checkpoint_each_time() {
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 10);
    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();

    let count_checkpoints = |e: &Arc<StateEngine>| {
        e.all_ops()
            .unwrap()
            .iter()
            .filter(|o| matches!(o.decode_body(), Some(OpBody::Checkpoint { .. })))
            .count()
    };
    assert_eq!(count_checkpoints(&e), 1);

    // A second pass with nothing new to drop still supersedes the first
    // checkpoint rather than stacking on it.
    for i in 21..=25u8 {
        let me = node.node_id();
        for (n, state) in [(1u64, JobState::Queued), (2, JobState::Succeeded)] {
            e.author_op(job(
                me,
                i,
                state,
                Hlc {
                    wall_micros: u64::from(i) * 10 + n,
                    logical: 0,
                },
            ))
            .unwrap();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();
    assert_eq!(count_checkpoints(&e), 1, "checkpoints accumulated");
}

fn checkpoints_in(e: &Arc<StateEngine>) -> usize {
    e.all_ops()
        .unwrap()
        .iter()
        .filter(|o| matches!(o.decode_body(), Some(OpBody::Checkpoint { .. })))
        .count()
}

fn applied_by(e: &Arc<StateEngine>, me: &NodeId) -> u64 {
    e.read(|s| s.applied.get(me).copied().unwrap_or(0))
}

/// ENG-5. Once one pass had pruned anything, every later pass found
/// something to do — the ranges already gone, and the checkpoint that said
/// so — and wrote a new checkpoint each hour for every peer to keep.
#[test]
fn an_idle_pass_writes_nothing() {
    let (dir, node, genesis) = setup();
    let me = node.node_id();
    let e = seeded(dir.path(), &node, &genesis, 10);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, checkpoint) = e.compact_self(0).unwrap();
    assert!(dropped > 0 && checkpoint > 0);
    let seq = applied_by(&e, &me);
    let stored = e.stored_ops_by(&me).unwrap();
    let reclaims = e.store_reclaims();

    for _ in 0..3 {
        assert_eq!(e.compact_self(0).unwrap(), (0, 0));
    }
    assert_eq!(applied_by(&e, &me), seq, "an idle pass authored an op");
    assert_eq!(e.stored_ops_by(&me).unwrap(), stored);
    assert_eq!(
        e.store_reclaims(),
        reclaims,
        "an idle pass rewrote the file"
    );
    assert_eq!(checkpoints_in(&e), 1);
}

/// A pass that drops a little does not stop the store to rewrite the file
/// for it; one that drops most of the log does (ENG-5).
#[test]
fn only_a_pass_that_frees_real_space_rewrites_the_file() {
    let (dir, node, genesis) = setup();
    let me = node.node_id();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    bulk_jobs(&e, me, 12_000);
    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();
    let reclaims = e.store_reclaims();
    assert_eq!(reclaims, 1, "dropping most of the log did not reclaim it");

    // One more job's worth of superseded writes: droppable, and tiny.
    for (n, state) in [(1u64, JobState::Queued), (2, JobState::Succeeded)] {
        e.author_op(job(
            me,
            200,
            state,
            Hlc {
                wall_micros: 9_000_000 + n,
                logical: 0,
            },
        ))
        .unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, _) = e.compact_self(0).unwrap();
    assert!(dropped >= 1);
    assert_eq!(
        e.store_reclaims(),
        reclaims,
        "a small pass rewrote the file"
    );
}

/// A pass that stopped between its checkpoint and its prune is finished by
/// the next one, without another checkpoint (ENG-5).
#[test]
fn an_interrupted_pass_is_finished_without_a_new_checkpoint() {
    let (dir, node, genesis) = setup();
    let me = node.node_id();
    let e = seeded(dir.path(), &node, &genesis, 10);
    std::thread::sleep(std::time::Duration::from_millis(5));
    // The checkpoint, and no prune — as if the node died in between.
    let plan = e.compaction_plan(0).unwrap();
    let doomed = plan.drop.len() as u64;
    assert!(doomed > 0);
    let through = applied_by(&e, &me);
    let after = plan.ops_before - plan.dropped_total() + 1;
    e.author_op(mesh_core::compact::checkpoint_body(
        &plan, through, 0, after,
    ))
    .unwrap();
    let stored = e.stored_ops_by(&me).unwrap();
    let seq = applied_by(&e, &me);

    let (dropped, checkpoint) = e.compact_self(0).unwrap();
    assert_eq!((dropped, checkpoint), (doomed, 0));
    assert_eq!(e.stored_ops_by(&me).unwrap(), stored - doomed);
    assert_eq!(applied_by(&e, &me), seq, "finishing a pass authored an op");
    assert_eq!(checkpoints_in(&e), 1);
    assert_eq!(e.compact_self(0).unwrap(), (0, 0));

    // And the state it describes is the state a restart rebuilds.
    let before = e.read(summarize);
    drop(e);
    let e = StateEngine::open(dir.path(), node, genesis).unwrap();
    assert_eq!(e.read(summarize), before);
}

#[test]
fn the_horizon_is_what_holds_recent_ops() {
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 10);
    // A 30-day horizon: everything here was written seconds ago.
    let (dropped, _) = e.compact_self(30 * 24 * 3600).unwrap();
    assert_eq!(dropped, 0, "the horizon did not hold anything back");
    let plan = e.compaction_plan(30 * 24 * 3600).unwrap();
    assert!(plan.is_empty());
    assert_eq!(plan.held_by_horizon, 20);
}

// ---------------------------------------------------------------------------
// The C0 P1 demo, through the surface an operator actually touches.
// ---------------------------------------------------------------------------

/// `meshnode state show` reports ops before and after — which is the phase's
/// stated demo, and the reason the IPC answer carries both numbers rather
/// than only the total.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_show_reports_ops_before_and_after_a_pass() {
    use mesh_proto::{IpcRequest, IpcResponse};
    use mesh_testkit::{founder_user_key, ipc_as, spawn_founder};

    let node = spawn_founder("alpha", "c0-mesh").await;
    let admin = founder_user_key(&node);

    let IpcResponse::StateStats(before) = ipc_as(&node, admin, IpcRequest::StateShow).await else {
        panic!("state show refused");
    };
    assert!(
        before.ops_total > 0,
        "a founder has authored its genesis ops"
    );
    assert_eq!(before.pruned_mine, 0, "nothing compacted yet");
    assert_eq!(before.horizon_secs, 30 * 24 * 3600, "the shipped default");

    // A dry run is a reading, not a write: the log is exactly as it was.
    let IpcResponse::StateStats(dry) = ipc_as(
        &node,
        admin,
        IpcRequest::StateCompact {
            horizon_secs: Some(0),
            dry_run: true,
        },
    )
    .await
    else {
        panic!("dry run refused");
    };
    assert!(dry.dry_run);
    assert_eq!(dry.checkpoint_seq, 0);
    let IpcResponse::StateStats(still) = ipc_as(&node, admin, IpcRequest::StateShow).await else {
        panic!("state show refused");
    };
    // A dry run compacts nothing. Deliberately *not* asserted as "the op
    // count did not move": a live node authors bookkeeping ops of its own
    // in the background (C1's usage counters and reservations), so a total
    // that happened to be stable would be an accident of timing rather
    // than the property being tested.
    assert_eq!(still.pruned_mine, 0, "a dry run compacted something");

    // Something that is certainly droppable: a job's superseded write.
    let supersede = |id: u8| {
        let me = node.node.node_id();
        for (n, state) in [(1u64, JobState::Queued), (2, JobState::Succeeded)] {
            let hlc = Hlc {
                wall_micros: u64::from(id) * 10 + n,
                logical: 0,
            };
            node.node
                .engine()
                .author_op(job(me, id, state, hlc))
                .unwrap();
        }
    };
    supersede(201);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // And the real pass reports what it took.
    let IpcResponse::StateStats(after) = ipc_as(
        &node,
        admin,
        IpcRequest::StateCompact {
            horizon_secs: Some(0),
            dry_run: false,
        },
    )
    .await
    else {
        panic!("compact refused");
    };
    assert!(!after.dry_run);
    // What the dry run predicted is not asserted equal to what the pass
    // took: the two are separated in time and this node keeps authoring
    // bookkeeping ops in between, so equality would be a timing assertion
    // wearing a correctness assertion's clothes.
    assert!(dry.dropped <= after.dropped.max(after.pruned_mine));
    assert!(after.dropped > 0);
    assert!(after.checkpoint_seq > 0);
    assert_eq!(after.pruned_mine, after.dropped);

    // A dry run after a pass counts what is newly droppable. It used to
    // subtract what earlier passes took from a number that no longer
    // included it, and read zero (ENG-5).
    supersede(202);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let IpcResponse::StateStats(dry) = ipc_as(
        &node,
        admin,
        IpcRequest::StateCompact {
            horizon_secs: Some(0),
            dry_run: true,
        },
    )
    .await
    else {
        panic!("dry run refused");
    };
    assert!(dry.dropped >= 1, "a dry run missed a superseded write");
}

// ---------------------------------------------------------------------------
// C0 P2 — live compaction, reclaim, and a sync path that knows about holes
// ---------------------------------------------------------------------------

/// Author `jobs` complete job lifecycles in one batch — the shape a busy
/// executor produces, and the shape that made C0 a prerequisite for C1.
fn bulk_jobs(e: &Arc<StateEngine>, me: NodeId, jobs: u32) {
    let bodies: Vec<OpBody> = (0..jobs)
        .flat_map(|i| {
            let id = (i % 251) as u8;
            let base = u64::from(i) * 8;
            [
                (base + 1, JobState::Queued),
                (base + 2, JobState::Running),
                (base + 3, JobState::Succeeded),
            ]
            .into_iter()
            .map(move |(wall, state)| {
                // Distinct job ids across the whole run: the low byte cycles,
                // so mix the index into the rest of the id.
                let mut raw = [id; 16];
                raw[..4].copy_from_slice(&i.to_be_bytes());
                OpBody::JobUpsert {
                    summary: JobSummary {
                        job_id: JobId(raw),
                        cmd_preview: "echo hi".into(),
                        runtime: "bash".into(),
                        submitter: me,
                        submitter_user: UserId([7u8; 32]),
                        executor: me,
                        state,
                        exit_code: None,
                        created_at_micros: 0,
                        started_at_micros: None,
                        finished_at_micros: None,
                        fanout: None,
                        updated_hlc: Hlc {
                            wall_micros: wall,
                            logical: 0,
                        },
                    },
                }
            })
            .collect::<Vec<_>>()
        })
        .collect();
    e.author_ops(bodies).unwrap();
}

/// Anti-entropy, in a loop, with no network: exactly what gossip does.
fn drain_into(from: &Arc<StateEngine>, to: &Arc<StateEngine>, max_rounds: usize) -> usize {
    let mut rounds = 0;
    loop {
        rounds += 1;
        assert!(rounds <= max_rounds, "anti-entropy is not converging");
        let have = to.version_vector();
        let (ops, more) = from.missing_for(&have).unwrap();
        if ops.is_empty() {
            assert!(!more, "the sender claims more history but sent none");
            return rounds;
        }
        let applied = to.ingest(ops).unwrap();
        assert!(
            applied > 0,
            "round {rounds} applied nothing — the receiver is parked on a hole \
             nobody vouched for"
        );
        if !more && to.version_vector() == from.version_vector() {
            return rounds;
        }
    }
}

#[test]
fn a_large_log_compacts_and_a_fresh_peer_still_converges() {
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    // ~100k ops: a third of a million-op year, and well past the point where
    // one sync batch could ever carry the checkpoint by luck.
    bulk_jobs(&e, node.node_id(), 33_000);

    let before = e.all_ops().unwrap().len();
    assert!(before > 99_000, "{before}");
    let expected = e.read(summarize);

    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, checkpoint_seq) = e.compact_self(0).unwrap();
    assert_eq!(
        dropped, 66_000,
        "two of every three upserts are dead weight"
    );
    assert!(checkpoint_seq > 0);

    let after = e.all_ops().unwrap().len();
    assert_eq!(after, before - 66_000 + 1);
    assert_eq!(e.read(summarize), expected, "compaction changed state");

    // The peer that has never heard of this mesh. It is behind on an author
    // with tens of thousands of surviving ops *above* a compacted stretch,
    // which is precisely the case that parks forever without P2's
    // checkpoint-aware batch.
    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    drain_into(&e, &b, 400);

    assert_eq!(b.read(summarize), expected);
    assert_eq!(b.version_vector(), e.version_vector());
}

#[test]
fn a_peer_behind_a_hole_is_not_served_the_same_batch_forever() {
    // The P2 regression in miniature, and fast enough to run every time.
    // Without the checkpoint in the batch the receiver parks on the first
    // hole, advances nothing, and is handed the identical ops on the next
    // tick — the quietest possible way for a mesh to stop converging.
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    // Enough surviving ops above the hole that the checkpoint cannot land
    // in the first batch by accident.
    bulk_jobs(&e, node.node_id(), 1_200);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, _) = e.compact_self(0).unwrap();
    assert_eq!(dropped, 2_400);

    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();

    // One batch is enough to unstick it: the checkpoint rides along.
    let have = b.version_vector();
    let (first, _) = e.missing_for(&have).unwrap();
    assert!(
        first
            .iter()
            .any(|o| matches!(o.decode_body(), Some(OpBody::Checkpoint { .. }))),
        "the first batch across a compacted stretch carried no checkpoint"
    );
    assert!(b.ingest(first).unwrap() > 0);

    drain_into(&e, &b, 40);
    assert_eq!(b.version_vector(), e.version_vector());
}

#[test]
fn a_peer_that_is_already_caught_up_is_not_sent_a_checkpoint() {
    // The batch carries the checkpoint because a peer needs it, not as a
    // habit: an author's newest checkpoint is otherwise just an op, and
    // re-sending it on every tick to every caught-up peer would make
    // compaction cost bandwidth forever.
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 12);
    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    drain_into(&e, &b, 40);
    assert_eq!(b.version_vector(), e.version_vector());

    let (ops, more) = e.missing_for(&b.version_vector()).unwrap();
    assert!(
        ops.is_empty(),
        "a caught-up peer was sent {} ops",
        ops.len()
    );
    assert!(!more);
}

#[test]
fn the_snapshot_vector_never_claims_past_a_hole_it_has_not_filled() {
    // `SnapshotProgress` replaced "take the max seq" for this exact reason:
    // the checkpoint rides *ahead* of the ops it explains, and claiming its
    // sequence number would tell the sender the joiner is caught up on an
    // author it has barely started.
    use mesh_core::membership::SnapshotProgress;
    use mesh_proto::{Checkpoint, SeqRange};

    let author = NodeId([3u8; 32]);
    let op = |seq: u64, body: &OpBody| OpEnvelope {
        author,
        seq,
        hlc: Hlc {
            wall_micros: seq,
            logical: 0,
        },
        body: postcard::to_stdvec(body).unwrap(),
        signature: vec![0u8; 64],
    };
    let checkpoint = OpBody::Checkpoint {
        checkpoint: Box::new(Checkpoint {
            through_seq: 900,
            subsumed: vec![SeqRange::new(2, 400)],
            ops_before: 900,
            ops_after: 500,
            horizon_secs: 0,
        }),
    };

    let mut p = SnapshotProgress::default();
    // The realistic instalment: the checkpoint out in front, then the ops
    // it does *not* subsume, and only some of them.
    p.observe(&[
        op(901, &checkpoint),
        op(1, &job(author, 1, JobState::Succeeded, Hlc::default())),
        op(401, &job(author, 2, JobState::Succeeded, Hlc::default())),
        op(402, &job(author, 3, JobState::Succeeded, Hlc::default())),
    ]);
    assert_eq!(
        p.vv(),
        vec![(author, 402)],
        "the vector claimed the checkpoint's seq and skipped 403..900"
    );

    // The rest arrives and the mark runs all the way up, checkpoint included.
    let rest: Vec<OpEnvelope> = (403..=900)
        .map(|s| op(s, &job(author, 4, JobState::Succeeded, Hlc::default())))
        .collect();
    p.observe(&rest);
    assert_eq!(p.vv(), vec![(author, 901)]);
}

// ---------------------------------------------------------------------------
// C0 P3 — counter compaction: decay windows past the horizon
// ---------------------------------------------------------------------------

use mesh_proto::{USAGE_DECAY_SECS, USAGE_WINDOW_SECS, UsagePartial, usage_window};

const HOUR_MICROS: u64 = USAGE_WINDOW_SECS * 1_000_000;

/// A year of hourly usage windows for `principals`, as a real busy node
/// writes them: several republished running totals inside each window, and
/// a fresh window every hour for 365 days.
fn synthetic_year(e: &Arc<StateEngine>, now_micros: u64, principals: &[UserId]) -> u64 {
    let first = usage_window(now_micros.saturating_sub(365 * 24 * HOUR_MICROS));
    let last = usage_window(now_micros);
    let mut bodies = Vec::new();
    for window in first..=last {
        for who in principals {
            // Three republished running totals per window per principal —
            // queued, mid-window, final — of which only the last is the
            // counter.
            for step in 1..=3u64 {
                bodies.push(OpBody::UsageAdd {
                    usage: Box::new(UsagePartial {
                        principal: *who,
                        window,
                        cpu_secs: window * 10 + step,
                        mem_mb_secs: window * 100 + step,
                        gpu_secs: step,
                        wall_secs: 0,
                        jobs: 1,
                    }),
                });
            }
        }
    }
    let n = bodies.len() as u64;
    e.author_ops(bodies).unwrap();
    n
}

#[test]
fn a_synthetic_year_of_usage_counters_stays_bounded() {
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();

    let now = mesh_proto::now_micros();
    let principals = [UserId([1u8; 32]), UserId([2u8; 32]), UserId([3u8; 32])];
    let written = synthetic_year(&e, now, &principals);
    assert!(written > 26_000, "a year of hourly windows: {written}");

    // Before the pass: every op is in the log, and most of them are for
    // windows nobody will ever read again.
    assert!(e.all_ops().unwrap().len() as u64 > written);

    let (dropped, _) = e.compact_self(0).unwrap();

    // What survives is one op per live window per principal — a week of
    // hours, not a year of them — plus the checkpoint and the membership op.
    let live_windows = USAGE_DECAY_SECS / USAGE_WINDOW_SECS;
    let survivors = e
        .all_ops()
        .unwrap()
        .iter()
        .filter(|o| matches!(o.decode_body(), Some(OpBody::UsageAdd { .. })))
        .count() as u64;
    assert!(
        survivors <= (live_windows + 2) * principals.len() as u64,
        "a year of counters did not stay bounded: {survivors} usage ops left, \
         {live_windows} live windows × {} principals",
        principals.len()
    );
    assert_eq!(dropped, written - survivors);

    // And the bound is what the *horizon* says, not what the retention
    // knob says: this pass ran with a zero retention horizon, but a
    // thirty-day one would drop exactly the same decayed windows.
    let (dir_c, node_c, genesis_c) = setup();
    let c = StateEngine::open(dir_c.path(), node_c.clone(), genesis_c.clone()).unwrap();
    c.author_op(OpBody::MemberAdd {
        member: member_info(&node_c, "gamma"),
        invite: None,
    })
    .unwrap();
    synthetic_year(&c, now, &principals);
    let plan = c.compaction_plan(30 * 24 * 3600).unwrap();
    assert!(
        plan.decayed > 20_000,
        "the retention horizon held back decayed windows: {} decayed",
        plan.decayed
    );
}

#[test]
fn compacting_decayed_windows_changes_no_answer() {
    // The safety argument for `Compaction::Decaying`, stated as a test.
    // Dropping the *survivor* of a key would normally diverge two nodes.
    // It cannot here, because every reader already filters the window out
    // by a clock and a mesh-wide constant — so a node that compacts and a
    // node that never does answer identically.
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    let now = mesh_proto::now_micros();
    let principals = [UserId([1u8; 32]), UserId([2u8; 32])];
    synthetic_year(&e, now, &principals);

    let before: Vec<(u64, u64, u64)> = principals
        .iter()
        .map(|p| e.read(|s| s.usage_of(p, now)))
        .collect();
    assert!(before[0].0 > 0, "a year of usage summed to nothing");

    e.compact_self(0).unwrap();
    let after: Vec<(u64, u64, u64)> = principals
        .iter()
        .map(|p| e.read(|s| s.usage_of(p, now)))
        .collect();
    assert_eq!(before, after, "compaction changed a principal's usage");

    // And a peer that only ever sees the compacted log agrees.
    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    drain_into(&e, &b, 400);
    let peer: Vec<(u64, u64, u64)> = principals
        .iter()
        .map(|p| b.read(|s| s.usage_of(p, now)))
        .collect();
    assert_eq!(before, peer, "a compacted peer disagrees about usage");
}

#[test]
fn a_usage_counter_never_moves_backwards() {
    // Grow-only per component is what makes this safe to re-send: anti-
    // entropy promises delivery, not order, and a delta would need both.
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    let now = mesh_proto::now_micros();
    let who = UserId([9u8; 32]);
    let window = usage_window(now);

    e.publish_usage(UsagePartial {
        principal: who,
        window,
        cpu_secs: 500,
        mem_mb_secs: 40,
        gpu_secs: 2,
        wall_secs: 0,
        jobs: 1,
    })
    .unwrap();
    // A stale total arriving late — a re-send from before the last publish.
    e.publish_usage(UsagePartial {
        principal: who,
        window,
        cpu_secs: 100,
        mem_mb_secs: 10,
        gpu_secs: 0,
        wall_secs: 0,
        jobs: 1,
    })
    .unwrap();
    assert_eq!(e.read(|s| s.usage_of(&who, now)), (500, 40, 2));

    // A genuine increase does move it.
    e.publish_usage(UsagePartial {
        principal: who,
        window,
        cpu_secs: 900,
        mem_mb_secs: 40,
        gpu_secs: 3,
        wall_secs: 0,
        jobs: 1,
    })
    .unwrap();
    assert_eq!(e.read(|s| s.usage_of(&who, now)), (900, 40, 3));
}

#[test]
fn expired_windows_are_released_from_memory_too() {
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    let now = mesh_proto::now_micros();
    synthetic_year(&e, now, &[UserId([4u8; 32])]);

    let held = e.read(|s| s.usage.len());
    let live_windows = (USAGE_DECAY_SECS / USAGE_WINDOW_SECS) as usize;
    assert!(held > live_windows * 2, "{held}");

    let released = e.expire_usage(now);
    assert!(released > 0);
    let after = e.read(|s| s.usage.len());
    assert!(after <= live_windows + 2, "{after} windows still resident");
    // Releasing memory is not a merge rule: nobody's answer changed.
    assert!(e.read(|s| s.usage_of(&UserId([4u8; 32]), now)).0 > 0);
}

#[test]
fn the_file_gives_the_space_back() {
    // redb frees pages on delete but leaves the file at its high-water
    // mark, so without an explicit reclaim `df` would show a compaction
    // that recovered nothing — which is how an operator concludes the
    // feature does not work.
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: member_info(&node, "alpha"),
        invite: None,
    })
    .unwrap();
    bulk_jobs(&e, node.node_id(), 12_000);

    let before = e.op_stats(0).unwrap().bytes_on_disk;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, _) = e.compact_self(0).unwrap();
    assert_eq!(dropped, 24_000);
    let after = e.op_stats(0).unwrap().bytes_on_disk;
    assert!(
        after < before,
        "the log dropped {dropped} ops and the file did not shrink \
         ({before} -> {after} bytes)"
    );
}

#[test]
fn a_batch_of_checkpoints_still_fits_in_a_frame() {
    // The bound on P2's prepend. A node coming back from a long outage is
    // behind on every author at once, and if each compacted author's
    // checkpoint rode along unconditionally the batch would eventually be
    // one the codec refuses to write — which is not slow, it is stuck.
    let founder_user = Keypair::generate().unwrap();
    let founder = Keypair::generate().unwrap();
    let genesis = Genesis::create("c0-wide", &founder_user, founder.node_id()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let hub = StateEngine::open(dir.path(), founder.clone(), genesis.clone()).unwrap();
    hub.author_op(OpBody::MemberAdd {
        member: member_info(&founder, "founder"),
        invite: None,
    })
    .unwrap();

    // Forty authors, each with its own compacted log, all merged into one
    // hub — which is what any node in a forty-node mesh looks like.
    let mut peers = Vec::new();
    for i in 0..40 {
        let key = Keypair::generate().unwrap();
        hub.author_op(OpBody::MemberAdd {
            member: member_info(&key, &format!("n{i}")),
            invite: None,
        })
        .unwrap();
        peers.push(key);
    }
    for (i, key) in peers.iter().enumerate() {
        let d = tempfile::tempdir().unwrap();
        let e = StateEngine::open(d.path(), key.clone(), genesis.clone()).unwrap();
        e.ingest(hub.all_ops().unwrap()).unwrap();
        bulk_jobs(&e, key.node_id(), 60 + i as u32);
        std::thread::sleep(std::time::Duration::from_millis(2));
        e.compact_self(0).unwrap();
        // Only what survived reaches the hub — the peer's own compacted log.
        hub.ingest(e.load_author_ops(key.node_id())).unwrap();
        std::mem::forget(d);
    }

    // The newcomer, behind on all forty compacted authors at once. Driven
    // the way `membership::pull_snapshot` drives it: instalments buffered,
    // the vector advanced by contiguity, one ingest at the end.
    let mut progress = mesh_core::membership::SnapshotProgress::default();
    let mut buffered: Vec<OpEnvelope> = Vec::new();
    let mut rounds = 0;
    loop {
        rounds += 1;
        assert!(rounds < 400, "the snapshot is not draining");
        let (ops, more) = hub.missing_for(&progress.vv()).unwrap();
        if ops.is_empty() {
            assert!(!more, "the sender claims more history but sent none");
            break;
        }
        // The property under test: every instalment is a frame that can
        // actually be written, checkpoints and all.
        let encoded = postcard::to_stdvec(&ops).unwrap().len();
        assert!(
            encoded <= mesh_proto::MAX_FRAME_LEN as usize,
            "a batch of {} ops is {encoded} bytes — larger than a frame",
            ops.len()
        );
        progress.observe(&ops);
        buffered.extend(ops);
        if !more {
            break;
        }
    }
    assert!(rounds > 1, "the mesh should not have fit in one instalment");

    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    b.ingest(buffered).unwrap();
    assert_eq!(b.version_vector(), hub.version_vector());
    assert_eq!(b.read(|s| s.members.len()), hub.read(|s| s.members.len()));
    assert_eq!(b.read(|s| s.jobs.len()), hub.read(|s| s.jobs.len()));
}

#[test]
fn a_batch_whose_first_present_op_is_above_a_hole_still_lands() {
    // The subtle one. The merge's fixpoint loop stops when a pass applies
    // nothing — that is what keeps a batch of genuinely un-appliable ops
    // from spinning. A checkpoint understood *before* the gapless test
    // moves a watermark without applying anything, so unless that counts as
    // progress, a batch whose lowest present sequence number sits above a
    // compacted run is judged stuck on the first pass and thrown away
    // whole.
    //
    // It is exactly what a peer sees when an author compacted its earliest
    // ops, which is the normal case for any node that has been running a
    // while.
    let (dir, node, genesis) = setup();
    let e = StateEngine::open(dir.path(), node.clone(), genesis.clone()).unwrap();
    let me = node.node_id();

    // Seq 1..=6: three jobs, two dead upserts each. After the pass the
    // survivors are 3 and 6 — nothing at seq 1 or 2 at all.
    bulk_jobs(&e, me, 3);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (dropped, _) = e.compact_self(0).unwrap();
    assert_eq!(dropped, 6);
    let surviving = e.load_author_ops(me);
    assert_eq!(
        surviving.first().map(|o| o.seq),
        Some(3),
        "expected a hole at 1..2"
    );

    // The founder's own membership op comes from genesis, so this author is
    // authorized without needing any op of its own to land first.
    let dir_b = tempfile::tempdir().unwrap();
    let b = StateEngine::open(dir_b.path(), Keypair::generate().unwrap(), genesis.clone()).unwrap();
    let applied = b.ingest(surviving).unwrap();
    assert!(applied > 0, "the whole batch was judged stuck and dropped");
    assert_eq!(b.version_vector(), e.version_vector());
    assert_eq!(b.read(|s| s.jobs.len()), 3);
}

#[test]
fn a_pre_1_7_peer_is_recorded_as_unable_to_follow_a_compacted_author() {
    // The compatibility corner. A 1.6 peer has no variant for `Checkpoint`,
    // so the V1 downgrade withholds the only op that could explain the
    // holes — and the ops themselves are gone. It cannot follow this author
    // past the first pruned seq, ever.
    //
    // `legacy_cut` is the existing name for exactly that condition, and
    // recording it there is the difference between a peer the operator can
    // see is stuck and one that parks in silence.
    let (dir, node, genesis) = setup();
    let e = seeded(dir.path(), &node, &genesis, 8);
    assert_eq!(
        e.read(|s| s.legacy_cut.get(&node.node_id()).copied()),
        None,
        "nothing yet: seeded ops are all pre-1.7 variants"
    );

    std::thread::sleep(std::time::Duration::from_millis(5));
    e.compact_self(0).unwrap();

    let cut = e
        .read(|s| s.legacy_cut.get(&node.node_id()).copied())
        .expect("compaction must record the legacy cut");
    let lowest = e
        .read(|s| s.pruned.get(&node.node_id()).map(|p| p.ranges()[0].lo))
        .unwrap();
    assert_eq!(cut, lowest);

    // And a legacy joiner is refused with a sentence rather than admitted
    // into a mesh whose history it cannot read.
    assert!(
        mesh_core::membership::legacy_join_refusal(&e).is_some(),
        "a pre-1.7 joiner was not refused by a compacted member"
    );
}
