//! End-to-end: rows in, Parquet on disk, SQL back out.
//!
//! These go through the real writer and the real query engine. The point is
//! not that DataFusion works — it does — but that the files this crate
//! writes are the files it claims to write, and that ingesting the same
//! history twice does not double it.

use mesh_archive::{Archive, FactArchiveRow, Table};
use mesh_proto::{HistoryJobRow, Hlc, JobId, JobState, NodeId, ResultRow, UsageRow, UserId};

/// 2026-08-22T12:00:00Z, so the partitions in assertions are readable.
const NOON: u64 = 1_787_400_000_000_000;
/// The mesh every row in these tests belongs to (M31 P0). Spelled the way
/// a real one is, because it ends up in an assertion.
const MESH: &str = "mesh-aaaaaaaaaaaaaaaaaaaaaaaaaa";
const DAY: u64 = 86_400 * 1_000_000;
const HOUR: u64 = 3_600 * 1_000_000;

fn hlc(n: u64) -> Hlc {
    Hlc {
        wall_micros: n,
        logical: 0,
    }
}

/// One `(node, principal, window)` usage partial (C1 P1).
fn usage(node: u8, principal: u8, at_micros: u64, cpu_secs: u64) -> UsageRow {
    UsageRow {
        node: NodeId([node; 32]),
        principal: UserId([principal; 32]),
        window: mesh_proto::usage_window(at_micros),
        cpu_secs,
        mem_mb_secs: cpu_secs * 512,
        gpu_secs: 0,
        wall_secs: cpu_secs,
        jobs: cpu_secs / 10,
    }
}

/// One fact observation (M18 §4.8).
fn fact(subject: &str, kernel: &str, os_id: &str, observed: u64) -> FactArchiveRow {
    let doc = format!(r#"{{"kernel_release":"{kernel}","os_id":"{os_id}"}}"#);
    FactArchiveRow {
        subject: subject.to_string(),
        collector: "system".into(),
        observed_at_micros: observed,
        digest: blake3::hash(doc.as_bytes()).to_hex().to_string(),
        doc,
    }
}

/// One fan-out target's row (M19 §5.7).
fn result(seed: u8, node: &str, ok: bool, finished: u64) -> ResultRow {
    ResultRow {
        job_id: JobId([seed; 16]),
        node: NodeId([seed; 32]),
        node_name: node.to_string(),
        state: if ok {
            JobState::Succeeded
        } else {
            JobState::Failed
        },
        exit_code: Some(i32::from(!ok)),
        outputs: vec![
            (
                "stdout".into(),
                if ok {
                    "permitrootlogin no"
                } else {
                    "permitrootlogin yes"
                }
                .into(),
            ),
            ("exit_code".into(), if ok { "0" } else { "1" }.into()),
        ],
        finished_at_micros: Some(finished),
        expect_passed: Some(ok),
        expect_detail: String::new(),
    }
}

fn job(seed: u8, command: &str, executor: u8, finished: u64) -> HistoryJobRow {
    HistoryJobRow {
        job_id: JobId([seed; 16]),
        cmd_preview: command.to_string(),
        runtime: "bash".into(),
        submitter: NodeId([1; 32]),
        submitter_user: UserId([7; 32]),
        executor: NodeId([executor; 32]),
        state: JobState::Succeeded,
        exit_code: Some(0),
        created_at_micros: finished.saturating_sub(5 * 1_000_000),
        started_at_micros: Some(finished.saturating_sub(4 * 1_000_000)),
        finished_at_micros: Some(finished),
        updated_hlc: hlc(finished),
        payload_sink: None,
        payload_key: None,
        from_archive: false,
        output: None,
    }
}

/// One genuinely signed audit row (M31 B4).
///
/// Signed for real rather than stubbed: `ingest_audit` refuses a row whose
/// bytes do not bind its own entry, so a fake would be rejected — which is
/// the check doing its job and would make this fixture confusing rather
/// than convenient.
fn audit_row(seed: u8, at: u64, action: &str, outcome: &str) -> mesh_proto::SignedAuditRow {
    let key = mesh_core::identity::Keypair::from_seed([seed; 32]);
    let entry = mesh_proto::AuditEntry {
        audit_id: mesh_proto::AuditId([seed; 16]),
        at_micros: at,
        user: UserId([7; 32]),
        username: "ada".into(),
        action: action.into(),
        target: "job-1".into(),
        node: key.node_id(),
        outcome: outcome.into(),
        detail: String::new(),
        request_sig: Vec::new(),
    };
    let body = postcard::to_stdvec(&mesh_proto::OpBody::AuditAppend {
        entry: entry.clone(),
    })
    .expect("encode");
    let hlc = hlc(at);
    let signing_bytes = mesh_proto::op_signing_bytes_raw(&key.node_id(), 1, &hlc, &body);
    mesh_proto::SignedAuditRow {
        entry,
        author: key.node_id(),
        seq: 1,
        hlc,
        signature: key.sign(&signing_bytes).to_vec(),
        signing_bytes,
    }
}

/// "Now" for ingest: well past every row, so the settle lag never hides one.
fn later() -> u64 {
    NOON + 7 * DAY
}

#[tokio::test]
async fn history_goes_in_as_rows_and_comes_back_as_sql() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    let rows = vec![
        job(1, "systemctl restart nginx", 9, NOON),
        job(2, "df -h", 8, NOON + HOUR),
        job(3, "backup", 9, NOON - DAY),
    ];

    let report = archive.ingest(&rows, later()).expect("ingest");
    assert_eq!(report.jobs_written, 3);
    assert!(archive.exists());

    // Partitioned by the UTC calendar day the row finished on.
    let jobs_dir = archive.table_dir(Table::Jobs);
    assert!(jobs_dir.join("date=2026-08-22").is_dir());
    assert!(jobs_dir.join("date=2026-08-21").is_dir());

    let result = archive
        .sql("SELECT command, state FROM jobs ORDER BY finished_at")
        .await
        .expect("query");
    assert_eq!(result.columns, vec!["command", "state"]);
    assert_eq!(
        result.rows,
        vec![
            vec!["backup".to_string(), "succeeded".to_string()],
            vec![
                "systemctl restart nginx".to_string(),
                "succeeded".to_string()
            ],
            vec!["df -h".to_string(), "succeeded".to_string()],
        ]
    );

    let jobs_out = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(jobs_out.rows[0][0], "3");
}

#[tokio::test]
async fn ingesting_the_same_history_twice_does_not_double_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    let rows = vec![job(1, "deploy", 9, NOON), job(2, "deploy", 9, NOON + HOUR)];

    let first = archive.ingest(&rows, later()).expect("first");
    assert_eq!(first.jobs_written, 2);

    // Exactly what a polling UI does: hand it the same page again.
    let second = archive.ingest(&rows, later()).expect("second");
    assert_eq!(second.jobs_written, 0);
    assert_eq!(second.jobs_skipped, 2);

    let count = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(count.rows[0][0], "2");
}

#[tokio::test]
async fn a_run_still_in_flight_is_never_frozen_into_the_archive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    let mut running = job(1, "deploy", 9, NOON);
    running.finished_at_micros = None;

    // And one that finished a second ago — still inside the settle lag.
    let now = NOON + 2 * DAY;
    let just_now = job(2, "deploy", 9, now - 1_000_000);

    let report = archive.ingest(&[running, just_now], now).expect("ingest");
    assert_eq!(report.jobs_written, 0);
    assert_eq!(report.jobs_skipped, 2);
    assert!(!archive.exists(), "nothing settled, so nothing was written");
}

#[tokio::test]
async fn a_late_arriving_run_still_lands_in_its_own_day() {
    // The case the settle lag exists for: a run that finished on another
    // node yesterday only reaches this one today.
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    archive
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("first");

    let late = job(2, "deploy", 9, NOON + 30 * 60 * 1_000_000);
    let report = archive.ingest(&[late], later()).expect("late");

    // Its finish instant is above the watermark, so it is archived — into
    // the partition for the day it *happened*, not the day it arrived.
    assert_eq!(report.jobs_written, 1);
    let rows = archive
        .sql("SELECT date, count(*) AS n FROM jobs GROUP BY date")
        .await
        .expect("query");
    assert_eq!(
        rows.rows,
        vec![vec!["2026-08-22".to_string(), "2".to_string()]]
    );
}

#[tokio::test]
async fn compaction_merges_parts_and_keeps_every_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    // Twelve separate ingests into one day — what a polling archiver does.
    for i in 0..12u8 {
        archive
            .ingest(
                &[job(i, "deploy", 9, NOON + u64::from(i) * 1_000_000)],
                later(),
            )
            .expect("ingest");
    }
    let partition = archive.table_dir(Table::Jobs).join("date=2026-08-22");
    let before = std::fs::read_dir(&partition).expect("read").count();
    assert_eq!(before, 12);

    let compacted = archive.compact(Table::Jobs).expect("compact");
    assert_eq!(compacted, vec!["date=2026-08-22".to_string()]);

    let after = std::fs::read_dir(&partition).expect("read").count();
    assert_eq!(after, 1, "the whole day should be one file now");

    let count = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(count.rows[0][0], "12", "compaction lost rows");
}

/// The part sequence numbers in a partition, ascending. Part numbers come
/// from one counter shared by every table, so a test cannot assume they
/// start at zero.
fn part_seqs(partition: &std::path::Path) -> Vec<u64> {
    let mut seqs: Vec<u64> = std::fs::read_dir(partition)
        .expect("read")
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix("part-")?
                .strip_suffix(".parquet")?
                .parse()
                .ok()
        })
        .collect();
    seqs.sort_unstable();
    seqs
}

/// ENG-19 — a compaction interrupted halfway must not lose a day's rows.
///
/// The merge used to delete the originals and *then* rename the merged file
/// into place. A crash in that window left the merged rows only in a file
/// nothing reads and the partition permanently short — silent, and
/// unrecoverable, because the rows it dropped were the ones it had just
/// finished reading.
///
/// The staging file is written and fsynced before anything is removed, and
/// carries the range it covers in its name, so the next pass can finish the
/// job. This simulates the crash by staging that file and leaving the
/// originals where they were.
#[tokio::test]
async fn an_interrupted_compaction_is_finished_by_the_next_pass() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    for i in 0..12u8 {
        archive
            .ingest(
                &[job(i, "deploy", 9, NOON + u64::from(i) * 1_000_000)],
                later(),
            )
            .expect("ingest");
    }
    let partition = archive.table_dir(Table::Jobs).join("date=2026-08-22");

    // A merge that got as far as staging its output and then died: the
    // staging file is in place and every original is still there.
    let seqs = part_seqs(&partition);
    let (lo, hi) = (seqs[0], seqs[seqs.len() - 1]);
    let merged = partition.join(format!("merging-{lo:08}-{hi:08}.parquet.tmp"));
    std::fs::copy(partition.join(format!("part-{lo:08}.parquet")), &merged).expect("stage");

    // A reader ignores it, so the archive is intact in the meantime.
    let before = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(before.rows[0][0], "12", "the staging file must not be read");

    // The next pass finishes it before starting anything new.
    archive.compact(Table::Jobs).expect("compact");

    let files: Vec<String> = std::fs::read_dir(&partition)
        .expect("read")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        files,
        vec![format!("part-{lo:08}.parquet")],
        "the staged merge should have taken the lowest part's name and left nothing else"
    );

    // The staged file here held one run, so one row is what survives — the
    // point being that the *staged* file is what is authoritative, not the
    // originals it stood for. A real merge stages all twelve.
    let after = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(after.rows[0][0], "1");
}

/// The range in the staging name is what stops a resumed merge from
/// swallowing an ingest that arrived while it was down.
#[tokio::test]
async fn a_resumed_merge_leaves_later_ingests_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    for i in 0..12u8 {
        archive
            .ingest(
                &[job(i, "deploy", 9, NOON + u64::from(i) * 1_000_000)],
                later(),
            )
            .expect("ingest");
    }
    let partition = archive.table_dir(Table::Jobs).join("date=2026-08-22");
    // Staged over the first six parts only.
    let seqs = part_seqs(&partition);
    let (lo, hi) = (seqs[0], seqs[5]);
    let merged = partition.join(format!("merging-{lo:08}-{hi:08}.parquet.tmp"));
    std::fs::copy(partition.join(format!("part-{lo:08}.parquet")), &merged).expect("stage");

    archive.compact(Table::Jobs).expect("compact");

    let files: Vec<String> = std::fs::read_dir(&partition)
        .expect("read")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !files.iter().any(|f| f.ends_with(".tmp")),
        "the staged merge should have been finished: {files:?}"
    );
    // Parts 6..11 were outside the staged range, so they were not its to
    // remove. They may since have been compacted together, which is fine —
    // what must not happen is their rows going missing.
    let after = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(
        after.rows[0][0], "7",
        "the resumed merge stood for parts 0-5 (one row) and must have left 6-11 (six rows)"
    );
}

/// ENG-19 — the `archive` window was a number `retention show` printed and
/// nothing applied, while the docs said `meshware-archive` pruned against
/// it. Now it does.
#[tokio::test]
async fn pruning_removes_whole_days_older_than_the_cutoff_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    // Five consecutive days, one run each. One ingest, because the
    // watermark makes a second one skip what the first already took.
    let rows: Vec<_> = (0..5u8)
        .map(|i| job(i, "deploy", 9, NOON - u64::from(i) * DAY))
        .collect();
    archive.ingest(&rows, later()).expect("ingest");
    let days = || {
        let mut d: Vec<String> = std::fs::read_dir(archive.table_dir(Table::Jobs))
            .expect("read")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        d.sort();
        d
    };
    assert_eq!(days().len(), 5);

    // A dry run deletes nothing and says the same thing the real one will.
    let would = archive
        .partitions_before(Table::Jobs, "2026-08-21")
        .expect("dry run");
    assert_eq!(days().len(), 5, "a dry run must not delete");

    let pruned = archive.prune(Table::Jobs, "2026-08-21").expect("prune");
    assert_eq!(pruned, would, "the dry run must match what prune does");
    assert_eq!(
        days(),
        vec!["date=2026-08-21".to_string(), "date=2026-08-22".to_string()],
        "the cutoff day itself is kept — it is not older than the window"
    );

    // The rows that remain are the ones in the days that remain.
    let count = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(count.rows[0][0], "2");
}

/// An archive directory belongs to the operator. Something in it that this
/// crate did not write is not this crate's to delete.
#[tokio::test]
async fn pruning_leaves_alone_anything_that_is_not_a_date_partition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    archive
        .ingest(&[job(0, "deploy", 9, NOON - 10 * DAY)], later())
        .expect("ingest");

    let theirs = archive.table_dir(Table::Jobs).join("notes");
    std::fs::create_dir_all(&theirs).expect("mkdir");
    std::fs::write(theirs.join("README"), b"mine").expect("write");

    archive.prune(Table::Jobs, "2099-01-01").expect("prune");
    assert!(
        theirs.join("README").exists(),
        "a directory this crate did not write must survive a prune"
    );
}

/// A cutoff in the wrong shape would read as "everything is older than
/// this" and empty the archive. It is refused instead.
#[test]
fn a_malformed_prune_cutoff_is_refused_rather_than_deleting_everything() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    for bad in ["date=2026-08-21", "2026-8-21", "", "yesterday"] {
        assert!(
            archive.prune(Table::Jobs, bad).is_err(),
            "prune accepted {bad:?}"
        );
        assert!(archive.partitions_before(Table::Jobs, bad).is_err());
    }
}

#[tokio::test]
async fn partition_pruning_means_a_date_filter_reads_one_day() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    let rows: Vec<_> = (0..10u8)
        .map(|i| job(i, "deploy", 9, NOON - u64::from(i) * DAY))
        .collect();
    archive.ingest(&rows, later()).expect("ingest");

    let plan = archive
        .sql("EXPLAIN SELECT count(*) FROM jobs WHERE date = '2026-08-22'")
        .await
        .expect("explain");
    let text = plan
        .rows
        .iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    // One partition's worth of files, not ten. This is the property that
    // makes a decade of history queryable at all.
    assert!(
        !text.contains("date=2026-08-21"),
        "the planner is reading days it was not asked about:\n{text}"
    );

    let count = archive
        .sql("SELECT count(*) AS n FROM jobs WHERE date = '2026-08-22'")
        .await
        .expect("query");
    assert_eq!(count.rows[0][0], "1");
}

#[tokio::test]
async fn the_shipped_example_queries_all_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    let rows = vec![
        job(1, "deploy", 9, NOON),
        job(2, "deploy", 9, NOON + HOUR),
        job(3, "backup", 9, NOON - DAY),
    ];
    archive.ingest(&rows, later()).expect("ingest");
    // Facts and results too, or the examples that query them would fail
    // for the honest reason that nothing was archived into those tables.
    archive
        .ingest_knowledge(
            &[
                fact("hermes", "6.1.0-18-amd64", "debian", NOON),
                fact("edge-7", "5.15.0-1052-raspi", "ubuntu", NOON - DAY),
            ],
            &[(
                "fan-abc".to_string(),
                vec![
                    result(1, "hermes", true, NOON),
                    result(2, "edge-7", false, NOON),
                ],
            )],
        )
        .expect("ingest knowledge");
    // And usage, for the same reason: an example that queries a table
    // nothing was archived into fails for an honest but useless reason.
    archive
        .ingest_usage(
            &[
                usage(1, 7, NOON - DAY, 3_600),
                usage(1, 8, NOON - DAY, 1_200),
                usage(2, 7, NOON - DAY, 600),
            ],
            later(),
        )
        .expect("ingest usage");

    // And the audit trail (M31 B4), for the same reason again.
    archive
        .ingest_audit(&[audit_row(1, NOON - DAY, "job.run", "ok")])
        .expect("ingest audit");

    // A broken example in the UI is worse than no example: somebody clicks
    // it, gets a planner error, and concludes the feature is broken.
    for (title, sql) in Archive::examples() {
        archive
            .sql(sql)
            .await
            .unwrap_or_else(|e| panic!("example {title:?} failed: {e}"));
    }
}

/// The JSON columns are readable with the engine that ships in the box, not
/// only with DuckDB.
///
/// `doc` and `outputs` are JSON strings on purpose — their keys are
/// whatever a collector or a job printed, so a column per key would be a
/// schema nobody controls (§4.8). The cost of that choice is that reading
/// one takes a regex here rather than a JSON path, and this is where that
/// stays true.
#[tokio::test]
async fn json_columns_are_queryable_with_the_shipped_engine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    archive
        .ingest_knowledge(
            &[
                fact("hermes", "6.1.0-18-amd64", "debian", NOON),
                fact("edge-7", "5.15.0-1052-raspi", "ubuntu", NOON),
            ],
            &[],
        )
        .expect("ingest");

    // Filtering: a JSON key/value pair is a literal substring of canonical
    // JSON, so `LIKE` is enough and needs no regex at all.
    let out = archive
        .sql("SELECT subject FROM facts WHERE doc LIKE '%\"os_id\":\"debian\"%'")
        .await
        .expect("filter");
    assert_eq!(out.rows.len(), 1, "{:?}", out.rows);
    assert_eq!(out.rows[0][0], "hermes");

    // Extraction: one capture group, pulled out with `regexp_replace`.
    let out = archive
        .sql(
            "SELECT subject, \
             regexp_replace(doc, '^.*\"os_id\":\"([^\"]+)\".*$', '$1') AS os \
             FROM facts ORDER BY subject",
        )
        .await
        .expect("extract");
    assert_eq!(out.rows.len(), 2, "{:?}", out.rows);
    assert_eq!(
        out.rows[0],
        vec!["edge-7".to_string(), "ubuntu".to_string()]
    );
    assert_eq!(
        out.rows[1],
        vec!["hermes".to_string(), "debian".to_string()]
    );
}

#[tokio::test]
async fn a_typo_in_a_table_name_says_what_the_tables_are() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    archive
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("ingest");

    let err = archive
        .sql("SELECT * FROM job")
        .await
        .expect_err("should fail");
    let msg = err.to_string();
    assert!(msg.contains("`jobs`"), "unhelpful error: {msg}");
}

#[tokio::test]
async fn null_timestamps_stay_null_rather_than_becoming_1970() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    let mut never_started = job(1, "deploy", 9, NOON);
    never_started.started_at_micros = None;
    archive.ingest(&[never_started], later()).expect("ingest");

    let rows = archive
        .sql("SELECT count(*) AS n FROM jobs WHERE started_at IS NULL")
        .await
        .expect("query");
    assert_eq!(rows.rows[0][0], "1");
}

// ---------------------------------------------------------------------------
// M31 P0 — mesh identity, the format-1 read path, and the watermark tie-break
// ---------------------------------------------------------------------------

/// Every table leads with the mesh that produced its rows, and it is not
/// null. This is the irreversible half of P0: the column can be added for
/// free today and costs an `ALTER` on a customer's warehouse later.
#[tokio::test]
async fn every_table_carries_the_mesh_that_wrote_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    archive
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("ingest");
    archive
        .ingest_usage(&[usage(1, 7, NOON, 120)], later())
        .expect("usage");
    archive
        .ingest_knowledge(
            &[FactArchiveRow {
                subject: "hermes".into(),
                collector: "system".into(),
                observed_at_micros: NOON,
                doc: r#"{"os_id":"debian"}"#.into(),
                digest: "d".into(),
            }],
            &[("grp".to_string(), vec![result(1, "hermes", true, NOON)])],
        )
        .expect("knowledge");

    for table in ["jobs", "usage", "facts", "results"] {
        let out = archive
            .sql(&format!(
                "SELECT count(*) AS n FROM {table} WHERE mesh_id = '{MESH}'"
            ))
            .await
            .unwrap_or_else(|e| panic!("{table}: {e}"));
        assert_eq!(out.rows[0][0], "1", "{table} lost its mesh_id");
    }

    // And it is the *first* column, because that is where a reader doing
    // `SELECT *` looks for the coarsest thing they can filter on.
    let star = archive.sql("SELECT * FROM jobs").await.expect("select *");
    assert_eq!(star.columns[0], "mesh_id");
}

/// The payload locator B2 fills and B3 reads. Present and null today; the
/// point of the test is that it is *present*, since that is the half a
/// customer's warehouse takes its DDL from.
#[tokio::test]
async fn the_payload_locator_columns_are_reserved_and_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    archive
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("ingest");

    for table in ["jobs"] {
        let out = archive
            .sql(&format!(
                "SELECT count(*) AS n FROM {table} \
                 WHERE payload_sink IS NULL AND payload_key IS NULL"
            ))
            .await
            .unwrap_or_else(|e| panic!("{table}: {e}"));
        assert_eq!(out.rows[0][0], "1", "{table} is missing the locator");
    }
}

/// Two runs finishing in the same microsecond, with a batch cut between
/// them. Before P0 the second was dropped for good; the composite cursor is
/// what makes both arrive.
#[tokio::test]
async fn two_rows_in_the_same_microsecond_both_arrive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    let a = job(1, "deploy", 9, NOON);
    let b = job(2, "backup", 9, NOON);
    assert_eq!(a.finished_at_micros, b.finished_at_micros);

    // The batch cut: two ingests, exactly as a paging caller produces.
    assert_eq!(archive.ingest(&[a], later()).expect("a").jobs_written, 1);
    assert_eq!(archive.ingest(&[b], later()).expect("b").jobs_written, 1);

    let out = archive
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query");
    assert_eq!(out.rows[0][0], "2", "the tie dropped a row");

    // And it is still idempotent: replaying both changes nothing.
    let again = archive
        .ingest(
            &[job(1, "deploy", 9, NOON), job(2, "backup", 9, NOON)],
            later(),
        )
        .expect("replay");
    assert_eq!(again.jobs_written, 0);
    assert_eq!(again.jobs_skipped, 2);
}

/// Strip the columns format 2 added, producing a genuine pre-M31 part file.
///
/// The fixture has to be real. Writing a format-2 file and only winding the
/// *manifest* back would prove the reader tolerates a column that is
/// actually present, which is not the thing at risk — what is at risk is
/// whether a listing table fills a declared column that no file has.
fn downgrade_to_format_one(root: &std::path::Path) {
    use datafusion::arrow::array::RecordBatch;
    use datafusion::arrow::datatypes::Schema;
    use datafusion::parquet::arrow::ArrowWriter;
    use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    for table in ["jobs", "usage", "facts", "results"] {
        let dir = root.join(table);
        if !dir.is_dir() {
            continue;
        }
        for day in std::fs::read_dir(&dir).expect("table dir").flatten() {
            for part in std::fs::read_dir(day.path()).expect("partition").flatten() {
                let path = part.path();
                if path.extension().is_none_or(|e| e != "parquet") {
                    continue;
                }
                let batches: Vec<RecordBatch> = ParquetRecordBatchReaderBuilder::try_new(
                    std::fs::File::open(&path).expect("open"),
                )
                .expect("reader")
                .build()
                .expect("build")
                .map(|b| b.expect("batch"))
                .collect();

                let stripped: Vec<RecordBatch> = batches
                    .iter()
                    .map(|b| {
                        let keep: Vec<usize> = b
                            .schema()
                            .fields()
                            .iter()
                            .enumerate()
                            .filter(|(_, f)| {
                                f.name() != "mesh_id"
                                    && f.name() != "payload_sink"
                                    && f.name() != "payload_key"
                            })
                            .map(|(i, _)| i)
                            .collect();
                        b.project(&keep).expect("project")
                    })
                    .collect();

                let schema: std::sync::Arc<Schema> = stripped
                    .first()
                    .map(|b| b.schema())
                    .expect("at least one batch");
                let file = std::fs::File::create(&path).expect("truncate");
                let mut w = ArrowWriter::try_new(file, schema, None).expect("writer");
                for b in &stripped {
                    w.write(b).expect("write");
                }
                w.close().expect("close");
            }
        }
    }

    let manifest_path = root.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read")).expect("json");
    manifest["format"] = serde_json::json!(1);
    manifest["mesh_id"] = serde_json::Value::Null;
    manifest["legacy_parts"] = serde_json::json!(false);
    manifest["runs_watermark_id"] = serde_json::json!("");
    manifest["jobs_watermark_id"] = serde_json::json!("");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("encode"),
    )
    .expect("write");
}

/// A format-1 archive is **read**, not refused, with `mesh_id` projected
/// from the local mesh. Rebuilding costs a full re-page of history and the
/// projection is correct for every single-mesh archive, which is all of
/// them.
#[tokio::test]
async fn a_format_one_archive_still_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("archive");
    Archive::for_mesh(&root, MESH)
        .expect("open")
        .ingest(
            &[job(1, "deploy", 9, NOON), job(2, "backup", 9, NOON + HOUR)],
            later(),
        )
        .expect("ingest");
    downgrade_to_format_one(&root);

    // It opens, rather than being refused with Error::Format.
    let reader = Archive::open(&root)
        .expect("a format-1 archive opens")
        .reading_as_mesh(MESH);
    assert!(reader.has_legacy_parts());

    let out = reader
        .sql("SELECT count(*) AS n FROM jobs")
        .await
        .expect("query a format-1 archive");
    assert_eq!(out.rows[0][0], "2");

    // The projection puts the local mesh id on rows that predate the column.
    let projected = reader
        .sql(&format!(
            "SELECT count(*) AS n FROM jobs WHERE mesh_id = '{MESH}'"
        ))
        .await
        .expect("projection");
    assert_eq!(projected.rows[0][0], "2");

    // The columns format 2 added read as null rather than as an error.
    let locator = reader
        .sql("SELECT count(*) AS n FROM jobs WHERE payload_sink IS NULL")
        .await
        .expect("locator");
    assert_eq!(locator.rows[0][0], "2");
}

/// Ingesting into a format-1 archive upgrades it in place: new parts carry
/// the column, old ones do not, and one view reads both.
#[tokio::test]
async fn ingesting_into_a_format_one_archive_upgrades_it_and_both_halves_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("archive");
    Archive::for_mesh(&root, MESH)
        .expect("open")
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("first");
    downgrade_to_format_one(&root);

    let archive = Archive::for_mesh(&root, MESH).expect("reopen");
    archive
        .ingest(&[job(2, "backup", 9, NOON + HOUR)], later())
        .expect("second");

    let manifest = archive.manifest().expect("manifest");
    assert_eq!(manifest.format, mesh_archive::ARCHIVE_FORMAT);
    assert!(
        manifest.legacy_parts,
        "the upgrade must record that old files are still here"
    );
    assert_eq!(manifest.mesh_id.as_deref(), Some(MESH));

    // Both halves, through one view: the old row's mesh_id is projected and
    // the new row's is read from the file.
    let out = archive
        .sql(&format!(
            "SELECT count(*) AS n FROM jobs WHERE mesh_id = '{MESH}'"
        ))
        .await
        .expect("query");
    assert_eq!(out.rows[0][0], "2");
}

/// With no local mesh — a restored archive on a machine that has never been
/// in one — the projection is null rather than a guess. A wrong mesh id is
/// worse than an absent one, because only one of them is visibly absent.
#[tokio::test]
async fn a_format_one_archive_with_no_local_mesh_reads_null_rather_than_guessing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("archive");
    Archive::for_mesh(&root, MESH)
        .expect("open")
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("ingest");
    downgrade_to_format_one(&root);

    let out = Archive::open(&root)
        .expect("open")
        .sql("SELECT count(*) AS n FROM jobs WHERE mesh_id IS NULL")
        .await
        .expect("query");
    assert_eq!(out.rows[0][0], "1");
}

/// **M31 B2's half of §7's third crossing.** A job whose payload was
/// exported carries its locator into the Parquet row — because A1 will have
/// pruned the op-log row long before anyone asks the six-month question, so
/// the archive is where the locator has to survive.
#[tokio::test]
async fn an_exported_payloads_locator_reaches_the_archive_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");

    let mut exported = job(1, "echo hi", 9, NOON);
    exported.payload_sink = Some("warehouse".into());
    exported.payload_key = Some("mesh-aaaa/jobs/job-1".into());
    let plain = job(2, "df -h", 9, NOON + HOUR);

    archive.ingest(&[exported, plain], later()).expect("ingest");

    let out = archive
        .sql("SELECT count(*) AS n FROM jobs WHERE payload_sink = 'warehouse'")
        .await
        .expect("query");
    assert_eq!(out.rows[0][0], "1");

    // And a job that was never exported still reads null rather than
    // acquiring somebody else's locator.
    let out = archive
        .sql("SELECT count(*) AS n FROM jobs WHERE payload_sink IS NULL")
        .await
        .expect("query");
    assert_eq!(out.rows[0][0], "1");
}

/// One archive, one mesh. The manifest carries a single cursor pair, so two
/// meshes in one directory do not merge — they interleave, each advancing
/// the other's watermark past rows it never wrote.
#[test]
fn an_archive_refuses_a_second_mesh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("archive");
    Archive::for_mesh(&root, MESH)
        .expect("open")
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("ingest");

    let err = Archive::for_mesh(&root, "mesh-bbbbbbbbbbbbbbbbbbbbbbbbbb")
        .expect_err("a second mesh must be refused");
    let msg = err.to_string();
    assert!(msg.contains(MESH), "{msg}");
    assert!(msg.contains("one archive, one mesh"), "{msg}");
}

/// Ingest needs the identity; reading does not. The reader's whole promise
/// is that it works on a copy of the directory on a machine that has never
/// been part of a mesh, so opening read-only must stay free.
#[test]
fn ingesting_without_a_mesh_id_is_refused_and_says_why() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::open(dir.path().join("archive")).expect("open");
    let err = archive
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect_err("no mesh id");
    assert!(err.to_string().contains("for_mesh"), "{err}");
}

/// S4: the archive answers questions and does nothing else.
///
/// `SessionContext` ships with DDL and `COPY` enabled, which made
/// `Archive::sql` — reachable from `meshnode archive query` and from
/// Director's IPC — an arbitrary file read and write as the process. These
/// are the four shapes that mattered, plus the one that proves the surface
/// still works.
#[tokio::test]
async fn a_query_cannot_read_or_write_the_filesystem() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    archive
        .ingest(&[job(1, "deploy", 9, NOON)], later())
        .expect("ingest");

    let secret = dir.path().join("secret.csv");
    std::fs::write(&secret, "user,password\nroot,hunter2\n").expect("write");
    let written = dir.path().join("written");

    let refused = [
        // Read any file by declaring a table over it.
        format!(
            "CREATE EXTERNAL TABLE leak STORED AS CSV LOCATION '{}'",
            secret.display()
        ),
        // Write any file the process can write.
        format!(
            "COPY (SELECT 1 AS x) TO '{}' STORED AS CSV",
            written.display()
        ),
        // The same two, one level down — the reason this is a plan walk
        // rather than a look at the first keyword.
        format!(
            "EXPLAIN COPY (SELECT 1 AS x) TO '{}' STORED AS CSV",
            written.display()
        ),
        // Change the session out from under the next query.
        "SET datafusion.execution.batch_size = 1".to_string(),
        // And an in-memory table is still a write.
        "CREATE TABLE scratch AS SELECT 1 AS x".to_string(),
    ];
    for sql in &refused {
        let err = archive
            .sql(sql)
            .await
            .err()
            .unwrap_or_else(|| panic!("{sql} should have been refused"));
        let msg = err.to_string();
        assert!(
            msg.contains("not supported"),
            "{sql} was refused for the wrong reason: {msg}"
        );
    }
    assert!(!written.exists(), "COPY must not have written anything");

    // And the surface it exists for still works.
    let ok = archive
        .sql("SELECT command FROM jobs")
        .await
        .expect("a select is still a select");
    assert_eq!(ok.rows, vec![vec!["deploy".to_string()]]);
    assert!(
        archive
            .sql("EXPLAIN SELECT command FROM jobs")
            .await
            .is_ok(),
        "EXPLAIN over a SELECT is a question, not a change"
    );
}

/// **H-7**: a backfill paged newest-first must archive every page, not
/// just the first.
///
/// `Archive::ingest` skips any row at or below the watermark it held when
/// the call started, and writes the new high-water mark at the end. The
/// CLI's backfill walked the index newest-first with a rising `offset`, so
/// page 1 pushed the watermark to the newest row and every older page that
/// followed was — correctly, by that rule — discarded. The effect was that
/// `meshware-archive ingest` archived the newest page of pre-existing
/// history and silently dropped the rest.
///
/// This pins the ordering the CLI must use: oldest first, so the watermark
/// only ever moves forward past rows that have actually been written.
// `1 * DAY` keeps the three-row ladder below visually parallel with its
// `2 * DAY` neighbour; clippy's "reduce it to DAY" would break the alignment
// that makes the ordering under test readable at a glance.
#[allow(clippy::identity_op)]
#[test]
fn a_backfill_paged_oldest_first_archives_every_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    let now = NOON + 30 * DAY;

    // Three "pages", oldest first — the order the fixed CLI walks them in.
    let pages = [
        vec![job(1, "oldest", 2, NOON - 2 * DAY)],
        vec![job(2, "middle", 2, NOON - 1 * DAY)],
        vec![job(3, "newest", 2, NOON)],
    ];
    let mut written = 0u64;
    for page in &pages {
        written += archive.ingest(page, now).expect("ingest").jobs_written;
    }
    assert_eq!(written, 3, "every page must reach the archive");
}

/// The same three rows walked newest-first are *not* all archived — the
/// behaviour that made the old backfill lossy. Pinned so the ordering
/// requirement above cannot be quietly reverted.
// `1 * DAY` keeps the three-row ladder below visually parallel with its
// `2 * DAY` neighbour; clippy's "reduce it to DAY" would break the alignment
// that makes the ordering under test readable at a glance.
#[allow(clippy::identity_op)]
#[test]
fn a_backfill_paged_newest_first_loses_the_older_pages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = Archive::for_mesh(dir.path().join("archive"), MESH).expect("open");
    let now = NOON + 30 * DAY;

    let pages = [
        vec![job(3, "newest", 2, NOON)],
        vec![job(2, "middle", 2, NOON - 1 * DAY)],
        vec![job(1, "oldest", 2, NOON - 2 * DAY)],
    ];
    let mut written = 0u64;
    for page in &pages {
        written += archive.ingest(page, now).expect("ingest").jobs_written;
    }
    assert_eq!(
        written, 1,
        "the watermark discards everything older than the first page"
    );
}
