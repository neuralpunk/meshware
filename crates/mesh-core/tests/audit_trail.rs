//! **Every action is on the record, and no flood can switch the record
//! off** (SEC-6).
//!
//! Audit rows went through the op-rate limiter, so about a thousand junk
//! requests from unknown keys paused a node's audit trail for five minutes,
//! and an action whose row was refused went ahead unrecorded. The engine
//! test (`backpressure_a3`) covers the arithmetic; these cover the daemon:
//! a stranger flood costs two rows, real work is recorded row for row, and
//! a node that cannot write refuses what it would have to record.
#![allow(clippy::unwrap_used)]

use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority};
use mesh_testkit::{RunOutcome, founder_user_key, ipc_as, run_to_end, spawn_founder};

fn run(command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(30),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

#[tokio::test]
async fn a_stranger_flood_costs_two_rows_and_real_work_is_recorded() {
    let founder = spawn_founder("NIM", "audit-flood").await;
    let admin = founder_user_key(&founder).clone();
    let engine = founder.node.engine();
    let count = |denied: bool| {
        engine.read(|s| {
            s.audit
                .values()
                .filter(|e| e.action == "job.run" && e.outcome.starts_with("denied") == denied)
                .count()
        })
    };

    // Five hundred attempts to run something, each from a key made up for
    // it — the audit-worthy kind of denial.
    for _ in 0..500 {
        let stranger = Keypair::generate().unwrap();
        match run_to_end(&founder, &stranger, run("id")).await {
            RunOutcome::Denied(_) => {}
            other => panic!("a stranger ran something: {other:?}"),
        }
    }
    assert_eq!(count(true), 1, "strangers were audited one by one");

    // Real work goes on being recorded, one row per run.
    for i in 0..20 {
        match run_to_end(&founder, &admin, run(&format!("echo {i}"))).await {
            RunOutcome::Done { .. } => {}
            other => panic!("run {i}: {other:?}"),
        }
    }
    assert_eq!(count(false), 20);

    // The strangers' summary, when the window closes.
    mesh_core::audit::flush_folded(engine, true);
    assert_eq!(count(true), 2);

    founder.stop().await;
}

#[tokio::test]
async fn a_node_that_cannot_write_refuses_what_it_would_record() {
    let founder = spawn_founder("NIM", "audit-unwritable").await;
    let admin = founder_user_key(&founder).clone();
    let engine = founder.node.engine();

    // The disk fills, and the next thing this node writes finds out.
    engine.inject_write_failure_for_test(Some("disk full"));
    mesh_core::audit::record(engine, admin.user_id(), "test", "", "ok", "", &[]);
    assert!(engine.write_failure().is_some());
    match run_to_end(&founder, &admin, run("echo unrecorded")).await {
        RunOutcome::Error(message) => {
            assert!(message.contains("cannot write to its log"), "{message}");
            assert!(message.contains("disk full"), "{message}");
        }
        other => panic!("a job ran while the log was unwritable: {other:?}"),
    }
    // Reading is not refused: there is nothing to record.
    assert!(matches!(
        ipc_as(&founder, &admin, IpcRequest::Nodes).await,
        IpcResponse::Nodes(_)
    ));

    // Space is freed, and the next successful write clears it.
    engine.inject_write_failure_for_test(None);
    mesh_core::audit::record(engine, admin.user_id(), "test", "", "ok", "", &[]);
    assert!(engine.write_failure().is_none());
    match run_to_end(&founder, &admin, run("echo recorded")).await {
        RunOutcome::Done { stdout, .. } => assert_eq!(stdout.trim(), "recorded"),
        other => panic!("run after recovery: {other:?}"),
    }

    founder.stop().await;
}

/// **Memory holds the newest rows; the rest are still there** (ENG-3).
///
/// Every node used to keep every audit row it had ever seen in memory.
/// With the tail bounded, `meshnode audit` reads past it from the history
/// index — the same rows, newest first, none twice.
#[tokio::test(flavor = "multi_thread")]
async fn audit_reads_past_the_in_memory_tail() {
    let founder = spawn_founder("NIM", "audit-tail").await;
    let admin = founder_user_key(&founder).clone();
    let engine = founder.node.engine();
    engine.set_audit_resident_for_test(5);
    for i in 0..30 {
        mesh_core::audit::record(
            engine,
            admin.user_id(),
            "test.row",
            &format!("row-{i:02}"),
            "ok",
            "",
            &[],
        );
    }
    assert!(engine.read(|s| s.audit.len()) <= 5);
    assert!(engine.audit_tail_start().is_some());

    let query = IpcRequest::AuditQuery {
        user: None,
        action: Some("test.".into()),
        since_micros: None,
        limit: 100,
    };
    // The index catches up on its own timer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let rows = loop {
        let IpcResponse::Audit(rows) = ipc_as(&founder, &admin, query.clone()).await else {
            panic!("not an audit answer");
        };
        if rows.len() >= 30 || std::time::Instant::now() > deadline {
            break rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    // Every row once, newest id first.
    let mut targets: Vec<&str> = rows.iter().map(|e| e.target.as_str()).collect();
    assert!(
        rows.windows(2).all(|w| w[0].audit_id > w[1].audit_id),
        "out of order"
    );
    targets.sort_unstable();
    let expected: Vec<String> = (0..30).map(|i| format!("row-{i:02}")).collect();
    assert_eq!(targets, expected, "rows missing or repeated");
}
