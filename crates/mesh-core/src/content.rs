//! Content-addressed bundle store (MASTERPLAN §8.1, §11).
//!
//! On-disk layout under `<data_dir>/content/`:
//!   blobs/<hex[0:2]>/<hex>      — one file per chunk, named by BLAKE3 hash
//!   manifests/<bundle-hex>      — postcard-encoded Manifest per bundle
//!
//! Chunking is fixed-size (1 MiB) for M2 — simple and correct; content-defined
//! chunking for better dedup is a later refinement. Hashing is BLAKE3's
//! single-shot `hash`, which is already SIMD-accelerated; the multithreaded
//! path needs an explicit `Hasher::update_rayon` and is not used (L-11).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mesh_proto::{BundleId, BundleMeta, ChunkHash, FileEntry, Manifest};
use tracing::debug;
use walkdir::WalkDir;

use crate::error::{CoreError, Result};

/// Fixed chunk size. Must stay well under `mesh_proto::MAX_FRAME_LEN` so a
/// chunk fits in a single wire frame.
pub const CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ContentStore {
    root: PathBuf,
}

fn hash_chunk(bytes: &[u8]) -> ChunkHash {
    ChunkHash(*blake3::hash(bytes).as_bytes())
}

/// Distinguishes concurrent writers of the *same* chunk.
///
/// Two lanes fetching one bundle at the same moment write the same content
/// address, and a shared `<hash>.tmp` meant one of them renamed the file
/// out from under the other — which surfaced as a bare `ENOENT` from a
/// chunk fetch, in a fraction of runs, with nothing in the message to say
/// what had happened. Every write gets its own temp name instead; the
/// rename onto the final path stays the atomic step.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub(crate) fn temp_path(final_path: &Path) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut name = final_path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{pid}.{seq}.tmp"));
    final_path.with_file_name(name)
}

impl ContentStore {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let root = data_dir.join("content");
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("manifests"))?;
        Ok(Self { root })
    }

    fn blob_path(&self, hash: &ChunkHash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join("blobs").join(&hex[..2]).join(&hex)
    }

    fn manifest_path(&self, id: &BundleId) -> PathBuf {
        self.root
            .join("manifests")
            .join(data_encoding::HEXLOWER.encode(&id.0))
    }

    pub fn has_chunk(&self, hash: &ChunkHash) -> bool {
        self.blob_path(hash).exists()
    }

    /// A bundle is fully local iff its manifest and every referenced chunk
    /// are present.
    pub fn has_bundle(&self, id: &BundleId) -> bool {
        match self.load_manifest(id) {
            Ok(Some(m)) => m.distinct_chunks().iter().all(|c| self.has_chunk(c)),
            _ => false,
        }
    }

    pub fn write_chunk(&self, hash: &ChunkHash, data: &[u8]) -> Result<()> {
        // Verify the bytes match the claimed hash — never trust a peer.
        if &hash_chunk(data) != hash {
            return Err(CoreError::Store("chunk hash mismatch on write".into()));
        }
        let path = self.blob_path(hash);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a *private* temp file then rename for atomicity.
        let tmp = temp_path(&path);
        std::fs::write(&tmp, data)?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    pub fn read_chunk(&self, hash: &ChunkHash) -> Result<Option<Vec<u8>>> {
        let path = self.blob_path(hash);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(std::fs::read(path)?))
    }

    pub fn save_manifest(&self, manifest: &Manifest) -> Result<BundleId> {
        // The first of the two checks (S2). A manifest reaches this from
        // `content_sync` as well as from local ingestion, so refusing to
        // *store* a hostile file list keeps the bad bundle from ever
        // becoming a thing this node holds and serves onward.
        manifest.validate().map_err(CoreError::Store)?;
        let id = manifest.bundle_id();
        let path = self.manifest_path(&id);
        // Atomic for the same reason chunks are: a concurrent reader must
        // see the whole manifest or none of it, never a half-written one.
        let tmp = temp_path(&path);
        std::fs::write(&tmp, manifest.canonical_bytes())?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(id)
    }

    pub fn load_manifest(&self, id: &BundleId) -> Result<Option<Manifest>> {
        let path = self.manifest_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(path)?;
        let manifest: Manifest = postcard::from_bytes(&bytes)
            .map_err(|e| CoreError::Store(format!("decode manifest: {e}")))?;
        // Integrity: the stored manifest must still hash to the id.
        if manifest.bundle_id() != *id {
            return Err(CoreError::Store("manifest id mismatch".into()));
        }
        // And safety, which the hash says nothing about: the id is the hash
        // *of the manifest*, so a crafted file list is a correct id for a
        // hostile bundle. A store written before this check existed is the
        // reason it is here and not only on the way in.
        manifest.validate().map_err(CoreError::Store)?;
        Ok(Some(manifest))
    }

    pub fn meta(&self, manifest: &Manifest) -> BundleMeta {
        BundleMeta {
            bundle_id: manifest.bundle_id(),
            total_size: manifest.total_size(),
            file_count: u32::try_from(manifest.files.len()).unwrap_or(u32::MAX),
            chunk_count: u32::try_from(manifest.distinct_chunks().len()).unwrap_or(u32::MAX),
        }
    }

    /// Ingest whatever a person pointed at — a directory tree, or a single
    /// file — into the store, returning its manifest.
    ///
    /// A single file becomes a one-file bundle rather than an error. It used
    /// to be an error, and the cost was not obvious from here: Director's
    /// "drop a script on the window" gesture pushes exactly the path it was
    /// given, so dropping `burn-in.sh` produced *"…/burn-in.sh is not a
    /// directory"* and nothing was imported at all. `meshnode add` had
    /// worked around it by copying the file into a temporary directory
    /// first, which is why the CLI door worked and the GUI door did not.
    ///
    /// Ingesting the file's own directory instead would be wrong for the
    /// reason that workaround existed: a script in a home directory would
    /// sweep up the home directory. The one exception is the script's
    /// [`definitions file`], which is not a neighbour that happens to be
    /// nearby — it is half of the thing being pushed, and a bundle that
    /// arrived without it would register the empty form this all exists to
    /// prevent.
    ///
    /// [`definitions file`]: crate::script_spec::definitions_path
    pub fn ingest_path(&self, path: &Path) -> Result<Manifest> {
        if path.is_dir() {
            return self.ingest_dir(path);
        }
        if !path.is_file() {
            return Err(CoreError::Store(format!(
                "{} is neither a file nor a directory",
                path.display()
            )));
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CoreError::Store(format!("non-UTF8 path: {}", path.display())))?
            .to_string();

        let mut files = Vec::new();
        let mut add = |p: &Path, name: String| -> Result<()> {
            let (chunks, size) = self.ingest_file(p)?;
            files.push(FileEntry {
                path: name,
                mode: file_mode(p),
                symlink: None,
                chunks,
                size,
            });
            Ok(())
        };
        add(path, name.clone())?;

        let defs_name = crate::script_spec::definitions_path(&name);
        let defs = path.with_file_name(&defs_name);
        if defs.is_file() {
            add(&defs, defs_name)?;
        }

        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Manifest { files })
    }

    /// Ingest a directory tree into the store, returning its manifest.
    /// Symlinks are recorded as links (not followed); regular files are
    /// chunked and stored. `.git` directories are always skipped — version
    /// control metadata is not automation content, and Git-source ingestion
    /// (MASTERPLAN §8.1) relies on the working tree alone determining the
    /// BundleId.
    pub fn ingest_dir(&self, dir: &Path) -> Result<Manifest> {
        if !dir.is_dir() {
            return Err(CoreError::Store(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        let mut files = Vec::new();
        for entry in WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| e.file_name() != ".git")
        {
            let entry = entry.map_err(|e| CoreError::Store(format!("walk: {e}")))?;
            let path = entry.path();
            let rel = path
                .strip_prefix(dir)
                .map_err(|e| CoreError::Store(format!("strip prefix: {e}")))?;
            if rel.as_os_str().is_empty() {
                continue; // the root itself
            }
            let rel_str = rel
                .to_str()
                .ok_or_else(|| CoreError::Store(format!("non-UTF8 path: {}", rel.display())))?
                .replace('\\', "/");
            let file_type = entry.file_type();

            if file_type.is_symlink() {
                let target = std::fs::read_link(path)?;
                let target = target.to_string_lossy().replace('\\', "/");
                // Refused here rather than at unpack, so the person
                // publishing hears about it while they can still look at
                // the file (S2). A bundle is unpacked on machines that are
                // not this one, and a link out of the tree means something
                // different on each of them — at best a broken checkout, at
                // worst a write the checkout was never meant to reach.
                mesh_proto::content::check_symlink_target(&rel_str, &target)
                    .map_err(|e| CoreError::Store(format!("{}: {e}", path.display())))?;
                files.push(FileEntry {
                    path: rel_str,
                    mode: 0o777,
                    symlink: Some(target),
                    chunks: Vec::new(),
                    size: 0,
                });
            } else if file_type.is_file() {
                let (chunks, size) = self.ingest_file(path)?;
                files.push(FileEntry {
                    path: rel_str,
                    mode: file_mode(path),
                    symlink: None,
                    chunks,
                    size,
                });
            }
            // Directories are implied by file paths; empty dirs are dropped
            // (a known limitation, acceptable for automation content).
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let manifest = Manifest { files };
        debug!(
            files = manifest.files.len(),
            chunks = manifest.distinct_chunks().len(),
            "ingested directory"
        );
        Ok(manifest)
    }

    fn ingest_file(&self, path: &Path) -> Result<(Vec<ChunkHash>, u64)> {
        let mut file = std::fs::File::open(path)?;
        let mut chunks = Vec::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let mut filled = 0;
            // Fill a full chunk (read may return short).
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
            self.write_chunk(&hash, &buf[..filled])?;
            chunks.push(hash);
            total += filled as u64;
            if filled < CHUNK_SIZE {
                break;
            }
        }
        Ok((chunks, total))
    }

    /// Reconstruct a bundle's tree into `dest`. Fails if any chunk is missing.
    ///
    /// **Nothing is written until every entry has been checked** (S2). The
    /// checkout happens in the daemon process, *before* the job's sandbox
    /// exists and outside it — so an entry reading `../../../../etc/cron.d/x`
    /// or `/home/meshnode/.ssh/authorized_keys` would walk straight past the
    /// promise the sandbox makes. `Manifest::validate` is what stops it, and
    /// it runs here as well as at `save_manifest` because a store written by
    /// an older build is exactly the case the second check is for.
    pub fn materialize(&self, id: &BundleId, dest: &Path) -> Result<()> {
        // Unpacking a bundle to run it is the clearest "this is in use"
        // signal there is, so the retention window restarts here. Note the
        // sweep reaches for `load_manifest`, never this — a reclaim that
        // counted as a use would keep alive exactly what it came to remove.
        self.touch(id);
        let manifest = self
            .load_manifest(id)
            .and_then(|m| m.ok_or_else(|| CoreError::Store("manifest not present".into())))?;
        manifest.validate().map_err(CoreError::Store)?;
        std::fs::create_dir_all(dest)?;
        // Files first, links second. `fs::write` follows a symlink and the
        // manifest's order is whatever the publisher chose, so a single pass
        // would let `link -> somewhere` earlier in the list decide where a
        // later `link/file` lands. Validation already keeps every target
        // inside the checkout, so this is belt and braces — but it costs one
        // extra pass over a list that is already in memory, and it means the
        // write phase runs against a tree with no links in it at all.
        for f in manifest.files.iter().filter(|f| f.symlink.is_none()) {
            let out = dest.join(&f.path);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut bytes = Vec::with_capacity(f.size as usize);
            for c in &f.chunks {
                let chunk = self
                    .read_chunk(c)?
                    .ok_or_else(|| CoreError::Store(format!("missing chunk {c:?}")))?;
                bytes.extend_from_slice(&chunk);
            }
            std::fs::write(&out, &bytes)?;
            set_mode(&out, f.mode);
        }
        for f in manifest.files.iter().filter(|f| f.symlink.is_some()) {
            let out = dest.join(&f.path);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            #[cfg(unix)]
            if let Some(target) = &f.symlink {
                // Best effort; ignore if it already exists.
                let _ = std::os::unix::fs::symlink(target, &out);
            }
        }
        Ok(())
    }

    /// Which chunks of this manifest are not present locally.
    pub fn missing_chunks(&self, manifest: &Manifest) -> Vec<ChunkHash> {
        manifest
            .distinct_chunks()
            .into_iter()
            .filter(|c| !self.has_chunk(c))
            .collect()
    }

    // -----------------------------------------------------------------
    // Reclaim (M31 A2)
    // -----------------------------------------------------------------

    fn used_path(&self, id: &BundleId) -> PathBuf {
        self.root
            .join("used")
            .join(data_encoding::HEXLOWER.encode(&id.0))
    }

    /// Record that something actually used this bundle, now.
    ///
    /// **A sidecar file holding the timestamp, not the manifest's mtime.**
    /// The obvious version of this is `utimensat` on the manifest, and it is
    /// wrong in two ways that only show up later: half the fleet mounts
    /// `relatime` or `noatime` and the other half does not, and a backup,
    /// an `rsync -a` or a restore rewrites every mtime in the store to the
    /// same instant — which would make a restored node think its entire
    /// content store was touched this morning and keep all of it for another
    /// window. A number this code wrote is a number this code can trust.
    ///
    /// Rate-limited to [`TOUCH_GRANULARITY`], so the serve path can call it
    /// per chunk without turning a bundle fetch into a thousand writes.
    /// Best-effort throughout: failing to record a use must never fail the
    /// use itself, and the cost of losing one is that a bundle looks
    /// slightly older than it is.
    pub fn touch(&self, id: &BundleId) {
        let path = self.used_path(id);
        let now = mesh_proto::now_micros();
        if let Some(prev) = read_micros(&path)
            && now.saturating_sub(prev) < TOUCH_GRANULARITY
        {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = temp_path(&path);
        if std::fs::write(&tmp, now.to_string().as_bytes()).is_ok()
            && std::fs::rename(&tmp, &path).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// When this bundle was last used, falling back to when it arrived.
    ///
    /// A bundle nobody has run yet is not therefore ancient — it is as old
    /// as its manifest, which is when this node first had it. Without the
    /// fallback the first sweep after an upgrade would treat the entire
    /// pre-A2 store as last used at the epoch and reclaim all of it at once,
    /// which is a correct reading of "no data" and a terrible thing to do.
    pub fn last_used(&self, id: &BundleId) -> u64 {
        if let Some(t) = read_micros(&self.used_path(id)) {
            return t;
        }
        std::fs::metadata(self.manifest_path(id))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_micros().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0)
    }

    /// Every bundle whose manifest is on this node's disk.
    pub fn local_bundles(&self) -> Vec<BundleId> {
        let Ok(entries) = std::fs::read_dir(self.root.join("manifests")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                let raw = data_encoding::HEXLOWER.decode(name.as_bytes()).ok()?;
                Some(BundleId(<[u8; 32]>::try_from(raw.as_slice()).ok()?))
            })
            .collect()
    }

    /// What this node is holding, and how much of it a sweep would take.
    ///
    /// **The reclaimable figure comes from the reclaim itself, run dry.**
    /// It used to be computed here from the same inputs, which is the
    /// obvious shape and was wrong within a week: a node with a disk floor
    /// set — every node — has `min_free_bytes > 0`, which makes every
    /// unprotected bundle a *candidate*, and this counted candidates. So
    /// `content` reported 390 KB reclaimable while `content prune
    /// --dry-run` correctly reported nothing, because only the sweep knew
    /// the floor was not actually pressing. Two code paths answering one
    /// question is how that happens; there is now one.
    pub fn usage(&self, plan: &SweepPlan) -> ContentUsage {
        let mut usage = ContentUsage::default();
        let mut chunks: std::collections::BTreeSet<ChunkHash> = Default::default();
        for id in self.local_bundles() {
            let Ok(Some(manifest)) = self.load_manifest(&id) else {
                continue;
            };
            usage.bundles += 1;
            chunks.extend(manifest.distinct_chunks());
        }
        // A chunk two bundles share is one chunk on disk. Counting it twice
        // would make this disagree with `du` on exactly the stores where
        // dedup is doing something.
        usage.chunks = chunks.len();
        usage.bytes = chunks.iter().map(|c| self.chunk_len(c)).sum();
        let dry = SweepPlan {
            dry_run: true,
            ..plan.clone()
        };
        if let Ok(report) = self.sweep(&dry) {
            usage.reclaimable_bundles = report.bundles_removed.len();
            usage.reclaimable_bytes = report.bytes_reclaimed;
        }
        usage
    }

    fn chunk_len(&self, c: &ChunkHash) -> u64 {
        std::fs::metadata(self.blob_path(c))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Reclaim what `plan` says may go.
    ///
    /// Mark and sweep, in that order and never the other way round: decide
    /// which *bundles* go, union the chunks every survivor still needs, and
    /// only then delete chunks outside that union. Deleting a bundle's
    /// chunks as the bundle is dropped would be the obvious shape and it
    /// would corrupt the store the first time two bundles shared a chunk —
    /// which is the entire point of a content-addressed store, and happens
    /// on the very first re-publish of an unchanged file.
    ///
    /// Ordering within the sweep is oldest-first, so a size cap or a disk
    /// floor that only needs to reclaim *some* of the eligible set takes
    /// the least recently used part of it.
    pub fn sweep(&self, plan: &SweepPlan) -> Result<SweepReport> {
        use std::collections::BTreeMap;
        let mut report = SweepReport::default();
        struct Candidate {
            id: BundleId,
            last_used: u64,
            chunks: Vec<ChunkHash>,
        }
        // A refcount per chunk — how many *surviving* bundles still name it
        // — rather than a "keep" set built after the fact. Dropping a
        // bundle decrements its chunks, and a chunk reaching zero is one
        // this sweep actually frees. That makes the running total exact,
        // which matters because the disk floor stops as soon as it has
        // reclaimed enough: an estimate that ran optimistic would stop
        // short of the floor it exists to restore.
        let mut refs: BTreeMap<ChunkHash, usize> = BTreeMap::new();
        let mut sizes: BTreeMap<ChunkHash, u64> = BTreeMap::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        for id in self.local_bundles() {
            let Ok(Some(manifest)) = self.load_manifest(&id) else {
                continue;
            };
            let chunks = manifest.distinct_chunks();
            for c in &chunks {
                *refs.entry(*c).or_insert(0) += 1;
                sizes.entry(*c).or_insert_with(|| self.chunk_len(c));
            }
            let last_used = self.last_used(&id);
            if plan.protected.contains(&id) {
                report.protected += 1;
            } else if plan.would_reclaim(&id, last_used) {
                candidates.push(Candidate {
                    id,
                    last_used,
                    chunks,
                });
            }
        }
        // Least recently used first, so a cap or a floor that only needs
        // part of the eligible set takes the coldest part of it.
        candidates.sort_by_key(|c| c.last_used);

        let live_bytes: u64 = sizes.values().sum();
        let mut budget = plan.bytes_to_reclaim(live_bytes, self.free_bytes());
        let mut freed: Vec<ChunkHash> = Vec::new();
        for c in candidates {
            // Past the window: goes regardless of how much has been
            // reclaimed. Not past it: only while a cap or floor still wants
            // bytes back.
            let by_age = plan.age_reclaims(c.last_used);
            if !by_age && budget == 0 {
                break;
            }
            let mut gained = 0u64;
            for h in &c.chunks {
                let n = refs.entry(*h).or_insert(1);
                *n = n.saturating_sub(1);
                if *n == 0 {
                    gained += sizes.get(h).copied().unwrap_or(0);
                    freed.push(*h);
                }
            }
            budget = budget.saturating_sub(gained);
            report.bytes_reclaimed += gained;
            report.bundles_removed.push(c.id);
        }
        report.chunks_removed = freed.len();
        report.bytes_remaining = live_bytes.saturating_sub(report.bytes_reclaimed);
        if plan.dry_run {
            return Ok(report);
        }
        // Manifests first. A manifest whose chunks are gone is a bundle
        // that answers `has_bundle` truthfully only until something tries
        // to materialize it; a chunk whose manifest is gone is merely an
        // orphan the next `collect_orphans` tidies. If this is interrupted
        // halfway, the second is the failure to be left holding.
        for id in &report.bundles_removed {
            let _ = std::fs::remove_file(self.manifest_path(id));
            let _ = std::fs::remove_file(self.used_path(id));
        }
        for h in &freed {
            let _ = std::fs::remove_file(self.blob_path(h));
        }
        if !report.bundles_removed.is_empty() {
            debug!(
                bundles = report.bundles_removed.len(),
                chunks = report.chunks_removed,
                bytes = report.bytes_reclaimed,
                "content store reclaim"
            );
        }
        Ok(report)
    }

    /// Delete chunks and stale temp files no manifest refers to.
    ///
    /// Separate from [`Self::sweep`] because it answers a different
    /// question — not *"may this go"* but *"what is this even"*. A fetch
    /// interrupted between its last chunk and its manifest leaves bytes
    /// nothing will ever name again, and they are invisible to every other
    /// accounting surface precisely because no bundle admits to them.
    pub fn collect_orphans(&self, dry_run: bool) -> Result<(usize, u64)> {
        let mut referenced: std::collections::BTreeSet<ChunkHash> = Default::default();
        for id in self.local_bundles() {
            if let Ok(Some(m)) = self.load_manifest(&id) {
                referenced.extend(m.distinct_chunks());
            }
        }
        let mut count = 0usize;
        let mut bytes = 0u64;
        let Ok(shards) = std::fs::read_dir(self.root.join("blobs")) else {
            return Ok((0, 0));
        };
        for shard in shards.flatten() {
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name();
                let Some(name) = name.to_str() else { continue };
                // A `.tmp` older than any plausible write is a crashed
                // fetch, not a race with a live one.
                let stale_tmp = name.contains(".tmp")
                    && file
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > std::time::Duration::from_secs(3600));
                let orphan = match data_encoding::HEXLOWER.decode(name.as_bytes()) {
                    Ok(raw) => <[u8; 32]>::try_from(raw.as_slice())
                        .map(|h| !referenced.contains(&ChunkHash(h)))
                        .unwrap_or(false),
                    Err(_) => false,
                };
                if !stale_tmp && !orphan {
                    continue;
                }
                count += 1;
                bytes += file.metadata().map(|m| m.len()).unwrap_or(0);
                if !dry_run {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        Ok((count, bytes))
    }

    /// Free bytes on the filesystem holding the store, when that can be
    /// measured. Zero means *unknown*, which every caller treats as "no
    /// disk-floor pressure" rather than "the disk is full".
    pub fn free_bytes(&self) -> u64 {
        crate::capacity::disk_free_mb(&self.root)
            .map(|mb| mb.saturating_mul(1024 * 1024))
            .unwrap_or(0)
    }
}

/// How often [`ContentStore::touch`] actually writes. An hour: fine enough
/// that a day-granularity window is never wrong by more than a rounding
/// error, coarse enough that serving a bundle is not a write per chunk.
const TOUCH_GRANULARITY: u64 = 3600 * 1_000_000;

fn read_micros(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// What a sweep is allowed to reclaim.
#[derive(Debug, Clone, Default)]
pub struct SweepPlan {
    /// Bundles no rule may touch — see [`crate::state::MeshState::referenced_bundles`].
    pub protected: std::collections::BTreeSet<BundleId>,
    /// Reclaim unreferenced bundles last used before this instant.
    /// `None` when the window is forever and only a cap or floor may act.
    pub cutoff_micros: Option<u64>,
    /// Reclaim immediately, with no grace period (`ContentPolicy::Ephemeral`).
    pub reclaim_immediately: bool,
    /// Reclaim down to this many bytes. `0` = no cap.
    pub max_bytes: u64,
    /// Reclaim until the filesystem has at least this much free. `0` = off.
    pub min_free_bytes: u64,
    /// Measure, change nothing.
    pub dry_run: bool,
}

impl SweepPlan {
    /// Does the *age window alone* condemn a bundle last used at this
    /// instant? Kept separate from [`Self::would_reclaim`] because a size
    /// cap reclaims only as much as it needs while a window reclaims
    /// everything past it, and the sweep has to tell the two apart.
    pub fn age_reclaims(&self, last_used: u64) -> bool {
        self.reclaim_immediately || self.cutoff_micros.is_some_and(|cut| last_used < cut)
    }

    /// Is this bundle a *candidate* — unprotected, and eligible under the
    /// window, the cap or the floor?
    ///
    /// Candidate, not condemned. A cap or a floor makes every unprotected
    /// bundle eligible and then reclaims only as many as it needs, so this
    /// answers "may the sweep consider this", never "will this go". Only
    /// [`ContentStore::sweep`] knows the second, because only it knows how
    /// much has already been freed by the time it reaches a given bundle.
    pub fn would_reclaim(&self, id: &BundleId, last_used: u64) -> bool {
        if self.protected.contains(id) {
            return false;
        }
        self.age_reclaims(last_used) || self.max_bytes > 0 || self.min_free_bytes > 0
    }

    /// Bytes a cap or a floor still needs back, given what the store holds
    /// and what the filesystem has free. Zero when neither is pressing.
    pub fn bytes_to_reclaim(&self, live_bytes: u64, free_bytes: u64) -> u64 {
        let over_cap = match self.max_bytes {
            0 => 0,
            cap => live_bytes.saturating_sub(cap),
        };
        // `free_bytes == 0` is "could not measure", never "disk full": a
        // platform without statvfs must not reclaim the entire store.
        let under_floor = match (self.min_free_bytes, free_bytes) {
            (0, _) | (_, 0) => 0,
            (floor, free) => floor.saturating_sub(free),
        };
        over_cap.max(under_floor)
    }
}

/// What a sweep did, or would have done.
#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    pub bundles_removed: Vec<BundleId>,
    pub chunks_removed: usize,
    pub bytes_reclaimed: u64,
    pub bytes_remaining: u64,
    /// Bundles something still needs, skipped however old they are.
    pub protected: usize,
}

/// What the store is holding right now.
#[derive(Debug, Clone, Default)]
pub struct ContentUsage {
    pub bundles: usize,
    pub chunks: usize,
    pub bytes: u64,
    pub reclaimable_bundles: usize,
    pub reclaimable_bytes: u64,
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .map(|m| m.mode() & 0o777)
        .unwrap_or(0o644)
}
#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0o644
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_single_script_is_a_bundle_of_it_and_its_definitions() {
        // Director's "drop a script on the window" pushes exactly the path it
        // was given, so this used to fail with "…/burn-in.sh is not a
        // directory" and nothing was imported at all.
        let src = tempfile::tempdir().expect("src");
        let script = src.path().join("burn-in.sh");
        std::fs::write(&script, b"#!/usr/bin/env bash\necho hi\n").expect("w1");
        std::fs::write(src.path().join("burn-in.sh.definitions"), b"{}").expect("w2");
        // A neighbour that is not the script's own definitions file: pushing
        // a script must not sweep up the directory it happens to live in.
        std::fs::write(src.path().join("secrets.env"), b"TOKEN=hunter2").expect("w3");

        let store_dir = tempfile::tempdir().expect("store");
        let store = ContentStore::open(store_dir.path()).expect("open");
        let manifest = store.ingest_path(&script).expect("ingest");

        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["burn-in.sh", "burn-in.sh.definitions"]);
        // And a directory still ingests as a directory.
        assert_eq!(store.ingest_path(src.path()).expect("dir").files.len(), 3);
    }

    #[test]
    fn a_script_with_no_definitions_file_is_a_one_file_bundle() {
        let src = tempfile::tempdir().expect("src");
        let script = src.path().join("plain.sh");
        std::fs::write(&script, b"echo hi\n").expect("w");
        let store_dir = tempfile::tempdir().expect("store");
        let store = ContentStore::open(store_dir.path()).expect("open");
        let manifest = store.ingest_path(&script).expect("ingest");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "plain.sh");
    }

    #[test]
    fn ingest_materialize_roundtrip() {
        let src = tempfile::tempdir().expect("src");
        std::fs::write(src.path().join("hello.txt"), b"hello world").expect("w1");
        std::fs::create_dir(src.path().join("sub")).expect("mkdir");
        std::fs::write(
            src.path().join("sub/data.bin"),
            vec![7u8; 3 * CHUNK_SIZE + 5],
        )
        .expect("w2");

        let store_dir = tempfile::tempdir().expect("store");
        let store = ContentStore::open(store_dir.path()).expect("open");
        let manifest = store.ingest_dir(src.path()).expect("ingest");
        let id = store.save_manifest(&manifest).expect("save");
        assert!(store.has_bundle(&id));
        // The 3MiB+5 file needs 4 chunks; hello shares none.
        assert_eq!(manifest.files.len(), 2);

        let dest = tempfile::tempdir().expect("dest");
        store.materialize(&id, dest.path()).expect("materialize");
        assert_eq!(
            std::fs::read(dest.path().join("hello.txt")).expect("read"),
            b"hello world"
        );
        assert_eq!(
            std::fs::read(dest.path().join("sub/data.bin"))
                .expect("read")
                .len(),
            3 * CHUNK_SIZE + 5
        );
    }

    #[test]
    fn determinism_same_tree_same_id() {
        let mk = || {
            let d = tempfile::tempdir().expect("d");
            std::fs::write(d.path().join("a"), b"aaa").expect("w");
            std::fs::write(d.path().join("b"), b"bbb").expect("w");
            d
        };
        let (d1, d2) = (mk(), mk());
        let s1 = ContentStore::open(tempfile::tempdir().unwrap().path()).unwrap();
        let s2 = ContentStore::open(tempfile::tempdir().unwrap().path()).unwrap();
        let id1 = s1.ingest_dir(d1.path()).unwrap().bundle_id();
        let id2 = s2.ingest_dir(d2.path()).unwrap().bundle_id();
        assert_eq!(id1, id2, "identical trees must yield identical BundleIds");
    }

    #[test]
    fn concurrent_writers_of_one_chunk_do_not_race() {
        // Two lanes fetching the same bundle write the same content
        // address at the same moment. A shared temp name made one of them
        // fail with a bare ENOENT, in a fraction of runs.
        let dir = tempfile::tempdir().expect("store");
        let store = ContentStore::open(dir.path()).expect("open");
        let data = vec![42u8; 64 * 1024];
        let hash = hash_chunk(&data);
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..16 {
                let store = store.clone();
                let data = data.clone();
                handles.push(scope.spawn(move || store.write_chunk(&hash, &data)));
            }
            for h in handles {
                h.join()
                    .expect("thread")
                    .expect("every writer must succeed");
            }
        });
        assert_eq!(
            store.read_chunk(&hash).expect("read").as_deref(),
            Some(&data[..])
        );
    }

    #[test]
    fn rejects_corrupt_chunk() {
        let store = ContentStore::open(tempfile::tempdir().unwrap().path()).unwrap();
        let hash = hash_chunk(b"real data");
        assert!(store.write_chunk(&hash, b"tampered").is_err());
    }

    // -----------------------------------------------------------------
    // Reclaim (M31 A2)
    // -----------------------------------------------------------------

    /// A store holding one bundle per `(name, body)`, plus the ids.
    fn store_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, ContentStore, Vec<BundleId>) {
        let dir = tempfile::tempdir().expect("dir");
        let store = ContentStore::open(dir.path()).expect("open");
        let mut ids = Vec::new();
        for (name, body) in files {
            let src = dir.path().join(format!("src-{name}"));
            std::fs::create_dir_all(&src).expect("mk");
            std::fs::write(src.join(name), body).expect("w");
            let manifest = store.ingest_dir(&src).expect("ingest");
            ids.push(store.save_manifest(&manifest).expect("save"));
        }
        (dir, store, ids)
    }

    /// Backdate a bundle's last use by writing the sidecar directly — the
    /// same file `touch` writes, so this exercises the real read path.
    fn age(store: &ContentStore, id: &BundleId, micros_ago: u64) {
        let path = store.used_path(id);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let when = mesh_proto::now_micros().saturating_sub(micros_ago);
        std::fs::write(&path, when.to_string()).expect("write");
    }

    const DAY: u64 = 86_400 * 1_000_000;

    #[test]
    fn a_bundle_past_the_window_is_reclaimed_and_its_chunks_with_it() {
        let (_d, store, ids) = store_with(&[("a.txt", b"aaaa")]);
        age(&store, &ids[0], 40 * DAY);
        let plan = SweepPlan {
            cutoff_micros: Some(mesh_proto::now_micros() - 30 * DAY),
            ..Default::default()
        };
        let report = store.sweep(&plan).expect("sweep");

        assert_eq!(report.bundles_removed, ids);
        assert_eq!(report.chunks_removed, 1);
        assert!(report.bytes_reclaimed > 0);
        assert!(!store.has_bundle(&ids[0]), "the manifest is gone too");
        assert!(store.local_bundles().is_empty());
    }

    #[test]
    fn a_bundle_inside_the_window_is_left_alone() {
        let (_d, store, ids) = store_with(&[("a.txt", b"aaaa")]);
        age(&store, &ids[0], 3 * DAY);
        let plan = SweepPlan {
            cutoff_micros: Some(mesh_proto::now_micros() - 30 * DAY),
            ..Default::default()
        };
        let report = store.sweep(&plan).expect("sweep");

        assert!(report.bundles_removed.is_empty());
        assert!(store.has_bundle(&ids[0]));
    }

    /// The rule no window outranks: something still needs this.
    #[test]
    fn a_protected_bundle_survives_however_old_it_is() {
        let (_d, store, ids) = store_with(&[("a.txt", b"aaaa")]);
        age(&store, &ids[0], 4000 * DAY);
        let plan = SweepPlan {
            protected: ids.iter().copied().collect(),
            // Ephemeral *and* a cap *and* a floor — every reason at once.
            reclaim_immediately: true,
            max_bytes: 1,
            min_free_bytes: u64::MAX,
            ..Default::default()
        };
        let report = store.sweep(&plan).expect("sweep");

        assert!(report.bundles_removed.is_empty());
        assert_eq!(report.protected, 1);
        assert!(
            store.has_bundle(&ids[0]),
            "a running deployment keeps its code"
        );
    }

    /// Dedup means "drop this bundle" and "delete these bytes" are
    /// different statements — the first must never imply the second for a
    /// chunk somebody else still names.
    #[test]
    fn a_chunk_two_bundles_share_survives_the_first_of_them() {
        // Identical bodies under different names: same chunk, two bundles.
        let (_d, store, ids) = store_with(&[("old.txt", b"same"), ("new.txt", b"same")]);
        let chunk = store
            .load_manifest(&ids[0])
            .expect("load")
            .expect("present")
            .distinct_chunks()[0];
        assert_eq!(
            store
                .load_manifest(&ids[1])
                .expect("load")
                .expect("present")
                .distinct_chunks()[0],
            chunk,
            "the fixture only tests anything if the chunk really is shared"
        );

        age(&store, &ids[0], 40 * DAY);
        age(&store, &ids[1], DAY);
        let plan = SweepPlan {
            cutoff_micros: Some(mesh_proto::now_micros() - 30 * DAY),
            ..Default::default()
        };
        let report = store.sweep(&plan).expect("sweep");

        assert_eq!(report.bundles_removed, vec![ids[0]]);
        assert_eq!(report.chunks_removed, 0, "the chunk is still spoken for");
        assert_eq!(report.bytes_reclaimed, 0);
        assert!(store.has_chunk(&chunk));
        assert!(store.has_bundle(&ids[1]), "the survivor still materializes");
    }

    /// A cap reclaims the coldest bundles and stops as soon as it is under,
    /// rather than emptying everything eligible.
    #[test]
    fn a_size_cap_takes_the_coldest_bundles_and_stops() {
        let big = vec![b'x'; 40_000];
        let (_d, store, ids) = store_with(&[
            ("cold.bin", &big),
            ("warm.bin", &vec![b'y'; 40_000]),
            ("hot.bin", &vec![b'z'; 40_000]),
        ]);
        age(&store, &ids[0], 9 * DAY);
        age(&store, &ids[1], 5 * DAY);
        age(&store, &ids[2], DAY);
        // Everything is inside the window, so only the cap may act.
        let plan = SweepPlan {
            cutoff_micros: Some(mesh_proto::now_micros() - 30 * DAY),
            max_bytes: 100_000,
            ..Default::default()
        };
        let report = store.sweep(&plan).expect("sweep");

        assert_eq!(
            report.bundles_removed,
            vec![ids[0]],
            "coldest first, then stop"
        );
        assert!(store.has_bundle(&ids[1]));
        assert!(store.has_bundle(&ids[2]));
        assert!(
            report.bytes_remaining <= 100_000,
            "{}",
            report.bytes_remaining
        );
    }

    #[test]
    fn keep_reclaims_nothing_and_ephemeral_reclaims_at_once() {
        for (immediate, expect_removed) in [(false, 0), (true, 1)] {
            let (_d, store, ids) = store_with(&[("a.txt", b"aaaa")]);
            // Used one second ago: no window would ever catch this.
            age(&store, &ids[0], 1_000_000);
            let plan = SweepPlan {
                cutoff_micros: None,
                reclaim_immediately: immediate,
                ..Default::default()
            };
            let report = store.sweep(&plan).expect("sweep");
            assert_eq!(report.bundles_removed.len(), expect_removed);
        }
    }

    #[test]
    fn a_dry_run_reports_what_it_would_take_and_takes_nothing() {
        let (_d, store, ids) = store_with(&[("a.txt", b"aaaa")]);
        age(&store, &ids[0], 40 * DAY);
        let plan = SweepPlan {
            cutoff_micros: Some(mesh_proto::now_micros() - 30 * DAY),
            dry_run: true,
            ..Default::default()
        };
        let report = store.sweep(&plan).expect("sweep");

        assert_eq!(report.bundles_removed, ids);
        assert!(report.bytes_reclaimed > 0);
        assert!(store.has_bundle(&ids[0]), "dry run changed nothing");
    }

    /// A bundle nobody has run yet is as old as its arrival, not as old as
    /// the epoch — otherwise the first sweep after an upgrade empties the
    /// entire pre-A2 store at once.
    #[test]
    fn a_never_used_bundle_dates_from_its_manifest_not_the_epoch() {
        let (_d, store, ids) = store_with(&[("a.txt", b"aaaa")]);
        let now = mesh_proto::now_micros();
        let last = store.last_used(&ids[0]);
        assert!(
            last > now.saturating_sub(DAY) && last <= now + DAY,
            "last_used {last} should sit at roughly now ({now})"
        );

        let plan = SweepPlan {
            cutoff_micros: Some(now - 30 * DAY),
            ..Default::default()
        };
        assert!(
            store
                .sweep(&plan)
                .expect("sweep")
                .bundles_removed
                .is_empty()
        );
    }

    #[test]
    fn touch_moves_a_bundle_out_of_the_window() {
        let (_d, store, ids) = store_with(&[("a.txt", b"aaaa")]);
        age(&store, &ids[0], 40 * DAY);
        store.touch(&ids[0]);
        let plan = SweepPlan {
            cutoff_micros: Some(mesh_proto::now_micros() - 30 * DAY),
            ..Default::default()
        };
        assert!(
            store
                .sweep(&plan)
                .expect("sweep")
                .bundles_removed
                .is_empty()
        );
    }

    #[test]
    fn usage_counts_a_shared_chunk_once() {
        let (_d, store, _ids) = store_with(&[("a.txt", b"same"), ("b.txt", b"same")]);
        let usage = store.usage(&SweepPlan::default());
        assert_eq!(usage.bundles, 2);
        assert_eq!(usage.chunks, 1, "one chunk on disk, named twice");
    }

    /// Bytes no manifest names — a fetch that died between its last chunk
    /// and its manifest — are invisible to every other accounting surface
    /// precisely because nothing admits to them.
    #[test]
    fn orphaned_chunks_are_collected() {
        let (_d, store, _ids) = store_with(&[("a.txt", b"aaaa")]);
        let stray = ChunkHash(*blake3::hash(b"nobody references this").as_bytes());
        store
            .write_chunk(&stray, b"nobody references this")
            .expect("write");
        assert!(store.has_chunk(&stray));

        let (count, bytes) = store.collect_orphans(true).expect("dry");
        assert_eq!(count, 1);
        assert!(bytes > 0);
        assert!(store.has_chunk(&stray), "dry run changed nothing");

        let (count, _) = store.collect_orphans(false).expect("collect");
        assert_eq!(count, 1);
        assert!(!store.has_chunk(&stray));
    }

    /// The screen and the button must not disagree. `usage` once counted
    /// *candidates* while `sweep` counted what it would actually take, and
    /// since every node has a disk floor, every node's status screen
    /// overstated what a prune would free.
    #[test]
    fn usage_agrees_with_what_a_prune_would_actually_take() {
        let (_d, store, ids) = store_with(&[("a.txt", b"aaaa"), ("b.txt", b"bbbb")]);
        // Used just now, so no window condemns them — but a disk floor is
        // set, which is the state every real node is in.
        for id in &ids {
            age(&store, id, 1_000_000);
        }
        let plan = SweepPlan {
            cutoff_micros: Some(mesh_proto::now_micros() - 30 * DAY),
            min_free_bytes: 2 * 1024 * 1024 * 1024,
            ..Default::default()
        };
        let usage = store.usage(&plan);
        let report = store
            .sweep(&SweepPlan {
                dry_run: true,
                ..plan.clone()
            })
            .expect("sweep");

        assert_eq!(usage.reclaimable_bundles, report.bundles_removed.len());
        assert_eq!(usage.reclaimable_bytes, report.bytes_reclaimed);
        assert_eq!(
            usage.reclaimable_bundles, 0,
            "a floor that is not pressing reclaims nothing"
        );
    }
}
