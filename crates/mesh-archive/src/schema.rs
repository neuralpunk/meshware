//! The Arrow schemas the archive writes, and the conversion into them.
//!
//! These are the *published* shape of meshware history. Somebody will point
//! pandas, DuckDB or Spark at these files in two years without meshware
//! installed, so the columns are named for what they mean rather than for
//! what the Rust struct calls them, ids are strings rather than 16-byte
//! blobs, and instants are real Arrow timestamps rather than integers.
//!
//! Adding a column is safe: Parquet readers match by name and a file
//! without it reads as null. Renaming or retyping one is not, and is a
//! format change.

use std::sync::Arc;

use datafusion::arrow::array::{
    ArrayRef, BinaryBuilder, Int32Builder, StringBuilder, TimestampMicrosecondBuilder,
    UInt64Builder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;

use mesh_proto::{HistoryJobRow, ResultRow, SignedAuditRow, UsageRow};

use crate::Error;

/// Bumped only for a change a reader cannot absorb — a rename, a retype, a
/// dropped column. New columns do not bump it.
///
/// **2 (M31 P0)** adds `mesh_id` as the first column of every table, and
/// `payload_sink` / `payload_key` to `runs` and `jobs`. Adding columns does
/// not normally bump this, and strictly it did not have to here either —
/// but the *cursor* changed shape in the same commit (see
/// [`crate::Manifest::runs_watermark_id`]), and a manifest written by a
/// build that only compares instants would skip rows a format-2 build
/// wrote. That a reader cannot absorb, so the number moves.
///
/// A format-1 archive is still **read**, with `mesh_id` projected as the
/// local mesh's id — see [`crate::Archive::session`]. It is not refused:
/// rebuilding costs a full re-page of history, and the projection is
/// correct for every single-mesh archive, which is all of them today.
pub const ARCHIVE_FORMAT: u32 = 2;

/// The column every table now leads with: which mesh these rows came from.
///
/// First rather than appended, because it is the coarsest partition key a
/// reader will ever filter on and a `SELECT *` should put it where the eye
/// lands. Non-nullable, because a row whose mesh is unknown is a row nobody
/// can safely union with another archive — which is the entire reason the
/// column exists.
pub const MESH_ID_COLUMN: &str = "mesh_id";

/// The two columns B2 fills and B3 reads: where a payload was exported to,
/// and under what key.
///
/// Reserved here, written null, and deliberately so. Their *shape* is
/// knowable today and does not depend on which destinations ever exist — a
/// sink name and an opaque key — while discovering them in B3 would cost a
/// third format and an `ALTER` on every customer's warehouse. Two nullable
/// columns in a format bump that was already happening is the cheapest item
/// in M31; missing it is the most annoying.
pub const PAYLOAD_SINK_COLUMN: &str = "payload_sink";
pub const PAYLOAD_KEY_COLUMN: &str = "payload_key";

fn ts() -> DataType {
    // No timezone: these are absolute instants in UTC, and stamping a
    // timezone on them would imply the reader should shift them.
    DataType::Timestamp(TimeUnit::Microsecond, None)
}

/// One row per job.
pub fn jobs_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(MESH_ID_COLUMN, DataType::Utf8, false),
        Field::new("job_id", DataType::Utf8, false),
        Field::new("command", DataType::Utf8, false),
        Field::new("runtime", DataType::Utf8, false),
        Field::new("submitter_node", DataType::Utf8, false),
        Field::new("submitter_user", DataType::Utf8, false),
        Field::new("executor_node", DataType::Utf8, false),
        Field::new("state", DataType::Utf8, false),
        Field::new("exit_code", DataType::Int32, true),
        Field::new("created_at", ts(), false),
        Field::new("started_at", ts(), true),
        Field::new("finished_at", ts(), true),
        Field::new("duration_ms", DataType::UInt64, true),
        // Written null until B2. See PAYLOAD_SINK_COLUMN.
        Field::new(PAYLOAD_SINK_COLUMN, DataType::Utf8, true),
        Field::new(PAYLOAD_KEY_COLUMN, DataType::Utf8, true),
    ]))
}

/// Micros to an optional Arrow timestamp. Zero means "never set" throughout
/// meshware, and writing 1970-01-01 into an archive somebody will chart is
/// worse than writing null.
fn opt_ts(micros: Option<u64>) -> Option<i64> {
    micros.filter(|m| *m > 0).map(|m| m as i64)
}

/// A column holding the same string on every row.
///
/// `mesh_id` is constant per archive, so this is what "first column,
/// non-nullable" costs at write time: one repeated string, which Parquet's
/// dictionary encoding stores once per row group.
fn constant_column(value: &str, rows: usize) -> ArrayRef {
    let mut b = StringBuilder::new();
    for _ in 0..rows {
        b.append_value(value);
    }
    Arc::new(b.finish())
}

pub fn jobs_batch(mesh_id: &str, rows: &[HistoryJobRow]) -> Result<RecordBatch, Error> {
    let mut job_id = StringBuilder::new();
    let mut command = StringBuilder::new();
    let mut runtime = StringBuilder::new();
    let mut submitter = StringBuilder::new();
    let mut user = StringBuilder::new();
    let mut executor = StringBuilder::new();
    let mut state = StringBuilder::new();
    let mut exit = Int32Builder::new();
    let mut created = TimestampMicrosecondBuilder::new();
    let mut started = TimestampMicrosecondBuilder::new();
    let mut finished = TimestampMicrosecondBuilder::new();
    let mut duration = UInt64Builder::new();
    // M31 B2 fills what P0 reserved. This is §7's third crossing arriving:
    // A1 prunes the op-log row long before anyone asks the six-month
    // question, so the locator has to reach the **archive** row or it is
    // gone exactly when it is needed.
    let mut payload_sink = StringBuilder::new();
    let mut payload_key = StringBuilder::new();

    for r in rows {
        payload_sink.append_option(r.payload_sink.as_deref());
        payload_key.append_option(r.payload_key.as_deref());
        job_id.append_value(r.job_id.to_display_string());
        command.append_value(&r.cmd_preview);
        runtime.append_value(&r.runtime);
        submitter.append_value(r.submitter.to_display_string());
        user.append_value(r.submitter_user.to_display_string());
        executor.append_value(r.executor.to_display_string());
        state.append_value(r.state.as_str());
        exit.append_option(r.exit_code);
        created.append_value(r.created_at_micros as i64);
        started.append_option(opt_ts(r.started_at_micros));
        finished.append_option(opt_ts(r.finished_at_micros));
        duration.append_option(r.duration_micros().map(|d| d / 1000));
    }

    let columns: Vec<ArrayRef> = vec![
        constant_column(mesh_id, rows.len()),
        Arc::new(job_id.finish()),
        Arc::new(command.finish()),
        Arc::new(runtime.finish()),
        Arc::new(submitter.finish()),
        Arc::new(user.finish()),
        Arc::new(executor.finish()),
        Arc::new(state.finish()),
        Arc::new(exit.finish()),
        Arc::new(created.finish()),
        Arc::new(started.finish()),
        Arc::new(finished.finish()),
        Arc::new(duration.finish()),
        Arc::new(payload_sink.finish()),
        Arc::new(payload_key.finish()),
    ];
    RecordBatch::try_new(jobs_schema(), columns).map_err(|e| Error::Encode(e.to_string()))
}

// ---------------------------------------------------------------------------
// Usage (C1 P1)
// ---------------------------------------------------------------------------

/// One row per `(node, principal, window)`: what one machine charged one
/// principal in one hour.
///
/// Deliberately **not** aggregated on the way in. An archive that stored
/// daily totals per user would answer the question somebody asked this year
/// and none of the ones they ask next year; hourly partials answer both,
/// and a `GROUP BY` costs the reader nothing.
///
/// The resource columns are seconds *reserved*, and the column names say
/// so — `cpu_secs_reserved`, not `cpu_secs` — because in two years somebody
/// will point pandas at this without reading a word of documentation, and
/// the difference between reserved and consumed is exactly the thing they
/// must not guess wrong.
pub fn usage_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(MESH_ID_COLUMN, DataType::Utf8, false),
        Field::new("window_start", ts(), false),
        Field::new("node", DataType::Utf8, false),
        Field::new("principal", DataType::Utf8, false),
        Field::new("cpu_secs_reserved", DataType::UInt64, false),
        Field::new("mem_mb_secs_reserved", DataType::UInt64, false),
        Field::new("gpu_secs_reserved", DataType::UInt64, false),
        Field::new("wall_secs", DataType::UInt64, false),
        Field::new("jobs", DataType::UInt64, false),
    ]))
}

pub fn usage_batch(mesh_id: &str, rows: &[UsageRow]) -> Result<RecordBatch, Error> {
    let mut window = TimestampMicrosecondBuilder::new();
    let mut node = StringBuilder::new();
    let mut principal = StringBuilder::new();
    let mut cpu = UInt64Builder::new();
    let mut mem = UInt64Builder::new();
    let mut gpu = UInt64Builder::new();
    let mut wall = UInt64Builder::new();
    let mut jobs = UInt64Builder::new();

    for r in rows {
        window.append_value(r.at_micros() as i64);
        node.append_value(r.node.to_display_string());
        principal.append_value(r.principal.to_display_string());
        cpu.append_value(r.cpu_secs);
        mem.append_value(r.mem_mb_secs);
        gpu.append_value(r.gpu_secs);
        wall.append_value(r.wall_secs);
        jobs.append_value(r.jobs);
    }

    let columns: Vec<ArrayRef> = vec![
        constant_column(mesh_id, rows.len()),
        Arc::new(window.finish()),
        Arc::new(node.finish()),
        Arc::new(principal.finish()),
        Arc::new(cpu.finish()),
        Arc::new(mem.finish()),
        Arc::new(gpu.finish()),
        Arc::new(wall.finish()),
        Arc::new(jobs.finish()),
    ];
    RecordBatch::try_new(usage_schema(), columns).map_err(|e| Error::Encode(e.to_string()))
}

// ---------------------------------------------------------------------------
// Audit (M31 B4)
// ---------------------------------------------------------------------------

/// One row per audited action, **with the proof that meshware signed it**.
///
/// The three binary columns are the phase's whole point (§19 Q1, decided).
/// `node` is a `NodeId`, and a `NodeId` *is* an Ed25519 public key, so a
/// reader holding nothing but this table can check every row:
///
/// ```sql
/// -- the verifier ships in the row; meshware does not have to
/// SELECT node, signature, signing_bytes FROM audit WHERE ...
/// ```
///
/// `signing_bytes` is carried rather than derived on purpose. Deriving it
/// would save about a hundred bytes a row and would make verification
/// depend on owning a meshware build whose op framing had not changed since
/// the row was written — which is precisely the dependency a compliance
/// archive exists to remove.
///
/// `request_sig` is the **user's** signature over the request that caused
/// the action. It is not verifiable from this table (the signed payload is
/// not carried) and is exported anyway, because it ties a recorded action
/// to a client envelope. Two signatures, one verifiable; the column
/// comments in any generated DDL say which is which.
pub fn audit_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(MESH_ID_COLUMN, DataType::Utf8, false),
        Field::new("audit_id", DataType::Utf8, false),
        Field::new("at", ts(), false),
        Field::new("user", DataType::Utf8, false),
        Field::new("username", DataType::Utf8, false),
        Field::new("action", DataType::Utf8, false),
        Field::new("target", DataType::Utf8, false),
        // The authoring node — and the public key that verifies
        // `signature`. Non-nullable: a row nobody can attribute is a row
        // nobody can verify.
        Field::new("node", DataType::Utf8, false),
        Field::new("outcome", DataType::Utf8, false),
        Field::new("detail", DataType::Utf8, false),
        Field::new("seq", DataType::UInt64, false),
        Field::new("signature", DataType::Binary, false),
        Field::new("signing_bytes", DataType::Binary, false),
        // Nullable, and it genuinely is: node-internal events (a secret
        // resolution, a rate-limit refusal) have no user request behind
        // them, and writing an empty blob would imply one existed.
        Field::new("request_sig", DataType::Binary, true),
    ]))
}

pub fn audit_batch(mesh_id: &str, rows: &[SignedAuditRow]) -> Result<RecordBatch, Error> {
    let mut audit_id = StringBuilder::new();
    let mut at = TimestampMicrosecondBuilder::new();
    let mut user = StringBuilder::new();
    let mut username = StringBuilder::new();
    let mut action = StringBuilder::new();
    let mut target = StringBuilder::new();
    let mut node = StringBuilder::new();
    let mut outcome = StringBuilder::new();
    let mut detail = StringBuilder::new();
    let mut seq = UInt64Builder::new();
    let mut signature = BinaryBuilder::new();
    let mut signing_bytes = BinaryBuilder::new();
    let mut request_sig = BinaryBuilder::new();

    for r in rows {
        audit_id.append_value(r.entry.audit_id.to_display_string());
        at.append_value(r.entry.at_micros as i64);
        user.append_value(r.entry.user.to_display_string());
        username.append_value(&r.entry.username);
        action.append_value(&r.entry.action);
        target.append_value(&r.entry.target);
        // The **author**, not `entry.node`. They agree in every row this
        // mesh writes, and the signature is checked against this one — so
        // the exported column has to be the one the proof used.
        node.append_value(r.author.to_display_string());
        outcome.append_value(&r.entry.outcome);
        detail.append_value(&r.entry.detail);
        seq.append_value(r.seq);
        signature.append_value(&r.signature);
        signing_bytes.append_value(&r.signing_bytes);
        if r.entry.request_sig.is_empty() {
            request_sig.append_null();
        } else {
            request_sig.append_value(&r.entry.request_sig);
        }
    }

    let columns: Vec<ArrayRef> = vec![
        constant_column(mesh_id, rows.len()),
        Arc::new(audit_id.finish()),
        Arc::new(at.finish()),
        Arc::new(user.finish()),
        Arc::new(username.finish()),
        Arc::new(action.finish()),
        Arc::new(target.finish()),
        Arc::new(node.finish()),
        Arc::new(outcome.finish()),
        Arc::new(detail.finish()),
        Arc::new(seq.finish()),
        Arc::new(signature.finish()),
        Arc::new(signing_bytes.finish()),
        Arc::new(request_sig.finish()),
    ];
    RecordBatch::try_new(audit_schema(), columns).map_err(|e| Error::Encode(e.to_string()))
}

// ---------------------------------------------------------------------------
// Facts (M18 §4.8)
// ---------------------------------------------------------------------------

/// One row per observation: what a collector said about a subject, and when.
///
/// The **document stays JSON**, in one string column, rather than being
/// exploded into columns. Two reasons, and the second is the load-bearing
/// one. A fact document's keys are whatever a collector prints, so a
/// column-per-key schema would be a schema nobody controls; and DuckDB,
/// pandas and Spark all read JSON out of a string column happily —
/// `SELECT json_extract_string(doc, '$.os_id')` costs a reader nothing and
/// costs meshware no format change every time somebody adds a collector.
pub fn facts_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(MESH_ID_COLUMN, DataType::Utf8, false),
        Field::new("subject", DataType::Utf8, false),
        Field::new("collector", DataType::Utf8, false),
        Field::new("observed_at", ts(), false),
        Field::new("doc", DataType::Utf8, false),
        Field::new("digest", DataType::Utf8, false),
    ]))
}

/// One archived observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactArchiveRow {
    pub subject: String,
    pub collector: String,
    pub observed_at_micros: u64,
    pub doc: String,
    pub digest: String,
}

pub fn facts_batch(mesh_id: &str, rows: &[FactArchiveRow]) -> Result<RecordBatch, Error> {
    let mut subject = StringBuilder::new();
    let mut collector = StringBuilder::new();
    let mut observed = TimestampMicrosecondBuilder::new();
    let mut doc = StringBuilder::new();
    let mut digest = StringBuilder::new();
    for r in rows {
        subject.append_value(&r.subject);
        collector.append_value(&r.collector);
        observed.append_value(r.observed_at_micros as i64);
        doc.append_value(&r.doc);
        digest.append_value(&r.digest);
    }
    let columns: Vec<ArrayRef> = vec![
        constant_column(mesh_id, rows.len()),
        Arc::new(subject.finish()),
        Arc::new(collector.finish()),
        Arc::new(observed.finish()),
        Arc::new(doc.finish()),
        Arc::new(digest.finish()),
    ];
    RecordBatch::try_new(facts_schema(), columns).map_err(|e| Error::Encode(e.to_string()))
}

// ---------------------------------------------------------------------------
// Results (M19 §5.7)
// ---------------------------------------------------------------------------

/// One row per fan-out target: what that node answered.
///
/// `outputs` is JSON for the same reason a fact document is: the keys are
/// whatever the job published, and a column per key would be a schema
/// nobody controls.
pub fn results_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(MESH_ID_COLUMN, DataType::Utf8, false),
        Field::new("group", DataType::Utf8, false),
        Field::new("job_id", DataType::Utf8, false),
        Field::new("node", DataType::Utf8, false),
        Field::new("state", DataType::Utf8, false),
        Field::new("exit_code", DataType::Int32, true),
        Field::new("finished_at", ts(), true),
        Field::new("outputs", DataType::Utf8, false),
    ]))
}

pub fn results_batch(mesh_id: &str, group: &str, rows: &[ResultRow]) -> Result<RecordBatch, Error> {
    let mut group_col = StringBuilder::new();
    let mut job_id = StringBuilder::new();
    let mut node = StringBuilder::new();
    let mut state = StringBuilder::new();
    let mut exit = Int32Builder::new();
    let mut finished = TimestampMicrosecondBuilder::new();
    let mut outputs = StringBuilder::new();
    for r in rows {
        group_col.append_value(group);
        job_id.append_value(r.job_id.to_display_string());
        node.append_value(&r.node_name);
        state.append_value(r.state.as_str());
        exit.append_option(r.exit_code);
        finished.append_option(opt_ts(r.finished_at_micros));
        let map: serde_json::Map<String, serde_json::Value> = r
            .outputs
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        outputs.append_value(serde_json::Value::Object(map).to_string());
    }
    let columns: Vec<ArrayRef> = vec![
        constant_column(mesh_id, rows.len()),
        Arc::new(group_col.finish()),
        Arc::new(job_id.finish()),
        Arc::new(node.finish()),
        Arc::new(state.finish()),
        Arc::new(exit.finish()),
        Arc::new(finished.finish()),
        Arc::new(outputs.finish()),
    ];
    RecordBatch::try_new(results_schema(), columns).map_err(|e| Error::Encode(e.to_string()))
}

/// The same schema, with `mesh_id` relaxed to nullable.
///
/// Used only to *register* a table whose directory may hold Parquet files
/// written before format 2. Those files have no `mesh_id` column at all, and
/// a listing table told to expect a non-nullable one errors rather than
/// filling nulls — so the declared read schema has to admit the null that
/// the file will produce. [`crate::Archive::session`] then wraps the table
/// in a view that coalesces the null away, which is where the *projection*
/// of the local mesh id actually happens.
///
/// Nothing writes through this. The published shape stays non-nullable, and
/// a format-2 archive is registered with its real schema.
pub fn read_schema_lenient(schema: &Schema) -> Arc<Schema> {
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|f| {
            if f.name() == MESH_ID_COLUMN {
                Field::new(f.name(), f.data_type().clone(), true)
            } else {
                (**f).clone()
            }
        })
        .collect();
    Arc::new(Schema::new(fields))
}

/// The `date=` partition a row belongs to, from its sort instant.
///
/// UTC, always. A partition boundary that moves with the reader's timezone
/// would mean the same run lands in different files on two machines, and
/// the whole point of these files is that they are the same everywhere.
pub fn partition_of(micros: u64) -> String {
    let days = (micros / 1_000_000) as i64 / 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the Unix epoch to a civil date (Howard Hinnant's algorithm).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_are_utc_calendar_days() {
        // 2026-08-22T00:00:00Z and one microsecond before it.
        let midnight = 1_787_356_800_000_000u64;
        assert_eq!(partition_of(midnight), "2026-08-22");
        assert_eq!(partition_of(midnight - 1), "2026-08-21");
    }

    #[test]
    fn the_epoch_itself_is_day_zero() {
        assert_eq!(partition_of(0), "1970-01-01");
    }
}
