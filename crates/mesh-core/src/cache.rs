//! The action cache: key derivation and the directory shard (C0–C9 §11).
//!
//! ## What this is not
//!
//! It is not Bazel remote execution, and the gRPC dependency that deferred
//! this milestone twice was never the point. Everything the cache needs
//! shipped in M9: BLAKE3 content addressing, Merkle manifests, structured
//! signed job specs, an ephemeral object store, and a rendezvous-hashed
//! directory that is deliberately not the op-log ([`crate::objects`]). The
//! cache is an index and a lookup over infrastructure that already exists.
//!
//! ## Two halves
//!
//! [`derive`] turns a [`JobSpec`] into the enumerated tuple
//! [`mesh_proto::ActionSpec`] hashes. It runs on the **origin**, before
//! dispatch, because the origin is the node that does the lookup — and a
//! key computed on two nodes from two views of the same job is a key that
//! silently never hits.
//!
//! [`ActionCache`] is this node's shard of the directory: the entries whose
//! keys rendezvous-hash here, refcounted and LRU-evicted like any object,
//! plus the hit/miss counters `meshnode cache stats` reports.
//!
//! ## A miss must cost nothing
//!
//! One rendezvous ranking and one request to one node. No broadcast, no
//! index to consult, no coordinator to ask, and — when the mesh holds no
//! member that speaks [`mesh_proto::CACHE_MINOR`] — not even that: the
//! ranking comes back empty and the job dispatches exactly as it did before
//! this file existed.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::Instant;

use mesh_proto::{
    ActionKey, ActionSpec, CacheEntry, CacheStatRow, CacheStats, JobPayload, JobSpec, NodeId,
    ObjectId, PurgeScope, cache::CACHE_CONTEXT, cache::CACHE_REPLICAS,
};

/// How a job is isolated. Named rather than implied, because the day a
/// container runtime lands it becomes a second value — and every entry
/// produced before that day must stop matching, or a cached result from a
/// bare process will be handed to a job that asked for a sandbox.
///
/// **That day arrived with N4.** The key's *shape* is unchanged, which is
/// the whole point of having reserved the field: a `container` job and a
/// `process` job with otherwise identical inputs now derive different
/// keys, so nothing cached before N4 can be served to a container and
/// nothing a container produced can be served to a bare process. The
/// decision was made in advance; this is it arriving, and it cost one
/// `match`.
pub const SANDBOX_PROCESS: &str = "process";

/// A job run by a container plugin (N4).
pub const SANDBOX_CONTAINER: &str = "container";

/// The isolation class a payload runs under.
///
/// One function rather than a match at the call site, so "which class is
/// this" has exactly one answer in the product — a second reading of it
/// would be a second way for the cache to be wrong about what it is
/// holding.
pub fn sandbox_class(payload: &JobPayload) -> &'static str {
    let runtime = match payload {
        JobPayload::Service(exec) => Some(exec.runtime),
        JobPayload::Script(script) => Some(script.runtime),
        _ => None,
    };
    match runtime {
        Some(r) if r.is_plugin() => SANDBOX_CONTAINER,
        _ => SANDBOX_PROCESS,
    }
}

/// Entries one node's shard holds before LRU eviction starts. Roughly ten
/// megabytes of index at a hundred bytes an entry — small enough that no
/// operator has to think about it, large enough to hold a week of a busy
/// mesh.
pub const DEFAULT_CAPACITY: usize = 100_000;

/// This node's architecture triple, as it enters the key. A cached binary
/// artefact from one architecture is not an answer for another, and the
/// most expensive kind of cache bug is the one that hands you a working
/// answer for the wrong machine.
pub fn arch() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Every architecture triple a key may have been derived under, this
/// node's first (SEC-18).
///
/// The origin derives a job's key under *its own* triple, and a `map`
/// worker under its own — and neither is recorded anywhere a replica can
/// read. So a replica checking a key tries each triple the product builds
/// for. A handful of hashes, and a node on a platform not listed here only
/// loses its entries' indexing elsewhere, which is a miss.
pub fn known_arches() -> Vec<String> {
    let mut out = vec![arch()];
    for os in ["linux", "macos", "windows", "freebsd"] {
        for cpu in ["x86_64", "aarch64", "riscv64", "arm", "x86"] {
            let triple = format!("{cpu}-{os}");
            if !out.contains(&triple) {
                out.push(triple);
            }
        }
    }
    out
}

/// Build the action tuple for a job, or `None` when the job did not ask to
/// be cached.
///
/// **Ad-hoc bash is never cacheable**, even when asked. `meshnode run --cmd
/// 'date'` is not hermetic, cannot be made so, and a user who typed
/// `--cacheable` next to it has made a mistake the system should not
/// honour silently — the caller reports the refusal rather than caching it.
pub fn derive(spec: &JobSpec, epoch: u64, arch: &str) -> Result<ActionSpec, &'static str> {
    derive_from(&spec.payload, &spec.env, epoch, arch)
}

/// [`derive`] from the parts, for the origin — which holds the payload and
/// the environment well before it holds a [`JobSpec`], and must derive the
/// key *before* dispatch so it can decide whether to dispatch at all.
pub fn derive_from(
    payload: &JobPayload,
    job_env: &[(String, String)],
    epoch: u64,
    arch: &str,
) -> Result<ActionSpec, &'static str> {
    if !mesh_proto::is_cacheable(job_env) {
        return Err("this job did not ask to be cached");
    }
    let inputs: Vec<ObjectId> = mesh_proto::cache::input_objects(job_env);
    let outputs = mesh_proto::cache::output_globs(job_env);
    let (runtime, entrypoint, argv, bundle_root, source_digest) = match payload {
        JobPayload::Command(_) => {
            return Err(
                "ad-hoc commands are never cached: `--cmd` is not hermetic and cannot be \
                 declared so — publish it as a service and mark that cacheable",
            );
        }
        JobPayload::Service(exec) => (
            exec.runtime.as_str().to_string(),
            exec.entrypoint.clone(),
            argv_of(&exec.inputs),
            Some(exec.bundle_id),
            None,
        ),
        JobPayload::Script(script) => (
            script.runtime.as_str().to_string(),
            script.entrypoint().to_string(),
            argv_of(&script.inputs),
            None,
            // The source *is* the content address for an inline step: there
            // is no bundle to hash, and the requirements go in with it
            // because a different dependency set is a different action even
            // when the twelve lines above it are identical.
            Some(
                *blake3::Hasher::new()
                    .update(script.source.as_bytes())
                    .update(b"\x00")
                    .update(script.requirements.as_bytes())
                    .finalize()
                    .as_bytes(),
            ),
        ),
    };
    // The *service's* static environment travels in `ServiceExec`, and it
    // is as much an input as anything the submitter typed. Missing it would
    // mean a service whose definition changed only in `env` returning the
    // old answer forever.
    let mut env: Vec<(String, String)> = match payload {
        JobPayload::Service(exec) => exec.env.clone(),
        _ => Vec::new(),
    };
    env.extend(job_env.iter().cloned());
    let mut action = ActionSpec {
        runtime,
        entrypoint,
        argv,
        env,
        bundle_root,
        inputs,
        source_digest,
        outputs,
        sandbox_class: sandbox_class(payload).to_string(),
        arch: arch.to_string(),
        epoch,
    };
    action.canonicalise();
    Ok(action)
}

/// Inputs rendered the way the executor will pass them: `--key value`, in
/// declaration order.
///
/// Order matters and is therefore preserved: `--a 1 --b 2` and `--b 2 --a
/// 1` reach a program as different argv, and a program is entitled to care.
fn argv_of(inputs: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::with_capacity(inputs.len() * 2);
    for (k, v) in inputs {
        out.push(format!("--{k}"));
        out.push(v.clone());
    }
    out
}

/// Which nodes index an action key.
///
/// The same rendezvous ranking the object directory uses, under its own
/// context so an object id and an action key that happen to share bytes do
/// not share a shard.
///
/// **Ranked over every live member, including ones too old to answer.** The
/// alternative — filtering by negotiated minor — would give each node a
/// different ranking, because a minor is only known where a connection is
/// open, and a directory whose shape depends on who you have dialled is not
/// a directory. The caller skips a replica it cannot speak to, so a mesh
/// mid-upgrade indexes *some* keys nowhere and misses on them. A miss is
/// free; an inconsistent directory is a cache that never hits and nobody
/// can explain.
pub fn directory_for(key: &ActionKey, members: impl IntoIterator<Item = NodeId>) -> Vec<NodeId> {
    let mut ranked = crate::scheduler::rendezvous_rank(CACHE_CONTEXT, &key.0, members);
    ranked.truncate(CACHE_REPLICAS);
    ranked
}

#[derive(Debug)]
struct Held {
    entry: CacheEntry,
    /// Last read or write. The LRU key, and read-refreshed rather than
    /// write-only: an entry that keeps getting hit is the most valuable
    /// thing in the shard and must not age out behind one that was
    /// published later and never used.
    touched: Instant,
    /// The service the producing job ran, for per-service stats and for
    /// `purge --service`. `None` = an inline step or a `map`.
    service: Option<String>,
}

#[derive(Debug, Default)]
struct Counters {
    hits: u64,
    misses: u64,
    bytes_saved: u64,
    evicted: u64,
    published: u64,
    verified: u64,
    mismatched: u64,
    per_service: HashMap<String, (u64, u64, u64)>,
}

/// This node's shard of the cache directory, plus the counters behind
/// `meshnode cache stats`.
///
/// In memory and lost on restart, exactly like the object directory it sits
/// beside — and for the same reason. An entry points at objects held by a
/// node whose object store is itself cleared on boot, so persisting the
/// index across a restart would preserve pointers to bytes that no longer
/// exist. Losing the index costs re-executions; keeping a stale one costs
/// failed materialisations, which are worse because they look like bugs.
#[derive(Debug)]
pub struct ActionCache {
    entries: Mutex<HashMap<ActionKey, Held>>,
    counters: Mutex<Counters>,
    capacity: usize,
    epoch: u64,
    verify_rate: f64,
    enabled: bool,
}

impl Default for ActionCache {
    fn default() -> Self {
        ActionCache::new(
            true,
            mesh_proto::cache::DEFAULT_EPOCH,
            0.0,
            DEFAULT_CAPACITY,
        )
    }
}

impl ActionCache {
    pub fn new(enabled: bool, epoch: u64, verify_rate: f64, capacity: usize) -> ActionCache {
        ActionCache {
            entries: Mutex::new(HashMap::new()),
            counters: Mutex::new(Counters::default()),
            capacity: capacity.max(1),
            epoch,
            verify_rate: verify_rate.clamp(0.0, 1.0),
            enabled,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn verify_rate(&self) -> f64 {
        self.verify_rate
    }

    /// Should this hit be re-executed and checked?
    ///
    /// D14 says a mesh is one trust domain, so an entry signed by a member
    /// is sufficient and this is off by default. For the paranoid it is one
    /// percent of the cost of redundant execution — which is where the 2025
    /// GPU-validation literature landed, and it is a far better trade than
    /// running everything twice.
    pub fn should_verify(&self) -> bool {
        self.verify_rate > 0.0 && rand_unit() < self.verify_rate
    }

    /// Look an action up. Refreshes the LRU position on a hit, because an
    /// entry that keeps being useful is the last one that should be
    /// evicted.
    pub fn get(&self, key: &ActionKey) -> Option<CacheEntry> {
        let mut entries = self.entries.lock();
        let held = entries.get_mut(key)?;
        // An entry from a superseded epoch is not a hit and not an error:
        // the operator invalidated the world, and this is what that looks
        // like from underneath.
        if held.entry.epoch != self.epoch {
            entries.remove(key);
            self.counters.lock().evicted += 1;
            return None;
        }
        held.touched = Instant::now();
        Some(held.entry.clone())
    }

    /// Record an entry from its producer.
    ///
    /// Last-writer-wins on the producer's HLC, the same key every other
    /// record in this system merges on: two nodes that computed the same
    /// action concurrently and both announced converge on one answer
    /// instead of flapping between two identical ones.
    pub fn put(&self, entry: CacheEntry, service: Option<String>) {
        if entry.epoch != self.epoch {
            return;
        }
        {
            let mut entries = self.entries.lock();
            match entries.get_mut(&entry.key) {
                Some(held) if held.entry.hlc >= entry.hlc => {
                    held.touched = Instant::now();
                    return;
                }
                Some(held) => {
                    held.entry = entry;
                    held.touched = Instant::now();
                    held.service = service;
                }
                None => {
                    let key = entry.key;
                    entries.insert(
                        key,
                        Held {
                            entry,
                            touched: Instant::now(),
                            service,
                        },
                    );
                }
            }
        }
        let _ = self.evict_to_capacity();
    }

    /// [`Self::put`], reporting the objects that eviction released so the
    /// caller can unpin them.
    pub fn put_and_evict(&self, entry: CacheEntry, service: Option<String>) -> Vec<ObjectId> {
        self.put(entry, service);
        self.evict_to_capacity()
    }

    /// Drop the least recently used entries until the shard fits, and
    /// report the objects nothing references any more.
    ///
    /// A separate pass rather than an eviction inside `put`, so the common
    /// case — a shard well under capacity — takes one hash insert and no
    /// scan at all.
    ///
    /// The returned ids are what the caller unpins. Returned rather than
    /// unpinned here because this type knows nothing about an object store
    /// and should not: a directory shard that could delete bytes would be a
    /// directory shard that deletes somebody's live intermediate the first
    /// time the two refcounts disagree.
    fn evict_to_capacity(&self) -> Vec<ObjectId> {
        let mut entries = self.entries.lock();
        if entries.len() <= self.capacity {
            return Vec::new();
        }
        let mut by_age: Vec<(ActionKey, Instant)> =
            entries.iter().map(|(k, h)| (*k, h.touched)).collect();
        by_age.sort_by_key(|(_, t)| *t);
        let excess = entries.len() - self.capacity;
        let mut released = Vec::new();
        for (key, _) in by_age.into_iter().take(excess) {
            if let Some(held) = entries.remove(&key) {
                released.extend(held.entry.objects());
            }
        }
        self.counters.lock().evicted += excess as u64;
        released
    }

    /// Forget entries. Returns how many went, and the objects nothing
    /// references any more so the caller can unpin them.
    pub fn purge(&self, scope: &PurgeScope) -> (usize, Vec<ObjectId>) {
        let mut entries = self.entries.lock();
        let before = entries.len();
        let mut released = Vec::new();
        let mut take = |held: &Held| released.extend(held.entry.objects());
        match scope {
            PurgeScope::All => {
                entries.values().for_each(&mut take);
                entries.clear();
            }
            PurgeScope::Key(key) => {
                if let Some(held) = entries.remove(key) {
                    take(&held);
                }
            }
            PurgeScope::Service(name) => {
                entries.retain(|_, h| {
                    let keep = h.service.as_deref() != Some(name.as_str());
                    if !keep {
                        take(h);
                    }
                    keep
                });
            }
        }
        let gone = before - entries.len();
        self.counters.lock().evicted += gone as u64;
        (gone, released)
    }

    /// Drop entries whose producing job's objects are gone — the node that
    /// held them left, or their owning job was swept.
    ///
    /// `holder_live` answers "is this node still a member". An entry
    /// pointing at a departed producer is a guaranteed failed
    /// materialisation, and one of those is worse than a miss because it
    /// costs a fetch attempt before it costs the execution anyway.
    pub fn forget_producer(&self, holder_live: impl Fn(&NodeId) -> bool) -> usize {
        let mut entries = self.entries.lock();
        let before = entries.len();
        entries.retain(|_, h| holder_live(&h.entry.producer));
        let gone = before - entries.len();
        if gone > 0 {
            self.counters.lock().evicted += gone as u64;
        }
        gone
    }

    // -- accounting --------------------------------------------------------

    pub fn record_hit(&self, service: Option<&str>, bytes: u64) {
        let mut c = self.counters.lock();
        c.hits += 1;
        c.bytes_saved = c.bytes_saved.saturating_add(bytes);
        let row = c
            .per_service
            .entry(service.unwrap_or("(ad-hoc)").to_string())
            .or_default();
        row.0 += 1;
        row.2 = row.2.saturating_add(bytes);
    }

    pub fn record_miss(&self, service: Option<&str>) {
        let mut c = self.counters.lock();
        c.misses += 1;
        c.per_service
            .entry(service.unwrap_or("(ad-hoc)").to_string())
            .or_default()
            .1 += 1;
    }

    pub fn record_published(&self) {
        self.counters.lock().published += 1;
    }

    /// A spot-check finished. `agreed = false` is the only number on
    /// `cache stats` that is an emergency, and the caller raises an audit
    /// event beside it.
    pub fn record_verification(&self, agreed: bool) {
        let mut c = self.counters.lock();
        c.verified += 1;
        if !agreed {
            c.mismatched += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn stats(&self) -> CacheStats {
        let entries = self.entries.lock();
        let mut per_service_entries: HashMap<String, u64> = HashMap::new();
        for held in entries.values() {
            *per_service_entries
                .entry(
                    held.service
                        .clone()
                        .unwrap_or_else(|| "(ad-hoc)".to_string()),
                )
                .or_default() += 1;
        }
        let count = entries.len() as u64;
        drop(entries);

        let c = self.counters.lock();
        let mut names: Vec<String> = c.per_service.keys().cloned().collect();
        names.extend(per_service_entries.keys().cloned());
        names.sort();
        names.dedup();
        let by_service = names
            .into_iter()
            .map(|service| {
                let (hits, misses, bytes_saved) =
                    c.per_service.get(&service).copied().unwrap_or_default();
                CacheStatRow {
                    entries: per_service_entries.get(&service).copied().unwrap_or(0),
                    service,
                    hits,
                    misses,
                    bytes_saved,
                }
            })
            .collect();
        CacheStats {
            entries: count,
            capacity: self.capacity as u64,
            hits: c.hits,
            misses: c.misses,
            bytes_saved: c.bytes_saved,
            evicted: c.evicted,
            published: c.published,
            verified: c.verified,
            mismatched: c.mismatched,
            epoch: self.epoch,
            verify_rate: self.verify_rate,
            enabled: self.enabled,
            by_service,
        }
    }
}

/// A uniform draw in `[0, 1)`. Sampling, not cryptography: the question is
/// "check roughly one hit in a hundred", and a CSPRNG would be a slower
/// answer to it.
fn rand_unit() -> f64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(mesh_proto::now_micros());
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use mesh_proto::{Hlc, JobId};

    fn entry(key: u8, hlc: u64) -> CacheEntry {
        CacheEntry {
            key: ActionKey([key; 32]),
            exit_code: Some(0),
            stdout: None,
            stdout_len: 12,
            stderr: None,
            stderr_len: 0,
            outputs: Vec::new(),
            producer: NodeId([1; 32]),
            job: JobId([key; 16]),
            epoch: 0,
            hlc: Hlc {
                wall_micros: hlc,
                logical: 0,
            },
            at_micros: hlc,
        }
    }

    #[test]
    fn an_entry_round_trips_and_a_miss_is_an_ordinary_answer() {
        let cache = ActionCache::default();
        assert_eq!(cache.get(&ActionKey([1; 32])), None);
        cache.put(entry(1, 100), Some("deploy".into()));
        assert_eq!(cache.get(&ActionKey([1; 32])).unwrap().stdout_len, 12);
    }

    #[test]
    fn the_newer_publication_wins_and_the_older_one_is_dropped() {
        let cache = ActionCache::default();
        let mut newer = entry(1, 200);
        newer.stdout_len = 99;
        cache.put(newer, None);
        cache.put(entry(1, 100), None);
        assert_eq!(
            cache.get(&ActionKey([1; 32])).unwrap().stdout_len,
            99,
            "a late-arriving older entry must not talk the cache backwards"
        );
    }

    #[test]
    fn an_entry_from_a_superseded_epoch_is_not_a_hit() {
        let cache = ActionCache::new(true, 7, 0.0, DEFAULT_CAPACITY);
        cache.put(entry(1, 100), None);
        assert_eq!(cache.len(), 0, "a wrong-epoch entry is not even recorded");

        // …and one already held when the epoch moves is swept on read.
        let cache = ActionCache::new(true, 0, 0.0, DEFAULT_CAPACITY);
        cache.put(entry(1, 100), None);
        let bumped = ActionCache::new(true, 1, 0.0, DEFAULT_CAPACITY);
        bumped.put(entry(1, 100), None);
        assert_eq!(bumped.get(&ActionKey([1; 32])), None);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn the_least_recently_used_entry_is_the_one_that_goes() {
        let cache = ActionCache::new(true, 0, 0.0, 2);
        cache.put(entry(1, 100), None);
        cache.put(entry(2, 101), None);
        // Touch 1, so 2 is now the coldest.
        assert!(cache.get(&ActionKey([1; 32])).is_some());
        cache.put(entry(3, 102), None);
        assert_eq!(cache.len(), 2);
        assert!(
            cache.get(&ActionKey([1; 32])).is_some(),
            "a hot entry stays"
        );
        assert!(
            cache.get(&ActionKey([2; 32])).is_none(),
            "the cold one goes"
        );
    }

    #[test]
    fn purge_is_aimable() {
        let cache = ActionCache::default();
        cache.put(entry(1, 100), Some("deploy".into()));
        cache.put(entry(2, 100), Some("build".into()));
        cache.put(entry(3, 100), None);
        assert_eq!(cache.purge(&PurgeScope::Service("deploy".into())).0, 1);
        assert_eq!(cache.purge(&PurgeScope::Key(ActionKey([2; 32]))).0, 1);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.purge(&PurgeScope::All).0, 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn an_entry_whose_producer_left_is_dropped() {
        let cache = ActionCache::default();
        cache.put(entry(1, 100), None);
        assert_eq!(cache.forget_producer(|_| false), 1);
        assert!(cache.is_empty());
    }

    #[test]
    fn stats_separate_the_services_that_hit_from_the_ones_that_do_not() {
        let cache = ActionCache::default();
        cache.put(entry(1, 100), Some("deploy".into()));
        cache.record_hit(Some("deploy"), 4096);
        cache.record_hit(Some("deploy"), 4096);
        cache.record_miss(Some("deploy"));
        cache.record_miss(Some("build"));
        let stats = cache.stats();
        assert_eq!(stats.bytes_saved, 8192);
        assert!((stats.hit_rate().unwrap() - 0.5).abs() < 1e-9);
        let deploy = stats
            .by_service
            .iter()
            .find(|r| r.service == "deploy")
            .unwrap();
        assert_eq!((deploy.hits, deploy.misses, deploy.entries), (2, 1, 1));
        let build = stats
            .by_service
            .iter()
            .find(|r| r.service == "build")
            .unwrap();
        assert_eq!(build.hit_rate(), Some(0.0));
    }

    #[test]
    fn a_key_lands_on_the_same_two_nodes_from_every_node() {
        let members: Vec<NodeId> = (1..=6).map(|i| NodeId([i; 32])).collect();
        let key = ActionKey([42; 32]);
        let from_one = directory_for(&key, members.iter().copied());
        let mut shuffled = members.clone();
        shuffled.reverse();
        assert_eq!(from_one, directory_for(&key, shuffled));
        assert_eq!(from_one.len(), CACHE_REPLICAS);
    }

    #[test]
    fn a_mesh_with_nobody_to_ask_costs_one_empty_ranking() {
        assert!(directory_for(&ActionKey([1; 32]), Vec::new()).is_empty());
    }

    #[test]
    fn verification_is_off_unless_it_is_configured() {
        let never = ActionCache::new(true, 0, 0.0, DEFAULT_CAPACITY);
        for _ in 0..64 {
            assert!(!never.should_verify());
        }
        let always = ActionCache::new(true, 0, 1.0, DEFAULT_CAPACITY);
        assert!(always.should_verify());
    }
}

// ---------------------------------------------------------------------------
// N4 — the second sandbox class arrives
// ---------------------------------------------------------------------------

#[cfg(test)]
mod sandbox_class_tests {
    use super::*;
    use mesh_proto::{Runtime, ServiceExec};

    fn service(runtime: Runtime) -> JobPayload {
        JobPayload::Service(Box::new(ServiceExec {
            service_name: "web".into(),
            version: 1,
            runtime,
            bundle_id: mesh_proto::BundleId([7; 32]),
            working_dir: String::new(),
            entrypoint: "run.sh".into(),
            env: Vec::new(),
            inputs: Vec::new(),
            inventory: None,
        }))
    }

    #[test]
    fn a_process_runtime_is_the_process_class() {
        for r in [
            Runtime::Bash,
            Runtime::Python,
            Runtime::Ansible,
            Runtime::Exec,
        ] {
            assert_eq!(sandbox_class(&service(r)), SANDBOX_PROCESS, "{r:?}");
        }
    }

    #[test]
    fn a_container_runtime_is_its_own_class() {
        assert_eq!(
            sandbox_class(&service(Runtime::Container)),
            SANDBOX_CONTAINER
        );
    }

    /// **The property the field was reserved for**, and the whole reason
    /// `sandbox_class` was in the key from the start with one value in it.
    ///
    /// Two jobs identical in every other respect — same image name in the
    /// entrypoint, same bundle, same env, same inputs — must derive
    /// *different* keys when one runs as a bare process and one runs in a
    /// container. Otherwise a result produced without isolation is served
    /// to a job that asked for isolation, which is the silent failure the
    /// advance decision exists to prevent.
    #[test]
    fn a_container_and_a_process_never_share_a_cache_key() {
        // A key is only derived for work whose author claimed it is a pure
        // function of its inputs, so the test has to claim it too.
        let env: Vec<(String, String)> = vec![
            (mesh_proto::REQ_CACHE.to_string(), "1".into()),
            ("K".into(), "V".into()),
        ];
        let bare = derive_from(&service(Runtime::Exec), &env, 0, "x86_64").expect("bare");
        let boxed =
            derive_from(&service(Runtime::Container), &env, 0, "x86_64").expect("container");
        assert_ne!(
            bare.key(),
            boxed.key(),
            "a cached bare-process result must never be served to a container"
        );
        assert_eq!(bare.sandbox_class, SANDBOX_PROCESS);
        assert_eq!(boxed.sandbox_class, SANDBOX_CONTAINER);
        // The key's *shape* is unchanged — N4's instruction was to add the
        // second value, not to restructure the key. Everything but the
        // class is identical between the two.
        assert_eq!(bare.entrypoint, boxed.entrypoint);
        assert_eq!(bare.env, boxed.env);
        assert_eq!(bare.arch, boxed.arch);
    }
}
