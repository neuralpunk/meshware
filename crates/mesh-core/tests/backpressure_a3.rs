//! M31 A3 — backpressure, at the one place an op becomes an op.
//!
//! A1 governs the steady state; this governs the bad day. The unit tests in
//! `backpressure.rs` cover the arithmetic — burst, refill, pause, expiry.
//! These cover the two things only a real engine can answer:
//!
//! 1. **The refusal happens where the op was produced**, so a loop is a
//!    caller's error rather than every other machine's problem — and the
//!    log does not grow while it is happening.
//! 2. **The exemptions are real.** A list of strings compared against
//!    `OpBody::kind()` is exactly the kind of control that stops working
//!    silently when somebody renames a variant, so the list is checked
//!    against ops actually constructed here.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mesh_core::backpressure::{BURST_FACTOR, DEFAULT_ENTITY_OPS_PER_MINUTE};

/// Ops one runaway entity attempts in the loop test. Comfortably past the
/// burst allowance, so what gets through is the allowance and whatever the
/// sustained rate earned back while the test ran.
const LOOP_ATTEMPTS: u32 = DEFAULT_ENTITY_OPS_PER_MINUTE * 20;
use mesh_core::error::CoreError;
use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::{KEEPALIVE_KINDS, StateEngine};
use mesh_proto::{
    Checkpoint, GrantRevocation, JobState, JobSummary, MemberInfo, NodeRevocation, OpBody,
    UserRevocation,
};

fn engine(dir: &std::path::Path) -> (Arc<StateEngine>, Keypair) {
    let founder = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("a3-mesh", &founder, node.node_id()).unwrap();
    let e = StateEngine::open(dir, node.clone(), genesis).unwrap();
    // The limiter is off until a daemon turns it on — so these tests turn
    // it on the same way `Node::start` does, rather than relying on a
    // default that offline tools deliberately do not get.
    e.rate_guard().set_limit(DEFAULT_ENTITY_OPS_PER_MINUTE);
    e.author_op(OpBody::MemberAdd {
        member: MemberInfo {
            node_id: node.node_id(),
            name: "alpha".into(),
            addrs: vec!["127.0.0.1:7777".into()],
            tags: vec![],
            slots: 8,
            joined_at_micros: mesh_proto::now_micros(),
        },
        invite: None,
    })
    .unwrap();
    (e, node)
}

/// One job's ops, over and over — which is what a looping workflow looks
/// like from the log's point of view.
fn job_op(engine: &StateEngine, node: &Keypair, seed: u8, state: JobState) -> OpBody {
    let _ = engine;
    OpBody::JobUpsert {
        summary: JobSummary {
            job_id: mesh_proto::JobId([seed; 16]),
            cmd_preview: "loop".into(),
            runtime: "bash".into(),
            submitter: node.node_id(),
            submitter_user: mesh_proto::UserId([7; 32]),
            executor: node.node_id(),
            state,
            exit_code: None,
            created_at_micros: 1_000,
            started_at_micros: None,
            finished_at_micros: None,
            fanout: None,
            updated_hlc: mesh_proto::Hlc {
                wall_micros: mesh_proto::now_micros(),
                logical: 0,
            },
        },
    }
}

/// The exit demo, at engine scale: a loop is refused **by name**, at the
/// node that produced it, and the log stops growing while it happens.
#[test]
fn a_loop_is_refused_by_name_and_the_log_stops_growing() {
    let dir = tempfile::tempdir().unwrap();
    let (e, node) = engine(dir.path());

    let before = e.all_ops().unwrap().len();
    let mut refusal = None;
    let mut authored = 0usize;
    // Far past the burst allowance for one entity.
    for _ in 0..LOOP_ATTEMPTS {
        match e.author_op(job_op(&e, &node, 1, JobState::Running)) {
            Ok(_) => authored += 1,
            Err(err) => {
                refusal.get_or_insert(err);
            }
        }
    }

    let Some(CoreError::Backpressure { entity, message }) = refusal else {
        panic!("a sustained loop was never refused after {authored} ops");
    };
    // Named, so the bug gets fixed rather than absorbed.
    assert!(entity.starts_with("job:"), "{entity}");
    assert!(message.contains(&entity), "{message}");
    assert!(
        message.contains(&DEFAULT_ENTITY_OPS_PER_MINUTE.to_string()),
        "the limit is not in the message: {message}"
    );

    // And the log is bounded by the **burst allowance**, not by the loop:
    // a runaway spends its instant credit once and then earns ops at the
    // sustained rate, which on a wall clock that has not moved is none.
    // Plus whatever the sustained rate earned back while the loop ran:
    // this is a real clock, and four ops a second is the *point* of the
    // limit rather than an inconvenience to the assertion.
    let burst = (f64::from(DEFAULT_ENTITY_OPS_PER_MINUTE) * BURST_FACTOR) as usize;
    let grew = e.all_ops().unwrap().len() - before;
    let ceiling = burst + DEFAULT_ENTITY_OPS_PER_MINUTE as usize; // + a minute of refill
    assert!(
        grew <= ceiling,
        "the log grew by {grew} ops against a burst allowance of {burst}"
    );
    assert!(
        grew * 4 <= LOOP_ATTEMPTS as usize,
        "the limiter let through {grew} of {LOOP_ATTEMPTS} attempts — most of a loop must \
         be stopped"
    );

    // The offender is reportable — this is what `doctor` prints.
    let pressured = e.rate_guard().pressured(mesh_proto::now_micros());
    assert_eq!(pressured.len(), 1, "{pressured:?}");
    assert!(pressured[0].refused_total > 0);
    assert!(pressured[0].observed_per_minute > DEFAULT_ENTITY_OPS_PER_MINUTE);
}

/// **The regression that gets the feature disabled.** Real work is many
/// entities, not one: a fan-out authoring an op for each of two hundred
/// jobs is two hundred entities with one op each, and none of it is a loop.
#[test]
fn a_wide_fanout_is_not_a_loop_and_is_never_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (e, node) = engine(dir.path());
    for seed in 0..200u8 {
        e.author_op(job_op(&e, &node, seed, JobState::Running))
            .unwrap_or_else(|err| panic!("job {seed} of a fan-out was refused: {err}"));
    }
    assert!(
        e.rate_guard()
            .pressured(mesh_proto::now_micros())
            .is_empty(),
        "a fan-out must not register as pressure"
    );
}

/// A control that can make a join fail is worse than the flood it prevents.
/// Every exempt kind is exercised here as a real op, so the day somebody
/// renames a variant this test fails rather than the exemption.
#[test]
fn keepalive_kinds_are_real_kinds_and_are_never_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (_e, node) = engine(dir.path());
    let user = mesh_proto::UserId([7; 32]);

    let bodies: Vec<OpBody> = vec![
        OpBody::MemberAdd {
            member: MemberInfo {
                node_id: node.node_id(),
                name: "alpha".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
                joined_at_micros: 0,
            },
            invite: None,
        },
        OpBody::MemberUpdate {
            node_id: node.node_id(),
            name: "alpha".into(),
            addrs: vec![],
            tags: vec![],
            slots: 1,
        },
        OpBody::MemberLeft {
            node_id: node.node_id(),
        },
        OpBody::MemberRevoke {
            revocation: NodeRevocation {
                node_id: node.node_id(),
                reason: String::new(),
                by: user,
                at_micros: 0,
                signature: Vec::new(),
            },
        },
        OpBody::UserRevoke {
            revocation: UserRevocation {
                user_id: user,
                reason: String::new(),
                revoked_by: user,
                at_micros: 0,
                signature: Vec::new(),
            },
        },
        OpBody::GrantRevoke {
            revocation: GrantRevocation {
                grant_id: mesh_proto::GrantId([1; 8]),
                revoked_by: user,
                at_micros: 0,
                signature: Vec::new(),
            },
        },
        OpBody::Checkpoint {
            checkpoint: Box::new(Checkpoint {
                through_seq: 1,
                subsumed: Vec::new(),
                ops_before: 1,
                ops_after: 1,
                horizon_secs: 3600,
            }),
        },
        OpBody::RetentionPut {
            policy: Box::new(mesh_proto::RetentionPolicy::default()),
        },
    ];

    let named: Vec<&str> = bodies.iter().map(|b| b.kind()).collect();
    for kind in KEEPALIVE_KINDS {
        assert!(
            named.contains(kind),
            "`{kind}` is exempt from the rate limiter and is not a kind any op returns — the \
             exemption silently does nothing"
        );
    }
    for kind in &named {
        assert!(
            KEEPALIVE_KINDS.contains(kind),
            "`{kind}` is constructed here but is not in the exempt list"
        );
    }

    // And they are genuinely exempt: a membership op authored far past any
    // budget still goes through, because refusing one turns a growth
    // problem into an availability problem.
    let dir2 = tempfile::tempdir().unwrap();
    let (e2, node2) = engine(dir2.path());
    for i in 0..(DEFAULT_ENTITY_OPS_PER_MINUTE * 8) {
        e2.author_op(OpBody::MemberUpdate {
            node_id: node2.node_id(),
            name: format!("alpha-{i}"),
            addrs: vec![],
            tags: vec![],
            slots: 1,
        })
        .unwrap_or_else(|err| panic!("a keepalive op was refused at {i}: {err}"));
    }
}

/// A batch is refused whole, never in part: half a batch is a state nothing
/// downstream models.
#[test]
fn a_batch_over_the_limit_is_refused_whole() {
    let dir = tempfile::tempdir().unwrap();
    let (e, node) = engine(dir.path());
    let before = e.all_ops().unwrap().len();
    let batch: Vec<OpBody> = (0..(DEFAULT_ENTITY_OPS_PER_MINUTE * 8))
        .map(|_| job_op(&e, &node, 3, JobState::Running))
        .collect();
    let err = e
        .author_ops(batch)
        .expect_err("an over-limit batch was accepted");
    assert!(matches!(err, CoreError::Backpressure { .. }), "{err:?}");
    assert_eq!(
        e.all_ops().unwrap().len(),
        before,
        "a refused batch must leave no ops behind"
    );
}

/// **A rate limit that silently drops is indistinguishable from a bug.**
///
/// So the refusal reaches `meshnode audit`, once per episode — the count
/// rides on the entry, because `AuditAppend` is `Grow` and never compacts,
/// and one entry per refused op would let the control become the incident.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_is_audited_once_with_the_count_on_it() {
    let node = mesh_testkit::spawn_founder("audited", "a3-mesh").await;
    let engine = node.node.engine();
    let me = engine.node_id();

    let mut refused = 0usize;
    for _ in 0..(DEFAULT_ENTITY_OPS_PER_MINUTE * 8) {
        if engine
            .author_op(OpBody::JobUpsert {
                summary: JobSummary {
                    job_id: mesh_proto::JobId([9; 16]),
                    cmd_preview: "loop".into(),
                    runtime: "bash".into(),
                    submitter: me,
                    submitter_user: mesh_proto::UserId([7; 32]),
                    executor: me,
                    state: JobState::Running,
                    exit_code: None,
                    created_at_micros: 1_000,
                    started_at_micros: None,
                    finished_at_micros: None,
                    fanout: None,
                    updated_hlc: mesh_proto::Hlc {
                        wall_micros: mesh_proto::now_micros(),
                        logical: 0,
                    },
                },
            })
            .is_err()
        {
            refused += 1;
        }
    }
    assert!(refused > 0, "nothing was refused");

    // Exactly what the periodic tick runs — not a second implementation of
    // it that would agree on the day it was written.
    assert_eq!(node.node.drain_backpressure(), 1, "one episode, one entry");
    assert_eq!(
        node.node.drain_backpressure(),
        0,
        "an episode must be audited once, not once per drain"
    );

    let resp = mesh_testkit::ipc_as(
        &node,
        mesh_testkit::founder_user_key(&node),
        mesh_proto::IpcRequest::AuditQuery {
            user: None,
            action: Some("state.backpressure".into()),
            since_micros: None,
            limit: 100,
        },
    )
    .await;
    let mesh_proto::IpcResponse::Audit(rows) = resp else {
        panic!("expected audit rows, got {resp:?}");
    };
    assert_eq!(rows.len(), 1, "{rows:?}");
    let entry = &rows[0];
    assert!(entry.target.starts_with("job:"), "{}", entry.target);
    assert_eq!(entry.outcome, "refused");
    // The count is on the entry, so saying it once loses nothing.
    assert!(
        entry.detail.contains(&refused.to_string()) || entry.detail.contains("refused so far"),
        "{}",
        entry.detail
    );
    assert!(
        entry
            .detail
            .contains(&format!("limit of {DEFAULT_ENTITY_OPS_PER_MINUTE}")),
        "{}",
        entry.detail
    );
}

/// **The escape hatch, and it has to work.**
///
/// A control that can refuse legitimate work and cannot be switched off is a
/// control an operator routes around by other means, at 3 a.m., under
/// pressure. `[state] op_rate_per_minute = 0` turns it off, and the same
/// setting raised turns it up.
#[test]
fn the_limiter_can_be_turned_off_and_turned_up() {
    let dir = tempfile::tempdir().unwrap();
    let (e, node) = engine(dir.path());

    e.rate_guard().set_limit(0);
    assert!(!e.rate_guard().is_on());
    for i in 0..(DEFAULT_ENTITY_OPS_PER_MINUTE * 8) {
        e.author_op(job_op(&e, &node, 2, JobState::Running))
            .unwrap_or_else(|err| panic!("op {i} was refused with the limiter off: {err}"));
    }
    assert!(
        e.rate_guard()
            .pressured(mesh_proto::now_micros())
            .is_empty()
    );

    // And back on, at a number an operator chose.
    e.rate_guard().set_limit(10);
    assert_eq!(e.rate_guard().limit(), 10);
}

/// A tool that opens a data dir directly — `restore`, a backup, a repair —
/// is not a daemon, and must not be rate limited by the config of whichever
/// node's directory it happens to be pointed at.
#[test]
fn opening_a_data_dir_outside_a_daemon_is_never_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let founder = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("a3-mesh", &founder, node.node_id()).unwrap();
    let e = StateEngine::open(dir.path(), node.clone(), genesis).unwrap();
    assert!(
        !e.rate_guard().is_on(),
        "the limiter must be off until a daemon turns it on"
    );
    for i in 0..(DEFAULT_ENTITY_OPS_PER_MINUTE * 8) {
        e.author_op(job_op(&e, &node, 5, JobState::Running))
            .unwrap_or_else(|err| panic!("op {i} was refused outside a daemon: {err}"));
    }
}

/// SEC-6: an audit row is never refused, however busy the node — the
/// limiter budgets audit by kind, and a CI pipeline submitting faster than
/// four jobs a second used to lose rows past the burst. What bounds the
/// trail instead is folding: a stranger minting a fresh key per request
/// buys one full row and one summary a minute.
#[test]
fn audit_rows_are_never_refused_and_strangers_fold() {
    use mesh_core::state::UNLIMITED_KINDS;

    let dir = tempfile::tempdir().unwrap();
    let founder = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("a3-audit", &founder, node.node_id()).unwrap();
    let e = StateEngine::open(dir.path(), node.clone(), genesis).unwrap();
    e.rate_guard().set_limit(DEFAULT_ENTITY_OPS_PER_MINUTE);
    let rows = |e: &StateEngine| e.read(|s| s.audit.len());

    // A real kind, or the exemption is a string that matches nothing.
    let body = OpBody::AuditAppend {
        entry: mesh_proto::AuditEntry {
            audit_id: mesh_proto::AuditId::generate(),
            at_micros: 0,
            user: founder.user_id(),
            username: String::new(),
            action: String::new(),
            target: String::new(),
            node: node.node_id(),
            outcome: String::new(),
            detail: String::new(),
            request_sig: Vec::new(),
        },
    };
    assert!(UNLIMITED_KINDS.contains(&body.kind()));

    // A thousand successful runs by an admitted user: a thousand rows, well
    // past the burst that used to swallow the rest.
    let jobs = 1000;
    assert!(f64::from(jobs) > f64::from(DEFAULT_ENTITY_OPS_PER_MINUTE) * BURST_FACTOR);
    for i in 0..jobs {
        mesh_core::audit::record(
            &e,
            founder.user_id(),
            "job.run",
            &format!("job-{i}"),
            "ok",
            "",
            &[],
        );
    }
    assert_eq!(rows(&e), jobs as usize, "an audit row was dropped");

    // A thousand requests from a thousand keys nobody admitted: one row now…
    let before = rows(&e);
    for _ in 0..1000 {
        let stranger = Keypair::generate().unwrap().user_id();
        mesh_core::audit::record(&e, stranger, "nodes", "", "denied: unknown user", "", &[]);
    }
    assert_eq!(rows(&e) - before, 1, "strangers were audited one by one");
    // …and one summary when the window closes, naming some of them.
    mesh_core::audit::flush_folded(&e, true);
    let summary = e.read(|s| s.audit.values().last().cloned()).unwrap();
    assert_eq!(rows(&e) - before, 2);
    assert!(summary.outcome.contains("999"), "{}", summary.outcome);
    assert!(
        summary.detail.contains("not admitted"),
        "{}",
        summary.detail
    );

    // An admitted user denied over and over folds the same way, and does
    // not share a window with the strangers.
    let before = rows(&e);
    for _ in 0..100 {
        mesh_core::audit::record(
            &e,
            founder.user_id(),
            "secret.set",
            "x",
            "denied: no",
            "",
            &[],
        );
    }
    mesh_core::audit::flush_folded(&e, true);
    assert_eq!(rows(&e) - before, 2);
}
