//! M31 B2 — one HTTP path, four named destinations.
//!
//! Two claims under test, and the second is the one that will be argued
//! with:
//!
//! 1. **Four destinations are one code path.** ClickHouse and VictoriaLogs
//!    are literally the same function; OpenSearch is that plus an action
//!    line; a webhook is that with a URL somebody typed. Everything either
//!    side of the write — the cursor, the spool, the drop-and-count — is
//!    shared, and the tests assert that rather than four connectors that
//!    happen to agree today.
//! 2. **The retention gate has a ceiling, and the ceiling wins** (§19 Q3,
//!    §18 decision 03). *"Don't delete until it is exported"* is
//!    reasonable; *"don't ever delete"* is C3 inverted, arriving through
//!    the back door, and the test that stops it is the one asserting
//!    deletion resumes with the gap reported.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use mesh_core::compact::ExportGate;
use mesh_core::genesis::Genesis;
use mesh_core::identity::Keypair;
use mesh_core::state::StateEngine;
use mesh_proto::{
    MemberInfo, OpBody, RetentionPolicy, RetentionTable, RetentionWindow, Sink, SinkKind,
    SinkOnFailure, SinkRecords, SinkTables, UserId,
};

const DAY: u64 = 86_400 * 1_000_000;

struct Fixture {
    node: Keypair,
    founder: Keypair,
    genesis: Genesis,
}

fn setup() -> Fixture {
    let founder = Keypair::generate().unwrap();
    let node = Keypair::generate().unwrap();
    let genesis = Genesis::create("b2-mesh", &founder, node.node_id()).unwrap();
    Fixture {
        node,
        founder,
        genesis,
    }
}

fn engine(fx: &Fixture, dir: &std::path::Path) -> Arc<StateEngine> {
    let e = StateEngine::open(dir, fx.node.clone(), fx.genesis.clone()).unwrap();
    e.author_op(OpBody::MemberAdd {
        member: MemberInfo {
            node_id: fx.node.node_id(),
            name: "alpha".into(),
            addrs: vec!["127.0.0.1:7777".into()],
            tags: vec![],
            slots: 8,
            joined_at_micros: mesh_proto::now_micros(),
        },
        invite: None,
    })
    .unwrap();
    e
}

fn sink(name: &str, kind: SinkKind, hold_days: u32) -> Sink {
    Sink {
        name: name.into(),
        kind,
        url: if kind.is_http() {
            "https://warehouse.corp:8443".into()
        } else {
            String::new()
        },
        database: String::new(),
        credential: String::new(),
        read_credential: String::new(),
        tables: SinkTables::default(),
        records: SinkRecords::default(),
        payloads: Default::default(),
        on_failure: SinkOnFailure {
            hold_for_export_days: hold_days,
            ..Default::default()
        },
        run_on: String::new(),
        enabled: true,
        description: String::new(),
        created_by: UserId([0u8; 32]),
        created_at_micros: 0,
        signature: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The gate (§19 Q3) — end to end, through replicated state
// ---------------------------------------------------------------------------

/// The gate is computed from **replicated** state, so every node's
/// compactor reaches the same answer. Without the replicated watermark it
/// would be a guarantee on the elected exporter and a lie everywhere else —
/// which is worse than no gate, because it would be believed.
#[test]
fn the_gate_is_replicated_so_every_node_computes_it_identically() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    // No sinks: no gate at all, which is the default.
    assert_eq!(e.export_gate(), ExportGate::default());

    mesh_core::sinks::put_as(&e, &fx.founder, sink("warehouse", SinkKind::ClickHouse, 7)).unwrap();
    let gate = e.export_gate();
    assert_eq!(gate.hold_micros, 7 * DAY);
    assert_eq!(gate.exported_through_micros, 0, "nothing exported yet");

    // The exporter publishes progress; every node sees it.
    e.author_op(OpBody::SinkProgress {
        sink: "warehouse".into(),
        exported_through_micros: 500 * DAY,
    })
    .unwrap();
    assert_eq!(e.export_gate().exported_through_micros, 500 * DAY);

    // Merged by **max**: a stale re-delivery cannot move it backwards.
    e.author_op(OpBody::SinkProgress {
        sink: "warehouse".into(),
        exported_through_micros: 100 * DAY,
    })
    .unwrap();
    assert_eq!(e.export_gate().exported_through_micros, 500 * DAY);
}

/// With two gating sinks, a record is held until **both** have taken it,
/// and the ceiling is the **smallest** — so no sink can extend another's
/// hold beyond what its own operator agreed to.
#[test]
fn two_gating_sinks_take_the_slowest_watermark_and_the_strictest_ceiling() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    mesh_core::sinks::put_as(&e, &fx.founder, sink("fast", SinkKind::ClickHouse, 30)).unwrap();
    mesh_core::sinks::put_as(&e, &fx.founder, sink("slow", SinkKind::VictoriaLogs, 7)).unwrap();
    e.author_op(OpBody::SinkProgress {
        sink: "fast".into(),
        exported_through_micros: 900 * DAY,
    })
    .unwrap();
    e.author_op(OpBody::SinkProgress {
        sink: "slow".into(),
        exported_through_micros: 100 * DAY,
    })
    .unwrap();

    let gate = e.export_gate();
    assert_eq!(
        gate.exported_through_micros,
        100 * DAY,
        "the faster sink's progress authorised deleting what the slower one still needs"
    );
    assert_eq!(
        gate.hold_micros,
        7 * DAY,
        "a sink extended another's ceiling"
    );
}

/// A sink that is switched off stops gating. A disabled sink holding the
/// op-log open across the fleet would be a footgun with no way to see it.
#[test]
fn a_disabled_sink_stops_holding_the_log_open() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let mut s = sink("warehouse", SinkKind::ClickHouse, 7);
    mesh_core::sinks::put_as(&e, &fx.founder, s.clone()).unwrap();
    assert!(e.export_gate().hold_micros > 0);

    s.enabled = false;
    s.created_at_micros += 1;
    mesh_core::sinks::put_as(&e, &fx.founder, s).unwrap();
    assert_eq!(e.export_gate(), ExportGate::default());
}

/// **The ceiling is enforced at creation, not only honoured at compaction.**
/// A ceiling an operator can set to a decade is the unbounded case wearing
/// a number.
#[test]
fn a_hold_longer_than_the_maximum_is_refused_and_names_the_limit() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());

    let s = sink("warehouse", SinkKind::ClickHouse, 400);
    let err = mesh_core::sinks::put_as(&e, &fx.founder, s.clone()).expect_err("too long");
    let msg = err.to_string();
    assert!(msg.contains("30"), "{msg}");
    assert!(msg.contains("permanent"), "{msg}");
    assert!(e.read(|st| st.sinks.is_empty()));

    // And every applier refuses it too, not just the writer.
    e.author_op(OpBody::SinkPut {
        sink: Box::new(mesh_core::sinks::sign(&fx.founder, s)),
    })
    .unwrap();
    assert!(
        e.read(|st| st.sinks.is_empty()),
        "an unbounded hold was materialized into state"
    );
}

/// **The A1 crossing, end to end.** A stalled sink delays deletion up to
/// the ceiling, then deletion resumes and the permanent gap is reported.
///
/// This is the test that stops C3 eroding through the back door: without
/// the ceiling, one unreachable warehouse would pin the op-log open on
/// every node in the fleet, for ever.
#[test]
fn a_stalled_sink_delays_deletion_to_the_ceiling_and_no_further() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());
    let me = fx.node.node_id();

    // 90-day window, 7-day hold, and a sink that has never exported.
    let mut policy = RetentionPolicy::default();
    policy.set(
        RetentionTable::Runs,
        RetentionWindow {
            days: 90,
            floor_days: 0,
        },
    );
    mesh_core::retention::put(&e, mesh_core::retention::sign(&fx.founder, policy, 0)).unwrap();
    mesh_core::sinks::put_as(&e, &fx.founder, sink("warehouse", SinkKind::ClickHouse, 7)).unwrap();

    // A job that finished long enough ago to be past both.
    let finished = 1_000;
    e.author_op(OpBody::JobUpsert {
        summary: mesh_proto::JobSummary {
            job_id: mesh_proto::JobId([1; 16]),
            cmd_preview: "echo hi".into(),
            runtime: "bash".into(),
            submitter: me,
            submitter_user: UserId([7u8; 32]),
            executor: me,
            state: mesh_proto::JobState::Succeeded,
            exit_code: Some(0),
            created_at_micros: 0,
            started_at_micros: None,
            finished_at_micros: Some(finished),
            fanout: None,
            updated_hlc: mesh_proto::Hlc {
                wall_micros: finished,
                logical: 0,
            },
        },
    })
    .unwrap();

    // The real clock is decades past 1970, so both the window and the
    // ceiling have long expired: deletion resumes, and the gap is counted.
    let plan = e.compaction_plan(0).unwrap();
    assert_eq!(
        plan.held_for_export, 0,
        "the ceiling did not release a record the sink never took"
    );
    assert_eq!(
        plan.forced_gaps, 1,
        "a permanent export gap was created without being counted"
    );
    assert!(plan.retired > 0);

    // Once the sink has caught up, there is no gap to report.
    e.author_op(OpBody::SinkProgress {
        sink: "warehouse".into(),
        exported_through_micros: mesh_proto::now_micros(),
    })
    .unwrap();
    let plan = e.compaction_plan(0).unwrap();
    assert_eq!(plan.forced_gaps, 0);
}

/// The gate's own op collapses to one live record per sink, however long
/// the mesh runs — a gate that grew the log it gates would be
/// self-defeating.
#[test]
fn the_progress_watermark_compacts_to_one_op_per_sink() {
    let fx = setup();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&fx, dir.path());
    mesh_core::sinks::put_as(&e, &fx.founder, sink("warehouse", SinkKind::ClickHouse, 7)).unwrap();

    for i in 1..=20u64 {
        e.author_op(OpBody::SinkProgress {
            sink: "warehouse".into(),
            exported_through_micros: i * DAY,
        })
        .unwrap();
    }
    e.compact_self(0).unwrap();

    let live = e
        .all_ops()
        .unwrap()
        .iter()
        .filter(|o| matches!(o.decode_body(), Some(OpBody::SinkProgress { .. })))
        .count();
    assert_eq!(live, 1, "the gate leaked {live} watermark ops");
    // And the surviving one is the newest.
    assert_eq!(e.export_gate().exported_through_micros, 20 * DAY);
}

// ---------------------------------------------------------------------------
// Per-table selection (item 3)
// ---------------------------------------------------------------------------

/// An `audit`-only sink receives no job rows. Splunk's per-GB pricing makes
/// *"send audit and nothing else"* mandatory, and designing it in from the
/// start cost nothing.
#[test]
fn per_table_selection_is_a_property_of_the_record_not_a_filter_somewhere() {
    let audit_only = SinkTables::parse("audit").unwrap();
    assert!(audit_only.audit);
    assert!(!audit_only.jobs);
    assert!(!audit_only.usage);
    assert_eq!(audit_only.names(), vec!["audit"]);

    // And the default still does not include it: an audit trail leaving
    // the mesh should be a thing somebody ticked.
    assert!(!SinkTables::default().audit);
}
