//! The ephemeral object store and its rendezvous-hashed directory
//! (MASTERPLAN M9 §5.5).
//!
//! This is G4 — the gap that makes rung 3 possible. Without it, every
//! intermediate byte a parallel job produces funnels back through the node
//! that accepted the job, and that node becomes exactly the central
//! bottleneck the whole project exists to avoid. With it, a worker's output
//! stays on the worker, the owner sees a 40-byte reference, and whoever
//! needs the bytes fetches them straight from the producer.
//!
//! Four things differ from a bundle, and only four:
//!
//! | | Bundles | Objects |
//! |---|---|---|
//! | Lifetime | durable, retention GC | ephemeral, refcounted to the owning job |
//! | Placement | pulled where needed | written locally, fetched on demand |
//! | Index | replicated op-log | rendezvous-hashed directory, never the op-log |
//! | Residency | disk | RAM with disk spill |
//!
//! Everything else is deliberately the same. Chunking, hashing and the
//! `GetChunk` fetch path are reused verbatim, because a chunk hash is a
//! global content address and the transfer path has no reason to care which
//! class of object a chunk belongs to.
//!
//! **Objects spill to their own blob directory**, not the bundle store's.
//! They share a hash space with bundle chunks, so an object chunk could
//! collide with a bundle chunk byte-for-byte — and reclaiming the object
//! would then delete a chunk a durable bundle still needs. Separate
//! directories make the sweep obviously safe rather than subtly correct.

use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mesh_proto::objects::{DIRECTORY_REPLICAS, OBJDIR_CONTEXT};
use mesh_proto::{ChunkHash, JobId, NodeId, ObjectId, ObjectManifest, ObjectRef, ObjectRow};
use tracing::debug;

use crate::content::CHUNK_SIZE;
use crate::error::{CoreError, Result};

/// Fallback RAM budget when the machine's memory cannot be read.
/// See [`default_ram_mb`], which is what actually gets used.
pub const FALLBACK_RAM_MB: u64 = 256;

/// Share of system memory the object cache may hold.
const RAM_SHARE: u64 = 8;
/// Floor and ceiling on the derived budget.
const RAM_MIN_MB: u64 = 64;
const RAM_MAX_MB: u64 = 2048;

/// RAM budget for object bytes before they spill to disk, derived from the
/// machine.
///
/// This was a flat 256 MB, which is the wrong answer twice: on a 1 GB
/// single-board node it is a quarter of the machine, and on a 512 GB
/// compute node it means spilling intermediates to disk while half a
/// terabyte of RAM sits idle. An eighth of total memory, floored at 64 MB
/// and capped at 2 GB, is right on both.
///
/// Still a *cache tier*, not the store, which is why the ceiling exists: a
/// node running forty lanes must not trade its executor's memory for one.
pub fn default_ram_mb() -> u64 {
    match crate::capacity::mem_total_mb() {
        Some(total) if total > 0 => (total / RAM_SHARE).clamp(RAM_MIN_MB, RAM_MAX_MB),
        _ => FALLBACK_RAM_MB,
    }
}

/// How long an object outlives the job that owns it. Long enough for a
/// reduce step, a `--out` fetch or an operator's `objects get` to arrive
/// after the run reports done; short enough that a busy node is not
/// carrying yesterday's intermediates.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

/// How long an object whose owning job this node has *never heard of* is
/// kept. Longer than the TTL because "unknown" is usually gossip lag, not
/// an orphan: the announcement of a job can easily reach a directory node
/// after the first object it indexes.
const UNKNOWN_GRACE: Duration = Duration::from_secs(30 * 60);

fn hash_chunk(bytes: &[u8]) -> ChunkHash {
    ChunkHash(*blake3::hash(bytes).as_bytes())
}

/// One object held on this node.
#[derive(Debug, Clone)]
struct Held {
    manifest: ObjectManifest,
    /// Every job that still needs this object — the refcount §5.5 asks
    /// for, and it has to be a set rather than a single job.
    ///
    /// An object is addressed by its content, so two jobs producing
    /// identical output produce *one* object with two owners. That is not
    /// an edge case: a validation pass where every item prints `ok` is
    /// exactly it. Keying the lifetime on one owner would have let the
    /// first job to finish delete an object the second still needs.
    jobs: BTreeSet<JobId>,
    /// Last time a job took a reference. A second owner refreshes it, so
    /// a live reference is never reclaimed on a dead one's schedule.
    touched: Instant,
    /// A cache entry references these bytes (C7 P3).
    ///
    /// **Pinning is what makes the cache outlive the job.** An object's
    /// refcount is the job that made it, and a job is over in minutes — so
    /// without this an action cache would forget everything on the object
    /// TTL and "the second run finishes in seconds" would be true for
    /// fifteen minutes a day. A pin says *somebody has published a promise
    /// about these bytes*, and the promise is kept until the entry holding
    /// it is evicted.
    ///
    /// A count rather than a flag: one object can back several entries —
    /// two actions whose stdout is byte-identical produce one object — and
    /// unpinning on the first eviction would break the second.
    pins: u32,
    /// When the pin stops being a promise and starts being a leak
    /// (ENG-19).
    ///
    /// **A pin had no way to be released on the node that took it.** The
    /// producer pins the bytes it published; the *directory* node — a
    /// different machine, chosen by rendezvous hashing over the key —
    /// evicts the entry and unpins, on an id it almost never holds, so the
    /// unpin is a no-op there and the producer is never told. There is no
    /// `ObjectRelease` on the wire and there should not be one: that is
    /// coordination the design deliberately does without.
    ///
    /// So the promise is a *lease* instead. It is renewed every time these
    /// bytes are read — an entry somebody is still hitting keeps them — and
    /// when nothing has asked for a lease's length, the pin lapses and the
    /// object falls back to the ordinary refcount rules. A cache entry may
    /// then outlive its bytes, which is a miss, which is what a cache is
    /// allowed to do; bytes nothing can release are what it is not.
    pin_until: Option<Instant>,
}

#[derive(Debug, Default)]
struct Resident {
    /// Chunks still in RAM, before the budget pushed them to disk.
    chunks: HashMap<ChunkHash, Arc<Vec<u8>>>,
    bytes: u64,
}

#[derive(Debug, Default)]
struct Inner {
    ram: Resident,
    held: BTreeMap<ObjectId, Held>,
    /// Finished runs whose results nobody has fetched yet (R3 P3), oldest
    /// first: the job, its objects, and their total size. A `Vec` rather
    /// than a map because the order *is* the eviction policy — the run you
    /// are still looking at is the newest one.
    uncollected: Vec<(JobId, Vec<ObjectId>, u64)>,
}

/// Ephemeral object storage: RAM first, disk when the budget runs out.
#[derive(Debug, Clone)]
pub struct ObjectStore {
    root: PathBuf,
    ram_budget: u64,
    ttl: Duration,
    inner: Arc<Mutex<Inner>>,
}

impl ObjectStore {
    pub fn open(data_dir: &Path, ram_mb: u64, ttl: Duration) -> Result<Self> {
        let root = data_dir.join("objects");
        // A restart forgets every object, exactly as it forgets the chunk
        // queue and liveness: an object is only meaningful while the job
        // that owns it is running, and no job survives its owner's process.
        // Clearing the spill area on boot is therefore the correct GC, not
        // merely a convenient one.
        if root.exists() {
            let _ = std::fs::remove_dir_all(&root);
        }
        std::fs::create_dir_all(root.join("blobs"))?;
        Ok(Self {
            root,
            ram_budget: ram_mb.saturating_mul(1024 * 1024),
            ttl,
            inner: Arc::new(Mutex::new(Inner::default())),
        })
    }

    fn blob_path(&self, hash: &ChunkHash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join("blobs").join(&hex[..2]).join(&hex)
    }

    /// Store one chunk: in RAM while the budget allows, on disk after.
    fn store_chunk(&self, hash: ChunkHash, data: &[u8]) -> Result<()> {
        {
            let mut inner = self.inner.lock();
            if inner.ram.chunks.contains_key(&hash) {
                return Ok(());
            }
            let len = data.len() as u64;
            if inner.ram.bytes.saturating_add(len) <= self.ram_budget {
                inner.ram.bytes += len;
                inner.ram.chunks.insert(hash, Arc::new(data.to_vec()));
                return Ok(());
            }
        }
        let path = self.blob_path(&hash);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A private temp name per writer, then an atomic rename — two
        // lanes spilling the same chunk at the same moment must not
        // rename the file out from under each other.
        let tmp = crate::content::temp_path(&path);
        std::fs::write(&tmp, data)?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// One chunk's bytes, RAM first. `None` means this node does not have
    /// it, which is an ordinary answer — objects are k=1, so most nodes do
    /// not have most chunks.
    pub fn read_chunk(&self, hash: &ChunkHash) -> Option<Vec<u8>> {
        if let Some(bytes) = self.inner.lock().ram.chunks.get(hash) {
            return Some(bytes.as_ref().clone());
        }
        std::fs::read(self.blob_path(hash)).ok()
    }

    /// Ingest bytes as a new object owned by `job`.
    pub fn put(&self, job: JobId, bytes: &[u8]) -> Result<ObjectRef> {
        let mut chunks = Vec::new();
        for piece in bytes.chunks(CHUNK_SIZE) {
            let hash = hash_chunk(piece);
            self.store_chunk(hash, piece)?;
            chunks.push(hash);
        }
        self.record(job, chunks, bytes.len() as u64)
    }

    /// Ingest a file an item wrote, without ever holding all of it in
    /// memory. This is the path a multi-GB map output takes.
    pub fn put_file(&self, job: JobId, path: &Path) -> Result<ObjectRef> {
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut chunks = Vec::new();
        let mut size = 0u64;
        loop {
            let mut filled = 0;
            while filled < CHUNK_SIZE {
                let n = file.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let hash = hash_chunk(&buf[..filled]);
            self.store_chunk(hash, &buf[..filled])?;
            chunks.push(hash);
            size += filled as u64;
            if filled < CHUNK_SIZE {
                break;
            }
        }
        self.record(job, chunks, size)
    }

    /// [`Self::put_file`] off the async workers (ENG-26).
    ///
    /// **The one that ingests a multi-gigabyte map output.** It opens,
    /// reads, hashes and writes the whole file synchronously, and every
    /// caller is inside an `async fn` — so for as long as it runs, one of
    /// the runtime's worker threads is doing none of the gossip, none of
    /// the job supervision and none of the request serving it is there
    /// for. On a node with a modest worker count a single large output
    /// stalls a measurable fraction of the daemon.
    ///
    /// The store is `Clone` over an `Arc`, so this costs a clone and a
    /// hop, and the blocking pool is exactly where a file this size
    /// belongs.
    pub async fn put_file_off_thread(&self, job: JobId, path: &Path) -> Result<ObjectRef> {
        let store = self.clone();
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || store.put_file(job, &path))
            .await
            .map_err(|e| crate::error::CoreError::Job(format!("object ingest panicked: {e}")))?
    }

    fn record(&self, job: JobId, chunks: Vec<ChunkHash>, size: u64) -> Result<ObjectRef> {
        let manifest = ObjectManifest { chunks, size };
        let id = manifest.object_id();
        self.reference(id, manifest, job);
        Ok(ObjectRef {
            id,
            size,
            // Filled in by the caller, which knows its own node id; the
            // store deliberately does not, so it stays a pure storage
            // component that a test can drive without a mesh.
            holder: NodeId([0; 32]),
        })
    }

    /// Take (or add) a reference to an object on `job`'s behalf.
    fn reference(&self, id: ObjectId, manifest: ObjectManifest, job: JobId) {
        let mut inner = self.inner.lock();
        let held = inner.held.entry(id).or_insert_with(|| Held {
            manifest,
            jobs: BTreeSet::new(),
            touched: Instant::now(),
            pins: 0,
            pin_until: None,
        });
        if held.jobs.insert(job) {
            held.touched = Instant::now();
        }
    }

    /// Adopt a manifest fetched from elsewhere, making this node a holder
    /// too. Called after every chunk has landed, so the entry never claims
    /// more than the store actually has.
    pub fn adopt(&self, job: JobId, manifest: ObjectManifest) -> ObjectId {
        let id = manifest.object_id();
        self.reference(id, manifest, job);
        id
    }

    pub fn manifest(&self, id: &ObjectId) -> Option<ObjectManifest> {
        self.inner.lock().held.get(id).map(|h| h.manifest.clone())
    }

    pub fn has(&self, id: &ObjectId) -> bool {
        self.inner.lock().held.contains_key(id)
    }

    /// Whether every chunk of a manifest is present here. A partially
    /// fetched object is not an object.
    pub fn complete(&self, manifest: &ObjectManifest) -> bool {
        manifest.chunks.iter().all(|c| self.has_chunk(c))
    }

    pub fn has_chunk(&self, hash: &ChunkHash) -> bool {
        self.inner.lock().ram.chunks.contains_key(hash) || self.blob_path(hash).exists()
    }

    /// Write a fetched chunk in, verifying it against its claimed hash —
    /// never trust a peer, the same rule the bundle store applies.
    pub fn accept_chunk(&self, hash: &ChunkHash, data: &[u8]) -> Result<()> {
        if &hash_chunk(data) != hash {
            return Err(CoreError::Store("object chunk hash mismatch".into()));
        }
        self.store_chunk(*hash, data)
    }

    /// Reassemble an object into memory. For anything large prefer
    /// [`Self::write_to`], which never holds the whole thing at once.
    pub fn read(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let manifest = self
            .manifest(id)
            .ok_or_else(|| CoreError::Store(format!("object not held: {}", id.short())))?;
        let mut out = Vec::with_capacity(manifest.size as usize);
        for chunk in &manifest.chunks {
            let bytes = self
                .read_chunk(chunk)
                .ok_or_else(|| CoreError::Store(format!("missing object chunk {chunk:?}")))?;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    /// Stream an object into a file, one chunk at a time.
    pub fn write_to(&self, id: &ObjectId, dest: &Path) -> Result<u64> {
        use std::io::Write;
        let manifest = self
            .manifest(id)
            .ok_or_else(|| CoreError::Store(format!("object not held: {}", id.short())))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(dest)?;
        for chunk in &manifest.chunks {
            let bytes = self
                .read_chunk(chunk)
                .ok_or_else(|| CoreError::Store(format!("missing object chunk {chunk:?}")))?;
            file.write_all(&bytes)?;
        }
        file.flush()?;
        Ok(manifest.size)
    }

    /// Every object held here, newest first.
    pub fn rows(&self) -> Vec<ObjectRow> {
        let inner = self.inner.lock();
        let awaiting: BTreeSet<ObjectId> = inner
            .uncollected
            .iter()
            .flat_map(|(_, ids, _)| ids.iter().copied())
            .collect();
        let mut rows: Vec<ObjectRow> = inner
            .held
            .iter()
            .map(|(id, held)| ObjectRow {
                id: *id,
                // The first owner, for a table cell. `jobs` is the real
                // answer, and nothing outside the store needs it.
                job: held.jobs.iter().copied().next().unwrap_or(JobId([0; 16])),
                size: held.manifest.size,
                // Distinct chunks only: a file of repeated blocks is one
                // stored chunk referenced many times, and counting it once
                // per reference would report more resident bytes than the
                // object even has.
                resident: held
                    .manifest
                    .chunks
                    .iter()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .filter_map(|c| inner.ram.chunks.get(c))
                    .map(|b| b.len() as u64)
                    .sum(),
                age_secs: held.touched.elapsed().as_secs(),
                awaiting_collection: awaiting.contains(id),
            })
            .collect();
        rows.sort_by_key(|r| r.age_secs);
        rows
    }

    /// Objects held here for `job`, and their total size — what makes a
    /// node the natural place to run a reduce over them (M9 §5.5).
    /// Keep these bytes past their job's lifetime (C7 P3).
    ///
    /// Returns how many of the ids were actually held here — a publisher
    /// that pins an object it does not have has a bug, and a silent no-op
    /// would hide it until somebody's cache hit failed to materialise.
    pub fn pin(&self, ids: &[ObjectId], lease: Duration) -> usize {
        let until = Instant::now() + lease;
        let mut inner = self.inner.lock();
        let mut pinned = 0;
        for id in ids {
            if let Some(held) = inner.held.get_mut(id) {
                held.pins = held.pins.saturating_add(1);
                // The longest promise wins: two entries backed by the same
                // bytes are kept until the later one lapses.
                held.pin_until = Some(match held.pin_until {
                    Some(at) => at.max(until),
                    None => until,
                });
                pinned += 1;
            }
        }
        pinned
    }

    /// Renew the lease on pinned objects that are still being read
    /// (ENG-19). A no-op for anything not pinned.
    ///
    /// This is the half that makes a lease a lease rather than a timer: the
    /// bytes behind an entry somebody keeps hitting are kept, and the node
    /// needs nobody's permission and no message to know it.
    pub fn renew_pins(&self, ids: &[ObjectId], lease: Duration) {
        let until = Instant::now() + lease;
        let mut inner = self.inner.lock();
        for id in ids {
            if let Some(held) = inner.held.get_mut(id)
                && held.pins > 0
            {
                held.pin_until = Some(match held.pin_until {
                    Some(at) => at.max(until),
                    None => until,
                });
            }
        }
    }

    /// Release a pin. The bytes go on the *next* sweep, on the ordinary
    /// refcount rules, rather than immediately: the job that produced them
    /// may still be running, and an eviction is not a reason to delete an
    /// intermediate somebody is reading.
    pub fn unpin(&self, ids: &[ObjectId]) {
        let mut inner = self.inner.lock();
        for id in ids {
            if let Some(held) = inner.held.get_mut(id) {
                held.pins = held.pins.saturating_sub(1);
            }
        }
    }

    /// How many objects this node is holding for the cache.
    /// Hold a finished run's results until somebody collects them (R3 P3).
    ///
    /// **The bug this closes.** An object outlives its job by
    /// [`DEFAULT_TTL`] — fifteen minutes — which is right for an
    /// intermediate that a reduce is about to consume and wrong for the
    /// last stage of a run somebody is watching. Ticking "keep each item's
    /// output in the mesh" on a four-hundred-file transcode and going to
    /// make coffee lost the encode. A result nobody has fetched is not an
    /// intermediate; it is the answer.
    ///
    /// Bounded, because pinning without a bound turns a store with a
    /// ceiling into one without. Past `ceiling_bytes` the **oldest**
    /// retained runs are released back to the ordinary TTL and named in the
    /// return, so the caller can say so out loud rather than letting a disk
    /// fill quietly. Releasing the oldest is the right way round: the run
    /// you are still looking at is the newest one.
    pub fn retain_results(&self, job: JobId, ids: &[ObjectId], ceiling_bytes: u64) -> Vec<JobId> {
        let mut inner = self.inner.lock();
        let mut bytes = 0u64;
        let mut kept = Vec::new();
        for id in ids {
            if let Some(held) = inner.held.get_mut(id) {
                held.pins = held.pins.saturating_add(1);
                bytes = bytes.saturating_add(held.manifest.size);
                kept.push(*id);
            }
        }
        if kept.is_empty() {
            return Vec::new();
        }
        inner.uncollected.push((job, kept, bytes));

        let mut evicted = Vec::new();
        while inner.uncollected.iter().map(|(_, _, b)| *b).sum::<u64>() > ceiling_bytes
            && inner.uncollected.len() > 1
        {
            let (old_job, ids, _) = inner.uncollected.remove(0);
            for id in &ids {
                if let Some(held) = inner.held.get_mut(id) {
                    held.pins = held.pins.saturating_sub(1);
                }
            }
            evicted.push(old_job);
        }
        evicted
    }

    /// Somebody collected them: let the ordinary TTL have them back.
    pub fn release_results(&self, job: &JobId) -> usize {
        let mut inner = self.inner.lock();
        let Some(i) = inner.uncollected.iter().position(|(j, _, _)| j == job) else {
            return 0;
        };
        let (_, ids, _) = inner.uncollected.remove(i);
        let n = ids.len();
        for id in &ids {
            if let Some(held) = inner.held.get_mut(id) {
                held.pins = held.pins.saturating_sub(1);
            }
        }
        n
    }

    /// Runs whose results are still waiting to be collected: job, objects,
    /// bytes. Newest last, which is the order they were retained in.
    pub fn uncollected(&self) -> Vec<(JobId, usize, u64)> {
        self.inner
            .lock()
            .uncollected
            .iter()
            .map(|(j, ids, b)| (*j, ids.len(), *b))
            .collect()
    }

    pub fn pinned(&self) -> usize {
        self.inner
            .lock()
            .held
            .values()
            .filter(|h| h.pins > 0)
            .count()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reclaim objects whose owning job is over (M9 §5.5).
    ///
    /// `job_state` reports what replicated state knows about a job:
    /// `Some(true)` = terminal, `Some(false)` = still running, `None` =
    /// this node has never heard of it. Terminal jobs are reclaimed after
    /// the TTL, unknown ones only after a much longer grace — because
    /// "unknown" is nearly always a job whose summary has not gossiped here
    /// yet, and reclaiming those would delete live intermediates.
    ///
    /// Returns how many objects were released.
    pub fn sweep(&self, job_state: impl Fn(&JobId) -> Option<bool>) -> usize {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        // Lapsed leases are dropped first, so the object is judged on the
        // ordinary refcount rules and `pinned()` stops counting it.
        for held in inner.held.values_mut() {
            if held.pins > 0 && held.pin_until.is_some_and(|at| at <= now) {
                held.pins = 0;
                held.pin_until = None;
            }
        }
        let doomed: Vec<ObjectId> = inner
            .held
            .iter()
            .filter(|(_, held)| {
                // A pinned object is a promise somebody else is keeping
                // (C7 P3). It leaves when the last cache entry referencing
                // it is evicted — or when the promise lapses, because the
                // eviction happens on a node that cannot tell this one
                // (ENG-19). An expired pin is not a promise; it is a leak
                // with a comment on it.
                if held.pins > 0 && held.pin_until.is_none_or(|at| at > now) {
                    return false;
                }
                let age = held.touched.elapsed();
                // *Every* owner must be finished with it. One live
                // reference keeps the object, however long ago the others
                // ended — which is what makes this a refcount rather than
                // a timer with extra steps.
                held.jobs.iter().all(|job| match job_state(job) {
                    Some(true) => age >= self.ttl,
                    Some(false) => false,
                    None => age >= UNKNOWN_GRACE,
                })
            })
            .map(|(id, _)| *id)
            .collect();
        if doomed.is_empty() {
            return 0;
        }
        // A chunk may belong to more than one object — two items producing
        // identical output is the common case for a validation pass — so a
        // chunk only goes when the last object referencing it does.
        let mut released = 0usize;
        for id in &doomed {
            let Some(held) = inner.held.remove(id) else {
                continue;
            };
            released += 1;
            for chunk in &held.manifest.chunks {
                let still_referenced = inner
                    .held
                    .values()
                    .any(|other| other.manifest.chunks.contains(chunk));
                if still_referenced {
                    continue;
                }
                if let Some(bytes) = inner.ram.chunks.remove(chunk) {
                    inner.ram.bytes = inner.ram.bytes.saturating_sub(bytes.len() as u64);
                } else {
                    let _ = std::fs::remove_file(self.blob_path(chunk));
                }
            }
        }
        debug!(released, "swept ephemeral objects");
        released
    }
}

// ---------------------------------------------------------------------------
// The directory
// ---------------------------------------------------------------------------

/// Where an object lives, as one directory node knows it.
#[derive(Debug, Clone)]
struct DirEntry {
    /// Every job that referenced it, for the same reason the store keeps a
    /// set: identical output from two jobs is one object with two owners.
    jobs: BTreeSet<JobId>,
    size: u64,
    holders: BTreeSet<NodeId>,
    touched: Instant,
}

/// The rendezvous-hashed object directory (M9 §5.5).
///
/// Every node runs one. Which node answers for a given object is decided by
/// [`directory_for`], which every node computes identically from the object
/// id — so "where is this?" costs no coordination, no election and no
/// central index. The op-log is deliberately not involved: per-object
/// records would be exactly the time-series-in-a-state-store mistake §3.4
/// rules out, at a thousand times the volume of the job records that do
/// belong there.
#[derive(Debug, Default)]
pub struct ObjectDirectory {
    entries: Mutex<HashMap<ObjectId, DirEntry>>,
    ttl: Duration,
}

impl ObjectDirectory {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Record that `holder` has `object`. Idempotent, and additive: two
    /// nodes that both produced identical output are both holders, which
    /// is free redundancy nobody had to ask for.
    pub fn record(&self, object: ObjectId, job: JobId, size: u64, holder: NodeId) {
        let mut entries = self.entries.lock();
        let entry = entries.entry(object).or_insert_with(|| DirEntry {
            jobs: BTreeSet::new(),
            size,
            holders: BTreeSet::new(),
            touched: Instant::now(),
        });
        if entry.jobs.insert(job) {
            entry.touched = Instant::now();
        }
        entry.holders.insert(holder);
    }

    /// Who holds this object. Empty is a real answer — the directory does
    /// not know — and the caller's response to it is to re-run the task
    /// that made the object, not to look harder (M9 §10, Q3).
    pub fn holders(&self, object: &ObjectId) -> Vec<ObjectRef> {
        let entries = self.entries.lock();
        let Some(entry) = entries.get(object) else {
            return Vec::new();
        };
        entry
            .holders
            .iter()
            .map(|holder| ObjectRef {
                id: *object,
                size: entry.size,
                holder: *holder,
            })
            .collect()
    }

    /// Which job owns this object, if this directory indexes it. A node
    /// that fetches an object on someone's behalf ties its local copy to
    /// the same job, so the copy is swept on the same schedule as the
    /// original rather than outliving it.
    pub fn owner_job(&self, object: &ObjectId) -> Option<JobId> {
        self.entries
            .lock()
            .get(object)
            .and_then(|entry| entry.jobs.iter().copied().next())
    }

    /// Drop a node from every entry it appears in — it is gone, and
    /// pointing fetchers at it wastes a dial each time.
    pub fn forget_holder(&self, holder: &NodeId) {
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| {
            entry.holders.remove(holder);
            !entry.holders.is_empty()
        });
    }

    /// Same rule as the store's sweep, on the index side.
    pub fn sweep(&self, job_state: impl Fn(&JobId) -> Option<bool>) -> usize {
        let mut entries = self.entries.lock();
        let before = entries.len();
        entries.retain(|_, entry| {
            let age = entry.touched.elapsed();
            !entry.jobs.iter().all(|job| match job_state(job) {
                Some(true) => age >= self.ttl,
                Some(false) => false,
                None => age >= UNKNOWN_GRACE,
            })
        });
        before - entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Which nodes hold the directory entry for `object`.
///
/// Rendezvous ranking over the live members, first [`DIRECTORY_REPLICAS`].
/// The bytes are k=1 — recomputing a lost object beats replicating every
/// one — but the *entry* is a hundred bytes, and losing it would strand
/// bytes that still exist, so the index alone is replicated (M9 §5.5).
pub fn directory_for(object: &ObjectId, members: impl IntoIterator<Item = NodeId>) -> Vec<NodeId> {
    let mut ranked = crate::scheduler::rendezvous_rank(OBJDIR_CONTEXT, &object.0, members);
    ranked.truncate(DIRECTORY_REPLICAS);
    ranked
}

/// Group a batch of objects by the directory node that indexes them, so a
/// chunk of a thousand items costs one announce per directory node rather
/// than a thousand round trips.
pub fn group_by_directory(
    objects: &[(ObjectId, u64)],
    members: &[NodeId],
) -> BTreeMap<NodeId, Vec<(ObjectId, u64)>> {
    let mut grouped: BTreeMap<NodeId, Vec<(ObjectId, u64)>> = BTreeMap::new();
    for (id, size) in objects {
        for node in directory_for(id, members.iter().copied()) {
            grouped.entry(node).or_default().push((*id, *size));
        }
    }
    grouped
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// ENG-19 — a pin the producer takes has no way to be released by
    /// anyone who can reach it. The entry lives on a *directory* node
    /// chosen by rendezvous hashing; that node evicts it and unpins an id
    /// it does not hold, and there is no message that tells the producer.
    /// So the pin lapses on its own, and is renewed by use.
    #[test]
    fn a_pin_nobody_renews_lapses_and_the_bytes_go() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ObjectStore::open(dir.path(), 0, Duration::from_secs(0)).expect("open");
        let terminal = |_: &JobId| Some(true);

        let job = JobId::generate();
        let obj = store.put(job, &vec![7u8; 1000]).expect("put");

        // A live lease keeps the bytes past the job that made them, which
        // is the whole reason pinning exists.
        assert_eq!(store.pin(&[obj.id], Duration::from_secs(3600)), 1);
        store.sweep(terminal);
        assert!(store.has(&obj.id), "a live pin must keep its object");
        assert_eq!(store.pinned(), 1);

        // An expired one does not. The cache entry may outlive its bytes —
        // that is a miss, and a cache is allowed to miss. Bytes nothing can
        // release are what it is not allowed to have.
        let stale = store.put(job, &vec![8u8; 1000]).expect("put");
        assert_eq!(store.pin(&[stale.id], Duration::from_millis(1)), 1);
        std::thread::sleep(Duration::from_millis(20));
        store.sweep(terminal);
        assert!(!store.has(&stale.id), "a lapsed pin must not hold bytes");
    }

    /// The half that makes it a lease rather than a timer: bytes behind an
    /// entry people still hit are kept, and the node works that out for
    /// itself from the reads it serves.
    #[test]
    fn reading_a_pinned_object_renews_its_lease() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ObjectStore::open(dir.path(), 0, Duration::from_secs(0)).expect("open");
        let terminal = |_: &JobId| Some(true);

        let job = JobId::generate();
        let obj = store.put(job, &vec![9u8; 1000]).expect("put");
        store.pin(&[obj.id], Duration::from_millis(30));
        // Somebody asks for it before the lease runs out.
        std::thread::sleep(Duration::from_millis(10));
        store.renew_pins(&[obj.id], Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(40));

        store.sweep(terminal);
        assert!(
            store.has(&obj.id),
            "an object somebody is still reading must not be reclaimed"
        );

        // Renewal touches only what is pinned: an unpinned object is not
        // quietly given a lease by being read.
        let loose = store.put(job, &vec![10u8; 1000]).expect("put");
        store.renew_pins(&[loose.id], Duration::from_secs(3600));
        store.sweep(terminal);
        assert!(!store.has(&loose.id), "renew_pins must not create a pin");
    }

    #[test]
    fn a_finished_runs_results_wait_for_collection_and_the_wait_is_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Everything on disk, so `size` is the only thing being counted, and
        // a TTL of zero so the sweep is the fifteen-minute clock in
        // miniature: it takes anything unpinned the moment it is asked.
        let store = ObjectStore::open(dir.path(), 0, Duration::from_secs(0)).expect("open");
        let terminal = |_: &JobId| Some(true);
        let put = |byte: u8| {
            let job = JobId::generate();
            let obj = store.put(job, &vec![byte; 1000]).expect("put");
            (job, obj.id)
        };

        // Retained: held, however aggressive the sweep is. Without this the
        // result of a finished run is gone fifteen minutes later, which is
        // how a transcode gets lost.
        let (job_a, a) = put(1);
        store.retain_results(job_a, &[a], 10_000);
        store.sweep(terminal);
        assert!(store.has(&a), "a retained result was swept");

        // The ceiling releases the *oldest* — the run you are still looking
        // at is the newest — and names it, so the caller can say so out loud
        // instead of letting a disk fill quietly.
        let (job_b, b) = put(2);
        let evicted = store.retain_results(job_b, &[b], 1_500);
        assert_eq!(evicted, vec![job_a]);
        assert_eq!(store.uncollected().len(), 1);
        store.sweep(terminal);
        assert!(
            !store.has(&a),
            "the evicted result should be sweepable again"
        );
        assert!(store.has(&b), "the newest result must survive the ceiling");

        // Collected: back on the ordinary clock.
        let (job_c, c) = put(3);
        store.retain_results(job_c, &[c], 10_000);
        assert_eq!(store.release_results(&job_c), 1);
        store.sweep(terminal);
        assert!(!store.has(&c));

        // Idempotent: releasing twice, or releasing a run that retained
        // nothing, is not an error — a retry of a partial fetch must not
        // fail on the half that already worked.
        assert_eq!(store.release_results(&job_c), 0);
        assert_eq!(store.release_results(&JobId::generate()), 0);
    }

    fn store() -> (ObjectStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ObjectStore::open(dir.path(), 1, DEFAULT_TTL).expect("open");
        (store, dir)
    }

    #[test]
    fn an_object_round_trips_through_the_store() {
        let (store, _dir) = store();
        let job = JobId::generate();
        let payload = b"the output of one item".to_vec();
        let obj = store.put(job, &payload).unwrap();
        assert_eq!(obj.size, payload.len() as u64);
        assert_eq!(store.read(&obj.id).unwrap(), payload);
        // Content addressing: the same bytes are the same object, so a
        // re-executed task recreates what it lost rather than duplicating.
        let again = store.put(job, &payload).unwrap();
        assert_eq!(again.id, obj.id);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn large_objects_spill_to_disk_and_still_read_back() {
        // 1 MB budget, 3 MB of payload: most of it must land on disk.
        // Each megabyte differs, or the three would dedupe to one chunk
        // and there would be nothing to spill.
        let (store, _dir) = store();
        let job = JobId::generate();
        let mut payload = Vec::with_capacity(3 * CHUNK_SIZE);
        for block in 0..3u8 {
            payload.extend(std::iter::repeat_n(0xA0 | block, CHUNK_SIZE));
        }
        let obj = store.put(job, &payload).unwrap();
        let resident = store.rows()[0].resident;
        assert!(
            resident < obj.size,
            "a 3 MB object must not sit entirely in a 1 MB RAM budget \
             (resident {resident} of {})",
            obj.size
        );
        assert_eq!(store.read(&obj.id).unwrap(), payload);
    }

    #[test]
    fn repeated_blocks_are_stored_once_and_counted_once() {
        // A file of identical megabytes is one chunk referenced three
        // times. Reporting it as three would claim more resident bytes
        // than the object has.
        let (store, _dir) = store();
        let job = JobId::generate();
        let obj = store.put(job, &vec![0xABu8; 3 * CHUNK_SIZE]).unwrap();
        let row = &store.rows()[0];
        assert_eq!(obj.size, 3 * CHUNK_SIZE as u64);
        assert!(
            row.resident <= obj.size,
            "resident cannot exceed the object"
        );
        assert_eq!(store.read(&obj.id).unwrap().len(), 3 * CHUNK_SIZE);
    }

    #[test]
    fn a_file_becomes_an_object_without_being_read_whole() {
        let (store, dir) = store();
        let path = dir.path().join("item-output");
        std::fs::write(&path, vec![7u8; CHUNK_SIZE + 11]).unwrap();
        let job = JobId::generate();
        let obj = store.put_file(job, &path).unwrap();
        assert_eq!(obj.size, (CHUNK_SIZE + 11) as u64);
        let dest = dir.path().join("fetched");
        assert_eq!(store.write_to(&obj.id, &dest).unwrap(), obj.size);
        assert_eq!(std::fs::read(&dest).unwrap().len(), CHUNK_SIZE + 11);
    }

    #[test]
    fn the_sweep_releases_finished_jobs_and_spares_running_ones() {
        let dir = tempfile::tempdir().unwrap();
        // Zero TTL so "terminal" means "reclaimable now".
        let store = ObjectStore::open(dir.path(), 8, Duration::ZERO).unwrap();
        let done = JobId::generate();
        let running = JobId::generate();
        store.put(done, b"finished work").unwrap();
        store.put(running, b"work in flight").unwrap();
        assert_eq!(store.len(), 2);

        let released = store.sweep(|job| Some(*job == done));
        assert_eq!(released, 1);
        assert_eq!(store.len(), 1, "the running job keeps its object");

        // An unknown job is *not* reclaimed on sight: that is nearly
        // always gossip lag, and deleting live intermediates over it would
        // be the worst possible reading of a missing record.
        assert_eq!(store.sweep(|_| None), 0);
    }

    #[test]
    fn an_object_two_jobs_share_outlives_the_first_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(dir.path(), 8, Duration::ZERO).unwrap();
        let (a, b) = (JobId::generate(), JobId::generate());
        // Two jobs, identical output — the common case for a validation
        // pass, where every item prints "ok". Content addressing makes
        // that *one* object with two owners.
        let one = store.put(a, b"ok").unwrap();
        let two = store.put(b, b"ok").unwrap();
        assert_eq!(one.id, two.id, "identical bytes are one object");
        assert_eq!(store.len(), 1);

        // Job `a` finishing must not take it: job `b` is still running.
        assert_eq!(store.sweep(|job| Some(*job != b)), 0);
        assert_eq!(store.read(&one.id).unwrap(), b"ok");

        // Both finished, and now it goes.
        assert_eq!(store.sweep(|_| Some(true)), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn the_directory_is_rendezvous_ranked_and_replicated_twice() {
        let members: Vec<NodeId> = (1..=5).map(|i| NodeId([i; 32])).collect();
        let object = ObjectManifest {
            chunks: vec![ChunkHash([3; 32])],
            size: 10,
        }
        .object_id();
        let a = directory_for(&object, members.clone());
        let b = directory_for(&object, members.clone());
        assert_eq!(a, b, "every node must compute the same directory");
        assert_eq!(a.len(), DIRECTORY_REPLICAS);
        // Different objects land on different nodes: that is what spreads
        // the index instead of concentrating it.
        let other = ObjectManifest {
            chunks: vec![ChunkHash([4; 32])],
            size: 10,
        }
        .object_id();
        let spread: BTreeSet<NodeId> = a
            .iter()
            .chain(directory_for(&other, members).iter())
            .copied()
            .collect();
        assert!(spread.len() > DIRECTORY_REPLICAS.min(2));
    }

    #[test]
    fn a_batch_is_grouped_so_each_directory_node_is_told_once() {
        let members: Vec<NodeId> = (1..=3).map(|i| NodeId([i; 32])).collect();
        let objects: Vec<(ObjectId, u64)> = (0..20u8)
            .map(|i| {
                (
                    ObjectManifest {
                        chunks: vec![ChunkHash([i; 32])],
                        size: 1,
                    }
                    .object_id(),
                    1,
                )
            })
            .collect();
        let grouped = group_by_directory(&objects, &members);
        assert!(grouped.len() <= members.len(), "one announce per node, max");
        let announced: usize = grouped.values().map(|v| v.len()).sum();
        assert_eq!(
            announced,
            objects.len() * DIRECTORY_REPLICAS,
            "every object is indexed on exactly its replica set"
        );
    }

    #[test]
    fn a_dead_holder_is_dropped_from_the_directory() {
        let directory = ObjectDirectory::new(DEFAULT_TTL);
        let job = JobId::generate();
        let object = ObjectManifest {
            chunks: vec![ChunkHash([1; 32])],
            size: 4,
        }
        .object_id();
        let gone = NodeId([1; 32]);
        let alive = NodeId([2; 32]);
        directory.record(object, job, 4, gone);
        directory.record(object, job, 4, alive);
        assert_eq!(directory.owner_job(&object), Some(job));
        assert_eq!(directory.holders(&object).len(), 2);
        directory.forget_holder(&gone);
        let left = directory.holders(&object);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].holder, alive);
        // The last holder going takes the entry with it: an entry naming
        // nobody is worse than no entry, because it looks like an answer.
        directory.forget_holder(&alive);
        assert!(directory.is_empty());
    }
}
