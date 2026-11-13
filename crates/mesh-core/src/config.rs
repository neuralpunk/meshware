//! Node configuration: one TOML file + env overrides + flags, resolved with
//! precedence flag > env > file > default (MASTERPLAN §13).
//!
//! Zero-config principle: every field has a sane default; `meshnode up` with
//! no file at all must produce a working node.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

pub const DEFAULT_PORT: u16 = 7777;
/// Default local observability port (`/healthz`, `/metrics`) — loopback
/// only unless configured otherwise (MASTERPLAN §12).
pub const DEFAULT_HTTP_PORT: u16 = 7776;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigFile {
    /// This machine's name in the mesh, as `meshnode nodes` and Director
    /// show it. Default: the hostname.
    pub node_name: Option<String>,
    /// Where the heavy directories (state, jobs, content) live, when they
    /// belong on a different volume from the config. Default: the home
    /// itself. The home is chosen by `--data-dir` or `$MESHWARE_HOME`, never
    /// by this key: the file lives inside it.
    pub data_dir: Option<PathBuf>,
    /// Tags this node announces for itself, which selectors, grants and
    /// placement match on: `["gpu", "dc=east"]`. Tags an admin assigns from
    /// anywhere (`meshnode nodes tag`) are kept separately and survive a
    /// rebuild; these are rewritten from this file at every start.
    pub tags: Option<Vec<String>>,
    pub listen: ListenSection,
    pub mesh: MeshSection,
    pub log: LogSection,
    pub secrets: SecretsSection,
    pub limits: LimitsSection,
    pub http: HttpSection,
    pub api: ApiSection,
    pub retention: RetentionSection,
    pub wan: WanSection,
    pub compute: ComputeSection,
    pub network: NetworkSection,
    /// Directories this node lends to jobs (N5 P1). `[[volume]]`.
    pub volume: Vec<VolumeSection>,
    pub runtime: RuntimeSection,
    pub history: HistorySection,
    #[serde(default)]
    pub facts: FactsSection,
    #[serde(default)]
    pub sessions: SessionsSection,
    #[serde(default)]
    pub release: ReleaseSection,
    #[serde(default)]
    pub state: StateSection,
    #[serde(default)]
    pub cache: CacheSection,
    #[serde(default)]
    pub integrations: IntegrationsSection,
    #[serde(default)]
    pub notify: NotifySection,
    #[serde(default)]
    pub disk: DiskSection,
    #[serde(default)]
    pub git: GitSection,
    #[serde(default)]
    pub shutdown: ShutdownSection,
}

/// `[git]` — how this node runs `git` for a source it watches (SEC-21).
///
/// Node-local on purpose. A source record is signed by whoever added it and
/// replicates everywhere; whether a machine will fetch code over a
/// transport anyone on the path can rewrite, or run a commit nobody signed,
/// is the decision of the person responsible for that machine.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct GitSection {
    /// Fetch `http://` and `git://` sources. Absent = false: neither
    /// transport authenticates the server or protects the bytes, and what
    /// comes back is rolled out to every service the source bumps.
    pub allow_plain: Option<bool>,
    /// Ingest only a commit `git verify-commit` accepts, against the
    /// trust this account's git and GnuPG (or SSH allowed-signers) are
    /// configured with. Absent = false.
    pub require_signed: Option<bool>,
    /// Longest one `git` command may run before it is killed. Absent = 10m.
    pub timeout: Option<String>,
}

/// Resolved `[git]` settings.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitConfig {
    pub allow_plain: bool,
    pub require_signed: bool,
    pub timeout: Duration,
}

impl Default for GitConfig {
    fn default() -> Self {
        GitConfig {
            allow_plain: false,
            require_signed: false,
            timeout: Duration::from_secs(600),
        }
    }
}

impl GitConfig {
    fn resolve(section: &GitSection) -> Result<GitConfig> {
        let d = GitConfig::default();
        let timeout = match section.timeout.as_deref().map(str::trim) {
            None => d.timeout,
            Some(text) => humantime::parse_duration(text)
                .ok()
                .filter(|t| !t.is_zero())
                .ok_or_else(|| {
                    CoreError::Config(format!(
                        "[git] timeout must be a duration like 10m (got {text:?})"
                    ))
                })?,
        };
        Ok(GitConfig {
            allow_plain: section.allow_plain.unwrap_or(d.allow_plain),
            require_signed: section.require_signed.unwrap_or(d.require_signed),
            timeout,
        })
    }
}

/// `[notify]` — where "it finished" goes when somebody asked to be told
/// (`meshnode run --notify`, `meshnode jobs wait --notify`).
///
/// Node-local, like `[integrations]`: it is a fact about *this* machine and
/// the person sitting at it, not about the mesh. Nothing here fires unless
/// a command asked for it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct NotifySection {
    /// An HTTP(S) endpoint to POST to. An `ntfy` topic URL gets a plain-text
    /// message with a `Title` header; anything else gets one JSON object.
    /// Plain `http://` is allowed, because the usual destination is a
    /// self-hosted ntfy on the LAN.
    pub url: Option<String>,
    /// A command run with `sh -c`, with the event in `MESHWARE_NOTIFY_*`
    /// environment variables — for everything the URL shape cannot say.
    pub command: Option<String>,
    /// Also show a desktop notification (`notify-send`) when one can be
    /// shown. Absent = true: a person at a desk asked to be told.
    pub desktop: Option<bool>,
}

/// Resolved `[notify]`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct NotifyConfig {
    pub url: Option<String>,
    pub command: Option<String>,
    pub desktop: bool,
}

impl NotifyConfig {
    pub fn resolve(section: &NotifySection) -> NotifyConfig {
        NotifyConfig {
            url: section
                .url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(String::from),
            command: section
                .command
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(String::from),
            desktop: section.desktop.unwrap_or(true),
        }
    }

    /// Is there anywhere at all for a notification to go?
    pub fn is_configured(&self) -> bool {
        self.url.is_some() || self.command.is_some() || self.desktop
    }
}

/// `[integrations]` — governed egress: which hosts this machine may send
/// data to (M15 §3.11, built for M31 B1).
///
/// One list and one match function, and it exists now rather than "later"
/// for a reason worth stating: a sink is the easiest exfiltration channel in
/// the product by construction — a text box that ships history anywhere,
/// filled in by somebody who does not know what an allowlist is. Retrofitting
/// a governance control **after** the first customer has a sink is how it
/// ends up optional for ever.
///
/// Node-local rather than replicated, and deliberately so. It is the answer
/// to *"what may this machine talk to"*, which is a fact about the machine
/// and its network, not a mesh-wide policy — and it is checked on the
/// exporting node **regardless of what any signed record says**. That
/// asymmetry is the whole value: a signed sink is what the mesh agreed to,
/// and this is what the machine will actually do.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct IntegrationsSection {
    /// Host patterns this node may send data to. Empty = no restriction,
    /// matching M15's specification.
    ///
    /// A pattern is a hostname, optionally with a leading `*.` wildcard:
    /// `warehouse.corp`, `*.internal`, `10.0.0.7`. Matched against the
    /// URL's host only — never the path — because a path-aware allowlist
    /// reads as more precise than it is and invites somebody to trust it
    /// for authorization.
    pub egress_allow: Option<Vec<String>>,
    /// How often this node looks for rows to export, in seconds.
    ///
    /// Thirty by default, which is the right answer for almost everybody: a
    /// sink is a copy of history, not a live feed, and polling a warehouse
    /// twice a second buys nothing but load. It is a knob because two real
    /// situations need it — a rate-limited destination that wants it
    /// *longer*, and a test that cannot afford to sleep through a real one.
    pub export_interval_secs: Option<u64>,
    /// How long a row must have been terminal before a sink sees it, in
    /// seconds.
    ///
    /// Five minutes by default, and the default is right for a real mesh:
    /// a run that finished elsewhere arrives whenever gossip gets round to
    /// it, and exporting behind the present turns "the row was late" into a
    /// non-event rather than a permanent hole in a compliance store. Zero
    /// is honest on a single-node mesh, which has nothing to converge with.
    pub settle_secs: Option<u64>,
}

/// `[state]` — replicated-state retention (C0 §4.2).
///
/// **One knob, on purpose.** Retention is a horizon, not a size: an op is
/// eligible to be dropped when it is *both* superseded by a later op of the
/// same author *and* older than the horizon. A size cap would be a second
/// knob that answers the same question worse — under pressure it drops the
/// newest history first, which is the history anyone is actually asking
/// about.
///
/// Not replicated, for the same reason `[release]` is not: it is a
/// statement about this machine's disk. A mesh with mixed horizons
/// converges perfectly well — each node vouches for its own holes — and the
/// horizon each pass used rides on the checkpoint, so a mixed mesh is
/// diagnosable from the log itself.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct StateSection {
    /// How long a superseded op is kept anyway, e.g. `"30d"`. Absent = 30d.
    /// `"0s"` compacts as soon as an op is superseded, which is legal and
    /// almost never what anyone wants.
    pub compaction_horizon: Option<String>,
    /// Run compaction on a timer. Absent = true.
    pub compaction_enabled: Option<bool>,
    /// Ops a minute one *entity* may author on this node (M31 A3).
    ///
    /// Absent takes [`crate::backpressure::DEFAULT_ENTITY_OPS_PER_MINUTE`],
    /// which is set off the busiest legitimate writer in the tree and has
    /// a wide burst allowance on top. **`0` turns the limiter off.**
    ///
    /// The knob exists for one reason and it is worth naming: a control
    /// that can refuse legitimate work and cannot be adjusted is a control
    /// an operator routes around by other means, at 3 a.m., under pressure.
    /// If this mesh has a writer nobody anticipated, raising a number in a
    /// config file has to be the answer.
    pub op_rate_per_minute: Option<u32>,
}

/// Resolved `[state]` settings.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct StateConfig {
    pub compaction_enabled: bool,
    pub compaction_horizon: std::time::Duration,
    /// Per-entity op rate; `0` is off. See [`StateSection::op_rate_per_minute`].
    pub op_rate_per_minute: u32,
}

impl Default for StateConfig {
    fn default() -> Self {
        StateConfig {
            compaction_enabled: true,
            compaction_horizon: std::time::Duration::from_secs(30 * 24 * 60 * 60),
            op_rate_per_minute: crate::backpressure::DEFAULT_ENTITY_OPS_PER_MINUTE,
        }
    }
}

impl StateConfig {
    pub fn horizon_secs(&self) -> u64 {
        self.compaction_horizon.as_secs()
    }

    pub fn resolve(section: &StateSection) -> Result<StateConfig> {
        let d = StateConfig::default();
        let compaction_horizon = match section.compaction_horizon.as_deref() {
            None => d.compaction_horizon,
            Some(text) => humantime::parse_duration(text).map_err(|_| {
                CoreError::Config(format!(
                    "[state] compaction_horizon: '{text}' is not a duration (try 30d, 72h)"
                ))
            })?,
        };
        Ok(StateConfig {
            compaction_enabled: section.compaction_enabled.unwrap_or(d.compaction_enabled),
            compaction_horizon,
            op_rate_per_minute: section.op_rate_per_minute.unwrap_or(d.op_rate_per_minute),
        })
    }
}

/// `[release]` — where this node fetches its own signed binary from
/// (M24 §4).
///
/// A fleet upgrade never hands a node a binary; each node fetches and
/// verifies its own. That makes *where from* a per-node setting, and on a
/// fleet of any size it needs to be a settable one: a thousand nodes each
/// pulling twenty megabytes from GitHub is a bill and an outage waiting to
/// happen, and an air-gapped fleet cannot reach it at all.
///
/// This is deliberately **not** replicated. A release source is a trust
/// anchor — the place this machine will download an executable from — and a
/// mesh-wide setting for it would mean one compromised admin could
/// re-point every node's supply chain with one op. It stays where the
/// machine's owner put it, in the machine's own config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ReleaseSection {
    /// Base URL releases are laid out under: `<base>/latest` holds the tag
    /// text, `<base>/<tag>/<artifact>` the binary and `<artifact>.minisig`
    /// its signature. Default: the project's GitHub releases.
    pub base: Option<String>,
    /// Base64 minisign public key to verify against, if this fleet signs
    /// its own builds. Default: the key embedded in the binary.
    pub pubkey: Option<String>,
}

/// Resolved `[release]` settings.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ReleaseConfig {
    pub base: Option<String>,
    pub pubkey: Option<String>,
}

impl ReleaseConfig {
    fn resolve(file: &ReleaseSection) -> ReleaseConfig {
        // The env vars stay as the *test* seam they always were, and lose
        // to an explicit config file: a fleet that has written down where
        // it gets its binaries should not have that quietly overridden by
        // whatever was exported into the daemon's environment.
        let pick = |from_file: &Option<String>, env: &str| {
            from_file
                .clone()
                .or_else(|| std::env::var(env).ok())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        ReleaseConfig {
            base: pick(&file.base, "MESHWARE_RELEASE_BASE"),
            pubkey: pick(&file.pubkey, "MESHWARE_MINISIGN_PUBKEY"),
        }
    }
}

/// `[wan]` — WAN reachability (MASTERPLAN M7): manually advertised public
/// endpoints, whether this node relays for NATed peers, and whether a NATed
/// node may use relays. Everything here is hot-reloadable (SIGHUP).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct WanSection {
    /// Extra `host:port` endpoints this node is reachable on (a cloud VM's
    /// elastic IP, a port-forward). Announced as direct candidates and
    /// treated as operator-verified public.
    pub advertise: Option<Vec<String>>,
    /// `"on"` | `"off"` (default off): relay traffic for NATed members once
    /// this node has verified it is publicly reachable (or `advertise` is
    /// set). Announces the reserved `relay` tag.
    pub relay: Option<String>,
    /// `"auto"` (default) | `"off"`: when NATed, register with up to two
    /// relay-tagged members and announce `relay:<id>` candidates.
    pub relay_client: Option<String>,
}

/// `[facts]` — the fleet's self-description (M18 §4).
///
/// One setting, and it is **on** by default: collecting facts reads this
/// machine's own `/etc/os-release` and `/proc`, which is the machine
/// describing itself. A node that cannot say what it is cannot
/// be selected, patched or diagnosed, and the bootstrap problem M18 solves
/// by having a built-in collector comes straight back if the built-in one
/// is off by default.
///
/// The switch exists for one real case: a mesh that still admits **pre-1.7
/// joiners**. Every fact publish is an op index a 1.6 build has no variant
/// for, so a member that has published facts can no longer admit one
/// (F0 §3.3). Turning collection off on the member that holds the invite
/// keeps that door open.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct FactsSection {
    /// `"on"` (default) | `"off"`: run the built-in `system` collector and
    /// any published collector whose selector matches this node.
    pub collect: Option<String>,
}

/// `[sessions]` — interactive terminals and forwards on this node: whether
/// it accepts them, how many, for how long, and whether they are recorded
/// (M17 §3.8, §3.10).
///
/// Two settings, and both are bounds rather than features. A forward is a
/// tunnel and tunnels grow, so the number one peer may hold at once is
/// capped and the refusal names the cap. Recording a human's terminal is a
/// policy decision an organisation makes, not a default a tool imposes, so
/// it is off — and the *audit entry* is not optional and does not depend on
/// it. Both are hot-reloadable (SIGHUP); a lowered cap refuses the next
/// session rather than killing live ones.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct SessionsSection {
    /// `"off"` (default) | `"on"`: accept terminals on this node at all.
    ///
    /// **Off is the default, and that is the whole security posture.** An
    /// agent you installed to run jobs does not silently become an agent
    /// somebody can get a prompt through; enabling it is a deliberate,
    /// local, auditable act. Turning it on is still not enough on its own —
    /// a session policy has to permit the user, and they still need the
    /// runner grant they would need to `meshnode run` here.
    pub accept: Option<String>,
    /// Concurrent sessions one peer may hold here. Default 8.
    pub max_per_peer: Option<u32>,
    /// `"off"` (default) | `"on"`: write a transcript of every session to
    /// `<home>/sessions/<id>/`.
    ///
    /// Deliberately off, and not merely for storage. A terminal stream
    /// carries whatever was typed and printed — `export API_KEY=…`, the
    /// contents of a credentials file, a database dump — so a recording
    /// store is a secret store that nobody designed as one. The *audit
    /// entry* does not depend on this and is never optional.
    pub record: Option<String>,
    /// Close a terminal that has seen no traffic for this long. Default 30m.
    /// `"off"` disables it.
    pub idle_timeout: Option<String>,
    /// Close a terminal this long after it opened, busy or not. Default 4h.
    /// `"off"` disables it.
    pub max_duration: Option<String>,
    /// `"off"` (default) | `"on"`: permit a terminal when the daemon runs
    /// as uid 0.
    ///
    /// The packaged unit runs as an unprivileged `meshnode` user, so this
    /// changes nothing there. It exists for the case somebody runs
    /// `meshnode up` as root and would otherwise hand out root prompts
    /// without ever deciding to.
    pub allow_root: Option<String>,
}

/// `[compute]` — parallel compute (MASTERPLAN M9): how much memory the
/// ephemeral object store may hold before it spills, how long an object
/// outlives the job that made it, and whether the tail of a job is
/// speculatively re-run.
///
/// **Read once, at startup.** This section used to claim it was
/// hot-reloadable except the RAM budget; it is not, and an operator who
/// edited `speculate` and sent SIGHUP got silence and no change. `SIGHUP`
/// now says so out loud (`apply_config`). Making it true means putting the
/// straggler policy behind the same kind of mutable holder `[limits]` uses,
/// which is a change to how the executor reads it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ComputeSection {
    /// RAM budget in MB for ephemeral object bytes; beyond it, objects
    /// spill to disk (M9 §5.5). Absent = an eighth of system memory,
    /// between 64 MB and 2 GB. 0 = spill everything.
    pub object_ram_mb: Option<u64>,
    /// `spread` (the default) or `pack` — how this node fills the mesh
    /// when it dispatches work (N6 §12).
    ///
    /// An unknown word falls back to `spread` **with a warning**, rather
    /// than refusing to start: the failure it guards against is a typo
    /// silently inverting every placement decision the node makes.
    pub placement: Option<String>,
    /// How many minutes an object outlives its job's completion. Absent =
    /// 15. Long enough for a reduce or a `--out` fetch to arrive after the
    /// run reports done.
    pub object_ttl_mins: Option<u64>,
    /// Re-run the last stragglers of a parallel job elsewhere and take
    /// whichever copy finishes first (M9 §5.6). Absent = true.
    pub speculate: Option<bool>,
    /// Fraction of a job that must be done before speculation starts.
    /// Absent = 0.95. Below it, an outstanding chunk is just work — unless
    /// there is no fresh work left to hand out, which opens the gate on
    /// its own (M9 §5.6).
    pub speculate_at: Option<f64>,
    /// A chunk is never re-run before it has been outstanding this many
    /// seconds. Absent = 5. Raise it where a worker's first chunk pays a
    /// large one-off setup cost that should not be mistaken for a stall.
    pub speculate_after_secs: Option<u64>,
    /// How much of a finished run's *uncollected* output this node holds
    /// past the ordinary object TTL, in MB. Absent = 4096.
    ///
    /// A result nobody has fetched is the answer, not an intermediate, and
    /// expiring it fifteen minutes after the run is how a transcode gets
    /// lost (R3 P3). Past this ceiling the oldest retained runs go back to
    /// the TTL and say so — pinning without a bound would turn a store with
    /// a ceiling into one without.
    ///
    /// Deliberately **not** derived from `object_ram_mb`: that is a RAM
    /// budget and results spill to disk, so a fraction of it would be the
    /// wrong size by orders of magnitude on exactly the machines that hold
    /// the most.
    pub uncollected_max_mb: Option<u64>,
    /// Largest total, in MB, that this node will **stage** to workers that
    /// cannot see the data themselves (R4 P3). Absent = 1024. 0 disables
    /// staging, and the run is refused with the mount named instead.
    ///
    /// Staging is right for a folder of videos and wrong for a dataset: the
    /// bytes cross the network once per item, and a ceiling is what keeps
    /// "meshware moved my files for me" from becoming "meshware saturated
    /// my uplink for an hour". Over it, the refusal names the ceiling as
    /// well as the mount, so both fixes are visible.
    pub stage_max_mb: Option<u64>,
}

/// `[cache]` — the action cache: whether this node serves and verifies
/// results the mesh has already computed (C0–C9 §11).
///
/// Not replicated, for the same reason `[state]` and `[release]` are not:
/// every setting here is a statement about this machine's memory and this
/// operator's paranoia. A mesh with mixed settings works — each node
/// indexes its own shard, each node checks as often as it was told to — and
/// the one figure that *must* agree, the epoch, is the one an operator sets
/// deliberately on all of them when they mean to invalidate the world.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct CacheSection {
    /// Serve and publish cache entries. Absent = true. `false` still lets
    /// the node index other nodes' entries — it stops it *using* them,
    /// which is what somebody debugging a suspected cache bug wants.
    pub enabled: Option<bool>,
    /// Mesh-wide invalidation counter. Bump it on every node to make every
    /// entry ever produced stop matching, with no migration and no purge
    /// sweep. Absent = 0.
    pub epoch: Option<u64>,
    /// Fraction of hits re-executed and compared (§11.2). Absent = 0.
    /// `0.01` is the figure the milestone recommends: one percent of the
    /// cost of redundant execution, and enough to notice a systematic
    /// mismatch within a day on any busy mesh.
    pub verify_rate: Option<f64>,
    /// Entries this node's directory shard holds before LRU eviction.
    /// Absent = 100_000.
    pub max_entries: Option<usize>,
    /// Days this node keeps the bytes behind an entry it published, with
    /// nobody reading them. Absent = 7; floored at 1.
    ///
    /// Renewed on every read, so an entry people still hit never lapses.
    /// Raise it on a node whose cache is hit in bursts weeks apart; lower
    /// it on one whose disk is the scarce thing.
    pub pin_lease_days: Option<u64>,
}

/// Resolved `[cache]` settings.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct CacheConfig {
    pub enabled: bool,
    pub epoch: u64,
    pub verify_rate: f64,
    pub max_entries: usize,
    /// How long this node keeps the bytes behind a cache entry it
    /// published, without anyone reading them (ENG-19).
    ///
    /// A producer pins the objects it publishes so they outlive the job
    /// that made them. Nothing could release that pin: the entry lives on a
    /// *directory* node picked by rendezvous hashing over the key, and that
    /// node evicts it without any way to say so — there is no
    /// `ObjectRelease` on the wire, deliberately. So the pin is a lease,
    /// renewed every time the object is read. Bytes behind an entry people
    /// still hit stay; the rest lapse and are swept on the ordinary
    /// refcount rules.
    pub pin_lease: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            enabled: true,
            epoch: mesh_proto::cache::DEFAULT_EPOCH,
            verify_rate: 0.0,
            max_entries: crate::cache::DEFAULT_CAPACITY,
            // A week. Long enough that a nightly build still hits its own
            // cache on Monday, short enough that a CI job nobody runs any
            // more stops costing disk within a sprint.
            pin_lease: Duration::from_secs(7 * 86_400),
        }
    }
}

impl CacheConfig {
    pub fn resolve(section: &CacheSection) -> CacheConfig {
        let d = CacheConfig::default();
        CacheConfig {
            enabled: section.enabled.unwrap_or(d.enabled),
            epoch: section.epoch.unwrap_or(d.epoch),
            // Clamped rather than refused: a typo that asked for 5.0 means
            // "check everything", which is a legal if expensive intent, and
            // failing to boot over it would be the wrong trade.
            verify_rate: section.verify_rate.unwrap_or(d.verify_rate).clamp(0.0, 1.0),
            max_entries: section.max_entries.unwrap_or(d.max_entries).max(1),
            pin_lease: section
                .pin_lease_days
                .map_or(d.pin_lease, |days| Duration::from_secs(days * 86_400))
                // Zero would mean "release on the next sweep", which makes
                // the cache useless rather than smaller. One day is the
                // shortest setting that still caches anything.
                .max(Duration::from_secs(86_400)),
        }
    }
}

/// `[limits]` — caps on the capacity-derived defaults (MASTERPLAN §11).
/// By default the node is greedy: slots = CPU cores, queue = 4 × slots.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsSection {
    /// Concurrent executor slots. Absent/0 = derive from hardware.
    pub slots: Option<u32>,
    /// Pending queue depth. Absent = 4 × slots (min 8).
    pub queue: Option<u32>,
    /// Memory admission floor (MASTERPLAN §11): queued jobs are not
    /// promoted into slots while `MemAvailable` is below this. Absent =
    /// 256 MB; 0 disables the gate.
    pub min_free_mem_mb: Option<u64>,
    /// Quiet hours: time-boxed caps on `slots`, in this machine's own local
    /// time (`crate::quiet`).
    ///
    /// ```toml
    /// quiet = "08:00-23:00 @ 30%"
    /// quiet = ["00:00-08:00 @ 100%", "08:00-23:00 @ 30%"]
    /// ```
    ///
    /// Absent = no quiet hours, which is what every node did before this
    /// existed and what a machine in a rack should keep doing.
    pub quiet: Option<QuietSpec>,
}

/// One window or several. TOML makes both natural and people write both, so
/// the parser takes both rather than making anyone look it up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuietSpec {
    One(String),
    Many(Vec<String>),
}

impl QuietSpec {
    fn resolve(&self) -> std::result::Result<crate::quiet::QuietHours, String> {
        match self {
            QuietSpec::One(s) => crate::quiet::QuietHours::parse(s),
            QuietSpec::Many(v) => crate::quiet::QuietHours::parse_all(v),
        }
    }
}

/// `[http]` — the local observability endpoint: `GET /healthz` and
/// `GET /metrics` (Prometheus) on every node (MASTERPLAN §12).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct HttpSection {
    /// Default true. The endpoint is loopback-only by default; disabling it
    /// removes the listener entirely.
    pub enabled: Option<bool>,
    /// Default `127.0.0.1:7776`. Port 0 = kernel-assigned (tests).
    pub addr: Option<String>,
}

/// `[api]` — the HTTP API (E1-P6). **Off unless `listen` is set.**
///
/// Deliberately a second listener rather than a route on `[http]`: that one
/// serves `/healthz` and `/metrics` unauthenticated by design, and sharing
/// a port would mean exposing the API to a network silently exposed the
/// metrics too.
///
/// The API is plain HTTP and every request is a *signed envelope*, so a
/// network attacker cannot forge one — only read a response. Put a reverse
/// proxy in front of it if it needs to leave the host.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ApiSection {
    /// e.g. `127.0.0.1:7778`. Unset means no listener at all.
    pub listen: Option<String>,
}

/// `[disk]` — the floor under everything this node stores (M31 A2).
///
/// **Node-local, and it has to be.** How much room a machine can spare is a
/// fact about that machine: the build box with a 4 TB array and the ARM
/// board with a 16 GB card are both legitimate members of one mesh, and a
/// replicated number that suited either would be wrong for the other. This
/// is the same reasoning `[retention]`'s node half rests on, applied to the
/// resource that actually runs out.
///
/// What it governs is *meshware's own reclaimable data* — the content
/// store, and job spools through the existing caps. It cannot make room
/// that something else on the machine is using, and it does not try to: a
/// node whose disk is full of somebody else's data warns and refuses to add
/// to the problem, which is the honest thing to do and the whole of what is
/// being promised.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct DiskSection {
    /// Never let free space fall below this many MB. Absent = 2048.
    pub min_free_mb: Option<u64>,
    /// Also keep this percentage of the filesystem free. Absent = 5.
    ///
    /// Capped internally at [`MAX_PCT_RESERVE_MB`] — five percent of a
    /// 4 TB array is 200 GB, which is not a safety margin, it is a disk
    /// nobody may use. The percentage exists to protect small volumes,
    /// where it is the number that scales correctly.
    pub min_free_pct: Option<u32>,
    /// Warn below this many MB free. Absent = twice the floor, so the
    /// warning always arrives before the reclaim does.
    pub warn_free_mb: Option<u64>,
    /// Reclaim the content store when free space is under the floor.
    /// Absent = true. Setting it false keeps the warnings and stops the
    /// deleting — for an operator who would rather be told than helped.
    pub reclaim_under_pressure: Option<bool>,
    /// The most output one job keeps per stream (stdout, stderr), in MB.
    /// Absent = 256; 0 = no limit. Past it the job keeps running and its
    /// output is read and dropped, with a line saying so (ENG-4).
    pub job_output_max_mb: Option<u64>,
}

/// The most a percentage floor may reserve, whatever the disk's size.
pub const MAX_PCT_RESERVE_MB: u64 = 20 * 1024;

/// Resolved [`DiskSection`].
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DiskConfig {
    pub min_free_mb: u64,
    pub min_free_pct: u32,
    pub warn_free_mb: u64,
    pub reclaim_under_pressure: bool,
    /// Per stream, per job; 0 = no limit.
    pub job_output_max_mb: u64,
}

impl Default for DiskConfig {
    fn default() -> Self {
        DiskConfig {
            min_free_mb: 2048,
            min_free_pct: 5,
            warn_free_mb: 0, // 0 = derive from the floor; see `warn_bytes`
            reclaim_under_pressure: true,
            job_output_max_mb: 256,
        }
    }
}

impl DiskConfig {
    pub fn resolve(section: &DiskSection) -> DiskConfig {
        let d = DiskConfig::default();
        DiskConfig {
            min_free_mb: section.min_free_mb.unwrap_or(d.min_free_mb),
            min_free_pct: section.min_free_pct.unwrap_or(d.min_free_pct),
            warn_free_mb: section.warn_free_mb.unwrap_or(d.warn_free_mb),
            reclaim_under_pressure: section
                .reclaim_under_pressure
                .unwrap_or(d.reclaim_under_pressure),
            job_output_max_mb: section.job_output_max_mb.unwrap_or(d.job_output_max_mb),
        }
    }

    /// [`Self::job_output_max_mb`] in bytes; `u64::MAX` for no limit.
    pub fn job_output_max_bytes(&self) -> u64 {
        match self.job_output_max_mb {
            0 => u64::MAX,
            mb => mb.saturating_mul(1024 * 1024),
        }
    }

    /// The floor, in bytes, for the filesystem holding `path`.
    ///
    /// Zero when the filesystem cannot be measured. Every caller reads that
    /// as *no pressure* rather than *no room*, because a platform without
    /// `statvfs` must not be a platform that deletes everything.
    pub fn min_free_bytes(&self, path: &std::path::Path) -> u64 {
        let mb = 1024 * 1024;
        let absolute = self.min_free_mb.saturating_mul(mb);
        let Some(total_mb) = crate::capacity::disk_total_mb(path) else {
            return absolute;
        };
        let pct = total_mb.saturating_mul(u64::from(self.min_free_pct)) / 100;
        absolute.max(pct.min(MAX_PCT_RESERVE_MB).saturating_mul(mb))
    }

    /// Where a warning starts, in bytes. Twice the floor unless an operator
    /// named a number — a warning that fires at the same moment the reclaim
    /// does is a warning nobody can act on.
    pub fn warn_bytes(&self, path: &std::path::Path) -> u64 {
        match self.warn_free_mb {
            0 => self.min_free_bytes(path).saturating_mul(2),
            mb => mb.saturating_mul(1024 * 1024),
        }
    }
}

/// `[shutdown]` — how long a stop waits for work already running (ENG-15).
///
/// `systemctl restart` is the documented package-upgrade path, and a
/// restart that kills whatever happens to be running makes upgrading a
/// thing operators schedule at 3am. The daemon instead stops accepting new
/// work, lets what it already accepted finish, and only then goes down.
///
/// This is a *grace*, not a drain: it is bounded, it does not announce
/// `draining` mesh-wide, and it does not move queued work to other nodes.
/// `meshnode drain` remains the way to empty a node on purpose.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ShutdownSection {
    /// Seconds a stop waits for running jobs. Absent = 30; 0 = don't wait.
    ///
    /// Keep it under the unit's `TimeoutStopSec`, or systemd's SIGKILL
    /// arrives first and the grace was only a delay.
    pub grace_secs: Option<u64>,
}

/// Resolved [`ShutdownSection`].
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ShutdownConfig {
    pub grace: Duration,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        ShutdownConfig {
            grace: Duration::from_secs(30),
        }
    }
}

impl ShutdownConfig {
    pub fn resolve(section: &ShutdownSection) -> ShutdownConfig {
        let d = ShutdownConfig::default();
        ShutdownConfig {
            grace: section
                .grace_secs
                .map_or(d.grace, Duration::from_secs)
                // A grace longer than the unit's own stop timeout cannot be
                // honoured, and one longer than an hour is a drain wearing
                // a restart's clothes.
                .min(Duration::from_secs(3600)),
        }
    }
}

/// `[retention]` — how long this node keeps finished jobs' files and
/// unused content, and how much of them (MASTERPLAN §9.1: default 30 days /
/// 1 GB per node, vs their 15 minutes).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct RetentionSection {
    /// Terminal jobs older than this many days are pruned. 0 = keep forever.
    pub job_days: Option<u32>,
    /// Total size cap for job history in MB; oldest terminal jobs are
    /// pruned first. 0 = unlimited.
    pub job_max_mb: Option<u64>,
    /// How hard this node tidies its content store (M31 A2): `keep`,
    /// `relaxed`, `balanced` (the default), `tidy`, `lean`, `ephemeral`.
    ///
    /// A level rather than a number, because the decision is a posture and
    /// the day count is how it is implemented — see
    /// [`mesh_proto::ContentPolicy`]. An operator who wants the number
    /// writes `content_days` instead; both go through the mesh policy, and
    /// neither can keep *more* than the mesh allows.
    pub content: Option<String>,
    /// The window in days, for an operator who would rather say `45` than
    /// pick a rung. Wins over `content` when both are set.
    pub content_days: Option<u32>,
    /// Total size cap for the content store in MB; the coldest unreferenced
    /// bundles go first. 0 = no cap, and the `[disk]` floor is then the
    /// only thing bounding it.
    pub content_max_mb: Option<u64>,
}

/// `[network]` — what this node contributes to the port ledger (N2 P1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkSection {
    /// The dynamic host-port range this node will allocate from:
    /// `"20000-32000"`. Absent = Nomad's default, and for Nomad's reason —
    /// above every registered service port, wide enough that exhaustion is
    /// not something an operator thinks about, and bounded so a node cannot
    /// be talked into binding something privileged.
    ///
    /// A malformed value advertises **no** range, which disqualifies this
    /// node from work that wants a port rather than silently substituting
    /// the default. A typo must not place a service somewhere its operator
    /// was trying to say something else about.
    pub dynamic_ports: Option<String>,
    /// Host ports this node will never hand out, whatever the range says:
    /// `[22, 443, 5432]`. For whatever else is already listening on the
    /// machine and did not ask meshware's permission.
    pub reserved_ports: Option<Vec<u16>>,
    /// Answer `<service>.mesh` on this node (N3 P3). Absent = on.
    ///
    /// Every node holds the whole catalogue, so every node can answer
    /// authoritatively with no round trip — which is the property worth
    /// having and the reason this is on by default rather than a role some
    /// nodes take.
    pub dns: Option<bool>,
    /// The port the responder binds. Absent = 8600, Consul's, **not 53**:
    /// binding below 1024 needs `CAP_NET_BIND_SERVICE` and this product's
    /// whole posture is that a node does not need root.
    pub dns_port: Option<u16>,
    /// The address it binds. Absent = `127.0.0.1`, which is the safe
    /// default: a resolver on this machine can reach it and the rest of
    /// the network cannot. Set `0.0.0.0` deliberately.
    pub dns_bind: Option<String>,
    /// Interfaces mesh traffic belongs on: `["ib0"]` (ENG-22). Absent =
    /// every interface with a usable address.
    ///
    /// For the host that has more than one answer to "where is this
    /// machine": an HPC or GPU node with a management NIC holding the
    /// default route and a fast fabric the mesh should actually use, and
    /// a build host whose `docker0` and forty `veth`s are all real
    /// addresses that reach nothing.
    ///
    /// A name that matches no interface **warns and falls back to every
    /// interface** rather than advertising nothing: a typo here must not
    /// be a machine that silently cannot be dialed.
    pub advertise_interfaces: Option<Vec<String>>,
}

/// `[[volume]]` — a directory this node lends to jobs (N5 P1).
///
/// Declared per node rather than centrally, because a host volume *is* a
/// property of one host: the mesh has no way to know what is on a machine's
/// disks, and a central declaration would be a claim nobody could verify.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VolumeSection {
    /// The name a job claims it by. Becomes `MESHWARE_VOLUME_<NAME>`.
    pub name: String,
    /// The directory on this node.
    pub path: String,
    /// Refuse write claims entirely, whatever a job asks for. The
    /// operator's statement about the directory outranks the job's wish
    /// for it.
    pub read_only: Option<bool>,
}

/// `[runtime]` — how much of the machine the async runtime may use, how
/// jobs are sandboxed, and which container and volume plugins to drive.
///
/// The default is every core, which is right for a dedicated node and wrong
/// for a laptop that is also somebody's workstation. Both numbers are read
/// once at startup: a thread pool cannot be resized under load without
/// stalling exactly the work it exists to serve.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct RuntimeSection {
    /// Async worker threads. Absent/0 = one per hardware thread.
    pub worker_threads: Option<usize>,
    /// Ceiling on the blocking pool — the threads that run `spawn_blocking`
    /// work: redb commits, content hashing, filesystem walks. Absent = 512,
    /// tokio's default, which is a ceiling rather than a reservation.
    pub max_blocking_threads: Option<usize>,
    /// Per-thread stack size in KB. Absent = the platform default (2 MB on
    /// Linux). Lowering it is how a node with 64 workers keeps its virtual
    /// footprint sane on a small box.
    pub thread_stack_kb: Option<usize>,
    /// Per-job filesystem sandboxing with Landlock (`crate::sandbox`):
    /// `auto` (the default: sandbox where this machine can), `on` (refuse
    /// a job that cannot be sandboxed), `off`.
    pub sandbox: Option<String>,
    /// Extra paths a sandboxed job may read and execute from.
    pub sandbox_read: Option<Vec<String>>,
    /// Extra paths a sandboxed job may also write.
    pub sandbox_write: Option<Vec<String>>,
    /// An **override** for how this node runs containers (N4, T2).
    ///
    /// Absent is the ordinary case and the one to reach for first: meshnode
    /// detects podman or docker by itself and needs no configuration at
    /// all. This exists for the runtime nobody has written yet — gVisor,
    /// Firecracker, a hardened corporate wrapper — and it wins the
    /// resolution order when it is set.
    ///
    /// `meshnode` still links no OCI client: the static-musl, no-C,
    /// four-crate property is load-bearing. Driving a container CLI that is
    /// already on the machine costs nothing against it, which is what
    /// makes the built-in path possible at all.
    pub container_plugin: Option<String>,
    /// Which container engine to prefer, when a machine has more than one.
    ///
    /// `podman` or `docker`. Absent means podman first — rootless, and no
    /// daemon to be down. This **reorders the search; it never restricts
    /// it**, because a preference that became an outage the day that engine
    /// stopped would be a worse setting than no setting.
    pub container_engine: Option<String>,
    /// The subprocess that provisions volumes on demand (N5 P2). Absent =
    /// this node lends only the `[[volume]]` blocks it was given, which is
    /// the default.
    ///
    /// Nomad's Dynamic Host Volume shape: the operation in `$1`, inputs in
    /// the environment, one JSON object on stdout. See
    /// `mesh_proto::volumes` for the contract and the two holes it closes.
    pub volume_plugin: Option<String>,
    /// Where that plugin is told to find its own configuration — the first
    /// of the two holes. Passed through as
    /// `MESHWARE_VOLUME_PLUGIN_CONFIG` and never read by meshware, which
    /// has no opinion about what a plugin needs to know.
    pub volume_plugin_config: Option<String>,
}

/// `[history]` — the local query index over jobs.
///
/// This is derived data: every fact in it also exists in the op-log, so the
/// index can be deleted and rebuilt at any time. What it buys is answering
/// "failed `deploy` jobs in the last 90 days" with a range scan instead of
/// a walk over every job the mesh has ever run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct HistorySection {
    /// Maintain the index. Absent = true. Turning it off leaves the node
    /// fully functional and makes `history` queries fall back to a scan.
    pub enabled: Option<bool>,
    /// Prune indexed rows older than this many days. Absent = 0 = forever.
    /// The index is small (a few hundred bytes per job); the reason to cap
    /// it is a policy about how long history is kept, not disk pressure.
    pub keep_days: Option<u32>,
}

/// `[secrets.providers.<name>]` — external secret provider plugins
/// (MASTERPLAN §7.3). The plugin contract: `plugin get`, JSON
/// `{path,key,config}` on stdin, `{"value":…}` on stdout.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct SecretsSection {
    /// One table per provider, named for it: `[secrets.providers.vault]`.
    pub providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderEntry {
    /// Path to the provider plugin executable.
    pub plugin: String,
    /// Arbitrary provider configuration, passed to the plugin verbatim.
    pub config: toml::Table,
}

/// `[listen]` — where this node accepts mesh traffic: QUIC, over UDP.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ListenSection {
    /// The address to bind. Default `0.0.0.0`, every IPv4 interface. Write
    /// an IPv6 address in brackets — `"[::]"` serves both families on a
    /// dual-stack host — because it is joined to the port as `addr:port`.
    /// A specific address is also the only one the node advertises.
    pub addr: Option<String>,
    /// The UDP port. Default 7777, the one port every member must be able to
    /// reach on every other.
    pub port: Option<u16>,
}

/// `[mesh]` — the mesh this node founds, if it founds one.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct MeshSection {
    /// Name used when this node bootstraps a brand-new mesh. Ignored once a
    /// genesis document exists.
    pub name: Option<String>,
}

/// `[log]` — how much the daemon logs, and where.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct LogSection {
    /// `error`, `warn`, `info` (default), `debug` or `trace` — or a full
    /// filter such as `info,mesh_core::gossip=debug`. `$MESHWARE_LOG`
    /// overrides it, and `$RUST_LOG`, when set, overrides both.
    pub level: Option<String>,
    /// `text` (default) or `json` (structured logs for shippers).
    pub format: Option<String>,
    /// When set, the daemon logs to `<dir>/meshnode.log` with built-in
    /// size-based rotation (MASTERPLAN §12) instead of stderr.
    ///
    /// A **relative** path is resolved under the home, so `dir = "logs"`
    /// means `<home>/logs` — which is the one place logs should go if they
    /// are going anywhere. An absolute path is honoured as given, for the
    /// operator who wants them on a separate volume.
    pub dir: Option<PathBuf>,
    /// Rotate the log file once it exceeds this size (default 20 MB).
    pub max_size_mb: Option<u64>,
    /// Rotated files kept (`meshnode.log.1` … `.N`, default 5).
    pub max_files: Option<u32>,
}

/// Fully resolved runtime configuration.
#[derive(Debug, Clone, Serialize)]
pub struct NodeConfig {
    pub node_name: String,
    /// The one home everything lives under (`crate::layout`). `data_dir` is
    /// its root; the named accessors on [`NodeConfig::layout`] are what new
    /// code should use rather than joining strings onto it.
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub mesh_name: String,
    pub log_level: String,
    /// Node tags for selectors and scheduling (e.g. `gpu`, `dc=east`).
    pub tags: Vec<String>,
    /// External secret providers configured on this node.
    pub secret_providers: BTreeMap<String, ProviderConfig>,
    /// Executor capacity limits (MASTERPLAN §10.2, §11).
    pub limits: Limits,
    /// Time-boxed caps on `limits.slots`, in local time (`crate::quiet`).
    ///
    /// Not a field of [`Limits`], which is `Copy` and is passed around by
    /// value on every admission path. A window list is a `Vec`; making
    /// `Limits` clone-only to carry one would put an allocation on the hot
    /// path for a setting almost nobody sets.
    pub quiet: crate::quiet::QuietHours,
    /// Local observability endpoint; `None` = disabled (MASTERPLAN §12).
    pub http_addr: Option<SocketAddr>,
    /// The HTTP API listener (E1-P6). `None` — the default — means the
    /// daemon binds nothing and the API does not exist on this node.
    pub api_addr: Option<SocketAddr>,
    /// Structured logging + rotation (MASTERPLAN §12).
    pub log: LogConfig,
    /// Job history retention (MASTERPLAN §9.1).
    pub retention: Retention,
    /// The disk floor under the content store and the job spool (M31 A2).
    pub disk: DiskConfig,
    /// WAN reachability settings (MASTERPLAN M7).
    pub wan: WanConfig,
    /// Parallel compute settings (MASTERPLAN M9).
    pub compute: ComputeConfig,
    pub network: NetworkConfig,
    /// Async runtime sizing — how many cores this node is allowed to use.
    pub runtime: RuntimeConfig,
    /// The local run/job query index and how much of it stays resident.
    pub history: HistoryConfig,
    /// Fact collection settings (M18 §4).
    pub facts: FactsConfig,
    /// Session limits and recording (M17 §3.8, §3.10).
    pub sessions: SessionsConfig,
    /// Where this node fetches its own signed release from (M24 §4).
    pub release: ReleaseConfig,
    /// How `git` is run for watched sources (SEC-21).
    pub git: GitConfig,
    /// Op-log retention and compaction (C0 §4).
    pub state: StateConfig,
    /// The action cache (C7 §11).
    pub cache: CacheConfig,
    /// Governed egress (M15 §3.11; M31 B1 is what needed it built).
    pub integrations: IntegrationsConfig,
    /// `[notify]` — where a finished job's news goes, when asked.
    pub notify: NotifyConfig,
    /// How long a stop waits for work already accepted here (ENG-15).
    pub shutdown: ShutdownConfig,
    /// Where this config came from, for `config show`.
    pub config_path: Option<PathBuf>,
    /// Settings the file still carries that no longer do anything, one
    /// sentence each. Collected here rather than logged at resolve time,
    /// because the node resolves its config before its logger exists, and
    /// a warning written to no subscriber is a warning nobody reads;
    /// [`NodeConfig::check`] returns them, and `meshnode up` logs them.
    pub retired_settings: Vec<String>,
    /// How the transport obtains its UDP socket. `None` = the kernel
    /// socket. `mesh-testkit` installs userspace NATs here; every endpoint
    /// this config spawns (daemon, join, pairing) routes through it.
    #[serde(skip)]
    pub socket_factory: Option<crate::transport::SocketFactory>,
    /// Test seam: advertise an older protocol minor to simulate a mixed
    /// mesh. Never set in production.
    #[doc(hidden)]
    #[serde(skip)]
    pub proto_minor_override: Option<u16>,
    /// Test seam: announce this capacity instead of probing the machine
    /// (M9 §5.1). Every in-process test node shares one host, so a
    /// heterogeneous mesh — the whole point of the capacity model — can
    /// only be built by declaring it.
    #[doc(hidden)]
    #[serde(skip)]
    pub capacity_override: Option<mesh_proto::Capacity>,
    /// Test/packaging seam: re-exec **this** binary as the PTY shim rather
    /// than `current_exe()` (M17 §3.6). An in-process test's
    /// `current_exe()` is a test harness that knows nothing about the shim
    /// argument, so a PTY session started from one has to be told.
    #[doc(hidden)]
    #[serde(skip)]
    pub pty_shim_override: Option<PathBuf>,
}

/// Resolved `[wan]` settings (MASTERPLAN M7).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct WanConfig {
    /// Operator-declared public endpoints (`host:port`).
    pub advertise: Vec<String>,
    /// Act as a relay for NATed members (requires verified public
    /// reachability or a non-empty `advertise`).
    pub relay: bool,
    /// When NATed, use relay-tagged members (register + announce
    /// `relay:<id>` candidates).
    pub relay_client: bool,
}

impl WanConfig {
    fn resolve(
        section: &WanSection,
        env_advertise: Option<String>,
        env_relay: Option<String>,
        advertise_override: Option<Vec<String>>,
    ) -> Result<WanConfig> {
        let advertise = advertise_override
            .or_else(|| env_advertise.map(|s| split_list(&s)))
            .or_else(|| section.advertise.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect::<Vec<_>>();
        for a in &advertise {
            if a.starts_with("relay:") {
                return Err(CoreError::Config(format!(
                    "[wan] advertise entries must be host:port, not relay candidates ({a:?})"
                )));
            }
            if !a.contains(':') {
                return Err(CoreError::Config(format!(
                    "[wan] advertise entry {a:?} must be host:port"
                )));
            }
        }
        let relay = match env_relay.as_deref().or(section.relay.as_deref()) {
            None | Some("off") | Some("false") | Some("0") => false,
            Some("on") | Some("true") | Some("1") => true,
            Some(other) => {
                return Err(CoreError::Config(format!(
                    "[wan] relay must be \"on\" or \"off\" (got {other:?})"
                )));
            }
        };
        let relay_client = match section.relay_client.as_deref() {
            None | Some("auto") | Some("on") => true,
            Some("off") => false,
            Some(other) => {
                return Err(CoreError::Config(format!(
                    "[wan] relay_client must be \"auto\" or \"off\" (got {other:?})"
                )));
            }
        };
        Ok(WanConfig {
            advertise,
            relay,
            relay_client,
        })
    }
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Resolved capacity limits. Defaults derive from the machine, not from
/// arbitrary numbers (MASTERPLAN §11).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Limits {
    /// Concurrent executor slots.
    pub slots: u32,
    /// Bounded pending queue depth; beyond it, submissions get `Busy`.
    pub queue: u32,
    /// Memory admission floor in MB (MASTERPLAN §11). 0 = disabled.
    pub min_free_mem_mb: u64,
}

impl Limits {
    pub fn resolve(section: &LimitsSection) -> Limits {
        let slots = match section.slots {
            Some(n) if n > 0 => n,
            _ => derive_slots(),
        };
        let queue = section.queue.unwrap_or_else(|| (slots * 4).max(8));
        Limits {
            slots,
            queue,
            min_free_mem_mb: section.min_free_mem_mb.unwrap_or(256),
        }
    }
}

/// Resolved async-runtime sizing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub worker_threads: usize,
    pub max_blocking_threads: usize,
    /// `None` = the platform default.
    pub thread_stack_bytes: Option<usize>,
    /// Per-job sandboxing (`crate::sandbox`).
    pub sandbox: crate::sandbox::Policy,
    /// The container plugin, when the operator overrode the built-in
    /// engines (N4, T2). Absent is the ordinary case.
    pub container_plugin: Option<String>,
    /// Which engine to prefer where a machine has both (T2).
    pub container_engine: Option<String>,
    /// The volume plugin, when this node provisions volumes (N5 P2).
    pub volume_plugin: Option<String>,
    /// That plugin's own configuration path, handed to it verbatim.
    pub volume_plugin_config: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig {
            worker_threads: default_worker_threads(),
            max_blocking_threads: 512,
            thread_stack_bytes: None,
            sandbox: crate::sandbox::Policy::default(),
            container_plugin: None,
            container_engine: None,
            volume_plugin: None,
            volume_plugin_config: None,
        }
    }
}

impl RuntimeConfig {
    pub fn resolve(section: &RuntimeSection) -> RuntimeConfig {
        let d = RuntimeConfig::default();
        RuntimeConfig {
            worker_threads: match section.worker_threads {
                Some(n) if n > 0 => n,
                _ => d.worker_threads,
            },
            max_blocking_threads: match section.max_blocking_threads {
                Some(n) if n > 0 => n,
                _ => d.max_blocking_threads,
            },
            thread_stack_bytes: section
                .thread_stack_kb
                .filter(|kb| *kb > 0)
                .map(|kb| kb.saturating_mul(1024)),
            container_plugin: section
                .container_plugin
                .clone()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            container_engine: section
                .container_engine
                .clone()
                .map(|p| p.trim().to_ascii_lowercase())
                .filter(|p| !p.is_empty()),
            volume_plugin: section
                .volume_plugin
                .clone()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            volume_plugin_config: section
                .volume_plugin_config
                .clone()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            sandbox: crate::sandbox::Policy {
                // An unknown word is `auto`, never `off`: a typo must not
                // quietly switch the sandbox off.
                mode: section
                    .sandbox
                    .as_deref()
                    .and_then(crate::sandbox::Mode::parse)
                    .unwrap_or_default(),
                read: section
                    .sandbox_read
                    .iter()
                    .flatten()
                    .map(std::path::PathBuf::from)
                    .collect(),
                write: section
                    .sandbox_write
                    .iter()
                    .flatten()
                    .map(std::path::PathBuf::from)
                    .collect(),
                shim: None,
            },
        }
    }
}

/// One async worker per hardware thread. The daemon's own work is I/O bound
/// — gossip, QUIC, IPC — and every genuinely CPU-heavy thing it does (job
/// execution, hashing, redb commits) either lives in a child process or on
/// the blocking pool, so oversubscribing the worker pool buys nothing and
/// costs a scheduler that thrashes.
fn default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}

/// Resolved history-index settings.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct HistoryConfig {
    pub enabled: bool,
    /// Prune indexed rows older than this many days. 0 = keep forever.
    pub keep_days: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        HistoryConfig {
            enabled: true,
            keep_days: 0,
        }
    }
}

impl HistoryConfig {
    pub fn resolve(section: &HistorySection) -> HistoryConfig {
        let d = HistoryConfig::default();
        HistoryConfig {
            enabled: section.enabled.unwrap_or(d.enabled),
            keep_days: section.keep_days.unwrap_or(d.keep_days),
        }
    }
}

/// Resolved log settings.
#[derive(Debug, Clone, Serialize)]
pub struct LogConfig {
    /// `true` = JSON lines; `false` = human-readable text.
    pub json: bool,
    /// When set, log to `<dir>/meshnode.log` with rotation instead of stderr.
    pub dir: Option<PathBuf>,
    pub max_size_bytes: u64,
    pub max_files: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            json: false,
            dir: None,
            max_size_bytes: 20 * 1024 * 1024,
            max_files: 5,
        }
    }
}

/// Resolved job-history retention (MASTERPLAN §9.1).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Retention {
    /// Prune terminal jobs older than this. 0 = keep forever.
    pub job_days: u32,
    /// Total job-history size cap in bytes. 0 = unlimited.
    pub job_max_bytes: u64,
    /// This node's content-store posture (M31 A2).
    pub content: mesh_proto::ContentPolicy,
    /// An explicit window in days, overriding `content`'s rung. `None` =
    /// use the level.
    pub content_days: Option<u32>,
    /// Total content-store size cap in bytes. 0 = unlimited.
    pub content_max_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            job_days: 30,
            job_max_bytes: 1024 * 1024 * 1024,
            content: mesh_proto::ContentPolicy::Balanced,
            content_days: None,
            content_max_bytes: 0,
        }
    }
}

impl Retention {
    /// The window in days this node is *asking* for, before the mesh policy
    /// has its say. `None` where the level is `keep` and no explicit number
    /// overrides it — which `resolve_for_node` reads as "the mesh default".
    pub fn content_request(&self) -> Option<u32> {
        match self.content_days {
            Some(d) => Some(d),
            None if self.content.never_reclaims() => None,
            None => Some(self.content.days()),
        }
    }
}

/// Resolved [`IntegrationsSection`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IntegrationsConfig {
    /// Host patterns this node may send data to. Empty = no restriction.
    pub egress_allow: Vec<String>,
    /// Seconds between export passes. Floored at one second — a zero here
    /// would be a spin loop against somebody's warehouse.
    pub export_interval_secs: u64,
    /// Seconds a row must have been terminal before a sink sees it.
    pub settle_secs: u64,
}

impl IntegrationsConfig {
    pub fn resolve(section: &IntegrationsSection) -> IntegrationsConfig {
        IntegrationsConfig {
            egress_allow: section
                .egress_allow
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.trim().to_ascii_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
            export_interval_secs: section
                .export_interval_secs
                .unwrap_or(crate::daemon::SINK_TICK_SECS)
                .max(1),
            settle_secs: section
                .settle_secs
                .unwrap_or(crate::daemon::SINK_SETTLE_SECS),
        }
    }

    /// May this node send data to `host`?
    ///
    /// An empty allowlist permits everything, which is M15's specified
    /// behaviour and the only defensible default: a control that breaks
    /// every existing deployment on upgrade is a control operators turn off.
    ///
    /// Matching is on the **host only**, case-insensitively, with a leading
    /// `*.` wildcard covering one or more labels. Never the path — a
    /// path-aware allowlist reads as more precise than it is, and invites
    /// somebody to trust it for authorization it cannot provide.
    pub fn allows_host(&self, host: &str) -> bool {
        if self.egress_allow.is_empty() {
            return true;
        }
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.egress_allow.iter().any(|pattern| {
            match pattern.strip_prefix("*.") {
                // `*.internal` matches `a.internal` and `a.b.internal`,
                // and deliberately **not** bare `internal`: a wildcard that
                // matched its own suffix would make `*.corp` permit `corp`,
                // which is not what anybody writing it meant.
                Some(suffix) => host.len() > suffix.len() && host.ends_with(&format!(".{suffix}")),
                None => host == *pattern,
            }
        })
    }

    /// The refusal to print, naming the list so an operator can act on it.
    ///
    /// "Sink unhealthy" sends somebody to the wrong machine; naming the
    /// host and the list sends them to the config file that refused it.
    pub fn refusal_for(&self, host: &str) -> String {
        format!(
            "this node may not send data to {host}: it is not in `[integrations] egress_allow` \
             ({}). Add it there, or point the sink at a node that may reach it (`run_on`).",
            if self.egress_allow.is_empty() {
                "empty".to_string()
            } else {
                self.egress_allow.join(", ")
            }
        )
    }
}

impl Retention {
    pub fn resolve(section: &RetentionSection) -> Retention {
        let d = Retention::default();
        Retention {
            job_days: section.job_days.unwrap_or(d.job_days),
            job_max_bytes: section
                .job_max_mb
                .map(|mb| mb.saturating_mul(1024 * 1024))
                .unwrap_or(d.job_max_bytes),
            // An unparseable level takes the default rather than failing
            // the node's boot. A typo in a tidiness setting is not a reason
            // for a machine not to come back, and `doctor` says so loudly.
            content: section
                .content
                .as_deref()
                .and_then(mesh_proto::ContentPolicy::parse)
                .unwrap_or(d.content),
            content_days: section.content_days,
            content_max_bytes: section
                .content_max_mb
                .map(|mb| mb.saturating_mul(1024 * 1024))
                .unwrap_or(d.content_max_bytes),
        }
    }
}

/// Resolved `[facts]` settings (M18 §4).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct FactsConfig {
    /// Run collectors on this node. **Default true** — see
    /// [`FactsSection::collect`].
    pub collect: bool,
}

impl Default for FactsConfig {
    fn default() -> FactsConfig {
        FactsConfig { collect: true }
    }
}

impl FactsConfig {
    fn resolve(section: &FactsSection) -> Result<FactsConfig> {
        let collect = match section.collect.as_deref() {
            None | Some("on") | Some("true") | Some("1") => true,
            Some("off") | Some("false") | Some("0") => false,
            Some(other) => {
                return Err(CoreError::Config(format!(
                    "[facts] collect must be \"on\" or \"off\" (got {other:?})"
                )));
            }
        };
        Ok(FactsConfig { collect })
    }
}

/// Resolved `[sessions]` settings (M17 §3.8, §3.10).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SessionsConfig {
    /// Accept inbound terminals at all. **Off by default.**
    pub accept: bool,
    /// Concurrent sessions one peer may hold on this node.
    pub max_per_peer: u32,
    /// Record transcripts to `<home>/sessions/<id>/`.
    pub record: bool,
    /// Idle and maximum-duration limits.
    pub limits: crate::sessions::Limits,
    /// Permit a terminal when the daemon runs as uid 0.
    pub allow_root: bool,
}

impl Default for SessionsConfig {
    fn default() -> SessionsConfig {
        SessionsConfig {
            accept: false,
            max_per_peer: mesh_proto::DEFAULT_MAX_SESSIONS_PER_PEER,
            record: false,
            limits: crate::sessions::Limits::default(),
            allow_root: false,
        }
    }
}

impl SessionsConfig {
    fn resolve(section: &SessionsSection) -> Result<SessionsConfig> {
        fn flag(name: &str, value: Option<&str>, default: bool) -> Result<bool> {
            match value {
                None => Ok(default),
                Some("on" | "true" | "1") => Ok(true),
                Some("off" | "false" | "0") => Ok(false),
                Some(other) => Err(CoreError::Config(format!(
                    "[sessions] {name} must be \"on\" or \"off\" (got {other:?})"
                ))),
            }
        }
        let d = SessionsConfig::default();
        let max_per_peer = section.max_per_peer.unwrap_or(d.max_per_peer);
        if max_per_peer == 0 {
            return Err(CoreError::Config(
                "[sessions] max_per_peer must be at least 1 — use accept = \"off\" to refuse them all"
                    .into(),
            ));
        }
        fn duration(
            name: &str,
            value: Option<&str>,
            default: Option<Duration>,
        ) -> Result<Option<Duration>> {
            match value.map(str::trim) {
                None => Ok(default),
                Some("off" | "0" | "none") => Ok(None),
                Some(text) => humantime::parse_duration(text).map(Some).map_err(|_| {
                    CoreError::Config(format!(
                        "[sessions] {name} must be a duration like 30m or 4h, or \"off\" \
                         (got {text:?})"
                    ))
                }),
            }
        }
        let limits = crate::sessions::Limits {
            idle: duration(
                "idle_timeout",
                section.idle_timeout.as_deref(),
                d.limits.idle,
            )?,
            max: duration(
                "max_duration",
                section.max_duration.as_deref(),
                d.limits.max,
            )?,
        };
        if let (Some(idle), Some(max)) = (limits.idle, limits.max)
            && idle > max
        {
            return Err(CoreError::Config(
                "[sessions] idle_timeout is longer than max_duration, so it can never fire".into(),
            ));
        }
        Ok(SessionsConfig {
            accept: flag("accept", section.accept.as_deref(), d.accept)?,
            max_per_peer,
            record: flag("record", section.record.as_deref(), d.record)?,
            limits,
            allow_root: flag("allow_root", section.allow_root.as_deref(), d.allow_root)?,
        })
    }
}

/// Resolved `[compute]` settings (MASTERPLAN M9).
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct ComputeConfig {
    /// RAM budget for ephemeral object bytes, in MB (M9 §5.5).
    pub object_ram_mb: u64,
    /// Uncollected results held past the TTL, in MB (R3 P3).
    pub uncollected_max_mb: u64,
    /// Most this node will stage to workers that cannot see the data (R4 P3).
    pub stage_max_mb: u64,
    /// How long an object outlives its job's completion.
    pub object_ttl: std::time::Duration,
    /// Speculatively re-run the tail of a parallel job (M9 §5.6), or
    /// `None` when the operator turned it off.
    pub straggler: Option<crate::parallel::Straggler>,
    /// Pack or spread (N6 §12). Genuinely hot-reloadable, unlike the rest
    /// of `[compute]` — see `Daemon::apply_config`.
    pub placement: mesh_proto::FitPolicy,
}

/// What this node contributes to, and withholds from, the port ledger
/// (N2 P1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkConfig {
    /// The dynamic range, or `None` when the operator wrote something this
    /// build could not parse.
    ///
    /// `None` is **not** the default range: a node that advertises no range
    /// is disqualified from port work, which is the fail-closed direction.
    /// Substituting the default for a typo would put a service on a node
    /// whose operator was trying to say something else about its ports.
    pub dynamic: Option<mesh_proto::PortRange>,
    /// Sorted, deduplicated, and never handed out.
    pub reserved: Vec<u16>,
    /// Directories this node lends (N5 P1), validated and deduplicated.
    pub volumes: Vec<mesh_proto::Volume>,
    /// Answer `<service>.mesh` here (N3 P3).
    pub dns: bool,
    /// Where the responder binds. `None` when DNS is off.
    pub dns_bind: Option<std::net::SocketAddr>,
    /// Interfaces mesh traffic belongs on (ENG-22), or empty for all of
    /// them. See [`NetworkSection::advertise_interfaces`].
    pub advertise_interfaces: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            dynamic: Some(mesh_proto::PortRange::default()),
            reserved: Vec::new(),
            volumes: Vec::new(),
            dns: true,
            dns_bind: Some(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                mesh_proto::DEFAULT_DNS_PORT,
            ))),
            advertise_interfaces: Vec::new(),
        }
    }
}

impl NetworkConfig {
    pub fn resolve_with_volumes(
        section: &NetworkSection,
        volumes: &[VolumeSection],
    ) -> NetworkConfig {
        NetworkConfig::resolve_with_volumes_and_store(section, volumes, &[])
    }

    /// [`Self::resolve_with_volumes`], plus what this node has
    /// **provisioned for itself** (N5 P2).
    ///
    /// Merged here rather than kept in a second list, because from this
    /// point on a created volume must be indistinguishable from a declared
    /// one: same offer, same claim, same lease, same `cap.volumes` tag,
    /// same release. A separate list would be a second lifecycle, and a
    /// second lifecycle is where the claim and the mount learn to disagree.
    ///
    /// **A hand-written `[[volume]]` wins a name collision.** The operator
    /// typed it; the plugin was only asked.
    pub fn resolve_with_volumes_and_store(
        section: &NetworkSection,
        volumes: &[VolumeSection],
        provisioned: &[mesh_proto::Volume],
    ) -> NetworkConfig {
        let mut out = NetworkConfig::resolve(section);
        // A malformed declaration is **dropped with a reason**, not
        // substituted: a volume nobody can claim is better than a volume
        // that turns out to be a different directory from the one the
        // operator meant.
        let mut seen: Vec<String> = Vec::new();
        for v in volumes {
            let name = v.name.trim();
            let path = v.path.trim();
            if !mesh_proto::volumes::is_valid_volume_name(name) {
                tracing::warn!(
                    concat!(
                        "[[volume]] {:?} is not a usable name (letters, digits, ",
                        "'-' or '_', starting with a letter or digit) — not offered",
                    ),
                    name
                );
                continue;
            }
            if path.is_empty() || !std::path::Path::new(path).is_absolute() {
                tracing::warn!("[[volume]] {name:?} needs an absolute `path` — not offered");
                continue;
            }
            if seen.iter().any(|s| s == name) {
                tracing::warn!("[[volume]] {name:?} is declared twice — the first wins");
                continue;
            }
            seen.push(name.to_string());
            out.volumes.push(mesh_proto::Volume {
                name: name.to_string(),
                path: path.to_string(),
                read_only: v.read_only.unwrap_or(false),
                source: mesh_proto::SOURCE_STATIC.into(),
            });
        }
        for v in provisioned {
            if !mesh_proto::volumes::is_valid_volume_name(&v.name)
                || !std::path::Path::new(&v.path).is_absolute()
            {
                continue;
            }
            if seen.iter().any(|n| n == &v.name) {
                tracing::warn!(
                    concat!(
                        "volume {:?} is both a [[volume]] block and provisioned by a ",
                        "plugin — the block wins, and `meshnode volume rm` will not ",
                        "reach the declared one",
                    ),
                    v.name
                );
                continue;
            }
            seen.push(v.name.clone());
            out.volumes.push(v.clone());
        }

        out
    }

    pub fn resolve(section: &NetworkSection) -> NetworkConfig {
        let dynamic = match section.dynamic_ports.as_deref() {
            None => Some(mesh_proto::PortRange::default()),
            Some(text) => mesh_proto::PortRange::parse(text),
        };
        let mut reserved: Vec<u16> = section
            .reserved_ports
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| *p > 0)
            .collect();
        reserved.sort_unstable();
        reserved.dedup();
        let dns = section.dns.unwrap_or(true);
        let port = section.dns_port.unwrap_or(mesh_proto::DEFAULT_DNS_PORT);
        // A malformed bind address turns the responder **off** with the
        // config named, rather than silently falling back to loopback: the
        // operator who typed `0.0.0.0` and meant it should find out, and
        // the one who typo'd should not quietly get a narrower binding
        // than they asked for.
        let dns_bind = dns.then(|| {
            let host = section.dns_bind.as_deref().unwrap_or("127.0.0.1").trim();
            format!("{host}:{port}")
                .parse::<std::net::SocketAddr>()
                .ok()
        });
        let advertise_interfaces: Vec<String> = section
            .advertise_interfaces
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        NetworkConfig {
            dynamic,
            reserved,
            volumes: Vec::new(),
            dns,
            dns_bind: dns_bind.flatten(),
            advertise_interfaces,
        }
    }

    /// The capacity tag this node announces, or nothing when it advertises
    /// no range.
    pub fn cap_tag(&self) -> Option<String> {
        self.dynamic
            .as_ref()
            .map(|r| format!("{}={}", mesh_proto::CAP_PORTS, r.as_tag_value()))
    }
}

impl Default for ComputeConfig {
    fn default() -> Self {
        ComputeConfig {
            object_ram_mb: crate::objects::default_ram_mb(),
            uncollected_max_mb: 4096,
            stage_max_mb: 1024,
            object_ttl: crate::objects::DEFAULT_TTL,
            straggler: Some(crate::parallel::Straggler::default()),
            // Spread. The mesh meshware is for has nodes that are also
            // somebody's desktop, and the default has to be the one that
            // keeps those responsive.
            placement: mesh_proto::FitPolicy::Spread,
        }
    }
}

impl ComputeConfig {
    pub fn resolve(section: &ComputeSection) -> ComputeConfig {
        let d = ComputeConfig::default();
        ComputeConfig {
            object_ram_mb: section.object_ram_mb.unwrap_or(d.object_ram_mb),
            placement: match section.placement.as_deref().map(str::trim) {
                None | Some("") => d.placement,
                Some(word) => mesh_proto::FitPolicy::parse(word).unwrap_or_else(|| {
                    tracing::warn!(
                        concat!(
                            "[compute] placement = {:?} is not `spread` or `pack` — ",
                            "using spread. A typo must not silently invert every ",
                            "placement this node makes.",
                        ),
                        word
                    );
                    d.placement
                }),
            },
            uncollected_max_mb: section.uncollected_max_mb.unwrap_or(d.uncollected_max_mb),
            stage_max_mb: section.stage_max_mb.unwrap_or(d.stage_max_mb),
            object_ttl: section
                .object_ttl_mins
                .map(|m| std::time::Duration::from_secs(m.saturating_mul(60)))
                .unwrap_or(d.object_ttl),
            straggler: section.speculate.unwrap_or(true).then(|| {
                let d = crate::parallel::Straggler::default();
                crate::parallel::Straggler {
                    // Clamped rather than rejected: a nonsense threshold
                    // should degrade to "race only the last item" or
                    // "never", not refuse to start the node.
                    at: section.speculate_at.unwrap_or(d.at).clamp(0.5, 1.0),
                    min_wait: section
                        .speculate_after_secs
                        .map(std::time::Duration::from_secs)
                        .unwrap_or(d.min_wait),
                }
            }),
        }
    }
}

/// Capacity-derived concurrency default: one slot per core (MASTERPLAN §11).
pub fn derive_slots() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(2)
}

/// A resolved secret provider: plugin path + its JSON-ified config.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderConfig {
    pub plugin: String,
    pub config: serde_json::Value,
}

/// Optional overrides supplied by CLI flags (highest precedence).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub data_dir: Option<PathBuf>,
    pub listen: Option<String>,
    pub mesh_name: Option<String>,
    pub node_name: Option<String>,
    pub tags: Option<Vec<String>>,
    /// `up --advertise HOST:PORT` (repeatable) — MASTERPLAN M7.
    pub advertise: Option<Vec<String>>,
}

/// The home directory when nothing overrides it: `~/.meshware`.
/// See [`crate::layout`] for why it is not `dirs::data_dir()`.
fn default_data_dir() -> PathBuf {
    crate::layout::default_home()
}

fn default_node_name() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The pre-consolidation config path, still read when the home has no
/// config of its own so an upgrade does not silently lose an operator's
/// settings. [`crate::layout::Layout::adopt_legacy`] moves it for good.
pub fn legacy_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("meshware")
        .join("meshnode.toml")
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl NodeConfig {
    /// Load and resolve configuration.
    ///
    /// `explicit_path`: `--config` flag. If given, the file must exist.
    /// Otherwise the default path is used if present, else pure defaults.
    pub fn load(explicit_path: Option<&Path>, overrides: &Overrides) -> Result<Self> {
        // The home is settled before a byte of config is read: the config
        // file lives *inside* it, so it cannot be what decides where it is.
        // Flag, then `$MESHWARE_HOME`, then legacy `$MESHWARE_DATA_DIR`,
        // then `~/.meshware`.
        let home = crate::layout::Layout::resolve(overrides.data_dir.as_deref());

        let (file, retired_settings, config_path): (ConfigFile, Vec<String>, Option<PathBuf>) =
            match explicit_path {
                Some(p) => {
                    let text = std::fs::read_to_string(p).map_err(|e| {
                        CoreError::Config(format!("cannot read {}: {e}", p.display()))
                    })?;
                    let (f, r) = parse(&text, p)?;
                    (f, r, Some(p.to_path_buf()))
                }
                None => {
                    // The home's config first — `/etc/meshware/meshnode.toml`
                    // for a packaged node (DOC-5) — then `<home>/meshnode.toml`,
                    // where a packaged node's config used to be moved to, then
                    // the pre-consolidation path, so upgrading a node does not
                    // quietly reset it to defaults.
                    let mut candidates = vec![home.config_file()];
                    let in_home = home.root().join("meshnode.toml");
                    if !candidates.contains(&in_home) {
                        candidates.push(in_home);
                    }
                    candidates.push(legacy_config_path());
                    match candidates.into_iter().find(|p| p.exists()) {
                        Some(p) => {
                            let text = std::fs::read_to_string(&p).map_err(|e| {
                                CoreError::Config(format!("cannot read {}: {e}", p.display()))
                            })?;
                            let (f, r) = parse(&text, &p)?;
                            (f, r, Some(p))
                        }
                        None => (ConfigFile::default(), Vec::new(), None),
                    }
                }
            };

        // `data_dir` in the file is the escape hatch for putting the heavy
        // directories on a different volume from the config. It is the only
        // way the file gets a say in the home, and it is deliberately last.
        let data_dir = overrides
            .data_dir
            .clone()
            .or_else(|| env_var(crate::layout::HOME_ENV).map(PathBuf::from))
            .or_else(|| env_var(crate::layout::LEGACY_DATA_ENV).map(PathBuf::from))
            .or(file.data_dir)
            .unwrap_or_else(default_data_dir);

        let node_name = overrides
            .node_name
            .clone()
            .or_else(|| env_var("MESHWARE_NODE_NAME"))
            .or(file.node_name)
            .unwrap_or_else(default_node_name);

        let listen_str = overrides
            .listen
            .clone()
            .or_else(|| env_var("MESHWARE_LISTEN"))
            .unwrap_or_else(|| {
                format!(
                    "{}:{}",
                    file.listen.addr.unwrap_or_else(|| "0.0.0.0".to_string()),
                    file.listen.port.unwrap_or(DEFAULT_PORT)
                )
            });
        let listen: SocketAddr = listen_str.parse().map_err(|e| {
            CoreError::Config(format!("invalid listen address {listen_str:?}: {e}"))
        })?;

        let mesh_name = overrides
            .mesh_name
            .clone()
            .or_else(|| env_var("MESHWARE_MESH_NAME"))
            .or(file.mesh.name)
            .unwrap_or_else(|| format!("{node_name}-mesh"));

        let log_level = env_var("MESHWARE_LOG")
            .or(file.log.level)
            .unwrap_or_else(|| "info".to_string());

        let log = LogConfig {
            json: match file.log.format.as_deref() {
                None | Some("text") => false,
                Some("json") => true,
                Some(other) => {
                    return Err(CoreError::Config(format!(
                        "unknown log format {other:?} (text|json)"
                    )));
                }
            },
            dir: file
                .log
                .dir
                .map(|d| if d.is_absolute() { d } else { data_dir.join(d) }),
            max_size_bytes: file
                .log
                .max_size_mb
                .map(|mb| mb.saturating_mul(1024 * 1024).max(64 * 1024))
                .unwrap_or(LogConfig::default().max_size_bytes),
            max_files: file.log.max_files.unwrap_or(5).max(1),
        };

        // `MESHWARE_HTTP=off` disables the endpoint; any other value is an
        // address override.
        // `[api] listen`, or MESHWARE_API. Absent is off, and stays off —
        // there is no default port, because a management API that appears
        // because somebody upgraded is a management API nobody decided to
        // run.
        let api_addr = match env_var("MESHWARE_API")
            .filter(|v| v != "off" && v != "disabled")
            .or_else(|| file.api.listen.clone())
        {
            Some(s) => Some(
                s.parse()
                    .map_err(|e| CoreError::Config(format!("invalid api address {s:?}: {e}")))?,
            ),
            None => None,
        };

        let http_env = env_var("MESHWARE_HTTP");
        let http_addr = match http_env.as_deref() {
            Some("off") | Some("disabled") => None,
            other => {
                let enabled = file.http.enabled.unwrap_or(true);
                if !enabled && other.is_none() {
                    None
                } else {
                    let s = other
                        .map(str::to_string)
                        .or(file.http.addr.clone())
                        .unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_HTTP_PORT}"));
                    Some(s.parse().map_err(|e| {
                        CoreError::Config(format!("invalid http address {s:?}: {e}"))
                    })?)
                }
            }
        };

        let tags = overrides
            .tags
            .clone()
            .or_else(|| {
                env_var("MESHWARE_TAGS").map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
            })
            .or(file.tags)
            .unwrap_or_default();

        let secret_providers = file
            .secrets
            .providers
            .into_iter()
            .map(|(name, entry)| {
                let config = serde_json::to_value(&entry.config)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                (
                    name,
                    ProviderConfig {
                        plugin: entry.plugin,
                        config,
                    },
                )
            })
            .collect();

        let wan = WanConfig::resolve(
            &file.wan,
            env_var("MESHWARE_ADVERTISE"),
            env_var("MESHWARE_RELAY"),
            overrides.advertise.clone(),
        )?;

        // Read before the struct takes ownership of `data_dir`.
        let provisioned = crate::volumes::Store::load(&data_dir).offered();

        Ok(NodeConfig {
            node_name,
            data_dir,
            listen,
            mesh_name,
            log_level,
            tags,
            secret_providers,
            limits: Limits::resolve(&file.limits),
            disk: DiskConfig::resolve(&file.disk),
            quiet: match file.limits.quiet.as_ref().map(QuietSpec::resolve) {
                Some(Ok(q)) => q,
                Some(Err(e)) => return Err(CoreError::Config(e)),
                None => crate::quiet::QuietHours::default(),
            },
            http_addr,
            api_addr,
            log,
            retention: Retention::resolve(&file.retention),
            wan,
            compute: ComputeConfig::resolve(&file.compute),
            // N5 P2 — what this node has provisioned for itself, read back
            // from `<data_dir>/volumes.json`. It is a note this node wrote,
            // not replicated state: a volume is a directory on one machine,
            // so a claim about it that travelled would be a claim no other
            // node could verify or act on.
            network: NetworkConfig::resolve_with_volumes_and_store(
                &file.network,
                &file.volume,
                &provisioned,
            ),
            runtime: RuntimeConfig::resolve(&file.runtime),
            history: HistoryConfig::resolve(&file.history),
            facts: FactsConfig::resolve(&file.facts)?,
            sessions: SessionsConfig::resolve(&file.sessions)?,
            release: ReleaseConfig::resolve(&file.release),
            git: GitConfig::resolve(&file.git)?,
            state: StateConfig::resolve(&file.state)?,
            cache: CacheConfig::resolve(&file.cache),
            integrations: IntegrationsConfig::resolve(&file.integrations),
            notify: NotifyConfig::resolve(&file.notify),
            shutdown: ShutdownConfig::resolve(&file.shutdown),
            config_path,
            retired_settings,
            socket_factory: None,
            proto_minor_override: None,
            capacity_override: None,
            pty_shim_override: None,
        })
    }

    /// Transport bind options derived from this config (socket seam + test
    /// protocol override).
    pub fn bind_options(&self) -> crate::transport::BindOptions {
        crate::transport::BindOptions {
            socket_factory: self.socket_factory.clone(),
            advertised_minor: self.proto_minor_override,
        }
    }

    /// The one home this node writes to. Prefer its named accessors over
    /// joining strings onto `data_dir` — that is how the layout drifted
    /// into three conventions the first time.
    pub fn layout(&self) -> crate::layout::Layout {
        crate::layout::Layout::new(&self.data_dir)
    }

    /// Validate the environment this config points at (dirs writable, etc.).
    pub fn check(&self) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        self.layout().ensure()?;
        if self.node_name.is_empty() {
            return Err(CoreError::Config("node_name must not be empty".into()));
        }
        // The same rule a join applies (SEC-9), so a node never announces a
        // name the mesh would refuse.
        if let Some(problem) = mesh_proto::text::name_problem(&self.node_name) {
            return Err(CoreError::Config(format!("node_name: {problem}")));
        }
        if self.listen.port() == 0 {
            notes.push("listen port 0: kernel will pick an ephemeral port".to_string());
        }
        notes.extend(self.retired_settings.iter().cloned());
        Ok(notes)
    }

    /// IPC socket path. Prefers `XDG_RUNTIME_DIR` (the conventional home for
    /// sockets, and immune to the 108-byte `sun_path` limit that a deep
    /// data_dir would hit), keyed by a hash of the data dir so multiple
    /// nodes on one host don't collide. Falls back to `<data_dir>/run/`.
    pub fn socket_path(&self) -> PathBuf {
        socket_path_for(&self.data_dir)
    }
}

/// [`NodeConfig::socket_path`] derived from a data dir alone.
///
/// Split out because the job executor has to tell a task where this node
/// is listening and holds a `data_dir`, not a `NodeConfig` — and because a
/// second implementation of this hash is exactly the thing that would drift.
pub fn socket_path_for(data_dir: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let hash = blake3::hash(canon.as_os_str().as_encoded_bytes());
        let short = data_encoding::HEXLOWER.encode(&hash.as_bytes()[..6]);
        return PathBuf::from(runtime)
            .join("meshware")
            .join(format!("node-{short}.sock"));
    }
    crate::layout::Layout::new(data_dir)
        .run_dir()
        .join("meshnode.sock")
}

/// One change to a `meshnode.toml`, for [`set_in_file`].
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    Text(String),
    Number(i64),
    Flag(bool),
    /// Take the key out, so the node's default applies again.
    Unset,
}

/// Set keys in a `meshnode.toml` — `"node_name"`, `"retention.content"`,
/// `"mesh.name"` — leaving everything else as the person left it: comments,
/// order, and every key nobody asked about.
///
/// **For a frontend that is a view onto the node** — Director's settings
/// screens — which must change a node's configuration the way its operator
/// would, in the node's own file, and not in a second store beside it that
/// the node never reads. A rewrite through `toml` would drop every comment
/// in a file a person wrote, so this edits it in place.
///
/// The result must still be a configuration this build reads, or nothing
/// is written: a settings screen must not be able to leave a node that
/// refuses to start. Written whole and renamed into place, so a node
/// reloading at that moment reads the old file or the new one, never half.
pub fn set_in_file(path: &Path, edits: &[(&str, Setting)]) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(CoreError::Config(format!(
                "cannot read {}: {e}",
                path.display()
            )));
        }
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
    for (key, setting) in edits {
        let mut parts: Vec<&str> = key.split('.').collect();
        let Some(leaf) = parts.pop() else { continue };
        let mut table = doc.as_table_mut();
        for part in parts {
            let entry = table
                .entry(part)
                .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
            table = entry.as_table_mut().ok_or_else(|| {
                CoreError::Config(format!("{}: `{part}` is not a table", path.display()))
            })?;
        }
        match setting {
            Setting::Text(s) => table[leaf] = toml_edit::value(s.as_str()),
            Setting::Number(n) => table[leaf] = toml_edit::value(*n),
            Setting::Flag(b) => table[leaf] = toml_edit::value(*b),
            Setting::Unset => {
                table.remove(leaf);
            }
        }
    }
    let out = doc.to_string();
    // Round-trip check: what we are about to write must still load. The
    // retired keys it may have carried are the caller's to report, not this
    // path's — an edit that preserved one is not an error.
    let _ = parse(&out, path)?;
    let staged = path.with_extension("toml.writing");
    crate::keyfile::write_private(&staged, out.as_bytes())
        .map_err(|e| CoreError::Config(format!("{}: {e}", staged.display())))?;
    std::fs::rename(&staged, path)
        .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
    Ok(())
}

/// Keys this node no longer has, and what to say about each.
///
/// **A table, not a field per key.** Every section here refuses unknown
/// keys — a misspelling must not be a setting that silently does nothing —
/// so a setting that has been retired still has to be *accepted* by
/// something, or a node upgraded under a config written last month refuses
/// to start over a line that no longer does anything. That is the worst
/// possible way to find out a feature was removed.
///
/// Carrying each one as a typed `Option<T>` field was how that was done, and
/// it cost four places per key: the field, its doc comment, a hand-written
/// `if …is_some()` warning, and an entry in the generated reference that CI
/// then checks forever. Three dead keys were carrying twelve.
///
/// Here they are one line each. The path is dotted from the document root
/// (`network.proxy`), or names a whole table (`inventory`). `strip_retired`
/// removes them before `deny_unknown_fields` ever sees them, and the node
/// says so once at startup, by name, with what to do instead.
const RETIRED_KEYS: &[(&str, &str)] = &[
    (
        "network.proxy",
        "[network] proxy is ignored: the mesh proxy is gone. An instance is reached at its \
         node's address and port — `meshnode registry <name>` lists them — and a stable name \
         goes in a load balancer in front",
    ),
    (
        "network.proxy_prefix",
        "[network] proxy_prefix is ignored: the mesh proxy is gone, and the prefix went with it",
    ),
    (
        "history.resident_runs",
        "[history] resident_runs is ignored: it bounded how many workflow-run plans stayed in \
         memory, and the run index went with the workflow engine",
    ),
    (
        "history.shed_after_mins",
        "[history] shed_after_mins is ignored: it paced the same run-plan shedding",
    ),
    (
        "inventory",
        "[inventory] is ignored: the host inventory was removed from meshware, and the section \
         can be deleted",
    ),
];

/// Remove every retired key present in `doc`, and say which were there.
///
/// Returns the warnings in `RETIRED_KEYS` order, so two nodes reading the
/// same file print the same lines in the same order.
fn strip_retired(doc: &mut toml::Table) -> Vec<String> {
    let mut found = Vec::new();
    for (path, message) in RETIRED_KEYS {
        let removed = match path.split_once('.') {
            // A whole table: `[inventory]`.
            None => doc.remove(*path).is_some(),
            // One key inside one: `network.proxy`.
            Some((table, key)) => doc
                .get_mut(table)
                .and_then(toml::Value::as_table_mut)
                .and_then(|t| t.remove(key))
                .is_some(),
        };
        if removed {
            found.push((*message).to_string());
        }
    }
    // A table left empty only because its keys were retired is not a table
    // the operator still means to have. Dropping it keeps `[network]` from
    // deserializing as a present-but-empty section when the file's only
    // `[network]` lines were `proxy` and `proxy_prefix`.
    doc.retain(|_, v| !matches!(v.as_table(), Some(t) if t.is_empty()));
    found
}

fn parse(text: &str, path: &Path) -> Result<(ConfigFile, Vec<String>)> {
    let mut doc: toml::Table =
        toml::from_str(text).map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
    let retired = strip_retired(&mut doc);
    let file = doc
        .try_into()
        .map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?;
    Ok((file, retired))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config written while the mesh proxy existed still loads, and says
    /// so. The node refusing to start over a line that no longer does
    /// anything is the failure `RETIRED_KEYS` exists to prevent.
    #[test]
    fn a_retired_key_loads_and_is_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meshnode.toml");
        let (file, retired) = parse(
            "node_name = \"anvil\"\n\n[network]\nproxy = true\nproxy_prefix = \"/mesh\"\n\
             dynamic_ports = \"20000-32000\"\n\n[inventory]\nprobe = \"on\"\n",
            &path,
        )
        .expect("a file with retired keys still loads");

        assert_eq!(file.node_name.as_deref(), Some("anvil"));
        // The live key beside them is untouched.
        assert_eq!(file.network.dynamic_ports.as_deref(), Some("20000-32000"));
        // One line per retired key, in table order, each naming the key.
        assert_eq!(retired.len(), 3, "three retired keys, three warnings");
        assert!(retired[0].contains("[network] proxy is ignored"));
        assert!(retired[1].contains("proxy_prefix"));
        assert!(retired[2].contains("[inventory]"));
    }

    /// A `[network]` whose only lines were retired is not a `[network]` the
    /// operator still means to have, and must not deserialize as one.
    #[test]
    fn a_table_emptied_by_retirement_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meshnode.toml");
        let (file, retired) = parse("[network]\nproxy = false\n", &path).expect("loads");
        assert_eq!(retired.len(), 1);
        assert!(file.network.dynamic_ports.is_none());
    }

    /// The ordinary case pays nothing: no retired keys, no warnings, and an
    /// unknown key is still refused.
    #[test]
    fn an_unknown_key_is_still_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meshnode.toml");
        let (_, retired) = parse("node_name = \"hq\"\n", &path).expect("loads");
        assert!(retired.is_empty(), "nothing retired, nothing said");

        let err = parse("[network]\nproxxy = true\n", &path)
            .expect_err("a misspelling is not a retirement");
        assert!(
            format!("{err}").contains("proxxy"),
            "the error names the key"
        );
    }

    #[test]
    fn a_settings_edit_keeps_what_the_person_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meshnode.toml");
        std::fs::write(
            &path,
            "# my laptop\nnode_name = \"NIM\"\n\n[retention]\n# keep it small\ncontent = \"tidy\"\n",
        )
        .expect("write");

        set_in_file(
            &path,
            &[
                ("retention.content", Setting::Text("ephemeral".into())),
                ("retention.content_max_mb", Setting::Number(2048)),
                ("network.dns", Setting::Flag(false)),
            ],
        )
        .expect("edit");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("# my laptop") && text.contains("# keep it small"),
            "{text}"
        );
        let cfg: ConfigFile = toml::from_str(&text).expect("still a config");
        assert_eq!(cfg.node_name.as_deref(), Some("NIM"));
        assert_eq!(cfg.retention.content.as_deref(), Some("ephemeral"));
        assert_eq!(cfg.retention.content_max_mb, Some(2048));
        assert_eq!(cfg.network.dns, Some(false));

        set_in_file(&path, &[("retention.content_max_mb", Setting::Unset)]).expect("unset");
        let cfg: ConfigFile =
            toml::from_str(&std::fs::read_to_string(&path).expect("read")).expect("config");
        assert_eq!(cfg.retention.content_max_mb, None);
    }

    #[test]
    fn a_settings_edit_that_would_break_the_config_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meshnode.toml");
        std::fs::write(&path, "node_name = \"NIM\"\n").expect("write");
        assert!(set_in_file(&path, &[("no_such_key", Setting::Flag(true))]).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "node_name = \"NIM\"\n"
        );
        // …and a missing file is simply a new one.
        let fresh = dir.path().join("new.toml");
        set_in_file(&fresh, &[("mesh.name", Setting::Text("home".into()))]).expect("new");
        let cfg: ConfigFile =
            toml::from_str(&std::fs::read_to_string(&fresh).expect("read")).expect("config");
        assert_eq!(cfg.mesh.name.as_deref(), Some("home"));
    }

    #[test]
    fn defaults_resolve() {
        let cfg = NodeConfig::load(
            None,
            &Overrides {
                data_dir: Some(std::env::temp_dir().join("mw-test-cfg")),
                ..Default::default()
            },
        )
        .expect("load defaults");
        assert_eq!(cfg.listen.port(), DEFAULT_PORT);
        assert!(!cfg.node_name.is_empty());
        assert!(cfg.mesh_name.ends_with("-mesh"));
    }

    #[test]
    fn file_and_override_precedence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("meshnode.toml");
        std::fs::write(
            &p,
            r#"
node_name = "from-file"
[listen]
port = 9999
[mesh]
name = "hyacinth"
"#,
        )
        .expect("write config");
        let cfg = NodeConfig::load(Some(&p), &Overrides::default()).expect("load");
        assert_eq!(cfg.node_name, "from-file");
        assert_eq!(cfg.listen.port(), 9999);
        assert_eq!(cfg.mesh_name, "hyacinth");

        let cfg2 = NodeConfig::load(
            Some(&p),
            &Overrides {
                listen: Some("127.0.0.1:1234".into()),
                ..Default::default()
            },
        )
        .expect("load with override");
        assert_eq!(cfg2.listen.port(), 1234);
    }

    #[test]
    fn the_compaction_horizon_is_one_knob_and_defaults_to_thirty_days() {
        let d = StateConfig::resolve(&StateSection::default()).expect("default");
        assert!(d.compaction_enabled);
        assert_eq!(d.horizon_secs(), 30 * 24 * 3600);

        let c = StateConfig::resolve(&StateSection {
            compaction_horizon: Some("7d".into()),
            compaction_enabled: Some(false),
            ..Default::default()
        })
        .expect("7d");
        assert_eq!(c.horizon_secs(), 7 * 24 * 3600);
        assert!(!c.compaction_enabled);

        // A typo must refuse to start rather than silently mean 30 days —
        // an operator who wrote `30 days` believes they set a horizon.
        let err = StateConfig::resolve(&StateSection {
            compaction_horizon: Some("30 dayz".into()),
            ..Default::default()
        })
        .expect_err("a typo must not resolve");
        assert!(err.to_string().contains("not a duration"), "{err}");
    }

    #[test]
    fn wan_section_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("meshnode.toml");
        std::fs::write(
            &p,
            r#"
[wan]
advertise = ["203.0.113.5:7777", " 198.51.100.9:7777 "]
relay = "on"
relay_client = "off"
"#,
        )
        .expect("write config");
        let cfg = NodeConfig::load(Some(&p), &Overrides::default()).expect("load");
        assert_eq!(
            cfg.wan.advertise,
            vec![
                "203.0.113.5:7777".to_string(),
                "198.51.100.9:7777".to_string()
            ]
        );
        assert!(cfg.wan.relay);
        assert!(!cfg.wan.relay_client);

        // Flag beats file.
        let cfg2 = NodeConfig::load(
            Some(&p),
            &Overrides {
                advertise: Some(vec!["10.0.0.1:7777".into()]),
                ..Default::default()
            },
        )
        .expect("load");
        assert_eq!(cfg2.wan.advertise, vec!["10.0.0.1:7777".to_string()]);

        // Defaults: nothing advertised, relay off, relay client auto.
        let cfg3 = NodeConfig::load(
            None,
            &Overrides {
                data_dir: Some(std::env::temp_dir().join("mw-test-wan")),
                ..Default::default()
            },
        )
        .expect("defaults");
        assert!(cfg3.wan.advertise.is_empty());
        assert!(!cfg3.wan.relay);
        assert!(cfg3.wan.relay_client);

        std::fs::write(&p, "[wan]\nrelay = \"maybe\"\n").expect("write");
        assert!(NodeConfig::load(Some(&p), &Overrides::default()).is_err());
        std::fs::write(&p, "[wan]\nadvertise = [\"relay:mw1abc\"]\n").expect("write");
        assert!(NodeConfig::load(Some(&p), &Overrides::default()).is_err());
    }

    #[test]
    fn unknown_fields_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("meshnode.toml");
        std::fs::write(&p, "definitely_not_a_field = 1\n").expect("write config");
        assert!(NodeConfig::load(Some(&p), &Overrides::default()).is_err());
    }
}
