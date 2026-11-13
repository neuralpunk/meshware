//! The local query index over runs and jobs: `state/history.redb`.
//!
//! ## Why this exists
//!
//! Every run the mesh has ever done is in the op-log and materialized into
//! [`crate::state::MeshState`] as a `BTreeMap`. That is correct and it does
//! not scale to a year of use: answering "failed runs of `deploy` in the
//! last 90 days" means walking every run, and holding them all means the
//! resident set grows without limit for as long as the node is useful.
//!
//! So: a second redb file with secondary indexes. Nothing here is a new
//! source of truth. Every row is derived from the replicated record, the
//! file can be deleted at any moment, and the next start rebuilds it. Two
//! nodes with different indexes are not in conflict — they are two nodes
//! that have seen different amounts of the same log.
//!
//! ## Why hand-rolled indexes and not a database
//!
//! redb is already in the tree, it is pure Rust, and the query shapes a
//! workflow tool actually needs are few and known: recent, by workflow, by
//! state, by node, over a time range. Four ordered key spaces cover all of
//! them. Buying a general query engine to get four range scans would mean a
//! new engine's failure modes, a new on-disk format to migrate, and — in
//! every candidate worth considering today — either a C dependency or a
//! pre-1.0 stability promise. Neither is worth it for `k.starts_with(p)`.
//!
//! ## Key layout
//!
//! Keys are byte strings compared lexicographically, which is what makes a
//! prefix a range and a big-endian timestamp a sort order:
//!
//! ```text
//! run_recent     when(8 BE) || run_id(16)                       -> ()
//! run_workflow   name || 0x00 || when(8 BE) || run_id(16)       -> ()
//! run_state      state(1)   || when(8 BE) || run_id(16)         -> ()
//! run_node       node(32)   || when(8 BE) || run_id(16)         -> ()
//! ```
//!
//! and the same five for jobs. Iterating a range in reverse gives newest
//! first, which is the order every one of these is read in.
//!
//! `0x00` terminates the workflow name because names are variable length:
//! without it `deploy` and `deploy-staging` share a prefix and the range
//! for one would swallow the other.
//!
//! ## Convergence
//!
//! Upserts are last-writer-wins on `updated_hlc`, the same key the
//! replicated record merges on. An older write is dropped, so the index
//! converges to the same content regardless of the order ops arrive in —
//! and a re-delivered op is a no-op rather than a duplicate row.

use std::path::{Path, PathBuf};

use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
    WriteTransaction,
};

use mesh_proto::{
    FanId, HistoryJobPage, HistoryJobRow, HistoryQuery, HistoryStats, JobId, ResultRow, ResultSet,
    history::state_matches,
};

use crate::error::{CoreError, Result};

/// An index table and the key to write into (or remove from) it. Named
/// because the tuple appears in three signatures and reads as noise in all
/// of them.
type IndexTable = TableDefinition<'static, &'static [u8], ()>;

const JOB_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("job_rows");
// -- C8 placement explanations, C7 cache markers ----------------------------
//
// Two side tables rather than two fields on `HistoryJobRow`, and the reason
// is not squeamishness about migrations: a job row is *derived* from the
// replicated summary and can be rebuilt from it at any time, and these two
// cannot. The explanation is produced once, at dispatch, on the node that
// placed the job and nowhere else; the cache marker likewise. Keeping them
// in their own key space means a row rebuild — which happens on every index
// format change and every `rm history.redb` — does not silently rewrite
// them away as part of re-projecting something else.
//
// They are still *local, losable* data. Losing an explanation costs an
// unanswerable "why did it land there" for jobs that already ran; it costs
// nothing else, and no scheduling decision has ever depended on one.
const JOB_EXPLAIN: TableDefinition<&[u8], &[u8]> = TableDefinition::new("job_explain");
const JOB_CACHE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("job_cache");
const JOB_RECENT: TableDefinition<&[u8], ()> = TableDefinition::new("job_recent");
const JOB_STATE: TableDefinition<&[u8], ()> = TableDefinition::new("job_state");
const JOB_NODE: TableDefinition<&[u8], ()> = TableDefinition::new("job_node");

// -- M19 results (§5.3) ------------------------------------------------------
//
// A result set is **not replicated**: a 600-target fan-out would author 600
// ops that every node in the mesh carries forever to answer a question one
// person asked once. Rows land here, on the node that owns the group, and
// `meshnode results` asked anywhere else says so and names the owner.
//
// `RESULT_ROWS` is keyed `group || job_id`, so one group's rows are a
// contiguous prefix scan. `RESULT_SETS` holds the header (who owns it, what
// ran, how many targets never reported) and `RESULT_RECENT` is what
// retention prunes by.
// -- M18 fact history (§4.8) -------------------------------------------------
//
// Every `FactsPublish` stays in the op-log forever, which is not a cost to
// apologise for — it is the record of what changed on that machine and
// when, signed by the node that observed it. What it is not is *queryable*,
// so this table is the range scan that makes `meshnode facts diff hermes
// --since 7d` answerable without walking the log.
//
// Derived and rebuildable, like everything else in this file: deleting it
// costs one slow start and nothing else.
const FACT_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fact_rows");

// -- ENG-3 audit rows ----------------------------------------------------------
//
// Every node used to hold every audit row it had ever seen in memory. The
// replicated state now keeps a bounded tail (`state::AUDIT_RESIDENT`) and
// this table answers for the rest: `audit_id(16) || author(32)` → the signed
// op that carried the row, so an exported row can still be checked against
// its author's key. Audit ids start with their millisecond, so the key
// order is time order and a page is one range scan.
//
// Derived and rebuildable, like the rest of this file: it is refilled from
// the op-log, which is where the rows live.
const AUDIT_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit_rows");

const RESULT_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("result_rows");
const RESULT_SETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("result_sets");
const RESULT_RECENT: TableDefinition<&[u8], ()> = TableDefinition::new("result_recent");
/// One parallel run per owner-side record, by job id: the request and the
/// per-item outcomes `map --resume` and `results diff` read.
const PARALLEL_RUNS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("parallel_runs");
/// `when || job` → (), the recency index that lets retention find them.
const PARALLEL_RECENT: TableDefinition<&[u8], ()> = TableDefinition::new("parallel_recent");

/// Usage windows, keyed `window(8) || node(32) || principal(32)` (C1 P1).
///
/// **The op-log keeps seven days of these; this keeps them for as long as
/// history is kept.** That split is the whole of §5.2's "accounting records
/// ride the archive, not the op-log": what the scheduler reads is small,
/// recent and replicated; what an operator reports on is large, old and
/// local. Confusing the two is how this milestone becomes a database.
///
/// Window-major so a `--since` range is one contiguous scan, and so every
/// node's contribution to an hour sits together — which is what both
/// groupings of the report want to walk.
const USAGE_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("usage_rows");

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const FORMAT_KEY: &str = "format";
// Bumped for the M19 results tables, and again for C1's usage rows. A newer
// format is deleted and rebuilt rather than refused (see `open`), so this
// costs one slow start.
const INDEX_FORMAT: u64 = 3;

/// How many index entries a filtered scan will look at before it gives up
/// on counting the total. The page itself is always exact; only the
/// `total` is dropped, because a filtered scan over a decade of history
/// should not turn one keystroke in a UI into a full table walk.
const COUNT_SCAN_MAX: usize = 50_000;

/// The local, derived index over runs and jobs.
#[derive(Debug)]
pub struct HistoryStore {
    /// `None` only between a failed reopen and the next attempt (ENG-4).
    db: parking_lot::RwLock<Option<Database>>,
    path: PathBuf,
}

// -- key construction --------------------------------------------------------
//
// One helper per shape, so a reader can see the layout above and the code
// that produces it side by side.

/// The collector name out of a [`fact_key`], given the subject it starts
/// with.
fn collector_of(key: &[u8], subject: &str) -> Option<String> {
    let rest = key.get(subject.len() + 1..)?;
    let end = rest.iter().position(|b| *b == 0)?;
    String::from_utf8(rest[..end].to_vec()).ok()
}

/// The observation instant at the tail of a [`fact_key`].
fn when_of(key: &[u8]) -> u64 {
    key.get(key.len().saturating_sub(8)..)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0)
}

fn ts_key(when: u64, id: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(8 + id.len());
    k.extend_from_slice(&when.to_be_bytes());
    k.extend_from_slice(id);
    k
}

fn prefixed_key(prefix: &[u8], when: u64, id: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + 8 + id.len());
    k.extend_from_slice(prefix);
    k.extend_from_slice(&when.to_be_bytes());
    k.extend_from_slice(id);
    k
}

/// `subject || 0x00 || collector || 0x00 || when` — one subject's whole
/// history is a contiguous prefix, and so is one collector inside it.
///
/// The `0x00` terminators are what stop `hermes` matching `hermes-2`, the
/// same trick `name_prefix` uses for workflow names.
fn audit_key(id: &mesh_proto::AuditId, author: &mesh_proto::NodeId) -> [u8; 48] {
    let mut key = [0u8; 48];
    key[..16].copy_from_slice(&id.0);
    key[16..].copy_from_slice(&author.0);
    key
}

fn fact_key(subject: &str, collector: &str, when: u64) -> Vec<u8> {
    let mut k = fact_prefix(subject, Some(collector));
    k.extend_from_slice(&when.to_be_bytes());
    k
}

fn fact_prefix(subject: &str, collector: Option<&str>) -> Vec<u8> {
    let mut k = subject.as_bytes().to_vec();
    k.push(0);
    if let Some(collector) = collector {
        k.extend_from_slice(collector.as_bytes());
        k.push(0);
    }
    k
}

/// `window || node || principal` — window-major, so a `--since` filter is
/// one range scan from a computed lower bound.
fn usage_key(row: &mesh_proto::UsageRow) -> Vec<u8> {
    let mut k = Vec::with_capacity(72);
    k.extend_from_slice(&row.window.to_be_bytes());
    k.extend_from_slice(&row.node.0);
    k.extend_from_slice(&row.principal.0);
    k
}

/// `group || job_id` — one fan-out's rows are a contiguous prefix.
fn result_key(group: &FanId, job: &JobId) -> Vec<u8> {
    let mut k = Vec::with_capacity(64);
    k.extend_from_slice(&group.0);
    k.extend_from_slice(&job.0);
    k
}

/// The half-open upper bound for "every key starting with `prefix`".
/// Incrementing the last byte that can be incremented is the standard trick;
/// an all-`0xFF` prefix has no successor, which is why this returns an
/// `Option` and callers fall back to an unbounded scan.
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// The `(start, end)` byte range for a prefix narrowed to a time window.
/// Did this query's window reach past what the index still holds
/// (M31 A2 item 4)?
///
/// The index is the only place that knows the oldest row it still has, so
/// it is the only place that can tell *"nothing matched"* from *"retention
/// dropped it"*. A caller cannot work it out afterwards: an empty page
/// looks identical either way, which is exactly the confusion A2 exists to
/// remove.
///
/// `oldest` of `None` means an empty index — a fresh node, or one whose
/// history has not been rebuilt yet — and that is deliberately **not** a
/// known absence. "This mesh has never run anything" and "this mesh threw
/// away what you asked for" are also different facts.
fn absence_for(oldest: Option<u64>, since: Option<u64>) -> mesh_proto::KnownAbsence {
    let Some(oldest) = oldest.filter(|o| *o > 0) else {
        return mesh_proto::KnownAbsence::default();
    };
    mesh_proto::KnownAbsence {
        retained_from_micros: oldest,
        // Only when the caller actually asked about a time before the
        // oldest row — otherwise every query would explain retention to
        // somebody asking about yesterday.
        window_reached_past: since.is_some_and(|s| s < oldest),
    }
}

fn scan_range(prefix: &[u8], since: Option<u64>, until: Option<u64>) -> (Vec<u8>, Option<Vec<u8>>) {
    let start = match since {
        Some(s) => {
            let mut k = prefix.to_vec();
            k.extend_from_slice(&s.to_be_bytes());
            k
        }
        None => prefix.to_vec(),
    };
    let end = match until {
        // `until` is inclusive, so the exclusive bound is the instant after
        // it — and every id sorts below the next microsecond's empty id.
        Some(u) => {
            let mut k = prefix.to_vec();
            k.extend_from_slice(&u.saturating_add(1).to_be_bytes());
            Some(k)
        }
        None => prefix_end(prefix),
    };
    (start, end)
}

fn job_state_code(state: mesh_proto::JobState) -> u8 {
    use mesh_proto::JobState::*;
    match state {
        Queued => 0,
        Running => 1,
        Succeeded => 2,
        Failed => 3,
        Cancelled => 4,
        TimedOut => 5,
    }
}

fn store_err(what: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Store(format!("history {what}: {e}"))
}

impl HistoryStore {
    /// Open (or create) the index at `<home>/state/history.redb`.
    ///
    /// A file written by a newer format is *deleted and rebuilt*, not
    /// refused. That is the opposite of what [`crate::store::OpStore`] does
    /// with the op-log, and deliberately so: the op-log is irreplaceable and
    /// a downgrade must stop, while this is a cache whose worst case is one
    /// slow start.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| store_err("create dir", e))?;
        }
        let mut store = HistoryStore {
            db: parking_lot::RwLock::new(Some(Self::open_at(path)?)),
            path: path.to_path_buf(),
        };
        if store.format()? != Some(INDEX_FORMAT) {
            drop(store.db.write().take());
            let _ = std::fs::remove_file(path);
            *store.db.write() = Some(Self::open_at(path)?);
            store.stamp_format()?;
            tracing::info!(path = %path.display(), "rebuilt the history index");
        } else if store.stamp()?.is_none() {
            // A new, empty index is current by definition — and has to say
            // so now. Left unstamped, its next open found rows and no stamp,
            // took it for a file from before stamping, and deleted it: every
            // node's second start threw away its history, including rows the
            // op-log no longer holds.
            store.stamp_format()?;
        }
        Ok(store)
    }

    /// Close and reopen the index (ENG-4). redb refuses every write after
    /// one fails — a full disk, say — until the database is reopened, and
    /// this is a cache nobody would otherwise restart the node for.
    pub fn reopen(&self) -> Result<()> {
        let mut slot = self.db.write();
        drop(slot.take());
        *slot = Some(Self::open_at(&self.path)?);
        Ok(())
    }

    /// The open database, held for as long as the caller's transaction so
    /// a reopen waits for it. Recursive, because some readers nest.
    fn db(&self) -> Result<parking_lot::MappedRwLockReadGuard<'_, Database>> {
        parking_lot::RwLockReadGuard::try_map(self.db.read_recursive(), Option::as_ref).map_err(
            |_| CoreError::Store("the history index is closed until it can be reopened".into()),
        )
    }

    fn open_at(path: &Path) -> Result<Database> {
        let db = crate::store::open_repairing(path).map_err(|e| store_err("open", e))?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("init txn", e))?;
        {
            txn.open_table(JOB_ROWS).map_err(|e| store_err("init", e))?;
            txn.open_table(JOB_EXPLAIN)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(JOB_CACHE)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(JOB_RECENT)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(JOB_STATE)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(JOB_NODE).map_err(|e| store_err("init", e))?;
            txn.open_table(FACT_ROWS)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(AUDIT_ROWS)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(RESULT_ROWS)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(RESULT_SETS)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(RESULT_RECENT)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(PARALLEL_RUNS)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(PARALLEL_RECENT)
                .map_err(|e| store_err("init", e))?;
            txn.open_table(META).map_err(|e| store_err("init", e))?;
        }
        txn.commit().map_err(|e| store_err("init commit", e))?;
        Ok(db)
    }

    /// The format this file says it was written in, if it says.
    fn stamp(&self) -> Result<Option<u64>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let meta = txn.open_table(META).map_err(|e| store_err("meta", e))?;
        Ok(meta
            .get(FORMAT_KEY)
            .map_err(|e| store_err("format", e))?
            .map(|v| v.value()))
    }

    fn format(&self) -> Result<Option<u64>> {
        // An empty index is "no format yet" only if it is genuinely empty;
        // a populated table with no stamp is from before stamping existed.
        let stamped = self.stamp()?;
        if stamped.is_some() {
            return Ok(stamped);
        }
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let jobs = txn.open_table(JOB_ROWS).map_err(|e| store_err("rows", e))?;
        let empty = jobs.len().map_err(|e| store_err("len", e))? == 0;
        Ok(empty.then_some(INDEX_FORMAT))
    }

    fn stamp_format(&mut self) -> Result<()> {
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        {
            let mut meta = txn.open_table(META).map_err(|e| store_err("meta", e))?;
            meta.insert(FORMAT_KEY, INDEX_FORMAT)
                .map_err(|e| store_err("stamp", e))?;
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(())
    }

    // -- writing -----------------------------------------------------------

    /// Upsert usage windows (C1 P1).
    ///
    /// Idempotent and monotonic in the same way the counters themselves
    /// are: a window is re-indexed on every pass while it is live, and each
    /// pass takes the larger value per component. That means a node whose
    /// index was offline for a week catches up completely the moment it
    /// reads state again, and a node that sees an older partial arrive
    /// after a newer one is not talked backwards.
    pub fn index_usage(&self, rows: &[mesh_proto::UsageRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let mut written = 0;
        {
            let mut t = txn
                .open_table(USAGE_ROWS)
                .map_err(|e| store_err("usage", e))?;
            for row in rows {
                let key = usage_key(row);
                let previous: Option<mesh_proto::UsageRow> = t
                    .get(key.as_slice())
                    .map_err(|e| store_err("get", e))?
                    .and_then(|v| postcard::from_bytes(v.value()).ok());
                let merged = match previous {
                    Some(prev) if prev == *row => continue,
                    Some(prev) => mesh_proto::UsageRow {
                        cpu_secs: prev.cpu_secs.max(row.cpu_secs),
                        mem_mb_secs: prev.mem_mb_secs.max(row.mem_mb_secs),
                        gpu_secs: prev.gpu_secs.max(row.gpu_secs),
                        wall_secs: prev.wall_secs.max(row.wall_secs),
                        jobs: prev.jobs.max(row.jobs),
                        ..*row
                    },
                    None => *row,
                };
                let encoded = postcard::to_stdvec(&merged).map_err(|e| store_err("encode", e))?;
                t.insert(key.as_slice(), encoded.as_slice())
                    .map_err(|e| store_err("insert", e))?;
                written += 1;
            }
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(written)
    }

    /// Every indexed usage window at or after `since_micros`, filtered and
    /// capped — and cut on a **window boundary**, never mid-window.
    ///
    /// That is what makes paging safe for a caller that advances by "the
    /// last window I saw": a truncation inside an hour would strip some of
    /// that hour's nodes, and the next page, starting after it, would never
    /// send them. Whole windows only, even when that means overshooting the
    /// limit by one window's worth of rows.
    pub fn usage_page(
        &self,
        since_micros: u64,
        user: Option<&mesh_proto::UserId>,
        node: Option<&mesh_proto::NodeId>,
        limit: u32,
    ) -> Result<Vec<mesh_proto::UsageRow>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let t = txn
            .open_table(USAGE_ROWS)
            .map_err(|e| store_err("usage", e))?;
        let from = mesh_proto::usage_window(since_micros);
        let lo = from.to_be_bytes().to_vec();
        let mut out: Vec<mesh_proto::UsageRow> = Vec::new();
        for entry in t
            .range(lo.as_slice()..)
            .map_err(|e| store_err("range", e))?
        {
            let (_, value) = entry.map_err(|e| store_err("scan", e))?;
            let Ok(row) = postcard::from_bytes::<mesh_proto::UsageRow>(value.value()) else {
                continue;
            };
            if user.is_some_and(|u| row.principal != *u) {
                continue;
            }
            if node.is_some_and(|n| row.node != *n) {
                continue;
            }
            if out.len() >= limit as usize
                && out.last().is_some_and(|last| last.window != row.window)
            {
                break;
            }
            out.push(row);
        }
        Ok(out)
    }

    pub fn index_jobs(&self, rows: &[HistoryJobRow]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let mut written = 0;
        {
            for row in rows {
                if self.put_job(&txn, row)? {
                    written += 1;
                }
            }
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(written)
    }

    fn put_job(&self, txn: &WriteTransaction, row: &HistoryJobRow) -> Result<bool> {
        let id = row.job_id.0;
        let mut rows = txn.open_table(JOB_ROWS).map_err(|e| store_err("rows", e))?;
        let previous: Option<HistoryJobRow> = rows
            .get(id.as_slice())
            .map_err(|e| store_err("get", e))?
            .and_then(|v| postcard::from_bytes(v.value()).ok());
        if let Some(prev) = &previous
            && prev.updated_hlc >= row.updated_hlc
        {
            return Ok(false);
        }
        // M31 B2. The payload locator is **not** in the replicated summary
        // and never will be — it is discovered by the producing node at
        // export time, long after this row was projected. So a re-index
        // must carry it forward rather than projecting it away, or the
        // locator would survive exactly until the next indexer tick and the
        // six-month question would become unanswerable again.
        let mut row = row.clone();
        if let Some(prev) = &previous {
            row.payload_sink = row.payload_sink.or_else(|| prev.payload_sink.clone());
            row.payload_key = row.payload_key.or_else(|| prev.payload_key.clone());
        }
        let row = &row;
        let encoded = postcard::to_stdvec(row).map_err(|e| store_err("encode", e))?;
        rows.insert(id.as_slice(), encoded.as_slice())
            .map_err(|e| store_err("insert", e))?;
        drop(rows);

        if let Some(prev) = &previous {
            self.edit_job_index(txn, prev, false)?;
        }
        self.edit_job_index(txn, row, true)?;
        Ok(true)
    }

    fn edit_job_index(
        &self,
        txn: &WriteTransaction,
        row: &HistoryJobRow,
        insert: bool,
    ) -> Result<()> {
        let id = row.job_id.0;
        let when = row.sort_micros();
        let entries = [
            (JOB_RECENT, ts_key(when, &id)),
            (
                JOB_STATE,
                prefixed_key(&[job_state_code(row.state)], when, &id),
            ),
            (JOB_NODE, prefixed_key(&row.executor.0, when, &id)),
        ];
        for (table, key) in entries {
            let mut t = txn.open_table(table).map_err(|e| store_err("index", e))?;
            if insert {
                t.insert(key.as_slice(), ())
                    .map_err(|e| store_err("index insert", e))?;
            } else {
                t.remove(key.as_slice())
                    .map_err(|e| store_err("index remove", e))?;
            }
        }
        Ok(())
    }

    // -- C8 placement explanations (§12.2) ---------------------------------

    /// Keep why a job landed where it did.
    ///
    /// Written once, at dispatch, by the node that made the decision. Never
    /// updated: a placement happened once and re-deciding it later against
    /// a mesh that has since changed would produce a confident answer to a
    /// question nobody asked.
    pub fn put_explain(&self, job: &JobId, explain: &mesh_proto::PlacementExplain) -> Result<()> {
        let encoded = postcard::to_stdvec(explain).map_err(|e| store_err("encode", e))?;
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        {
            let mut t = txn
                .open_table(JOB_EXPLAIN)
                .map_err(|e| store_err("explain", e))?;
            t.insert(job.0.as_slice(), encoded.as_slice())
                .map_err(|e| store_err("insert", e))?;
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(())
    }

    /// Why a job landed where it did, if this node is the one that placed
    /// it.
    pub fn explain(&self, job: &JobId) -> Result<Option<mesh_proto::PlacementExplain>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let t = txn
            .open_table(JOB_EXPLAIN)
            .map_err(|e| store_err("explain", e))?;
        Ok(t.get(job.0.as_slice())
            .map_err(|e| store_err("get", e))?
            .and_then(|v| postcard::from_bytes(v.value()).ok()))
    }

    // -- C7 cache markers (§11.2) ------------------------------------------

    /// Record that a job was answered from the cache, or dispatched under a
    /// key.
    ///
    /// *A hit is visible or it is untrustworthy.* "It didn't run" and "it
    /// broke" are indistinguishable unless the product says which, and this
    /// row is where it says so.
    pub fn put_cache_marker(&self, job: &JobId, marker: &JobCacheMarker) -> Result<()> {
        let encoded = postcard::to_stdvec(marker).map_err(|e| store_err("encode", e))?;
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        {
            let mut t = txn
                .open_table(JOB_CACHE)
                .map_err(|e| store_err("cache", e))?;
            t.insert(job.0.as_slice(), encoded.as_slice())
                .map_err(|e| store_err("insert", e))?;
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(())
    }

    pub fn cache_marker(&self, job: &JobId) -> Result<Option<JobCacheMarker>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let t = txn
            .open_table(JOB_CACHE)
            .map_err(|e| store_err("cache", e))?;
        Ok(t.get(job.0.as_slice())
            .map_err(|e| store_err("get", e))?
            .and_then(|v| postcard::from_bytes(v.value()).ok()))
    }

    // -- reading -----------------------------------------------------------

    pub fn job(&self, id: &JobId) -> Result<Option<HistoryJobRow>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let rows = txn.open_table(JOB_ROWS).map_err(|e| store_err("rows", e))?;
        Ok(rows
            .get(id.0.as_slice())
            .map_err(|e| store_err("get", e))?
            .and_then(|v| postcard::from_bytes(v.value()).ok()))
    }

    /// Answer a job query, newest first. Same shape as [`Self::query_runs`];
    /// jobs have no workflow name, so the index choice is state then node.
    pub fn query_jobs(&self, q: &HistoryQuery) -> Result<HistoryJobPage> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let rows = txn.open_table(JOB_ROWS).map_err(|e| store_err("rows", e))?;

        let (table, prefix) = match (&q.state, &q.node) {
            (Some(state), _) => match job_code_for_label(state) {
                Some(code) => (JOB_STATE, vec![code]),
                None => {
                    return Ok(HistoryJobPage {
                        rows: Vec::new(),
                        total: Some(0),
                        absence: self.absence_of(q.since_micros),
                        tiers: Default::default(),
                    });
                }
            },
            (None, Some(node)) => (JOB_NODE, node.0.to_vec()),
            (None, None) => (JOB_RECENT, Vec::new()),
        };

        let index = txn.open_table(table).map_err(|e| store_err("index", e))?;
        let (start, end) = scan_range(&prefix, q.since_micros, q.until_micros);

        let keep = |row: &HistoryJobRow| -> bool {
            if let Some(state) = &q.state
                && !state_matches(row.state.as_str(), state)
            {
                return false;
            }
            if let Some(node) = &q.node
                && row.executor != *node
            {
                return false;
            }
            if let Some(user) = &q.user
                && row.submitter_user != *user
            {
                return false;
            }
            if let Some(needle) = &q.contains {
                let needle = needle.to_ascii_lowercase();
                if !row.cmd_preview.to_ascii_lowercase().contains(&needle)
                    && !row.runtime.to_ascii_lowercase().contains(&needle)
                {
                    return false;
                }
            }
            true
        };

        let limit = q.effective_limit();
        let offset = q.offset as usize;
        let mut out = Vec::with_capacity(limit.min(256));
        let mut matched = 0usize;
        let mut examined = 0usize;
        let mut exhausted = true;

        let iter = match &end {
            Some(e) => index
                .range(start.as_slice()..e.as_slice())
                .map_err(|e| store_err("range", e))?,
            None => index
                .range(start.as_slice()..)
                .map_err(|e| store_err("range", e))?,
        };
        for entry in iter.rev() {
            let (key, _) = entry.map_err(|e| store_err("scan", e))?;
            examined += 1;
            if examined > COUNT_SCAN_MAX && out.len() >= limit {
                exhausted = false;
                break;
            }
            let key = key.value();
            let Some(id) = key.get(key.len().saturating_sub(16)..) else {
                continue;
            };
            let Some(bytes) = rows.get(id).map_err(|e| store_err("get", e))? else {
                continue;
            };
            let Ok(row) = postcard::from_bytes::<HistoryJobRow>(bytes.value()) else {
                continue;
            };
            if !keep(&row) {
                continue;
            }
            matched += 1;
            if matched > offset && out.len() < limit {
                out.push(row);
            }
        }

        let out_len = out.len();
        Ok(HistoryJobPage {
            rows: out,
            total: exhausted.then_some(matched as u64),
            absence: self.absence_of(q.since_micros),
            // The index knows only what it answered; whoever spans the
            // tiers adds the rest.
            tiers: mesh_proto::TierReport {
                from_index: out_len as u64,
                ..Default::default()
            },
        })
    }

    // -- fact history (M18 §4.8) --------------------------------------------

    /// Record one observation, keyed by when it was *observed*.
    ///
    /// Idempotent: the same document at the same instant overwrites itself,
    /// so a node that re-indexes its log writes the same rows it already
    /// had. Returns false when this exact observation was already held.
    pub fn index_fact(
        &self,
        subject: &str,
        collector: &str,
        when_micros: u64,
        doc: &str,
    ) -> Result<bool> {
        Ok(self.index_facts(&[(subject, collector, when_micros, doc)])? > 0)
    }

    /// A batch of observations in one transaction (**H-4**).
    ///
    /// The counterpart to [`Self::index_runs`], and it should always have
    /// been one: `index_fact` opened a write transaction — an fsync — per
    /// *row*, and both of its callers loop over every observation the node
    /// has. A year-old thousand-node mesh has thousands of them, so a
    /// restart paid thousands of serialised flushes to rebuild an index it
    /// could have written in one.
    ///
    /// Returns how many rows were actually fresh. **Nothing is committed
    /// when nothing was fresh**, which is the common case on a re-run: the
    /// old code committed regardless and paid the fsync to write no bytes.
    pub fn index_facts(&self, rows: &[(&str, &str, u64, &str)]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let mut fresh_count = 0usize;
        {
            let mut table = txn
                .open_table(FACT_ROWS)
                .map_err(|e| store_err("rows", e))?;
            for (subject, collector, when_micros, doc) in rows {
                let key = fact_key(subject, collector, *when_micros);
                let fresh = table
                    .get(key.as_slice())
                    .map_err(|e| store_err("get", e))?
                    .is_none_or(|v| v.value() != doc.as_bytes());
                if fresh {
                    table
                        .insert(key.as_slice(), doc.as_bytes())
                        .map_err(|e| store_err("put", e))?;
                    fresh_count += 1;
                }
            }
        }
        if fresh_count == 0 {
            // An abort costs nothing; a commit costs an fsync to write
            // nothing at all.
            drop(txn);
            return Ok(0);
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(fresh_count)
    }

    /// Index audit rows from the signed ops that carry them (ENG-3).
    ///
    /// Ops that are not audit rows are skipped. Rows already present are
    /// left alone, and nothing is committed when nothing was new. Returns
    /// how many rows were added.
    pub fn index_audit(&self, ops: &[mesh_proto::OpEnvelope]) -> Result<usize> {
        if ops.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let mut fresh = 0usize;
        {
            let mut table = txn
                .open_table(AUDIT_ROWS)
                .map_err(|e| store_err("audit", e))?;
            for op in ops {
                let Some(mesh_proto::OpBody::AuditAppend { entry }) = op.decode_body() else {
                    continue;
                };
                let key = audit_key(&entry.audit_id, &op.author);
                if table
                    .get(key.as_slice())
                    .map_err(|e| store_err("get", e))?
                    .is_some()
                {
                    continue;
                }
                let value = postcard::to_stdvec(op).map_err(|e| store_err("encode", e))?;
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(|e| store_err("put", e))?;
                fresh += 1;
            }
        }
        if fresh == 0 {
            drop(txn);
            return Ok(0);
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(fresh)
    }

    /// Audit rows older than `before` (all of them when `None`), newest
    /// first, filtered, at most `limit` (ENG-3).
    ///
    /// The page past the in-memory tail: `before` is the oldest id still
    /// resident, so the two halves never overlap.
    pub fn audit_page(
        &self,
        filter: &crate::audit::AuditFilter,
        before: Option<mesh_proto::AuditId>,
        limit: usize,
    ) -> Result<Vec<mesh_proto::AuditEntry>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let table = txn
            .open_table(AUDIT_ROWS)
            .map_err(|e| store_err("audit", e))?;
        let end = before.map(|id| id.0.to_vec());
        let range = match &end {
            Some(end) => table.range::<&[u8]>(..end.as_slice()),
            None => table.range::<&[u8]>(..),
        }
        .map_err(|e| store_err("range", e))?;
        let mut out = Vec::new();
        for row in range.rev() {
            if out.len() >= limit {
                break;
            }
            let (_, value) = row.map_err(|e| store_err("scan", e))?;
            let Some(entry) = postcard::from_bytes::<mesh_proto::OpEnvelope>(value.value())
                .ok()
                .and_then(|op| match op.decode_body() {
                    Some(mesh_proto::OpBody::AuditAppend { entry }) => Some(entry),
                    _ => None,
                })
            else {
                continue;
            };
            // Keys are in time order, so nothing further back can match.
            if filter.since_micros.is_some_and(|t| entry.at_micros < t) {
                break;
            }
            if crate::audit::matches(filter, &entry) {
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// How many audit rows the index holds.
    pub fn audit_len(&self) -> Result<u64> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let table = txn
            .open_table(AUDIT_ROWS)
            .map_err(|e| store_err("audit", e))?;
        table.len().map_err(|e| store_err("len", e))
    }

    /// Drop audit rows written before `cutoff_micros` — the audit window,
    /// when an admin has made it finite (ENG-3). Returns how many.
    pub fn prune_audit_before(&self, cutoff_micros: u64) -> Result<usize> {
        let millis = cutoff_micros / 1000;
        let mut end = [0u8; 6];
        end.copy_from_slice(&millis.to_be_bytes()[2..8]);
        let doomed: Vec<Vec<u8>> = {
            let db = self.db()?;
            let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
            let table = txn
                .open_table(AUDIT_ROWS)
                .map_err(|e| store_err("audit", e))?;
            table
                .range::<&[u8]>(..end.as_slice())
                .map_err(|e| store_err("range", e))?
                .map(|row| row.map(|(k, _)| k.value().to_vec()))
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| store_err("scan", e))?
        };
        if doomed.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        {
            let mut table = txn
                .open_table(AUDIT_ROWS)
                .map_err(|e| store_err("audit", e))?;
            for key in &doomed {
                table
                    .remove(key.as_slice())
                    .map_err(|e| store_err("remove", e))?;
            }
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(doomed.len())
    }

    /// What changed about a subject, newest first.
    ///
    /// Each row is the diff between one observation and the one before it,
    /// which is the shape a person actually reads: "on Tuesday this box's
    /// kernel went from 6.1.0 to 6.1.4" beats two full documents to
    /// compare by eye. The oldest observation in range has nothing before
    /// it and is reported as the baseline it is.
    pub fn fact_diff(
        &self,
        subject: &mesh_proto::FactSubject,
        subject_name: &str,
        collector: Option<&str>,
        since_micros: Option<u64>,
        limit: u32,
    ) -> Result<Vec<mesh_proto::FactDiffRow>> {
        let key_name = subject.label();
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let rows = txn
            .open_table(FACT_ROWS)
            .map_err(|e| store_err("rows", e))?;

        // One pass per collector, because a diff only makes sense against
        // the previous document *of the same collector*.
        let collectors: Vec<String> = match collector {
            Some(one) => vec![one.to_string()],
            None => {
                let prefix = fact_prefix(&key_name, None);
                let (start, end) = scan_range(&prefix, None, None);
                let iter = match end {
                    Some(end) => rows.range(start.as_slice()..end.as_slice()),
                    None => rows.range(start.as_slice()..),
                }
                .map_err(|e| store_err("range", e))?;
                let mut seen: Vec<String> = Vec::new();
                for entry in iter {
                    let (key, _) = entry.map_err(|e| store_err("scan", e))?;
                    if let Some(name) = collector_of(key.value(), &key_name)
                        && !seen.iter().any(|s| s == &name)
                    {
                        seen.push(name);
                    }
                }
                seen
            }
        };

        let mut out: Vec<mesh_proto::FactDiffRow> = Vec::new();
        for collector in collectors {
            let prefix = fact_prefix(&key_name, Some(&collector));
            // The first observation inside the window has to be compared
            // against the one before it, or every window would open with a
            // spurious "everything changed".
            //
            // **Seek to that one observation; do not read up to it**
            // (**M-12**). This used to scan from the beginning of time and
            // JSON-parse every document ever recorded for the subject, only
            // to discard the ones before `since` — so "what changed on this
            // host last week" re-read and re-parsed the host's entire
            // history. The key is time-prefixed, so the row before the
            // window is one backwards step from the window's own start.
            let mut previous: Option<serde_json::Value> = None;
            if let Some(since) = since_micros.filter(|s| *s > 0) {
                let (start, end) = scan_range(&prefix, None, Some(since - 1));
                let mut before = match end {
                    Some(end) => rows.range(start.as_slice()..end.as_slice()),
                    None => rows.range(start.as_slice()..),
                }
                .map_err(|e| store_err("range", e))?;
                if let Some(entry) = before.next_back() {
                    let (_, value) = entry.map_err(|e| store_err("scan", e))?;
                    previous = Some(crate::facts::parse(&String::from_utf8_lossy(value.value())));
                }
            }
            let (start, end) = scan_range(&prefix, since_micros, None);
            let iter = match end {
                Some(end) => rows.range(start.as_slice()..end.as_slice()),
                None => rows.range(start.as_slice()..),
            }
            .map_err(|e| store_err("range", e))?;
            for entry in iter {
                let (key, value) = entry.map_err(|e| store_err("scan", e))?;
                let when = when_of(key.value());
                let doc = crate::facts::parse(&String::from_utf8_lossy(value.value()));
                let changes = match previous.as_ref() {
                    Some(before) => crate::facts::changes(before, &doc),
                    // The baseline: every key, as it first appeared.
                    None => crate::facts::changes(&serde_json::Value::Null, &doc),
                };
                // Moved, not cloned (**M-12**): the old code cloned every
                // document purely to keep a copy as the next comparison's
                // baseline.
                previous = Some(doc);
                if changes.is_empty() {
                    continue;
                }
                out.push(mesh_proto::FactDiffRow {
                    subject_name: subject_name.to_string(),
                    collector: collector.clone(),
                    at_micros: when,
                    changes,
                });
            }
        }
        // Newest first, across collectors.
        out.sort_by_key(|r| std::cmp::Reverse(r.at_micros));
        out.truncate(limit.max(1) as usize);
        Ok(out)
    }

    /// Every observation in the index, oldest first — the archive's read.
    pub fn fact_export(
        &self,
        since_micros: Option<u64>,
        limit: u32,
    ) -> Result<Vec<(String, String, u64, String)>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let rows = txn
            .open_table(FACT_ROWS)
            .map_err(|e| store_err("rows", e))?;
        let mut out = Vec::new();
        for entry in rows.iter().map_err(|e| store_err("scan", e))? {
            let (key, value) = entry.map_err(|e| store_err("scan", e))?;
            let key = key.value();
            let when = when_of(key);
            if since_micros.is_some_and(|since| when < since) {
                continue;
            }
            // `subject \0 collector \0 when` — split back out.
            let mut parts = key.split(|b| *b == 0);
            let Some(subject) = parts.next().and_then(|b| std::str::from_utf8(b).ok()) else {
                continue;
            };
            let Some(collector) = parts.next().and_then(|b| std::str::from_utf8(b).ok()) else {
                continue;
            };
            out.push((
                subject.to_string(),
                collector.to_string(),
                when,
                String::from_utf8_lossy(value.value()).into_owned(),
            ));
        }
        out.sort_by_key(|(_, _, when, _)| *when);
        out.truncate(limit.max(1) as usize);
        Ok(out)
    }

    // -- results (M19 §5.3) ------------------------------------------------

    /// Record one fan-out's rows and its header, in one transaction.
    ///
    /// Idempotent by `(group, job_id)`: an adopter that re-drives a target
    /// overwrites that target's row rather than appending a second one.
    pub fn index_results(&self, set: &ResultSet) -> Result<usize> {
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let written;
        {
            let mut header = ResultSet {
                rows: Vec::new(),
                ..set.clone()
            };
            // The header remembers the target count, never the rows —
            // reading a 600-row set to answer "how many?" would defeat the
            // point of a separate table.
            header.missing = set.missing;
            let mut sets = txn
                .open_table(RESULT_SETS)
                .map_err(|e| store_err("sets", e))?;
            let bytes = postcard::to_stdvec(&header).map_err(|e| store_err("encode", e))?;
            sets.insert(set.group.0.as_slice(), bytes.as_slice())
                .map_err(|e| store_err("put", e))?;
            drop(sets);

            let mut recent = txn
                .open_table(RESULT_RECENT)
                .map_err(|e| store_err("recent", e))?;
            recent
                .insert(ts_key(set.created_at_micros, &set.group.0).as_slice(), ())
                .map_err(|e| store_err("put", e))?;
            drop(recent);

            let mut rows = txn
                .open_table(RESULT_ROWS)
                .map_err(|e| store_err("rows", e))?;
            for row in &set.rows {
                let bytes = postcard::to_stdvec(row).map_err(|e| store_err("encode", e))?;
                rows.insert(
                    result_key(&set.group, &row.job_id).as_slice(),
                    bytes.as_slice(),
                )
                .map_err(|e| store_err("put", e))?;
            }
            written = set.rows.len();
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(written)
    }

    /// Remember a parallel run: written at start (request only) and again
    /// at the end (with outcomes). Idempotent by job id.
    pub fn put_parallel_record(&self, record: &mesh_proto::parallel::ParallelRecord) -> Result<()> {
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        {
            let mut runs = txn
                .open_table(PARALLEL_RUNS)
                .map_err(|e| store_err("parallel", e))?;
            let bytes = postcard::to_stdvec(record).map_err(|e| store_err("encode", e))?;
            runs.insert(record.job.0.as_slice(), bytes.as_slice())
                .map_err(|e| store_err("put", e))?;
            drop(runs);
            let mut recent = txn
                .open_table(PARALLEL_RECENT)
                .map_err(|e| store_err("recent", e))?;
            recent
                .insert(
                    ts_key(record.created_at_micros, &record.job.0).as_slice(),
                    (),
                )
                .map_err(|e| store_err("put", e))?;
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(())
    }

    /// One parallel run's record, or `None` when this node never owned it.
    pub fn parallel_record(
        &self,
        job: &JobId,
    ) -> Result<Option<mesh_proto::parallel::ParallelRecord>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let runs = txn
            .open_table(PARALLEL_RUNS)
            .map_err(|e| store_err("parallel", e))?;
        Ok(runs
            .get(job.0.as_slice())
            .map_err(|e| store_err("get", e))?
            .and_then(|v| postcard::from_bytes(v.value()).ok()))
    }

    /// Drop parallel records created before `cutoff`, with the retention
    /// pass that drops everything else.
    fn prune_parallel_before(&self, cutoff_micros: u64) -> Result<usize> {
        let doomed: Vec<Vec<u8>> = {
            let db = self.db()?;
            let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
            let recent = txn
                .open_table(PARALLEL_RECENT)
                .map_err(|e| store_err("recent", e))?;
            let mut out = Vec::new();
            for entry in recent.iter().map_err(|e| store_err("scan", e))? {
                let (key, _) = entry.map_err(|e| store_err("scan", e))?;
                let key = key.value();
                let when = key
                    .get(..8)
                    .and_then(|b| <[u8; 8]>::try_from(b).ok())
                    .map(u64::from_be_bytes)
                    .unwrap_or(0);
                if when < cutoff_micros {
                    out.push(key.to_vec());
                }
            }
            out
        };
        if doomed.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        {
            let mut recent = txn
                .open_table(PARALLEL_RECENT)
                .map_err(|e| store_err("recent", e))?;
            let mut runs = txn
                .open_table(PARALLEL_RUNS)
                .map_err(|e| store_err("parallel", e))?;
            for key in &doomed {
                recent
                    .remove(key.as_slice())
                    .map_err(|e| store_err("rm", e))?;
                if let Some(job) = key.get(8..) {
                    runs.remove(job).map_err(|e| store_err("rm", e))?;
                }
            }
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(doomed.len())
    }

    /// Every result set this node owns, newest first — the archive's read.
    pub fn result_sets(&self, since_micros: Option<u64>, limit: u32) -> Result<Vec<ResultSet>> {
        let groups: Vec<FanId> = {
            let db = self.db()?;
            let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
            let recent = txn
                .open_table(RESULT_RECENT)
                .map_err(|e| store_err("recent", e))?;
            let mut groups = Vec::new();
            for entry in recent.iter().map_err(|e| store_err("scan", e))? {
                let (key, _) = entry.map_err(|e| store_err("scan", e))?;
                let key = key.value();
                let when = key
                    .get(..8)
                    .and_then(|b| <[u8; 8]>::try_from(b).ok())
                    .map(u64::from_be_bytes)
                    .unwrap_or(0);
                if since_micros.is_some_and(|since| when < since) {
                    continue;
                }
                if let Some(id) = key.get(8..).and_then(|b| <[u8; 16]>::try_from(b).ok()) {
                    groups.push((when, FanId(id)));
                }
            }
            groups.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
            groups.truncate(limit.max(1) as usize);
            groups.into_iter().map(|(_, id)| id).collect()
        };
        let mut out = Vec::new();
        for group in groups {
            if let Some(set) = self.results(&group, false)? {
                out.push(set);
            }
        }
        Ok(out)
    }

    /// One fan-out's rows, in job-id order. `None` when this node has never
    /// held that group — which is the answer that must name the owner, and
    /// the caller is the one that knows who that is.
    pub fn results(&self, group: &FanId, failed_only: bool) -> Result<Option<ResultSet>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let sets = txn
            .open_table(RESULT_SETS)
            .map_err(|e| store_err("sets", e))?;
        let Some(header) = sets
            .get(group.0.as_slice())
            .map_err(|e| store_err("get", e))?
            .and_then(|v| postcard::from_bytes::<ResultSet>(v.value()).ok())
        else {
            return Ok(None);
        };
        let rows_table = txn
            .open_table(RESULT_ROWS)
            .map_err(|e| store_err("rows", e))?;
        let (start, end) = scan_range(&group.0, None, None);
        let mut rows: Vec<ResultRow> = Vec::new();
        let iter = match end {
            Some(end) => rows_table.range(start.as_slice()..end.as_slice()),
            None => rows_table.range(start.as_slice()..),
        }
        .map_err(|e| store_err("range", e))?;
        for entry in iter {
            let (_, value) = entry.map_err(|e| store_err("scan", e))?;
            if let Ok(row) = postcard::from_bytes::<ResultRow>(value.value())
                && (!failed_only || !row.succeeded())
            {
                rows.push(row);
            }
        }
        Ok(Some(ResultSet { rows, ..header }))
    }

    /// Counts and bounds. Cheap: table lengths plus one key from each end
    /// of the two recency indexes.
    pub fn stats(&self, path: &Path) -> Result<HistoryStats> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let jobs = txn.open_table(JOB_ROWS).map_err(|e| store_err("rows", e))?;
        let job_recent = txn
            .open_table(JOB_RECENT)
            .map_err(|e| store_err("index", e))?;

        let edge = |t: &redb::ReadOnlyTable<&[u8], ()>, first: bool| -> Option<u64> {
            let entry = if first { t.first() } else { t.last() };
            let (key, _) = entry.ok()??;
            let bytes: [u8; 8] = key.value().get(..8)?.try_into().ok()?;
            Some(u64::from_be_bytes(bytes))
        };

        let oldest = edge(&job_recent, true);
        let newest = edge(&job_recent, false);

        Ok(HistoryStats {
            jobs: jobs.len().map_err(|e| store_err("len", e))?,
            oldest_micros: oldest,
            newest_micros: newest,
            index_bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        })
    }

    /// Record where a sink put a job's output (M31 B2 item 5).
    ///
    /// A read-modify-write of the one row, and it has to be: the locator is
    /// discovered by the **producing** node at export time, long after the
    /// row was indexed from the replicated summary. Cheap — one point
    /// lookup and one put — and it happens once per job per sink, never on
    /// a path a job waits on.
    ///
    /// Silently does nothing if the row has already gone. That is not a
    /// failure: `[history] keep_days` and the mesh window may have dropped
    /// it between the export and this call, and A2's rule says the payload
    /// outliving its record is routine.
    pub fn set_payload_locator(&self, job: &JobId, sink: &str, key: &str) -> Result<()> {
        self.set_payload_locators(&[(*job, sink.to_string(), key.to_string())])
    }

    /// A batch of locators in one transaction (**L-4**).
    ///
    /// The exporter ships up to `PAYLOAD_BATCH` (100) payloads per sink per
    /// 30 s tick and recorded each locator with its own write transaction —
    /// so a full batch was a hundred fsyncs to write a hundred short
    /// strings. Same rule as [`Self::index_runs`]: the loop is inside the
    /// transaction, not around it.
    pub fn set_payload_locators(&self, locators: &[(JobId, String, String)]) -> Result<()> {
        if locators.is_empty() {
            return Ok(());
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let mut written = 0usize;
        {
            let mut rows = txn.open_table(JOB_ROWS).map_err(|e| store_err("rows", e))?;
            for (job, sink, key) in locators {
                let id = job.0.to_vec();
                let existing: Option<HistoryJobRow> = rows
                    .get(id.as_slice())
                    .map_err(|e| store_err("get", e))?
                    .and_then(|v| postcard::from_bytes(v.value()).ok());
                // A row the index no longer has is not a failure: the
                // window may have dropped it between the export and this
                // call, and A2's rule says a payload outliving its record
                // is routine.
                let Some(mut row) = existing else {
                    continue;
                };
                row.payload_sink = Some(sink.clone());
                row.payload_key = Some(key.clone());
                let bytes = postcard::to_stdvec(&row)
                    .map_err(|e| CoreError::Store(format!("encode: {e}")))?;
                rows.insert(id.as_slice(), bytes.as_slice())
                    .map_err(|e| store_err("insert", e))?;
                written += 1;
            }
        }
        if written == 0 {
            drop(txn);
            return Ok(());
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(())
    }

    /// The oldest instant this index still answers for, against the
    /// caller's window (M31 A2 item 4).
    ///
    /// Two range scans that stop at the first key, so it is a couple of
    /// B-tree descents rather than a walk — cheap enough to run on every
    /// query, which it has to be, because a note that only appears
    /// sometimes is a note nobody learns to trust.
    fn absence_of(&self, since: Option<u64>) -> mesh_proto::KnownAbsence {
        // Nothing to say if the caller did not ask about the past.
        if since.is_none() {
            return mesh_proto::KnownAbsence::default();
        }
        absence_for(self.oldest_indexed(), since)
    }

    /// The lowest sort instant across runs and jobs, or `None` on an empty
    /// index.
    fn oldest_indexed(&self) -> Option<u64> {
        let db = self.db().ok()?;
        let txn = db.begin_read().ok()?;
        let mut oldest: Option<u64> = None;
        for table in [JOB_RECENT] {
            let Ok(index) = txn.open_table(table) else {
                continue;
            };
            let Ok(mut iter) = index.iter() else {
                continue;
            };
            if let Some(Ok((key, _))) = iter.next() {
                let key = key.value();
                if key.len() >= 8 {
                    let mut buf = [0u8; 8];
                    buf.copy_from_slice(&key[..8]);
                    let micros = u64::from_be_bytes(buf);
                    oldest = Some(oldest.map_or(micros, |o: u64| o.min(micros)));
                }
            }
        }
        oldest
    }

    /// Drop indexed rows whose sort instant is older than `cutoff_micros`.
    /// Returns `(runs, jobs)` removed.
    ///
    /// Only ever called with a cutoff an operator configured. A default of
    /// "forever" is the right default for a few hundred bytes a run: the
    /// reason to prune history is a policy about retention, never disk.
    pub fn prune_before(&self, cutoff_micros: u64) -> Result<usize> {
        let doomed_jobs = self.ids_before(JOB_RECENT, cutoff_micros)?;
        self.prune_results_before(cutoff_micros)?;
        self.prune_parallel_before(cutoff_micros)?;
        if doomed_jobs.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let mut jobs_gone = 0;
        {
            for id in &doomed_jobs {
                let mut rows = txn.open_table(JOB_ROWS).map_err(|e| store_err("rows", e))?;
                let row: Option<HistoryJobRow> = rows
                    .get(id.as_slice())
                    .map_err(|e| store_err("get", e))?
                    .and_then(|v| postcard::from_bytes(v.value()).ok());
                rows.remove(id.as_slice())
                    .map_err(|e| store_err("remove", e))?;
                drop(rows);
                {
                    // The side tables age out with the job they describe.
                    // An explanation for a job the index no longer holds is
                    // an orphan nothing can ever ask for.
                    let mut explains = txn
                        .open_table(JOB_EXPLAIN)
                        .map_err(|e| store_err("explain", e))?;
                    explains
                        .remove(id.as_slice())
                        .map_err(|e| store_err("remove", e))?;
                    let mut markers = txn
                        .open_table(JOB_CACHE)
                        .map_err(|e| store_err("cache", e))?;
                    markers
                        .remove(id.as_slice())
                        .map_err(|e| store_err("remove", e))?;
                }
                if let Some(row) = row {
                    self.edit_job_index(&txn, &row, false)?;
                    jobs_gone += 1;
                }
            }
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(jobs_gone)
    }

    /// Result sets age out on the same retention clock. Rows go with their
    /// set: a header with no rows would answer "0 targets" instead of
    /// "that fan-out is older than this node keeps".
    fn prune_results_before(&self, cutoff_micros: u64) -> Result<usize> {
        let doomed = self.ids_before(RESULT_RECENT, cutoff_micros)?;
        if doomed.is_empty() {
            return Ok(0);
        }
        let db = self.db()?;
        let txn = crate::store::begin_write(&db).map_err(|e| store_err("txn", e))?;
        let mut gone = 0;
        {
            let mut sets = txn
                .open_table(RESULT_SETS)
                .map_err(|e| store_err("sets", e))?;
            let mut rows = txn
                .open_table(RESULT_ROWS)
                .map_err(|e| store_err("rows", e))?;
            let mut recent = txn
                .open_table(RESULT_RECENT)
                .map_err(|e| store_err("recent", e))?;
            for group in &doomed {
                let (start, end) = scan_range(group, None, None);
                let keys: Vec<Vec<u8>> = {
                    let iter = match &end {
                        Some(end) => rows.range(start.as_slice()..end.as_slice()),
                        None => rows.range(start.as_slice()..),
                    }
                    .map_err(|e| store_err("range", e))?;
                    let mut keys = Vec::new();
                    for entry in iter {
                        let (key, _) = entry.map_err(|e| store_err("scan", e))?;
                        keys.push(key.value().to_vec());
                    }
                    keys
                };
                for key in keys {
                    rows.remove(key.as_slice())
                        .map_err(|e| store_err("remove", e))?;
                }
                if sets
                    .remove(group.as_slice())
                    .map_err(|e| store_err("remove", e))?
                    .is_some()
                {
                    gone += 1;
                }
            }
            // The recent index is keyed by time, so re-derive and drop.
            let stale: Vec<Vec<u8>> = {
                let end = cutoff_micros.to_be_bytes();
                let mut keys = Vec::new();
                for entry in recent
                    .range(..end.as_slice())
                    .map_err(|e| store_err("range", e))?
                {
                    let (key, _) = entry.map_err(|e| store_err("scan", e))?;
                    keys.push(key.value().to_vec());
                }
                keys
            };
            for key in stale {
                recent
                    .remove(key.as_slice())
                    .map_err(|e| store_err("remove", e))?;
            }
        }
        txn.commit().map_err(|e| store_err("commit", e))?;
        Ok(gone)
    }

    fn ids_before(&self, table: IndexTable, cutoff: u64) -> Result<Vec<Vec<u8>>> {
        let db = self.db()?;
        let txn = db.begin_read().map_err(|e| store_err("read txn", e))?;
        let index = txn.open_table(table).map_err(|e| store_err("index", e))?;
        let end = cutoff.to_be_bytes();
        let mut ids = Vec::new();
        for entry in index
            .range(..end.as_slice())
            .map_err(|e| store_err("range", e))?
        {
            let (key, _) = entry.map_err(|e| store_err("scan", e))?;
            let key = key.value();
            if let Some(id) = key.get(key.len().saturating_sub(16)..) {
                ids.push(id.to_vec());
            }
        }
        Ok(ids)
    }
}

// -- projection ---------------------------------------------------------------
//
// The replicated record is the truth; these turn it into the flat row the
// index sorts and filters by. Nothing here reads the filesystem or the
// network, so the indexer can hold the state read-lock for exactly as long
// as it takes to copy some scalars.

/// Flatten one replicated job summary into an index row.
/// What the cache had to say about one job, kept on the node that
/// submitted it.
///
/// Both halves are recorded, hit *and* miss, because a hit rate needs a
/// denominator and because "this job ran under key `act-…`" is what makes
/// the *"why was this a miss"* diff possible at all: without the key of the
/// run that missed there is nothing to compare against the run that hit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobCacheMarker {
    pub key: mesh_proto::ActionKey,
    /// Set when this job's result came from the cache rather than a
    /// process.
    pub hit: Option<mesh_proto::CacheHit>,
    /// The key's fields as this job derived them.
    pub fields: Vec<mesh_proto::KeyField>,
}

pub fn project_job(summary: &mesh_proto::JobSummary) -> HistoryJobRow {
    HistoryJobRow {
        job_id: summary.job_id,
        cmd_preview: summary.cmd_preview.clone(),
        runtime: summary.runtime.clone(),
        submitter: summary.submitter,
        submitter_user: summary.submitter_user,
        executor: summary.executor,
        state: summary.state,
        exit_code: summary.exit_code,
        created_at_micros: summary.created_at_micros,
        started_at_micros: summary.started_at_micros,
        finished_at_micros: summary.finished_at_micros,
        updated_hlc: summary.updated_hlc,
        // A projection from the replicated summary can never know these:
        // the locator is the producing node's discovery at export time.
        // `put_job` carries an existing one forward rather than letting
        // this `None` erase it.
        payload_sink: None,
        payload_key: None,
        from_archive: false,
        output: None,
    }
}

fn job_code_for_label(label: &str) -> Option<u8> {
    use mesh_proto::JobState::*;
    [Queued, Running, Succeeded, Failed, Cancelled, TimedOut]
        .into_iter()
        .find(|s| state_matches(s.as_str(), label))
        .map(job_state_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::{Hlc, JobState, NodeId, UserId};

    fn hlc(n: u64) -> Hlc {
        Hlc {
            wall_micros: n,
            logical: 0,
        }
    }

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    /// A job row for the index tests below, which are about the *index*
    /// rather than about jobs: paging, reopen, prune and stats behave the
    /// same whatever a row holds. They used to be written against runs, and
    /// were rewritten when the run index went (2026-09-20) rather than
    /// deleted with it — the properties are the index's, not the run's.
    fn job_row(id: u8, cmd: &str, state: JobState, when: u64) -> HistoryJobRow {
        HistoryJobRow {
            job_id: JobId([id; 16]),
            cmd_preview: cmd.into(),
            runtime: "bash".into(),
            submitter: node(1),
            submitter_user: UserId([7; 32]),
            executor: node(9),
            state,
            exit_code: Some(0),
            created_at_micros: when,
            started_at_micros: Some(when),
            finished_at_micros: Some(when),
            updated_hlc: hlc(u64::from(id)),
            payload_sink: None,
            payload_key: None,
            from_archive: false,
            output: None,
        }
    }

    fn store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            HistoryStore::open(&dir.path().join("state").join("history.redb")).expect("open");
        (dir, store)
    }

    /// An audit row as the op that carries it, at millisecond `ms`.
    fn audit_op(ms: u64, action: &str, key: &crate::identity::Keypair) -> mesh_proto::OpEnvelope {
        let mut id = [0u8; 16];
        id[..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
        id[15] = (ms % 251) as u8;
        let body = mesh_proto::OpBody::AuditAppend {
            entry: mesh_proto::AuditEntry {
                audit_id: mesh_proto::AuditId(id),
                at_micros: ms * 1000,
                user: UserId([7; 32]),
                username: "ada".into(),
                action: action.into(),
                target: String::new(),
                node: key.node_id(),
                outcome: "ok".into(),
                detail: String::new(),
                request_sig: Vec::new(),
            },
        };
        let bytes = postcard::to_stdvec(&body).expect("encode");
        let hlc = hlc(ms * 1000);
        let signature = key
            .sign(&mesh_proto::op_signing_bytes_raw(
                &key.node_id(),
                ms,
                &hlc,
                &bytes,
            ))
            .to_vec();
        mesh_proto::OpEnvelope {
            author: key.node_id(),
            seq: ms,
            hlc,
            body: bytes,
            signature,
        }
    }

    /// ENG-3: rows past the in-memory tail come back from here, newest
    /// first, filtered, strictly older than where the tail begins — and a
    /// finite audit window takes them away.
    #[test]
    fn audit_rows_page_backwards_and_age_out() {
        let (_d, store) = store();
        let key = crate::identity::Keypair::generate().expect("key");
        let ops: Vec<_> = (1..=30u64)
            .map(|ms| audit_op(ms, if ms % 2 == 0 { "job.run" } else { "secret.set" }, &key))
            .collect();
        assert_eq!(store.index_audit(&ops).expect("index"), 30);
        // Idempotent: nothing new, nothing written.
        assert_eq!(store.index_audit(&ops).expect("index"), 0);
        assert_eq!(store.audit_len().expect("len"), 30);

        let all = crate::audit::AuditFilter::default();
        let page = store.audit_page(&all, None, 5).expect("page");
        let at: Vec<u64> = page.iter().map(|e| e.at_micros / 1000).collect();
        assert_eq!(at, vec![30, 29, 28, 27, 26]);

        // From where an in-memory tail would start.
        let start = match ops[19].decode_body() {
            Some(mesh_proto::OpBody::AuditAppend { entry }) => entry.audit_id,
            _ => unreachable!(),
        };
        let runs = crate::audit::AuditFilter {
            action: Some("job.".into()),
            since_micros: Some(10_000),
            ..Default::default()
        };
        let page = store.audit_page(&runs, Some(start), 100).expect("page");
        let at: Vec<u64> = page.iter().map(|e| e.at_micros / 1000).collect();
        assert_eq!(at, vec![18, 16, 14, 12, 10]);

        assert_eq!(store.prune_audit_before(21_000).expect("prune"), 20);
        let page = store.audit_page(&all, None, 100).expect("page");
        assert_eq!(page.len(), 10);
        assert!(page.iter().all(|e| e.at_micros >= 21_000));
    }

    #[test]
    fn jobs_index_by_executor_and_state() {
        let (_d, store) = store();
        let job = |i: u8, exec: u8, state: JobState| HistoryJobRow {
            job_id: JobId([i; 16]),
            cmd_preview: format!("echo {i}"),
            runtime: "bash".into(),
            submitter: node(1),
            submitter_user: UserId([7; 32]),
            executor: node(exec),
            state,
            exit_code: Some(0),
            created_at_micros: u64::from(i) * 1000,
            started_at_micros: Some(u64::from(i) * 1000),
            finished_at_micros: Some(u64::from(i) * 1_000_000),
            updated_hlc: hlc(u64::from(i)),
            payload_sink: None,
            payload_key: None,
            from_archive: false,
            output: None,
        };
        store
            .index_jobs(&[
                job(1, 9, JobState::Succeeded),
                job(2, 9, JobState::Failed),
                job(3, 8, JobState::Failed),
            ])
            .expect("index");

        let failed = store
            .query_jobs(&HistoryQuery {
                state: Some("failed".into()),
                ..Default::default()
            })
            .expect("query");
        assert_eq!(failed.rows.len(), 2);

        let on_nine = store
            .query_jobs(&HistoryQuery {
                node: Some(node(9)),
                ..Default::default()
            })
            .expect("query");
        assert_eq!(on_nine.rows.len(), 2);

        let both = store
            .query_jobs(&HistoryQuery {
                state: Some("failed".into()),
                node: Some(node(9)),
                ..Default::default()
            })
            .expect("query");
        assert_eq!(both.rows.len(), 1);
        assert_eq!(both.rows[0].job_id.0[0], 2);
    }

    /// **§7's third crossing, on the index side.** A1 prunes the op-log row
    /// long before anyone asks the six-month question, and every re-index
    /// projects a *fresh* row from the replicated summary — which has never
    /// heard of the locator, because the locator is the producing node's
    /// discovery at export time. So the write has to carry it forward.
    ///
    /// Get this wrong and the locator survives until the next indexer tick,
    /// which is the worst possible failure mode: it works in every test
    /// that looks immediately and is gone by the time anybody needs it.
    #[test]
    fn a_reindex_carries_the_payload_locator_forward() {
        let (_d, store) = store();
        let id = JobId([4; 16]);
        let row = |hlc_at: u64| HistoryJobRow {
            job_id: id,
            cmd_preview: "etl".into(),
            runtime: "bash".into(),
            submitter: node(1),
            submitter_user: UserId([7; 32]),
            executor: node(1),
            state: JobState::Succeeded,
            exit_code: Some(0),
            created_at_micros: 1000,
            started_at_micros: Some(1000),
            finished_at_micros: Some(2000),
            updated_hlc: hlc(hlc_at),
            payload_sink: None,
            payload_key: None,
            from_archive: false,
            output: None,
        };
        store.index_jobs(&[row(1)]).expect("index");
        store
            .set_payload_locator(&id, "warehouse", "mesh-aaaa/jobs/job-4")
            .expect("locator");

        // A later projection from the replicated summary: the summary has
        // no locator and never will.
        store.index_jobs(&[row(2)]).expect("re-index");

        let back = store.job(&id).expect("read").expect("present");
        assert_eq!(back.payload_sink.as_deref(), Some("warehouse"));
        assert_eq!(back.payload_key.as_deref(), Some("mesh-aaaa/jobs/job-4"));

        // And a *newer* locator wins, because an export to a second
        // destination is a real thing that happens.
        store
            .set_payload_locator(&id, "siem", "mesh-aaaa/jobs/job-4")
            .expect("locator");
        assert_eq!(
            store
                .job(&id)
                .expect("read")
                .expect("present")
                .payload_sink
                .as_deref(),
            Some("siem")
        );
    }

    #[test]
    fn a_substring_filter_searches_the_command() {
        let (_d, store) = store();
        let job = |i: u8, cmd: &str| HistoryJobRow {
            job_id: JobId([i; 16]),
            cmd_preview: cmd.into(),
            runtime: "bash".into(),
            submitter: node(1),
            submitter_user: UserId([7; 32]),
            executor: node(1),
            state: JobState::Succeeded,
            exit_code: Some(0),
            created_at_micros: 1000,
            started_at_micros: Some(1000),
            finished_at_micros: Some(u64::from(i) * 1_000_000),
            updated_hlc: hlc(u64::from(i)),
            payload_sink: None,
            payload_key: None,
            from_archive: false,
            output: None,
        };
        store
            .index_jobs(&[job(1, "systemctl restart nginx"), job(2, "df -h")])
            .expect("index");

        let page = store
            .query_jobs(&HistoryQuery {
                contains: Some("NGINX".into()),
                ..Default::default()
            })
            .expect("query");
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].job_id.0[0], 1);
    }

    #[test]
    fn paging_walks_the_whole_set_without_repeats() {
        let (_d, store) = store();
        let rows: Vec<_> = (1..=20u8)
            .map(|i| job_row(i, "deploy", JobState::Succeeded, u64::from(i) * 1_000_000))
            .collect();
        store.index_jobs(&rows).expect("index");

        let mut seen = Vec::new();
        for page in 0..4 {
            let got = store
                .query_jobs(&HistoryQuery {
                    limit: 5,
                    offset: page * 5,
                    ..Default::default()
                })
                .expect("query");
            assert_eq!(got.rows.len(), 5);
            seen.extend(got.rows.iter().map(|r| r.job_id.0[0]));
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 20);
    }

    #[test]
    fn a_reopened_index_keeps_its_rows_and_takes_writes() {
        let (_d, store) = store();
        store
            .index_jobs(&[job_row(1, "deploy", JobState::Succeeded, 1_000_000)])
            .expect("index");
        store.reopen().expect("reopen");
        assert!(store.job(&JobId([1; 16])).expect("get").is_some());
        store
            .index_jobs(&[job_row(2, "deploy", JobState::Succeeded, 2_000_000)])
            .expect("index after reopen");
        let page = store.query_jobs(&HistoryQuery::default()).expect("query");
        assert_eq!(page.rows.len(), 2);
    }

    #[test]
    fn pruning_removes_rows_and_their_index_entries() {
        let (_d, store) = store();
        let rows: Vec<_> = (1..=6u8)
            .map(|i| job_row(i, "deploy", JobState::Succeeded, u64::from(i) * 1_000_000))
            .collect();
        store.index_jobs(&rows).expect("index");

        let jobs = store.prune_before(4_000_000).expect("prune");
        assert_eq!(jobs, 3);

        let page = store.query_jobs(&HistoryQuery::default()).expect("query");
        assert_eq!(page.rows.len(), 3);
        assert!(store.job(&JobId([1; 16])).expect("get").is_none());

        // Pruned rows must not haunt the secondary indexes.
        let by_state = store
            .query_jobs(&HistoryQuery {
                state: Some("succeeded".into()),
                ..Default::default()
            })
            .expect("query");
        assert_eq!(by_state.rows.len(), 3);
    }

    #[test]
    fn a_corrupt_index_is_rebuilt_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state").join("history.redb");
        {
            let store = HistoryStore::open(&path).expect("open");
            store
                .index_jobs(&[job_row(1, "deploy", JobState::Succeeded, 1_000_000)])
                .expect("index");
        }
        // Stamp a format from the future: a downgrade must recover, because
        // this file is a cache and refusing to start over one is absurd.
        {
            let db = Database::create(&path).expect("open");
            let txn = db.begin_write().expect("txn");
            {
                let mut meta = txn.open_table(META).expect("meta");
                meta.insert(FORMAT_KEY, INDEX_FORMAT + 99).expect("stamp");
            }
            txn.commit().expect("commit");
        }
        let store = HistoryStore::open(&path).expect("reopen");
        assert!(store.job(&JobId([1; 16])).expect("get").is_none());
        assert_eq!(store.stats(&path).expect("stats").jobs, 0);
    }
}
