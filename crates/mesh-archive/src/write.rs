//! Writing rows into date-partitioned Parquet, and compacting what that
//! produces.
//!
//! ## Exactly-once, by watermark
//!
//! Ingest is called repeatedly with overlapping input — a UI polls, a timer
//! fires, somebody clicks refresh — so it has to be safe to hand it the
//! same rows twice. It is, via two rules:
//!
//! 1. Only **terminal** rows are archived. A run still going will change,
//!    and freezing a half-finished run into an immutable file would archive
//!    a lie.
//! 2. Only rows whose finish instant is **strictly above the watermark**
//!    are written, and the watermark advances to the highest one written.
//!
//! The second rule is why the watermark is on the finish instant rather
//! than on ingest time: finish instants are a property of the data, so two
//! machines archiving the same history converge on the same files.
//!
//! There used to be an honest gap here, named rather than papered over:
//! two jobs can finish in the same microsecond, and a batch cut between
//! them dropped the second. **M31 P0 closed it**, and the fix is not the
//! expensive one that note feared — nothing tracks every archived id.
//! The cursor is simply the *pair* `(finish_instant, id)`, compared
//! lexicographically, and a row is written when it sorts strictly above it.
//! Ids are unique and total-ordered, so the pair is a total order over
//! terminal rows and the tie has nowhere left to hide.
//!
//! Why it was worth closing now rather than living with the lag: a sink
//! (M31 track B) copies these rows into somebody's compliance store, and
//! "vanishingly unlikely" is a different sentence when it is being said to
//! an auditor. It is also a fix that costs one comparison in one place, and
//! costs an `ALTER` on a customer's warehouse if it is made later.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use datafusion::arrow::datatypes::Schema;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::basic::Compression;
use datafusion::parquet::file::properties::WriterProperties;
use tracing::{debug, warn};

use mesh_proto::HistoryJobRow;

use crate::query::Table;
use crate::schema::{jobs_batch, partition_of, usage_batch};
use crate::{Archive, Error, IngestReport, Result};

/// How long a row must have been terminal before it is archived.
///
/// Not paranoia about clocks — insurance against replication. A run that
/// finished on another node reaches this one whenever gossip gets round to
/// it, and archiving five minutes behind the present costs nothing while
/// making "the row arrived late" a non-event.
pub const INGEST_LAG_MICROS: u64 = 5 * 60 * 1_000_000;

/// Part files in one date partition before it is worth rewriting them as
/// one. Parquet's per-file overhead — footer, schema, row-group metadata —
/// is what makes many small files slow to query, so this is the knob that
/// keeps a chatty ingest from degrading reads.
pub const COMPACT_AT_PARTS: usize = 8;

fn writer_props() -> WriterProperties {
    WriterProperties::builder()
        // Snappy, not zstd. Zstd compresses this data better — these rows
        // are mostly repeated short strings — but the `zstd` crate is a
        // binding to the C library, and buying a C toolchain dependency for
        // a few percent on files that are already small is the wrong trade
        // in a workspace that has spent real effort avoiding one. `snap` is
        // pure Rust, and Snappy is what every Parquet reader assumes.
        .set_compression(Compression::SNAPPY)
        .build()
}

/// The archive cursor: a finish instant paired with the row's own id.
///
/// The pair rather than the instant alone, because instants collide and ids
/// do not. Ordered by instant first so the cursor still advances with time
/// — an id-first order would archive by alphabet, which is not a watermark
/// at all — and by id second, which is arbitrary but *total*, and total is
/// the only property the tie-break needs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Cursor {
    pub micros: u64,
    pub id: String,
}

impl Cursor {
    pub fn new(micros: u64, id: String) -> Cursor {
        Cursor { micros, id }
    }

    /// Rebuild the cursor a manifest recorded.
    ///
    /// An empty id is what a pre-M31 manifest carries, and it sorts below
    /// every real one — so the single row that set the old instant-only
    /// watermark is offered once more. One duplicate on one upgrade, in
    /// exchange for never silently dropping a tied row again; and every
    /// consumer of this data deduplicates on id anyway (M31 §4 decision 5).
    pub fn from_manifest(micros: u64, id: &str) -> Cursor {
        Cursor {
            micros,
            id: id.to_string(),
        }
    }
}

impl Archive {
    /// Record this archive's mesh, and note whether the files already here
    /// predate the column. Returns whether the manifest changed.
    fn note_identity(&self, manifest: &mut crate::Manifest, mesh_id: &str) -> bool {
        let mut changed = false;
        if manifest.mesh_id.as_deref() != Some(mesh_id) {
            manifest.mesh_id = Some(mesh_id.to_string());
            changed = true;
        }
        // The upgrade moment, and the only place `legacy_parts` is ever
        // set: a directory that already held format-1 parts is about to
        // hold format-2 ones beside them, and the read path has to know.
        if manifest.format < crate::ARCHIVE_FORMAT {
            if self.exists() {
                manifest.legacy_parts = true;
            }
            manifest.format = crate::ARCHIVE_FORMAT;
            changed = true;
        }
        changed
    }

    /// Archive whatever in `jobs` is finished, settled and new.
    ///
    /// Safe to call with the same rows repeatedly; see the module note.
    pub fn ingest(&self, jobs: &[HistoryJobRow], now_micros: u64) -> Result<IngestReport> {
        let mesh_id = self.writing_mesh_id()?.to_string();
        let mut manifest = self.manifest()?;
        let cutoff = now_micros.saturating_sub(INGEST_LAG_MICROS);
        let mut report = IngestReport::default();

        // Group by partition so each date is one file, not one per row.
        let mut jobs_by_day: BTreeMap<String, Vec<HistoryJobRow>> = BTreeMap::new();
        let mut high_job =
            Cursor::from_manifest(manifest.jobs_watermark_micros, &manifest.jobs_watermark_id);
        let jobs_mark = high_job.clone();
        for row in jobs {
            let Some(finished) = row.finished_at_micros.filter(|f| *f > 0) else {
                report.jobs_skipped += 1;
                continue;
            };
            let here = Cursor::new(finished, row.job_id.to_display_string());
            if finished > cutoff || here <= jobs_mark {
                report.jobs_skipped += 1;
                continue;
            }
            high_job = high_job.max(here);
            jobs_by_day
                .entry(partition_of(finished))
                .or_default()
                .push(row.clone());
        }

        if jobs_by_day.is_empty() {
            // Deliberately *not* stamping the identity here. A pass that
            // archived nothing must leave the directory exactly as it found
            // it — including not conjuring a manifest into existence, which
            // is the property `Archive::open` is careful about and which a
            // polling caller would otherwise break on its first tick.
            return Ok(report);
        }

        for (day, rows) in &jobs_by_day {
            let batch = jobs_batch(&mesh_id, rows)?;
            let path = self.part_path(Table::Jobs, day, manifest.next_part);
            manifest.next_part += 1;
            write_parquet(&path, &batch)?;
            report.jobs_written += rows.len() as u64;
            report.parts_written += 1;
        }

        // The manifest is written *after* the data. A crash between the two
        // re-archives a batch, which duplicates rows; a crash the other way
        // round loses them for good. Duplicates are recoverable and gaps
        // are not, so the ordering is not arbitrary.
        manifest.jobs_watermark_micros = high_job.micros;
        manifest.jobs_watermark_id = high_job.id;
        manifest.jobs_written += report.jobs_written;
        self.note_identity(&mut manifest, &mesh_id);
        self.write_manifest(&manifest)?;

        debug!(
            jobs = report.jobs_written,
            parts = report.parts_written,
            "archived history"
        );
        Ok(report)
    }

    /// Archive closed usage windows (C1 P1).
    ///
    /// The settling rule is different from every other table here and it is
    /// the only interesting thing about this function. A run is finished
    /// when it is finished; a usage window keeps *growing* for as long as
    /// it is the current hour, because every job that lands adds to it. So
    /// what settles a window is not lag after a row appeared — it is the
    /// window having **ended**, plus the ordinary lag for stragglers whose
    /// counters have not gossiped yet. Archiving a live hour would freeze a
    /// partial total into a file nobody rewrites.
    pub fn ingest_usage(
        &self,
        rows: &[mesh_proto::UsageRow],
        now_micros: u64,
    ) -> Result<IngestReport> {
        let mesh_id = self.writing_mesh_id()?.to_string();
        let mut manifest = self.manifest()?;
        let mut report = IngestReport::default();
        let window_micros = mesh_proto::USAGE_WINDOW_SECS.saturating_mul(1_000_000);
        let cutoff = now_micros.saturating_sub(INGEST_LAG_MICROS);

        let mut by_day: BTreeMap<String, Vec<mesh_proto::UsageRow>> = BTreeMap::new();
        let mut high = manifest.usage_watermark_micros;
        for row in rows {
            let start = row.at_micros();
            let ends = start.saturating_add(window_micros);
            if ends > cutoff || start <= manifest.usage_watermark_micros {
                continue;
            }
            high = high.max(start);
            by_day.entry(partition_of(start)).or_default().push(*row);
        }
        if by_day.is_empty() {
            return Ok(report);
        }
        for (day, rows) in &by_day {
            let batch = usage_batch(&mesh_id, rows)?;
            let path = self.part_path(Table::Usage, day, manifest.next_part);
            manifest.next_part += 1;
            write_parquet(&path, &batch)?;
            report.usage_written += rows.len() as u64;
            report.parts_written += 1;
        }
        manifest.usage_watermark_micros = high;
        manifest.usage_written += report.usage_written;
        self.note_identity(&mut manifest, &mesh_id);
        self.write_manifest(&manifest)?;
        debug!(rows = report.usage_written, "archived usage windows");
        Ok(report)
    }

    /// Archive the audit trail, **with the signatures that prove it**
    /// (M31 B4).
    ///
    /// Its own pass, for two reasons that are both about the settling rule
    /// rather than about tidiness:
    ///
    /// - **No ingest lag.** Everything else waits [`INGEST_LAG_MICROS`] in
    ///   case a peer's version of a row arrives late. An audit entry is
    ///   this node's record of something it already did — nothing later
    ///   supersedes it, `AuditAppend` is `Grow` and never compacts — so
    ///   waiting would only make the WORM copy five minutes behind the
    ///   trail it exists to preserve.
    /// - **Rows are refused rather than silently dropped when they do not
    ///   verify.** Everything else here is a projection, and a bad
    ///   projection is a display bug. An audit row that fails its own
    ///   signature check is either corruption or tampering, and writing it
    ///   into an immutable bucket would put a lie somewhere nobody can
    ///   delete it from. The count comes back so a caller can shout.
    pub fn ingest_audit(&self, rows: &[mesh_proto::SignedAuditRow]) -> Result<IngestReport> {
        let mesh_id = self.writing_mesh_id()?.to_string();
        let mut manifest = self.manifest()?;
        let mut report = IngestReport::default();

        let mut by_day: BTreeMap<String, Vec<mesh_proto::SignedAuditRow>> = BTreeMap::new();
        let mut high = manifest.audit_watermark_micros;
        for row in rows {
            let at = row.entry.at_micros;
            if at <= manifest.audit_watermark_micros {
                report.audit_skipped += 1;
                continue;
            }
            if !row.binds_its_entry() {
                // Not written, and not silently: see the doc comment.
                warn!(
                    audit_id = %row.entry.audit_id.to_display_string(),
                    "audit row does not bind its own signed bytes — refusing to archive it"
                );
                report.audit_refused += 1;
                continue;
            }
            high = high.max(at);
            by_day
                .entry(partition_of(at))
                .or_default()
                .push(row.clone());
        }
        if by_day.is_empty() {
            return Ok(report);
        }
        for (day, rows) in &by_day {
            let batch = crate::schema::audit_batch(&mesh_id, rows)?;
            let path = self.part_path(Table::Audit, day, manifest.next_part);
            manifest.next_part += 1;
            write_parquet(&path, &batch)?;
            report.audit_written += rows.len() as u64;
            report.parts_written += 1;
        }
        manifest.audit_watermark_micros = high;
        manifest.audit_written += report.audit_written;
        self.note_identity(&mut manifest, &mesh_id);
        self.write_manifest(&manifest)?;
        debug!(rows = report.audit_written, "archived audit entries");
        Ok(report)
    }

    /// Archive fact observations (M18 §4.8) and result rows (M19 §5.7).
    ///
    /// Separate from [`Archive::ingest`] because the two have different
    /// settling rules. A run is finished when it is finished; a *fact* is
    /// never finished — it is the current answer until the machine changes
    /// — so what settles it is simply that it has already been written.
    /// The watermark is over the observation instant, and re-ingesting the
    /// same history writes nothing.
    ///
    /// A result set is written once, keyed by its group. Nothing settles
    /// it either: a fan-out that has closed does not produce more rows.
    pub fn ingest_knowledge(
        &self,
        facts: &[crate::FactArchiveRow],
        results: &[(String, Vec<mesh_proto::ResultRow>)],
    ) -> Result<IngestReport> {
        let mesh_id = self.writing_mesh_id()?.to_string();
        let mut manifest = self.manifest()?;
        let mut report = IngestReport::default();

        let mut facts_by_day: BTreeMap<String, Vec<crate::FactArchiveRow>> = BTreeMap::new();
        let mut high_fact = manifest.facts_watermark_micros;
        for row in facts {
            if row.observed_at_micros == 0
                || row.observed_at_micros <= manifest.facts_watermark_micros
            {
                continue;
            }
            high_fact = high_fact.max(row.observed_at_micros);
            facts_by_day
                .entry(partition_of(row.observed_at_micros))
                .or_default()
                .push(row.clone());
        }

        // Results partition by the *set's* newest row, so one fan-out is
        // one file rather than one file per node that happened to finish
        // after midnight.
        let mut results_by_day: BTreeMap<String, Vec<(String, Vec<mesh_proto::ResultRow>)>> =
            BTreeMap::new();
        for (group, rows) in results {
            if rows.is_empty() {
                continue;
            }
            let when = rows
                .iter()
                .filter_map(|r| r.finished_at_micros)
                .max()
                .unwrap_or(0);
            results_by_day
                .entry(partition_of(when))
                .or_default()
                .push((group.clone(), rows.clone()));
        }

        if facts_by_day.is_empty() && results_by_day.is_empty() {
            return Ok(report);
        }

        for (day, rows) in &facts_by_day {
            let batch = crate::schema::facts_batch(&mesh_id, rows)?;
            let path = self.part_path(Table::Facts, day, manifest.next_part);
            manifest.next_part += 1;
            write_parquet(&path, &batch)?;
            report.facts_written += rows.len() as u64;
            report.parts_written += 1;
        }
        for (day, sets) in &results_by_day {
            for (group, rows) in sets {
                let batch = crate::schema::results_batch(&mesh_id, group, rows)?;
                let path = self.part_path(Table::Results, day, manifest.next_part);
                manifest.next_part += 1;
                write_parquet(&path, &batch)?;
                report.results_written += rows.len() as u64;
                report.parts_written += 1;
            }
        }

        // Data first, manifest second — a crash between them re-archives a
        // batch, and duplicates are recoverable where gaps are not.
        manifest.facts_watermark_micros = high_fact;
        manifest.facts_written += report.facts_written;
        manifest.results_written += report.results_written;
        self.note_identity(&mut manifest, &mesh_id);
        self.write_manifest(&manifest)?;
        debug!(
            facts = report.facts_written,
            results = report.results_written,
            parts = report.parts_written,
            "archived knowledge"
        );
        Ok(report)
    }

    fn part_path(&self, table: Table, day: &str, seq: u64) -> PathBuf {
        self.table_dir(table)
            .join(format!("date={day}"))
            .join(format!("part-{seq:08}.parquet"))
    }

    /// Rewrite fragmented date partitions as one file each.
    ///
    /// Returns the partitions that were compacted. **Interrupting this
    /// cannot lose a row** (ENG-19): the merged file is written and fsynced
    /// under a staging name that readers ignore, and only then are the
    /// originals removed. A crash at any point leaves the staging file
    /// holding everything it covered, and the next pass finishes the job
    /// before it starts a new one.
    pub fn compact(&self, table: Table) -> Result<Vec<String>> {
        let dir = self.table_dir(table);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut compacted = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let partition = entry.path();
            // First, always: a merge this partition did not get to finish.
            // Before `part_files`, so a resumed merge is never mixed with a
            // new one over an overlapping set of parts.
            resume_interrupted_merges(&partition)?;
            let parts = part_files(&partition)?;
            if parts.len() < COMPACT_AT_PARTS {
                continue;
            }
            merge_parts(&partition, &parts, table, self.mesh_id())?;
            compacted.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(compacted)
    }

    /// Delete whole date partitions older than `cutoff_day` (ENG-19).
    ///
    /// Returns the partitions removed. `cutoff_day` is a `YYYY-MM-DD`
    /// string and the comparison is lexicographic, which for that format is
    /// chronological — the same property the partition layout was chosen
    /// for.
    ///
    /// **Whole days only, and never a partial one.** The archive is the
    /// cold tier: rows in it are immutable, a day is the unit everything
    /// else here is written and read in, and deleting rows *within* a
    /// partition would mean rewriting a Parquet file to remove history
    /// somebody may be mid-query on. A day is either past the window or it
    /// is not.
    ///
    /// The caller supplies the cutoff rather than a window, because the
    /// window is the *mesh's* policy and this crate deliberately does not
    /// talk to a mesh — `meshware-archive prune` asks the node for it.
    pub fn prune(&self, table: Table, cutoff_day: &str) -> Result<Vec<String>> {
        // Checked, because the failure mode of a malformed cutoff is
        // deleting the whole archive: every real partition sorts below
        // almost any other string, so one wrong argument — `date=2026-01-01`
        // instead of `2026-01-01` — would read as "everything is older than
        // this". A function that deletes history does not get to be relaxed
        // about its input.
        if !is_day(cutoff_day) {
            return Err(Error::Encode(format!(
                "prune needs a YYYY-MM-DD day, not {cutoff_day:?}"
            )));
        }
        let dir = self.table_dir(table);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut pruned = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(day) = name.strip_prefix("date=") else {
                // Not a partition this crate wrote. Left alone: an archive
                // directory is the operator's, and something that is not
                // ours is not ours to delete.
                continue;
            };
            if day >= cutoff_day {
                continue;
            }
            std::fs::remove_dir_all(entry.path())?;
            pruned.push(name);
        }
        pruned.sort();
        sync_dir(&dir);
        Ok(pruned)
    }

    /// Which partitions [`Self::prune`] would remove, without removing
    /// them. The dry run an operator gets before deleting history.
    pub fn partitions_before(&self, table: Table, cutoff_day: &str) -> Result<Vec<String>> {
        if !is_day(cutoff_day) {
            return Err(Error::Encode(format!(
                "prune needs a YYYY-MM-DD day, not {cutoff_day:?}"
            )));
        }
        let dir = self.table_dir(table);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut found: Vec<String> = std::fs::read_dir(&dir)?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.strip_prefix("date=").is_some_and(|day| day < cutoff_day))
            .collect();
        found.sort();
        Ok(found)
    }

    /// Total bytes on disk, and the file count behind it.
    pub fn disk_usage(&self) -> (u64, u64) {
        fn walk(dir: &Path, bytes: &mut u64, files: &mut u64) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    walk(&entry.path(), bytes, files);
                } else if let Ok(meta) = entry.metadata() {
                    *bytes += meta.len();
                    *files += 1;
                }
            }
        }
        let mut bytes = 0;
        let mut files = 0;
        walk(&self.root, &mut bytes, &mut files);
        (bytes, files)
    }
}

fn part_files(partition: &Path) -> Result<Vec<PathBuf>> {
    let mut parts: Vec<PathBuf> = std::fs::read_dir(partition)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "parquet"))
        .collect();
    parts.sort();
    Ok(parts)
}

/// Bring a batch written by an older format up to the current schema.
///
/// This is where a legacy archive quietly heals: compaction already rewrites
/// a partition, so it costs nothing to rewrite it into the shape everything
/// else is in, and a directory that has been compacted once no longer needs
/// the compatibility view for that partition. Columns present in the file
/// are carried through by name; `mesh_id` is filled with the local mesh, and
/// anything else missing is filled with nulls.
///
/// Returns `None` when the batch cannot be brought forward — a legacy file
/// with no local mesh id to stamp on it. That is a *skip*, not an error: an
/// archive being read on a machine with no mesh is a supported state, and
/// refusing to compact it is a far better outcome than refusing to open it.
fn upgrade_batch(
    batch: &RecordBatch,
    target: &Schema,
    mesh_id: Option<&str>,
) -> Option<RecordBatch> {
    use datafusion::arrow::array::new_null_array;

    if batch.schema().fields() == target.fields() {
        return Some(batch.clone());
    }
    let rows = batch.num_rows();
    let mut columns: Vec<datafusion::arrow::array::ArrayRef> =
        Vec::with_capacity(target.fields().len());
    for field in target.fields() {
        match batch.column_by_name(field.name()) {
            Some(col) if col.data_type() == field.data_type() => columns.push(col.clone()),
            _ if field.name() == crate::MESH_ID_COLUMN => {
                let mut b = datafusion::arrow::array::StringBuilder::new();
                for _ in 0..rows {
                    b.append_value(mesh_id?);
                }
                columns.push(std::sync::Arc::new(b.finish()));
            }
            // A missing column the schema says may be null: the payload
            // locator, today, and whatever P0's successors reserve next.
            _ if field.is_nullable() => columns.push(new_null_array(field.data_type(), rows)),
            // A missing non-nullable column is a file this build cannot
            // honestly rewrite. Leave it where it is.
            _ => return None,
        }
    }
    RecordBatch::try_new(std::sync::Arc::new(target.clone()), columns).ok()
}

fn merge_parts(
    partition: &Path,
    parts: &[PathBuf],
    table: Table,
    mesh_id: Option<&str>,
) -> Result<()> {
    use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let target = table.schema();
    let mut batches = Vec::new();
    for path in parts {
        let file = std::fs::File::open(path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(|e| Error::Encode(e.to_string()))?
            .build()
            .map_err(|e| Error::Encode(e.to_string()))?;
        for batch in reader {
            let batch = batch.map_err(|e| Error::Encode(e.to_string()))?;
            match upgrade_batch(&batch, &target, mesh_id) {
                Some(b) => batches.push(b),
                // One file this pass cannot bring forward makes the whole
                // merge unsafe: the survivors would be written under the
                // merged name and the skipped file deleted with the rest.
                None => {
                    debug!(
                        partition = %partition.display(),
                        "left a pre-mesh_id partition uncompacted (no local mesh id to stamp)"
                    );
                    return Ok(());
                }
            }
        }
    }
    if batches.is_empty() {
        return Ok(());
    }

    // The range this merge covers, carried in the staging file's own name
    // so that a pass which finds it after a crash knows exactly which parts
    // it stands for — and does not touch anything an ingest added since.
    let (Some(lo), Some(hi)) = (part_seq(&parts[0]), part_seq(&parts[parts.len() - 1])) else {
        debug!(
            partition = %partition.display(),
            "left a partition uncompacted (a part file is not named part-NNNNNNNN.parquet)"
        );
        return Ok(());
    };

    let tmp = partition.join(staging_name(lo, hi));
    {
        let file = std::fs::File::create(&tmp)?;
        // A second handle on the same file: `ArrowWriter::close` consumes
        // the writer and drops the one it holds, and a Parquet file whose
        // footer is still in the page cache is a file no reader can open.
        let sync = file.try_clone()?;
        let mut writer = ArrowWriter::try_new(file, table.schema(), Some(writer_props()))
            .map_err(|e| Error::Encode(e.to_string()))?;
        for batch in &batches {
            writer
                .write(batch)
                .map_err(|e| Error::Encode(e.to_string()))?;
        }
        writer.close().map_err(|e| Error::Encode(e.to_string()))?;
        sync.sync_all()?;
    }
    // Durable *before* anything is deleted. This is the whole ordering:
    // previously the originals went first and the rename followed, so a
    // crash in between left the merged rows only in a file nothing reads
    // and the partition permanently short.
    sync_dir(partition);

    finish_merge(partition, lo, hi, &tmp)
}

/// Put a staged merge into place: remove the parts it covers, then take the
/// lowest part's name. Idempotent, so it is also the recovery path.
///
/// The merged file takes the lowest part number, so ordering by name still
/// reflects ordering by time and the next ingest's counter cannot collide
/// with it.
fn finish_merge(partition: &Path, lo: u64, hi: u64, tmp: &Path) -> Result<()> {
    for path in part_files(partition)? {
        match part_seq(&path) {
            Some(seq) if seq >= lo && seq <= hi => std::fs::remove_file(&path)?,
            // Anything outside the range is an ingest that arrived after
            // this merge began. Its rows are not in the staging file, so it
            // stays.
            _ => {}
        }
    }
    sync_dir(partition);
    std::fs::rename(tmp, partition.join(format!("part-{lo:08}.parquet")))?;
    sync_dir(partition);
    Ok(())
}

/// Finish any merge a previous pass staged and did not complete (ENG-19).
fn resume_interrupted_merges(partition: &Path) -> Result<()> {
    let mut staged: Vec<(u64, u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(partition)?.flatten() {
        let path = entry.path();
        if let Some((lo, hi)) = staging_range(&path) {
            staged.push((lo, hi, path));
        }
    }
    staged.sort();
    for (lo, hi, tmp) in staged {
        debug!(
            partition = %partition.display(),
            lo, hi, "finishing a merge a previous pass did not complete"
        );
        finish_merge(partition, lo, hi, &tmp)?;
    }
    Ok(())
}

/// The staging name for a merge covering parts `lo..=hi`. Not a `.parquet`
/// file, so [`part_files`] — and therefore every reader — ignores it.
fn staging_name(lo: u64, hi: u64) -> String {
    format!("merging-{lo:08}-{hi:08}.parquet.tmp")
}

/// The range a staging file covers, if this path is one.
fn staging_range(path: &Path) -> Option<(u64, u64)> {
    let name = path.file_name()?.to_str()?;
    let rest = name
        .strip_prefix("merging-")?
        .strip_suffix(".parquet.tmp")?;
    let (lo, hi) = rest.split_once('-')?;
    Some((lo.parse().ok()?, hi.parse().ok()?))
}

/// Is this a `YYYY-MM-DD` day, the form partition names carry and the only
/// form a cutoff may be given in?
fn is_day(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
}

/// The sequence number in `part-NNNNNNNN.parquet`.
fn part_seq(path: &Path) -> Option<u64> {
    path.file_name()?
        .to_str()?
        .strip_prefix("part-")?
        .strip_suffix(".parquet")?
        .parse()
        .ok()
}

/// fsync a directory, so a rename or an unlink inside it is durable. A
/// directory that cannot be opened for sync is not a reason to fail work
/// that has already landed, so this is deliberately quiet.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

fn write_parquet(path: &Path, batch: &RecordBatch) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Same reason as the manifest: a reader must never see a half-written
    // Parquet file, and a truncated footer makes the whole file unreadable
    // rather than short.
    let tmp = path.with_extension("parquet.tmp");
    {
        let file = std::fs::File::create(&tmp)?;
        let sync = file.try_clone()?;
        let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(writer_props()))
            .map_err(|e| Error::Encode(e.to_string()))?;
        writer
            .write(batch)
            .map_err(|e| Error::Encode(e.to_string()))?;
        writer.close().map_err(|e| Error::Encode(e.to_string()))?;
        // Before the rename, not after: the rename is what publishes these
        // bytes, and a footer still in the page cache makes the whole file
        // unreadable rather than short (ENG-19).
        sync.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
    Ok(())
}
