//! Reading the local Parquet archive from a node, out of process
//! (M31 B3 §14.2).
//!
//! ## Why a subprocess and not a library call
//!
//! Because **`meshnode` must not link DataFusion.** That is C2, stated as a
//! testable fact — `cargo tree -p meshware-node | grep datafusion` returns
//! nothing — and it is the reason a node is one small binary you can drop on
//! anything. Arrow and DataFusion together are larger than the rest of the
//! workspace put together.
//!
//! But B3 §14.2 needs a node to answer *"what ran six months ago"*, and at
//! six months the answer is in the archive. And §19 Q11 was decided as
//! **"ask an archive-holding peer"** — which means a node has to be able to
//! serve its archive to another node, not merely read it for a UI running
//! in the same process as Director.
//!
//! Those two requirements meet at the tool that already exists.
//! `meshware-archive` is a separate binary that already links DataFusion,
//! already reads the archive, and already prints JSON. So a node that holds
//! an archive answers by **spawning it** — the `plugin get` / `plugin write`
//! shape a third time, and exactly what §4 decision 1 says: *anything
//! needing a columnar encoder lives in `meshware-archive` or in a plugin.*
//!
//! A node without the binary, or without an archive, simply has no older
//! history — which is a state the caller renders honestly rather than an
//! error (A2's known-absence rule, one tier down).

use std::path::Path;
use std::time::Duration;

use mesh_proto::{HistoryJobRow, HistoryQuery};

use crate::error::{CoreError, Result};

/// How long an archive query may take before it is abandoned.
///
/// Fifteen seconds. A Parquet scan over years of partitions is genuinely
/// slower than a redb range read — that is the *point* of the two tiers —
/// but a History page that hangs is the screen §3's Temporal lesson warns
/// about, so there is a wall.
pub const ARCHIVE_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Rows one archive query may return, whatever the caller asked for.
pub const ARCHIVE_ROW_CAP: u32 = 2_000;

/// The binary that reads Parquet. Resolved from `PATH`, or beside the
/// running `meshnode` — an operator who installed one almost always
/// installed the other, and looking beside ourselves first is what makes a
/// tarball install work without a `PATH` edit.
fn archive_binary() -> std::ffi::OsString {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside = dir.join("meshware-archive");
        if beside.exists() {
            return beside.into_os_string();
        }
    }
    "meshware-archive".into()
}

/// Is there an archive here at all?
///
/// Cheap and filesystem-only: the caller uses it to decide whether this
/// node can answer for older history *before* paying for a subprocess.
pub fn has_archive(archive_dir: &Path) -> bool {
    archive_dir.join("manifest.json").exists()
}

/// One page of jobs out of the local archive.
///
/// The SQL is built here rather than taken from a caller: this is a
/// *browse* path, and accepting a statement from the wire would turn a
/// history query into a query engine somebody else drives.
pub async fn query(archive_dir: &Path, q: &HistoryQuery) -> Result<Vec<serde_json::Value>> {
    if !has_archive(archive_dir) {
        return Ok(Vec::new());
    }
    let sql = build_sql(q);
    let output = tokio::time::timeout(
        ARCHIVE_QUERY_TIMEOUT,
        tokio::process::Command::new(archive_binary())
            .arg("--archive")
            .arg(archive_dir)
            .arg("query")
            .arg(&sql)
            .arg("--json")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| CoreError::State("the archive query timed out".into()))?
    .map_err(|e| {
        // The commonest cause by far, and worth naming rather than
        // reporting as a generic spawn failure: the reader is a separate
        // binary and this machine does not have it.
        CoreError::State(format!(
            "could not run `meshware-archive` (older history lives in Parquet and is read by \
             that binary, which is installed alongside meshnode): {e}"
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(2).collect();
        return Err(CoreError::State(format!(
            "archive query failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| CoreError::State(format!("archive returned unreadable JSON: {e}")))
}

/// The statement one browse produces.
///
/// Every value is escaped and every bound is numeric, because this is
/// assembled from a query that crossed the wire. The alternative — a
/// parameterised interface through a CLI — does not exist, so the escaping
/// is the interface.
/// The Hive partition an instant falls in — `mesh-archive`'s
/// `schema::partition_of`, which this crate cannot call because it does not
/// depend on that one (it shells out to the binary).
///
/// Kept honest by `partition_of_matches_the_writer`, which walks the same
/// dates through both directions of the conversion already in this file.
fn partition_of(micros: u64) -> String {
    let days = (micros / 1_000_000) as i64 / 86_400;
    // Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn build_sql(q: &HistoryQuery) -> String {
    let mut where_parts: Vec<String> = Vec::new();
    let quote = |v: &str| v.replace('\'', "''");

    // **Bound the partition column, not only the timestamp** (**M-10**).
    //
    // The archive is written into Hive `date=YYYY-MM-DD` directories
    // precisely so a dated question can skip whole days — but the SQL only
    // ever constrained `finished_at`, which DataFusion cannot use to prune
    // a partition it has not opened. So "jobs from last Tuesday" read the
    // footer of every Parquet file written since the archive began: two
    // years × up to eight parts a day is well over a thousand
    // file opens for a single page.
    //
    // `date` is a string in `YYYY-MM-DD`, which sorts lexicographically in
    // the same order as the dates it names, so a plain `>=` / `<=` on it is
    // both correct and prunable. Emitted *alongside* the timestamp bounds
    // rather than instead of them: the partition is a whole day, so it
    // narrows the files while `finished_at` still decides the rows.
    if let Some(since) = q.since_micros {
        where_parts.push(format!(
            "finished_at >= to_timestamp_micros({})",
            since.min(i64::MAX as u64)
        ));
        where_parts.push(format!("date >= '{}'", partition_of(since)));
    }
    if let Some(until) = q.until_micros {
        where_parts.push(format!(
            "finished_at <= to_timestamp_micros({})",
            until.min(i64::MAX as u64)
        ));
        where_parts.push(format!("date <= '{}'", partition_of(until)));
    }
    if let Some(state) = &q.state {
        where_parts.push(format!("state = '{}'", quote(state)));
    }
    if let Some(node) = &q.node {
        where_parts.push(format!(
            "executor_node = '{}'",
            quote(&node.to_display_string())
        ));
    }
    if let Some(user) = &q.user {
        where_parts.push(format!(
            "submitter_user = '{}'",
            quote(&user.to_display_string())
        ));
    }
    if let Some(needle) = &q.contains {
        where_parts.push(format!(
            "position('{}' in lower(command)) > 0",
            quote(&needle.to_ascii_lowercase())
        ));
    }

    let filter = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };
    format!(
        "SELECT * FROM jobs{filter} ORDER BY finished_at DESC LIMIT {} OFFSET {}",
        q.limit.clamp(1, ARCHIVE_ROW_CAP),
        q.offset
    )
}

pub fn job_row_from(v: &serde_json::Value) -> Option<HistoryJobRow> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default();
    let opt = |k: &str| {
        let raw = s(k);
        (!raw.is_empty()).then(|| raw.to_string())
    };
    Some(HistoryJobRow {
        job_id: mesh_proto::JobId::parse(s("job_id")).ok()?,
        cmd_preview: s("command").to_string(),
        runtime: s("runtime").to_string(),
        submitter: mesh_proto::NodeId::parse(s("submitter_node"))
            .unwrap_or(mesh_proto::NodeId([0; 32])),
        submitter_user: mesh_proto::UserId::parse(s("submitter_user"))
            .unwrap_or(mesh_proto::UserId([0; 32])),
        executor: mesh_proto::NodeId::parse(s("executor_node"))
            .unwrap_or(mesh_proto::NodeId([0; 32])),
        state: [
            mesh_proto::JobState::Queued,
            mesh_proto::JobState::Running,
            mesh_proto::JobState::Succeeded,
            mesh_proto::JobState::Failed,
            mesh_proto::JobState::Cancelled,
            mesh_proto::JobState::TimedOut,
        ]
        .into_iter()
        .find(|st| st.as_str() == s("state"))
        .unwrap_or(mesh_proto::JobState::Succeeded),
        exit_code: s("exit_code").parse::<i32>().ok(),
        created_at_micros: parse_stamp(s("created_at")),
        started_at_micros: Some(parse_stamp(s("started_at"))).filter(|m| *m > 0),
        finished_at_micros: Some(parse_stamp(s("finished_at"))).filter(|m| *m > 0),
        updated_hlc: mesh_proto::Hlc {
            wall_micros: parse_stamp(s("finished_at")),
            logical: 0,
        },
        // **The whole reason P0 put these in the archive row.** A1 pruned
        // the op-log row months ago; without this the six-month question
        // would find the record and not the bytes.
        payload_sink: opt("payload_sink"),
        payload_key: opt("payload_key"),
        from_archive: true,
        // Filled by whoever spans the tiers; the archive knows only
        // where the bytes were sent, never whether they are still there.
        output: None,
    })
}

/// Arrow renders a timestamp as `YYYY-MM-DDTHH:MM:SS[.ffffff]`; turn it
/// back into micros without a date library, the same arithmetic
/// `mesh-archive` uses in the other direction.
fn parse_stamp(text: &str) -> u64 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    let (date, rest) = match text.split_once(['T', ' ']) {
        Some(pair) => pair,
        None => return 0,
    };
    let mut d = date.split('-');
    let (y, m, day) = match (d.next(), d.next(), d.next()) {
        (Some(y), Some(m), Some(d)) => (
            y.parse::<i64>().unwrap_or(0),
            m.parse::<i64>().unwrap_or(0),
            d.parse::<i64>().unwrap_or(0),
        ),
        _ => return 0,
    };
    let rest = rest.trim_end_matches('Z');
    let (clock, frac) = rest.split_once('.').unwrap_or((rest, ""));
    let mut c = clock.split(':');
    let (hh, mm, ss) = (
        c.next().unwrap_or("0").parse::<i64>().unwrap_or(0),
        c.next().unwrap_or("0").parse::<i64>().unwrap_or(0),
        c.next().unwrap_or("0").parse::<i64>().unwrap_or(0),
    );
    // Howard Hinnant's `days_from_civil`, the inverse of the one
    // `mesh-archive` partitions with.
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3_600 + mm * 60 + ss;
    let micros = format!("{frac:0<6}")
        .chars()
        .take(6)
        .collect::<String>()
        .parse::<u64>()
        .unwrap_or(0);
    (secs.max(0) as u64) * 1_000_000 + micros
}

#[cfg(test)]
mod tests {
    /// **M-10**: `partition_of` here must name the same directory the
    /// writer chose, or the `date` bound would prune away the very files
    /// the query wants — a wrong answer, not a slow one.
    ///
    /// Checked against `parse_stamp`, this file's independent inverse of
    /// the same calendar algorithm: for a date string `d`, converting it to
    /// an instant and back must return `d`. Walks a leap day, a century
    /// non-leap boundary, and a four-hundred-year leap boundary, which is
    /// where Hinnant's algorithm earns its keep.
    #[test]
    fn partition_of_matches_the_writer() {
        for date in [
            "1970-01-01",
            "1999-12-31",
            "2000-02-29",
            "2024-02-29",
            "2026-09-07",
            "2100-03-01",
            "2400-02-29",
        ] {
            let micros = super::parse_stamp(&format!("{date} 00:00:00"));
            assert_eq!(super::partition_of(micros), date, "round trip for {date}");
            // And any instant within the day maps to the same partition.
            let late = micros + 86_399 * 1_000_000;
            assert_eq!(super::partition_of(late), date, "end of {date}");
        }
    }

    use super::*;

    #[test]
    fn a_browse_becomes_a_bounded_ordered_statement() {
        let q = HistoryQuery {
            limit: 50,
            offset: 100,
            ..Default::default()
        };
        let sql = build_sql(&q);
        assert!(sql.starts_with("SELECT * FROM jobs"), "{sql}");
        assert!(sql.contains("ORDER BY finished_at DESC"), "{sql}");
        assert!(sql.contains("LIMIT 50"), "{sql}");
        assert!(sql.contains("OFFSET 100"), "{sql}");
    }

    /// The row cap is ours, not the caller's — this statement is assembled
    /// from a query that crossed the wire.
    #[test]
    fn a_caller_cannot_ask_for_more_rows_than_the_cap() {
        let q = HistoryQuery {
            limit: 10_000_000,
            ..Default::default()
        };
        assert!(build_sql(&q).contains(&format!("LIMIT {ARCHIVE_ROW_CAP}")));
        // And zero is not a valid page either.
        let q = HistoryQuery {
            limit: 0,
            ..Default::default()
        };
        assert!(build_sql(&q).contains("LIMIT 1"));
    }

    /// **The escaping is the interface.** There is no parameterised form
    /// through a CLI, so a quote in a filter must not become a second
    /// clause.
    #[test]
    fn a_quote_in_a_filter_is_escaped_rather_than_closing_the_string() {
        let q = HistoryQuery {
            contains: Some("o'brien' OR '1'='1".into()),
            ..Default::default()
        };
        let sql = build_sql(&q);
        assert!(sql.contains("o''brien"), "{sql}");
        assert!(
            !sql.contains("OR '1'='1'"),
            "an injected clause survived: {sql}"
        );
    }

    #[test]
    fn a_window_becomes_timestamp_bounds_rather_than_integers() {
        let q = HistoryQuery {
            since_micros: Some(1_780_790_400_000_000),
            until_micros: Some(1_780_876_800_000_000),
            ..Default::default()
        };
        let sql = build_sql(&q);
        assert!(
            sql.contains("finished_at >= to_timestamp_micros(1780790400000000)"),
            "{sql}"
        );
        assert!(
            sql.contains("finished_at <= to_timestamp_micros(1780876800000000)"),
            "{sql}"
        );
    }

    #[test]
    fn timestamps_round_trip_through_the_archives_own_rendering() {
        // 2026-06-07T00:00:00Z, the instant A2's messages use.
        assert_eq!(parse_stamp("2026-06-07T00:00:00"), 1_780_790_400_000_000);
        assert_eq!(
            parse_stamp("2026-06-07T00:00:00.000000"),
            1_780_790_400_000_000
        );
        assert_eq!(
            parse_stamp("2026-06-07T00:00:00.500000Z"),
            1_780_790_400_500_000
        );
        assert_eq!(parse_stamp("1970-01-01T00:00:00"), 0);
        // Absent is zero rather than a panic — the archive writes null for
        // a job that never started.
        assert_eq!(parse_stamp(""), 0);
        assert_eq!(parse_stamp("not a date"), 0);
    }

    /// A node with no archive answers "nothing older", not an error. That
    /// is A2's known-absence rule one tier down: an empty result and a
    /// failure are different facts.
    #[tokio::test]
    async fn a_node_without_an_archive_has_no_older_history_and_says_so_quietly() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!has_archive(dir.path()));
        let rows = query(dir.path(), &HistoryQuery::default())
            .await
            .expect("no archive is not an error");
        assert!(rows.is_empty());
    }

    /// The locator P0 reserved and B2 filled survives the trip back out of
    /// Parquet — without it the six-month question finds the record and
    /// not the bytes.
    #[test]
    fn a_job_row_carries_its_payload_locator_back() {
        let v = serde_json::json!({
            "job_id": mesh_proto::JobId([3u8; 16]).to_display_string(),
            "command": "echo hi",
            "runtime": "bash",
            "state": "succeeded",
            "exit_code": "0",
            "created_at": "2026-06-07T00:00:00",
            "finished_at": "2026-06-07T00:00:04",
            "payload_sink": "warehouse",
            "payload_key": "mesh-aaaa/jobs/job-3",
        });
        let row = job_row_from(&v).expect("row");
        assert_eq!(row.payload_sink.as_deref(), Some("warehouse"));
        assert_eq!(row.payload_key.as_deref(), Some("mesh-aaaa/jobs/job-3"));
        assert_eq!(row.exit_code, Some(0));
        assert_eq!(row.state, mesh_proto::JobState::Succeeded);

        // A job that was never exported reads null rather than empty
        // strings pretending to be a locator.
        let mut bare = v.clone();
        bare["payload_sink"] = serde_json::json!("");
        bare["payload_key"] = serde_json::json!("");
        let row = job_row_from(&bare).expect("row");
        assert_eq!(row.payload_sink, None);
        assert_eq!(row.payload_key, None);
    }
}
