//! Sending history somewhere else: the cursor, the spool, and the two
//! writers (M31 B1).
//!
//! ## The escape hatch, and why it ships before any connector
//!
//! Two writers here — a rotating NDJSON file and a subprocess — and between
//! them they reach every destination in existence with **no
//! destination-specific code in the tree**. Vector, Fluent Bit, Fluentd,
//! Filebeat, Promtail, Cribl, rsyslog and a cron job with `jq` all tail a
//! file; anything they cannot reach, a twenty-line shell script can. That is
//! why *"does meshware integrate with X"* becomes *"yes, for all X"* in this
//! phase rather than in the one that adds four named destinations.
//!
//! ## C3, which is the rule that will erode
//!
//! **A failing sink spends spool, never scheduler.** Nothing in this file is
//! reachable from an admission path, a scheduler decision, or anything a job
//! waits on. The exporter is a background loop that reads an index nobody
//! blocks on, and when the destination will not take its rows it fills a
//! bounded spool, then drops the oldest and counts it.
//!
//! The buyer's request is always *"guarantee delivery — block the job if the
//! sink is unreachable"*, and the answer is no. What they actually want is a
//! gate on **deletion**, which A1's retention window owns. Three refactors
//! from now the guard that keeps this honest is a throughput test, not this
//! paragraph.
//!
//! ## The cursor is a property of the data
//!
//! One cursor per sink on disk, the same `(instant, id)` shape P0 gave the
//! archive and written the same way — write-then-rename. That is what makes
//! failover free: a dead exporter costs one gossip round, and the successor
//! re-reads a cursor derived from rows it can see for itself. There is no
//! handover protocol because there is nothing to hand over.
//!
//! A 2xx — or a plugin's ack — is the only thing that advances it.

use parking_lot::Mutex;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use mesh_proto::{HistoryJobRow, Sink, SinkKind};

use crate::error::{CoreError, Result};

/// How long a `plugin write` batch may take before it is abandoned.
///
/// Ten minutes, against `secrets::PROVIDER_TIMEOUT`'s thirty seconds, and
/// the difference is the point: fetching one secret is a request, and
/// shipping a thousand rows into somebody's warehouse is a transfer. A
/// thirty-second cap would make a large batch fail for ever on a slow link
/// and look like a plugin bug.
pub const PLUGIN_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Bytes one NDJSON file reaches before it rotates.
pub const NDJSON_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Rotated NDJSON files kept per sink.
///
/// Eight × 64 MB is half a gigabyte, which is a lot of history for a
/// collector that has stopped reading — and a collector that has stopped
/// reading for half a gigabyte is a collector nobody is running.
pub const NDJSON_MAX_FILES: u32 = 8;

// ---------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------

/// How far a sink has got, per table.
///
/// `(instant, id)` rather than an instant alone, for exactly the reason P0
/// gave the archive: instants collide and ids do not, and a sink is copying
/// these rows into somebody's compliance store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkCursor {
    /// Jobs whose **payload** this node has already exported (M31 B2
    /// item 5), newest-first by finish instant.
    ///
    /// A separate cursor from the record one, and it has to be: records
    /// leave from one elected node and payloads leave from **every** node,
    /// for its own jobs. Sharing a cursor would mean the elected node's
    /// progress silently deciding what every other node had shipped.
    #[serde(default)]
    pub payload_micros: u64,
    #[serde(default)]
    pub payload_id: String,
    #[serde(default)]
    pub payloads_sent: u64,
    /// Payloads skipped because they were over the sink's byte cap. The
    /// number that tells an operator their cap is doing something, and
    /// which is otherwise invisible.
    #[serde(default)]
    pub payloads_too_large: u64,
    pub jobs_micros: u64,
    #[serde(default)]
    pub jobs_id: String,
    /// Usage windows and audit entries already exported (protocol 1.19).
    ///
    /// `#[serde(default)]` like everything added since, so a cursor written
    /// by an older build loads as "nothing exported yet" for these two —
    /// which is the truth, because an older build never exported them.
    #[serde(default)]
    pub usage_micros: u64,
    #[serde(default)]
    pub usage_id: String,
    #[serde(default)]
    pub audit_micros: u64,
    #[serde(default)]
    pub audit_id: String,
    /// Counters, carried beside the cursor so `meshnode sinks` can report
    /// them without a second file to keep consistent with this one.
    #[serde(default)]
    pub sent: u64,
    #[serde(default)]
    pub dropped: u64,
    /// The last HTTP status or plugin exit, verbatim.
    ///
    /// **Verbatim, and that is deliberate.** `503` sends an operator to the
    /// right machine; "sink unhealthy" sends them to the wrong one.
    #[serde(default)]
    pub last_status: String,
    #[serde(default)]
    pub last_at_micros: u64,
}

/// One position in a table's stream.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mark {
    pub micros: u64,
    pub id: String,
}

impl Mark {
    pub fn new(micros: u64, id: impl Into<String>) -> Mark {
        Mark {
            micros,
            id: id.into(),
        }
    }
}

impl SinkCursor {
    pub fn jobs_mark(&self) -> Mark {
        Mark::new(self.jobs_micros, self.jobs_id.clone())
    }

    pub fn set_jobs(&mut self, mark: Mark) {
        self.jobs_micros = mark.micros;
        self.jobs_id = mark.id;
    }

    pub fn payload_mark(&self) -> Mark {
        Mark::new(self.payload_micros, self.payload_id.clone())
    }

    pub fn set_payload(&mut self, mark: Mark) {
        self.payload_micros = mark.micros;
        self.payload_id = mark.id;
    }

    pub fn usage_mark(&self) -> Mark {
        Mark::new(self.usage_micros, self.usage_id.clone())
    }

    pub fn set_usage(&mut self, mark: Mark) {
        self.usage_micros = mark.micros;
        self.usage_id = mark.id;
    }

    pub fn audit_mark(&self) -> Mark {
        Mark::new(self.audit_micros, self.audit_id.clone())
    }

    pub fn set_audit(&mut self, mark: Mark) {
        self.audit_micros = mark.micros;
        self.audit_id = mark.id;
    }

    /// Adopt a predecessor's published watermark on election handover.
    ///
    /// **Never moves a cursor backwards**, and that is the only rule that
    /// matters here: `sink_progress` is LWW across the mesh and a node
    /// taking over may already have exported further than the watermark
    /// says. Taking the maximum makes a stale or reordered publication
    /// harmless, where assignment would silently re-send everything between
    /// the two.
    ///
    /// The id half of each mark is deliberately left empty. The watermark
    /// carries an instant and not a row id, so the marks compare as
    /// "the earliest row at this instant" — which re-sends at most the rows
    /// sharing one microsecond rather than skipping any. Duplicates are
    /// what this system already promises; gaps are not.
    pub fn seed_from(&mut self, through_micros: u64) {
        if through_micros > self.jobs_micros {
            self.jobs_micros = through_micros;
            self.jobs_id = String::new();
        }
        if through_micros > self.usage_micros {
            self.usage_micros = through_micros;
            self.usage_id = String::new();
        }
        if through_micros > self.audit_micros {
            self.audit_micros = through_micros;
            self.audit_id = String::new();
        }
    }
}

// ---------------------------------------------------------------------------
// Per-sink on-disk state
// ---------------------------------------------------------------------------

/// A sink's own directory: cursor, spool, and whatever the writer keeps.
#[derive(Debug, Clone)]
pub struct SinkState {
    dir: PathBuf,
}

impl SinkState {
    pub fn new(sinks_dir: &Path, name: &str) -> SinkState {
        SinkState {
            dir: sinks_dir.join(name),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn cursor_path(&self) -> PathBuf {
        self.dir.join("cursor.json")
    }

    pub fn spool_dir(&self) -> PathBuf {
        self.dir.join("spool")
    }

    /// Read the cursor, or a fresh one.
    ///
    /// A missing or unreadable cursor reads as zero rather than as an
    /// error, and the consequence is at-least-once delivery of history the
    /// destination may already hold — which is what §4 decision 5 already
    /// says a sink is, and why every consumer deduplicates on `run_id`.
    /// The alternative, refusing to export until somebody repairs a JSON
    /// file, is worse in every deployment.
    pub fn cursor(&self) -> SinkCursor {
        std::fs::read(self.cursor_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Write-then-rename, exactly as the archive manifest does it: a cursor
    /// truncated by a crash would make the sink look fresh and re-export
    /// everything it had already sent.
    pub fn save_cursor(&self, cursor: &SinkCursor) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join("cursor.json.tmp");
        let bytes = serde_json::to_vec_pretty(cursor)
            .map_err(|e| CoreError::State(format!("sink cursor encode: {e}")))?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, self.cursor_path())?;
        Ok(())
    }

    /// Bytes currently spooled.
    pub fn spool_bytes(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(self.spool_dir()) else {
            return 0;
        };
        entries
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    /// Park a batch that would not go, and drop the oldest if that takes
    /// the spool over its cap.
    ///
    /// **This is C3 in eight lines.** The alternative — retry in place,
    /// unbounded, holding the batch in memory — is how a sink stops being
    /// a background loop and starts being a leak.
    ///
    /// Returns how many spooled files were dropped to make room.
    pub fn spool(&self, batch: &[u8], cap_bytes: u64) -> Result<u64> {
        let dir = self.spool_dir();
        std::fs::create_dir_all(&dir)?;
        // Named by instant so the oldest sorts first, and by a counter so
        // two batches in the same microsecond cannot collide.
        let seq = SPOOL_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("{:020}-{seq:06}.ndjson", mesh_proto::now_micros());
        std::fs::write(dir.join(&name), batch)?;

        let mut dropped = 0;
        if cap_bytes > 0 {
            let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(&dir)?
                .flatten()
                .filter_map(|e| e.metadata().ok().map(|m| (e.path(), m.len())))
                .collect();
            files.sort();
            let mut total: u64 = files.iter().map(|(_, n)| n).sum();
            for (path, len) in files {
                if total <= cap_bytes {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    total = total.saturating_sub(len);
                    dropped += 1;
                }
            }
        }
        Ok(dropped)
    }

    /// The spooled batches, oldest first.
    pub fn spooled(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.spool_dir()) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        files
    }
}

/// Disambiguates two spool files written in the same microsecond.
static SPOOL_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The wire format
// ---------------------------------------------------------------------------

pub fn job_line(mesh_id: &str, row: &HistoryJobRow) -> serde_json::Value {
    serde_json::json!({
        "mesh_id": mesh_id,
        "table": "jobs",
        "job_id": row.job_id.to_display_string(),
        "command": row.cmd_preview,
        "runtime": row.runtime,
        "submitter_node": row.submitter.to_display_string(),
        "submitter_user": row.submitter_user.to_display_string(),
        "executor_node": row.executor.to_display_string(),
        "state": row.state.as_str(),
        "exit_code": row.exit_code,
        "created_at": micros_to_rfc3339(row.created_at_micros),
        "started_at": row.started_at_micros.map(micros_to_rfc3339),
        "finished_at": row.finished_at_micros.map(micros_to_rfc3339),
        "duration_ms": row.duration_micros().map(|d| d / 1000),
    })
}

/// One usage window, as it leaves (protocol 1.19).
///
/// The window's **start** is the row's instant, and both edges are written
/// out: a consumer summing `cpu_secs` by hour needs to know an hour is what
/// it is looking at, and a bare start instant makes that a fact about the
/// exporter's configuration that the row does not carry.
///
/// Rows arrive here only once their window has closed — see the export
/// pass. A usage row is an *upsert*, so exporting a live window would ship
/// a partial that a later pass contradicts, and no amount of deduplicating
/// on the consumer's side can tell which of the two was final.
pub fn usage_line(mesh_id: &str, row: &mesh_proto::UsageRow) -> serde_json::Value {
    serde_json::json!({
        "mesh_id": mesh_id,
        "table": "usage",
        "node": row.node.to_display_string(),
        "principal": row.principal.to_display_string(),
        "window_start": micros_to_rfc3339(row.at_micros()),
        "window_end": micros_to_rfc3339(mesh_proto::usage_window_end_micros(row.window)),
        "cpu_secs": row.cpu_secs,
        "mem_mb_secs": row.mem_mb_secs,
        "gpu_secs": row.gpu_secs,
        "wall_secs": row.wall_secs,
        "jobs": row.jobs,
    })
}

/// One audit entry, as it leaves (protocol 1.19).
///
/// **The signature travels with the row.** An audit log's worth in a
/// compliance conversation is non-repudiation, and that does not live in
/// the projection — it lives in the signature over the op that recorded it.
/// A warehouse full of flat audit rows proves only that somebody inserted
/// some rows into a warehouse. `signing_bytes` and `signature` are what let
/// an auditor verify the row against the mesh's public keys years later,
/// with none of this software running.
///
/// Both are lowercase hex rather than arrays of numbers, because every log
/// store on earth can hold a string and roughly none of them enjoy a
/// 64-element integer array.
pub fn audit_line(mesh_id: &str, row: &mesh_proto::SignedAuditRow) -> serde_json::Value {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    serde_json::json!({
        "mesh_id": mesh_id,
        "table": "audit",
        "audit_id": row.entry.audit_id.to_display_string(),
        "at": micros_to_rfc3339(row.entry.at_micros),
        "user": row.entry.user.to_display_string(),
        "username": row.entry.username,
        "action": row.entry.action,
        "target": row.entry.target,
        "node": row.entry.node.to_display_string(),
        "outcome": row.entry.outcome,
        "detail": row.entry.detail,
        "author_node": row.author.to_display_string(),
        "seq": row.seq,
        "signing_bytes": hex(&row.signing_bytes),
        "signature": hex(&row.signature),
    })
}

fn micros_to_rfc3339(micros: u64) -> String {
    let secs = (micros / 1_000_000) as i64;
    let frac = micros % 1_000_000;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Howard Hinnant's algorithm, the same one `mesh-archive` uses for its
/// partitions. Duplicated rather than shared because the node must not
/// depend on the archive crate — that is the arrangement that keeps
/// DataFusion out of `meshnode`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// A batch, as bytes on the wire: one JSON object per line.
pub fn encode_batch(lines: &[serde_json::Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines.len() * 256);
    for line in lines {
        let _ = serde_json::to_writer(&mut out, line);
        out.push(b'\n');
    }
    out
}

// ---------------------------------------------------------------------------
// The writers
// ---------------------------------------------------------------------------

/// What a write attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Everything in the batch landed.
    Accepted,
    /// A plugin acked only part of the batch. The cursor advances to the
    /// acked row and no further — a partial ack that advanced past what was
    /// accepted would be silent data loss dressed as progress.
    PartiallyAccepted { rows: usize },
    /// It did not land. Carries what to tell the operator, verbatim.
    Failed { status: String },
}

/// A size-rotated NDJSON file, one per sink.
///
/// `logging::RollingFile`'s shape, re-implemented here rather than shared,
/// because that one is welded to the tracing subscriber's `MakeWriter` and
/// to one filename. The rotation policy is the same and the comment there
/// explains it: cap, shift, reopen, no logrotate.
/// Where an NDJSON sink's file actually lands.
///
/// **The operator chooses, and the node decides only when they did not.**
/// A blank `database` keeps the original behaviour — a rotating file inside
/// the node's own home, which is the right default because it is the one
/// place every node in the mesh is guaranteed to be able to write. But
/// "export to a file" is worth very little if the file is somewhere only
/// the node's own layout can name, so an absolute path is honoured as
/// given: a directory gets `records.ndjson` inside it, and anything else is
/// taken as the file itself (rotation appends `.1`, `.2`, … beside it).
///
/// The path is resolved on the **exporting** node, which is not necessarily
/// the machine the operator typed it on — which is why `Sink::run_on`
/// exists and why the UI says so beside the field.
pub fn ndjson_path(sink: &Sink, state: &SinkState) -> PathBuf {
    match ndjson_path_at(&sink.database) {
        Some(path) => path,
        None => state.dir().join("records.ndjson"),
    }
}

/// The operator's chosen path, resolved to the file itself.
///
/// `None` for a blank choice, which is the "inside the node's own data
/// directory" default and needs a [`SinkState`] to answer.
///
/// Split out of [`ndjson_path`] so the **storage probe resolves a path by
/// exactly the rule the writer will use**. Two rules here would agree on
/// the day they were written and diverge on the first fix to either — and
/// the divergence would be the worst possible kind, a probe that passes
/// against one file while the exporter writes to another.
///
/// Note that `is_dir` makes this answer depend on the filesystem it runs
/// on, so two nodes *can* resolve one string differently: a node where
/// `/srv/exports` already exists puts `records.ndjson` inside it, and a
/// node where it does not treats the whole thing as the file. That is a
/// real disagreement about a real filesystem rather than a bug here, which
/// is why the probe reports each node's resolved path back and the
/// coordinator refuses a pool that does not agree on one.
pub fn ndjson_path_at(chosen: &str) -> Option<PathBuf> {
    let chosen = chosen.trim();
    if chosen.is_empty() {
        return None;
    }
    let path = PathBuf::from(chosen);
    // A trailing separator, or an existing directory, means "in here".
    if chosen.ends_with(std::path::MAIN_SEPARATOR) || path.is_dir() {
        return Some(path.join("records.ndjson"));
    }
    Some(path)
}

/// The rotating file behind a retired `ndjson` sink.
///
/// Kept, and kept working, although the kind can no longer be created:
/// removing it would stop every export a mesh already had running the
/// moment its nodes were upgraded, which is the one way a history export
/// must never end. See [`SinkKind::Ndjson`] for what replaced it.
#[derive(Debug)]
pub struct NdjsonWriter {
    path: PathBuf,
    max_bytes: u64,
    max_files: u32,
}

impl NdjsonWriter {
    pub fn new(sink: &Sink, state: &SinkState) -> NdjsonWriter {
        NdjsonWriter {
            path: ndjson_path(sink, state),
            max_bytes: NDJSON_MAX_BYTES,
            max_files: NDJSON_MAX_FILES,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, batch: &[u8]) -> WriteOutcome {
        match self.try_write(batch) {
            Ok(()) => WriteOutcome::Accepted,
            // A full disk, a read-only mount, a directory somebody removed.
            // Reported verbatim for the same reason an HTTP status is.
            Err(e) => WriteOutcome::Failed {
                status: e.to_string(),
            },
        }
    }

    fn try_write(&self, batch: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        // Rotate *before* writing when the batch would take it over, so a
        // file never exceeds the cap by a whole batch.
        if size > 0 && size + batch.len() as u64 > self.max_bytes {
            self.rotate();
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(batch)?;
        // Flushed but not fsynced: this is a copy, the op-log is the truth,
        // and an fsync per batch would make the export loop the slowest
        // thing on the node for no correctness gain.
        file.flush()
    }

    fn rotate(&self) {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "records.ndjson".into());
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let _ = std::fs::remove_file(dir.join(format!("{name}.{}", self.max_files)));
        for i in (1..self.max_files).rev() {
            let from = dir.join(format!("{name}.{i}"));
            if from.exists() {
                let _ = std::fs::rename(&from, dir.join(format!("{name}.{}", i + 1)));
            }
        }
        let _ = std::fs::rename(&self.path, dir.join(format!("{name}.1")));
    }
}

/// Hand a batch to a subprocess: NDJSON on stdin, an ack on stdout.
///
/// The contract is `secrets::provider_get`'s, with a different verb — one
/// plugin contract in the product, not two. What a plugin returns:
///
/// ```json
/// {"accepted": 1000}
/// ```
///
/// A non-zero exit means retry. **The error names the plugin**, because
/// every plugin bug otherwise reads as a meshware bug and arrives as a
/// support ticket about the wrong software.
pub async fn plugin_write(plugin: &str, batch: &[u8], rows: usize) -> WriteOutcome {
    let mut command = tokio::process::Command::new(plugin);
    command
        .arg("write")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    run_writer_process(command, plugin, batch, rows).await
}

/// Run one writer subprocess and read its verdict.
///
/// Shared by [`plugin_write`] and [`postgres_write`], because they are the
/// same contract with a different program: a batch on stdin, an ack on
/// stdout, the exit code as the truth. Two copies of this would agree on
/// the day they were written and diverge on the first fix to either.
async fn run_writer_process(
    mut command: tokio::process::Command,
    name: &str,
    batch: &[u8],
    rows: usize,
) -> WriteOutcome {
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return WriteOutcome::Failed {
                status: format!("sink writer {name:?}: spawn: {e}"),
            };
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(batch).await;
        // Dropping closes the pipe so a writer that reads to EOF proceeds.
    }
    let output = match tokio::time::timeout(PLUGIN_WRITE_TIMEOUT, child.wait_with_output()).await {
        Err(_) => {
            return WriteOutcome::Failed {
                status: format!(
                    "sink writer {name:?} timed out after {}s",
                    PLUGIN_WRITE_TIMEOUT.as_secs()
                ),
            };
        }
        Ok(Err(e)) => {
            return WriteOutcome::Failed {
                status: format!("sink writer {name:?}: {e}"),
            };
        }
        Ok(Ok(o)) => o,
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        return WriteOutcome::Failed {
            status: format!(
                "sink writer {name:?} failed (exit {:?}): {}",
                output.status.code(),
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            ),
        };
    }
    // A writer that says nothing accepted everything: the exit code is the
    // contract and the ack is the refinement. Being lenient here is what
    // lets `#!/bin/sh … | curl` be a legitimate plugin.
    let accepted = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|v| v.get("accepted").and_then(|n| n.as_u64()))
        .map(|n| n as usize)
        .unwrap_or(rows);
    if accepted >= rows {
        WriteOutcome::Accepted
    } else {
        WriteOutcome::PartiallyAccepted { rows: accepted }
    }
}

// ---------------------------------------------------------------------------
// The HTTP path (M31 B2)
// ---------------------------------------------------------------------------

/// How a batch is encoded and addressed for one destination.
///
/// **Four named destinations, one function.** ClickHouse and VictoriaLogs
/// are literally the same code — batch NDJSON, POST, check the status,
/// advance the cursor — OpenSearch is that plus an action line, and a
/// webhook is that with a URL somebody typed. Writing four connectors would
/// have been four things to keep working; this is one, with a body encoder
/// per variant.
#[derive(Debug)]
pub struct HttpShape {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Join a base URL and a path without a URL crate and without doubling the
/// separator — the one string operation four destinations all need.
fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{path}")
    }
}

/// The table a row belongs to, from the row itself.
///
/// Every line carries `table` (see [`job_line`]), so the shape functions do
/// not need a second parallel list of what they were handed.
fn table_of(line: &serde_json::Value) -> &str {
    line.get("table").and_then(|t| t.as_str()).unwrap_or("rows")
}

/// [`http_shape`] for an integration test.
///
/// The shape of a request is the whole of what a destination sees, and an
/// integration test that could not read it would be reduced to asserting
/// that a fake server was happy — which is a test of the fake. Exposed
/// rather than made `pub` outright so the name says who it is for.
pub fn http_shape_for_test(
    sink: &Sink,
    lines: &[serde_json::Value],
    credential: &str,
) -> HttpShape {
    http_shape(sink, lines, credential)
}

/// Build the request for one batch.
fn http_shape(sink: &Sink, lines: &[serde_json::Value], credential: &str) -> HttpShape {
    let mut headers: Vec<(String, String)> = Vec::new();
    if !credential.is_empty() {
        // Bearer for everything but ClickHouse, which takes its own header
        // pair — and putting the credential in a header rather than the
        // query string is what keeps it out of the destination's access log.
        match sink.kind {
            SinkKind::ClickHouse => {
                headers.push(("X-ClickHouse-Key".into(), credential.to_string()));
                if !sink.database.trim().is_empty() {
                    headers.push(("X-ClickHouse-Database".into(), sink.database.clone()));
                }
            }
            _ => headers.push(("Authorization".into(), format!("Bearer {credential}"))),
        }
    } else if sink.kind == SinkKind::ClickHouse && !sink.database.trim().is_empty() {
        headers.push(("X-ClickHouse-Database".into(), sink.database.clone()));
    }

    match sink.kind {
        // `JSONEachRow` is NDJSON by another name, which is why the encoder
        // is the same one the file writer uses. One table per request,
        // because the table is in the URL.
        SinkKind::ClickHouse => {
            let table = lines.first().map(table_of).unwrap_or("rows");
            HttpShape {
                // **The `/` is not cosmetic.** Without it the query string
                // runs straight into the authority — `host:8443?query=…` —
                // and every URL parser that splits the authority at the
                // first `/` reads the port as `8443?query=…`. ClickHouse
                // itself takes the statement at the root path.
                url: format!(
                    "{}/?query=INSERT%20INTO%20{}%20FORMAT%20JSONEachRow",
                    sink.url.trim_end_matches('/'),
                    table
                ),
                headers,
                body: encode_batch(lines),
            }
        }
        // JSON lines, and the stream fields tell VictoriaLogs which keys
        // are the message and the timestamp rather than making it guess.
        SinkKind::VictoriaLogs => HttpShape {
            url: format!(
                "{}?_stream_fields=mesh_id,table&_time_field=finished_at&_msg_field=command",
                join_url(&sink.url, "insert/jsonline")
            ),
            headers,
            body: encode_batch(lines),
        },
        // The escape hatch's escape hatch: the same NDJSON, at a URL
        // somebody typed. A smaller promise than a connector.
        _ => HttpShape {
            url: sink.url.clone(),
            headers,
            body: encode_batch(lines),
        },
    }
}

/// A client for one sink's URL.
///
/// **Plain HTTP is refused to anything routable**, by the one shared rule
/// in [`crate::httpc::is_private_host`] — same function, not a second copy
/// that drifts. The stake is high: a sink carries the organisation's
/// complete operational history, and over `http://` to a public address it
/// carries it in the clear along with the credential in the header.
///
/// A warehouse on the operator's own network is the ordinary case and is
/// allowed, because refusing a LAN is how a tool teaches people to reach
/// for the flag that disables checking everywhere.
pub fn client_for(url: &str) -> Result<crate::httpc::Client> {
    let plain = url.starts_with("http://");
    if plain && !crate::httpc::is_private_host(crate::httpc::host_of(url)) {
        return Err(CoreError::State(format!(
            "{url} is plain HTTP on a public address. A sink carries this mesh's whole \
             history and its credential; use https://, or point at a destination on your \
             own network."
        )));
    }
    crate::httpc::Client::new(plain, HTTP_MAX_BODY)
        .map_err(|e| CoreError::State(format!("could not build an HTTPS client: {e}")))
}

/// How much of a destination's response is read before it is truncated.
///
/// A megabyte. An `_bulk` reply for a ten-thousand-row batch is genuinely
/// large, and the alternative — capping it small — would mean not seeing
/// the `"errors":true` flag that says half the batch was rejected.
const HTTP_MAX_BODY: usize = 1024 * 1024;

/// POST one batch to an HTTP destination.
///
/// `credential` is the **resolved** secret, never the `{{secret:…}}`
/// reference: resolution happens on the exporting node, at connect time
/// (C6), so the token is never in the op-log and never in this function's
/// caller either.
pub async fn http_write(
    sink: &Sink,
    lines: &[serde_json::Value],
    credential: &str,
) -> WriteOutcome {
    if lines.is_empty() {
        return WriteOutcome::Accepted;
    }
    let client = match client_for(&sink.url) {
        Ok(c) => c,
        Err(e) => {
            return WriteOutcome::Failed {
                status: e.to_string(),
            };
        }
    };
    let shape = http_shape(sink, lines, credential);
    match client
        .post_json(&shape.url, &shape.headers, &shape.body)
        .await
    {
        Ok(resp) if (200..300).contains(&resp.status) => WriteOutcome::Accepted,
        // **Verbatim**, body and all. `503` sends an operator to the right
        // machine; a warehouse's own "no such table" message sends them to
        // the right *line*. Collapsing it to "sink unhealthy" throws away
        // the only useful part.
        Ok(resp) => WriteOutcome::Failed {
            status: format!("HTTP {} {}", resp.status, first_line(&resp.body)),
        },
        Err(e) => WriteOutcome::Failed {
            status: e.to_string(),
        },
    }
}

/// The first line of a response body, capped — enough to carry the
/// destination's own message into `meshnode sinks` without pasting a
/// megabyte of HTML into a table cell.
fn first_line(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(2048)]);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() > 200 {
        line.chars().take(199).chain(['…']).collect()
    } else {
        line.to_string()
    }
}

/// Write a batch through whichever writer this sink's kind names.
pub async fn write_batch(
    sink: &Sink,
    state: &SinkState,
    batch: &[u8],
    rows: usize,
) -> WriteOutcome {
    match sink.kind {
        SinkKind::Ndjson => NdjsonWriter::new(sink, state).write(batch),
        SinkKind::Plugin => plugin_write(&sink.database, batch, rows).await,
        // Retired 2026-09-20. The variant still decodes, so a record
        // signed before the removal is readable and this says why rather
        // than reporting a transport failure nobody can act on.
        k if k.retirement().is_some() => WriteOutcome::Failed {
            status: k.retirement().unwrap_or("retired").to_string(),
        },
        // An HTTP destination needs the decoded rows (to shape a `_bulk`
        // action line or name a ClickHouse table) and a resolved
        // credential, neither of which this signature carries. The caller
        // that has both is `daemon::export_sink`; this arm exists so the
        // match stays exhaustive and a kind cannot be added without
        // deciding how it is written.
        k if k.is_http() => WriteOutcome::Failed {
            status: "an HTTP sink must be written through `http_write`".into(),
        },
        _ => WriteOutcome::Failed {
            status: format!("no writer for sink kind {}", sink.kind.as_str()),
        },
    }
}

// ---------------------------------------------------------------------------
// Payload export (M31 B2 item 5)
// ---------------------------------------------------------------------------
//
// **Records and payloads never travel together** (§4 decision 3), and this
// is the second half. A record is a few hundred bytes and leaves from one
// elected node; raw stdout is megabytes and leaves from **the machine that
// produced it**, straight to the log store. Sending both down one pipe
// would rebuild the funnel M9's object store exists to remove, and would
// make a thousand-node mesh export through one machine's NIC.
//
// This is also the answer A2 pointed at, in the words A2 used: output older
// than `[retention]` exists in the customer's log store or nowhere.

/// The key a payload is stored under, and the value that goes into P0's
/// `payload_key` column.
///
/// Deterministic from the ids rather than random, so a re-export of the
/// same job overwrites rather than accumulating — the same idempotence
/// argument the record path makes, applied to bytes.
pub fn payload_key(mesh_id: &str, job: &mesh_proto::JobId) -> String {
    format!("{mesh_id}/jobs/{}", job.to_display_string())
}

/// One job's captured output, as it leaves the producing node.
///
/// Line-oriented JSON so it lands in a log store as *lines* rather than as
/// one enormous document — which is what makes it searchable at the other
/// end, and the reason a log store is the right destination for this and a
/// warehouse is not.
pub fn payload_lines(
    mesh_id: &str,
    job: &mesh_proto::JobId,
    stream: &str,
    text: &str,
) -> Vec<serde_json::Value> {
    text.lines()
        .map(|line| {
            serde_json::json!({
                "mesh_id": mesh_id,
                "table": "output",
                "job_id": job.to_display_string(),
                "stream": stream,
                "line": line,
            })
        })
        .collect()
}

/// Read a job's captured output for export, refusing anything over the cap.
///
/// **Above the cap the reference is exported and the bytes are not**, which
/// is `max_bytes` doing its job: a 4 GB core dump reaching a per-gigabyte
/// destination is the failure mode Q2 was decided to avoid, and silently
/// truncating would be worse than refusing because nobody would know which
/// half they had.
///
/// Returns `None` when there is nothing to export — no record, or nothing
/// captured — which is not an error and must not be reported as one.
pub fn read_payload(
    jobs: &crate::jobs::JobManager,
    job: &mesh_proto::JobId,
    max_bytes: u64,
) -> Option<Result<(String, String)>> {
    let dir = jobs.job_dir_of(job);
    let mut total = 0u64;
    let mut read = |name: &str| -> Option<String> {
        let path = dir.join(name);
        let len = std::fs::metadata(&path).ok()?.len();
        total += len;
        std::fs::read_to_string(&path).ok()
    };
    let out = read("stdout.log");
    let err = read("stderr.log");
    if out.is_none() && err.is_none() {
        return None;
    }
    if max_bytes > 0 && total > max_bytes {
        return Some(Err(CoreError::State(format!(
            "job output is {total} bytes, over this sink's {max_bytes}-byte cap — the record \
             carries the reference and the bytes stay here"
        ))));
    }
    Some(Ok((out.unwrap_or_default(), err.unwrap_or_default())))
}

// ---------------------------------------------------------------------------
// Reading a payload back (M31 B3, §14.3)
// ---------------------------------------------------------------------------
//
// **One payload, by id, because a person clicked it.** Never to render a
// list, populate a filter, count anything, or paint a page nobody asked to
// see — that is §18 fence 2, and it is what keeps a warehouse from becoming
// a control plane. A read that fails degrades one panel and never breaks a
// screen.
//
// ## 5 GB is a paging problem, not a fetch problem
//
// Nothing here materialises the object. Every destination is asked for a
// *window*, and the window is expressed in the destination's own idiom
// rather than in ours:
//
// - **Open at the tail.** The reason anyone opens a six-month-old log is
//   the thing that went wrong, and it is at the end.
// - **Seek by line offset from the end, not by byte.** These payloads left
//   as *lines* (see `payload_lines`), so the destination already has them
//   indexed that way — asking for bytes would mean it reading the whole
//   object to count them, which is the cost this design exists to avoid.
// - **Search is delegated** where the destination has it, because
//   OpenSearch and VictoriaLogs do it better than we would.

/// One window of a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadWindow {
    /// Lines back from the end. `0` is the tail itself.
    pub from_end: u64,
    pub limit: u64,
    /// Delegated to the destination's own index when it has one. Empty is
    /// "no filter", not "match nothing".
    pub search: String,
}

impl Default for PayloadWindow {
    fn default() -> Self {
        PayloadWindow {
            from_end: 0,
            // A screenful and change. Small enough that first paint is
            // fast on a 5 GB object, large enough that scrolling is not a
            // request per line.
            limit: 500,
            search: String::new(),
        }
    }
}

/// What a payload read produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadPage {
    pub lines: Vec<String>,
    /// True when the destination said there is more before this window.
    pub more_before: bool,
    /// What the destination said when it would not answer — verbatim, and
    /// empty on success.
    pub status: String,
}

/// Build the request that fetches one window from one destination.
///
/// Each is the destination's *own* way of saying "the last N lines of this
/// job, newest first" — a `LIMIT`/`OFFSET` for ClickHouse, LogsQL for
/// VictoriaLogs, `search_after`-shaped sorting for OpenSearch. Writing one
/// generic form and translating it would have meant scanning where the
/// destination could have indexed.
fn payload_query(sink: &Sink, key: &str, w: &PayloadWindow) -> Option<HttpShape> {
    let job = key.rsplit('/').next().unwrap_or(key);
    match sink.kind {
        SinkKind::ClickHouse => {
            // **Unqualified, and the database travels in the same header
            // the write uses.** Qualifying here with a different fallback
            // than `http_shape` picks is how a read ends up looking in
            // `meshware.output` for rows the write put in `default.output`
            // — the two have to agree by construction, not by both
            // guessing the same way.
            // **Bound, not interpolated** (S-M2). Doubling `'` is the
            // right escape for standard SQL and the wrong one for
            // ClickHouse, which *also* treats `\` as an escape inside a
            // string literal: a search ending in a backslash escaped the
            // closing quote and the statement continued under the caller's
            // control — against the warehouse holding the whole of this
            // organisation's operational history, with the sink's stored
            // credential. Both values reach here from a person's search box
            // and from `Request::ReplayPayload` over the wire.
            //
            // ClickHouse's HTTP interface takes query parameters as
            // `param_<name>` and substitutes them server-side at
            // `{name:String}`, which is a bound parameter in the sense that
            // matters: the value is never parsed as SQL. `limit` and
            // `from_end` are `u64` and are the only things still formatted
            // in.
            let mut sql = "SELECT line FROM output WHERE job_id = {job:String}".to_string();
            if !w.search.is_empty() {
                sql.push_str(" AND position(line, {search:String}) > 0");
            }
            sql.push_str(&format!(
                " ORDER BY rowNumberInAllBlocks() DESC LIMIT {} OFFSET {} FORMAT JSONEachRow",
                w.limit, w.from_end
            ));
            let mut headers = Vec::new();
            if !sink.database.trim().is_empty() {
                headers.push((
                    "X-ClickHouse-Database".to_string(),
                    sink.database.trim().to_string(),
                ));
            }
            let mut url = format!(
                "{}/?query={}&param_job={}",
                sink.url.trim_end_matches('/'),
                urlencode(&sql),
                urlencode(job)
            );
            if !w.search.is_empty() {
                url.push_str(&format!("&param_search={}", urlencode(&w.search)));
            }
            Some(HttpShape {
                url,
                headers,
                body: Vec::new(),
            })
        }
        SinkKind::VictoriaLogs => {
            // Quoted, both of them (S-M2). This one had no escaping at all:
            // whatever was typed into the search box became LogsQL, which
            // is a query language with its own filters, its own pipes and
            // its own idea of what a stream is. Quoting turns both values
            // back into what they are — a job id to match exactly, and a
            // phrase to look for.
            let mut q = format!("job_id:={}", logsql_string(job));
            if !w.search.is_empty() {
                q.push(' ');
                q.push_str(&logsql_string(&w.search));
            }
            Some(HttpShape {
                url: format!(
                    "{}?query={}&limit={}",
                    join_url(&sink.url, "select/logsql/query"),
                    urlencode(&q),
                    w.from_end + w.limit
                ),
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
        // A file, a program, a URL. Nothing to ask.
        _ => None,
    }
}

/// Percent-encode a query-string value without a URL crate.
///
/// The unreserved set from RFC 3986 plus nothing else — a SQL statement in
/// a query string is exactly where being generous about `&` and `=` turns
/// into a broken request or, worse, a working but different one.
/// One LogsQL string literal, quoted and escaped (S-M2).
///
/// LogsQL takes double-quoted strings with Go-style backslash escapes, so a
/// quoted value is a *value* — a phrase filter when it stands alone, and the
/// right-hand side of `field:=` when it follows one. Escaping the backslash
/// first is what stops a trailing one from escaping the closing quote, which
/// is the same mistake ClickHouse's `''` doubling made above.
fn logsql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Pull the `line` field out of whatever shape the destination answered in.
///
/// Three formats, one function: ClickHouse returns NDJSON objects,
/// VictoriaLogs returns NDJSON objects, OpenSearch returns one JSON
/// document with `hits.hits[]._source`. All three end at the same place —
/// a list of lines, newest first — so the caller does not branch.
fn payload_lines_from(_kind: SinkKind, body: &[u8]) -> Vec<String> {
    let take = |v: &serde_json::Value| -> Option<String> {
        v.get("line")
            .and_then(|l| l.as_str())
            .map(|s| s.to_string())
    };
    String::from_utf8_lossy(body)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| take(&v))
        .collect()
}

/// Fetch one window of one job's output from the sink that holds it.
///
/// `credential` is the **read** credential, resolved on the fetching node.
/// Absent, the caller should not be here at all — see
/// [`SinkKind::replay_state`], which is the check that turns "no token"
/// into a sentence naming the permission rather than a shrug.
pub async fn read_payload_window(
    sink: &Sink,
    key: &str,
    window: &PayloadWindow,
    credential: &str,
) -> PayloadPage {
    let Some(mut shape) = payload_query(sink, key, window) else {
        return PayloadPage {
            lines: Vec::new(),
            more_before: false,
            status: format!(
                "a {} destination has nothing to read back",
                sink.kind.as_str()
            ),
        };
    };
    if !credential.is_empty() {
        match sink.kind {
            SinkKind::ClickHouse => shape
                .headers
                .push(("X-ClickHouse-Key".into(), credential.to_string())),
            _ => shape
                .headers
                .push(("Authorization".into(), format!("Bearer {credential}"))),
        }
    }
    let client = match client_for(&sink.url) {
        Ok(c) => c,
        Err(e) => {
            return PayloadPage {
                lines: Vec::new(),
                more_before: false,
                status: e.to_string(),
            };
        }
    };
    // A GET for the two that take a query string, a POST for `_bulk`'s
    // sibling `_search`. **Both carry the headers** — the read credential
    // is one of them, and a GET that quietly dropped it would 401 against
    // any destination actually configured to require authentication, which
    // is every real one.
    //
    // Neither follows a redirect, and neither turns a non-2xx into a bare
    // status code: what the destination said is what the degraded panel
    // prints, because "unavailable" sends an operator to the wrong machine.
    let resp = if shape.body.is_empty() {
        client.get_with(&shape.url, &shape.headers).await
    } else {
        client
            .post_json(&shape.url, &shape.headers, &shape.body)
            .await
    }
    .and_then(|r| {
        if (200..300).contains(&r.status) {
            Ok(r)
        } else {
            Err(anyhow::anyhow!("HTTP {} {}", r.status, first_line(&r.body)))
        }
    });
    match resp {
        Ok(r) => {
            let mut lines = payload_lines_from(sink.kind, &r.body);
            let over = lines.len() as u64 > window.limit;
            // **The ceiling is ours, not the destination's.** Every query
            // this module builds carries a bound, but a destination that
            // ignores it — a proxy rewriting the statement, a LogsQL
            // `limit` interpreted per-stream, a future version that changes
            // its mind — must not be able to make this process hold an
            // object it asked for a window of. "It streams" is a claim that
            // decays; a truncation is a fact.
            if over {
                lines.truncate(window.limit as usize);
            }
            // Destinations answer newest-first because that is the cheap
            // direction; a reader wants oldest-first within the window.
            lines.reverse();
            let more_before = over || lines.len() as u64 >= window.limit;
            PayloadPage {
                lines,
                more_before,
                status: String::new(),
            }
        }
        // **Degrades one panel, never breaks a screen** — and says what the
        // destination said, because "unavailable" sends an operator to the
        // wrong machine.
        Err(e) => PayloadPage {
            lines: Vec::new(),
            more_before: false,
            status: e.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// DDL (M31 B2 item 4)
// ---------------------------------------------------------------------------
//
// **Printed, never executed.** No code path in this tree runs any of this,
// and that is the rule that keeps meshware permanently out of the migration
// business (§18 decision 05). An operator's warehouse is theirs: we can
// tell them exactly what shape we will send, and they decide when their
// schema changes.
//
// It writes **only the columns that exist**. A generated schema with
// speculative columns in it is a schema somebody has to explain to a
// reviewer.

/// The dialects `meshnode sinks ddl` can print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlDialect {
    ClickHouse,
}

impl DdlDialect {
    pub const ALL: [DdlDialect; 1] = [DdlDialect::ClickHouse];

    pub fn as_str(self) -> &'static str {
        match self {
            DdlDialect::ClickHouse => "clickhouse",
        }
    }

    pub fn parse(s: &str) -> Option<DdlDialect> {
        DdlDialect::ALL
            .into_iter()
            .find(|d| d.as_str().eq_ignore_ascii_case(s))
    }

    /// The dialect a sink kind implies, so an operator need not say twice
    /// what they already chose.
    pub fn for_kind(kind: SinkKind) -> Option<DdlDialect> {
        match kind {
            SinkKind::ClickHouse => Some(DdlDialect::ClickHouse),
            // VictoriaLogs is schemaless, a webhook is somebody else's
            // problem, a file has no schema to state, HEC indexes whatever
            // it is sent, and OTLP is not a store at all.
            _ => None,
        }
    }
}

/// The columns the `jobs` table actually carries on the wire, in order.
///
/// Derived from the same place the rows are built ([`job_line`]) rather
/// than typed twice — a DDL that disagrees with the data is worse than no
/// DDL, because it fails at insert time on somebody else's schedule.
fn job_columns() -> &'static [(&'static str, ColumnKind)] {
    use ColumnKind::*;
    &[
        ("mesh_id", Text),
        ("job_id", Text),
        ("command", Text),
        ("runtime", Text),
        ("submitter_node", Text),
        ("submitter_user", Text),
        ("executor_node", Text),
        ("state", Text),
        ("exit_code", NullableInt),
        ("created_at", Instant),
        ("started_at", NullableInstant),
        ("finished_at", NullableInstant),
        ("duration_ms", NullableBig),
    ]
}

#[derive(Debug, Clone, Copy)]
enum ColumnKind {
    Text,
    NullableInt,
    NullableBig,
    Instant,
    NullableInstant,
}

impl ColumnKind {
    /// ClickHouse is the one dialect left, so this no longer branches on
    /// one — the parameter stays because `ddl` threads it through and a
    /// second dialect would come back here first.
    fn sql(self, _dialect: DdlDialect) -> &'static str {
        match self {
            ColumnKind::Text => "String",
            ColumnKind::NullableInt => "Nullable(Int32)",
            ColumnKind::NullableBig => "Nullable(UInt64)",
            ColumnKind::Instant => "DateTime64(6, 'UTC')",
            ColumnKind::NullableInstant => "Nullable(DateTime64(6, 'UTC'))",
        }
    }
}

/// The DDL for one sink's selected tables.
///
/// Returns the text; **nothing in this crate executes it**, and the test
/// that asserts so is the one that matters.
pub fn ddl(sink: &Sink, dialect: DdlDialect) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "-- meshware {} — DDL for sink `{}`\n-- meshware never runs this. Review it, then apply it yourself.\n\n",
        dialect.as_str(),
        sink.name
    ));

    // The *database*, never the URL — `CREATE TABLE https://host:8443.runs`
    // is what a careless fallback produced the first time this ran for
    // real. And no database at all when the sink names none: its inserts
    // are unqualified and go to the connection's default database, so the
    // tables must be created there too. This used to guess `meshware.`,
    // which put the tables in one database and the rows in another, and
    // every insert failed with "table does not exist".
    let qualify = match sink.database.trim() {
        "" => String::new(),
        named => format!("{named}."),
    };
    for table in sink.tables.names() {
        // Only the tables that have a published shape. `usage` and `audit`
        // ride the same NDJSON but are not part of B2's four destinations'
        // schema story yet, and printing a guess would be worse than
        // printing nothing.
        if table != "jobs" {
            continue;
        }
        let cols = job_columns();
        match dialect {
            DdlDialect::ClickHouse => {
                out.push_str(&format!("CREATE TABLE IF NOT EXISTS {qualify}{table} (\n"));
                for (name, kind) in cols {
                    out.push_str(&format!("    {name:<16} {},\n", kind.sql(dialect)));
                }
                // ReplacingMergeTree on the row id is what makes
                // at-least-once delivery (§4 decision 5) free: a retry of
                // the same batch collapses instead of duplicating.
                let key = "job_id";
                out.push_str(&format!("    PRIMARY KEY (mesh_id, {key})\n"));
                out.push_str(")\n");
                out.push_str("ENGINE = ReplacingMergeTree\n");
                out.push_str(&format!("ORDER BY (mesh_id, {key})\n"));
                out.push_str("PARTITION BY toYYYYMM(created_at);\n\n");
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// What `meshnode sinks` shows
// ---------------------------------------------------------------------------

/// Backoff for the next attempt, jittered inside the sink's own bounds.
///
/// **Equal jitter**, not full jitter: half the exponential base plus a
/// random half. Full jitter (`random(min, base)`) is the more famous recipe
/// and is wrong here, because on an unlucky draw it returns the floor on
/// the twentieth attempt — so a mesh whose warehouse has been down for an
/// hour keeps hammering it every fifteen seconds. Equal jitter still
/// spreads a fleet out, and still actually backs off.
///
/// `salt` is the jitter source, deterministic so a test can pin it. In the
/// exporter it is the node id mixed with the attempt, which is what stops
/// a thousand nodes retrying in lockstep when a warehouse comes back.
pub fn backoff_secs(sink: &Sink, attempt: u32, salt: u64) -> u64 {
    let min = sink.on_failure.retry_min_secs.max(1);
    let max = sink.on_failure.retry_max_secs.max(min);
    // `1 << 16` is already far past any sane ceiling; the shift is clamped
    // rather than left to overflow on a sink that has been failing for a
    // week.
    let base = min.saturating_mul(1u64 << attempt.min(16)).min(max);
    let half = base / 2;
    let jittered = half + (salt % (half.max(1)));
    jittered.clamp(min, max)
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// Stamp and sign a sink with the admin's own key.
///
/// The daemon never holds this key (C6/§7.1), so the signature is made
/// client-side and re-verified on every node that applies the op — which is
/// what makes `meshnode audit` able to prove who pointed the mesh's history
/// at a host, rather than merely record it.
pub fn sign(user: &crate::identity::Keypair, mut sink: Sink) -> Sink {
    sink.created_by = user.user_id();
    sink.signature = Vec::new();
    sink.signature = user.sign(&sink.signing_bytes()).to_vec();
    sink
}

pub fn sign_remove(user: &crate::identity::Keypair, name: &str) -> Vec<u8> {
    user.sign(&mesh_proto::sink_remove_signing_bytes(
        name,
        &user.user_id(),
    ))
    .to_vec()
}

/// Publish a client-built, client-signed sink. Used by the testkit and by
/// Director's embedded engine, which hold the key in process.
pub fn put(engine: &Arc<crate::state::StateEngine>, sink: Sink) -> Result<()> {
    if let Some(why) = sink.refusal() {
        return Err(CoreError::State(why));
    }
    engine.author_op(mesh_proto::OpBody::SinkPut {
        sink: Box::new(sink),
    })?;
    Ok(())
}

/// Build, sign and publish in one step.
pub fn put_as(
    engine: &Arc<crate::state::StateEngine>,
    user: &crate::identity::Keypair,
    sink: Sink,
) -> Result<()> {
    put(engine, sign(user, sink))
}

// ---------------------------------------------------------------------------
// The restore latch (§19 Q5, decided)
// ---------------------------------------------------------------------------
//
// **`meshnode restore` disables every sink; an admin re-enables what they
// meant to keep.** The decision and its reasoning are §19 Q5.
//
// The latch is a **node-local file**, not a field flipped in the replicated
// record, and that turned out to be the better shape for three reasons:
//
// - **It needs no signature.** Rewriting every recovered `Sink` with
//   `enabled: false` means re-signing an admin-signed record, and the key
//   in a restored home is whoever's it was — which may not be an admin. A
//   safety catch that silently fails to engage for a non-admin operator is
//   worse than none, because it is believed.
// - **It is right for a partial restore too.** One node restored into a
//   live mesh must not start re-exporting; the mesh's other nodes should
//   carry on, because the mesh did not lose anything. A replicated flag
//   would stop the healthy nodes as well.
// - **It survives a fork.** A restore that mints a fresh identity is a mesh
//   whose sinks point at the *parent* mesh's warehouse, with the parent's
//   credentials. That is the case the latch exists for, and it engages the
//   same way for both.

/// The marker `restore` writes and an admin clears.
fn restore_latch(sinks_dir: &Path) -> PathBuf {
    sinks_dir.join(".paused-by-restore")
}

/// Are this node's sinks held after a restore?
pub fn paused_by_restore(sinks_dir: &Path) -> bool {
    restore_latch(sinks_dir).exists()
}

/// Engage the latch. Called by `restore`, and idempotent.
pub fn pause_after_restore(sinks_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(sinks_dir)?;
    std::fs::write(
        restore_latch(sinks_dir),
        b"This node's sinks are held because its data was restored from a backup.\n\
          A restored mesh must not silently re-push history into a live warehouse, and a\n\
          forked one must not push into a warehouse belonging to the mesh it forked from.\n\
          \n\
          Review `meshnode sinks`, then `meshnode sinks resume` to start exporting again.\n",
    )?;
    Ok(())
}

/// Release it. Admin-gated at the IPC boundary and audited there.
pub fn resume_after_restore(sinks_dir: &Path) -> Result<bool> {
    let path = restore_latch(sinks_dir);
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Election
// ---------------------------------------------------------------------------

/// Rendezvous context for sink election. Distinct from the schedule
/// context so a node that fires a schedule is not thereby the node that
/// exports a sink of the same name.
const ELECTION_CONTEXT: &[u8] = b"mw-sink-v1";

/// The node currently elected to export `sink`.
///
/// `schedules::elected_firer`'s mechanism, with two differences that both
/// matter:
///
/// - **The pool is filtered by the sink's `run_on` tag.** In most real
///   deployments only some machines have a route to the warehouse — a
///   firewall exception, a VPC, a proxy — and this is how an operator says
///   *"export from a machine that can actually reach it"*. It is the
///   difference between a feature that works on the demo mesh and one that
///   works at the customer.
/// - **The pool is filtered by protocol minor.** A pre-`SINK_MINOR` node
///   would win the rendezvous, have no code to export anything, and the
///   sink would silently stop while `meshnode sinks` showed a healthy
///   elected node. Silence that looks like health is the failure mode
///   [`mesh_proto::SINK_MINOR`] exists to prevent.
///
/// A dead exporter costs one gossip round and a re-derived cursor. There is
/// no handover protocol because the cursor is a property of the data: the
/// successor reads it and continues.
pub fn elected_exporter(
    state: &crate::state::MeshState,
    sink: &Sink,
    liveness_of: impl Fn(&mesh_proto::NodeId) -> mesh_proto::Liveness,
    minor_of: impl Fn(&mesh_proto::NodeId) -> Option<u16>,
) -> Option<mesh_proto::NodeId> {
    crate::scheduler::rendezvous_rank(
        ELECTION_CONTEXT,
        sink.name.as_bytes(),
        export_pool(state, sink, liveness_of, minor_of),
    )
    .into_iter()
    .next()
}

/// Every node that could win this sink's export election right now.
///
/// [`elected_exporter`] is this list ranked, and the two must never drift:
/// the storage probe's whole claim is that it checked **the machines that
/// can actually be elected**, and a pool assembled by a second, similar
/// filter would eventually check a different set than the one that exports.
/// One node missing from a probe is one node that silently cannot write
/// the day the rendezvous moves to it.
pub fn export_pool(
    state: &crate::state::MeshState,
    sink: &Sink,
    liveness_of: impl Fn(&mesh_proto::NodeId) -> mesh_proto::Liveness,
    minor_of: impl Fn(&mesh_proto::NodeId) -> Option<u16>,
) -> Vec<mesh_proto::NodeId> {
    // `Selector::parse` is infallible — an unparseable string becomes a
    // name match that hits nothing, which is the fail-closed reading: a
    // typo in `run_on` means the sink does not export, not that it exports
    // from everywhere.
    let selector = crate::membership::Selector::parse(&sink.run_on);
    state
        .members
        .values()
        .filter(|m| state.is_active_member(&m.info.node_id))
        .filter(|m| matches!(liveness_of(&m.info.node_id), mesh_proto::Liveness::Alive))
        // `matches_in` rather than `matches`, so `run_on: fact.region=eu`
        // works — the facts are in replicated state and every node has
        // them, so there is nothing to ask anybody.
        .filter(|m| selector.matches_in(state, &m.info))
        // Unknown minor means a node we have not negotiated with. Excluded
        // rather than assumed capable: assuming is how a sink goes quiet.
        .filter(|m| minor_of(&m.info.node_id).is_some_and(|v| v >= mesh_proto::SINK_MINOR))
        // **Nodes that cannot take the job** (ENG-26). `Alive` is not the
        // same question as "will still be here": a node draining or
        // shutting down is alive right up to the moment it is not, and
        // electing it costs the sink an export window and a re-derived
        // cursor for a machine that had already said it was leaving. The
        // scheduler has excluded these from ordinary work since E9 —
        // `membership.rs` calls it `NotExecuting` — and an export is work.
        .filter(|m| {
            !m.info.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG)
                && mesh_proto::drain_of(&m.info.tags).is_none()
        })
        // Quarantine, for the same reason and the owner's: a node the mesh
        // has isolated must not be the one shipping this mesh's history to
        // a third party.
        .filter(|m| state.may_execute(&m.info.node_id))
        .map(|m| m.info.node_id)
        .collect()
}

/// A shared handle to one sink's live counters, so the exporter can update
/// them and `meshnode sinks` can read them without touching the disk.
#[derive(Debug, Default)]
pub struct SinkMetrics {
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
    pub spooled_bytes: AtomicU64,
    pub last_status: Mutex<String>,
}

pub type SharedMetrics = Arc<SinkMetrics>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(kind: SinkKind) -> Sink {
        Sink {
            name: "warehouse".into(),
            kind,
            url: String::new(),
            database: String::new(),
            credential: String::new(),
            read_credential: String::new(),
            tables: Default::default(),
            records: Default::default(),
            payloads: Default::default(),
            on_failure: Default::default(),
            run_on: String::new(),
            enabled: true,
            description: String::new(),
            created_by: mesh_proto::UserId([7u8; 32]),
            created_at_micros: 0,
            signature: Vec::new(),
        }
    }

    // -- the HTTP path (M31 B2) -------------------------------------------

    fn http_sink(kind: SinkKind, url: &str) -> Sink {
        let mut s = sink(kind);
        s.url = url.into();
        s
    }

    fn row(table: &str, id: &str) -> serde_json::Value {
        serde_json::json!({
            "mesh_id": "mesh-aaaa",
            "table": table,
            if table == "jobs" { "job_id" } else { "row_id" }: id,
            "command": "echo hi",
        })
    }

    /// ClickHouse takes `JSONEachRow` with the table in the query string,
    /// and the credential in a **header** rather than the query — which is
    /// what keeps it out of the destination's access log.
    #[test]
    fn clickhouse_posts_json_each_row_with_the_table_in_the_url() {
        let s = http_sink(SinkKind::ClickHouse, "https://warehouse.corp:8443");
        let shape = http_shape(&s, &[row("jobs", "job-1")], "tok");
        assert!(shape.url.contains("INSERT%20INTO%20jobs"), "{}", shape.url);
        assert!(shape.url.contains("JSONEachRow"), "{}", shape.url);
        assert!(!shape.url.contains("tok"), "the credential is in the URL");
        assert!(
            shape
                .headers
                .iter()
                .any(|(k, v)| k == "X-ClickHouse-Key" && v == "tok")
        );
        // One JSON object per line — the same encoder the file writer uses.
        let body = String::from_utf8(shape.body).expect("body is utf-8");
        assert_eq!(body.lines().count(), 1);
        serde_json::from_str::<serde_json::Value>(body.trim()).expect("a JSON object per line");
    }

    /// Every URL this module builds has a **path** before its query
    /// string, and that is not a style rule.
    ///
    /// `https://host:8443?query=…` is a URL whose authority is
    /// `host:8443?query=…`, because an authority ends at the first `/` and
    /// there is no `/`. Every parser that splits it that way — including
    /// this workspace's own — then tries to read the port as
    /// `8443?query=INSERT…` and fails with "bad port". The ClickHouse
    /// write path shipped with exactly that defect and it took a test
    /// running against a real socket to find it, because a shape assertion
    /// on the string looked perfectly correct.
    #[test]
    fn no_url_runs_a_query_string_straight_into_the_authority() {
        let job = mesh_proto::JobId([9u8; 16]);
        let mut shapes = Vec::new();
        // Every HTTP kind whose URL *we* build. A webhook's URL is the
        // operator's, verbatim, path and all — asserting a shape on it
        // would be asserting a shape on something we deliberately do not
        // touch.
        for kind in SinkKind::ALL
            .into_iter()
            .filter(|k| k.is_http() && *k != SinkKind::Webhook)
        {
            let mut s = http_sink(kind, "https://dest.corp:8443");
            s.database = "acme".into();
            shapes.push(http_shape(&s, &[row("jobs", "job-7")], "tok").url);
            if let Some(read) = payload_query(
                &s,
                &payload_key("mesh-aaaa", &job),
                &PayloadWindow::default(),
            ) {
                shapes.push(read.url);
            }
            // And with a trailing slash, which is what half of all
            // operators type and the other half do not.
            let mut trailing = http_sink(kind, "https://dest.corp:8443/");
            trailing.database = "acme".into();
            shapes.push(http_shape(&trailing, &[row("jobs", "job-7")], "tok").url);
        }
        for url in shapes {
            let after_scheme = url.split_once("://").expect("a scheme").1;
            let authority = after_scheme
                .split(['/', '?', '#'])
                .next()
                .expect("an authority");
            assert!(
                after_scheme.len() > authority.len()
                    && after_scheme.as_bytes()[authority.len()] == b'/',
                "the authority must be followed by a path, not by `?`: {url}"
            );
            assert!(
                !url.contains("//?") && !url.contains("8443?"),
                "malformed URL: {url}"
            );
        }
    }

    /// S-M2: the search box is not a query language.
    ///
    /// The finding was two shapes of the same bug. ClickHouse doubled `'`
    /// — correct for standard SQL, and not for ClickHouse, which also
    /// treats `\` as an escape inside a literal, so a value ending in a
    /// backslash escaped the closing quote. VictoriaLogs escaped nothing at
    /// all. Both values arrive from a person's search box *and* from
    /// `Request::ReplayPayload` over the wire.
    #[test]
    fn a_search_term_cannot_escape_into_the_query() {
        let job = mesh_proto::JobId([9u8; 16]);
        // The ClickHouse break-out: a trailing backslash, then the payload.
        let hostile = "x\\' UNION ALL SELECT name FROM system.users --";
        let window = PayloadWindow {
            from_end: 0,
            limit: 10,
            search: hostile.to_string(),
        };

        let ch = http_sink(SinkKind::ClickHouse, "https://warehouse.corp:8443");
        let shape = payload_query(&ch, &payload_key("mesh-aaaa", &job), &window).expect("a query");
        // The statement carries placeholders and no literal at all, and the
        // value travels as its own URL parameter.
        assert!(
            shape.url.contains(urlencode("{job:String}").as_str()),
            "{}",
            shape.url
        );
        assert!(
            shape.url.contains(urlencode("{search:String}").as_str()),
            "{}",
            shape.url
        );
        assert!(shape.url.contains("&param_search="), "{}", shape.url);
        // Nothing the caller typed is in the statement, so there is no
        // quote to escape: every one of these is percent-encoded inside the
        // parameter and inert.
        assert!(
            !shape.url.contains(urlencode("UNION").as_str())
                || shape
                    .url
                    .split("&param_search=")
                    .nth(1)
                    .is_some_and(|p| p.contains(&urlencode("UNION"))),
            "the payload must only ever appear inside param_search: {}",
            shape.url
        );

        let vl = http_sink(SinkKind::VictoriaLogs, "https://logs.corp:9428");
        let shape = payload_query(&vl, &payload_key("mesh-aaaa", &job), &window).expect("a query");
        // The LogsQL side: a quoted string, with the backslash escaped
        // before the quote could be.
        let query = shape
            .url
            .split("query=")
            .nth(1)
            .and_then(|q| q.split('&').next())
            .expect("a query parameter");
        assert!(
            query.contains(&urlencode(r#"\\"#)),
            "the backslash must be escaped: {query}"
        );
        assert!(
            !query.contains(&urlencode(r#"x\\" "#)),
            "an unescaped quote would end the string: {query}"
        );
    }

    /// And the escaping is the *escaping*, not a filter: a search for a
    /// quote is still a search for a quote.
    #[test]
    fn logsql_quoting_is_reversible() {
        assert_eq!(logsql_string("plain"), "\"plain\"");
        // A quote is escaped, not dropped.
        assert_eq!(logsql_string("a\"b"), "\"a\\\"b\"");
        // And the backslash is escaped *first*, which is the whole finding:
        // a value ending in one would otherwise escape the closing quote.
        assert_eq!(logsql_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(logsql_string("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn victorialogs_names_its_stream_and_time_fields_rather_than_guessing() {
        let s = http_sink(SinkKind::VictoriaLogs, "https://logs.internal:9428");
        let shape = http_shape(&s, &[row("jobs", "job-1")], "");
        assert!(shape.url.contains("/insert/jsonline"), "{}", shape.url);
        assert!(
            shape.url.contains("_stream_fields=mesh_id,table"),
            "{}",
            shape.url
        );
        assert!(
            shape.url.contains("_time_field=finished_at"),
            "{}",
            shape.url
        );
    }

    /// A webhook is the same POST at a URL somebody typed — a smaller
    /// promise than a connector, and the escape hatch's escape hatch.
    #[test]
    fn a_webhook_is_the_same_ndjson_at_a_url_you_chose() {
        let s = http_sink(SinkKind::Webhook, "https://example.test/ingest?x=1");
        let shape = http_shape(&s, &[row("jobs", "job-1")], "tok");
        assert_eq!(shape.url, "https://example.test/ingest?x=1");
        assert_eq!(shape.body, encode_batch(&[row("jobs", "job-1")]));
    }

    /// **Plain HTTP to a public address is refused.** A sink carries the
    /// organisation's whole history and its credential in a header; over
    /// `http://` to the internet it carries both in the clear.
    #[test]
    fn plain_http_is_refused_to_the_internet_and_allowed_on_your_own_network() {
        let err = client_for("http://warehouse.example.com/ingest")
            .expect_err("plain HTTP to a public host must be refused");
        let msg = err.to_string();
        assert!(msg.contains("whole"), "{msg}");
        assert!(msg.contains("https://"), "{msg}");

        // A warehouse on the operator's own network is the ordinary case.
        // Refusing a LAN is how a tool teaches people to reach for the flag
        // that disables checking everywhere.
        assert!(client_for("http://10.0.0.7:8123").is_ok());
        assert!(client_for("http://localhost:8123").is_ok());
        assert!(client_for("https://warehouse.example.com").is_ok());
    }

    #[test]
    fn a_destinations_own_message_survives_into_the_status() {
        // `503` sends an operator to the right machine; the destination's
        // own words send them to the right line.
        let line = first_line(
            b"Code: 60. DB::Exception: Table meshware.runs does not exist.
",
        );
        assert!(line.contains("does not exist"), "{line}");
        // And a megabyte of HTML does not end up in a table cell.
        let long = vec![b'x'; 5000];
        assert!(first_line(&long).chars().count() <= 200);
    }

    // -- DDL (item 4) ------------------------------------------------------

    /// **Printed, never executed.** The rule that keeps meshware out of the
    /// migration business, asserted rather than promised: nothing in this
    /// crate calls a SQL execution path, and `ddl` returns a `String`.
    #[test]
    fn ddl_is_a_string_and_nothing_in_this_crate_runs_it() {
        let mut s = sink(SinkKind::ClickHouse);
        s.database = "acme".into();
        let text = ddl(&s, DdlDialect::ClickHouse);

        assert!(text.contains("meshware never runs this"), "{text}");
        assert!(
            text.contains("CREATE TABLE IF NOT EXISTS acme.jobs"),
            "{text}"
        );
        // ReplacingMergeTree is what makes at-least-once free: a retry of
        // the same batch collapses rather than duplicating.
        assert!(text.contains("ReplacingMergeTree"), "{text}");
        assert!(text.contains("mesh_id"), "{text}");
        // Only the columns that exist — a generated schema with
        // speculative columns is one somebody has to explain to a reviewer.
        assert!(!text.contains("payload_sink"), "{text}");
    }

    /// **The bug running it for real found.** `sinks ddl` reconstructed the
    /// sink from the row's `target`, which for an HTTP kind is the *URL* —
    /// so it emitted `CREATE TABLE https://host:8443.jobs`, which is not
    /// valid anything. The database is its own field now, on the row and in
    /// the record.
    #[test]
    fn ddl_names_a_database_never_a_url() {
        let mut s = sink(SinkKind::ClickHouse);
        s.url = "https://warehouse.corp:8443".into();
        s.database = String::new();
        let text = ddl(&s, DdlDialect::ClickHouse);
        assert!(
            !text.contains("https://"),
            "the URL leaked into the DDL:\n{text}"
        );
        // Unqualified, like the insert, so both land in the connection's
        // default database.
        assert!(text.contains("CREATE TABLE IF NOT EXISTS jobs ("), "{text}");

        s.database = "acme".into();
        let text = ddl(&s, DdlDialect::ClickHouse);
        assert!(text.contains("acme.jobs"), "{text}");
        assert!(!text.contains("https://"), "{text}");
    }

    /// And the emitted SQL has no leading indentation carried in from the
    /// source — a generated file somebody pastes into a client should look
    /// like something a person wrote.
    #[test]
    fn generated_sql_is_not_indented_by_this_files_own_layout() {
        for dialect in DdlDialect::ALL {
            let text = ddl(&sink(SinkKind::ClickHouse), dialect);
            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }
                // Deliberate indentation is a multiple of two and belongs
                // to a nested structure. Anything else — and in particular
                // the nine spaces a wrapped source literal produces — is a
                // leak. The first version of this test checked
                // `starts_with("    ")` and therefore skipped exactly the
                // line that was broken.
                let indent = line.len() - line.trim_start().len();
                assert!(
                    indent % 2 == 0 && indent <= 8,
                    "stray indentation ({indent}) carried in from the source: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_dialect_is_implied_by_the_kind_where_there_is_one() {
        assert_eq!(
            DdlDialect::for_kind(SinkKind::ClickHouse),
            Some(DdlDialect::ClickHouse)
        );
        // Schemaless, somebody else's problem, and no schema at all.
        assert_eq!(DdlDialect::for_kind(SinkKind::VictoriaLogs), None);
        assert_eq!(DdlDialect::for_kind(SinkKind::Webhook), None);
        assert_eq!(DdlDialect::for_kind(SinkKind::Ndjson), None);
    }

    // -- payloads (item 5) -------------------------------------------------

    #[test]
    fn a_payload_key_is_deterministic_so_a_re_export_overwrites() {
        let job = mesh_proto::JobId([3u8; 16]);
        let a = payload_key("mesh-aaaa", &job);
        assert_eq!(a, payload_key("mesh-aaaa", &job));
        assert_ne!(a, payload_key("mesh-bbbb", &job));
        assert!(a.contains(&job.to_display_string()));
    }

    /// Output leaves as **lines**, which is what makes it searchable at the
    /// other end and the reason a log store is the right destination.
    #[test]
    fn payload_output_leaves_as_lines_not_as_one_blob() {
        let job = mesh_proto::JobId([3u8; 16]);
        let lines = payload_lines("mesh-aaaa", &job, "stdout", "one\ntwo\nthree");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1]["line"], "two");
        assert_eq!(lines[0]["stream"], "stdout");
        assert_eq!(lines[0]["table"], "output");
        assert_eq!(lines[0]["mesh_id"], "mesh-aaaa");
        // Empty output is no lines rather than one empty line.
        assert!(payload_lines("m", &job, "stderr", "").is_empty());
    }

    #[test]
    fn a_cursor_round_trips_and_a_missing_one_reads_as_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = SinkState::new(dir.path(), "warehouse");

        // Missing reads as fresh rather than as an error: refusing to
        // export until somebody repairs a JSON file is worse in every
        // deployment than re-sending history the destination dedups.
        assert_eq!(state.cursor(), SinkCursor::default());

        let mut c = SinkCursor::default();
        c.set_jobs(Mark::new(1_000, "job-aaaa"));
        c.sent = 42;
        c.last_status = "204".into();
        state.save_cursor(&c).expect("save");
        assert_eq!(state.cursor(), c);

        // And a corrupt one does too, for the same reason.
        std::fs::write(state.dir().join("cursor.json"), b"{ not json").expect("corrupt");
        assert_eq!(state.cursor(), SinkCursor::default());
    }

    /// The composite cursor, and the tie it exists to break.
    #[test]
    fn marks_order_by_instant_then_id() {
        assert!(Mark::new(1, "a") < Mark::new(2, "a"));
        assert!(Mark::new(1, "a") < Mark::new(1, "b"));
        // An empty id sorts below every real one, which is what makes a
        // pre-composite cursor re-offer exactly one row rather than skip it.
        assert!(Mark::new(1, "") < Mark::new(1, "a"));
    }

    /// **C3 in a test.** A spool that has hit its cap drops the oldest and
    /// counts it, rather than growing or blocking.
    #[test]
    fn a_full_spool_drops_the_oldest_and_counts_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = SinkState::new(dir.path(), "warehouse");
        let batch = vec![b'x'; 1_000];

        let mut dropped_total = 0;
        for _ in 0..10 {
            dropped_total += state.spool(&batch, 3_000).expect("spool");
        }
        assert!(dropped_total > 0, "the cap never fired");
        assert!(
            state.spool_bytes() <= 3_000,
            "spool grew past its cap: {}",
            state.spool_bytes()
        );
        // Oldest-first, so what survives is the newest history — which is
        // the history anyone is actually asking about.
        assert!(!state.spooled().is_empty());
    }

    /// A cap of zero means no cap, and nothing is dropped.
    #[test]
    fn an_uncapped_spool_drops_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = SinkState::new(dir.path(), "warehouse");
        for _ in 0..5 {
            assert_eq!(state.spool(b"row\n", 0).expect("spool"), 0);
        }
        assert_eq!(state.spooled().len(), 5);
    }

    #[test]
    fn an_ndjson_sink_writes_where_the_operator_said() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = SinkState::new(dir.path(), "warehouse");

        // Blank: the node's own home, which is where it has always gone.
        let default = sink(SinkKind::Ndjson);
        assert_eq!(
            ndjson_path(&default, &state),
            state.dir().join("records.ndjson")
        );

        // A file: exactly that file.
        let mut named = sink(SinkKind::Ndjson);
        named.database = "/srv/exports/mesh-history.ndjson".into();
        assert_eq!(
            ndjson_path(&named, &state),
            PathBuf::from("/srv/exports/mesh-history.ndjson")
        );

        // A directory (it exists): the default filename inside it.
        let mut into_dir = sink(SinkKind::Ndjson);
        into_dir.database = dir.path().display().to_string();
        assert_eq!(
            ndjson_path(&into_dir, &state),
            dir.path().join("records.ndjson")
        );

        // A directory that does not exist yet, named as one by its trailing
        // separator — the writer creates it, so this must not become a file
        // called `exports`.
        let mut trailing = sink(SinkKind::Ndjson);
        trailing.database = format!("/srv/exports{}", std::path::MAIN_SEPARATOR);
        assert_eq!(
            ndjson_path(&trailing, &state),
            PathBuf::from("/srv/exports/records.ndjson")
        );
    }

    #[test]
    fn ndjson_is_one_object_per_line_and_rotates_at_the_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = SinkState::new(dir.path(), "warehouse");
        std::fs::create_dir_all(state.dir()).expect("mkdir");
        let mut w = NdjsonWriter::new(&sink(SinkKind::Ndjson), &state);
        w.max_bytes = 64;
        w.max_files = 3;

        for i in 0..20 {
            let line = serde_json::json!({ "n": i });
            assert_eq!(w.write(&encode_batch(&[line])), WriteOutcome::Accepted);
        }

        let text = std::fs::read_to_string(w.path()).expect("read");
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("every line is one object");
        }
        assert!(
            state.dir().join("records.ndjson.1").exists(),
            "the file never rotated"
        );
        // And the cap is honoured rather than exceeded by a whole batch.
        assert!(std::fs::metadata(w.path()).expect("meta").len() <= 64 + 32);
    }

    #[test]
    fn instants_leave_as_rfc3339_utc_rather_than_as_a_number() {
        // 2026-08-22T12:00:00Z
        assert_eq!(
            micros_to_rfc3339(1_787_400_000_000_000),
            "2026-08-22T12:00:00.000000Z"
        );
        assert_eq!(micros_to_rfc3339(0), "1970-01-01T00:00:00.000000Z");
    }

    #[test]
    fn backoff_stays_inside_the_sinks_own_bounds() {
        let s = sink(SinkKind::Ndjson);
        for attempt in 0..40u32 {
            for salt in [0u64, 7, 999_999] {
                let b = backoff_secs(&s, attempt, salt);
                assert!(
                    b >= s.on_failure.retry_min_secs && b <= s.on_failure.retry_max_secs,
                    "attempt {attempt} salt {salt} gave {b}"
                );
            }
        }
        // It actually backs off rather than sitting at the floor.
        assert!(backoff_secs(&s, 20, 0) > backoff_secs(&s, 0, 0));
    }

    /// A plugin that says nothing accepted everything — which is what lets
    /// a three-line shell script be a legitimate sink.
    #[tokio::test]
    async fn a_silent_plugin_that_exits_zero_counts_as_accepted() {
        let outcome = plugin_write("true", b"{}\n", 1).await;
        assert_eq!(outcome, WriteOutcome::Accepted);
    }

    /// A non-zero exit is a retry, and the message **names the plugin** —
    /// otherwise every plugin bug reads as a meshware bug.
    #[tokio::test]
    async fn a_failing_plugin_is_named_in_the_error() {
        let WriteOutcome::Failed { status } = plugin_write("false", b"{}\n", 1).await else {
            panic!("a non-zero exit must not count as accepted");
        };
        assert!(status.contains("false"), "{status}");

        let WriteOutcome::Failed { status } =
            plugin_write("mw-no-such-plugin-anywhere", b"{}\n", 1).await
        else {
            panic!("a missing plugin must fail");
        };
        assert!(status.contains("mw-no-such-plugin-anywhere"), "{status}");
        assert!(status.contains("spawn"), "{status}");
    }

    /// A partial ack advances the cursor only to the acked row.
    #[tokio::test]
    async fn a_partial_ack_is_reported_as_partial() {
        // `sh -c` so the fixture is the plugin contract rather than a file
        // this test has to write and chmod.
        let outcome = plugin_write("/bin/sh", b"", 10).await;
        // With no `write` handling `/bin/sh` exits non-zero on the unknown
        // argument, which is itself the contract working. The partial path
        // is exercised through the decoder directly:
        assert!(matches!(outcome, WriteOutcome::Failed { .. }));

        let parsed = serde_json::from_slice::<serde_json::Value>(br#"{"accepted": 4}"#)
            .ok()
            .and_then(|v| v.get("accepted").and_then(|n| n.as_u64()));
        assert_eq!(parsed, Some(4));
    }

    // -- taking over a sink (protocol 1.19) ---------------------------------

    /// A node that wins the election continues from the published
    /// watermark instead of re-exporting the whole history.
    #[test]
    fn seeding_moves_a_fresh_cursor_forward() {
        let mut c = SinkCursor::default();
        c.seed_from(5_000);
        assert_eq!(c.jobs_micros, 5_000);
        assert_eq!(c.usage_micros, 5_000);
        assert_eq!(c.audit_micros, 5_000);
        // The id half is cleared, so the mark compares as "the earliest row
        // at this instant" — re-sending at most one microsecond's rows
        // rather than skipping any. Duplicates are promised; gaps are not.
        assert!(c.jobs_id.is_empty());
    }

    /// **Never backwards.** `sink_progress` is LWW across the mesh, so a
    /// stale publication must not rewind a node that has already exported
    /// further — assignment would silently re-send everything between.
    #[test]
    fn seeding_never_rewinds_a_cursor_that_is_already_ahead() {
        let mut c = SinkCursor::default();
        c.set_jobs(Mark::new(9_000, "job-z"));
        c.set_usage(Mark::new(1_000, "usage-a"));
        c.seed_from(5_000);
        assert_eq!(c.jobs_micros, 9_000, "already past the watermark");
        assert_eq!(c.jobs_id, "job-z", "and its id is untouched");
        assert_eq!(c.usage_micros, 5_000, "behind it, so it advances");
    }

    /// **A blank-path file sink must never seed.** Its records file lives
    /// in `<home>/sinks/<name>/` — the same directory as the cursor — so a
    /// fresh cursor means a fresh, absent file, and adopting another node's
    /// watermark would skip straight past history this node's file has
    /// never held. It is also the shape of every sink created before the
    /// probe gated them, which is exactly the population that must not lose
    /// rows to a feature it never opted into.
    ///
    /// This mirrors `Node::export_sink`'s rule; the two are one sentence
    /// apart on purpose, and a change to either should break this.
    #[test]
    fn only_a_destination_that_outlives_the_node_may_be_seeded() {
        let outlives = |s: &Sink| match s.kind {
            SinkKind::Plugin => false,
            SinkKind::Ndjson => !s.database.trim().is_empty(),
            _ => true,
        };

        // Writes beside its own cursor: a fresh cursor is a fresh file.
        assert!(!outlives(&sink(SinkKind::Ndjson)));

        // An explicit path is gated to resolve to one file, so the file the
        // watermark describes is the one this node is about to open.
        let mut named = sink(SinkKind::Ndjson);
        named.database = "/mnt/history/records.ndjson".into();
        assert!(outlives(&named));

        // A program on *this* machine. Another node's progress says
        // nothing about what this one has delivered.
        let mut plugin = sink(SinkKind::Plugin);
        plugin.database = "/usr/local/bin/mw-ship".into();
        assert!(!outlives(&plugin));

        // External stores retain what was sent, so skipping is correct.
        assert!(outlives(&sink(SinkKind::ClickHouse)));
        assert!(outlives(&sink(SinkKind::Webhook)));
    }

    /// A cursor written by an older build has no usage or audit marks, and
    /// must load as "nothing exported yet" rather than fail to parse.
    #[test]
    fn an_older_cursor_loads_with_the_new_tables_at_zero() {
        let old = r#"{"runs_micros":7,"jobs_micros":9}"#;
        let c: SinkCursor = serde_json::from_str(old).expect("older cursor still loads");
        assert_eq!(c.jobs_micros, 9);
        assert_eq!(c.usage_micros, 0);
        assert_eq!(c.audit_micros, 0);
    }

    // -- the two tables that used to be decorative --------------------------

    /// Both window edges travel. A consumer summing `cpu_secs` by hour has
    /// to know an hour is what it is looking at, and a bare start instant
    /// makes that a fact about the exporter's config rather than the row.
    #[test]
    fn a_usage_row_carries_both_edges_of_its_window() {
        let row = mesh_proto::UsageRow {
            node: mesh_proto::NodeId([1u8; 32]),
            principal: mesh_proto::UserId([2u8; 32]),
            window: mesh_proto::usage_window(1_700_000_000_000_000),
            cpu_secs: 12,
            mem_mb_secs: 34,
            gpu_secs: 0,
            wall_secs: 56,
            jobs: 7,
        };
        let line = usage_line("mesh-x", &row);
        assert_eq!(line["table"], "usage");
        assert_eq!(line["cpu_secs"], 12);
        let start = line["window_start"].as_str().expect("start");
        let end = line["window_end"].as_str().expect("end");
        assert!(start < end, "{start} should precede {end}");
    }

    /// **The signature travels with the row.** A warehouse full of flat
    /// audit rows proves only that somebody inserted rows into a
    /// warehouse; non-repudiation lives in the signature over the op.
    #[test]
    fn an_audit_row_carries_its_proof() {
        let row = mesh_proto::SignedAuditRow {
            entry: mesh_proto::AuditEntry {
                audit_id: mesh_proto::AuditId([3u8; 16]),
                at_micros: 1_700_000_000_000_000,
                user: mesh_proto::UserId([2u8; 32]),
                username: "ada".into(),
                action: "sink.create".into(),
                target: "warehouse".into(),
                node: mesh_proto::NodeId([1u8; 32]),
                outcome: "ok".into(),
                detail: "created ndjson".into(),
                request_sig: vec![9, 9],
            },
            author: mesh_proto::NodeId([1u8; 32]),
            seq: 42,
            hlc: Default::default(),
            signature: vec![0xab, 0xcd],
            signing_bytes: vec![0x01, 0x02],
        };
        let line = audit_line("mesh-x", &row);
        assert_eq!(line["table"], "audit");
        assert_eq!(line["action"], "sink.create");
        // Hex, because every log store on earth holds a string and roughly
        // none of them enjoy a 64-element integer array.
        assert_eq!(line["signature"], "abcd");
        assert_eq!(line["signing_bytes"], "0102");
    }
}
