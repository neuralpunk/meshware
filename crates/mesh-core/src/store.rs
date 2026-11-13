//! Durable op-log storage: a single redb file per node (MASTERPLAN §4.5).
//!
//! Key layout: `author(32 bytes) || seq(8 bytes big-endian)` so that a range
//! scan per author yields ops in sequence order — anti-entropy is a plain
//! version-vector range pull. [`OpStore::missing_for`] is that range pull;
//! before F0 it decoded the whole log on every gossip tick and filtered in
//! memory, which was invisible at certification and expensive at year two.
//!
//! Values are `postcard(OpEnvelope)` (F0): the op body stays length-delimited
//! all the way to disk, so a node persists and forwards an op whose body it
//! has no code for instead of failing the scan that reads it.

use std::path::Path;

use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};

use mesh_proto::{NodeId, OpEnvelope, SeqRange, VersionVector};

use crate::error::{CoreError, Result};

const OPS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("ops");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// Store format key in [`META`].
const FORMAT_KEY: &str = "format";

/// Format 2 (F0): op values are `postcard(OpEnvelope)`. Format 1 — or a
/// non-empty `ops` table with no format key, which is what every pre-F0
/// node has — means `postcard(Op)` and is migrated in one pass on open.
/// **3 since protocol 2.0** (2026-09-20). Format 2 holds ops encoded with
/// 1.x's `OpBody` variant indices, and 2.0 deleted thirty retired variants,
/// so everything after the first deletion renumbered. A format-2 log read
/// by this binary would decode each of those ops as a *different kind* —
/// silently, because postcard has no type tag beyond the index, and the
/// signature check would then fail on data that was never corrupt.
///
/// So this bump is not a storage change. It is the guard that turns a
/// silent misread into a refusal with a remedy.
const STORE_FORMAT: u64 = 3;

const OPLOG_FILE: &str = "oplog.redb";
/// The pre-migration log, kept until the next clean start (F0 §3.3).
const OPLOG_PRE_F0: &str = "oplog.redb.pre-f0";

/// Max ops returned per sync batch; requesters loop while `more`.
pub const SYNC_BATCH: usize = 512;

/// Max *bytes* of encoded ops per sync batch; requesters loop while `more`.
///
/// [`SYNC_BATCH`] alone bounds the wrong quantity. A batch is a frame, and
/// a frame is capped at [`mesh_proto::MAX_FRAME_LEN`] — but 512 ops is a
/// count, and op bodies range from a 60-byte `InviteConsume` to a workflow
/// version or a fact document measured in kilobytes. 512 of the large kind
/// overflows the frame and the batch is not merely slow, it is undeliverable:
/// the encoder refuses it, the requester never advances, and the peer is
/// stuck at exactly the point in the log where the big ops start.
///
/// So the batch is bounded by both, and this is the one that matters. A
/// quarter of the frame budget leaves ample room for the liveness list and
/// version vector that ride alongside the ops, and for an op that is itself
/// larger than the budget — the count check runs first, so a batch always
/// carries at least one op and the log always drains.
pub const SYNC_BATCH_BYTES: usize = mesh_proto::MAX_FRAME_LEN as usize / 4;

#[derive(Debug)]
pub struct OpStore {
    /// Behind an `RwLock` because two things need it alone:
    /// `Database::compact`, which takes `&mut self` and refuses while any
    /// read transaction is live (C0 P2), and reopening after a failed write
    /// (ENG-4), which is why the handle is an `Option` — the old one has to
    /// be dropped, releasing its file lock, before the new one can open.
    /// Every other method takes the read guard, so the lock is uncontended
    /// on every path but those two.
    db: parking_lot::RwLock<Option<Database>>,
    /// Kept so `meshnode state show` can report what the log costs on disk
    /// without the caller having to know where it lives.
    path: std::path::PathBuf,
    /// Tests: writes fail as on a full disk, and — as redb does after a
    /// failed commit — keep failing until the store is reopened.
    faults: StoreFaults,
    /// How many times the file has been rewritten to shrink it.
    reclaims: std::sync::atomic::AtomicU64,
}

/// The least a reclaim must give back to be worth doing (ENG-5).
const RECLAIM_MIN_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default)]
struct StoreFaults {
    full: std::sync::atomic::AtomicBool,
    latched: std::sync::atomic::AtomicBool,
}

impl StoreFaults {
    fn check(&self) -> Result<()> {
        use std::sync::atomic::Ordering::Relaxed;
        if self.full.load(Relaxed) {
            self.latched.store(true, Relaxed);
            return Err(CoreError::Store(
                "No space left on device (os error 28) [injected]".into(),
            ));
        }
        if self.latched.load(Relaxed) {
            return Err(CoreError::Store(
                "a previous write failed; the store has to be reopened [injected]".into(),
            ));
        }
        Ok(())
    }
}

fn op_key(author: &NodeId, seq: u64) -> [u8; 40] {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(&author.0);
    key[32..].copy_from_slice(&seq.to_be_bytes());
    key
}

fn open_db(path: &Path) -> Result<Database> {
    let db = open_repairing(path).map_err(|e| {
        // A damaged file used to be the end of the node: it could not
        // start, and nothing said what to do (ENG-13).
        let damaged = match &e {
            redb::DatabaseError::Storage(redb::StorageError::Corrupted(_)) => true,
            redb::DatabaseError::Storage(redb::StorageError::Io(io)) => matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
            ),
            _ => false,
        };
        CoreError::Store(if damaged {
            format!(
                "open oplog {}: {e} — the file is damaged. `meshnode state rebuild --from \
                 <member host:port>` fetches this node's history from a peer (the file is kept), \
                 or restore a backup",
                path.display()
            )
        } else {
            format!("open oplog: {e}")
        })
    })?;
    // Ensure the tables exist so first reads don't error.
    let txn = begin_write(&db).map_err(|e| CoreError::Store(format!("init txn: {e}")))?;
    txn.open_table(OPS)
        .map_err(|e| CoreError::Store(format!("init table: {e}")))?;
    txn.open_table(META)
        .map_err(|e| CoreError::Store(format!("init meta: {e}")))?;
    txn.commit()
        .map_err(|e| CoreError::Store(format!("init commit: {e}")))?;
    Ok(db)
}

/// Open (or create) a redb file, saying so while a crash repair runs
/// (ENG-1).
///
/// A repair walks the whole file. Before quick-repair it was the normal
/// cost of every unclean stop, and on a large log it could outlast the
/// unit's start timeout — which, with the old `Restart=on-success`, left
/// the node down for good. The callback feeds the start-deadline
/// extension and puts the repair in the journal.
pub(crate) fn open_repairing(path: &Path) -> std::result::Result<Database, redb::DatabaseError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let last = std::cell::Cell::new(-1i64);
    redb::Builder::new()
        .set_repair_callback(move |session| {
            crate::vitals::startup_progress(1);
            let pct = (session.progress() * 100.0) as i64;
            if pct / 10 != last.get() / 10 {
                last.set(pct);
                tracing::warn!(file = %name, progress = pct, "repairing after an unclean stop");
            }
        })
        .create(path)
}

/// A write transaction with quick-repair on (ENG-1).
///
/// redb then saves its allocator state with every durable commit, so
/// reopening after a crash is near-instant instead of a walk of the whole
/// file. The price is a second sync per durable commit.
pub(crate) fn begin_write(
    db: &Database,
) -> std::result::Result<redb::WriteTransaction, redb::TransactionError> {
    let mut txn = db.begin_write()?;
    txn.set_quick_repair(true);
    Ok(txn)
}

/// What [`OpStore::open`] found on disk, before any migration ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnDisk {
    /// Nothing stored yet — stamp the current format and go.
    Fresh,
    /// Already `postcard(OpEnvelope)`.
    Current,
}

fn inspect(db: &Database) -> Result<OnDisk> {
    let txn = db
        .begin_read()
        .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
    let meta = txn
        .open_table(META)
        .map_err(|e| CoreError::Store(format!("open meta: {e}")))?;
    let format = meta
        .get(FORMAT_KEY)
        .map_err(|e| CoreError::Store(format!("read format: {e}")))?
        .map(|v| v.value());
    let ops = txn
        .open_table(OPS)
        .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
    let count = ops
        .len()
        .map_err(|e| CoreError::Store(format!("count ops: {e}")))?;
    match format {
        Some(v) if v > STORE_FORMAT => Err(CoreError::TooNew(format!(
            "op-log format {v} is newer than this binary understands ({STORE_FORMAT}); \
             upgrade meshnode or restore a backup"
        ))),
        Some(v) if v == STORE_FORMAT => Ok(OnDisk::Current),
        // A protocol-1.x log. Refused rather than migrated: see
        // `STORE_FORMAT`. There is no in-place migration to offer, because
        // every op in it is signed over bytes whose meaning changed.
        Some(2) => Err(CoreError::TooNew(format!(
            "this data directory was written by meshware speaking protocol 1.x, and this \
             binary speaks 2.0. The two encode the op-log differently, so it cannot be \
             read or migrated — see the changelog. Start a fresh data directory, or \
             restore a backup with a 1.x binary and `meshnode reset` the mesh before \
             upgrading ({count} ops on disk)"
        ))),
        // Format 1 was never stamped; an unstamped empty log is a fresh node.
        _ if count == 0 => Ok(OnDisk::Fresh),
        _ => Err(CoreError::TooNew(format!(
            "this data directory predates protocol 2.0 and cannot be read by this binary \
             ({count} ops on disk). Start a fresh data directory"
        ))),
    }
}

fn stamp_format(db: &Database) -> Result<()> {
    let txn = begin_write(db).map_err(|e| CoreError::Store(format!("write txn: {e}")))?;
    {
        let mut meta = txn
            .open_table(META)
            .map_err(|e| CoreError::Store(format!("open meta: {e}")))?;
        meta.insert(FORMAT_KEY, STORE_FORMAT)
            .map_err(|e| CoreError::Store(format!("stamp format: {e}")))?;
    }
    txn.commit()
        .map_err(|e| CoreError::Store(format!("commit: {e}")))?;
    Ok(())
}

impl OpStore {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("state");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(OPLOG_FILE);
        let keep = dir.join(OPLOG_PRE_F0);

        let db = open_db(&path)?;
        match inspect(&db)? {
            OnDisk::Fresh => stamp_format(&db)?,
            OnDisk::Current => {
                // The previous log is retained across exactly one clean
                // start, so a failed upgrade has something to roll back to.
                if keep.exists() {
                    let _ = std::fs::remove_file(&keep);
                }
            }
        }
        Ok(Self {
            db: parking_lot::RwLock::new(Some(db)),
            path,
            faults: StoreFaults::default(),
            reclaims: Default::default(),
        })
    }

    /// Make every write fail as on a full disk, until called with `false`
    /// *and* the store is reopened (tests).
    pub fn fill_disk_for_test(&self, full: bool) {
        self.faults
            .full
            .store(full, std::sync::atomic::Ordering::Relaxed);
    }

    fn read_guard(&self) -> Result<parking_lot::MappedRwLockReadGuard<'_, Database>> {
        parking_lot::RwLockReadGuard::try_map(self.db.read(), Option::as_ref)
            .map_err(|_| CoreError::Store("the op-log is closed after a failed reopen".into()))
    }

    /// Whether the store holds an open database.
    pub fn is_open(&self) -> bool {
        self.db.read().is_some()
    }

    /// Close the file and open it again (ENG-4).
    ///
    /// redb refuses every write transaction after a commit fails part-way —
    /// a full disk, say — until the database is reopened, and nothing
    /// reopened it: one failed write left a node that could never write
    /// again, however much space came back. Reopening runs redb's repair if
    /// it needs one.
    pub fn reopen(&self) -> Result<()> {
        let mut db = self.db.write();
        // Dropped first: the old handle holds the file lock.
        drop(db.take());
        self.faults
            .latched
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let reopened = open_db(&self.path)?;
        *db = Some(reopened);
        Ok(())
    }

    /// Reclaim the file after a prune (C0 P2).
    ///
    /// redb frees pages on delete but does not shrink the file, so the op
    /// *count* drops the moment a pass commits and the bytes follow only
    /// when something asks. `Database::compact` needs exclusive access and
    /// refuses outright while any read transaction is open — hence the lock
    /// around the handle, and hence this being a separate call from
    /// [`OpStore::prune`]: reclaiming is slow, and it must not run inside
    /// the critical section that made the holes.
    ///
    /// Returns whether the file actually shrank. `false` is a normal
    /// answer, not a failure.
    pub fn reclaim(&self) -> Result<bool> {
        let mut db = self.db.write();
        let Some(db) = db.as_mut() else {
            return Err(CoreError::Store("the op-log is closed".into()));
        };
        self.reclaims
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match db.compact() {
            Ok(shrank) => Ok(shrank),
            // A reader that outlived our guard cannot happen — the guard is
            // exclusive — but a savepoint from a concurrent backup can, and
            // the right answer is "not now" rather than an error the caller
            // has to special-case.
            Err(redb::CompactionError::TransactionInProgress) => Ok(false),
            Err(e) => Err(CoreError::Store(format!("reclaim oplog: {e}"))),
        }
    }

    /// [`Self::reclaim`], when it would give back enough to be worth it
    /// (ENG-5): a mebibyte at least, and either three quarters of the file
    /// or a real share of what is left on the volume.
    ///
    /// A reclaim rewrites the file while holding the store alone, so every
    /// read — a peer's sync, a status query — waits it out. Run after every
    /// pass that dropped anything, an hourly pass that dropped a handful of
    /// ops stopped the store to win back a page.
    ///
    /// Three quarters, not less, because redb doubles a small file when it
    /// grows: up to half of any file is room the next writes will use, and
    /// giving it back only has the file grow again.
    pub fn reclaim_if_worthwhile(&self) -> Result<bool> {
        let spare = self.reclaimable_bytes()?;
        if spare < RECLAIM_MIN_BYTES {
            return Ok(false);
        }
        let mostly_spare = spare.saturating_mul(4) >= self.bytes_on_disk().saturating_mul(3);
        let volume_is_short = crate::capacity::disk_free_mb(&self.path)
            .is_some_and(|mb| mb.saturating_mul(1024 * 1024) < spare.saturating_mul(4));
        if !mostly_spare && !volume_is_short {
            return Ok(false);
        }
        self.reclaim()
    }

    /// Bytes a reclaim could give back: free pages and the unused part of
    /// partly-filled ones, as redb counts them.
    pub fn reclaimable_bytes(&self) -> Result<u64> {
        let db = self.read_guard()?;
        // redb reports this on a write transaction only; nothing is written.
        let txn = begin_write(&db).map_err(|e| CoreError::Store(format!("write txn: {e}")))?;
        let spare = txn
            .stats()
            .map_err(|e| CoreError::Store(format!("stats: {e}")))?
            .fragmented_bytes();
        txn.abort()
            .map_err(|e| CoreError::Store(format!("abort: {e}")))?;
        Ok(spare)
    }

    /// How many times the file has been rewritten to shrink it (tests).
    pub fn reclaims(&self) -> u64 {
        self.reclaims.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bytes the op-log occupies on disk. redb never shrinks its file
    /// eagerly, so this is the honest number to show an operator after a
    /// compaction pass — the op count drops immediately, the file follows
    /// when redb reuses the freed pages.
    pub fn bytes_on_disk(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Persist ops (idempotent: re-inserting an existing key is a no-op
    /// overwrite with identical bytes).
    pub fn append(&self, ops: &[OpEnvelope]) -> Result<()> {
        self.append_with(ops, Durability::Immediate)
    }

    /// [`Self::append`] without waiting for the disk (**H-3**).
    ///
    /// redb's `Durability::None` writes into the database but does not
    /// force a flush; the bytes land the moment *any* later `Immediate`
    /// commit does, which [`Self::flush`] guarantees on a timer. A crash in
    /// between loses the tail.
    ///
    /// **Only ever for ops this node did not author, and that restriction
    /// is load-bearing.** `author_op` takes its next sequence number from
    /// the highest one it has applied, and that number is rebuilt by
    /// replaying this store at boot. Lose the tail of our *own* ops and the
    /// counter rolls back, so the node re-issues a sequence number that
    /// peers already hold bound to a different op — and the apply path
    /// treats the older one as a duplicate and drops it, so the two never
    /// reconcile. That is a silent permanent fork, and it is why the
    /// durability choice here is made by *provenance* rather than by how
    /// important the record looks.
    ///
    /// Ops from peers have none of that problem: they are re-fetchable by
    /// anti-entropy, which is the same path that would have delivered them
    /// had they never arrived.
    pub fn append_relaxed(&self, ops: &[OpEnvelope]) -> Result<()> {
        self.append_with(ops, Durability::None)
    }

    /// Force everything written with [`Self::append_relaxed`] to disk.
    ///
    /// An empty transaction is enough: redb persists earlier `None` commits
    /// on the next `Immediate` one, whatever it contains.
    pub fn flush(&self) -> Result<()> {
        self.faults.check()?;
        let db = self.read_guard()?;
        let txn = begin_write(&db).map_err(|e| CoreError::Store(format!("write txn: {e}")))?;
        txn.commit()
            .map_err(|e| CoreError::Store(format!("flush: {e}")))?;
        Ok(())
    }

    fn append_with(&self, ops: &[OpEnvelope], durability: Durability) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        self.faults.check()?;
        let db = self.read_guard()?;
        let mut txn = begin_write(&db).map_err(|e| CoreError::Store(format!("write txn: {e}")))?;
        txn.set_durability(durability)
            .map_err(|e| CoreError::Store(format!("durability: {e}")))?;
        {
            let mut table = txn
                .open_table(OPS)
                .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
            for op in ops {
                let key = op_key(&op.author, op.seq);
                let value = postcard::to_stdvec(op)
                    .map_err(|e| CoreError::Store(format!("encode op: {e}")))?;
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(|e| CoreError::Store(format!("insert: {e}")))?;
            }
        }
        txn.commit()
            .map_err(|e| CoreError::Store(format!("commit: {e}")))?;
        Ok(())
    }

    /// All ops, grouped per author in sequence order. Used for replay at
    /// boot, for backup, and for the full snapshot a joiner is handed —
    /// never on the gossip path.
    /// One op by its `(author, seq)` locator.
    ///
    /// A point lookup on the primary key, which is what makes releasing an
    /// immutable payload from memory cheap to undo: state keeps the locator,
    /// the bytes stay in the log, and reading them back is a B-tree descent
    /// rather than a scan. `None` means the log does not have it — the op
    /// was pruned, or this node never received it.
    pub fn get(&self, author: &NodeId, seq: u64) -> Result<Option<OpEnvelope>> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        let key = op_key(author, seq);
        let Some(value) = table
            .get(key.as_slice())
            .map_err(|e| CoreError::Store(format!("get op: {e}")))?
        else {
            return Ok(None);
        };
        let envelope: OpEnvelope = postcard::from_bytes(value.value())
            .map_err(|e| CoreError::Store(format!("decode op: {e}")))?;
        Ok(Some(envelope))
    }

    pub fn load_all(&self) -> Result<Vec<OpEnvelope>> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        let mut ops = Vec::new();
        for entry in table
            .range::<&[u8]>(..)
            .map_err(|e| CoreError::Store(format!("range: {e}")))?
        {
            let (_, value) = entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
            let op: OpEnvelope = postcard::from_bytes(value.value())
                .map_err(|e| CoreError::Store(format!("decode op: {e}")))?;
            ops.push(op);
        }
        Ok(ops)
    }

    /// Hand every stored op's **encoded bytes** to `f`, in key order,
    /// without decoding any of them (ENG-20).
    ///
    /// For the one caller that does not want `OpEnvelope`s: a backup copies
    /// the log, and decoding a million ops into structs only to re-encode
    /// them costs the decode, the allocations, and a second full copy of
    /// the log in memory — to arrive back at the bytes that were already
    /// on disk. What is written out is exactly what was read in, which is
    /// also a stronger guarantee than a round-trip through this build's
    /// idea of the type.
    pub fn for_each_op_bytes(&self, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        for entry in table
            .range::<&[u8]>(..)
            .map_err(|e| CoreError::Store(format!("range: {e}")))?
        {
            let (_, value) = entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
            f(value.value())?;
        }
        Ok(())
    }

    /// Hand every stored op to `f`, in key order, one at a time (ENG-20).
    ///
    /// The streaming counterpart to [`Self::load_all`], for the callers
    /// that filter: a scan that keeps one op alive at a time rather than a
    /// `Vec` of the whole log. The audit export and the census both walk
    /// the log to keep a small fraction of it, and both used to materialise
    /// all of it first.
    pub fn for_each_op(&self, mut f: impl FnMut(OpEnvelope) -> Result<()>) -> Result<()> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        for entry in table
            .range::<&[u8]>(..)
            .map_err(|e| CoreError::Store(format!("range: {e}")))?
        {
            let (_, value) = entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
            let op: OpEnvelope = postcard::from_bytes(value.value())
                .map_err(|e| CoreError::Store(format!("decode op: {e}")))?;
            f(op)?;
        }
        Ok(())
    }

    /// How many ops are stored (a redb table length, not a scan).
    pub fn len(&self) -> Result<u64> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        table
            .len()
            .map_err(|e| CoreError::Store(format!("count ops: {e}")))
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Version vector: max seq per author present in the log. A key-only
    /// scan — no op body is decoded. The live path reads
    /// `MeshState::applied` instead; this exists for diagnostics and for
    /// asserting the two agree.
    pub fn version_vector(&self) -> Result<VersionVector> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        let mut vv: VersionVector = Vec::new();
        for entry in table
            .range::<&[u8]>(..)
            .map_err(|e| CoreError::Store(format!("range: {e}")))?
        {
            let (key, _) = entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
            let raw = key.value();
            if raw.len() != 40 {
                return Err(CoreError::Store("op-log has a malformed key".into()));
            }
            let mut author = [0u8; 32];
            author.copy_from_slice(&raw[..32]);
            let mut seq_bytes = [0u8; 8];
            seq_bytes.copy_from_slice(&raw[32..]);
            let (author, seq) = (NodeId(author), u64::from_be_bytes(seq_bytes));
            match vv.iter_mut().find(|(a, _)| *a == author) {
                Some((_, cur)) => *cur = (*cur).max(seq),
                None => vv.push((author, seq)),
            }
        }
        Ok(vv)
    }

    /// Ops the holder of `theirs` is missing, bounded by [`SYNC_BATCH`].
    ///
    /// `ours` is this node's own version vector — supplied by the caller
    /// from in-memory state rather than recomputed here, so the whole
    /// gossip path costs one range scan per lagging author and nothing at
    /// all for authors the peer is caught up on.
    pub fn missing_for(
        &self,
        ours: &VersionVector,
        theirs: &VersionVector,
    ) -> Result<(Vec<OpEnvelope>, bool)> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;

        // Deterministic author order so a repeated request yields the same
        // batch, which is what makes the requester's `more` loop terminate.
        let mut authors: Vec<(NodeId, u64)> = ours.to_vec();
        authors.sort_by_key(|(author, _)| author.0);

        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;
        'authors: for (author, our_seq) in authors {
            let their_seq = theirs
                .iter()
                .find(|(a, _)| *a == author)
                .map(|(_, s)| *s)
                .unwrap_or(0);
            if their_seq >= our_seq {
                continue;
            }
            let lo = op_key(&author, their_seq + 1);
            let hi = op_key(&author, our_seq);
            for entry in table
                .range(lo.as_slice()..=hi.as_slice())
                .map_err(|e| CoreError::Store(format!("range: {e}")))?
            {
                let (_, value) = entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
                // Count first, so a batch is never empty: an op larger than
                // the byte budget still goes out on its own rather than
                // wedging the log behind a batch that can never be filled.
                if out.len() >= SYNC_BATCH || (!out.is_empty() && bytes >= SYNC_BATCH_BYTES) {
                    more = true;
                    break 'authors;
                }
                // The stored bytes *are* the wire bytes — the value is the
                // same `postcard(OpEnvelope)` the frame will carry — so this
                // measures the frame without encoding it twice.
                bytes += value.value().len();
                let op: OpEnvelope = postcard::from_bytes(value.value())
                    .map_err(|e| CoreError::Store(format!("decode op: {e}")))?;
                out.push(op);
            }
        }
        Ok((out, more))
    }

    // -- C0 compaction (C0-C9 §4) ------------------------------------------

    /// One author's ops in ascending `seq` order. The input to a compaction
    /// plan, and the only scan compaction does — an author's own ops are a
    /// contiguous key range, so this is one B-tree descent and a walk.
    pub fn load_author(&self, author: &NodeId) -> Result<Vec<OpEnvelope>> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        let lo = op_key(author, 0);
        let hi = op_key(author, u64::MAX);
        let mut ops = Vec::new();
        for entry in table
            .range(lo.as_slice()..=hi.as_slice())
            .map_err(|e| CoreError::Store(format!("range: {e}")))?
        {
            let (_, value) = entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
            ops.push(
                postcard::from_bytes(value.value())
                    .map_err(|e| CoreError::Store(format!("decode op: {e}")))?,
            );
        }
        Ok(ops)
    }

    /// How many ops this author has in the log.
    pub fn count_for(&self, author: &NodeId) -> Result<u64> {
        let db = self.read_guard()?;
        let txn = db
            .begin_read()
            .map_err(|e| CoreError::Store(format!("read txn: {e}")))?;
        let table = txn
            .open_table(OPS)
            .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
        let lo = op_key(author, 0);
        let hi = op_key(author, u64::MAX);
        let mut n = 0u64;
        for entry in table
            .range(lo.as_slice()..=hi.as_slice())
            .map_err(|e| CoreError::Store(format!("range: {e}")))?
        {
            entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
            n += 1;
        }
        Ok(n)
    }

    /// Delete `ranges` of `author`'s ops in one transaction.
    ///
    /// **Only ever for ranges `author` has vouched for.** An author is the
    /// only authority on which of its sequence numbers may stop existing:
    /// a node compacting its own log writes the checkpoint first, and a node
    /// pruning a peer's copy prunes what that peer's applied checkpoint says
    /// (ENG-3). A range nobody vouched for would be a hole the gapless rule
    /// parks behind for ever. The callers enforce it; this records it.
    ///
    /// Atomic with respect to readers: a sync in flight either sees the ops
    /// or sees them gone, and either answer is correct because the
    /// checkpoint that vouches for the hole is already durable — it is
    /// authored *before* this runs.
    pub fn prune(&self, author: &NodeId, ranges: &[SeqRange]) -> Result<u64> {
        if ranges.is_empty() {
            return Ok(0);
        }
        self.faults.check()?;
        let db = self.read_guard()?;
        let txn = begin_write(&db).map_err(|e| CoreError::Store(format!("write txn: {e}")))?;
        let mut removed = 0u64;
        {
            let mut table = txn
                .open_table(OPS)
                .map_err(|e| CoreError::Store(format!("open table: {e}")))?;
            for range in ranges {
                // Collect first: redb will not let the range borrow live
                // across the removes it feeds.
                let lo = op_key(author, range.lo);
                let hi = op_key(author, range.hi);
                let mut keys: Vec<[u8; 40]> = Vec::new();
                for entry in table
                    .range(lo.as_slice()..=hi.as_slice())
                    .map_err(|e| CoreError::Store(format!("range: {e}")))?
                {
                    let (key, _) = entry.map_err(|e| CoreError::Store(format!("scan: {e}")))?;
                    let k: [u8; 40] = key
                        .value()
                        .try_into()
                        .map_err(|_| CoreError::Store("op-log has a malformed key".into()))?;
                    keys.push(k);
                }
                for key in keys {
                    if table
                        .remove(key.as_slice())
                        .map_err(|e| CoreError::Store(format!("remove: {e}")))?
                        .is_some()
                    {
                        removed += 1;
                    }
                }
            }
        }
        txn.commit()
            .map_err(|e| CoreError::Store(format!("commit prune: {e}")))?;
        // The op *count* drops here; the file does not. redb reuses freed
        // pages on subsequent writes, and `Database::compact` needs `&mut`
        // — reclaiming the file is C0 P2's job, along with the timer that
        // calls this.
        Ok(removed)
    }
}
