//! Long-term run and job history: Parquet on disk, SQL over the top.
//!
//! ## What this is for
//!
//! A node's op-log is the truth and its index makes that truth searchable,
//! but both are shaped around the node's own lifetime: they live in its
//! home, they are its format, and reading them means being meshware. That
//! is the wrong shape for the thing people actually ask for — "keep every
//! run forever, let me back it up, let me query five years of it".
//!
//! So the archive is **a directory of ordinary Parquet files**:
//!
//! ```text
//! <home>/archive/
//! ├── manifest.json
//! ├── jobs/date=2026-08-21/part-000001.parquet
//! ├── jobs/date=2026-08-22/part-000002.parquet
//! └── audit/date=2026-08-22/part-000003.parquet
//! ```
//!
//! That layout is the entire argument for this design:
//!
//! - **It backs up with tools people already trust.** `restic`, `borg`,
//!   `rsync`, a USB stick, an S3 bucket. No export step, no dump format, no
//!   "make sure the daemon is stopped first".
//! - **It reads without meshware.** pandas, DuckDB, Polars, Spark, Athena,
//!   ClickHouse and every notebook on earth read Parquet. If this project
//!   disappeared tomorrow, the history would still open.
//! - **Nothing has to be running.** There is no archive server, no port, no
//!   process to keep alive, and therefore no new thing that can be down.
//!   That is not an implementation detail — it is the same constraint that
//!   shapes the rest of meshware, applied to storage.
//!
//! ## What it is not
//!
//! Not authoritative. The op-log is. If the two disagree the op-log wins,
//! and the archive can be deleted and rebuilt from any node that still has
//! the history. Not replicated, either: an archive is a local decision
//! about what to keep, and gossiping years of history to every node would
//! be exactly the centralization this avoids.
//!
//! ## Why DataFusion
//!
//! Because the alternative to SQL over Parquet is a hand-written query
//! layer that will grow into a bad SQL. DataFusion is pure Rust, reads the
//! files we already wrote, and as of late 2024 is the fastest single-node
//! Parquet engine on ClickBench. It costs a large dependency, which is why
//! it is confined to this crate and this crate is confined to Director.

pub mod bucket;
mod query;
mod schema;
mod write;

use std::path::{Path, PathBuf};

pub use query::{AuditProof, QueryResult, Table};
pub use schema::{
    ARCHIVE_FORMAT, FactArchiveRow, MESH_ID_COLUMN, PAYLOAD_KEY_COLUMN, PAYLOAD_SINK_COLUMN,
    facts_schema, jobs_schema, partition_of, read_schema_lenient, results_schema, usage_schema,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("archive io: {0}")]
    Io(#[from] std::io::Error),
    #[error("archive encode: {0}")]
    Encode(String),
    #[error("archive query: {0}")]
    Query(String),
    #[error("archive manifest: {0}")]
    Manifest(String),
    #[error(
        "this archive was written in format {found}, and this build understands {ARCHIVE_FORMAT}; \
         upgrade, or point at a different archive directory"
    )]
    Format { found: u32 },
    /// Writing needs to know which mesh the rows came from; reading does
    /// not. Raised rather than defaulted, because a `mesh_id` guessed at
    /// write time is a wrong answer baked into an immutable file.
    #[error(
        "this archive was opened for reading only — ingesting needs the mesh id, so open it \
         with `Archive::for_mesh`"
    )]
    MeshIdUnknown,
    /// Two meshes, one directory. The watermarks in the manifest are a
    /// single cursor, so this is not a merge — it is two histories
    /// interleaving into one set of part files with a cursor that skips
    /// half of each.
    #[error(
        "this archive holds history from mesh {holds}, and this node is {local}; point \
         `--archive` at a different directory (one archive, one mesh)"
    )]
    WrongMesh { holds: String, local: String },
    /// A bucket that was not reachable, not addressable, or not configured
    /// the way the operator asked for (M31 B4). Verbatim, because what the
    /// object store said is what sends somebody to the right console.
    #[error("bucket: {0}")]
    Bucket(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// What has already been archived. Small, human-readable, and the only
/// state the archive keeps outside the Parquet files themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// Which mesh these rows came from (M31 P0). Absent on an archive
    /// written before format 2, and on one that has never been ingested
    /// into.
    #[serde(default)]
    pub mesh_id: Option<String>,
    /// True when this directory holds part files written before `mesh_id`
    /// existed. Set once, when an ingest upgrades a format-1 manifest, and
    /// never cleared: the old files stay old.
    ///
    /// It is what tells the read path to go through the compatibility view
    /// rather than registering the tables directly, and keeping it explicit
    /// is what stops the common path — a format-2 archive — paying for a
    /// migration that does not apply to it.
    #[serde(default)]
    pub legacy_parts: bool,
    /// Highest job finish instant already written. Jobs at or below it are
    /// never written again, which is what makes repeated ingests safe.
    ///
    /// A `runs_watermark_micros` / `runs_watermark_id` pair sat beside this
    /// one until 2026-09-20, for the workflow-run table that went with the
    /// engine that wrote it. A manifest that still carries them loads: the
    /// fields are simply ignored, and the `runs/` day directories they
    /// tracked stay on disk and stay readable by any Parquet reader.
    pub jobs_watermark_micros: u64,
    /// The id half of the cursor (M31 P0).
    ///
    /// The watermark used to be an instant alone, and `write.rs` documented
    /// the gap that left: two jobs finishing in the same microsecond with a
    /// batch cut between them dropped the second. Vanishingly unlikely, and
    /// still the kind of thing that becomes a conversation with an auditor
    /// once a sink is copying these rows into a compliance store — so the
    /// cursor is now the pair `(finish_instant, id)` compared
    /// lexicographically, and a row is written when it sorts strictly above
    /// it.
    ///
    /// Empty on an archive written before this existed. Empty sorts below
    /// every real id, so the one row that set the old watermark is offered
    /// again on the next ingest — one duplicate, once, on an archive
    /// carried across the upgrade, and the alternative was to keep dropping
    /// rows silently.
    #[serde(default)]
    pub jobs_watermark_id: String,
    /// Monotonic part-file counter, so two ingests in the same second
    /// cannot pick the same filename.
    pub next_part: u64,
    pub jobs_written: u64,
    /// M18/M19 tables. `#[serde(default)]` so a manifest written before
    /// they existed still loads — the archive is disposable, but a
    /// gratuitous rebuild is still a rebuild.
    #[serde(default)]
    pub facts_watermark_micros: u64,
    /// The newest usage window archived, by its *start* instant (C1 P1).
    #[serde(default)]
    pub usage_watermark_micros: u64,
    #[serde(default)]
    pub facts_written: u64,
    #[serde(default)]
    pub results_written: u64,
    #[serde(default)]
    pub usage_written: u64,
    /// The audit trail's own cursor (M31 B4).
    ///
    /// Its own field rather than a shared one because audit rows settle by
    /// a rule nothing else uses: **there is no lag**. A run is archived
    /// five minutes after it finished, in case a peer's version arrives
    /// late; an audit entry is authored by *this* node about something it
    /// already did, and waiting on it would only mean a WORM bucket that is
    /// five minutes behind the trail it exists to preserve.
    #[serde(default)]
    pub audit_watermark_micros: u64,
    #[serde(default)]
    pub audit_written: u64,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            format: ARCHIVE_FORMAT,
            mesh_id: None,
            legacy_parts: false,
            jobs_watermark_micros: 0,
            jobs_watermark_id: String::new(),
            next_part: 1,
            jobs_written: 0,
            facts_watermark_micros: 0,
            usage_watermark_micros: 0,
            facts_written: 0,
            results_written: 0,
            usage_written: 0,
            audit_watermark_micros: 0,
            audit_written: 0,
        }
    }
}

/// A Parquet archive rooted at one directory.
///
/// `mesh_id` is the *writing* half of the identity and is absent on an
/// archive opened only to read — a restored copy on a machine that has
/// never been part of a mesh has no local mesh to name, and refusing to
/// open it would break the property this whole crate is built around.
#[derive(Debug, Clone)]
pub struct Archive {
    root: PathBuf,
    mesh_id: Option<String>,
}

/// What one ingest did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub jobs_written: u64,
    pub facts_written: u64,
    pub results_written: u64,
    pub usage_written: u64,
    pub audit_written: u64,
    /// Audit rows that failed their own signature binding and were **not**
    /// archived (M31 B4). Non-zero is a corruption or tampering report, not
    /// a statistic.
    pub audit_refused: u64,
    /// Rows that were already archived, or still in flight. Not an error:
    /// this is the number that should be large on a steady-state node.
    pub jobs_skipped: u64,
    pub audit_skipped: u64,
    pub parts_written: u64,
}

impl IngestReport {}

impl Archive {
    /// Open (or create) an archive. The directory is made on first write,
    /// not here — opening an archive to ask whether it has anything should
    /// not conjure one into existence.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let archive = Archive {
            root: root.into(),
            mesh_id: None,
        };
        // A format from the future is fatal, unlike the local index: this
        // file is the copy somebody may be relying on, and guessing at a
        // layout we do not understand is how an archive gets corrupted.
        let manifest = archive.manifest()?;
        if manifest.format > ARCHIVE_FORMAT {
            return Err(Error::Format {
                found: manifest.format,
            });
        }
        Ok(archive)
    }

    /// Open an archive for a named mesh — the form every ingest path uses.
    ///
    /// Refuses an archive that already holds a *different* mesh's rows.
    /// That is not defensiveness for its own sake: the manifest carries one
    /// cursor pair, so two meshes sharing a directory do not merge, they
    /// interleave, and each advances the other's watermark past rows it
    /// never wrote.
    pub fn for_mesh(root: impl Into<PathBuf>, mesh_id: impl Into<String>) -> Result<Self> {
        let mut archive = Archive::open(root)?;
        let mesh_id = mesh_id.into();
        let manifest = archive.manifest()?;
        if let Some(holds) = manifest.mesh_id.as_ref()
            && holds != &mesh_id
        {
            return Err(Error::WrongMesh {
                holds: holds.clone(),
                local: mesh_id,
            });
        }
        archive.mesh_id = Some(mesh_id);
        Ok(archive)
    }

    /// Tell a read-only archive which mesh is local, for projecting
    /// `mesh_id` onto format-1 rows that do not carry one.
    ///
    /// Distinct from [`Archive::for_mesh`] on purpose: this asserts nothing
    /// about the archive's contents and grants no right to write. It is
    /// what `meshware-archive query` does when it happens to find a node's
    /// genesis beside it, and skips when it does not.
    #[must_use]
    pub fn reading_as_mesh(mut self, mesh_id: impl Into<String>) -> Self {
        self.mesh_id = Some(mesh_id.into());
        self
    }

    /// The mesh whose rows this archive writes, if it has one.
    pub fn mesh_id(&self) -> Option<&str> {
        self.mesh_id.as_deref()
    }

    /// The mesh id to stamp on rows, or the error that says why there is
    /// none. Every ingest path starts here.
    pub(crate) fn writing_mesh_id(&self) -> Result<&str> {
        self.mesh_id.as_deref().ok_or(Error::MeshIdUnknown)
    }

    /// Does this directory hold part files written before `mesh_id`?
    ///
    /// True for a format-1 archive that has anything in it, and for one an
    /// ingest has since upgraded. False — the common case, and the cheap
    /// one — for an archive written entirely by this build.
    pub fn has_legacy_parts(&self) -> bool {
        match self.manifest() {
            Ok(m) => m.legacy_parts || (m.format < 2 && self.exists()),
            // An unreadable manifest is somebody else's error to report;
            // assuming the compatibility path here only costs a view.
            Err(_) => false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn exists(&self) -> bool {
        self.manifest_path().exists()
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    pub fn table_dir(&self, table: Table) -> PathBuf {
        self.root.join(table.dir_name())
    }

    /// Read the manifest, or a default one when the archive is new.
    pub fn manifest(&self) -> Result<Manifest> {
        let path = self.manifest_path();
        if !path.exists() {
            return Ok(Manifest::default());
        }
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Manifest(e.to_string()))
    }

    fn write_manifest(&self, manifest: &Manifest) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        // Write-then-rename: a manifest truncated by a crash would make the
        // archive look empty and the next ingest would duplicate everything
        // it had already written.
        let tmp = self.root.join("manifest.json.tmp");
        let bytes =
            serde_json::to_vec_pretty(manifest).map_err(|e| Error::Manifest(e.to_string()))?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, self.manifest_path())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_archive_reads_as_empty_without_creating_anything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("archive");
        let archive = Archive::open(&root).expect("open");
        assert!(!archive.exists());
        assert!(!root.exists(), "opening an archive must not create one");
        assert_eq!(archive.manifest().expect("manifest").jobs_written, 0);
    }

    #[test]
    fn an_archive_from_the_future_is_refused_not_guessed_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("archive");
        std::fs::create_dir_all(&root).expect("mkdir");
        let ahead = Manifest {
            format: ARCHIVE_FORMAT + 1,
            ..Default::default()
        };
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&ahead).expect("json"),
        )
        .expect("write");

        assert!(matches!(Archive::open(&root), Err(Error::Format { .. })));
    }
}
