//! ENG-3 — the replicated log stays bounded on **every** node.
//!
//! Compaction used to stop at the author: the author pruned its own log and
//! every other node kept every op it had ever received, and handed all of
//! it to each new joiner. Fan-out and gang plans never left anyone's log,
//! and every node held every audit row in memory for ever.
//!
//! The property under test is the one C0 already holds the author to, now
//! held to everyone: the log gets smaller **and nothing changes** — a peer
//! that pruned, a joiner that learnt the mesh from that peer, and a peer
//! that restarted all materialise the same state, and none of them parks.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::{MeshState, StateEngine};
use mesh_proto::{
    AuditEntry, AuditId, FanId, FanoutControls, FanoutPlanRecord, Hlc, JobId, JobPriority,
    JobState, JobSummary, MemberInfo, NodeId, OpBody, OpEnvelope, RetentionPolicy, RetentionTable,
    RetentionWindow, SignedIpc, UserId,
};

const DAY: u64 = 86_400 * 1_000_000;

struct Mesh {
    _dirs: Vec<tempfile::TempDir>,
    founder: Keypair,
    genesis: Genesis,
    keys: Vec<Keypair>,
}

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

fn mesh(nodes: usize) -> Mesh {
    let founder = Keypair::generate().unwrap();
    let keys: Vec<Keypair> = (0..nodes).map(|_| Keypair::generate().unwrap()).collect();
    let genesis = Genesis::create("eng3-mesh", &founder, keys[0].node_id()).unwrap();
    Mesh {
        _dirs: Vec::new(),
        founder,
        genesis,
        keys,
    }
}

fn open(m: &mut Mesh, i: usize) -> (Arc<StateEngine>, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    m._dirs.push(dir);
    let e = StateEngine::open(&path, m.keys[i].clone(), m.genesis.clone()).unwrap();
    (e, path)
}

/// Anti-entropy, in a loop, with no network: what gossip does.
fn sync(from: &Arc<StateEngine>, to: &Arc<StateEngine>) {
    for _ in 0..64 {
        let (ops, more) = from.missing_for(&to.version_vector()).unwrap();
        if ops.is_empty() {
            return;
        }
        to.ingest(ops).unwrap();
        if !more {
            return;
        }
    }
    panic!("anti-entropy is not converging");
}

fn policy(runs_days: u32, audit_days: u32) -> RetentionPolicy {
    let mut p = RetentionPolicy::default();
    p.set(
        RetentionTable::Runs,
        RetentionWindow {
            days: runs_days,
            floor_days: 0,
        },
    );
    p.set(
        RetentionTable::Audit,
        RetentionWindow {
            days: audit_days,
            floor_days: mesh_proto::AUDIT_FLOOR_DAYS,
        },
    );
    p
}

fn plan(group: FanId, me: NodeId, founder: &Keypair, target: NodeId) -> OpBody {
    OpBody::FanoutPlan {
        plan: Box::new(FanoutPlanRecord {
            group,
            payload: mesh_proto::JobPayload::Command("uptime".into()),
            env: vec![("X".into(), "y".repeat(200))],
            timeout_secs: None,
            priority: JobPriority::Normal,
            submitter: me,
            submitter_user: founder.user_id(),
            auth: SignedIpc {
                user: founder.user_id(),
                at_micros: mesh_proto::now_micros(),
                nonce: 1,
                payload: vec![7; 300],
                signature: vec![0; 64],
            },
            targets: vec![mesh_proto::PlannedTarget {
                job_id: JobId::generate(),
                node_id: target,
                node_name: "target".into(),
            }],
            controls: FanoutControls::default(),
            created_at_micros: mesh_proto::now_micros(),
        }),
    }
}

/// One busy stretch on the author: finished jobs, closed fan-outs that
/// nobody adopted, and an audit row per job.
fn volume(e: &Arc<StateEngine>, founder: &Keypair, target: NodeId, jobs: u32) {
    let me = e.node_id();
    let now = mesh_proto::now_micros();
    let mut bodies = Vec::new();
    for _ in 0..jobs {
        let job_id = JobId::generate();
        for (step, state) in [JobState::Queued, JobState::Running, JobState::Succeeded]
            .into_iter()
            .enumerate()
        {
            bodies.push(OpBody::JobUpsert {
                summary: JobSummary {
                    job_id,
                    cmd_preview: "uptime".into(),
                    runtime: "bash".into(),
                    submitter: me,
                    submitter_user: founder.user_id(),
                    executor: me,
                    state,
                    exit_code: Some(0),
                    created_at_micros: now,
                    started_at_micros: Some(now),
                    finished_at_micros: state.is_terminal().then_some(now),
                    fanout: None,
                    updated_hlc: Hlc {
                        wall_micros: now + step as u64,
                        logical: 0,
                    },
                },
            });
        }
        bodies.push(OpBody::AuditAppend {
            entry: AuditEntry {
                audit_id: AuditId::generate(),
                at_micros: now,
                user: founder.user_id(),
                username: "ada".into(),
                action: "job.run".into(),
                target: job_id.to_string(),
                node: me,
                outcome: "ok".into(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        });
    }
    e.author_ops(bodies).unwrap();
    for _ in 0..jobs / 4 {
        let group = FanId::generate();
        e.author_op(plan(group, me, founder, target)).unwrap();
        e.author_op(OpBody::FanoutClosed {
            group,
            succeeded: 1,
            failed: 0,
            skipped: 0,
        })
        .unwrap();
    }
}

/// What must read the same everywhere.
fn summarize(s: &MeshState) -> String {
    let mut out = String::new();
    for (id, m) in &s.members {
        out.push_str(&format!("member {} {}\n", id.short(), m.info.name));
    }
    out.push_str(&format!(
        "jobs {} fanouts {} gangs {} audit {}\n",
        s.jobs.len(),
        s.fanouts.len(),
        s.gangs.len(),
        s.audit.len()
    ));
    for (id, j) in &s.jobs {
        out.push_str(&format!("job {} {:?}\n", id.short(), j.state));
    }
    out
}

fn summary(e: &Arc<StateEngine>) -> String {
    e.read(summarize)
}

fn applied(e: &Arc<StateEngine>, author: &NodeId) -> u64 {
    e.read(|s| s.applied.get(author).copied().unwrap_or(0))
}

fn settle(engines: &[&Arc<StateEngine>], at: u64) {
    for e in engines {
        e.expire_closed(at);
    }
}

#[test]
fn every_node_drops_what_the_author_retired_and_stays_bounded() {
    let mut m = mesh(4);
    let (a, _) = open(&mut m, 0);
    let (b, b_dir) = open(&mut m, 1);
    let (c, _) = open(&mut m, 2);
    let names = ["alpha", "beta", "gamma", "delta"];
    for (i, key) in m.keys.iter().enumerate() {
        a.author_op(OpBody::MemberAdd {
            member: member_info(key, names[i]),
            invite: None,
        })
        .unwrap();
    }
    let signed = mesh_core::retention::sign(&m.founder, policy(1, 365), mesh_proto::now_micros());
    mesh_core::retention::put(&a, signed).unwrap();
    let alpha = a.node_id();
    let beta = b.node_id();

    // Beta writes a little of its own, so its stream is in play too.
    sync(&a, &b);
    b.author_op(OpBody::MemberUpdate {
        node_id: beta,
        name: "beta".into(),
        addrs: vec!["127.0.0.1:7778".into()],
        tags: vec![],
        slots: 4,
    })
    .unwrap();

    let mut settled_counts: Vec<u64> = Vec::new();
    for round in 1..=3u64 {
        volume(&a, &m.founder, beta, 200);
        sync(&a, &b);
        sync(&b, &a);
        sync(&b, &c);
        let full = a.stored_ops_by(&alpha).unwrap();
        assert_eq!(b.stored_ops_by(&alpha).unwrap(), full);
        assert_eq!(c.stored_ops_by(&alpha).unwrap(), full);

        // A year and a bit later — past the one-day runs window and the
        // year-long audit window — the author retires what it wrote.
        let later = mesh_proto::now_micros() + (400 + round) * DAY;
        let (dropped, _) = a.compact_self_at(0, later).unwrap();
        assert!(dropped > 0, "round {round}: nothing retired");
        let kept = a.stored_ops_by(&alpha).unwrap();
        assert!(
            kept < full / 10,
            "round {round}: the author kept {kept} of {full}"
        );

        // Peers learn the checkpoint and drop the same ops.
        sync(&a, &b);
        sync(&b, &c);
        assert!(
            b.prune_peer_logs().unwrap() > 0,
            "round {round}: beta pruned nothing"
        );
        assert!(
            c.prune_peer_logs().unwrap() > 0,
            "round {round}: gamma pruned nothing"
        );
        assert_eq!(b.stored_ops_by(&alpha).unwrap(), kept, "round {round}");
        assert_eq!(c.stored_ops_by(&alpha).unwrap(), kept, "round {round}");
        // A second pass has nothing left to do.
        assert_eq!(b.prune_peer_logs().unwrap(), 0);

        settle(&[&a, &b, &c], later);
        assert_eq!(summary(&a), summary(&b));
        assert_eq!(summary(&b), summary(&c));
        let (jobs, fanouts, audit) = b.read(|s| (s.jobs.len(), s.fanouts.len(), s.audit.len()));
        assert_eq!(
            jobs, 0,
            "round {round}: jobs past the window are still held"
        );
        assert_eq!(fanouts, 0, "round {round}: fan-outs past the window");
        assert_eq!(audit, 0, "round {round}: audit rows past the window");
        settled_counts.push(kept);
    }
    // Bounded: three rounds of volume leave the same log behind.
    assert!(
        settled_counts.windows(2).all(|w| w[1] <= w[0] + 2),
        "the settled log grows with volume: {settled_counts:?}"
    );

    // A joiner that learns the mesh from a peer — never the author — gets
    // the small log and the same state, without parking.
    let (d, _) = open(&mut m, 3);
    sync(&b, &d);
    assert_eq!(
        d.stored_ops_by(&alpha).unwrap(),
        a.stored_ops_by(&alpha).unwrap()
    );
    assert_eq!(d.version_vector(), b.version_vector());
    let later = mesh_proto::now_micros() + 410 * DAY;
    settle(&[&b, &d], later);
    assert_eq!(summary(&d), summary(&b));

    // A peer that restarts replays its pruned log to the same place.
    let before = summary(&b);
    let vv = b.version_vector();
    let before_seq = applied(&b, &beta);
    drop(b);
    let b = StateEngine::open(&b_dir, m.keys[1].clone(), m.genesis.clone()).unwrap();
    assert_eq!(
        b.version_vector(),
        vv,
        "the restarted peer parked on its own copy"
    );
    settle(&[&b], later);
    assert_eq!(summary(&b), before);
    // And it still writes the next sequence number, not one it has spent.
    let next = b
        .author_op(OpBody::MemberUpdate {
            node_id: beta,
            name: "beta".into(),
            addrs: vec![],
            tags: vec![],
            slots: 4,
        })
        .unwrap();
    assert_eq!(next.seq, before_seq + 1);
}

/// A plan somebody adopted stays: other authors' ops name it.
#[test]
fn an_adopted_fan_out_keeps_its_plan() {
    let mut m = mesh(2);
    let (a, _) = open(&mut m, 0);
    let (b, _) = open(&mut m, 1);
    for (i, key) in m.keys.iter().enumerate() {
        a.author_op(OpBody::MemberAdd {
            member: member_info(key, ["alpha", "beta"][i]),
            invite: None,
        })
        .unwrap();
    }
    let signed = mesh_core::retention::sign(&m.founder, policy(1, 0), mesh_proto::now_micros());
    mesh_core::retention::put(&a, signed).unwrap();
    let group = FanId::generate();
    a.author_op(plan(group, a.node_id(), &m.founder, b.node_id()))
        .unwrap();
    sync(&a, &b);
    b.author_op(OpBody::FanoutAdopt {
        group,
        by: b.node_id(),
    })
    .unwrap();
    b.author_op(OpBody::FanoutClosed {
        group,
        succeeded: 1,
        failed: 0,
        skipped: 0,
    })
    .unwrap();
    sync(&b, &a);
    assert!(a.retirable_groups().fanouts.is_empty());
    a.compact_self_at(0, mesh_proto::now_micros() + 30 * DAY)
        .unwrap();
    let plans = a
        .load_author_ops(a.node_id())
        .iter()
        .filter(|o| matches!(o.decode_body(), Some(OpBody::FanoutPlan { .. })))
        .count();
    assert_eq!(plans, 1, "an adopted plan left the log");
}

/// Sign an op by hand, as its author would, at a chosen instant.
fn sealed(key: &Keypair, seq: u64, wall_micros: u64, body: &OpBody) -> OpEnvelope {
    let author = key.node_id();
    let hlc = Hlc {
        wall_micros,
        logical: 0,
    };
    let bytes = postcard::to_stdvec(body).unwrap();
    let signature = key
        .sign(&mesh_proto::op_signing_bytes_raw(
            &author, seq, &hlc, &bytes,
        ))
        .to_vec();
    OpEnvelope {
        author,
        seq,
        hlc,
        body: bytes,
        signature,
    }
}

/// An adoption whose plan is gone does not hold up its author for ever
/// once it is older than the window — and one this node wrote itself is
/// never held up at all, which is what keeps a restart from re-issuing a
/// sequence number.
#[test]
fn an_orphaned_adoption_is_released_not_parked() {
    let mut m = mesh(2);
    let (a, _) = open(&mut m, 0);
    let (c, _) = open(&mut m, 1);
    let old = mesh_proto::now_micros() - 200 * DAY;
    let beta = Keypair::generate().unwrap();
    // The founder admitted beta long ago…
    let founder_ops = vec![
        sealed(
            &m.keys[0],
            1,
            old,
            &OpBody::MemberAdd {
                member: member_info(&m.keys[0], "alpha"),
                invite: None,
            },
        ),
        sealed(
            &m.keys[0],
            2,
            old + 1,
            &OpBody::MemberAdd {
                member: member_info(&beta, "beta"),
                invite: None,
            },
        ),
    ];
    c.ingest(founder_ops).unwrap();
    // …and beta adopted a fan-out whose plan nobody here will ever see,
    // then went on writing.
    let group = FanId::generate();
    let beta_ops = vec![
        sealed(
            &beta,
            1,
            old + 2,
            &OpBody::FanoutAdopt {
                group,
                by: beta.node_id(),
            },
        ),
        sealed(
            &beta,
            2,
            old + 3,
            &OpBody::MemberUpdate {
                node_id: beta.node_id(),
                name: "beta-renamed".into(),
                addrs: vec![],
                tags: vec![],
                slots: 2,
            },
        ),
    ];
    c.ingest(beta_ops).unwrap();
    assert_eq!(applied(&c, &beta.node_id()), 2, "beta is parked");
    assert_eq!(
        c.read(|s| s.members[&beta.node_id()].info.name.clone()),
        "beta-renamed"
    );

    // A node's own orphan, on the node that wrote it.
    a.author_op(OpBody::MemberAdd {
        member: member_info(&m.keys[0], "alpha"),
        invite: None,
    })
    .unwrap();
    let mine = a
        .author_op(OpBody::FanoutClosed {
            group: FanId::generate(),
            succeeded: 0,
            failed: 0,
            skipped: 0,
        })
        .unwrap();
    let after = a
        .author_op(OpBody::MemberUpdate {
            node_id: a.node_id(),
            name: "alpha".into(),
            addrs: vec![],
            tags: vec![],
            slots: 1,
        })
        .unwrap();
    assert_eq!(after.seq, mine.seq + 1);
    assert_eq!(applied(&a, &a.node_id()), after.seq);
}

/// A recent adoption whose plan is missing still waits: its plan may
/// simply not have arrived.
#[test]
fn a_recent_adoption_without_its_plan_still_waits() {
    let mut m = mesh(1);
    let (c, _) = open(&mut m, 0);
    let now = mesh_proto::now_micros();
    let beta = Keypair::generate().unwrap();
    c.ingest(vec![
        sealed(
            &m.keys[0],
            1,
            now - 10,
            &OpBody::MemberAdd {
                member: member_info(&m.keys[0], "alpha"),
                invite: None,
            },
        ),
        sealed(
            &m.keys[0],
            2,
            now - 9,
            &OpBody::MemberAdd {
                member: member_info(&beta, "beta"),
                invite: None,
            },
        ),
    ])
    .unwrap();
    c.ingest(vec![sealed(
        &beta,
        1,
        now,
        &OpBody::FanoutAdopt {
            group: FanId::generate(),
            by: beta.node_id(),
        },
    )])
    .unwrap();
    assert_eq!(applied(&c, &beta.node_id()), 0);
}

/// Only the newest audit rows stay in memory, and which ones does not
/// depend on the order they arrived in.
#[test]
fn the_audit_tail_is_bounded_and_the_same_everywhere() {
    let mut m = mesh(1);
    let (a, _) = open(&mut m, 0);
    let (x, _) = open(&mut m, 0);
    let (y, _) = open(&mut m, 0);
    x.set_audit_resident_for_test(10);
    y.set_audit_resident_for_test(10);
    a.author_op(OpBody::MemberAdd {
        member: member_info(&m.keys[0], "alpha"),
        invite: None,
    })
    .unwrap();
    let me = a.node_id();
    for i in 0..40u8 {
        let mut id = [0u8; 16];
        id[5] = i;
        a.author_op(OpBody::AuditAppend {
            entry: AuditEntry {
                audit_id: AuditId(id),
                at_micros: u64::from(i) + 1,
                user: UserId([1; 32]),
                username: "ada".into(),
                action: "job.run".into(),
                target: String::new(),
                node: me,
                outcome: "ok".into(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        })
        .unwrap();
    }
    let ops = a.all_ops().unwrap();
    x.ingest(ops.clone()).unwrap();
    // Twice, and the second time only the older half: nothing old gets back
    // in, and the result is the same set.
    y.ingest(ops[..20].to_vec()).unwrap();
    y.ingest(ops.clone()).unwrap();
    let tail = |e: &Arc<StateEngine>| e.read(|s| s.audit.keys().copied().collect::<Vec<_>>());
    assert_eq!(tail(&x).len(), 10);
    assert_eq!(tail(&x), tail(&y));
    assert_eq!(tail(&x).first().map(|id| id.0[5]), Some(30));
    assert_eq!(x.audit_tail_start(), tail(&x).first().copied());
    // Every row is still in the log for the history index to serve.
    assert_eq!(x.audit_ops().unwrap().len(), 40);
}
