//! SQL over the archive.
//!
//! Two tables, `runs` and `jobs`, registered over the Parquet directories
//! with the `date=` partition exposed as a column. DataFusion prunes both
//! by partition and by row-group statistics, so `WHERE date >= '2026-01-01'`
//! never opens a file from 2025.
//!
//! ## Why raw SQL is offered at all
//!
//! Because the alternative is guessing which questions people have. The
//! index in `mesh-core` covers the five queries a UI needs and covers them
//! without a query engine; this covers the sixth one, whatever it turns out
//! to be — "median duration by workflow this quarter", "which node fails
//! most often on Tuesdays", "every run that touched the `payments` service".
//! Those are not queries to anticipate. They are queries to permit.
//!
//! ## Read-only, and how
//!
//! Not "by construction" — that claim used to be here and it was wrong (S4).
//! Nothing in this module registers a writable table, but `SessionContext`
//! arrives with DataFusion's defaults, and those include DDL and `COPY`:
//! `CREATE EXTERNAL TABLE leak STORED AS CSV LOCATION '/etc/passwd'` reads
//! any file the process can read, and `COPY (SELECT 1) TO '~/.bashrc'`
//! writes one. A comment is not a control.
//!
//! The control is [`SQLOptions`]: every statement is planned first and the
//! plan is rejected if it contains DDL, DML/`COPY`, or a session statement —
//! *including inside a subquery*, which is why this is a plan walk and not a
//! string check. What is left is `SELECT` and `EXPLAIN`, over the tables
//! registered below and nothing else: there is no information schema (this
//! `SessionContext` does not enable one) and the only table functions in
//! scope are `generate_series` and `range`, neither of which reads a file.
//! DataFusion ships no file-reading table function at all.

use std::sync::Arc;

use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::prelude::{SQLOptions, SessionContext};

use crate::schema::jobs_schema;
use crate::{Archive, Error, Result};

/// What an archive holds.
///
/// `facts` (M18 §4.8) and `results` (M19 §5.7) join `runs` and `jobs`
/// because they answer the questions those two cannot: *what did this fleet
/// look like in March*, and *what did those six hundred boxes actually say
/// when we asked*. Both belong in the archive and **never in the daemon** —
/// DataFusion is confined to this crate for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    /// `Runs` was here until 2026-09-20. It held workflow runs, and the
    /// engine that wrote them went in September; existing `runs/` day
    /// partitions are still on disk and still readable by anything that
    /// reads Parquet, which is the promise this tier makes.
    Jobs,
    Facts,
    Results,
    Usage,
    /// The audit trail, with the signatures that prove it (M31 B4).
    Audit,
}

impl Table {
    pub fn dir_name(self) -> &'static str {
        match self {
            Table::Jobs => "jobs",
            Table::Facts => "facts",
            Table::Results => "results",
            Table::Usage => "usage",
            Table::Audit => "audit",
        }
    }

    /// The SQL name — the same word, which is the point.
    pub fn sql_name(self) -> &'static str {
        self.dir_name()
    }

    pub fn schema(self) -> Arc<Schema> {
        match self {
            Table::Jobs => jobs_schema(),
            Table::Facts => crate::schema::facts_schema(),
            Table::Results => crate::schema::results_schema(),
            Table::Usage => crate::schema::usage_schema(),
            Table::Audit => crate::schema::audit_schema(),
        }
    }

    pub const ALL: [Table; 5] = [
        Table::Jobs,
        Table::Facts,
        Table::Results,
        Table::Usage,
        Table::Audit,
    ];
}

/// One archived audit row's proof, with the bytes still bytes (M31 B4).
///
/// `node` is a `mw1…` display string of a `NodeId`, and a `NodeId` **is**
/// an Ed25519 public key — so this struct is self-verifying: decode the
/// node id, check `signature` over `signing_bytes`, done. No meshware
/// required, which is the entire point of exporting it this way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditProof {
    pub audit_id: String,
    pub at_micros: u64,
    pub node: String,
    pub signature: Vec<u8>,
    pub signing_bytes: Vec<u8>,
}

/// The answer to a query: column names plus rows of already-stringified
/// values.
///
/// Strings rather than a typed value enum on purpose. Every consumer of
/// this — a table in Director, `--output json`, a CSV dump — renders to
/// text anyway, and Arrow's own formatter already knows how to render a
/// timestamp or a null correctly. Re-deriving that badly, twice, is how a
/// UI ends up showing `1787356800000000` in a date column.
#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Rows the query produced before [`Archive::sql`]'s cap applied.
    pub truncated: bool,
}

impl QueryResult {
    /// As `[{column: value, …}, …]`, for IPC and for `--output json`.
    pub fn to_json(&self) -> Vec<serde_json::Value> {
        self.rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (name, value) in self.columns.iter().zip(row) {
                    obj.insert(name.clone(), serde_json::Value::String(value.clone()));
                }
                serde_json::Value::Object(obj)
            })
            .collect()
    }
}

/// Rows one query may return. A query engine will happily materialize ten
/// million rows into a UI that wanted twenty; this is the guard rail.
pub const QUERY_ROW_CAP: usize = 10_000;

/// What a query is allowed to be (S4).
///
/// `SELECT` and `EXPLAIN`. No `CREATE`, no `INSERT`, no `COPY`, no `SET` —
/// the three things DataFusion enables by default that turn "ask a question
/// about history" into "read and write files as this process". Verified
/// against the *plan*, subqueries included.
fn read_only() -> SQLOptions {
    SQLOptions::new()
        .with_allow_ddl(false)
        .with_allow_dml(false)
        .with_allow_statements(false)
}

impl Archive {
    /// Build a session with `runs` and `jobs` registered.
    ///
    /// A table whose directory does not exist yet is simply not registered,
    /// so querying a fresh archive gives "table not found" rather than an
    /// internal error about a missing path.
    pub async fn session(&self) -> Result<SessionContext> {
        let ctx = SessionContext::new();
        // A directory that may hold Parquet written before `mesh_id`
        // existed is registered under a private name and wrapped in a view
        // that fills the column in (M31 P0). A format-2 archive — every
        // archive this build creates — skips all of that and registers its
        // tables directly, because the common path should not pay for a
        // migration it does not need.
        let legacy = self.has_legacy_parts();
        for table in Table::ALL {
            let dir = self.table_dir(table);
            // A table with no directory has had nothing archived into it
            // yet. Registering it anyway would mean inventing a file
            // listing; leaving it out means a query against it is a
            // planner error, which `friendly` turns into the honest
            // sentence — "nothing has been archived into `facts` yet"
            // rather than "no such table".
            if !dir.is_dir() {
                continue;
            }
            let url = ListingTableUrl::parse(format!("file://{}/", dir.display()))
                .map_err(|e| Error::Query(e.to_string()))?;
            let options = ListingOptions::new(Arc::new(ParquetFormat::default()))
                .with_file_extension(".parquet")
                // `date=YYYY-MM-DD` becomes a `date` column DataFusion can
                // filter on without reading a single file.
                .with_table_partition_cols(vec![(
                    "date".to_string(),
                    datafusion::arrow::datatypes::DataType::Utf8,
                )]);
            let declared = if legacy {
                // `mesh_id` relaxed to nullable, because a format-1 file
                // genuinely has no such column and a listing table told to
                // expect a non-nullable one errors rather than filling it.
                crate::read_schema_lenient(&table.schema())
            } else {
                table.schema()
            };
            let config = ListingTableConfig::new(url)
                .with_listing_options(options)
                // The schema is declared, not inferred. Inference reads a
                // file to guess, which means an archive whose oldest
                // partition predates a column would type it differently
                // depending on which file got sampled.
                .with_schema(with_partition_column(&declared));
            let provider =
                ListingTable::try_new(config).map_err(|e| Error::Query(e.to_string()))?;
            let name = if legacy {
                format!("{}{LEGACY_SUFFIX}", table.sql_name())
            } else {
                table.sql_name().to_string()
            };
            ctx.register_table(&name, Arc::new(provider))
                .map_err(|e| Error::Query(e.to_string()))?;
            if legacy {
                ctx.sql(&legacy_view_sql(table, self.mesh_id()))
                    .await
                    .map_err(|e| Error::Query(e.to_string()))?;
            }
        }
        Ok(ctx)
    }

    /// Run one SQL statement against the archive.
    /// Every audit row in the archive, with its bytes still bytes
    /// (M31 B4).
    ///
    /// A typed read rather than [`Archive::sql`], and the reason is the
    /// thing this exists for: `QueryResult` stringifies every column so a
    /// table renders, and a signature that has been through a display
    /// formatter is not a signature any more. Verification needs the exact
    /// bytes, so this reaches into the Arrow arrays.
    ///
    /// Returns `(node, signature, signing_bytes, audit_id, at)` per row —
    /// enough to check the proof and to name the row that failed.
    pub async fn audit_proofs(&self) -> Result<Vec<AuditProof>> {
        use datafusion::arrow::array::{BinaryArray, StringArray, TimestampMicrosecondArray};

        if !self.table_dir(Table::Audit).is_dir() {
            return Ok(Vec::new());
        }
        let ctx = self.session().await?;
        let frame = ctx
            .sql("SELECT audit_id, at, node, signature, signing_bytes FROM audit ORDER BY at")
            .await
            .map_err(|e| Error::Query(e.to_string()))?;
        let batches = frame
            .collect()
            .await
            .map_err(|e| Error::Query(e.to_string()))?;

        let mut out = Vec::new();
        for batch in &batches {
            let col = |i: usize| batch.column(i).as_ref();
            let ids = col(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| Error::Query("audit_id is not a string column".into()))?;
            let at = col(1)
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .ok_or_else(|| Error::Query("at is not a timestamp column".into()))?;
            let node = col(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| Error::Query("node is not a string column".into()))?;
            let sig = col(3)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| Error::Query("signature is not a binary column".into()))?;
            let bytes = col(4)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| Error::Query("signing_bytes is not a binary column".into()))?;

            for i in 0..batch.num_rows() {
                out.push(AuditProof {
                    audit_id: ids.value(i).to_string(),
                    at_micros: at.value(i).max(0) as u64,
                    node: node.value(i).to_string(),
                    signature: sig.value(i).to_vec(),
                    signing_bytes: bytes.value(i).to_vec(),
                });
            }
        }
        Ok(out)
    }

    /// Run one **read-only** statement against the archive.
    ///
    /// The `SQLOptions` are the whole security story of this surface and are
    /// applied on every path in and out of it (S4): `meshware-archive query`
    /// and anything else that ever calls this. Refusing here rather than at
    /// each caller is deliberate — a denylist on the query string would be
    /// approximately right, and this is exactly right.
    ///
    /// Director had a `archive_query` command too, registered and invoked by
    /// nothing; it is gone rather than merely restricted, because the right
    /// shape for a loaded surface with no user is not a safer version of it.
    pub async fn sql(&self, query: &str) -> Result<QueryResult> {
        let ctx = self.session().await?;
        let present: Vec<Table> = Table::ALL
            .into_iter()
            .filter(|t| self.table_dir(*t).is_dir())
            .collect();
        let explain = |e: String| Error::Query(friendly_with(e, &present));
        let frame = ctx
            .sql_with_options(query, read_only())
            .await
            .map_err(|e| explain(e.to_string()))?;
        let schema = frame.schema().clone();
        let batches = frame.collect().await.map_err(|e| explain(e.to_string()))?;

        let columns: Vec<String> = schema
            .fields()
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        let mut rows = Vec::new();
        let mut truncated = false;
        let opts = FormatOptions::default().with_null("");

        'outer: for batch in &batches {
            let formatters: Vec<_> = batch
                .columns()
                .iter()
                .map(|c| ArrayFormatter::try_new(c.as_ref(), &opts))
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| Error::Query(e.to_string()))?;
            for i in 0..batch.num_rows() {
                if rows.len() >= QUERY_ROW_CAP {
                    truncated = true;
                    break 'outer;
                }
                rows.push(
                    formatters
                        .iter()
                        .map(|f| f.value(i).to_string())
                        .collect::<Vec<_>>(),
                );
            }
        }

        Ok(QueryResult {
            columns,
            rows,
            truncated,
        })
    }

    /// The queries worth putting in front of someone who has never written
    /// SQL against this. Shown by `meshware-archive query` with no statement,
    /// because a blank SQL box with no schema in sight is a feature nobody
    /// uses.
    pub fn examples() -> &'static [(&'static str, &'static str)] {
        &[
            // Jobs first (DOC-11): they are what every mesh produces. The
            // examples used to lead with `runs`, which only the retired
            // workflow engine ever wrote, so the first query a customer
            // tried came back empty.
            (
                "Failures this month",
                "SELECT date, job_id, command, executor_node, exit_code FROM jobs \
                 WHERE state IN ('failed', 'timed-out') AND date >= '2026-08-01' \
                 ORDER BY finished_at DESC",
            ),
            (
                "Slowest commands",
                "SELECT command, count(*) AS jobs, \
                 round(avg(duration_ms)/1000, 1) AS avg_secs \
                 FROM jobs GROUP BY command ORDER BY avg_secs DESC LIMIT 20",
            ),
            (
                "Success rate by command",
                "SELECT command, count(*) AS jobs, \
                 round(100.0 * sum(CASE WHEN state = 'succeeded' THEN 1 ELSE 0 END) \
                 / count(*), 1) AS pct_ok \
                 FROM jobs GROUP BY command ORDER BY pct_ok ASC",
            ),
            (
                "Busiest executors",
                "SELECT executor_node, count(*) AS jobs, \
                 round(sum(duration_ms)/60000.0, 1) AS total_minutes \
                 FROM jobs GROUP BY executor_node ORDER BY jobs DESC",
            ),
            (
                "Jobs per day",
                "SELECT date, count(*) AS jobs FROM jobs \
                 GROUP BY date ORDER BY date DESC LIMIT 30",
            ),
            // C1: chargeback, from the hourly partials. The numbers are
            // resource-seconds *reserved* — the column names say so, because
            // this is exactly where somebody reads them without reading a
            // word of documentation.
            (
                "Who used the mesh last month",
                "SELECT principal, \
                 round(sum(cpu_secs_reserved)/3600.0, 1) AS cpu_hours, \
                 round(sum(mem_mb_secs_reserved)/1024.0/3600.0, 1) AS gb_hours, \
                 sum(jobs) AS jobs \
                 FROM usage GROUP BY principal ORDER BY cpu_hours DESC",
            ),
            (
                "Daily consumption by team",
                "SELECT date, principal, \
                 round(sum(cpu_secs_reserved)/3600.0, 1) AS cpu_hours \
                 FROM usage GROUP BY date, principal ORDER BY date DESC, cpu_hours DESC",
            ),
            // M18: the question the whole milestone exists for, asked
            // about a fleet that no longer exists.
            //
            // `doc` is a JSON string, so a key comes out with a regex here
            // and with `json_extract_string` in DuckDB. That is the price
            // of not inventing a column per key a collector might print,
            // and it is the right price.
            // -- audit (M31 B4) ----------------------------------------
            //
            // The two questions an auditor actually arrives with, and they
            // are different: *"who did this"* is a search, and *"can I
            // trust the answer"* is `meshware-archive verify`. The examples
            // cover the first and the CLI covers the second, because a SQL
            // example that pretended to check a signature would be teaching
            // somebody to trust a `LENGTH(signature) = 64`.
            (
                "Who ran what, newest first",
                "SELECT at, username, action, target, outcome \
                 FROM audit ORDER BY at DESC LIMIT 100",
            ),
            (
                "Everything one person did",
                "SELECT at, action, target, outcome, detail \
                 FROM audit WHERE username = 'ada' ORDER BY at DESC",
            ),
            (
                "Refusals and denials, by action",
                "SELECT action, count(*) AS denials FROM audit \
                 WHERE outcome NOT LIKE 'ok%' GROUP BY action ORDER BY denials DESC",
            ),
            (
                "OS mix over time",
                "SELECT date, \
                 regexp_replace(doc, '^.*\"os_id\":\"([^\"]+)\".*$', '$1') AS os, \
                 count(DISTINCT subject) AS nodes \
                 FROM facts WHERE collector = 'system' \
                 GROUP BY date, os ORDER BY date DESC, nodes DESC",
            ),
            (
                "Every kernel change, newest first",
                "SELECT observed_at, subject, \
                 regexp_replace(doc, '^.*\"kernel_release\":\"([^\"]+)\".*$', '$1') AS kernel \
                 FROM facts WHERE collector = 'system' \
                 ORDER BY subject, observed_at DESC",
            ),
            // M19: the answer, an hour or a year after the question.
            (
                "Fan-outs with a failing target",
                "SELECT \"group\", count(*) AS targets, \
                 sum(CASE WHEN state <> 'succeeded' THEN 1 ELSE 0 END) AS failed \
                 FROM results GROUP BY \"group\" \
                 HAVING sum(CASE WHEN state <> 'succeeded' THEN 1 ELSE 0 END) > 0 \
                 ORDER BY failed DESC LIMIT 50",
            ),
        ]
    }
}

/// What a pre-`mesh_id` table is registered as before the view goes over it.
const LEGACY_SUFFIX: &str = "__parts";

/// The view that makes a format-1 archive read like a format-2 one.
///
/// `COALESCE(mesh_id, '<local>')` rather than a plain literal, because a
/// legacy directory that has since been ingested into holds *both* kinds of
/// file: the old rows have a null there and the new ones already carry the
/// right value. One expression covers both, and a partition that compaction
/// has rewritten (see `write::upgrade_batch`) simply stops producing nulls.
///
/// With no local mesh — a restored archive on a machine that has never been
/// in a mesh — the column reads null rather than guessing. A wrong mesh id
/// is worse than an absent one, because only one of them is visibly absent.
fn legacy_view_sql(table: Table, mesh_id: Option<&str>) -> String {
    let projected = match mesh_id {
        // Single-quote escaping: mesh ids are `mesh-` plus base32, so this
        // cannot fire today. It costs one line and outlives the assumption.
        Some(id) => format!(
            "COALESCE({m}, '{}')",
            id.replace('\'', "''"),
            m = crate::MESH_ID_COLUMN
        ),
        None => format!("CAST({} AS VARCHAR)", crate::MESH_ID_COLUMN),
    };
    let rest: Vec<String> = table
        .schema()
        .fields()
        .iter()
        .filter(|f| f.name() != crate::MESH_ID_COLUMN)
        .map(|f| format!("\"{}\"", f.name()))
        .collect();
    format!(
        "CREATE OR REPLACE VIEW {name} AS SELECT {projected} AS {mesh}, {rest}, \"date\" \
         FROM {name}{LEGACY_SUFFIX}",
        name = table.sql_name(),
        mesh = crate::MESH_ID_COLUMN,
        rest = rest.join(", "),
    )
}

/// The declared schema plus the `date` partition column DataFusion adds.
fn with_partition_column(schema: &Schema) -> Arc<Schema> {
    // The partition column is appended by the listing table itself, so the
    // schema handed to it must *not* include it — this returns the file
    // schema unchanged and exists to make that explicit rather than
    // leaving the next reader to wonder where `date` comes from.
    Arc::new(schema.clone())
}

/// Turn DataFusion's internal phrasing into something a person can act on.
///
/// Its errors are good but they are written for someone debugging a query
/// planner. "Error during planning: table 'run' not found" is accurate and
/// unhelpful; the useful half is that the table is called `runs`.
#[cfg(test)]
fn friendly(message: String) -> String {
    friendly_with(message, &[])
}

/// [`friendly`], told which tables this archive actually holds.
///
/// The distinction matters: "no such table" and "that table exists but
/// nothing has been archived into it yet" have completely different
/// answers, and only the second one is fixed by pressing Archive.
fn friendly_with(message: String, present: &[Table]) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("table") && lower.contains("not found") {
        let names = |list: &[Table]| {
            list.iter()
                .map(|t| format!("`{}`", t.sql_name()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let empty: Vec<Table> = Table::ALL
            .into_iter()
            .filter(|t| !present.contains(t))
            .collect();
        let mut out = format!("{message}\n\nthis archive holds {}", names(&Table::ALL));
        if !empty.is_empty() && !present.is_empty() {
            out.push_str(&format!(
                "; nothing has been archived into {} yet",
                names(&empty)
            ));
        }
        return out;
    }
    if lower.contains("no field named") {
        return format!(
            "{message}\n\n`jobs` has: job_id, command, runtime, submitter_node, submitter_user, \
             executor_node, state, exit_code, created_at, started_at, finished_at, \
             duration_ms, date\n\
             `facts` has: subject, collector, observed_at, doc, digest, date\n\
             `results` has: group, job_id, node, state, exit_code, finished_at, \
             outputs, date\n\n\
             `doc` and `outputs` are JSON strings. Filter one with \
             doc LIKE '%\"os_id\":\"debian\"%'; pull a value out with \
             regexp_replace(doc, '^.*\"os_id\":\"([^\"]+)\".*$', '$1'). \
             In DuckDB, json_extract_string(doc, '$.os_id') does both."
        );
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_names_match_their_directories() {
        for table in Table::ALL {
            assert_eq!(table.sql_name(), table.dir_name());
        }
    }

    /// Every example must name a table that actually exists. The list
    /// derives from [`Table::ALL`] rather than being spelled out, so a
    /// table added without an example — or an example naming a table that
    /// was never added — is caught here.
    #[test]
    fn every_example_names_a_real_table() {
        for (title, sql) in Archive::examples() {
            assert!(
                Table::ALL
                    .iter()
                    .any(|t| sql.contains(&format!("FROM {}", t.sql_name()))),
                "{title} queries something that does not exist"
            );
        }
    }

    /// And every table has at least one example, because a table nobody
    /// knows how to query is a table nobody queries.
    #[test]
    fn every_table_has_an_example() {
        for table in Table::ALL {
            assert!(
                Archive::examples()
                    .iter()
                    .any(|(_, sql)| sql.contains(&format!("FROM {}", table.sql_name()))),
                "no example queries `{}`",
                table.sql_name()
            );
        }
    }

    #[test]
    fn a_missing_table_error_says_what_does_exist() {
        let msg = friendly("Error during planning: table 'job' not found".into());
        assert!(msg.contains("`jobs`"), "{msg}");
        // Every table is named, so somebody who typed `fact` finds `facts`.
        for table in Table::ALL {
            assert!(msg.contains(&format!("`{}`", table.sql_name())), "{msg}");
        }

        // With some tables present, the message distinguishes "no such
        // table" from "nothing has been archived into it yet" — different
        // problems with completely different answers.
        let msg = friendly_with(
            "Error during planning: table 'facts' not found".into(),
            &[Table::Jobs, Table::Results],
        );
        assert!(msg.contains("nothing has been archived into"), "{msg}");
        assert!(msg.contains("`facts`"), "{msg}");
        assert!(msg.contains("`jobs`"));
    }
}
