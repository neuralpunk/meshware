//! View models crossing the Tauri IPC boundary — the JSON contract between
//! the embedded engine and the Svelte frontend. TypeScript bindings are
//! generated from these types with ts-rs (`cargo test -p meshware-director
//! export_bindings`), so UI and engine never drift (MASTERPLAN §14.1).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use mesh_proto::{
    AuditEntry, BundleRow, FanTarget, GrantRow, IpcResponse, JobRow, JobState, Liveness, NodeRow,
    PingResult, ProviderRow, SecretRow, ServiceRow, SourceRow, StatusInfo, TopRow, UserRow,
    WanInfo, WhoAmIInfo,
};

fn liveness_str(l: Liveness) -> &'static str {
    match l {
        Liveness::Alive => "alive",
        Liveness::Suspect => "suspect",
        Liveness::Dead => "dead",
    }
}

fn state_str(s: JobState) -> String {
    s.as_str().to_string()
}

/// Whether this person's key is sealed, and whether it is open right now (B9).
///
/// Director reads the same `~/.meshware/identity/user.key` the CLI does and
/// the same session unlock under `$XDG_RUNTIME_DIR`, so `meshnode identity
/// unlock` in a terminal and the dialog in Director are two doors onto one
/// state. This view is what the gate and the Settings panel both read.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct IdentityLockView {
    pub path: String,
    /// A key file exists. `false` before first run, when one is created on
    /// demand.
    pub exists: bool,
    /// Sealed with a passphrase. `false` is a plaintext 32-byte seed — the
    /// format before B9, and still valid — or a key on a security key.
    pub protected: bool,
    /// The file names a key an agent holds (Y2/Y3): there is no secret in
    /// this file to protect, and nothing to unlock.
    pub hardware: bool,
    /// For a `hardware` key: the agent is holding it right now, so Director
    /// can start. `false` is a key that is unplugged or not connected —
    /// asked by listing the agent's keys, never by asking it to sign.
    pub hardware_ready: bool,
    /// Open for this login session.
    pub unlocked: bool,
    /// Seconds left on the unlock, for a panel that says "12h" rather than
    /// "yes". A string because JSON numbers are f64 and this crosses the
    /// boundary as an exact count.
    pub unlocked_for_secs: Option<String>,
    /// Where the unlock is kept. `None` where there is no
    /// `$XDG_RUNTIME_DIR` — then there is no session at all, and Director
    /// says so rather than offering an unlock that cannot be remembered.
    pub session_store: Option<String>,
}

/// What the frontend needs before (and during) the first-run wizard.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapView {
    pub has_mesh: bool,
    pub running: bool,
    pub hostname: String,
    pub user_id: String,
    pub data_dir: String,
    /// This Director is running a demo mesh in a scratch home (L1 P3), not
    /// a real one. Drives the banner that offers to turn it into one.
    pub demo: bool,
    /// A demo could be started here: there is a `meshnode` binary this
    /// machine can run the other machines with.
    pub demo_available: bool,
}

// -- build farm (M9 §10) ----------------------------------------------------

/// The exact shell line a build farm run will execute, and what went into
/// it.
///
/// Returned rather than composed in the frontend for the reason the whole
/// feature exists: the two facts that make the line *work* — which binary is
/// the compiler wrapper, and which mesh home it should talk to — are known
/// only to the running process. A panel that guessed at them is how the old
/// copy-and-paste line came to be wrong.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct FarmBuildLine {
    /// What runs, in one `bash -lc` line. Shown, copyable, and executed
    /// verbatim — there is no second, hidden version of this.
    pub line: String,
    /// The binary that will be `CC`/`CXX`. This Director, normally.
    pub wrapper: String,
    /// The mesh home the wrapper is pointed at, as `MESHWARE_HOME`.
    pub home: String,
    /// True when the build command is make-shaped, so the assignments are
    /// appended as arguments as well as exported. See `farm_build_line`.
    pub make_style: bool,
    /// Anything the operator should know before pressing Run — an empty
    /// list is the good case.
    pub warnings: Vec<String>,
}

// -- history and archive (M14) ----------------------------------------------

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct HistoryJobView {
    pub job: String,
    pub command: String,
    pub runtime: String,
    pub executor: String,
    pub submitter_user: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub created_at_micros: String,
    pub finished_at_micros: Option<String>,
    pub duration_ms: Option<String>,
    /// This row came out of Parquet rather than the local index
    /// (M31 B3 §14.2 item 2). Rendered as a marker on the row, because a
    /// page-level flag on a merged page tells the reader nothing about the
    /// row they are looking at.
    pub from_archive: bool,
    /// The destination a sink exported this job's output to, if one did
    /// (M31 B3 §14.3 item 1). Present means the output can be *read back*;
    /// the key itself never reaches the frontend, because the client names
    /// a job and the daemon looks the locator up.
    pub payload_sink: Option<String>,
    /// Where this job's output is: `available`, `unavailable`, `expired`
    /// or `elsewhere` — A2's three states plus B3's fourth, computed by the
    /// daemon at answer time. Empty when nobody worked it out.
    pub output_state: String,
    /// The short form for a table cell — one phrase, composed in
    /// `mesh-proto` so no renderer writes its own.
    pub output_short: String,
    /// The sentence, for the panel. Empty when the output is simply here.
    pub output_message: String,
}

impl From<mesh_proto::HistoryJobRow> for HistoryJobView {
    fn from(r: mesh_proto::HistoryJobRow) -> Self {
        let duration = r.duration_micros().map(|d| (d / 1000).to_string());
        HistoryJobView {
            job: r.job_id.to_display_string(),
            command: r.cmd_preview,
            runtime: r.runtime,
            executor: r.executor.to_display_string(),
            submitter_user: r.submitter_user.to_display_string(),
            state: r.state.as_str().to_string(),
            exit_code: r.exit_code,
            created_at_micros: r.created_at_micros.to_string(),
            finished_at_micros: r.finished_at_micros.map(|v| v.to_string()),
            duration_ms: duration,
            from_archive: r.from_archive,
            payload_sink: r.payload_sink,
            output_state: r
                .output
                .as_ref()
                .map(|o| o.as_str().to_string())
                .unwrap_or_default(),
            output_short: r.output.as_ref().map(|o| o.short()).unwrap_or_default(),
            output_message: r.output.as_ref().map(|o| o.message()).unwrap_or_default(),
        }
    }
}

/// Which tiers answered a browse (M31 B3 §14.2).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TierView {
    /// Rows from `history.redb` — instant, recent.
    pub from_index: u32,
    /// Rows from Parquet — complete, slower.
    pub from_archive: u32,
    /// The peer whose archive answered, when it was not this node.
    pub archive_peer: String,
    /// No archive answered at all. **Not the same as "nothing matched"** —
    /// the screen says so, because a caller that conflates them reports
    /// data loss.
    pub archive_unreachable: bool,
    /// Why it did not, verbatim.
    pub archive_status: String,
    /// The oldest instant this browse could see, when the window asked for
    /// something older (M31 A2 item 4).
    pub retained_from_micros: Option<String>,
    /// The window reached past what this mesh still keeps.
    pub window_reached_past: bool,
}

impl TierView {
    pub fn of(tiers: mesh_proto::TierReport, absence: mesh_proto::KnownAbsence) -> Self {
        TierView {
            from_index: tiers.from_index.min(u64::from(u32::MAX)) as u32,
            from_archive: tiers.from_archive.min(u64::from(u32::MAX)) as u32,
            archive_peer: tiers.archive_peer,
            archive_unreachable: tiers.archive_unreachable,
            archive_status: tiers.archive_status,
            retained_from_micros: (absence.retained_from_micros > 0)
                .then(|| absence.retained_from_micros.to_string()),
            window_reached_past: absence.window_reached_past,
        }
    }
}

/// One window of a job's output, read back from wherever a sink put it
/// (M31 B3 §14.3).
///
/// **`status` degrades one panel and never breaks a screen.** A destination
/// that is down produces an empty window and a sentence naming it, which is
/// Q7's degrade-versus-break test as a type.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PayloadWindowView {
    pub lines: Vec<String>,
    /// There is more above this window — the panel offers "earlier".
    pub more_before: bool,
    pub status: String,
}

impl From<mesh_proto::PayloadWindowReply> for PayloadWindowView {
    fn from(r: mesh_proto::PayloadWindowReply) -> Self {
        PayloadWindowView {
            lines: r.lines,
            more_before: r.more_before,
            status: r.status,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct HistoryJobPageView {
    pub rows: Vec<HistoryJobView>,
    pub total: Option<u32>,
    pub tiers: TierView,
}

/// WAN reachability as the engine sees it (MASTERPLAN M7).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WanView {
    /// unknown | public | natted | symmetric-nat
    pub reachability: String,
    pub reflexive_addr: Option<String>,
    pub advertise: Vec<String>,
    pub relay_server: bool,
    pub relay_clients: u32,
    pub relays: Vec<String>,
}

impl From<WanInfo> for WanView {
    fn from(w: WanInfo) -> Self {
        WanView {
            reachability: w.reachability,
            reflexive_addr: w.reflexive_addr,
            advertise: w.advertise,
            relay_server: w.relay_server,
            relay_clients: w.relay_clients,
            relays: w.relays,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    pub version: String,
    pub proto: String,
    pub node_id: String,
    pub node_short: String,
    pub node_name: String,
    pub mesh_id: Option<String>,
    pub mesh_name: Option<String>,
    pub listen_addr: String,
    #[ts(type = "number")]
    pub uptime_secs: u64,
    #[ts(type = "number")]
    pub members_alive: u64,
    #[ts(type = "number")]
    pub members_total: u64,
    #[ts(type = "number")]
    pub jobs_running: u64,
    #[ts(type = "number")]
    pub jobs_queued: u64,
    #[ts(type = "number")]
    pub slots: u64,
    /// The node is draining: finishing accepted work, refusing new work.
    pub draining: bool,
    /// Whether this Director peer currently contributes compute.
    pub executing: bool,
    pub wan: Option<WanView>,
    /// The node genesis names as this mesh's founder, and whether that is
    /// this peer. Only the founder may repair another node's recorded
    /// address, so the surface that offers the repair has to know.
    pub founder_node: String,
    pub is_founder: bool,
    /// A timed drain in force (`meshnode drain --until`): when it lifts,
    /// as Unix seconds, and why. Both absent when the node is not draining
    /// or is draining for good.
    #[ts(type = "number | null")]
    pub drain_until_secs: Option<u64>,
    pub drain_reason: Option<String>,
    /// `spread` or `pack` — how this node fills the mesh when it dispatches
    /// work (N6 §12). A string rather than a boolean because there will be
    /// a third answer before there is a second boolean.
    pub placement: String,
    /// What this peer's own dispatches have cost (N6 §12): how many, and
    /// the bucket edges the median and p95 landed in, in **milliseconds**.
    /// All absent until this peer has placed something — a peer that has
    /// dispatched nothing has no latency, and zeroes would read as instant.
    #[ts(type = "number | null")]
    pub dispatch_count: Option<u64>,
    #[ts(type = "number | null")]
    pub dispatch_p50_ms: Option<f64>,
    #[ts(type = "number | null")]
    pub dispatch_p95_ms: Option<f64>,
}

impl StatusView {
    pub fn from(info: StatusInfo, executing: bool) -> Self {
        StatusView {
            version: info.version,
            proto: format!("{}.{}", info.proto_major, info.proto_minor),
            node_id: info.node_id.to_display_string(),
            node_short: info.node_id.short(),
            node_name: info.node_name,
            mesh_id: info.mesh_id.map(|m| m.to_display_string()),
            mesh_name: info.mesh_name,
            listen_addr: info.listen_addr,
            uptime_secs: info.uptime_secs,
            members_alive: info.members_alive as u64,
            members_total: info.members_total as u64,
            jobs_running: info.jobs_running as u64,
            jobs_queued: info.jobs_queued as u64,
            slots: info.slots as u64,
            draining: info.draining,
            executing,
            wan: info.wan.map(Into::into),
            founder_node: info.founder_node.to_display_string(),
            is_founder: info.founder_node == info.node_id,
            drain_until_secs: info.drain.as_ref().map(|d| d.until_micros / 1_000_000),
            drain_reason: info
                .drain
                .as_ref()
                .map(|d| d.reason.clone())
                .filter(|r| !r.is_empty()),
            placement: info.placement.as_str().to_string(),
            dispatch_count: info.dispatch.as_ref().map(|d| d.count),
            dispatch_p50_ms: info
                .dispatch
                .as_ref()
                .and_then(|d| d.p50_secs)
                .map(|s| s * 1000.0),
            dispatch_p95_ms: info
                .dispatch
                .as_ref()
                .and_then(|d| d.p95_secs)
                .map(|s| s * 1000.0),
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct NodeView {
    pub node_id: String,
    pub short_id: String,
    pub name: String,
    pub addrs: Vec<String>,
    pub tags: Vec<String>,
    /// alive | suspect | dead
    pub liveness: String,
    pub is_self: bool,
    pub revoked: bool,
    pub left: bool,
    #[ts(type = "number")]
    pub joined_at_micros: u64,
    pub executing: bool,
    /// Advertised hardware (MASTERPLAN M9 §5.1), `null` on a node that
    /// predates capacity announcements. Node information, shown on Nodes
    /// regardless of who consumes it (§6.4).
    pub capacity: Option<CapacityView>,
    /// Set when this peer predates the forward-compatible op-log and is
    /// being served less than the full op stream (F0 §3.3). Not a fault:
    /// it is the expected middle of a rolling upgrade, and it clears when
    /// the peer upgrades.
    pub stale: Option<StalePeerView>,
    /// This node accepts interactive terminals (M17 §3.4) — its own
    /// statement, so it is true from anywhere rather than only readable in
    /// a TOML file on that machine.
    pub accepts_terminals: bool,
    /// …and this node is tagged `no-shell`: it runs jobs, but it is not a
    /// place anybody gets a prompt.
    pub no_shell: bool,
    /// How much of this node's op log the peer had at its last gossip
    /// exchange with us. `null` for this peer itself, and for a member that
    /// has not synced since the daemon started.
    pub sync: Option<SyncProgressView>,
    /// The meshnode build this node announced (`cap.ver`), M24 §4. `null`
    /// for a node that predates version announcement — a real state, and
    /// shown as unknown rather than guessed either way.
    pub version: Option<String>,
    /// Whether the mesh's owner allows work to be placed here. `true` when
    /// nobody has decided, which is every node on a mesh that has never
    /// used the control.
    ///
    /// Not the same as [`Self::executing`], which is the node's own
    /// statement about itself and which it rewrites on every boot. This is
    /// the owner's answer, signed by an admin, and the node cannot clear it.
    pub contributes: bool,
    /// Isolated pending investigation (E2.4), with the reason. `null` is
    /// the ordinary case. Distinct from `contributes` — that is a capacity
    /// decision, this is *"nobody go near it"*.
    pub quarantined: Option<String>,
    /// Why the owner is not sending work here. Empty when they are.
    pub contribute_reason: String,
    /// Which of [`Self::tags`] an admin **assigned** from the mesh, rather
    /// than this node announcing them itself.
    ///
    /// A subset of `tags`, not a second list. It is here so the tag editor
    /// can offer to remove the half it is able to remove: an assigned tag
    /// comes off from anywhere, and a node's own announcement only comes
    /// off on that node.
    pub assigned_tags: Vec<String>,
}

/// How far one peer trailed this node's op log the last time it asked.
///
/// A statement about one conversation, not a mesh-wide truth: a gossip mesh
/// has no vantage point from which "the mesh is 97% synced" would mean
/// anything, and the surface is careful to say "as of 4s ago, from here".
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SyncProgressView {
    #[ts(type = "number")]
    pub behind: u64,
    #[ts(type = "number")]
    pub total: u64,
    pub authors: u32,
    #[ts(type = "number")]
    pub age_micros: u64,
    /// How much of the log the peer had, 0–100. Carried rather than derived
    /// in the client so the "total is zero" case has exactly one answer.
    pub pct: u8,
}

impl From<mesh_proto::SyncProgress> for SyncProgressView {
    fn from(p: mesh_proto::SyncProgress) -> Self {
        // A log with nothing in it is fully replicated by definition —
        // there is nothing anyone could be missing.
        let pct = if p.total == 0 {
            100
        } else {
            let have = p.total.saturating_sub(p.behind);
            u8::try_from(have.saturating_mul(100) / p.total).unwrap_or(100)
        };
        SyncProgressView {
            behind: p.behind,
            total: p.total,
            authors: p.authors,
            age_micros: p.age_micros,
            pct,
        }
    }
}

/// Why a peer cannot receive everything this node has (F0 §3.3).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StalePeerView {
    pub minor: u16,
    pub authors_blocked: u32,
    #[ts(type = "number")]
    pub ops_withheld: u64,
}

impl From<mesh_proto::StalePeer> for StalePeerView {
    fn from(s: mesh_proto::StalePeer) -> Self {
        StalePeerView {
            minor: s.minor,
            authors_blocked: s.authors_blocked,
            ops_withheld: s.ops_withheld,
        }
    }
}

/// What hardware a node says it has (M9 §5.1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CapacityView {
    pub cores: u32,
    #[ts(type = "number")]
    pub mem_mb: u64,
    #[ts(type = "number")]
    pub disk_mb: u64,
    pub arch: String,
}

impl From<mesh_proto::Capacity> for CapacityView {
    fn from(c: mesh_proto::Capacity) -> Self {
        CapacityView {
            cores: c.cores,
            mem_mb: c.mem_mb,
            disk_mb: c.disk_mb,
            arch: c.arch,
        }
    }
}

impl From<NodeRow> for NodeView {
    fn from(r: NodeRow) -> Self {
        let executing = !r.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG);
        let no_shell = r.tags.iter().any(|t| t == mesh_proto::NO_SHELL_TAG);
        let accepts_terminals = r.accepts_terminals;
        NodeView {
            accepts_terminals,
            no_shell,
            node_id: r.node_id.to_display_string(),
            short_id: r.node_id.short(),
            name: r.name,
            addrs: r.addrs,
            tags: r.tags,
            liveness: liveness_str(r.liveness).to_string(),
            is_self: r.is_self,
            revoked: r.revoked,
            left: r.left,
            joined_at_micros: r.joined_at_micros,
            executing,
            capacity: r.capacity.map(Into::into),
            stale: r.stale.map(Into::into),
            sync: r.sync.map(Into::into),
            version: r.version,
            contributes: r.contributes,
            quarantined: r.quarantined.clone(),
            contribute_reason: r.contribute_reason,
            assigned_tags: r.assigned_tags,
        }
    }
}

/// One node's saturation (`meshnode top`, MASTERPLAN §10.2).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TopView {
    pub node_id: String,
    pub short_id: String,
    pub name: String,
    pub liveness: String,
    pub is_self: bool,
    pub no_execute: bool,
    pub slots: u32,
    pub running: u32,
    pub queued: u32,
    /// running / slots, 0–100 (0 for non-executing peers).
    pub saturation_pct: u32,
    /// Advertised hardware threads (M9 §5.1); 0 = not advertised.
    pub cores: u32,
    /// Live 1-minute load per core (M9 §5.2). `null` when unknown — a peer
    /// on an older minor, a reading that aged out, or one only reachable
    /// through a relay. Unknown is neutral, never "unhealthy".
    pub load_per_core: Option<f64>,
    /// Live `MemAvailable`, in MB. `null` when unknown.
    #[ts(type = "number | null")]
    pub mem_available_mb: Option<u64>,
    /// **What the machine is actually doing** (`TELEMETRY_MINOR`): CPU
    /// utilisation, memory and disk in use, all 0–100.
    ///
    /// Distinct from `load_per_core`, and the distinction is why these
    /// exist: a box with every core pegged reports a load per core of 0.2
    /// for the first minute of it, because `load1` is a one-minute average
    /// of the run queue. `cpu_pct` moves the moment the work starts.
    pub cpu_pct: Option<f64>,
    pub mem_used_pct: Option<f64>,
    pub disk_used_pct: Option<f64>,
    /// The worst of the above, 0–1 — one number for "how loaded is this",
    /// which is what a bar in a table wants.
    pub busy: Option<f64>,
    /// Seconds since the machine booted. A node that keeps coming back
    /// with a small number here is rebooting, which nothing else says.
    #[ts(type = "number | null")]
    pub uptime_secs: Option<u64>,
    /// Network throughput, bytes per second in and out. A **rate, not a
    /// fraction** — there is no link speed the machine can honestly report
    /// to divide by — so it is shown as itself and never as a bar.
    #[ts(type = "number | null")]
    pub net_rx_bps: Option<u64>,
    #[ts(type = "number | null")]
    pub net_tx_bps: Option<u64>,
    /// Reservations in flight on this node, and the cores behind them
    /// (C4 P1). Zero on a mesh that has never run a gang, which is what
    /// keeps the column out of the way until there is something in it.
    pub leases: u32,
    pub leased_cpus: u32,
    /// Accelerators this node advertises (C5 P7). Zero on a mesh with no
    /// GPUs, which keeps the column out of the way until there is
    /// something to put in it.
    pub devices: u32,
    /// How many of them are spoken for. **Exact for this node and `null`
    /// for every other**, because a device hold is node-local and there is
    /// no honest way to read a remote node's ledger — the same limit the
    /// lease column states rather than a number nobody can stand behind.
    pub devices_busy: Option<u32>,
    /// How busy the idlest advertised device is, 0–100. `null` when
    /// unknown, which is neutral — never idle and never busy.
    pub device_busy_pct: Option<u32>,
}

impl From<TopRow> for TopView {
    fn from(r: TopRow) -> Self {
        let saturation_pct = r
            .running
            .saturating_mul(100)
            .checked_div(r.slots)
            .unwrap_or(0)
            .min(100);
        TopView {
            node_id: r.node_id.to_display_string(),
            short_id: r.node_id.short(),
            name: r.name,
            liveness: liveness_str(r.liveness).to_string(),
            is_self: r.is_self,
            no_execute: r.no_execute,
            cores: r.cores,
            load_per_core: r
                .pressure
                .as_ref()
                .and_then(|p| p.load_per_core(if r.cores > 0 { r.cores } else { r.slots })),
            mem_available_mb: r.pressure.as_ref().and_then(|p| p.mem_available_mb),
            cpu_pct: r.system.as_ref().and_then(|s| s.cpu_pct),
            mem_used_pct: r
                .system
                .as_ref()
                .and_then(mesh_proto::SystemSnapshot::mem_used_pct),
            disk_used_pct: r
                .system
                .as_ref()
                .and_then(mesh_proto::SystemSnapshot::disk_used_pct),
            busy: r.system.as_ref().and_then(mesh_proto::SystemSnapshot::busy),
            uptime_secs: r.system.as_ref().and_then(|s| s.uptime_secs),
            net_rx_bps: r.system.as_ref().and_then(|s| s.net_rx_bps),
            net_tx_bps: r.system.as_ref().and_then(|s| s.net_tx_bps),
            slots: r.slots,
            running: r.running,
            queued: r.queued,
            saturation_pct,
            leases: r.leases,
            leased_cpus: r.leased_cpus,
            devices: r.devices,
            devices_busy: r.devices_busy,
            device_busy_pct: r
                .device_busy
                .map(|b| (b.clamp(0.0, 1.0) * 100.0).round() as u32),
        }
    }
}

/// What the mesh adds up to (`meshnode totals`, M9 §6.3).
///
/// Every hardware figure is paired with how many nodes it is a total *of*.
/// Director renders the pair, not the number alone: "182 GB across 36 of 40"
/// is a fact, and "182 GB" on a mesh where four nodes have not reported yet
/// is a number somebody will check against the rack and stop trusting.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TotalsView {
    pub nodes_known: u32,
    pub nodes_live: u32,
    pub nodes_draining: u32,
    #[ts(type = "number")]
    pub cores: u64,
    pub cores_from: u32,
    pub slots: u32,
    pub slots_busy: u32,
    /// running / slots across the mesh, 0–100.
    pub saturation_pct: u32,
    #[ts(type = "number")]
    pub mem_mb: u64,
    pub mem_from: u32,
    #[ts(type = "number")]
    pub mem_available_mb: u64,
    pub mem_available_from: u32,
    #[ts(type = "number")]
    pub disk_mb: u64,
    pub disk_from: u32,
    #[ts(type = "number")]
    pub disk_free_mb: u64,
    pub disk_free_from: u32,
    pub devices: u32,
    pub devices_busy_here: u32,
    pub jobs_running: u32,
    pub jobs_queued: u32,
    /// `[arch, count]`, most common first.
    pub arches: Vec<(String, u32)>,
    /// Mean load per core over nodes with a fresh reading. `null` when no
    /// node has one, which is a real state on a mesh that just started.
    pub load_per_core: Option<f64>,
    /// Schedulable GPUs and the memory on them (L2 P1). `vramMb` is the
    /// *sum*, which is the spec-sheet question; `cap.vram_mb` is the
    /// largest single card, which is the scheduling one.
    pub gpus: u32,
    #[ts(type = "number")]
    pub vram_mb: u64,
    pub capacity_from: u32,
    /// The mesh's spec sheet as one sentence, composed in `mesh-proto` so
    /// this view, the CLI and the shared snapshot cannot phrase it three
    /// ways (L2 P1). Render it; do not re-derive it from the figures above.
    pub strip: String,
    /// *"3 of 4 machines reporting"*, and `null` when they all are. Shown
    /// whenever the figures are a floor, and never otherwise (D-L2.2).
    pub coverage: Option<String>,
}

impl From<mesh_proto::Totals> for TotalsView {
    fn from(t: mesh_proto::Totals) -> Self {
        let saturation_pct = t
            .slots_busy
            .saturating_mul(100)
            .checked_div(t.slots)
            .unwrap_or(0)
            .min(100);
        TotalsView {
            nodes_known: t.nodes_known,
            nodes_live: t.nodes_live,
            nodes_draining: t.nodes_draining,
            cores: t.cores,
            cores_from: t.cores_from,
            slots: t.slots,
            slots_busy: t.slots_busy,
            saturation_pct,
            mem_mb: t.mem_mb,
            mem_from: t.mem_from,
            mem_available_mb: t.mem_available_mb,
            mem_available_from: t.mem_available_from,
            disk_mb: t.disk_mb,
            disk_from: t.disk_from,
            disk_free_mb: t.disk_free_mb,
            disk_free_from: t.disk_free_from,
            devices: t.devices,
            devices_busy_here: t.devices_busy_here,
            jobs_running: t.jobs_running,
            jobs_queued: t.jobs_queued,
            gpus: t.gpus,
            vram_mb: t.vram_mb,
            capacity_from: t.capacity_from,
            strip: t.strip(),
            coverage: t.coverage(),
            arches: t.arches,
            load_per_core: t.load_per_core,
        }
    }
}

/// One accelerator, as Director's Nodes view shows it (C5 P7).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub node_id: String,
    pub node_name: String,
    pub index: u32,
    /// gpu | fpga | nic | …
    pub kind: String,
    pub vendor: String,
    pub model: String,
    pub uuid: String,
    #[ts(type = "number")]
    pub mem_mb: u64,
    pub numa_node: Option<u32>,
    /// ok | degraded | unavailable
    pub health: String,
    pub health_reason: Option<String>,
    /// The job or gang holding this device. `null` for a remote node — a
    /// hold is node-local and there is no honest way to read one from here.
    pub held_by: Option<String>,
    pub util_pct: Option<f64>,
    #[ts(type = "number | null")]
    pub mem_free_mb: Option<u64>,
    pub temp_c: Option<f64>,
    pub throttled: Option<bool>,
}

impl From<mesh_proto::DeviceRow> for DeviceView {
    fn from(r: mesh_proto::DeviceRow) -> Self {
        DeviceView {
            node_id: r.node.to_display_string(),
            node_name: r.node_name,
            index: r.index,
            kind: r.kind,
            vendor: r.vendor,
            model: r.model,
            uuid: r.uuid,
            mem_mb: r.mem_mb,
            numa_node: r.numa_node,
            health: r.health,
            health_reason: r.health_reason,
            held_by: r.held_by,
            util_pct: r.util_pct,
            mem_free_mb: r.mem_free_mb,
            temp_c: r.temp_c,
            throttled: r.throttled,
        }
    }
}

// ---------------------------------------------------------------------------
// Gangs (C4 §8, C0-C9 §14.3)
// ---------------------------------------------------------------------------

/// One all-or-nothing reservation, as the cluster view shows it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct GangRowView {
    pub gang: String,
    pub short_id: String,
    pub preview: String,
    pub size: u32,
    /// pack | spread
    pub placement: String,
    pub username: String,
    pub owner_name: String,
    /// offering | running | succeeded | failed | never-started | member-died
    pub state: String,
    /// Members accounted for: reservations while it is offering, jobs once
    /// it has started.
    pub held: u32,
    /// low | normal | high — the outer key of the seniority order.
    pub priority: String,
    #[ts(type = "number")]
    pub created_at_micros: u64,
}

impl From<mesh_proto::GangRow> for GangRowView {
    fn from(r: mesh_proto::GangRow) -> Self {
        GangRowView {
            gang: r.gang.to_display_string(),
            short_id: r.gang.short(),
            preview: r.preview,
            size: r.size,
            placement: r.placement.as_str().to_string(),
            username: r.username,
            owner_name: r.owner_name,
            state: r.state,
            held: r.held,
            priority: r.ticket.priority().as_str().to_string(),
            created_at_micros: r.created_at_micros,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub job_id: String,
    pub short_id: String,
    pub cmd_preview: String,
    pub runtime: String,
    /// queued | running | succeeded | failed | cancelled | timed-out
    pub state: String,
    pub exit_code: Option<i32>,
    pub executor_name: Option<String>,
    pub submitter_username: Option<String>,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    #[ts(type = "number | null")]
    pub finished_at_micros: Option<u64>,
    pub fan_group: Option<String>,
    pub fan_index: Option<u32>,
    pub fan_total: Option<u32>,
    /// Where this job's output is (M31 A2): `available`, `unavailable`,
    /// `expired` or `elsewhere`.
    ///
    /// On the row rather than discovered by opening the log pane, so a list
    /// can mark the ones there is nothing to open — and so *"expired"* and
    /// *"the machine is asleep"* are visibly different before anybody
    /// clicks.
    pub output_state: String,
    /// Empty when available; otherwise the one sentence, composed in
    /// `mesh-proto` so no renderer writes its own.
    pub output_message: String,
}

impl From<JobRow> for JobView {
    fn from(r: JobRow) -> Self {
        JobView {
            output_state: r.output.as_str().to_string(),
            output_message: r.output.message(),
            job_id: r.job_id.to_display_string(),
            short_id: r.job_id.short(),
            cmd_preview: r.cmd_preview,
            runtime: r.runtime,
            state: state_str(r.state),
            exit_code: r.exit_code,
            executor_name: r.executor_name,
            submitter_username: r.submitter_username,
            created_at_micros: r.created_at_micros,
            finished_at_micros: r.finished_at_micros,
            fan_group: r.fanout.map(|f| f.group.to_display_string()),
            fan_index: r.fanout.map(|f| f.index),
            fan_total: r.fanout.map(|f| f.total),
        }
    }
}

/// The engine's change-signal counters (**H-8**).
///
/// Handed to the frontend as a plain string→number map rather than the
/// wire's fixed array: the frontend compares topics by name, and an array
/// whose meaning is positional is a contract two languages have to agree
/// about by counting.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StateVersionsView {
    #[ts(type = "Record<string, number>")]
    pub topics: std::collections::BTreeMap<String, u64>,
}

impl From<mesh_proto::StateVersions> for StateVersionsView {
    fn from(v: mesh_proto::StateVersions) -> Self {
        StateVersionsView {
            topics: mesh_proto::StateTopic::ALL
                .iter()
                .map(|t| (t.as_str().to_string(), v.get(*t)))
                .collect(),
        }
    }
}

/// How many jobs are in each state, mesh-wide (**H-1**).
///
/// Carried beside a *page* of rows because the counters above a bounded
/// list cannot be derived from the list. Computing them in the frontend is
/// what forced Director to hold every job the mesh remembers.
#[derive(Debug, Clone, Copy, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct JobCountsView {
    #[ts(type = "number")]
    pub all: u64,
    #[ts(type = "number")]
    pub queued: u64,
    #[ts(type = "number")]
    pub running: u64,
    #[ts(type = "number")]
    pub succeeded: u64,
    #[ts(type = "number")]
    pub failed: u64,
    #[ts(type = "number")]
    pub cancelled: u64,
    #[ts(type = "number")]
    pub timed_out: u64,
}

impl From<mesh_proto::JobCounts> for JobCountsView {
    fn from(c: mesh_proto::JobCounts) -> Self {
        JobCountsView {
            all: c.all,
            queued: c.queued,
            running: c.running,
            succeeded: c.succeeded,
            failed: c.failed,
            cancelled: c.cancelled,
            timed_out: c.timed_out,
        }
    }
}

/// One page of the job table (**H-1**).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct JobsPageView {
    pub rows: Vec<JobView>,
    /// Matching the filter, before the page was cut.
    #[ts(type = "number")]
    pub total: u64,
    pub counts: JobCountsView,
}

impl From<mesh_proto::JobsPage> for JobsPageView {
    fn from(p: mesh_proto::JobsPage) -> Self {
        JobsPageView {
            rows: p.rows.into_iter().map(Into::into).collect(),
            total: p.total,
            counts: p.counts.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServiceView {
    pub name: String,
    pub runtime: String,
    pub version: u32,
    pub bundle_id: String,
    pub bundle_short: String,
    pub working_dir: String,
    pub entrypoint: String,
    pub description: String,
    pub tags: Vec<String>,
    pub versions: Vec<u32>,
    pub has_contract: bool,
    /// JSON Schema text; Director generates the input form from it (§8.3).
    pub contract: Option<String>,
}

impl From<ServiceRow> for ServiceView {
    fn from(r: ServiceRow) -> Self {
        ServiceView {
            name: r.name,
            runtime: r.runtime.as_str().to_string(),
            version: r.version,
            bundle_id: r.bundle_id.to_display_string(),
            bundle_short: r.bundle_id.short(),
            working_dir: r.working_dir,
            entrypoint: r.entrypoint,
            description: r.description,
            tags: r.tags,
            versions: r.versions,
            has_contract: r.has_contract,
            contract: r.contract,
        }
    }
}

/// A content-addressed bundle known to the mesh (MASTERPLAN §8.1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BundleView {
    pub bundle_id: String,
    pub short_id: String,
    #[ts(type = "number")]
    pub total_size: u64,
    pub file_count: u32,
    pub chunk_count: u32,
    pub holders: u32,
    /// This peer holds a complete copy.
    pub local: bool,
}

impl From<BundleRow> for BundleView {
    fn from(r: BundleRow) -> Self {
        BundleView {
            bundle_id: r.bundle_id.to_display_string(),
            short_id: r.bundle_id.short(),
            total_size: r.total_size,
            file_count: r.file_count,
            chunk_count: r.chunk_count,
            holders: r.holders as u32,
            local: r.local,
        }
    }
}

/// One file inside a bundle, with the runtimes it could plausibly be an
/// entrypoint for (Director's "browse scripts by type" picker).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BundleFileView {
    /// Full path inside the bundle (forward slashes).
    pub path: String,
    /// Directory part ("" for the bundle root) — the natural working dir.
    pub dir: String,
    /// File name — the natural entrypoint.
    pub name: String,
    #[ts(type = "number")]
    pub size: u64,
    pub executable: bool,
    pub symlink: Option<String>,
    /// bash | python | ansible | exec | contract | text — by extension and
    /// mode; a file may match several (an executable .sh is bash *and* exec).
    pub kinds: Vec<String>,
}

impl From<mesh_proto::BundleFileRow> for BundleFileView {
    fn from(f: mesh_proto::BundleFileRow) -> Self {
        let (dir, name) = match f.path.rfind('/') {
            Some(i) => (f.path[..i].to_string(), f.path[i + 1..].to_string()),
            None => (String::new(), f.path.clone()),
        };
        let lower = name.to_ascii_lowercase();
        let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let mut kinds: Vec<String> = Vec::new();
        match ext {
            "sh" | "bash" => kinds.push("bash".into()),
            "py" => kinds.push("python".into()),
            "yml" | "yaml" => kinds.push("ansible".into()),
            "json" => kinds.push("contract".into()),
            "md" | "txt" | "toml" | "cfg" | "ini" | "env" => kinds.push("text".into()),
            _ => {}
        }
        if f.executable && f.symlink.is_none() {
            kinds.push("exec".into());
        }
        BundleFileView {
            path: f.path,
            dir,
            name,
            size: f.size,
            executable: f.executable,
            symlink: f.symlink,
            kinds,
        }
    }
}

/// Outcome of pushing a directory into the content store.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PushResultView {
    pub bundle_id: String,
    pub short_id: String,
    pub file_count: u32,
    pub chunk_count: u32,
    #[ts(type = "number")]
    pub total_size: u64,
}

/// A registered Git source (MASTERPLAN §8.1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SourceView {
    pub name: String,
    pub url: String,
    pub branch: String,
    #[ts(type = "number")]
    pub interval_secs: u64,
    pub bump: Vec<String>,
    pub created_by_name: String,
    pub last_commit: Option<String>,
    pub last_bundle: Option<String>,
    pub last_bundle_short: Option<String>,
    #[ts(type = "number | null")]
    pub last_synced_at_micros: Option<u64>,
    pub watcher: Option<String>,
}

impl From<SourceRow> for SourceView {
    fn from(r: SourceRow) -> Self {
        SourceView {
            name: r.name,
            url: r.url,
            branch: r.branch,
            interval_secs: r.interval_secs,
            bump: r.bump,
            created_by_name: r.created_by_name,
            last_commit: r.last_commit,
            last_bundle: r.last_bundle.map(|b| b.to_display_string()),
            last_bundle_short: r.last_bundle.map(|b| b.short()),
            last_synced_at_micros: r.last_synced_at_micros,
            watcher: r.watcher,
        }
    }
}

/// Outcome of a manual `sync now`.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SyncResultView {
    pub commit: String,
    pub bundle_id: String,
    pub bundle_short: String,
    pub unchanged: bool,
    pub bumped: Vec<BumpedView>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BumpedView {
    pub service: String,
    pub version: u32,
}

/// A secret's metadata — never its value (MASTERPLAN §7.3).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SecretView {
    pub name: String,
    pub version: u32,
    pub created_by_name: String,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    /// Nodes holding a wrap (can decrypt) vs total active members.
    pub wrapped_for: u32,
    pub members: u32,
    pub removed: bool,
}

impl From<SecretRow> for SecretView {
    fn from(r: SecretRow) -> Self {
        SecretView {
            name: r.name,
            version: r.version,
            created_by_name: r.created_by_name,
            created_at_micros: r.created_at_micros,
            wrapped_for: r.wrapped_for,
            members: r.members,
            removed: r.removed,
        }
    }
}

/// Outcome of sealing a secret from this Director.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SecretSetView {
    pub name: String,
    pub version: u32,
    pub recipients: u32,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub name: String,
    pub plugin: String,
}

impl From<ProviderRow> for ProviderView {
    fn from(r: ProviderRow) -> Self {
        ProviderView {
            name: r.name,
            plugin: r.plugin,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct UserView {
    pub user_id: String,
    pub short_id: String,
    pub username: String,
    pub is_founder: bool,
    pub revoked: bool,
    pub roles: Vec<String>,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    /// Their key is provably on a security key (Y2/Y3), when they have
    /// published the proof and the mesh verified it.
    pub hardware: Option<UserHardwareView>,
}

/// A verified proof that somebody's key was made on a device and cannot
/// leave it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct UserHardwareView {
    /// `piv`, `openpgp` or `fido2`.
    pub kind: String,
    pub slot: String,
    /// The authority that vouched for it.
    pub root_label: String,
    /// The device certificate's subject — on a YubiKey, its serial.
    pub subject: String,
    #[ts(type = "number")]
    pub attested_at_micros: u64,
    /// When the certificate says it expires. Shown, never enforced.
    #[ts(type = "number")]
    pub not_after_micros: u64,
    pub revoked: bool,
    pub revoked_reason: String,
}

impl From<UserRow> for UserView {
    fn from(r: UserRow) -> Self {
        UserView {
            user_id: r.user_id.to_display_string(),
            short_id: r.user_id.short(),
            username: r.username,
            is_founder: r.is_founder,
            revoked: r.revoked,
            roles: r.roles,
            created_at_micros: r.created_at_micros,
            hardware: r.hardware.map(|h| UserHardwareView {
                kind: h.kind,
                slot: h.slot,
                root_label: h.root_label,
                subject: h.subject,
                attested_at_micros: h.attested_at_micros,
                not_after_micros: h.not_after_micros,
                revoked: h.revoked,
                revoked_reason: h.revoked_reason,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct GrantView {
    pub grant_id: String,
    pub user_id: String,
    pub username: String,
    pub role: String,
    pub nodes: String,
    pub services: String,
    #[ts(type = "number | null")]
    pub expires_at_micros: Option<u64>,
    pub revoked: bool,
    pub expired: bool,
    #[ts(type = "number")]
    pub created_at_micros: u64,
}

impl From<GrantRow> for GrantView {
    fn from(r: GrantRow) -> Self {
        GrantView {
            grant_id: r.grant_id.to_string(),
            user_id: r.user_id.to_display_string(),
            username: r.username,
            role: r.role,
            nodes: r.nodes,
            services: r.services,
            expires_at_micros: r.expires_at_micros,
            revoked: r.revoked,
            expired: r.expired,
            created_at_micros: r.created_at_micros,
        }
    }
}

/// One audit event (MASTERPLAN §7.4).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AuditView {
    pub audit_id: String,
    #[ts(type = "number")]
    pub at_micros: u64,
    pub user_id: String,
    pub username: String,
    pub action: String,
    pub target: String,
    pub node_short: String,
    pub outcome: String,
    pub detail: String,
}

impl From<AuditEntry> for AuditView {
    fn from(e: AuditEntry) -> Self {
        AuditView {
            audit_id: e.audit_id.to_string(),
            at_micros: e.at_micros,
            user_id: e.user.to_display_string(),
            username: e.username,
            action: e.action,
            target: e.target,
            node_short: e.node.short(),
            outcome: e.outcome,
            detail: e.detail,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmIView {
    pub user_id: String,
    pub username: Option<String>,
    pub registered: bool,
    pub is_founder: bool,
    pub roles: Vec<String>,
    pub grants: Vec<GrantView>,
    /// The node this Director is driving — half of the sentence an
    /// unenrolled person needs (M23 §4).
    pub node_name: String,
    /// `none` | `pending` | `declined` | `expired` | `enrolled`.
    pub enrolment: String,
    /// The code to show an admin, while there is a live request.
    pub enrolment_code: Option<String>,
}

impl From<WhoAmIInfo> for WhoAmIView {
    fn from(i: WhoAmIInfo) -> Self {
        WhoAmIView {
            user_id: i.user_id.to_display_string(),
            username: i.username,
            registered: i.registered,
            is_founder: i.is_founder,
            roles: i.roles,
            grants: i.grants.into_iter().map(Into::into).collect(),
            node_name: i.node_name,
            enrolment: i.enrolment.as_str().to_string(),
            enrolment_code: i.enrolment_code,
        }
    }
}

/// One person waiting to be recognised by this mesh (M23 §3.2).
///
/// A row in a queue, and nothing more: approving it is the existing
/// admin-signed `UserAdd` + `GrantAdd` pair. Director shows the code
/// because an admin who wants out-of-band confirmation — "read me the code
/// on your screen" — should be able to have it, exactly as node pairing
/// already allows.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct EnrolmentView {
    pub user_id: String,
    pub short_id: String,
    pub suggested_username: String,
    pub from_node: String,
    pub code: String,
    #[ts(type = "number")]
    pub requested_at_micros: u64,
    #[ts(type = "number")]
    pub expires_at_micros: u64,
    pub declined: bool,
    pub declined_by: Option<String>,
}

impl From<mesh_proto::EnrolmentRow> for EnrolmentView {
    fn from(r: mesh_proto::EnrolmentRow) -> Self {
        EnrolmentView {
            user_id: r.user_id.to_display_string(),
            short_id: r.user_id.short(),
            suggested_username: r.suggested_username,
            from_node: r.from_node,
            code: r.code,
            requested_at_micros: r.requested_at_micros,
            expires_at_micros: r.expires_at_micros,
            declined: r.declined,
            declined_by: r.declined_by,
        }
    }
}

/// `meshnode ping` from this peer.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PingView {
    pub peer_node_id: String,
    pub peer_short: String,
    pub peer_node_name: String,
    pub peer_mesh_name: Option<String>,
    pub same_mesh: bool,
    #[ts(type = "number")]
    pub rtt_micros: u64,
    /// The route that answered — a socket address, or `relay <node>`.
    pub via: String,
}

impl From<PingResult> for PingView {
    fn from(p: PingResult) -> Self {
        PingView {
            peer_node_id: p.peer_node_id.to_display_string(),
            peer_short: p.peer_node_id.short(),
            peer_node_name: p.peer_node_name,
            peer_mesh_name: p.peer_mesh_name,
            same_mesh: p.same_mesh,
            rtt_micros: p.rtt_micros,
            via: p.via,
        }
    }
}

/// Whether the tray icon is actually being drawn, and what to do if not.
///
/// Registering a StatusNotifierItem and having a panel display it are two
/// different things on Linux, and Director's window behaviour depends on
/// the second: close hides only where there is somewhere to hide to. That
/// is invisible from the UI unless it is said, so Settings says it next to
/// "Start at login", which is the switch it actually breaks.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TrayStatusView {
    /// Was a tray item registered at all?
    pub present: bool,
    /// Is something on this desktop drawing it?
    pub visible: bool,
    /// What to install or enable, when it is not. Empty when it is.
    pub hint: String,
}

/// One `doctor` probe result (MASTERPLAN §12).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheckView {
    pub name: String,
    /// ok | warn | fail
    pub verdict: String,
    pub detail: String,
}

/// What `doctor --fix` did about one finding.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DoctorFixView {
    pub name: String,
    pub applied: bool,
    pub detail: String,
}

/// The whole of a fix pass: what was done, and the checks run again after.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DoctorFixReport {
    pub fixes: Vec<DoctorFixView>,
    pub after: Vec<DoctorCheckView>,
}

/// A backup file was written (MASTERPLAN §4.5).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BackupView {
    pub path: String,
    #[ts(type = "number")]
    pub ops: u64,
    #[ts(type = "number")]
    pub bytes: u64,
}

/// A backup file was restored into this Director's data dir (wizard path).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RestoreView {
    pub mesh_name: String,
    #[ts(type = "number")]
    pub ops: u64,
    #[ts(type = "number")]
    pub members: u64,
}

/// A file or folder a person picked in a native dialog, or dropped on the
/// window (SEC-14). The window holds the token and shows the path; only
/// the token is ever sent back, and it names only this pick.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PickedView {
    pub token: String,
    pub name: String,
    pub path: String,
}

/// An invite, as the Invite panel lists it (SEC-17).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InviteView {
    pub invite_id: String,
    pub created_by: String,
    #[ts(type = "number")]
    pub expires_at_micros: u64,
    pub uses: u32,
    pub used: u32,
    pub revoked: bool,
}

impl From<mesh_proto::InviteRow> for InviteView {
    fn from(r: mesh_proto::InviteRow) -> Self {
        InviteView {
            invite_id: r.invite_id.to_display_string(),
            created_by: mesh_proto::text::clean(
                &r.created_by_username,
                mesh_proto::text::MAX_NAME_CHARS,
            ),
            expires_at_micros: r.expires_at_micros,
            uses: r.uses,
            used: r.used,
            revoked: r.revoked,
        }
    }
}

/// A machine on this network waiting in the wizard's "Join a mesh on this
/// network" (or `meshnode join --pair`).
///
/// Everything here is what the joiner *claims* — the code both humans
/// compare is what authenticates. It is shown so the admin knows what they
/// are being asked about before they press anything: who, on what machine,
/// at what address, and for how long.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CandidateView {
    pub node_name: String,
    pub node_id: Option<String>,
    pub platform: Option<String>,
    /// Dialable, best first.
    pub addrs: Vec<String>,
    /// The person at the machine, as typed into the wizard.
    pub person: Option<String>,
    /// The OS hostname, where it differs from the node name.
    pub hostname: Option<String>,
    pub via_mdns: bool,
    pub via_beacon: bool,
    #[ts(type = "number")]
    pub first_seen_micros: u64,
    #[ts(type = "number")]
    pub last_seen_micros: u64,
}

impl From<mesh_core::pairing::Candidate> for CandidateView {
    fn from(c: mesh_core::pairing::Candidate) -> Self {
        CandidateView {
            node_name: c.node_name,
            node_id: c.node_id.map(|i| i.to_display_string()),
            platform: c.platform,
            addrs: c.addrs.iter().map(|a| a.to_string()).collect(),
            person: c.person,
            hostname: c.hostname,
            via_mdns: c.via_mdns,
            via_beacon: c.via_beacon,
            first_seen_micros: c.first_seen_micros,
            last_seen_micros: c.last_seen_micros,
        }
    }
}

/// The background LAN watch: what it has heard, and whether each of its
/// two ears is working. An empty queue with both channels reporting an
/// error is a different screen from an empty queue on a healthy network.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LanWatchView {
    pub running: bool,
    pub candidates: Vec<CandidateView>,
    pub mdns_error: Option<String>,
    pub beacon_error: Option<String>,
    /// The UDP port the beacon listener is on, for the hint text.
    pub beacon_port: u16,
    /// This machine's own addresses — what a joiner would have to be
    /// able to reach, and what a firewall rule would name.
    pub local_addrs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct FanTargetView {
    pub job_id: String,
    pub node_id: String,
    pub node_name: String,
}

impl From<FanTarget> for FanTargetView {
    fn from(t: FanTarget) -> Self {
        FanTargetView {
            job_id: t.job_id.to_display_string(),
            node_id: t.node_id.to_display_string(),
            node_name: t.node_name,
        }
    }
}

/// Streamed frames of a run (single-target or fan-out) or a log follow.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RunEvent {
    Started {
        job_id: String,
        executor_name: String,
    },
    FanoutStarted {
        group: String,
        targets: Vec<FanTargetView>,
    },
    /// One chunk of output. `index` is the fan-out target (null when
    /// single-target); `stream` 1 = stdout, 2 = stderr.
    Chunk {
        index: Option<u32>,
        stream: u8,
        text: String,
    },
    Done {
        index: Option<u32>,
        state: String,
        exit_code: Option<i32>,
    },
    /// The output is not readable, and this says which of expired /
    /// unavailable / elsewhere happened (M31 A2).
    ///
    /// Rendered **in the log pane, where the output would have been**, and
    /// followed by an ordinary `Done` carrying the job's real state — a
    /// successful job whose log aged out is a successful job. The fields
    /// are separate rather than one pre-rendered sentence so the pane can
    /// style a temporary absence differently from a permanent one.
    OutputUnavailable {
        /// `available` / `unavailable` / `expired` / `elsewhere`.
        kind: String,
        /// The one sentence, composed once in `mesh-proto` so the CLI,
        /// Director and the SDK cannot drift apart about it.
        message: String,
        /// Whether waiting will help. Drives whether the pane offers a
        /// retry.
        may_return: bool,
    },
    FanoutDone {
        succeeded: u32,
        failed: u32,
        skipped: u32,
    },
    // -- C4 gangs (§8) -------------------------------------------------------
    //
    // A gang reuses the fan-out's `chunk`/`done` frames once it is running,
    // so these three are only about the part that is new: the reservation.
    // A submission that is *waiting* for one is the normal case here, and a
    // UI that showed nothing while it waited would be a UI that looked hung
    // exactly when the scheduler was working.
    /// The offer phase is still running. `behind` names the senior gang the
    /// last refusal named, when there was one — the difference between
    /// "the mesh is full" and "you are behind somebody".
    GangWaiting {
        gang: String,
        short_id: String,
        held: u32,
        size: u32,
        round: u32,
        behind: Option<String>,
    },
    /// Every member started at the same moment. The `index` on the frames
    /// that follow is the member's rank.
    GangStarted {
        gang: String,
        short_id: String,
        members: Vec<FanTargetView>,
        rounds: u32,
    },
    GangDone {
        gang: String,
        short_id: String,
        /// succeeded | failed | never-started | member-died
        outcome: String,
        succeeded: u32,
        failed: u32,
    },
    // -- M19 results (§5.4) --------------------------------------------------
    /// One target's *answer*, as distinct from its log. Sent just before
    /// that target's `done`, and only when the caller asked for outputs.
    Result {
        row: ResultRowView,
    },
    /// The verdict of an `--expect` assertion over the stream's rows.
    Expected {
        expression: String,
        passed: u32,
        failed: Vec<String>,
    },
    // -- M11 change control ------------------------------------------------
    /// The answer to a dry run: nothing was dispatched.
    ///
    /// Boxed: a resolved preview carries the whole plan and dwarfs every
    /// other frame, and this enum is moved once per output chunk.
    Preview {
        preview: Box<JobPreviewView>,
    },
    Failed {
        message: String,
    },
}

/// How much of a parallel job one node has done (M9 §6.3). This is the
/// view that makes work stealing legible: a 32-core node visibly drawing
/// twice the chunks of a 16-core one, with no operator input.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct NodeProgressView {
    pub node_id: String,
    pub name: String,
    #[ts(type = "number")]
    pub items: u64,
    pub chunks: u32,
    pub items_per_sec: f64,
    /// This node's share of the whole job, 0–100.
    pub share_pct: f64,
}

impl NodeProgressView {
    fn from_progress(p: mesh_proto::NodeProgress, total: u64) -> Self {
        NodeProgressView {
            node_id: p.node.to_display_string(),
            name: p.name,
            items: p.items,
            chunks: p.chunks,
            items_per_sec: p.items_per_sec,
            share_pct: if total > 0 {
                p.items as f64 / total as f64 * 100.0
            } else {
                0.0
            },
        }
    }
}

/// Frames of a parallel run, for the Compute section (M9 §6.3).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ParallelEvent {
    Started {
        job_id: String,
        #[ts(type = "number")]
        total: u64,
        /// Nodes now pulling chunks, the owner first.
        workers: Vec<String>,
        /// `name: reason` for every node that did not take the job on.
        declined: Vec<String>,
        /// Roughly how long, as a range (R5 P4). `null` when nothing
        /// comparable has run.
        estimate: Option<DurationRangeView>,
    },
    /// One item finished, wherever it ran.
    Item {
        item: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        node_name: String,
        /// Under "keep output where it is produced", where this item's
        /// output stayed (M9 §5.5). `null` for an ordinary run.
        output: Option<ObjectRefView>,
    },
    Progress {
        #[ts(type = "number")]
        total: u64,
        #[ts(type = "number")]
        done: u64,
        #[ts(type = "number")]
        failed: u64,
        #[ts(type = "number")]
        inflight: u64,
        /// Items per chunk right now — adaptive, so this moves (M9 §5.4).
        chunk_items: u32,
        /// Chunks re-run elsewhere to beat a straggler (M9 §5.6). Shown as
        /// mitigation, never as a failure count.
        speculated: u32,
        per_node: Vec<NodeProgressView>,
    },
    /// The reduce step's answer, just before `Done` (M9 §5.6).
    Reduced {
        node_name: String,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        #[ts(type = "number")]
        inputs: u64,
    },
    Done {
        #[ts(type = "number")]
        total: u64,
        #[ts(type = "number")]
        succeeded: u64,
        #[ts(type = "number")]
        failed: u64,
        #[ts(type = "number")]
        micros: u64,
        speculated: u32,
        per_node: Vec<NodeProgressView>,
    },
    // -- M11 change control ------------------------------------------------
    /// The answer to a dry run: nothing was dispatched.
    Preview {
        preview: Box<JobPreviewView>,
    },
    Failed {
        message: String,
    },
}

/// What a script says about itself, for the Add-a-script wizard (R2 P4).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ScriptSpecView {
    pub name: Option<String>,
    pub about: String,
    /// Which argument a per-item run fills in with each item.
    pub item: Option<String>,
    /// True when the script declares itself — in a `.definitions` file or a
    /// `#meshware:` header. False means everything below was read out of the
    /// script and should say so.
    pub declared: bool,
    /// Which of the three answers this is: `definitions`, `header`,
    /// `inferred` or `none`. The screen says so rather than presenting three
    /// quite different levels of confidence as one filled-in form.
    pub source: String,
    pub inferred_count: u32,
    pub arg_count: u32,
    /// The JSON Schema this script would register with, or `null` when it
    /// takes no arguments at all.
    pub contract: Option<String>,
    /// The definitions file as it stands, for the editor to open.
    ///
    /// Always populated, even when the script has no definitions file: what
    /// comes back is then this spec written out as one, which is exactly the
    /// starting point a person wants when they press *Edit definitions* on a
    /// script that has only a header, or only what could be read out of it.
    /// [`has_definitions`] is what says whether the file exists yet.
    ///
    /// [`has_definitions`]: Self::has_definitions
    pub definitions: String,
    /// Whether a definitions file is actually there.
    pub has_definitions: bool,
    /// Where one would be written — `burn-in.sh.definitions`.
    pub definitions_path: String,
    /// Why the definitions file could not be used. A file with a trailing
    /// comma must say so, loudly: silently falling back to an empty form is
    /// the exact failure this whole path exists to remove.
    pub error: Option<String>,
}

/// What happened when a run's settings were saved as the script's defaults.
///
/// Two places are written and they are not equally important, so the result
/// says which of them actually happened rather than collapsing both into a
/// boolean:
///
///   * **The contract** is the one that matters. It replicates, so the
///     person who sets a script up on their laptop and runs it from the
///     office box gets the same form pre-filled. A new service version is
///     minted for it — signed, audited, and rollback-able like any other.
///   * **The definitions file** is the copy on disk a person edits and
///     commits. It is only reachable when this Director knows the folder
///     the bundle was pushed from, which is why it is optional and why a
///     failure to write it does not fail the save.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RunDefaultsSavedView {
    /// The service version this minted, or `null` when the settings were
    /// already exactly what the script said and nothing was written. A
    /// no-op is reported as a no-op: minting an identical version on every
    /// run would turn the version list into a log of button presses.
    pub version: Option<u32>,
    /// The definitions file written, when there was a local folder to
    /// write one to.
    pub definitions_path: Option<String>,
    /// Why the definitions file could not be written. Never fatal — the
    /// contract is already saved by this point, and the file is the
    /// convenience half.
    pub definitions_error: Option<String>,
}

/// Roughly how long a run will take, as a range (R5 P4).
///
/// A range and its sample count, never a single number. "17 minutes" is
/// wrong precisely often enough to be untrusted forever; "12–25, from 41
/// past runs" is right and stays trusted, and a preview built from two runs
/// should say it was built from two runs.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DurationRangeView {
    #[ts(type = "number")]
    pub p10_secs: u64,
    #[ts(type = "number")]
    pub p50_secs: u64,
    #[ts(type = "number")]
    pub p90_secs: u64,
    pub samples: u32,
}

impl From<mesh_proto::DurationRange> for DurationRangeView {
    fn from(d: mesh_proto::DurationRange) -> Self {
        DurationRangeView {
            p10_secs: d.p10_secs,
            p50_secs: d.p50_secs,
            p90_secs: d.p90_secs,
            samples: d.samples,
        }
    }
}

/// Where one item's output ended up (M9 §5.5). The id and the node, not
/// the bytes — which is the whole point.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRefView {
    pub id: String,
    #[ts(type = "number")]
    pub size: u64,
    pub holder: String,
}

impl From<mesh_proto::ObjectRef> for ObjectRefView {
    fn from(o: mesh_proto::ObjectRef) -> Self {
        ObjectRefView {
            id: o.id.to_display_string(),
            size: o.size,
            holder: o.holder.to_display_string(),
        }
    }
}

impl ParallelEvent {
    /// Map an engine stream frame; `None` for frames with no UI meaning.
    pub fn from_frame(frame: IpcResponse) -> Option<ParallelEvent> {
        Some(match frame {
            IpcResponse::ParallelStarted {
                job_id,
                total,
                workers,
                declined,
                estimate,
            } => ParallelEvent::Started {
                job_id: job_id.to_display_string(),
                total,
                workers,
                declined,
                estimate: estimate.map(DurationRangeView::from),
            },
            IpcResponse::ParallelItem {
                item,
                exit_code,
                stdout,
                stderr,
                node_name,
                output,
                ..
            } => ParallelEvent::Item {
                item,
                exit_code,
                stdout,
                stderr,
                node_name,
                output: output.map(ObjectRefView::from),
            },
            IpcResponse::ParallelProgress {
                total,
                done,
                failed,
                inflight,
                chunk_items,
                speculated,
                per_node,
            } => ParallelEvent::Progress {
                total,
                done,
                failed,
                inflight,
                chunk_items,
                speculated,
                per_node: per_node
                    .into_iter()
                    .map(|n| NodeProgressView::from_progress(n, total))
                    .collect(),
            },
            IpcResponse::ParallelReduced {
                node_name,
                exit_code,
                stdout,
                stderr,
                inputs,
                ..
            } => ParallelEvent::Reduced {
                node_name,
                exit_code,
                stdout,
                stderr,
                inputs,
            },
            IpcResponse::ParallelDone {
                total,
                succeeded,
                failed,
                micros,
                speculated,
                per_node,
                ..
            } => ParallelEvent::Done {
                total,
                succeeded,
                failed,
                micros,
                speculated,
                per_node: per_node
                    .into_iter()
                    .map(|n| NodeProgressView::from_progress(n, total))
                    .collect(),
            },
            IpcResponse::JobPreview(p) => ParallelEvent::Preview {
                preview: Box::new(JobPreviewView::from(*p)),
            },
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                ParallelEvent::Failed { message }
            }
            _ => return None,
        })
    }
}

/// UTF-8 decoding that survives a character split across two frames
/// (**M-21**).
///
/// One carry per `(fan-out index, stream)`, because a fan-out multiplexes
/// thirty hosts' stdout and stderr down one channel — a single shared
/// buffer would splice one host's trailing bytes onto another host's next
/// chunk and corrupt both.
#[derive(Default)]
pub struct StreamText {
    tails: std::collections::HashMap<(Option<u32>, u8), Vec<u8>>,
}

impl StreamText {
    /// Decode `data`, holding back any incomplete trailing character.
    pub fn push(&mut self, index: Option<u32>, stream: u8, data: &[u8]) -> String {
        let tail = self.tails.entry((index, stream)).or_default();
        let buf: Vec<u8> = if tail.is_empty() {
            data.to_vec()
        } else {
            let mut b = std::mem::take(tail);
            b.extend_from_slice(data);
            b
        };
        match std::str::from_utf8(&buf) {
            Ok(text) => text.to_string(),
            Err(e) => {
                let good = e.valid_up_to();
                let mut text = String::from_utf8_lossy(&buf[..good]).into_owned();
                match e.error_len() {
                    // Truly invalid bytes: emit the replacement now, since
                    // no future byte can rescue them.
                    Some(n) => {
                        text.push('\u{fffd}');
                        // What follows the bad sequence may itself be a
                        // partial character; run it through again.
                        let rest = buf[good + n..].to_vec();
                        if !rest.is_empty() {
                            text.push_str(&self.push(index, stream, &rest));
                        }
                    }
                    // Ran out of input mid-character: keep it for next time.
                    None => {
                        *self.tails.entry((index, stream)).or_default() = buf[good..].to_vec();
                    }
                }
                text
            }
        }
    }
}

impl RunEvent {
    /// Map an engine stream frame, decoding chunk text across frame
    /// boundaries (**M-21**).
    ///
    /// Each chunk used to be `String::from_utf8_lossy`'d on its own, so a
    /// character whose bytes straddled two frames was replaced by U+FFFD in
    /// *both* halves — visible on any job that prints a name with an accent
    /// in it, and indistinguishable from the job having produced mojibake
    /// itself. `carry` holds the trailing partial sequence until the bytes
    /// that finish it arrive, so it must live for the whole stream rather
    /// than per frame. `None` for frames with no UI meaning.
    pub fn from_frame_with(frame: IpcResponse, carry: &mut StreamText) -> Option<RunEvent> {
        Some(match frame {
            IpcResponse::JobStarted {
                job_id,
                executor_name,
                ..
            } => RunEvent::Started {
                job_id: job_id.to_display_string(),
                executor_name,
            },
            IpcResponse::FanoutStarted { group, targets } => RunEvent::FanoutStarted {
                group: group.to_display_string(),
                targets: targets.into_iter().map(Into::into).collect(),
            },
            IpcResponse::LogChunk { stream, data } => RunEvent::Chunk {
                index: None,
                stream,
                text: carry.push(None, stream, &data),
            },
            IpcResponse::FanChunk {
                index,
                stream,
                data,
            } => RunEvent::Chunk {
                index: Some(index),
                stream,
                text: carry.push(Some(index), stream, &data),
            },
            IpcResponse::JobDone { state, exit_code } => RunEvent::Done {
                index: None,
                state: state_str(state),
                exit_code,
            },
            IpcResponse::LogsUnavailable { availability } => RunEvent::OutputUnavailable {
                kind: availability.as_str().to_string(),
                message: availability.message(),
                may_return: availability.may_return(),
            },
            IpcResponse::FanJobDone {
                index,
                state,
                exit_code,
            } => RunEvent::Done {
                index: Some(index),
                state: state_str(state),
                exit_code,
            },
            IpcResponse::Result(row) => RunEvent::Result { row: (*row).into() },
            IpcResponse::Expected {
                expression,
                passed,
                failed,
            } => RunEvent::Expected {
                expression,
                passed,
                failed,
            },
            IpcResponse::FanoutDone {
                succeeded,
                failed,
                skipped,
            } => RunEvent::FanoutDone {
                succeeded,
                failed,
                skipped,
            },
            IpcResponse::GangWaiting {
                gang,
                held,
                size,
                round,
                behind,
            } => RunEvent::GangWaiting {
                gang: gang.to_display_string(),
                short_id: gang.short(),
                held,
                size,
                round,
                behind: behind.map(|g| g.short()),
            },
            IpcResponse::GangStarted {
                gang,
                members,
                rounds,
            } => RunEvent::GangStarted {
                gang: gang.to_display_string(),
                short_id: gang.short(),
                members: members.into_iter().map(Into::into).collect(),
                rounds,
            },
            IpcResponse::GangDone {
                gang,
                outcome,
                succeeded,
                failed,
            } => RunEvent::GangDone {
                gang: gang.to_display_string(),
                short_id: gang.short(),
                outcome: outcome.as_str().to_string(),
                succeeded,
                failed,
            },
            IpcResponse::JobPreview(preview) => RunEvent::Preview {
                preview: Box::new(JobPreviewView::from(*preview)),
            },
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                RunEvent::Failed { message }
            }
            _ => return None,
        })
    }
}

/// Streamed frames of a drain (MASTERPLAN §12).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DrainEvent {
    Progress {
        running: u32,
        queued: u32,
        moved: u32,
        /// Accelerators still held by unfinished work (C5 P6) — the
        /// question somebody draining a GPU box for a card swap has, and a
        /// different one from whether a slot is free.
        devices_held: u32,
    },
    /// Idle: the engine has stopped accepting work and finished what it had.
    Done,
    Failed {
        message: String,
    },
}

/// One kind of thing this mesh distributes (M24 §3.1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionView {
    pub kind: String,
    /// A label a person reads: `Fact collectors`, not `collectors`.
    pub label: String,
    pub what: String,
    pub op: String,
    pub count: u32,
    #[ts(type = "number | null")]
    pub last_changed_micros: Option<u64>,
    pub last_changed_what: Option<String>,
    /// This kind runs on nodes without anybody pressing Run — the ones
    /// where widening a selector is a deployment (§3.3).
    pub executes: bool,
}

impl From<mesh_proto::DefinitionRow> for DefinitionView {
    fn from(r: mesh_proto::DefinitionRow) -> Self {
        let label = match r.kind.as_str() {
            "services" => "Scripts and services",
            "collectors" => "Fact collectors",
            "schedules" => "Schedules",
            "session_policies" => "Terminal policies",
            "sources" => "Git sources",
            other => other,
        }
        .to_string();
        DefinitionView {
            kind: r.kind,
            label,
            what: r.what,
            op: r.op,
            count: r.count,
            last_changed_micros: r.last_changed_micros,
            last_changed_what: r.last_changed_what,
            executes: r.executes,
        }
    }
}

/// Per-node progress of a bundle push (M24 §3.2).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PrefetchEvent {
    Node {
        node: String,
        have_chunks: u32,
        total_chunks: u32,
        #[ts(type = "number")]
        have_bytes: u64,
        #[ts(type = "number")]
        total_bytes: u64,
        complete: bool,
        /// Set when the node could not be asked at all. Not a failure of
        /// the bundle — it fetches lazily when it needs the content, as it
        /// always did.
        unreachable: Option<String>,
    },
    Done {
        complete: u32,
        fetching: u32,
        skipped: u32,
    },
    Failed {
        message: String,
    },
}

/// Per-node progress of a fleet upgrade (M24 §4).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UpgradeEvent {
    Node {
        node: String,
        state: String,
        detail: String,
    },
    Done {
        /// Nodes seen back on the target, alive and caught up. Never a node
        /// that was merely asked.
        upgraded: u32,
        already: u32,
        /// This Director's own peer is restarting into the target and is
        /// deliberately uncounted — it cannot verify itself.
        driver_restarting: bool,
        remaining: u32,
        /// Set when the rollout stopped early, naming where.
        stopped_at: Option<String>,
    },
    Failed {
        message: String,
    },
}

/// Wizard pairing (this Director as the *joiner*).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WizardPairEvent {
    Listening {
        addr: String,
        /// Every address this machine can be dialled at on `addr`'s port
        /// — read one to an admin whose Director cannot see you.
        addrs: Vec<String>,
        /// Why mDNS is not announcing, if it is not (the beacon still is).
        mdns_error: Option<String>,
    },
    Code {
        code: String,
        mesh_name: String,
        approver_name: String,
    },
    Joined {
        mesh_name: String,
    },
    Failed {
        message: String,
    },
    /// The person gave up waiting (`wizard_pair_cancel`, DIR-6). Not a
    /// failure: nothing is said, and the wizard is back where it was.
    Cancelled,
}

/// One pairing code, in every form the dialog shows it (`mesh_core::qr`,
/// `mesh_core::wordpair`).
///
/// Built in the engine rather than the browser, and for the reason M11's
/// window badge is: the joiner's console and this screen are the two things
/// a human is being asked to compare, and a word pair computed twice in two
/// languages is a word pair that eventually disagrees — which reads to that
/// human as an attack rather than as a bug.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PairCodeView {
    /// The six digits, raw.
    pub code: String,
    /// Grouped for reading aloud: `421 733`.
    pub grouped: String,
    /// The same six digits as two words — a bijection, not a summary, so
    /// comparing the words is comparing the code. Absent only for a
    /// malformed code, which never reaches this screen.
    pub words: Option<String>,
    /// `viewBox` extent for `qrPath`, in modules, quiet zone included.
    pub qr_size: u32,
    /// SVG path data for the QR, in module coordinates. Absent if the code
    /// would not encode — the digits are still the protocol.
    pub qr_path: Option<String>,
}

impl PairCodeView {
    /// The quiet zone, in modules. Four is the standard's minimum and
    /// scanners genuinely need it.
    const QUIET: usize = 4;

    pub fn of(code: &str) -> PairCodeView {
        let shown = mesh_core::pairing::display(code);
        PairCodeView {
            code: shown.code,
            grouped: shown.grouped,
            words: shown.words,
            qr_size: shown
                .qr
                .as_ref()
                .map_or(0, |q| q.svg_size(Self::QUIET) as u32),
            qr_path: shown.qr.as_ref().map(|q| q.to_svg_path(Self::QUIET)),
        }
    }
}

/// Approving a candidate from Director (this peer as a mesh member).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ApproveEvent {
    /// Dialling one of the candidate's addresses. A machine with several
    /// (a VPN, Docker) is tried best-first; this says which.
    Trying {
        addr: String,
    },
    Code {
        #[ts(type = "number")]
        session: u64,
        code: String,
        joiner_name: String,
        joiner_id: String,
    },
    Done {
        node_id: String,
        node_name: String,
    },
    Failed {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Change control (M11 §5)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Facts (M18 §4.10 P6)
// ---------------------------------------------------------------------------

/// One collector's answer about one subject, flattened for display.
///
/// The document arrives as JSON and leaves as `dotted.path -> value` pairs,
/// because the keys **are** what a `fact.*` selector addresses. Showing a
/// pretty-printed tree would look tidier and hide the one thing a reader
/// needs to copy.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct FactDocView {
    pub collector: String,
    pub subject_name: String,
    /// `(key, value)` in key order, arrays already joined.
    pub entries: Vec<(String, String)>,
    /// The document verbatim, for the copy button and for anyone who wants
    /// the shape rather than the keys.
    pub json: String,
    pub digest: String,
    /// When the collector **observed** this. The timestamp the panel shows,
    /// and the one the question "how old is this?" is actually about.
    #[ts(type = "number")]
    pub collected_at_micros: u64,
    /// When this node merged the op that carried it. Usually within
    /// seconds of the above; a large gap means the subject was partitioned
    /// or offline, which is worth being able to see.
    #[ts(type = "number")]
    pub published_at_micros: u64,
    pub author_name: String,
}

impl From<mesh_proto::FactRow> for FactDocView {
    fn from(r: mesh_proto::FactRow) -> Self {
        let parsed = mesh_core::facts::parse(&r.doc);
        let entries: Vec<(String, String)> = mesh_core::facts::flatten(&parsed)
            .into_iter()
            // An array publishes both a joined form and its indices; the
            // joined form is the one a person reads.
            .filter(|(k, _)| {
                !k.rsplit_once('.')
                    .is_some_and(|(_, tail)| tail.parse::<usize>().is_ok())
            })
            .collect();
        FactDocView {
            collector: r.collector,
            subject_name: r.subject_name,
            entries,
            json: serde_json::from_str::<serde_json::Value>(&r.doc)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or(r.doc),
            digest: r.digest,
            collected_at_micros: r.collected_at_micros,
            published_at_micros: r.published_at_micros,
            author_name: r.author_name,
        }
    }
}

/// One collector, as the Facts panel and the collectors list show it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct FactCollectorView {
    pub name: String,
    pub run: String,
    pub on: String,
    #[ts(type = "number")]
    pub interval_secs: u64,
    pub subject: String,
    pub paused: bool,
    pub built_in: bool,
    pub description: String,
    pub subjects: u32,
    /// Non-empty when this node has held the collector back for publishing
    /// faster than once an hour: the keys that changed when nothing
    /// changed. Never hidden behind a disclosure triangle — an unbounded
    /// op-log is a correctness problem, not a style one (§4.6).
    pub flapping_keys: Vec<String>,
    pub source: String,
}

impl From<mesh_proto::FactCollectorRow> for FactCollectorView {
    fn from(r: mesh_proto::FactCollectorRow) -> Self {
        FactCollectorView {
            name: r.name,
            run: r.run,
            on: r.on,
            interval_secs: r.interval_secs,
            subject: r.subject,
            paused: r.paused,
            built_in: r.built_in,
            description: r.description,
            subjects: r.subjects,
            flapping_keys: r.flapping_keys,
            source: r.source,
        }
    }
}

/// One step of a subject's fact history.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct FactChangeView {
    pub collector: String,
    #[ts(type = "number")]
    pub at_micros: u64,
    pub changes: Vec<FactKeyChangeView>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct FactKeyChangeView {
    pub key: String,
    /// `added` | `removed` | `changed`.
    pub verb: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl From<mesh_proto::FactDiffRow> for FactChangeView {
    fn from(r: mesh_proto::FactDiffRow) -> Self {
        FactChangeView {
            collector: r.collector,
            at_micros: r.at_micros,
            changes: r
                .changes
                .into_iter()
                .map(|c| FactKeyChangeView {
                    verb: c.verb().to_string(),
                    key: c.key,
                    before: c.before,
                    after: c.after,
                })
                .collect(),
        }
    }
}

/// Everything the Facts modal shows about one node, in one round trip.
///
/// One command rather than three, because the modal opens on a click and
/// three sequential IPC calls is three chances to render half a panel.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct NodeFactsView {
    pub subject_name: String,
    pub docs: Vec<FactDocView>,
    /// Recent changes, newest first.
    pub history: Vec<FactChangeView>,
    /// True when the history index is off on this node, so the panel can
    /// say *why* the timeline is empty instead of implying nothing changed.
    pub history_available: bool,
}

// ---------------------------------------------------------------------------
// Results (M19 §5.7 P6)
// ---------------------------------------------------------------------------

/// One target's answer, as the results table shows it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ResultRowView {
    pub node: String,
    pub job_id: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub outputs: Vec<(String, String)>,
    /// `null` when nothing was asserted; `false` is the row that made the
    /// command exit non-zero.
    pub expect_passed: Option<bool>,
    pub expect_detail: String,
}

impl From<mesh_proto::ResultRow> for ResultRowView {
    fn from(r: mesh_proto::ResultRow) -> Self {
        ResultRowView {
            node: r.node_name,
            job_id: r.job_id.to_display_string(),
            state: r.state.as_str().to_string(),
            exit_code: r.exit_code,
            outputs: r.outputs,
            expect_passed: r.expect_passed,
            expect_detail: r.expect_detail,
        }
    }
}

/// One fan-out's rows, plus who is answering for them.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ResultSetView {
    pub group: String,
    /// The node that owns or owned the group. Named on every response,
    /// because "this node has never seen that group" is only useful when it
    /// comes with the address of the node that has (§5.3).
    pub owner: String,
    pub command: String,
    pub rows: Vec<ResultRowView>,
    pub succeeded: u32,
    pub failed: u32,
    /// Targets in the plan that never reported a row.
    pub missing: u32,
    #[ts(type = "number")]
    pub created_at_micros: u64,
}

impl From<mesh_proto::ResultSet> for ResultSetView {
    fn from(set: mesh_proto::ResultSet) -> Self {
        ResultSetView {
            group: set.group.to_display_string(),
            owner: set.owner_name.clone(),
            command: set.preview.clone(),
            succeeded: u32::try_from(set.succeeded()).unwrap_or(u32::MAX),
            failed: u32::try_from(set.failed()).unwrap_or(u32::MAX),
            missing: set.missing,
            created_at_micros: set.created_at_micros,
            rows: set.rows.into_iter().map(Into::into).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Schedules (M8 §3.10)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Arbitration (C1 P6): the queue
// ---------------------------------------------------------------------------

/// One waiting job, in the order this node will run them.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct QueueRowView {
    pub position: u32,
    pub job: String,
    pub user: String,
    pub username: String,
    pub priority: String,
    /// Dominant share, as a percentage. Lower is further behind,
    /// and further behind runs first.
    pub share_pct: u32,
}

impl From<mesh_proto::QueueRow> for QueueRowView {
    fn from(r: mesh_proto::QueueRow) -> Self {
        QueueRowView {
            position: r.position,
            job: r.job.to_display_string(),
            user: r.principal.to_display_string(),
            username: r.username,
            priority: format!("{:?}", r.priority).to_lowercase(),
            share_pct: r.share_permille / 10,
        }
    }
}

/// The answer to a `--dry-run`: the fully resolved submission.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct JobPreviewView {
    pub kind: String,
    pub action: String,
    pub service: Option<String>,
    pub version: Option<u32>,
    pub bundle_id: Option<String>,
    pub runtime: Option<String>,
    pub selector: String,
    pub targets: Vec<String>,
    /// Matched, but the caller's grants exclude them.
    pub excluded: Vec<String>,
    pub env_keys: Vec<String>,
    /// Secrets by *name*. A preview is a thing people paste into tickets.
    pub secrets: Vec<String>,
    pub priority: String,
    #[ts(type = "number | null")]
    pub timeout_secs: Option<u64>,
    pub blast_radius: u32,
}

impl From<mesh_proto::JobPreview> for JobPreviewView {
    fn from(p: mesh_proto::JobPreview) -> Self {
        JobPreviewView {
            kind: p.kind,
            action: p.action,
            service: p.service,
            version: p.version,
            bundle_id: p.bundle_id.map(|b| b.to_display_string()),
            runtime: p.runtime,
            selector: p.selector,
            targets: p.targets,
            excluded: p.excluded,
            env_keys: p.env_keys,
            secrets: p.secrets,
            priority: p.priority,
            timeout_secs: p.timeout_secs,
            blast_radius: p.blast_radius,
        }
    }
}

// -- deployments (C2 §6) ----------------------------------------------------

/// One deployment, as the Deployments list shows it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentView {
    pub name: String,
    pub service: String,
    pub count: String,
    pub targets: String,
    pub generation: u32,
    /// `active`, `progressing`, `updating`, `canarying`, `paused`, or
    /// `degraded`.
    pub status: String,
    pub desired: u32,
    pub running: u32,
    pub healthy: u32,
    pub created_by: String,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    pub source: Option<String>,
}

impl From<mesh_proto::DeploymentRow> for DeploymentView {
    fn from(r: mesh_proto::DeploymentRow) -> Self {
        DeploymentView {
            name: r.name,
            service: r.service,
            count: r.count,
            targets: r.targets,
            generation: r.generation,
            status: r.status,
            desired: r.desired,
            running: r.running,
            healthy: r.healthy,
            created_by: r.created_by_username,
            created_at_micros: r.created_at_micros,
            source: r.source,
        }
    }
}

/// One machine in the placement map (Phase C, U8) — Mesh → Placement.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlacementNodeView {
    pub node_id: String,
    pub name: String,
    pub liveness: String,
    pub cores: u32,
    #[ts(type = "number")]
    pub mem_mb: u64,
    pub eligible: bool,
    pub draining: bool,
    pub leaving: bool,
    #[ts(type = "number | null")]
    pub drain_deadline_secs: Option<u64>,
    pub instances: Vec<PlacementInstanceView>,
}

/// One deployment instance on a machine (Phase C, U8).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlacementInstanceView {
    pub deployment: String,
    pub slot: String,
    pub generation: u32,
    pub state: String,
    pub health: String,
    pub cpus: u32,
    #[ts(type = "number")]
    pub mem_mb: u64,
}

impl From<mesh_proto::PlacementNodeRow> for PlacementNodeView {
    fn from(r: mesh_proto::PlacementNodeRow) -> Self {
        PlacementNodeView {
            node_id: r.node.to_display_string(),
            name: r.name,
            liveness: r.liveness,
            cores: r.cores,
            mem_mb: r.mem_mb,
            eligible: r.eligible,
            draining: r.draining,
            leaving: r.leaving,
            drain_deadline_secs: r.drain_deadline_secs,
            instances: r
                .instances
                .into_iter()
                .map(|i| PlacementInstanceView {
                    deployment: i.deployment,
                    slot: i.slot,
                    generation: i.generation,
                    state: i.state,
                    health: i.health,
                    cpus: i.cpus,
                    mem_mb: i.mem_mb,
                })
                .collect(),
        }
    }
}

/// One instance in a deployment's live status.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentInstanceView {
    pub job_id: String,
    pub slot: String,
    pub node: String,
    pub generation: u32,
    pub state: String,
    /// `healthy`, `unhealthy`, `starting`, or `unknown`.
    pub health: String,
    pub health_detail: String,
    #[ts(type = "number | null")]
    pub started_at_micros: Option<u64>,
}

impl From<mesh_proto::DeploymentInstanceRow> for DeploymentInstanceView {
    fn from(r: mesh_proto::DeploymentInstanceRow) -> Self {
        DeploymentInstanceView {
            job_id: r.job_id.to_display_string(),
            slot: r.slot,
            node: r.node,
            generation: r.generation,
            state: r.state,
            health: r.health,
            health_detail: r.health_detail,
            started_at_micros: r.started_at_micros,
        }
    }
}

/// A deployment's full live status: the roll's progress and every instance.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatusView {
    pub name: String,
    pub service: String,
    pub count: String,
    pub targets: String,
    pub generation: u32,
    pub status: String,
    pub desired: u32,
    pub running: u32,
    pub healthy: u32,
    pub reconciler: Option<String>,
    /// The generation a roll is coming from, if one is in flight.
    pub updating_from: Option<u32>,
    pub degraded_reason: Option<String>,
    /// The canary set is up beside the old instances and still proving
    /// itself; nothing has been cut over (N1 P3).
    pub canarying: bool,
    /// Settled canaries holding for `deployment promote` (Phase B, E2).
    pub awaiting_promotion: bool,
    /// The roll has made no progress for `progress_deadline` (N1 P3).
    /// Distinct from `degraded_reason`: a stalled roll may have nothing
    /// crashing at all.
    pub stalled_reason: Option<String>,
    /// Live members too old to apply this deployment's policy (N1 P3/P4).
    /// Empty unless there is a policy to lose.
    pub policy_blind: Vec<String>,
    /// The port labels this deployment's instances ask for (N2 P2).
    ///
    /// What was *declared*, not what was allocated: a dynamic port is
    /// chosen on the executor at spawn and does not replicate until the
    /// service registry lands. `meshnode ports` — and the Mesh → Capacity
    /// panel — show one node's real numbers.
    pub port_labels: Vec<String>,
    /// Why nothing can be placed, when nothing can be (N1 P2). `None`
    /// whenever the mesh has *any* feasible node, so a value here is
    /// always a real refusal rather than a roll in progress.
    pub placement: Option<PlacementRefusalView>,
    /// Instances on a node this node does not currently hear as Alive
    /// (Phase A, U4). Their rows read `unreachable`.
    pub unreachable: u32,
    /// What happens when a node running an instance goes away, as one
    /// sentence from `mesh_proto::availability` (Phase A, U6).
    pub availability: String,
    /// `container`, `bash`, … — a *Shell* inside an instance is offered only
    /// for containers (Phase B, E3).
    pub runtime: String,
    pub instances: Vec<DeploymentInstanceView>,
}

/// Why a placement cannot be satisfied (N1 P2).
///
/// The sentences come from `PlacementRefusal::lines()` in `mesh-proto`, not
/// from this layer: the CLI prints them and Director shows them, and two
/// renderings of one refusal would eventually say different things about
/// the same mesh. The structured fields ride along so the UI can lay them
/// out rather than print a list.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlacementRefusalView {
    pub selector: String,
    pub considered: u32,
    pub feasible: u32,
    /// Reason slug → how many nodes it excluded.
    pub excluded: Vec<(String, u32)>,
    /// True when every reason clears on its own: a queue to watch rather
    /// than a file to edit.
    pub transient: bool,
    /// The refusal as the lines a person reads.
    pub lines: Vec<String>,
}

impl From<mesh_proto::PlacementRefusal> for PlacementRefusalView {
    fn from(r: mesh_proto::PlacementRefusal) -> Self {
        PlacementRefusalView {
            lines: r.lines(),
            transient: r.transient(),
            selector: r.selector,
            considered: r.considered,
            feasible: r.feasible,
            excluded: r
                .excluded
                .into_iter()
                .map(|(reason, n)| (reason.as_str().to_string(), n))
                .collect(),
        }
    }
}

impl From<mesh_proto::DeploymentStatusView> for DeploymentStatusView {
    fn from(v: mesh_proto::DeploymentStatusView) -> Self {
        DeploymentStatusView {
            name: v.name,
            service: v.service,
            count: v.count,
            targets: v.targets,
            generation: v.generation,
            status: v.status,
            desired: v.desired,
            running: v.running,
            healthy: v.healthy,
            reconciler: v.reconciler,
            updating_from: v.updating_from,
            degraded_reason: v.degraded_reason,
            canarying: v.canarying,
            awaiting_promotion: v.awaiting_promotion,
            stalled_reason: v.stalled_reason,
            policy_blind: v.policy_blind,
            port_labels: v.port_labels,
            placement: v.placement.map(Into::into),
            unreachable: v.unreachable,
            availability: v.availability,
            runtime: v.runtime,
            instances: v.instances.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod export {
    // `cargo test -p meshware-director export_bindings` regenerates the
    // TypeScript bindings under apps/director/src/lib/bindings/ — the only
    // place they are written. Types derive `TS` without `#[ts(export)]`:
    // that attribute makes ts-rs add a test per type writing to the crate's
    // own `bindings/`, which is how a stale second copy of all of them came
    // to be checked in (DIR-15).
    #[test]
    fn export_bindings() {
        use ts_rs::TS as _;
        let cfg = ts_rs::Config::new().with_out_dir("../src/lib/bindings");
        macro_rules! export {
            ($($ty:ty),+ $(,)?) => {
                $(<$ty>::export_all(&cfg).expect("export");)+
            };
        }
        export!(
            // UX-7 — what this build is, and its licence.
            crate::about::AboutView,
            // R2 P4 — what a script says about itself, for the wizard.
            super::ScriptSpecView,
            // B9 — the key-at-rest gate and its Settings panel.
            super::IdentityLockView,
            // Y2/Y3 — the security key card and the authorities it trusts.
            crate::security_key::SecurityKeyStatusView,
            crate::security_key::SecurityKeyPlanView,
            crate::security_key::SecurityKeyBoundView,
            crate::security_key::SecurityKeyTestView,
            crate::security_key::SetAsideView,
            crate::security_key::AttestRootView,
            // The join queue: who is waiting on this network, and whether
            // the two ways of hearing them are working.
            // E1-P4 / E1-P5 / E2.4 — machine principals, identity
            // providers, and the quarantine field on NodeView.
            super::ServiceAccountView,
            super::ServiceAccountCreated,
            super::OidcProviderView,
            super::CandidateView,
            // SEC-17 — the open invites, and taking one back.
            super::InviteView,
            // SEC-14 — a pick, named by a token, and the builds that carry one.
            super::PickedView,
            super::BuildCandidateView,
            super::LanWatchView,
            super::ApproveEvent,
            super::WizardPairEvent,
            // M31: how long we keep it, and who else gets a copy.
            super::RetentionRowView,
            super::RetentionView,
            super::SinkKindView,
            super::SinkView,
            super::SinkDraft,
            // M31 B3: which tier answered, and reading a payload back.
            super::TierView,
            super::PayloadWindowView,
            // M31 A3: what this node is refusing.
            super::BackpressureView,
            super::PressureRowView,
            super::HistoryJobPageView,
            // Add machines / upgrade over SSH / the release channel.
            super::DeployAuthDraft,
            super::RememberedCredentialView,
            super::DeployDraft,
            super::BinarySourceView,
            super::DeployPreviewView,
            super::DeployEvent,
            super::SshUpgradeNodeDraft,
            super::SshUpgradeDraft,
            super::ReleaseCheckView,
            super::PublishedReleaseView,
            super::ReleaseFetchEvent,
            crate::selfupdate::DirectorUpdateView,
            crate::selfupdate::DirectorUpdateEvent,
            super::FactDocView,
            super::FactCollectorView,
            super::FactChangeView,
            super::FactKeyChangeView,
            super::NodeFactsView,
            super::ResultRowView,
            super::ResultSetView,
            super::SessionView,
            super::SessionPolicyView,
            super::TerminalEvent,
            // C7 and C8 (C0-C9 §11, §12).
            super::ScoreTermView,
            super::CandidateScoreView,
            super::PlacementView,
            super::CacheHitView,
            super::KeyFieldView,
            super::JobExplainView,
            super::CacheServiceView,
            super::CacheStatsView,
            super::FarmBuildLine,
            super::PlanDelayView,
            super::PlanView,
            super::QueueRowView,
            super::JobPreviewView,
            super::BootstrapView,
            super::StatusView,
            super::WanView,
            super::NodeView,
            super::TopView,
            // Missing from this list until L2 P1, so `src/lib/bindings`
            // carried a hand-stale copy that the frontend compiled
            // against — the exact drift the export test exists to prevent.
            super::TotalsView,
            crate::deeplink::DeepLinkView,
            super::JobView,
            super::JobCountsView,
            super::JobsPageView,
            super::StateVersionsView,
            super::ServiceView,
            super::BundleView,
            super::BundleFileView,
            super::PushResultView,
            super::SourceView,
            super::SyncResultView,
            super::BumpedView,
            super::SecretView,
            super::SecretSetView,
            super::ProviderView,
            super::UserView,
            super::GrantView,
            super::AuditView,
            super::WhoAmIView,
            super::EnrolmentView,
            super::PingView,
            super::DoctorCheckView,
            super::DoctorFixView,
            super::DoctorFixReport,
            super::BackupView,
            super::RestoreView,
            super::CandidateView,
            super::FanTargetView,
            super::RunEvent,
            super::CapacityView,
            super::DeviceView,
            super::NodeProgressView,
            super::ParallelEvent,
            super::DrainEvent,
            super::DefinitionView,
            super::PrefetchEvent,
            super::UpgradeEvent,
            super::WizardPairEvent,
            crate::demo::DemoEvent,
            super::ApproveEvent,
            super::PairCodeView,
            super::ObjectRefView,
            super::DeploymentView,
            super::DeploymentInstanceView,
            // Phase C, U8 — the placement map.
            super::PlacementNodeView,
            super::PlacementInstanceView,
            super::DeploymentStatusView,
            // N1 P2/P5 — the refusal a stuck rollout shows, and schedules.
            super::PlacementRefusalView,
            super::DeploymentCheckView,
            super::PlanChangeView,
            super::InstanceStatView,
            super::IncarnationView,
            super::DeploymentInstanceDetailView,
            super::ServiceScheduleView,
            super::ServiceScheduleCheckView,
            // N2 P1 — the port ledger panel.
            super::PortLedgerViewT,
            super::PortHeldViewT,
            // N3 — the registry and the responder.
            super::RegistryRowView,
            super::RegistryInstanceView,
            super::DnsStatusViewT,
            // N5 — the volume ledger panel.
            super::VolumeRowView,
            // N6 — the mesh-wide shed.
            super::ShedView,
            // Seventeen the frontend imports that this list never named
            // (DIR-15). Their files came from ts-rs's per-type
            // `#[ts(export)]` tests, which wrote a second copy of every
            // binding to `src-tauri/bindings/`, and were copied across by
            // hand — so a change to any of them regenerated the copy nobody
            // reads and not the one the app does. This list is the only
            // export now: a type the frontend needs is named here, or
            // reached from one that is.
            super::ContentPolicyOption,
            super::ContentPruneView,
            super::ContentStatusView,
            super::DeploymentHistoryView,
            super::DeploymentTagView,
            super::DeploymentVersionView,
            super::DiskView,
            super::GangRowView,
            super::NodeRuntimesT,
            super::ReleaseKeyView,
            super::ReleaseTrustView,
            super::ReleaseCosignView,
            super::RunDefaultsSavedView,
            super::RuntimeReportT,
            super::RuntimesViewT,
            super::TrayStatusView,
        );
    }
}

// ---------------------------------------------------------------------------
// M17 the fabric (§3.12 P6)
// ---------------------------------------------------------------------------

/// One live session, as the Sessions panel shows it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    #[ts(type = "number")]
    pub id: u64,
    /// `shell` | `exec` | `cp` | `forward`.
    pub kind: String,
    pub detail: String,
    /// True when this machine is the one running the process, holding the
    /// file or dialling the port; false when it is the initiator. The two
    /// are very different facts and the panel says which.
    pub hosting: bool,
    pub peer_name: String,
    pub username: String,
    /// Why the operator said they opened it.
    pub reason: String,
    /// The OS account the shell is running as on the host.
    pub os_user: String,
    #[ts(type = "number")]
    pub opened_at_micros: u64,
    #[ts(type = "number")]
    pub bytes_in: u64,
    #[ts(type = "number")]
    pub bytes_out: u64,
}

impl From<mesh_proto::SessionRow> for SessionView {
    fn from(r: mesh_proto::SessionRow) -> Self {
        SessionView {
            id: r.id,
            kind: r.kind,
            detail: r.detail,
            hosting: r.hosting,
            peer_name: r.peer_name,
            username: r.username,
            reason: r.reason,
            os_user: r.os_user,
            opened_at_micros: r.opened_at_micros,
            bytes_in: r.bytes_in,
            bytes_out: r.bytes_out,
        }
    }
}

/// Frames of a live terminal, streamed to the panel that renders it.
///
/// `bytes` rather than a string, and deliberately: terminal output is a
/// byte stream that can split a UTF-8 sequence — or an escape sequence —
/// across two reads, and a `String` here would either lose those bytes or
/// force the backend to buffer on a boundary it cannot know. The emulator
/// in the panel is the thing that reassembles them, because it is the thing
/// that understands them.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TerminalEvent {
    /// The far end authorized the session and is live.
    Opened,
    /// PTY output, **base64** (**M-20**).
    ///
    /// A `Vec<u8>` serialises into a JSON array of decimal numbers —
    /// `[104,101,...]` — which is about four bytes of JSON per byte of
    /// output, plus a large numeric-array parse in the webview for every
    /// frame. A 32 KiB session frame became ~120 KB of JSON, so `cat
    /// bigfile` or a fast `htop` lagged a native terminal showing the same
    /// thing. Base64 is 1.33 bytes per byte and one string parse.
    ///
    /// Not Tauri's binary channel, which would be 1:1: this enum also
    /// carries `Opened`, `Exit` and `Error`, and a raw-bytes channel can
    /// only carry bytes — so it would take either a second channel or a
    /// hand-rolled frame header to say which kind a message is. One
    /// encoding of one event type is the cheaper trade.
    Data {
        b64: String,
    },
    Exit {
        code: Option<i32>,
    },
    Error {
        message: String,
    },
}

/// One session policy, as the Access view lists it (M17 §3.4).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionPolicyView {
    pub id: String,
    pub description: String,
    pub users: String,
    pub nodes: String,
    pub allow: Vec<String>,
    #[ts(type = "number | null")]
    pub expires_at_micros: Option<u64>,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    pub created_by_username: String,
    /// Whether this policy covers the node answering — the answer to "why
    /// can I not get a prompt on *this* box".
    pub covers_this_node: bool,
}

impl From<mesh_proto::SessionPolicyRow> for SessionPolicyView {
    fn from(r: mesh_proto::SessionPolicyRow) -> Self {
        SessionPolicyView {
            id: r.id,
            description: r.description,
            users: r.users,
            nodes: r.nodes,
            allow: r.allow,
            expires_at_micros: r.expires_at_micros,
            created_at_micros: r.created_at_micros,
            created_by_username: r.created_by_username,
            covers_this_node: r.covers_this_node,
        }
    }
}

// -- release keys and publishing (M24 §9) ------------------------------------

/// One minisign key this mesh trusts to sign releases.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseKeyView {
    pub pubkey: String,
    pub label: String,
    pub added_by: String,
    pub added_at_micros: String,
    pub revoked: bool,
    /// False when this node's own `[release] pubkey` overrides the mesh's
    /// list — a distinction a fleet screen must not hide.
    pub in_use_here: bool,
}

/// What the fleet verifies releases against, and what it would say if
/// asked to install one right now.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseTrustView {
    pub keys: Vec<ReleaseKeyView>,
    /// A security key's release key is trusted (Y6), so every release needs
    /// its co-signature before any node installs it.
    pub cosign_required: bool,
    /// Every published binary and whether it has that co-signature.
    pub releases: Vec<ReleaseCosignView>,
    pub local_override: Option<String>,
    /// True while this build carries the in-tree placeholder key: until a
    /// key is trusted, **no release will install anywhere**. The one fact
    /// that turns a baffling refusal months later into a sentence now.
    pub embedded_is_placeholder: bool,
    /// True when nothing would verify a release: no trusted key, no local
    /// override, and a placeholder embedded key.
    pub nothing_would_verify: bool,
}

/// One published binary, and the co-signature it has or lacks (Y6).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseCosignView {
    pub version: String,
    pub arch: String,
    /// What a co-signature is filed under; sent back to `release_cosign`.
    pub digest: String,
    /// The trusted key that co-signed it, if one has.
    pub cosigned_by: Option<String>,
    /// A human touched the device to make that signature.
    pub user_present: bool,
}

/// A build sitting on this machine, ready to publish (or not, and why).
///
/// Publishing used to be a path typed from memory into a CLI. This is the
/// same information as a list to click, with every reason a build would be
/// refused worked out *before* anyone commits to it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BuildCandidateView {
    /// Shown, never sent back: [`Self::token`] is what publishes it.
    pub path: String,
    /// Names this build to `release_publish` (SEC-14).
    pub token: String,
    pub file_name: String,
    pub version: String,
    pub arch: String,
    pub size: String,
    /// A `.minisig` beside the binary.
    pub signed: bool,
    /// The signature verifies against a key this mesh trusts.
    pub verified: bool,
    /// Already published into this mesh under that version.
    pub published: bool,
    /// Why it cannot be published, in a sentence, or null when it can.
    pub blocked: Option<String>,
}

// ---------------------------------------------------------------------------
// M28 the drafting agent
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// C7 the action cache, C8 legibility (C0-C9 §11, §12, §14.3)
// ---------------------------------------------------------------------------

/// One scoring term, ready to draw.
///
/// `weighted` is the bar's length and `raw` is its tooltip: the two answer
/// different questions — *how much did that cost it* and *how loaded was
/// that box* — and a chart that plotted only one would be lying in one of
/// the two directions.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ScoreTermView {
    pub name: String,
    pub raw: f64,
    pub weighted: f64,
    pub weight: f64,
    /// Nothing could measure this input; the neutral default stood in. A
    /// chart that does not mark this makes an unmeasured node look like a
    /// mediocre one (M9 §5.2).
    pub unknown: bool,
    /// What this term measures, in the words a person would use (L2 P4).
    /// `null` for a term this build has no words for — a newer node's
    /// placement record, which says nothing rather than guessing.
    pub gloss: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CandidateScoreView {
    pub node: String,
    pub name: String,
    pub total: f64,
    pub chosen: bool,
    pub terms: Vec<ScoreTermView>,
}

/// The placement chart's data (C8 P2) — *the screenshot that sells the
/// product*, and the one thing in this milestone to build first if only one
/// gets built.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlacementView {
    pub candidates: Vec<CandidateScoreView>,
    pub considered: u32,
    /// The best candidate answered Busy and the job went to the next one.
    /// Shown, because otherwise the chart appears to pick the runner-up for
    /// no reason.
    pub spilled: bool,
    /// The term that cost the runner-up the most, and by how much: the
    /// one-line answer under the chart.
    pub decisive: Option<String>,
    pub decisive_delta: Option<f64>,
    /// The largest weighted value anywhere in the chart, so every bar can
    /// be scaled against one axis rather than against its own row.
    pub max_weighted: f64,
    /// **Why the work landed there, in one sentence** (L2 P4).
    ///
    /// Composed by `PlacementExplain::why()`, so this hover, the CLI's
    /// `jobs explain` and its `--output json` all say the same words.
    /// Render it; never re-derive it from the figures above.
    pub why: Option<String>,
}

impl From<mesh_proto::PlacementExplain> for PlacementView {
    fn from(p: mesh_proto::PlacementExplain) -> Self {
        let (decisive, decisive_delta) = match p.decisive_term() {
            Some((name, delta)) => (Some(name), Some(delta)),
            None => (None, None),
        };
        let max_weighted = p
            .candidates
            .iter()
            .flat_map(|c| c.terms.iter().map(|t| t.weighted))
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON);
        PlacementView {
            candidates: p
                .candidates
                .iter()
                .map(|c| CandidateScoreView {
                    node: c.node.to_display_string(),
                    name: c.name.clone(),
                    total: c.total,
                    chosen: Some(c.node) == p.chosen,
                    terms: c
                        .terms
                        .iter()
                        .map(|t| ScoreTermView {
                            name: t.name.clone(),
                            raw: t.raw,
                            weighted: t.weighted,
                            weight: t.weight,
                            unknown: t.unknown,
                            gloss: mesh_proto::term_gloss(&t.name).map(str::to_string),
                        })
                        .collect(),
                })
                .collect(),
            considered: p.considered,
            spilled: p.spilled,
            decisive,
            decisive_delta,
            max_weighted,
            why: p.why(),
        }
    }
}

/// A cache hit, as a badge on a run or a step (C7 P5).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CacheHitView {
    pub key: String,
    pub short_key: String,
    pub produced_by: String,
    pub producer: String,
    #[ts(type = "number")]
    pub bytes_saved: u64,
    pub verified: bool,
}

/// One field of the action key, for *"why was this a miss"* (C7 P5).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct KeyFieldView {
    pub name: String,
    pub value: String,
    pub digest: String,
    /// This field differs from the run being compared against. The whole
    /// point of the panel: a miss is one highlighted row, not a mystery.
    pub differs: bool,
}

/// Everything Director shows about one job's placement and cache history.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct JobExplainView {
    pub job: String,
    pub short_id: String,
    pub cmd_preview: String,
    pub state: String,
    pub executor_name: String,
    #[ts(type = "number | null")]
    pub duration_micros: Option<u64>,
    pub placement: Option<PlacementView>,
    /// When this node did not place the job: who did.
    pub placement_on: Option<String>,
    pub action_key: Option<String>,
    pub cache_hit: Option<CacheHitView>,
    pub key_fields: Vec<KeyFieldView>,
    /// What the action key is and what matching one means (L2 P4) — the
    /// hover on the cache badge and on the key panel. `null` on a job that
    /// was never cacheable, where there is no key to explain.
    pub key_why: Option<String>,
}

impl JobExplainView {
    /// Build the view, marking key fields that differ from `compare`.
    ///
    /// `compare` is another run of the same shape — usually the previous
    /// one — and the diff is what turns "this was a miss" into "this was a
    /// miss because `env` changed".
    pub fn of(v: mesh_proto::JobExplainView, compare: Option<&[mesh_proto::KeyField]>) -> Self {
        // The same sentence `KeyExplain::why()` gives the CLI, composed
        // from the field count so the panel, the footer and `--output json`
        // cannot drift.
        let key_why = (!v.key_fields.is_empty()).then(|| mesh_proto::key_why(v.key_fields.len()));
        JobExplainView {
            job: v.job.to_display_string(),
            short_id: v.job.short(),
            cmd_preview: v.cmd_preview,
            state: v.state,
            executor_name: v.executor_name,
            duration_micros: v.duration_micros,
            placement: v.placement.map(Into::into),
            placement_on: v.placement_on.map(|n| n.short()),
            action_key: v.action_key.map(|k| k.to_display_string()),
            cache_hit: v.cache_hit.map(|h| CacheHitView {
                key: h.key.to_display_string(),
                short_key: h.key.short(),
                produced_by: h.produced_by.short(),
                producer: h.producer.short(),
                bytes_saved: h.bytes_saved,
                verified: h.verified,
            }),
            key_fields: v
                .key_fields
                .into_iter()
                .map(|f| {
                    let differs = compare.is_some_and(|other| {
                        !other
                            .iter()
                            .any(|o| o.name == f.name && o.digest == f.digest)
                    });
                    KeyFieldView {
                        name: f.name,
                        value: f.value,
                        digest: f.digest,
                        differs,
                    }
                })
                .collect(),
            key_why,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CacheServiceView {
    pub service: String,
    #[ts(type = "number")]
    pub hits: u64,
    #[ts(type = "number")]
    pub misses: u64,
    #[ts(type = "number")]
    pub entries: u64,
    #[ts(type = "number")]
    pub bytes_saved: u64,
    /// `null` when nothing has been asked of this service yet — which is
    /// not the same as a zero percent hit rate and must not render as one.
    pub hit_rate: Option<f64>,
}

/// The hit-rate panel (C7 P5).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatsView {
    pub enabled: bool,
    #[ts(type = "number")]
    pub entries: u64,
    #[ts(type = "number")]
    pub capacity: u64,
    #[ts(type = "number")]
    pub hits: u64,
    #[ts(type = "number")]
    pub misses: u64,
    pub hit_rate: Option<f64>,
    #[ts(type = "number")]
    pub bytes_saved: u64,
    #[ts(type = "number")]
    pub published: u64,
    #[ts(type = "number")]
    pub evicted: u64,
    #[ts(type = "number")]
    pub verified: u64,
    /// Non-zero is an emergency, and the only number on this panel that is.
    #[ts(type = "number")]
    pub mismatched: u64,
    #[ts(type = "number")]
    pub epoch: u64,
    pub verify_rate: f64,
    pub by_service: Vec<CacheServiceView>,
}

impl From<mesh_proto::CacheStats> for CacheStatsView {
    fn from(s: mesh_proto::CacheStats) -> Self {
        let hit_rate = s.hit_rate();
        CacheStatsView {
            enabled: s.enabled,
            entries: s.entries,
            capacity: s.capacity,
            hits: s.hits,
            misses: s.misses,
            hit_rate,
            bytes_saved: s.bytes_saved,
            published: s.published,
            evicted: s.evicted,
            verified: s.verified,
            mismatched: s.mismatched,
            epoch: s.epoch,
            verify_rate: s.verify_rate,
            by_service: s
                .by_service
                .into_iter()
                .map(|r| CacheServiceView {
                    hit_rate: r.hit_rate(),
                    service: r.service,
                    hits: r.hits,
                    misses: r.misses,
                    entries: r.entries,
                    bytes_saved: r.bytes_saved,
                })
                .collect(),
        }
    }
}

/// The what-if planner's answer (C8 P3).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub placement: Option<PlacementView>,
    pub chosen_name: Option<String>,
    pub slots_free: u32,
    pub slots_wanted: u32,
    #[ts(type = "number")]
    pub wait_secs: u64,
    /// Rendered as a range, never as a time: "12m–25m, p50 16m". A planner
    /// that says "17 minutes" is wrong precisely often enough to be
    /// untrusted forever (§12.2).
    pub duration: Option<String>,
    pub finish: Option<String>,
    pub samples: Option<u32>,
    pub delays: Vec<PlanDelayView>,
    pub refusal: Option<String>,
    /// What the range is and where it came from, in one sentence (L2 P4) —
    /// the hover on the estimate. `null` when nothing like this has run
    /// here before, which is a real state and not a missing sentence.
    pub duration_why: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlanDelayView {
    pub name: String,
    pub jobs: u32,
    #[ts(type = "number")]
    pub by_secs: u64,
}

impl From<mesh_proto::PlanEstimate> for PlanView {
    fn from(p: mesh_proto::PlanEstimate) -> Self {
        let chosen_name = p.placement.chosen_score().map(|c| c.name.clone());
        let has_placement = !p.placement.candidates.is_empty();
        PlanView {
            chosen_name,
            placement: has_placement.then(|| p.placement.into()),
            slots_free: p.slots_free,
            slots_wanted: p.slots_wanted,
            wait_secs: p.wait_secs,
            duration: p.duration.map(|d| d.describe()),
            finish: p.finish.map(|d| d.describe()),
            samples: p.duration.map(|d| d.samples),
            duration_why: p.duration.map(|d| d.why()),
            delays: p
                .delays
                .into_iter()
                .map(|d| PlanDelayView {
                    name: d.name,
                    jobs: d.jobs,
                    by_secs: d.by_secs,
                })
                .collect(),
            refusal: p.refusal,
        }
    }
}

// ---------------------------------------------------------------------------
// Add machines / upgrade over SSH (`deploy.rs`)
// ---------------------------------------------------------------------------

/// Credentials for one deploy, as the form sends them.
///
/// Crosses the webview boundary in both directions — down with a deploy,
/// and back up when the form is filled from the OS keychain
/// (`credentials.rs`) — and nowhere else: nothing in Director writes one
/// to disk, logs one, or keeps one past the run it was typed for. The
/// keychain is the OS's store, not Director's.
#[derive(Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeployAuthDraft {
    /// The SSH account, unless a target names its own (`ops@rack-1`).
    pub user: String,
    /// `password` | `key` | `agent`.
    pub method: String,
    pub password: Option<String>,
    /// A picker token for the private key file (SEC-14), or nothing — never
    /// a path. Resolved to one on the Rust side before anything reads it.
    pub key_path: Option<String>,
    pub passphrase: Option<String>,
    /// For `sudo` when it is not the SSH password (key and agent logins).
    pub sudo_password: Option<String>,
    /// Keep these in the OS keychain under the user name (`credentials.rs`).
    #[serde(default)]
    pub remember: bool,
    /// "Whatever is remembered for this user, fetch it yourself" (S3).
    ///
    /// Set by the form when it filled itself in from the keychain. The
    /// secret fields above then arrive **empty** and the backend reads them
    /// out of the OS store at the moment it needs them — so a remembered SSH
    /// password, key passphrase and sudo password never cross into the
    /// window at all. A field the person *typed* still wins: it is present
    /// here, and present beats remembered.
    #[serde(default)]
    pub from_keychain: bool,
}

/// Everything but the secrets (SEC-23): a draft is logged as part of the
/// request that carried it, and the passwords in it are not log material.
impl std::fmt::Debug for DeployAuthDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = |v: &Option<String>| v.as_ref().map(|_| "<redacted>");
        f.debug_struct("DeployAuthDraft")
            .field("user", &self.user)
            .field("method", &self.method)
            .field("password", &shown(&self.password))
            .field("key_path", &self.key_path)
            .field("passphrase", &shown(&self.passphrase))
            .field("sudo_password", &shown(&self.sudo_password))
            .field("remember", &self.remember)
            .field("from_keychain", &self.from_keychain)
            .finish()
    }
}

/// What is remembered for one SSH user — **which** secrets exist, never the
/// secrets (S3).
///
/// `credential_get` used to answer with the passwords themselves, which put
/// the operator's SSH and sudo passwords one `invoke` away from anything
/// running in the window. This is the same answer minus the part that
/// mattered: enough for the form to say "filled in from your keychain" and
/// to know which fields it may leave blank.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RememberedCredentialView {
    pub user: String,
    /// `password` | `key` | `agent`.
    pub method: String,
    /// Not a secret: the path to the private key file. Shown, never sent
    /// back (SEC-14).
    pub key_path: Option<String>,
    /// A picker token for that key file, which the form carries exactly as
    /// it carries a file the person picked. Without one the remembered key
    /// lived only in the keychain, so retyping a passphrase — which stops
    /// deferring to the keychain — dropped the key with it and sent the
    /// person back to the file picker for a file already on screen.
    pub key_token: Option<String>,
    pub has_password: bool,
    pub has_passphrase: bool,
    pub has_sudo_password: bool,
}

/// Everything **Add machines** was asked to do.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeployDraft {
    /// Raw text: one target per line or space, brace ranges allowed.
    pub targets: Vec<String>,
    pub auth: DeployAuthDraft,
    pub tags: Vec<String>,
    /// Where the binary goes; the scope's default when empty.
    pub install_path: Option<String>,
    /// Push this file instead of resolving a binary.
    /// A picker token for a meshnode binary to push (SEC-14), never a path.
    pub binary_path: Option<String>,
    /// Write the fleet-upgrade drop-in, so mesh-driven rollouts reach it.
    pub fleet_upgrade: bool,
    /// Open the mesh port in firewalld/ufw where one runs.
    pub open_firewall: bool,
    /// Hosts worked on at once.
    pub parallel: u32,
    /// A machine that already belongs to a mesh: move its identity aside
    /// and pair it again.
    pub replace_existing: bool,
    /// The SSH port for targets that do not name one. 22 is a default,
    /// not a law — a rack on 2222 should say so once rather than on every
    /// line, and a line that names its own port still wins.
    pub port: u16,
}

/// Where the binary for an architecture would come from.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BinarySourceView {
    pub arch: String,
    /// `file` | `mesh` | `bundled` | `local` | `channel` | `none`.
    pub kind: String,
    pub version: String,
    /// A sentence for the review step.
    pub detail: String,
}

/// What a deploy would do, before it does it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeployPreviewView {
    /// The targets, expanded and de-duplicated.
    pub hosts: Vec<String>,
    /// Entries that could not be read as a target, with why.
    pub problems: Vec<String>,
    pub sources: Vec<BinarySourceView>,
    /// Where host keys are recorded on first sight.
    pub known_hosts: String,
}

/// One deploy (or SSH upgrade), as it happens.
///
/// Keyed by `host` — the text the person typed for that machine — so the
/// screen can address the card it drew for it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DeployEvent {
    /// The run is under way; `hosts` is the order the cards go in.
    Plan {
        #[ts(type = "number")]
        run_id: u64,
        hosts: Vec<String>,
    },
    /// The host moved to a new step.
    Stage {
        host: String,
        /// `connecting` | `inspecting` | `preparing` | `installing` |
        /// `configuring` | `pairing` | `confirming` | `joining` |
        /// `starting` | `verifying`.
        stage: String,
        detail: String,
    },
    /// The host key, as soon as the session is up.
    HostKey {
        host: String,
        fingerprint: String,
        /// `known` | `new` | `changed`.
        status: String,
    },
    /// A machine Director has never seen before, asking whether to trust the
    /// key it just offered — **before** anything is authenticated (S-M9).
    ///
    /// Answered with `deploy_decide`, the same call the pairing code uses.
    /// Declining costs nothing: no password has been sent at this point, and
    /// that is the entire reason the question is asked here rather than
    /// after the session is up.
    HostKeyPrompt {
        host: String,
        fingerprint: String,
        algorithm: String,
    },
    /// What the machine turned out to be.
    Machine {
        host: String,
        hostname: String,
        distro: String,
        arch: String,
        /// `system` | `user`.
        scope: String,
        /// Something worth one sentence: a user-scope fallback, a firewall
        /// left alone.
        note: Option<String>,
    },
    /// Both pairing codes are in. `matched` is Director's own comparison
    /// — the person confirms it, never overrides it.
    Code {
        host: String,
        /// The code this Director's engine derived.
        ours: String,
        /// The code the machine printed over SSH.
        theirs: String,
        matched: bool,
        node_name: String,
    },
    Done {
        host: String,
        node_name: String,
        node_id: String,
        /// The build the node reported once it was seen alive.
        version: String,
        /// `systemd` | `systemd-user` | `nohup` | `none`.
        service: String,
        note: Option<String>,
    },
    Failed {
        host: String,
        stage: String,
        message: String,
        /// What to do about it, when there is something to do.
        hint: Option<String>,
    },
    /// Every host is accounted for.
    Finished {
        done: u32,
        failed: u32,
        cancelled: bool,
    },
}

/// One node to upgrade over SSH.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SshUpgradeNodeDraft {
    pub node_id: String,
    /// The SSH target, when it is not the node's own address.
    pub host: Option<String>,
}

/// Upgrade nodes by pushing a binary over SSH — the path for machines
/// that cannot replace their own binary (package installs, hardened units
/// without the drop-in, builds too old to be driven).
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SshUpgradeDraft {
    pub nodes: Vec<SshUpgradeNodeDraft>,
    pub auth: DeployAuthDraft,
    /// The version to install; the newest available when empty.
    pub version: Option<String>,
    /// A picker token for a meshnode binary to push (SEC-14), never a path.
    pub binary_path: Option<String>,
    pub parallel: u32,
    /// The SSH port for machines whose address does not name one. 22 is a
    /// default, not a law — a fleet on 2222 should say so once rather than
    /// on every row, and a row that names its own port still wins.
    pub port: u16,
    /// Push the binary even to a node already reporting this version.
    ///
    /// The version number is usually the right thing to skip on, and the
    /// wrong thing exactly when it is what is in dispute — a binary
    /// replaced by hand, an install that half-finished, a rebuild that
    /// kept its version. Then the operator wants these bytes on that
    /// machine, and "it says it already has them" is the claim they came
    /// here to overrule.
    pub force: bool,
}

/// The release channel against the fleet: what is running, what is
/// published in the mesh, what the channel offers.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseCheckView {
    /// This Director's own build.
    pub director_version: String,
    /// Newest version any live node reports.
    pub fleet_newest: Option<String>,
    /// Versions published into the mesh, newest first, with their
    /// architectures.
    pub published: Vec<PublishedReleaseView>,
    /// The newest tag on the release channel, when it answered.
    pub channel_latest: Option<String>,
    pub channel_error: Option<String>,
    /// A download could be verified: some key is trusted, or a real key
    /// is embedded.
    pub can_verify: bool,
    pub verify_note: Option<String>,
    /// Architectures live nodes report, so a fetch knows what to get.
    pub fleet_arches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PublishedReleaseView {
    pub version: String,
    pub arches: Vec<String>,
}

/// Fetching a release from the channel into the mesh, step by step.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ReleaseFetchEvent {
    Progress {
        arch: String,
        /// `downloading` | `verifying` | `publishing` | `published` | `skipped`.
        stage: String,
        detail: String,
    },
    Done {
        version: String,
        arches: Vec<String>,
        note: String,
    },
    Failed {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// M31 — retention and history export (A1's window, B2's sinks)
// ---------------------------------------------------------------------------
//
// Two cards, one paragraph: *how long do we keep it, and who else gets a
// copy*. They sit together in Settings → Data & safety because that is the
// truth about them (§18 decision 01) — and because a History-export card on
// its own would be a feature looking for a reason, while the pair reads as
// one decision an operator is already making.

/// One retention window, as the card renders it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RetentionRowView {
    pub table: String,
    /// `90d`, `1y`, `forever` — as an operator types and reads it.
    pub window: String,
    pub floor: Option<String>,
    /// What this node actually applies. May be *shorter* than the mesh
    /// window, never longer, and the card says so when they differ.
    pub effective_here: String,
    /// The one row that bounds the replicated op-log. Exactly one is true,
    /// and the screen marks it — otherwise five windows look equally
    /// consequential and only one of them is.
    pub bounds_the_op_log: bool,
}

/// The retention policy plus the projection that makes it a decision.
///
/// **The projection is not decoration.** A knob whose effect nobody can
/// predict is a knob nobody turns; *"settles at ~1.6 GB"* is what turns a
/// number into a choice, and it is shown before anything is signed.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RetentionView {
    pub windows: Vec<RetentionRowView>,
    /// Who signed it, or null for the shipped defaults. Two different
    /// sentences, and only one of them means somebody decided.
    pub signed_by: Option<String>,
    #[ts(type = "number")]
    pub signed_at_micros: u64,
    #[ts(type = "number")]
    pub oplog_bytes: u64,
    /// Growth the window *can* retire, and growth it cannot. Shown apart
    /// because conflating them would make retention look more powerful
    /// than it is — the audit log grows whatever an operator types.
    #[ts(type = "number")]
    pub retirable_bytes_per_day: u64,
    #[ts(type = "number")]
    pub permanent_bytes_per_day: u64,
    /// Where the log settles, or null when `runs` is forever — in which
    /// case it does not settle, and saying so beats printing a number.
    #[ts(type = "number | null")]
    pub settles_at_bytes: Option<u64>,
}

/// What this node is refusing, and what is churning (M31 A3).
///
/// **Empty on a healthy mesh, and the screen shows nothing when it is.** A
/// panel that reports a control nobody has tripped is furniture; this
/// appears the day something loops, which is the day somebody is looking.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BackpressureView {
    pub limit_per_minute: u32,
    pub entities: Vec<PressureRowView>,
    /// Fact collectors held back for changing again inside their publish
    /// window — the same question asked of the one entity kind that had an
    /// answer before A3 generalised it.
    pub flapping: Vec<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PressureRowView {
    /// The compaction key, as the log itself names the entity.
    pub entity: String,
    pub observed_per_minute: u32,
    #[ts(type = "number")]
    pub refused_total: u64,
    /// Non-zero while auto-paused — and it expires by itself, which is the
    /// difference between a brake and a tombstone.
    #[ts(type = "number")]
    pub paused_until_micros: u64,
}

impl From<mesh_proto::BackpressureView> for BackpressureView {
    fn from(v: mesh_proto::BackpressureView) -> Self {
        BackpressureView {
            limit_per_minute: v.limit_per_minute,
            entities: v
                .entities
                .into_iter()
                .map(|e| PressureRowView {
                    entity: e.entity,
                    observed_per_minute: e.observed_per_minute,
                    refused_total: e.refused_total,
                    paused_until_micros: e.paused_until_micros,
                })
                .collect(),
            flapping: v.flapping,
        }
    }
}

impl From<mesh_proto::RetentionView> for RetentionView {
    fn from(v: mesh_proto::RetentionView) -> Self {
        RetentionView {
            windows: v
                .windows
                .into_iter()
                .map(|r| RetentionRowView {
                    table: r.table,
                    window: r.window,
                    floor: r.floor,
                    effective_here: r.effective_here,
                    bounds_the_op_log: r.bounds_the_op_log,
                })
                .collect(),
            signed_by: v.signed_by,
            signed_at_micros: v.signed_at_micros,
            oplog_bytes: v.oplog_bytes,
            retirable_bytes_per_day: v.retirable_bytes_per_day,
            permanent_bytes_per_day: v.permanent_bytes_per_day,
            settles_at_bytes: v.settles_at_bytes,
        }
    }
}

/// What one destination gets you, rendered from the capability triple on
/// the enum so it cannot drift from what the code does (§14.4).
///
/// **Shown at selection, not in a tooltip afterwards.** B2 is where the
/// first customer chooses, and a picker that stays silent has already
/// misled them.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SinkKindView {
    pub kind: String,
    pub label: String,
    /// The one sentence about what you can do with it afterwards.
    pub capability: String,
    pub needs_url: bool,
    /// Whether raw job output is exported here unless told otherwise. True
    /// only for a log store, which is what raw output is *for*.
    pub payloads_by_default: bool,
    pub archives: bool,
    pub replay_payloads: bool,
}

/// One sink, as the History-export card shows it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SinkView {
    pub name: String,
    pub kind: String,
    pub target: String,
    pub database: String,
    pub tables: Vec<String>,
    pub enabled: bool,
    /// The node currently exporting this, and whether it is this one.
    /// Empty when no live node matches `run_on` — a real and common state,
    /// and one that must not read as healthy.
    pub elected: String,
    pub elected_here: bool,
    /// Tag selector narrowing which nodes may export, or empty for any.
    #[serde(default)]
    pub run_on: String,
    /// Whether the counters below are this sink's state or this node's copy
    /// of nothing.
    ///
    /// The cursor and spool are files on the **exporting** node, so
    /// everywhere else they read back as a clean zero row — which is how a
    /// sink failing every thirty seconds looks healthy from every machine
    /// but one. False means the card must say where the reading lives
    /// instead of drawing zeroes as though they were measurements.
    #[serde(default)]
    pub status_is_local: bool,
    #[ts(type = "number")]
    pub lag_secs: u64,
    #[ts(type = "number")]
    pub sent: u64,
    #[ts(type = "number")]
    pub spooled_bytes: u64,
    #[ts(type = "number")]
    pub dropped: u64,
    /// The destination's own words. `503` sends an operator to the right
    /// machine; "unhealthy" sends them to the wrong one.
    pub last_status: String,
    #[ts(type = "number")]
    pub last_at_micros: u64,
    pub paused_by_restore: bool,
    #[ts(type = "number")]
    pub payloads_sent: u64,
    #[ts(type = "number")]
    pub payloads_too_large: u64,
    #[ts(type = "number")]
    pub hold_for_export_days: u32,
    /// Records deleted before this sink took them, because its hold
    /// expired. **The number that must be said out loud** — every one is a
    /// row that will never reach the destination.
    #[ts(type = "number")]
    pub forced_gaps: u64,
    pub archives: bool,
    pub replay_payloads: bool,
    pub created_by: String,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    pub description: String,
}

impl From<mesh_proto::SinkRow> for SinkView {
    fn from(r: mesh_proto::SinkRow) -> Self {
        SinkView {
            name: r.name,
            kind: r.kind,
            target: r.target,
            database: r.database,
            tables: r.tables,
            enabled: r.enabled,
            run_on: r.run_on,
            elected: r.elected,
            elected_here: r.elected_here,
            status_is_local: r.status_is_local,
            lag_secs: r.lag_secs,
            sent: r.sent,
            spooled_bytes: r.spooled_bytes,
            dropped: r.dropped,
            last_status: r.last_status,
            last_at_micros: r.last_at_micros,
            paused_by_restore: r.paused_by_restore,
            payloads_sent: r.payloads_sent,
            payloads_too_large: r.payloads_too_large,
            hold_for_export_days: r.hold_for_export_days,
            forced_gaps: r.forced_gaps,
            archives: r.archives,
            replay_payloads: r.replay_payloads,
            created_by: r.created_by,
            created_at_micros: r.created_at_micros,
            description: r.description,
        }
    }
}

/// The form, as Director submits it.
///
/// **The credential is a reference, never a value.** Director sends
/// `{{secret:name}}` picked from the mesh's own vault; there is no
/// free-text password field, because a literal here would be signed into a
/// record replicated to every node and never compacted away (C6).
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SinkDraft {
    pub name: String,
    pub kind: String,
    pub url: String,
    /// Database (ClickHouse) or index prefix (OpenSearch), or the
    /// executable for a plugin sink.
    pub database: String,
    /// A secret *name* from the vault picker, or empty. Director turns it
    /// into `{{secret:name}}`; the value never enters this process.
    pub credential: String,
    /// A second secret name, for reading job output back (M31 B3). Empty
    /// means replay is refused — deliberately, rather than reusing the
    /// write token (§19 Q10).
    #[serde(default)]
    pub read_credential: String,
    pub tables: Vec<String>,
    /// Tag selector, or empty for every live executor.
    pub run_on: String,
    pub payloads: bool,
    #[ts(type = "number")]
    pub payload_max_mb: u64,
    pub hold_for_export_days: u32,
    pub enabled: bool,
    pub description: String,
}

/// One machine principal, as Director's Access view shows it (E1-P4).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccountView {
    pub user_id: String,
    pub short_id: String,
    pub name: String,
    pub description: String,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    #[ts(type = "number | null")]
    pub expires_at_micros: Option<u64>,
    /// Past its own clock: it holds no roles whatever it was granted, and
    /// re-granting it will not help.
    pub expired: bool,
    pub revoked: bool,
    pub roles: Vec<String>,
}

impl ServiceAccountView {
    pub fn of(r: &mesh_proto::ServiceAccountRow) -> ServiceAccountView {
        ServiceAccountView {
            user_id: r.user_id.to_display_string(),
            short_id: r.user_id.short(),
            name: r.name.clone(),
            description: r.description.clone(),
            created_at_micros: r.created_at_micros,
            expires_at_micros: r.expires_at_micros,
            expired: r.expired,
            revoked: r.revoked,
            roles: r.roles.clone(),
        }
    }
}

/// One configured identity provider (E1-P5).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct OidcProviderView {
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub groups_claim: String,
    /// `group=role` pairs, rendered.
    pub group_roles: Vec<String>,
    #[ts(type = "number")]
    pub configured_at_micros: u64,
}

impl OidcProviderView {
    pub fn of(r: &mesh_proto::OidcProviderRow) -> OidcProviderView {
        OidcProviderView {
            name: r.name.clone(),
            issuer: r.issuer.clone(),
            client_id: r.client_id.clone(),
            groups_claim: r.groups_claim.clone(),
            group_roles: r.group_roles.clone(),
            configured_at_micros: r.configured_at_micros,
        }
    }
}

/// What `service_account_create` hands back — **including the private
/// key**, for the one moment it exists outside the function that made it.
///
/// Director writes it to a file the operator chooses and keeps no copy;
/// meshware never had one. There is no recovery path and that is
/// deliberate: a credential the product could re-print is a credential the
/// product stores.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccountCreated {
    pub name: String,
    pub user_id: String,
    /// The raw 32-byte seed — the same bytes `identity/user.key` holds, so
    /// the receiving system needs no decoding step.
    #[ts(type = "number[]")]
    pub key: Vec<u8>,
    #[ts(type = "number | null")]
    pub expires_at_micros: Option<u64>,
}

#[cfg(test)]
mod stream_text {
    use super::StreamText;

    /// **M-21**: a character whose bytes straddle two frames must survive.
    #[test]
    fn a_character_split_across_frames_is_not_corrupted() {
        // "é" is 0xC3 0xA9 — split it down the middle.
        let mut t = StreamText::default();
        assert_eq!(t.push(None, 1, b"caf\xc3"), "caf");
        assert_eq!(t.push(None, 1, b"\xa9 au lait"), "é au lait");
    }

    /// A four-byte character split three ways.
    #[test]
    fn a_four_byte_character_survives_any_split() {
        let emoji = "🎉".as_bytes(); // F0 9F 8E 89
        for cut in 1..emoji.len() {
            let mut t = StreamText::default();
            let first = t.push(None, 1, &emoji[..cut]);
            let second = t.push(None, 1, &emoji[cut..]);
            assert_eq!(format!("{first}{second}"), "🎉", "split at {cut}");
        }
    }

    /// Two streams interleaved must not splice into each other.
    #[test]
    fn streams_do_not_borrow_each_others_trailing_bytes() {
        let mut t = StreamText::default();
        assert_eq!(t.push(None, 1, b"a\xc3"), "a");
        // stderr arrives in between; it must not consume stdout's carry.
        assert_eq!(t.push(None, 2, b"err"), "err");
        assert_eq!(t.push(None, 1, b"\xa9"), "é");
    }

    /// Fan-out targets are separate streams too.
    #[test]
    fn fanout_targets_do_not_share_a_carry() {
        let mut t = StreamText::default();
        assert_eq!(t.push(Some(0), 1, b"\xc3"), "");
        assert_eq!(t.push(Some(1), 1, b"x"), "x");
        assert_eq!(t.push(Some(0), 1, b"\xa9"), "é");
    }

    /// Genuinely invalid bytes are still reported, not held forever —
    /// otherwise a binary blob on stdout would stall the pane.
    #[test]
    fn invalid_bytes_are_emitted_rather_than_hoarded() {
        let mut t = StreamText::default();
        let out = t.push(None, 1, b"ok\xff\xfeok");
        assert!(out.starts_with("ok"), "{out:?}");
        assert!(out.ends_with("ok"), "{out:?}");
        assert!(out.contains('\u{fffd}'), "{out:?}");
    }
}

// ---------------------------------------------------------------------------
// Content store and disk (M31 A2)
// ---------------------------------------------------------------------------

/// One rung of the tidiness ladder, as the settings screen renders it.
///
/// The description ships from the engine rather than being written into the
/// Svelte, so the screen and `meshnode content --help` cannot come to
/// disagree about what `tidy` means.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ContentPolicyOption {
    pub id: String,
    pub label: String,
    pub description: String,
    /// `0` renders as "forever" for `keep` and "immediately" for
    /// `ephemeral` — the two ends of the ladder share a number and mean
    /// opposite things, so the screen must not print it raw.
    pub days: u32,
    pub immediate: bool,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ContentStatusView {
    pub policy: String,
    pub window_days: u32,
    pub capped_by_mesh: bool,
    #[ts(type = "number")]
    pub bundles: u64,
    #[ts(type = "number")]
    pub chunks: u64,
    #[ts(type = "number")]
    pub bytes: u64,
    #[ts(type = "number")]
    pub protected: u64,
    #[ts(type = "number")]
    pub reclaimable_bundles: u64,
    #[ts(type = "number")]
    pub reclaimable_bytes: u64,
    #[ts(type = "number")]
    pub max_bytes: u64,
    #[ts(type = "number")]
    pub disk_free_bytes: u64,
    #[ts(type = "number")]
    pub disk_total_bytes: u64,
    #[ts(type = "number")]
    pub disk_floor_bytes: u64,
    #[ts(type = "number")]
    pub disk_warn_bytes: u64,
    pub disk_critical: bool,
    /// The ladder, so the screen never hard-codes the rungs.
    pub options: Vec<ContentPolicyOption>,
}

impl ContentStatusView {
    pub fn from_ipc(v: mesh_proto::ContentStatusView) -> Self {
        ContentStatusView {
            policy: v.policy,
            window_days: v.window_days,
            capped_by_mesh: v.capped_by_mesh,
            bundles: v.bundles,
            chunks: v.chunks,
            bytes: v.bytes,
            protected: v.protected,
            reclaimable_bundles: v.reclaimable_bundles,
            reclaimable_bytes: v.reclaimable_bytes,
            max_bytes: v.max_bytes,
            disk_free_bytes: v.disk_free_bytes,
            disk_total_bytes: v.disk_total_bytes,
            disk_floor_bytes: v.disk_floor_bytes,
            disk_warn_bytes: v.disk_warn_bytes,
            disk_critical: v.disk_critical,
            options: mesh_proto::ContentPolicy::ALL
                .into_iter()
                .map(|p| ContentPolicyOption {
                    id: p.as_str().to_string(),
                    label: {
                        let mut c = p.as_str().chars();
                        match c.next() {
                            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                            None => String::new(),
                        }
                    },
                    description: p.describe().to_string(),
                    days: p.days(),
                    immediate: p.reclaims_immediately(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ContentPruneView {
    pub dry_run: bool,
    #[ts(type = "number")]
    pub bundles_removed: u64,
    #[ts(type = "number")]
    pub chunks_removed: u64,
    #[ts(type = "number")]
    pub bytes_reclaimed: u64,
    #[ts(type = "number")]
    pub bytes_remaining: u64,
    #[ts(type = "number")]
    pub protected: u64,
    #[ts(type = "number")]
    pub orphan_chunks: u64,
    #[ts(type = "number")]
    pub orphan_bytes: u64,
}

impl From<mesh_proto::ContentPruneView> for ContentPruneView {
    fn from(v: mesh_proto::ContentPruneView) -> Self {
        ContentPruneView {
            dry_run: v.dry_run,
            bundles_removed: v.bundles_removed,
            chunks_removed: v.chunks_removed,
            bytes_reclaimed: v.bytes_reclaimed,
            bytes_remaining: v.bytes_remaining,
            protected: v.protected,
            orphan_chunks: v.orphan_chunks,
            orphan_bytes: v.orphan_bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DiskView {
    pub node_id: String,
    pub name: String,
    pub online: bool,
    #[ts(type = "number")]
    pub free_bytes: u64,
    #[ts(type = "number")]
    pub total_bytes: u64,
    /// ok | warn | low | unknown
    pub state: String,
}

impl From<mesh_proto::DiskRow> for DiskView {
    fn from(r: mesh_proto::DiskRow) -> Self {
        DiskView {
            node_id: r.node_id.short(),
            name: r.name,
            online: r.online,
            free_bytes: r.free_bytes,
            total_bytes: r.total_bytes,
            state: r.state,
        }
    }
}

// -- schedules (N1 P5) ------------------------------------------------------

/// One schedule, as the Schedules list and detail show it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServiceScheduleView {
    pub name: String,
    /// `backup@4`, as written.
    pub service: String,
    pub targets: String,
    /// The trigger in the words the file used: `every 10m`, `03:00 mon-fri`.
    pub when: String,
    /// `active`, `paused`, or `orphaned` — the service it names is gone.
    pub status: String,
    #[ts(type = "number | null")]
    pub next_micros: Option<u64>,
    #[ts(type = "number | null")]
    pub last_micros: Option<u64>,
    /// The node currently elected to fire it, if any is live.
    pub firer: Option<String>,
    pub created_by: String,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    pub source: Option<String>,
}

impl From<mesh_proto::ServiceScheduleRow> for ServiceScheduleView {
    fn from(r: mesh_proto::ServiceScheduleRow) -> Self {
        ServiceScheduleView {
            name: r.name,
            service: r.service,
            targets: r.targets,
            when: r.when,
            status: r.status,
            next_micros: r.next_micros,
            last_micros: r.last_micros,
            firer: r.firer,
            created_by: r.created_by_username,
            created_at_micros: r.created_at_micros,
            source: r.source,
        }
    }
}

/// The answer to a dry-run schedule put.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServiceScheduleCheckView {
    pub name: String,
    pub service: String,
    pub targets: String,
    pub when: String,
    pub matching_nodes: u32,
    #[ts(type = "number | null")]
    pub next_micros: Option<u64>,
    pub issues: Vec<String>,
}

impl From<mesh_proto::ServiceScheduleCheck> for ServiceScheduleCheckView {
    fn from(v: mesh_proto::ServiceScheduleCheck) -> Self {
        ServiceScheduleCheckView {
            name: v.name,
            service: v.service,
            targets: v.targets,
            when: v.when,
            matching_nodes: v.matching_nodes,
            next_micros: v.next_micros,
            issues: v.issues,
        }
    }
}

/// The answer to a dry-run deployment put: what the record parsed to, what
/// it would do, and what is wrong with it.
///
/// Director had no deployment editor at all until N1 — the screen could
/// roll, pause and delete a deployment but not *make* one, and its own
/// empty state sent the reader to the terminal. D-E3 says a capability
/// landing in one front end is a capability half the product does not
/// have, and this is what that half looked like.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentCheckView {
    pub name: String,
    pub service: String,
    pub count: String,
    pub targets: String,
    /// Nodes the selector matches right now. A `count` bigger than this, or
    /// a selector matching nothing, is worth seeing before publishing.
    pub matching_nodes: u32,
    pub health: Option<String>,
    pub issues: Vec<String>,
    /// Why nothing could take it, when nothing can (N1 P2) — answered
    /// before publishing rather than after.
    pub placement: Option<PlacementRefusalView>,
    /// What happens when a node running an instance goes away, for the
    /// policy this file produces — defaults included (Phase A, U6).
    pub availability: String,
    /// What publishing changes, field by field (Phase B, E5, U3).
    pub changes: Vec<PlanChangeView>,
    /// Publishing starts a new generation and replaces every instance.
    pub rolls: bool,
    /// Nothing by this name is published yet.
    pub first_publish: bool,
}

/// What one running instance is using right now (Phase B, E3).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct InstanceStatView {
    pub job_id: String,
    /// Percent of one core: two busy cores read 200.
    pub cpu_pct: Option<f32>,
    #[ts(type = "number | null")]
    pub mem_bytes: Option<u64>,
    #[ts(type = "number | null")]
    pub mem_limit_bytes: Option<u64>,
    /// `container` | `cgroup` | `process` — or empty when unmeasured.
    pub source: String,
}

impl From<mesh_proto::InstanceStat> for InstanceStatView {
    fn from(s: mesh_proto::InstanceStat) -> Self {
        InstanceStatView {
            job_id: s.job.to_display_string(),
            cpu_pct: s.cpu_pct,
            mem_bytes: s.mem_bytes,
            mem_limit_bytes: s.mem_limit_bytes,
            source: s.source,
        }
    }
}

/// One incarnation of an instance slot (Phase B, U1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct IncarnationView {
    pub job_id: String,
    pub generation: u32,
    pub incarnation: u32,
    pub node: String,
    pub state: String,
    #[ts(type = "number")]
    pub created_at_micros: u64,
    #[ts(type = "number | null")]
    pub started_at_micros: Option<u64>,
    #[ts(type = "number | null")]
    pub finished_at_micros: Option<u64>,
    pub exit_code: Option<i32>,
}

/// One instance slot up close (Phase B, E3 and U1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentInstanceDetailView {
    pub deployment: String,
    pub slot: String,
    pub generation: u32,
    pub runtime: String,
    pub current: Option<DeploymentInstanceView>,
    /// Oldest first: the slot's timeline.
    pub incarnations: Vec<IncarnationView>,
    pub stats: Option<InstanceStatView>,
}

impl From<mesh_proto::DeploymentInstanceDetail> for DeploymentInstanceDetailView {
    fn from(d: mesh_proto::DeploymentInstanceDetail) -> Self {
        DeploymentInstanceDetailView {
            deployment: d.deployment,
            slot: d.slot,
            generation: d.generation,
            runtime: d.runtime,
            current: d.current.map(Into::into),
            incarnations: d
                .incarnations
                .into_iter()
                .map(|i| IncarnationView {
                    job_id: i.job_id.to_display_string(),
                    generation: i.generation,
                    incarnation: i.incarnation,
                    node: i.node,
                    state: i.state,
                    created_at_micros: i.created_at_micros,
                    started_at_micros: i.started_at_micros,
                    finished_at_micros: i.finished_at_micros,
                    exit_code: i.exit_code,
                })
                .collect(),
            stats: d.stats.map(Into::into),
        }
    }
}

/// One version a deployment keeps (Phase B, E1 and U2).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentVersionView {
    pub generation: u32,
    /// `collector@7`.
    pub service: String,
    /// A number, or `system`.
    pub count: String,
    pub targets: String,
    pub description: String,
    pub published_by: String,
    #[ts(type = "number")]
    pub published_at_micros: u64,
    pub tags: Vec<String>,
    /// What runs now.
    pub current: bool,
    /// Older than the ten kept; listed because a tag names it.
    pub kept_by_tag: bool,
    /// Against the next older version; empty for the oldest.
    pub changes: Vec<PlanChangeView>,
    pub source: String,
}

/// A name on one version (Phase B, E1).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentTagView {
    pub tag: String,
    pub generation: u32,
    pub description: String,
    pub set_by: String,
    #[ts(type = "number")]
    pub set_at_micros: u64,
}

/// `deployment history` (Phase B, E1 and U2).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentHistoryView {
    pub deployment: String,
    /// Newest first.
    pub versions: Vec<DeploymentVersionView>,
    pub tags: Vec<DeploymentTagView>,
}

impl From<mesh_proto::DeploymentHistoryView> for DeploymentHistoryView {
    fn from(h: mesh_proto::DeploymentHistoryView) -> Self {
        DeploymentHistoryView {
            deployment: h.deployment,
            versions: h
                .versions
                .into_iter()
                .map(|v| {
                    let dep = &v.version.deployment;
                    DeploymentVersionView {
                        generation: v.generation,
                        service: dep.service_ref(),
                        count: match dep.count {
                            mesh_proto::Count::System => "system".into(),
                            mesh_proto::Count::Fixed(n) => n.to_string(),
                        },
                        targets: dep.targets.clone(),
                        description: dep.description.clone(),
                        published_by: v.published_by_username,
                        published_at_micros: v.published_at_micros,
                        tags: v.tags,
                        current: v.current,
                        kept_by_tag: v.kept_by_tag,
                        changes: v
                            .changes
                            .into_iter()
                            .map(|c| PlanChangeView {
                                field: c.field,
                                from: c.from,
                                to: c.to,
                                effect: c.effect,
                            })
                            .collect(),
                        source: v.source,
                    }
                })
                .collect(),
            tags: h
                .tags
                .into_iter()
                .map(|t| DeploymentTagView {
                    tag: t.tag,
                    generation: t.generation,
                    description: t.description,
                    set_by: t.set_by_username,
                    set_at_micros: t.set_at_micros,
                })
                .collect(),
        }
    }
}

/// One changed field in a plan (Phase B, E5).
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PlanChangeView {
    /// `health.path`, `network.port.http.to`.
    pub field: String,
    /// As published; empty when new.
    pub from: String,
    /// As in the file; empty when removed.
    pub to: String,
    /// `rolls` | `rollout knob` | `record only`.
    pub effect: String,
}

impl From<mesh_proto::DeploymentCheckView> for DeploymentCheckView {
    fn from(v: mesh_proto::DeploymentCheckView) -> Self {
        DeploymentCheckView {
            name: v.name,
            service: v.service,
            count: v.count,
            targets: v.targets,
            matching_nodes: v.matching_nodes,
            health: v.health,
            issues: v.issues,
            placement: v.placement.map(Into::into),
            availability: v.availability,
            changes: v
                .changes
                .into_iter()
                .map(|c| PlanChangeView {
                    field: c.field,
                    from: c.from,
                    to: c.to,
                    effect: c.effect,
                })
                .collect(),
            rolls: v.rolls,
            first_publish: v.first_publish,
        }
    }
}

// -- ports (N2 P1) ----------------------------------------------------------

/// One node's port ledger, as the Ports panel shows it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PortLedgerViewT {
    pub node: String,
    /// `20000-32000`, or `null` when this node advertises no range — which
    /// means it takes no work that wants a port. The UI must say which:
    /// "0 free" and "advertises none" mean opposite things.
    pub range: Option<String>,
    pub range_min: Option<u16>,
    pub range_max: Option<u16>,
    pub reserved: Vec<u16>,
    pub held: Vec<PortHeldViewT>,
    pub free: u32,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct PortHeldViewT {
    pub port: u16,
    pub label: String,
    pub holder: String,
    /// `job` or `gang`.
    pub kind: String,
}

impl From<mesh_proto::PortLedgerView> for PortLedgerViewT {
    fn from(v: mesh_proto::PortLedgerView) -> Self {
        PortLedgerViewT {
            node: v.node,
            range: v.range.as_ref().map(|r| r.as_tag_value()),
            range_min: v.range.as_ref().map(|r| r.min),
            range_max: v.range.as_ref().map(|r| r.max),
            reserved: v.reserved,
            held: v
                .held
                .into_iter()
                .map(|h| PortHeldViewT {
                    port: h.port,
                    label: h.label,
                    holder: h.holder,
                    kind: h.kind,
                })
                .collect(),
            free: v.free,
        }
    }
}

// -- runtimes (T4) ----------------------------------------------------------

/// One runtime's answer on the node Director is attached to.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeReportT {
    /// `exec` | `bash` | `python` | `ansible` | `container`.
    pub runtime: String,
    /// `ready` | `absent` | `broken`.
    ///
    /// Three states, not two, and the UI must keep them apart: *installed
    /// but not working* has a different fix from *not installed*, and
    /// collapsing them sends somebody to install what they already have.
    pub detection: String,
    /// `podman 5.8.4 (rootless)`. Empty when nothing was found.
    pub detail: String,
    /// What to do about it. Empty when ready.
    pub remedy: String,
}

/// What one node in the mesh announces it can run.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct NodeRuntimesT {
    pub node: String,
    /// `null` when the node announces nothing — a build from before this
    /// existed. The UI must render that as *unknown*, never as *none*:
    /// placement treats it as able to run anything, and a panel that said
    /// "none" would contradict where the jobs actually go.
    pub runtimes: Option<Vec<String>>,
}

/// The Runtimes panel's whole payload.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RuntimesViewT {
    pub node: String,
    pub local: Vec<RuntimeReportT>,
    pub mesh: Vec<NodeRuntimesT>,
    /// Nodes announcing nothing, by name — surfaced rather than inferred,
    /// so one place decides what "unknown" means.
    pub unknown: Vec<String>,
}

impl From<mesh_proto::runtimes::RuntimesView> for RuntimesViewT {
    fn from(v: mesh_proto::runtimes::RuntimesView) -> Self {
        let unknown = v.unknown_nodes().iter().map(|s| (*s).to_string()).collect();
        RuntimesViewT {
            node: v.node,
            local: v
                .local
                .into_iter()
                .map(|r| RuntimeReportT {
                    runtime: r.runtime.as_str().to_string(),
                    detection: r.detection.as_str().to_string(),
                    detail: r.detail,
                    remedy: r.remedy,
                })
                .collect(),
            mesh: v
                .mesh
                .into_iter()
                .map(|n| NodeRuntimesT {
                    node: n.node,
                    runtimes: n
                        .runtimes
                        .map(|rs| rs.iter().map(|r| r.as_str().to_string()).collect()),
                })
                .collect(),
            unknown,
        }
    }
}

// -- the service registry (N3) ----------------------------------------------

/// One registered service name, and what is behind it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RegistryRowView {
    pub service: String,
    /// Instances discovery will hand out.
    pub healthy: u32,
    /// Instances registered at all. The gap between these two is the
    /// column a reader is actually scanning.
    pub total: u32,
    pub nodes: u32,
    pub tags: Vec<String>,
    /// `webapp.mesh` — the name to point something at.
    pub fqdn: String,
}

/// One instance of one service.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RegistryInstanceView {
    pub instance: String,
    pub node: String,
    pub address: String,
    pub port: u16,
    pub label: String,
    /// `unknown` | `starting` | `passing` | `critical`.
    pub health: String,
    #[ts(type = "number")]
    pub since_micros: u64,
    pub tags: Vec<String>,
    pub deployment: String,
    /// Would discovery hand this one out? An instance that is registered
    /// and *not* resolvable is the interesting row.
    pub resolvable: bool,
}

/// What the DNS responder on this node is doing.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DnsStatusViewT {
    pub enabled: bool,
    pub bound: Option<String>,
    pub port: u16,
    /// Bound below 1024, which needs a capability this product does not
    /// take by default.
    pub privileged: bool,
    pub suffix: String,
    pub names: u32,
    /// Why it is not answering, when it is not.
    pub detail: String,
    /// This machine's resolver sends `.mesh` to the responder.
    pub resolver_routed: bool,
}

impl From<mesh_proto::RegistryRow> for RegistryRowView {
    fn from(r: mesh_proto::RegistryRow) -> Self {
        RegistryRowView {
            fqdn: format!("{}.{}", r.service, mesh_proto::DNS_SUFFIX),
            service: r.service,
            healthy: r.healthy,
            total: r.total,
            nodes: r.nodes,
            tags: r.tags,
        }
    }
}

impl From<mesh_proto::RegistryInstanceRow> for RegistryInstanceView {
    fn from(r: mesh_proto::RegistryInstanceRow) -> Self {
        RegistryInstanceView {
            instance: r.instance.to_display_string(),
            node: r.node,
            address: r.address,
            port: r.port,
            label: r.label,
            health: r.health,
            since_micros: r.since_micros,
            tags: r.tags,
            deployment: r.deployment,
            resolvable: r.resolvable,
        }
    }
}

impl From<mesh_proto::DnsStatusView> for DnsStatusViewT {
    fn from(v: mesh_proto::DnsStatusView) -> Self {
        DnsStatusViewT {
            enabled: v.enabled,
            bound: v.bound,
            port: v.port,
            privileged: v.privileged,
            suffix: v.suffix,
            names: v.names,
            detail: v.detail,
            resolver_routed: v.resolver_routed,
        }
    }
}

// -- the mesh-wide shed (N6 §12) --------------------------------------------

/// What is shed right now, and how much of the mesh will honour it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ShedView {
    pub submissions: bool,
    pub reconcile: bool,
    pub reason: String,
    pub by_name: Option<String>,
    #[ts(type = "number")]
    pub at_micros: u64,
    #[ts(type = "number")]
    pub members: u32,
    #[ts(type = "number")]
    pub honouring: u32,
    /// Nodes that will keep taking work regardless, by name.
    pub behind: Vec<String>,
    /// The sentence to print when `behind` is not empty. Built on the Rust
    /// side so the CLI and the panel cannot word this differently — it is
    /// the one warning in the product about a gate that fails *open*.
    pub warning: Option<String>,
}

impl From<mesh_proto::ShedView> for ShedView {
    fn from(v: mesh_proto::ShedView) -> Self {
        ShedView {
            submissions: v.submissions,
            reconcile: v.reconcile,
            reason: v.reason,
            by_name: v.by_name,
            at_micros: v.at_micros,
            members: v.readiness.members,
            honouring: v.readiness.honouring,
            behind: v.readiness.behind.clone(),
            warning: v.readiness.warning(),
        }
    }
}

// -- volumes (N5) -----------------------------------------------------------

/// One volume this node lends, and who is holding it.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct VolumeRowView {
    pub name: String,
    pub node: String,
    pub path: String,
    pub read_only: bool,
    /// `static`, or the plugin that created it.
    pub source: String,
    pub holders: Vec<String>,
    /// A writer holds it, so nothing else may write.
    pub write_held: bool,
    /// Could a writer take it right now? A read-only volume is never
    /// writable and a volume held by a *reader* is not either — "free" and
    /// "writable" are different questions and the UI must not blur them.
    pub writable_now: bool,
}

impl From<mesh_proto::VolumeRow> for VolumeRowView {
    fn from(r: mesh_proto::VolumeRow) -> Self {
        VolumeRowView {
            name: r.name,
            node: r.node,
            path: r.path,
            read_only: r.read_only,
            source: r.source,
            holders: r.holders,
            write_held: r.write_held,
            writable_now: r.writable_now,
        }
    }
}
