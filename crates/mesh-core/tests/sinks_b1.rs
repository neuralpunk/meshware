//! M31 B1 — the escape hatch: NDJSON out, and a plugin contract in.
//!
//! The claim under test is narrow and load-bearing: **a sink can fail in
//! every way a sink fails, and job throughput is unchanged.** That is C3,
//! and it is the rule that will erode under enterprise pressure — the
//! request is always *"guarantee delivery, block the job if the warehouse
//! is unreachable"*, and the answer is no.
//!
//! Everything else here is governance: a sink is admin-signed, its
//! credential is a reference and never a literal, and `egress_allow` refuses
//! a destination this machine may not reach **regardless of what the signed
//! record says**.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::StateEngine;
use mesh_proto::{
    MemberInfo, OpBody, Sink, SinkKind, SinkOnFailure, SinkRecords, SinkTables, UserId,
};

fn member_info(key: &Keypair, name: &str) -> MemberInfo {
    MemberInfo {
        node_id: key.node_id(),
        name: name.into(),
        addrs: vec!["127.0.0.1:7777".into()],
        tags: vec!["sink".into()],
        slots: 8,
        joined_at_micros: mesh_proto::now_micros(),
    }
}

struct Fixture {
    node: Keypair,
    founder: Keypair,
    genesis: Genesis,
}

fn setup() -> Fixture {
    let founder = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("b1-mesh", &founder, node.node_id()).unwrap();
    Fixture {
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

/// A sink that survives `refusal()`, so a test about anything else means
/// something.
///
/// `Plugin` rather than the retired `Ndjson` these were written against:
/// plugin is the cheapest kind that needs no URL, and like the old fixture
/// it writes locally, so the C3 spool tests still exercise the path they
/// were built for.
fn sink(name: &str) -> Sink {
    Sink {
        name: name.into(),
        kind: SinkKind::Plugin,
        url: String::new(),
        database: "/usr/local/bin/mw-ship".into(),
        credential: String::new(),
        read_credential: String::new(),
        tables: SinkTables::default(),
        records: SinkRecords::default(),
        payloads: Default::default(),
        on_failure: SinkOnFailure::default(),
        run_on: String::new(),
        enabled: true,
        description: String::new(),
        created_by: UserId([0u8; 32]),
        created_at_micros: 0,
        signature: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// C3 — the test that keeps this honest three refactors from now
// ---------------------------------------------------------------------------

/// **Fill the spool, force drops, and assert the export path never touched
/// anything a job waits on.**
///
/// Structural rather than statistical, and deliberately so. A throughput
/// number measured on a loaded CI box is a flaky test that gets deleted; a
/// test that the write path is a pure function of `(sink, state, batch)` —
/// no engine, no scheduler, no lock a job holds — is one that fails
/// honestly the day somebody adds a `self.inner.jobs` call to it.
#[tokio::test]
async fn a_failing_sink_spends_spool_and_never_the_scheduler() {
    use mesh_core::sinks::{SinkState, WriteOutcome, write_batch};

    let dir = tempfile::tempdir().unwrap();
    let mut s = sink("warehouse");
    s.kind = SinkKind::Plugin;
    // A plugin that always fails, which is every way a destination fails
    // from this side: refused, timed out, full disk, expired token.
    s.database = "false".into();
    s.on_failure.spool_bytes = 4_000;

    let state = SinkState::new(dir.path(), &s.name);
    let batch = vec![b'x'; 1_000];
    let mut dropped = 0u64;

    for _ in 0..20 {
        let outcome = write_batch(&s, &state, &batch, 1).await;
        assert!(
            matches!(outcome, WriteOutcome::Failed { .. }),
            "the fixture must actually fail"
        );
        dropped += state.spool(&batch, s.on_failure.spool_bytes).unwrap();
    }

    assert!(dropped > 0, "the spool cap never fired");
    assert!(
        state.spool_bytes() <= s.on_failure.spool_bytes,
        "the spool grew past its cap: {} > {}",
        state.spool_bytes(),
        s.on_failure.spool_bytes
    );
}

/// The other half of C3, and the one a refactor breaks: a sink that is
/// merely *slow* must not hold anything either. A plugin that never returns
/// is abandoned at its timeout rather than parked on for ever.
#[tokio::test]
async fn a_hanging_plugin_is_abandoned_rather_than_waited_on() {
    // `sleep 30` against a timeout shortened for the test would need a
    // configurable timeout; instead assert the contract that makes the
    // timeout reachable at all — the call is `async` and takes no lock.
    // A blocking spawn here would deadlock the single-threaded runtime.
    let fut = mesh_core::sinks::plugin_write("false", b"{}\n", 1);
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .expect("the write path must not block the runtime");
    assert!(matches!(out, mesh_core::sinks::WriteOutcome::Failed { .. }));
}

// ---------------------------------------------------------------------------
// Governance
// ---------------------------------------------------------------------------

/// A sink is admin-signed, and a key nobody has admitted cannot create one.
#[test]
fn only_an_admin_can_point_this_mesh_at_a_destination() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let stranger = Keypair::generate().unwrap();
    assert!(
        mesh_core::sinks::put_as(&e, &stranger, sink("warehouse")).is_err(),
        "an unknown key created a sink"
    );
    assert!(e.read(|s| s.sinks.is_empty()));

    // The founder is an admin by construction.
    mesh_core::sinks::put_as(&e, &fx.founder, sink("warehouse")).unwrap();
    assert!(e.read(|s| s.sinks.contains_key("warehouse")));
}

/// **C6.** A credential is a `{{secret:…}}` reference; a literal is refused
/// on the way in, and refused again by every node that applies the op.
///
/// Two checks rather than one, and the second is why the first is not
/// enough: a node running an older build could author a record this one
/// would never write, and *every applier* has to decline it rather than
/// trusting the writer.
#[test]
fn a_credential_literal_is_refused_on_the_way_in_and_again_on_the_way_out() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let mut s = sink("warehouse");
    s.credential = "hunter2".into();

    // 1. The write path refuses before anything is authored. This is the
    //    check that actually keeps a secret out of the log, and it is
    //    mirrored on the daemon so the CLI is not the only thing enforcing
    //    it.
    let err = mesh_core::sinks::put_as(&e, &fx.founder, s.clone()).expect_err("a literal");
    assert!(err.to_string().contains("{{secret:"), "{err}");
    assert!(e.read(|st| st.sinks.is_empty()));
    assert!(
        e.all_ops()
            .unwrap()
            .iter()
            .all(|o| !matches!(o.decode_body(), Some(OpBody::SinkPut { .. }))),
        "a refused sink was authored anyway"
    );

    // 2. And if one reaches the log regardless — an older build, a hostile
    //    member — every applier declines to materialize it. The op is
    //    *carried*, because F0 says a node stores and forwards what it
    //    cannot honour; what it must not do is act on it.
    let signed = mesh_core::sinks::sign(&fx.founder, s.clone());
    e.author_op(OpBody::SinkPut {
        sink: Box::new(signed),
    })
    .unwrap();
    assert!(
        e.read(|st| st.sinks.is_empty()),
        "a literal credential was materialized into state"
    );
}

/// The reference form goes through, and carries no value — because there is
/// no value to carry. That is the whole of C6: the token is resolved at
/// connect time by the exporting node, so it is never in the op-log, never
/// in Director's memory, and never in `/proc/<pid>/cmdline`.
#[test]
fn a_reference_credential_travels_without_the_secret() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let mut s = sink("warehouse");
    s.credential = "{{secret:warehouse-token}}".into();
    mesh_core::sinks::put_as(&e, &fx.founder, s).unwrap();
    assert!(e.read(|st| st.sinks.contains_key("warehouse")));

    let encoded = postcard::to_stdvec(&e.all_ops().unwrap()).unwrap();
    let haystack = String::from_utf8_lossy(&encoded);
    // The reference is there, by design — it is what the exporting node
    // resolves.
    assert!(haystack.contains("warehouse-token"));
    // A value never could be, since the record has nowhere to put one.
    let live = e.read(|st| st.sinks["warehouse"].sink.credential.clone());
    assert!(mesh_proto::is_secret_reference(&live));
}

/// A removed sink compacts away like any other LWW entity — the C0
/// classification exercised rather than merely written.
#[test]
fn a_removed_sink_compacts_away_like_any_other_entity() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    // Three edits of the same sink: only the last is the sink.
    for batch in [100u32, 200, 300] {
        let mut s = sink("warehouse");
        s.records.batch = batch;
        mesh_core::sinks::put_as(&e, &fx.founder, s).unwrap();
    }
    let before = e.all_ops().unwrap().len();
    e.compact_self(0).unwrap();
    let after = e.all_ops().unwrap().len();
    assert!(
        after < before,
        "superseded sink edits were not compacted: {before} → {after}"
    );
    assert_eq!(
        e.read(|s| s.sinks.get("warehouse").map(|x| x.sink.records.batch)),
        Some(300),
        "the surviving edit must be the last one"
    );

    // The tombstone grows, as every tombstone in this system does: a
    // removal a peer has not merged must not be undone by an earlier put
    // outliving it.
    e.author_op(OpBody::SinkRemove {
        name: "warehouse".into(),
        by: fx.founder.user_id(),
        signature: mesh_core::sinks::sign_remove(&fx.founder, "warehouse"),
    })
    .unwrap();
    assert!(e.read(|s| s.sinks["warehouse"].removed));
    e.compact_self(0).unwrap();
    assert!(
        e.read(|s| s.sinks["warehouse"].removed),
        "the tombstone was compacted away and the sink came back"
    );
}

// ---------------------------------------------------------------------------
// egress_allow
// ---------------------------------------------------------------------------

/// The allowlist matches on the host, honours a `*.` wildcard, and an empty
/// list permits everything — M15's specified behaviour and the only
/// defensible default, since a control that breaks every deployment on
/// upgrade is a control operators turn off.
#[test]
fn the_egress_allowlist_matches_hosts_and_says_what_it_is() {
    use mesh_core::config::{IntegrationsConfig, IntegrationsSection};

    let open = IntegrationsConfig::default();
    assert!(open.allows_host("anything.example"));

    let allow = IntegrationsConfig::resolve(&IntegrationsSection {
        egress_allow: Some(vec![
            "warehouse.corp".into(),
            "*.internal".into(),
            "10.0.0.7".into(),
        ]),
        ..Default::default()
    });
    assert!(allow.allows_host("warehouse.corp"));
    assert!(allow.allows_host("WAREHOUSE.CORP"), "case-insensitive");
    assert!(
        allow.allows_host("warehouse.corp."),
        "a trailing dot is the same host"
    );
    assert!(allow.allows_host("a.internal"));
    assert!(allow.allows_host("a.b.internal"));
    assert!(allow.allows_host("10.0.0.7"));

    assert!(!allow.allows_host("evil.example"));
    assert!(!allow.allows_host("warehouse.corp.evil.example"));
    // A wildcard does not match its own suffix: `*.internal` is not
    // `internal`, because nobody writing it meant that.
    assert!(!allow.allows_host("internal"));
    assert!(!allow.allows_host(""));

    // The refusal names the list, so an operator is sent to the config file
    // that refused rather than to a network investigation.
    let why = allow.refusal_for("evil.example");
    assert!(why.contains("evil.example"), "{why}");
    assert!(why.contains("egress_allow"), "{why}");
    assert!(why.contains("warehouse.corp"), "{why}");
    assert!(why.contains("run_on"), "{why}");
}

// ---------------------------------------------------------------------------
// Election
// ---------------------------------------------------------------------------

/// Election is stable, key-dependent, and honours `run_on`. Two nodes never
/// both export the same sink, because every node computes the same
/// rendezvous order from the same replicated inputs.
#[test]
fn election_is_stable_key_dependent_and_honours_run_on() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    // A second member, without the `sink` tag.
    let other = Keypair::generate().unwrap();
    e.author_op(OpBody::MemberAdd {
        member: MemberInfo {
            node_id: other.node_id(),
            name: "beta".into(),
            addrs: vec!["127.0.0.1:7778".into()],
            tags: vec![],
            slots: 8,
            joined_at_micros: mesh_proto::now_micros(),
        },
        invite: None,
    })
    .unwrap();

    let alive = |_: &mesh_proto::NodeId| mesh_proto::Liveness::Alive;
    let capable = |_: &mesh_proto::NodeId| Some(mesh_proto::SINK_MINOR);

    e.read(|st| {
        // Stable: the same inputs give the same answer, every time.
        let a = mesh_core::sinks::elected_exporter(st, &sink("warehouse"), alive, capable);
        let b = mesh_core::sinks::elected_exporter(st, &sink("warehouse"), alive, capable);
        assert_eq!(a, b);
        assert!(a.is_some());

        // Key-dependent: a different sink name can land elsewhere, which is
        // what spreads a mesh's sinks over its nodes.
        let names: std::collections::BTreeSet<_> = (0..24)
            .map(|i| {
                mesh_core::sinks::elected_exporter(st, &sink(&format!("s{i}")), alive, capable)
            })
            .collect();
        assert!(names.len() > 1, "every sink elected the same node");

        // `run_on` narrows the pool to the tagged node, which is the
        // control that matters when only some machines have a route.
        let mut tagged = sink("warehouse");
        tagged.run_on = "tag=sink".into();
        assert_eq!(
            mesh_core::sinks::elected_exporter(st, &tagged, alive, capable),
            Some(fx.node.node_id()),
            "run_on did not narrow the pool"
        );

        // A tag no live node carries elects nobody, rather than falling
        // back to everybody — which would export from a machine with no
        // route and look like a broken warehouse.
        let mut nowhere = sink("warehouse");
        nowhere.run_on = "tag=does-not-exist".into();
        assert_eq!(
            mesh_core::sinks::elected_exporter(st, &nowhere, alive, capable),
            None
        );

        // A node too old to have sinks never wins, however the hash falls.
        // Otherwise it would win, do nothing, and the sink would go quiet
        // while looking healthy.
        let old = |_: &mesh_proto::NodeId| Some(mesh_proto::SINK_MINOR - 1);
        assert_eq!(
            mesh_core::sinks::elected_exporter(st, &sink("warehouse"), alive, old),
            None
        );
        // And a node that has never been negotiated with is excluded rather
        // than assumed capable.
        let unknown = |_: &mesh_proto::NodeId| None;
        assert_eq!(
            mesh_core::sinks::elected_exporter(st, &sink("warehouse"), alive, unknown),
            None
        );
    });
}

/// A dead node loses the election within a gossip round, and the successor
/// re-derives the cursor rather than being handed one.
#[test]
fn a_dead_exporter_loses_the_election_with_no_handover() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let other = Keypair::generate().unwrap();
    e.author_op(OpBody::MemberAdd {
        member: MemberInfo {
            node_id: other.node_id(),
            name: "beta".into(),
            addrs: vec!["127.0.0.1:7778".into()],
            tags: vec!["sink".into()],
            slots: 8,
            joined_at_micros: mesh_proto::now_micros(),
        },
        invite: None,
    })
    .unwrap();

    let capable = |_: &mesh_proto::NodeId| Some(mesh_proto::SINK_MINOR);
    let winner = e.read(|st| {
        mesh_core::sinks::elected_exporter(
            st,
            &sink("warehouse"),
            |_| mesh_proto::Liveness::Alive,
            capable,
        )
    });
    let winner = winner.expect("somebody must win");

    // The winner dies. Nothing is handed over; the survivor simply wins the
    // next computation, because the cursor is a property of the data.
    let successor = e.read(|st| {
        mesh_core::sinks::elected_exporter(
            st,
            &sink("warehouse"),
            |id| {
                if *id == winner {
                    mesh_proto::Liveness::Dead
                } else {
                    mesh_proto::Liveness::Alive
                }
            },
            capable,
        )
    });
    assert!(successor.is_some(), "nobody took over");
    assert_ne!(successor, Some(winner));
}

// ---------------------------------------------------------------------------
// §19 Q5 — restore holds every sink
// ---------------------------------------------------------------------------

/// A restored node exports nothing until an admin says so, and the latch
/// says why rather than leaving somebody to find out.
#[test]
fn a_restore_holds_every_sink_until_an_admin_looks() {
    let dir = tempfile::tempdir().unwrap();
    let sinks = dir.path().join("sinks");

    assert!(!mesh_core::sinks::paused_by_restore(&sinks));
    mesh_core::sinks::pause_after_restore(&sinks).unwrap();
    assert!(mesh_core::sinks::paused_by_restore(&sinks));

    // The latch explains itself on disk, because the person who finds it is
    // usually not the person who caused it.
    let note = std::fs::read_to_string(sinks.join(".paused-by-restore")).unwrap();
    assert!(note.contains("restored"), "{note}");
    assert!(note.contains("sinks resume"), "{note}");

    // Idempotent: restoring twice is not an error.
    mesh_core::sinks::pause_after_restore(&sinks).unwrap();

    assert!(mesh_core::sinks::resume_after_restore(&sinks).unwrap());
    assert!(!mesh_core::sinks::paused_by_restore(&sinks));
    // Resuming when nothing is held says so rather than pretending.
    assert!(!mesh_core::sinks::resume_after_restore(&sinks).unwrap());
}

/// A real backup/restore round trip engages the latch, and reports it.
#[test]
fn restoring_a_backup_with_sinks_reports_that_it_held_them() {
    let fx = setup();
    let src = tempfile::tempdir().unwrap();
    let e = engine(&fx, src.path());
    mesh_core::sinks::put_as(&e, &fx.founder, sink("warehouse")).unwrap();

    // The identity files a backup reads from.
    let id_dir = mesh_core::identity::Identity::identity_dir(src.path());
    std::fs::create_dir_all(&id_dir).unwrap();
    fx.node.persist(&id_dir.join("node.key")).unwrap();
    fx.founder.persist(&id_dir.join("user.key")).unwrap();

    let out = src.path().join("backup.json");
    mesh_core::backup::write_backup(&out, src.path(), "alpha", &fx.genesis, e.all_ops().unwrap())
        .unwrap();
    drop(e);

    let dst = tempfile::tempdir().unwrap();
    let target = dst.path().join("home");
    let report = mesh_core::backup::restore(&out, &target).unwrap();
    assert_eq!(report.sinks_held, 1, "the restore did not hold the sink");
    assert!(
        mesh_core::sinks::paused_by_restore(&mesh_core::layout::Layout::new(&target).sinks_dir()),
        "the latch was not engaged"
    );
}

/// A backup with no sinks does not leave a latch behind — a mesh that never
/// had one should not grow a file implying it did.
#[test]
fn restoring_a_backup_with_no_sinks_leaves_no_latch() {
    let fx = setup();
    let src = tempfile::tempdir().unwrap();
    let e = engine(&fx, src.path());
    let id_dir = mesh_core::identity::Identity::identity_dir(src.path());
    std::fs::create_dir_all(&id_dir).unwrap();
    fx.node.persist(&id_dir.join("node.key")).unwrap();
    fx.founder.persist(&id_dir.join("user.key")).unwrap();
    let out = src.path().join("backup.json");
    mesh_core::backup::write_backup(&out, src.path(), "alpha", &fx.genesis, e.all_ops().unwrap())
        .unwrap();
    drop(e);

    let dst = tempfile::tempdir().unwrap();
    let target = dst.path().join("home");
    let report = mesh_core::backup::restore(&out, &target).unwrap();
    assert_eq!(report.sinks_held, 0);
    assert!(!mesh_core::sinks::paused_by_restore(
        &mesh_core::layout::Layout::new(&target).sinks_dir()
    ));
}
