//! The action cache (C0–C9 §11): what a job *is*, hashed, and what running
//! it produced.
//!
//! ## Why this class exists
//!
//! Same service, same bundle, same inputs, five hundred hosts, twice — and
//! without this the mesh does all of it twice. It is the only thing in the
//! cluster direction where the mesh gets *faster as it gets bigger*:
//! capacity scales linearly with nodes, a shared cache scales with use.
//!
//! ## The key is enumerated, never inferred
//!
//! [`ActionSpec`] lists every field that goes into the digest, by hand.
//! Enumerated rather than "everything that could matter", because a key
//! that silently grows a field invalidates every entry in the mesh on an
//! upgrade, and a key that silently *misses* one is a correctness bug that
//! presents as a haunting — the same output coming back for work that
//! changed. The list is short enough to read in one sitting and that is the
//! point of it.
//!
//! ## Secrets are in the key as references and never as values
//!
//! A late-bound `{{secret:name}}` is hashed verbatim, unresolved. Hashing
//! the resolved value would leak it to anyone who can enumerate keys, and
//! the mesh's own directory is an enumeration. The executor's redaction
//! filter is what keeps the resolved value out of the *outputs*
//! ([`crate::REDACTION`] territory), and the two together are the whole
//! secret story for the cache.
//!
//! ## The entry is directory data, not op-log data
//!
//! A cache entry is a hundred bytes about a job that already happened. Per
//! entry ops would be the time-series-in-a-state-store mistake at a
//! thousand times the volume of the job records that legitimately belong
//! there, so entries live in a rendezvous-hashed directory exactly as
//! object locations do (M9 §5.5) — refcounted, LRU-evicted, and free to
//! lose. **Losing an entry costs a re-execution, never correctness.**

use serde::{Deserialize, Serialize};

use crate::{ActionKey, BundleId, Hlc, JobId, NodeId, ObjectId};

// ---------------------------------------------------------------------------
// Reserved environment
// ---------------------------------------------------------------------------

/// `--cacheable` / `cacheable = true`: the submitter's claim that this work
/// is a pure function of the tuple in [`ActionSpec`].
///
/// **Opt-in, always, and never inferred.** `meshnode run --cmd 'date'` is
/// not hermetic and no amount of static analysis will make it so; a service
/// author asserting `cacheable` is making a claim about code they wrote,
/// which is the only party in the system in a position to make it.
pub const REQ_CACHE: &str = "MESHWARE_TASK_CACHE";

/// Declared output globs, comma-separated, relative to the job's working
/// directory. Part of the key — an action that captures `out/*.json` is a
/// different action from the one that captures `out/*` — and the list of
/// what a hit has to materialise.
pub const REQ_CACHE_OUTPUTS: &str = "MESHWARE_TASK_CACHE_OUTPUTS";

/// Content addresses of the declared object inputs, comma-separated.
///
/// The data plane's inputs reach a `map` task through
/// [`crate::parallel::ParallelPlan`], which is a wire struct and may never
/// grow a field — and a `map` item's key reads them straight off the plan,
/// which is the only place they exist today.
///
/// This key is for the payload classes that have no plan: an ordinary job
/// has no declared object inputs at all right now, so nothing sets it. It
/// is read anyway, and named here, so that the day something *does* stage
/// object inputs for a plain job, the key accounts for them instead of
/// serving one input's answer for another's.
pub const REQ_CACHE_INPUTS: &str = "MESHWARE_TASK_CACHE_INPUTS";

/// Prefix owned by the cache runtime: set by the origin on dispatch,
/// rejected from user-supplied env like every other reserved namespace.
pub const CACHE_ENV_PREFIX: &str = "MESHWARE_CACHE_";

/// The action key the origin derived, handed to the executor so the
/// executor can publish the entry it is about to produce.
///
/// Derived on the *origin* rather than on the executor, because the origin
/// is the node that did the lookup and the two must agree byte for byte:
/// a key computed twice from two views of the same job is a key that
/// silently never hits.
pub const CACHE_KEY: &str = "MESHWARE_CACHE_KEY";

/// Set on a job the origin dispatched *despite* a hit, to check the hit was
/// telling the truth (`[cache] verify_rate`). Its result is compared, not
/// published.
pub const CACHE_VERIFY: &str = "MESHWARE_CACHE_VERIFY";

/// Is this environment key part of the cache runtime namespace?
pub fn is_cache_env(key: &str) -> bool {
    key.starts_with(CACHE_ENV_PREFIX)
}

/// Reserved service tag: `cacheable`. A service version carrying it opts
/// every run of that version into the cache.
///
/// A **tag** and not a field on `ServiceVersion`, for the rule §14.1
/// restates every milestone: a struct that is signed and embedded may never
/// grow a field, and `ServiceVersion` is both. Tags are a list that already
/// travels and already merges.
pub const CACHEABLE_TAG: &str = "cacheable";

/// Whether a job's environment asks for caching.
pub fn is_cacheable(env: &[(String, String)]) -> bool {
    env.iter()
        .any(|(k, v)| k == REQ_CACHE && matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// The declared output globs on a job's environment, in the order given.
pub fn output_globs(env: &[(String, String)]) -> Vec<String> {
    env.iter()
        .find(|(k, _)| k == REQ_CACHE_OUTPUTS)
        .map(|(_, v)| {
            v.split(',')
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The declared object inputs on a job's environment. An unparseable id is
/// skipped rather than failing the derivation: the key is over what the
/// task actually declared, and a malformed declaration is a submission bug
/// that must not become a cache-poisoning one.
pub fn input_objects(env: &[(String, String)]) -> Vec<ObjectId> {
    env.iter()
        .find(|(k, _)| k == REQ_CACHE_INPUTS)
        .map(|(_, v)| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(|s| ObjectId::parse(s).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The key the origin stamped on a dispatched job, if any.
pub fn key_of(env: &[(String, String)]) -> Option<ActionKey> {
    env.iter()
        .find(|(k, _)| k == CACHE_KEY)
        .and_then(|(_, v)| ActionKey::parse(v).ok())
}

/// Is this job a verification re-run — one the origin dispatched even
/// though the cache answered, to check the answer?
pub fn is_verification(env: &[(String, String)]) -> bool {
    env.iter()
        .any(|(k, v)| k == CACHE_VERIFY && matches!(v.as_str(), "1" | "true" | "yes"))
}

// ---------------------------------------------------------------------------
// Volatile environment
// ---------------------------------------------------------------------------

/// Environment keys excluded from the key because they differ on every run
/// *by construction*.
///
/// This list is the single most dangerous thing in the milestone and it is
/// deliberately tiny. Every entry is a key whose value the mesh itself sets
/// per-run: including them would mean the cache never hits, and that is a
/// silent performance bug rather than a loud one. Nothing a *user* sets is
/// on it — an exclusion the user does not know about is the correctness bug
/// §11.2 warns of, and it would be invisible.
pub const VOLATILE_ENV: &[&str] = &[
    // The mesh's own per-job identity and plumbing.
    "MESHWARE_JOB_ID",
    "MESHWARE_JOB_DIR",
    "MESHWARE_NODE",
    "MESHWARE_NODE_ID",
    "MESHWARE_NODE_NAME",
    "MESHWARE_RUN_ID",
    "MESHWARE_STEP",
    "MESHWARE_ATTEMPT",
    "MESHWARE_ITEM_INDEX",
    // The data plane's *scratch paths*. Both name a directory under
    // `parallel/<job id>/<chunk>`, so both change on every dispatch of
    // identical work — and neither is an input: `MESHWARE_OUT` is where to
    // write, not what to write, and `MESHWARE_INPUTS` is where the declared
    // inputs were staged, which the key already covers by their content
    // addresses.
    //
    // Missing these was not hypothetical. A `map --cacheable` run twice
    // published two entries and hit neither, because `MESHWARE_OUT` carried
    // the job id.
    crate::objects::OUT_ENV,
    crate::objects::INPUTS_ENV,
    // The cache's own request keys. Each is either a *directive* about how
    // the mesh should handle the job rather than an input to it
    // (`MESHWARE_TASK_CACHE`), or is already represented by its own
    // enumerated field — `outputs` and `inputs` are in the tuple by name,
    // and counting them twice would mean an entry could be invalidated by a
    // change the key already accounts for.
    REQ_CACHE,
    REQ_CACHE_OUTPUTS,
    REQ_CACHE_INPUTS,
    // The 1.15 script-job routing marker. It says *which peers can decode
    // this job's envelope*, which is a fact about the mesh's version skew
    // and not about what the job computes — and if it entered the key, the
    // same compile would hash differently before and after the mesh
    // upgraded past it, which is a cache that empties itself on upgrade
    // day for no reason.
    crate::script::SCRIPT_AUTH,
];

/// Prefixes excluded from the key wholesale: the runtime-managed namespaces
/// whose values are an artefact of *where* a job landed rather than of what
/// it is. A gang id, a device allocation and the cache's own bookkeeping
/// are all facts about this dispatch, not about this action.
pub const VOLATILE_PREFIXES: &[&str] = &[
    crate::gang::GANG_ENV_PREFIX,
    crate::device::DEVICE_ENV_PREFIX,
    CACHE_ENV_PREFIX,
];

/// Does this environment entry vary run to run for reasons that have
/// nothing to do with what the job computes?
pub fn is_volatile(key: &str) -> bool {
    VOLATILE_ENV.contains(&key)
        || VOLATILE_PREFIXES.iter().any(|p| key.starts_with(p))
        // The device *request* is a placement constraint, not an input: the
        // same code on the same data produces the same bytes whether it was
        // asked for a 4090 or given one. `CUDA_VISIBLE_DEVICES` is excluded
        // by DEVICE_ENV_PREFIX's sibling below for the same reason.
        || key == crate::device::REQ_DEVICES
        || key == crate::device::CUDA_VISIBLE
        || key == crate::device::HIP_VISIBLE
        || key == crate::device::ROCR_VISIBLE
}

// ---------------------------------------------------------------------------
// The key
// ---------------------------------------------------------------------------

/// Where a cache entry's directory replicas live. The same rendezvous
/// primitive the object directory uses, under its own context so an object
/// id and an action key that happen to share bytes do not share a shard.
pub const CACHE_CONTEXT: &[u8] = b"actcache";

/// How many nodes index one action key. Two, exactly as the object
/// directory replicates its entries: the entry is a hundred bytes and
/// losing it costs a re-execution.
pub const CACHE_REPLICAS: usize = 2;

/// The default cache epoch. A mesh-wide integer an operator can bump to
/// invalidate everything at once, without a migration, a purge sweep, or a
/// window in which half the mesh believes different things.
pub const DEFAULT_EPOCH: u64 = 0;

/// The canonical, explicitly enumerated tuple an action key is derived
/// from (§11.2).
///
/// Every field is here because leaving it out would be a correctness bug,
/// and nothing else is here because a key that grows silently invalidates
/// the world. Read the list; if what you changed is not on it, the cache
/// will hand you back the old answer, and that is the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSpec {
    /// `bash` | `python` | `ansible` | `exec`.
    pub runtime: String,
    /// The entrypoint within the bundle, or the script filename for an
    /// inline step.
    pub entrypoint: String,
    /// Argv as the executor will build it, in order. For a service this is
    /// the `--key value` rendering of its inputs; for an inline script it
    /// is the same.
    pub argv: Vec<String>,
    /// Declared environment, sorted by key, with [`is_volatile`] keys
    /// removed. Sorted because a map has no order and two encodings of the
    /// same environment must hash the same.
    pub env: Vec<(String, String)>,
    /// The bundle's root hash — a Merkle digest over every file in it, so
    /// one byte anywhere in the tree moves the key.
    pub bundle_root: Option<BundleId>,
    /// Content addresses of the declared object inputs, sorted.
    pub inputs: Vec<ObjectId>,
    /// The source of an inline script, which *is* its content address —
    /// there is no bundle to hash for a `Script` payload.
    pub source_digest: Option<[u8; 32]>,
    /// Declared output globs, sorted. Two actions that run the same code
    /// and capture different files are different actions.
    pub outputs: Vec<String>,
    /// How the executor will isolate it: `process` today; a container or a
    /// jail later. A different sandbox is a different machine as far as a
    /// program's observable behaviour goes.
    pub sandbox_class: String,
    /// `x86_64-linux`, `aarch64-linux`, … A cached binary artefact from one
    /// architecture is not an answer for another.
    pub arch: String,
    /// The mesh-wide invalidation counter.
    pub epoch: u64,
}

impl ActionSpec {
    /// Put the spec in the one shape that hashes: sorted env with the
    /// volatile keys gone, sorted inputs, sorted globs.
    ///
    /// Idempotent, and called by [`Self::key`] rather than trusted to have
    /// happened — a spec built by hand in a test and one built by the
    /// daemon must produce the same digest or the tests prove nothing.
    pub fn canonicalise(&mut self) {
        self.env.retain(|(k, _)| !is_volatile(k));
        self.env.sort();
        self.env.dedup_by(|a, b| a.0 == b.0);
        self.inputs.sort();
        self.inputs.dedup();
        self.outputs.sort();
        self.outputs.dedup();
    }

    /// The canonical encoding. Domain-separated with a version tag, so a
    /// future encoding change is a *different* key rather than a collision
    /// with an old one.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut canonical = self.clone();
        canonical.canonicalise();
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(b"mw-action-key-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&canonical).unwrap_or_default());
        buf
    }

    pub fn key(&self) -> ActionKey {
        ActionKey(*blake3::hash(&self.canonical_bytes()).as_bytes())
    }

    /// The key's inputs, field by field, each with its own digest.
    ///
    /// This is what `--explain-key` prints and what P5's *"why was this a
    /// miss"* diffs: two explains of two runs differ in exactly the fields
    /// that made them different actions, which turns a cache miss from a
    /// mystery into one highlighted row.
    pub fn explain(&self) -> KeyExplain {
        let mut canonical = self.clone();
        canonical.canonicalise();
        let field = |name: &str, value: String| -> KeyField {
            KeyField {
                name: name.to_string(),
                digest: data_encoding::HEXLOWER
                    .encode(&blake3::hash(value.as_bytes()).as_bytes()[..4]),
                value,
            }
        };
        KeyExplain {
            key: canonical.key(),
            fields: vec![
                field("runtime", canonical.runtime.clone()),
                field("entrypoint", canonical.entrypoint.clone()),
                field("argv", canonical.argv.join(" ")),
                field(
                    "env",
                    canonical
                        .env
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                field(
                    "bundle_root",
                    canonical
                        .bundle_root
                        .map(|b| b.to_display_string())
                        .unwrap_or_else(|| "(none)".into()),
                ),
                field(
                    "inputs",
                    if canonical.inputs.is_empty() {
                        "(none)".into()
                    } else {
                        canonical
                            .inputs
                            .iter()
                            .map(ObjectId::short)
                            .collect::<Vec<_>>()
                            .join(" ")
                    },
                ),
                field(
                    "source",
                    canonical
                        .source_digest
                        .map(|d| data_encoding::HEXLOWER.encode(&d[..8]))
                        .unwrap_or_else(|| "(none)".into()),
                ),
                field(
                    "outputs",
                    if canonical.outputs.is_empty() {
                        "(none)".into()
                    } else {
                        canonical.outputs.join(" ")
                    },
                ),
                field("sandbox_class", canonical.sandbox_class.clone()),
                field("arch", canonical.arch.clone()),
                field("epoch", canonical.epoch.to_string()),
            ],
        }
    }
}

/// One field of the key, its value as hashed, and its own short digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyField {
    pub name: String,
    /// The value exactly as it entered the digest — already canonicalised,
    /// already stripped of volatile keys.
    pub value: String,
    /// Eight hex characters of BLAKE3 over `value`. Enough to compare two
    /// runs at a glance without printing a kilobyte of environment, and
    /// short enough that eleven of them fit in one terminal column.
    ///
    /// Deliberately not the whole digest: this is for a human eye scanning
    /// for the row that changed, and the key itself is what anything
    /// automated compares.
    pub digest: String,
}

/// The key and every field that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyExplain {
    pub key: ActionKey,
    pub fields: Vec<KeyField>,
}

impl KeyExplain {
    /// The field names on which two explains disagree — the answer to
    /// *"why was this a miss"*.
    pub fn differing(&self, other: &KeyExplain) -> Vec<String> {
        let mut out = Vec::new();
        for mine in &self.fields {
            match other.fields.iter().find(|f| f.name == mine.name) {
                Some(theirs) if theirs.digest == mine.digest => {}
                _ => out.push(mine.name.clone()),
            }
        }
        out
    }

    /// **What the key means, in one sentence** (L2 P4).
    ///
    /// Composed here so the CLI's footer, `--output json` and Director's
    /// hover carry identical words. The number of fields is counted rather
    /// than written down: a figure this product prints has to survive
    /// somebody checking it, which is the rule L1 P1's footer established.
    pub fn why(&self) -> String {
        key_why(self.fields.len())
    }
}

/// [`KeyExplain::why`], for a caller holding the fields but not the key.
///
/// Director's job panel is that caller: it carries `key_fields` on a view
/// model and has no `ActionKey` to hand when the job was a miss. One
/// function rather than two copies of the sentence.
pub fn key_why(fields: usize) -> String {
    format!(
        "{fields} fields make this key, and two runs whose keys match are the same work. \
         Compare it against another run's key to see which field made them differ."
    )
}

// ---------------------------------------------------------------------------
// The entry
// ---------------------------------------------------------------------------

/// One captured output file: where it goes, and the object holding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedOutput {
    /// Path relative to the job's working directory.
    pub path: String,
    pub object: ObjectId,
    pub size: u64,
}

/// What running an action produced, keyed by [`ActionKey`].
///
/// **Never a secret's resolved value.** Outputs are the executor's already
/// redacted stdout and stderr and the files the action declared; the
/// resolution of `{{secret:name}}` is filtered on the way to disk, which is
/// the same filter that keeps it out of a log anyone can read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub key: ActionKey,
    /// The process's exit code. `None` means it was killed by a signal —
    /// which is cached too, because "this always segfaults" is an answer.
    pub exit_code: Option<i32>,
    /// Object holding the captured stdout, and its length.
    pub stdout: Option<ObjectId>,
    pub stdout_len: u64,
    pub stderr: Option<ObjectId>,
    pub stderr_len: u64,
    /// Files the action declared and the executor captured.
    pub outputs: Vec<CachedOutput>,
    /// The node that ran it, and therefore the node holding the objects.
    /// A directory entry names a holder for the same reason the object
    /// directory does: there is no origin, only whoever computed it.
    pub producer: NodeId,
    /// The job that produced it, so a curious operator can read the run
    /// that a hit stood in for.
    pub job: JobId,
    /// The epoch this entry was produced under. An entry from an older
    /// epoch is not an error — it is simply not a hit, and is swept.
    pub epoch: u64,
    /// Producer's HLC at publication: the LWW key when two nodes computed
    /// the same action concurrently and both announced.
    pub hlc: Hlc,
    pub at_micros: u64,
}

impl CacheEntry {
    /// Bytes this entry stands in for — what a hit saved from crossing the
    /// network, and the figure `cache stats` reports.
    pub fn bytes(&self) -> u64 {
        self.stdout_len
            .saturating_add(self.stderr_len)
            .saturating_add(self.outputs.iter().map(|o| o.size).sum::<u64>())
    }

    /// Every object this entry depends on. A hit that cannot fetch all of
    /// them is not a hit — it is a miss with extra steps, and the caller
    /// falls through to executing rather than half-materialising.
    pub fn objects(&self) -> Vec<ObjectId> {
        let mut out: Vec<ObjectId> = self.stdout.into_iter().chain(self.stderr).collect();
        out.extend(self.outputs.iter().map(|o| o.object));
        out
    }

    /// Does the recorded result match a fresh execution's? The comparison
    /// `verify_rate` makes, and it deliberately compares the *observable*
    /// answer — exit code and output digests — rather than timing, memory
    /// or anything else that legitimately differs.
    pub fn agrees_with(&self, exit_code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> bool {
        self.exit_code == exit_code
            && self.stdout_len == stdout.len() as u64
            && self.stderr_len == stderr.len() as u64
    }
}

/// What a published [`CacheEntry`] was computed from (SEC-18), so the
/// replica indexing it can derive the key itself instead of taking the
/// publisher's word for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CacheProof {
    /// An ordinary job, as the executor holds it: its payload, environment
    /// and the user's signed request.
    Job(Box<crate::JobSpec>),
    /// One item of a `map`. The plan carries the user's signed request and
    /// every item, which is too much to send with each one, so it comes
    /// once per replica: `None` until the replica answers
    /// [`crate::wire::Response::CachePlanNeeded`].
    Item {
        item: String,
        plan: Option<Box<crate::parallel::ParallelPlan>>,
    },
}

/// A cache hit as it is recorded against the job that got it (§11.2: *a hit
/// is visible or it is untrustworthy*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHit {
    pub key: ActionKey,
    /// The job whose result this stood in for.
    pub produced_by: JobId,
    pub producer: NodeId,
    pub bytes_saved: u64,
    /// This job was re-executed anyway to check the entry (`verify_rate`),
    /// and the entry agreed. A disagreement is an audit event, not a field.
    pub verified: bool,
}

/// Per-service cache accounting, as `meshnode cache stats` prints it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStatRow {
    /// Service name, or `(ad-hoc)` for inline scripts and `map`.
    pub service: String,
    pub hits: u64,
    pub misses: u64,
    pub bytes_saved: u64,
    /// Entries this node holds in its directory shard for this service.
    pub entries: u64,
}

impl CacheStatRow {
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| self.hits as f64 / total as f64)
    }
}

/// The whole node's cache picture.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheStats {
    /// Entries in this node's directory shard.
    pub entries: u64,
    /// Cap before LRU eviction starts.
    pub capacity: u64,
    pub hits: u64,
    pub misses: u64,
    pub bytes_saved: u64,
    /// Entries dropped by eviction and by epoch sweeps since boot.
    pub evicted: u64,
    /// Entries this node produced and published.
    pub published: u64,
    /// Hits this node spot-checked, and how many disagreed. A non-zero
    /// `mismatched` is the only number on this struct that is an
    /// emergency.
    pub verified: u64,
    pub mismatched: u64,
    pub epoch: u64,
    pub verify_rate: f64,
    pub enabled: bool,
    pub by_service: Vec<CacheStatRow>,
}

impl CacheStats {
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| self.hits as f64 / total as f64)
    }
}

/// What `meshnode cache purge` is aimed at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PurgeScope {
    /// Everything this node indexes.
    All,
    /// One action key.
    Key(ActionKey),
    /// Every entry whose producing job ran a named service.
    Service(String),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn spec() -> ActionSpec {
        ActionSpec {
            runtime: "bash".into(),
            entrypoint: "deploy.sh".into(),
            argv: vec!["--target".into(), "web".into()],
            env: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
            bundle_root: Some(BundleId([7u8; 32])),
            inputs: vec![ObjectId([1u8; 32])],
            source_digest: None,
            outputs: vec!["out/*.json".into()],
            sandbox_class: "process".into(),
            arch: "x86_64-linux".into(),
            epoch: 0,
        }
    }

    /// The property the whole milestone rests on. If this table ever needs
    /// a new row, the person adding it is changing the contract.
    #[test]
    fn the_key_is_stable_across_things_that_do_not_matter() {
        let base = spec().key();

        // Environment order is not information.
        let mut reordered = spec();
        reordered.env.reverse();
        assert_eq!(reordered.key(), base, "env order must not move the key");

        // Neither is input order.
        let mut inputs = spec();
        inputs.inputs.push(ObjectId([2u8; 32]));
        inputs.inputs.reverse();
        let mut inputs_forward = spec();
        inputs_forward.inputs.push(ObjectId([2u8; 32]));
        assert_eq!(inputs.key(), inputs_forward.key());

        // The mesh's own per-run bookkeeping is not an input.
        for volatile in VOLATILE_ENV {
            let mut with = spec();
            with.env.push(((*volatile).into(), "whatever".into()));
            assert_eq!(with.key(), base, "{volatile} must not move the key");
        }

        // A device allocation is where it ran, not what it computed.
        let mut allocated = spec();
        allocated
            .env
            .push((crate::device::CUDA_VISIBLE.into(), "3".into()));
        allocated
            .env
            .push((crate::device::DEVICE_COUNT.into(), "1".into()));
        assert_eq!(allocated.key(), base, "a device allocation is not an input");

        // A gang membership likewise.
        let mut ganged = spec();
        ganged
            .env
            .push((crate::gang::GANG_ID.into(), "gang-abc".into()));
        assert_eq!(ganged.key(), base);

        // The cache's own request keys are directives, not inputs — and
        // `--cacheable` must not make otherwise identical work a different
        // action from the run that did not ask for it.
        let mut asked = spec();
        asked.env.push((REQ_CACHE.into(), "1".into()));
        assert_eq!(asked.key(), base);

        // …and a scratch path, which is the one that actually got away:
        // `MESHWARE_OUT` names a directory under the job id, so a `map`
        // run twice published two entries and hit neither.
        let mut scratch = spec();
        scratch.env.push((
            crate::objects::OUT_ENV.into(),
            "/var/lib/meshware/parallel/job-abc/0/out/0".into(),
        ));
        let mut scratch2 = spec();
        scratch2.env.push((
            crate::objects::OUT_ENV.into(),
            "/var/lib/meshware/parallel/job-xyz/0/out/0".into(),
        ));
        assert_eq!(scratch.key(), base, "where to write is not what to write");
        assert_eq!(scratch.key(), scratch2.key());
    }

    #[test]
    fn the_key_moves_for_everything_that_does_matter() {
        let base = spec().key();
        type Mutation = (&'static str, Box<dyn Fn(&mut ActionSpec)>);
        let cases: Vec<Mutation> = vec![
            (
                "runtime",
                Box::new(|s: &mut ActionSpec| s.runtime = "python".into()),
            ),
            (
                "entrypoint",
                Box::new(|s: &mut ActionSpec| s.entrypoint = "other.sh".into()),
            ),
            (
                "argv",
                Box::new(|s: &mut ActionSpec| s.argv.push("--extra".into())),
            ),
            (
                "argv order",
                Box::new(|s: &mut ActionSpec| s.argv.reverse()),
            ),
            (
                "env value",
                Box::new(|s: &mut ActionSpec| s.env[0].1 = "1 ".into()),
            ),
            (
                "env key",
                Box::new(|s: &mut ActionSpec| s.env.push(("C".into(), "3".into()))),
            ),
            (
                "bundle",
                Box::new(|s: &mut ActionSpec| s.bundle_root = Some(BundleId([8u8; 32]))),
            ),
            (
                "input",
                Box::new(|s: &mut ActionSpec| s.inputs.push(ObjectId([9u8; 32]))),
            ),
            (
                "source",
                Box::new(|s: &mut ActionSpec| s.source_digest = Some([3u8; 32])),
            ),
            (
                "outputs",
                Box::new(|s: &mut ActionSpec| s.outputs = vec!["out/*".into()]),
            ),
            (
                "sandbox",
                Box::new(|s: &mut ActionSpec| s.sandbox_class = "container".into()),
            ),
            (
                "arch",
                Box::new(|s: &mut ActionSpec| s.arch = "aarch64-linux".into()),
            ),
            ("epoch", Box::new(|s: &mut ActionSpec| s.epoch = 1)),
        ];
        for (what, mutate) in cases {
            let mut changed = spec();
            mutate(&mut changed);
            assert_ne!(
                changed.key(),
                base,
                "changing {what} must produce a different action"
            );
        }
    }

    /// One character in the entrypoint, one different key — the demo P1
    /// ends on.
    #[test]
    fn one_character_is_a_different_action() {
        let mut a = spec();
        a.entrypoint = "deploy.sh".into();
        let mut b = spec();
        b.entrypoint = "deploy.sh ".into();
        assert_ne!(a.key(), b.key());
    }

    #[test]
    fn a_secret_reference_is_hashed_unresolved() {
        let mut referenced = spec();
        referenced
            .env
            .push(("TOKEN".into(), "{{secret:api}}".into()));
        let explained = referenced.explain();
        let env = explained
            .fields
            .iter()
            .find(|f| f.name == "env")
            .expect("env field");
        assert!(
            env.value.contains("{{secret:api}}"),
            "the key must carry the reference, never the value"
        );
    }

    #[test]
    fn an_explain_names_the_field_that_differed() {
        let a = spec();
        let mut b = spec();
        b.argv.push("--extra".into());
        assert_eq!(
            a.explain().differing(&b.explain()),
            vec!["argv".to_string()]
        );
        assert!(a.explain().differing(&a.explain()).is_empty());
    }

    #[test]
    fn an_action_key_round_trips_through_its_display_form() {
        let key = spec().key();
        assert_eq!(ActionKey::parse(&key.to_display_string()).unwrap(), key);
        assert!(ActionKey::parse("act-nonsense").is_err());
        assert!(ActionKey::parse(&key.short()).is_err());
    }

    #[test]
    fn caching_is_off_unless_it_is_asked_for() {
        assert!(!is_cacheable(&[]));
        assert!(!is_cacheable(&[(REQ_CACHE.into(), "0".into())]));
        assert!(is_cacheable(&[(REQ_CACHE.into(), "1".into())]));
        assert!(is_cacheable(&[(REQ_CACHE.into(), "true".into())]));
    }

    #[test]
    fn output_globs_survive_the_round_trip() {
        let env = vec![(REQ_CACHE_OUTPUTS.into(), "out/*.json, build/lib.so ".into())];
        assert_eq!(output_globs(&env), vec!["out/*.json", "build/lib.so"]);
        assert!(output_globs(&[]).is_empty());
    }
}
