//! Gossip: heartbeat + anti-entropy state sync + failure detection
//! (MASTERPLAN §4.3, §4.4).
//!
//! Every second, each node syncs with one other member (rotating): version
//! vectors are exchanged, missing ops flow both ways, and liveness
//! observations piggyback. A member that stops answering becomes `suspect`,
//! then `dead` — liveness is ephemeral gossip, never persisted in the op-log.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::state::StateEngine;
use crate::transport::{ConnKind, Transport};
use mesh_proto::{
    COMPUTE_MINOR, Hlc, Liveness, LivenessEntry, MemberInfo, NodeId, OPLOG_MINOR, OpEnvelope,
    PressureSnapshot, Request, Response, SyncRequest, SyncRequestV2, VersionVector,
};

pub const SYNC_INTERVAL: Duration = Duration::from_millis(1000);
const FAILS_SUSPECT: u32 = 3;
const FAILS_DEAD: u32 = 8;

/// How long a sync may take to reach its peer and for the peer's first
/// answer to *start* arriving (ENG-7): the dial and the first bytes,
/// together. Syncs had no deadline, so a peer whose packets were being
/// dropped cost a whole dial timeout per probe, and one that accepted a
/// connection and then said nothing held the gossip loop for as long as it
/// liked. How long the rest of the answer takes is [`REQUEST_DEADLINE`]'s
/// business: a large batch on a slow link is not a dead peer.
pub const FIRST_ANSWER_DEADLINE: Duration = Duration::from_secs(2);
/// The same, for a peer that has already failed recently — the short leash
/// that turns "probably gone" into "gone" in seconds rather than minutes.
const PROBE_DEADLINE: Duration = Duration::from_millis(1500);
/// Each later request of a sync, once the peer has answered once. Longer,
/// because a batch can be a megabyte and a link can be slow, and a peer
/// that has just answered is not the one being tested.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
/// A peer that has failed [`FAILS_SUSPECT`] probes and has been failing for
/// this long is dead, however few probes that took (ENG-7). The count alone
/// paced death by how fast failures happened to accrue — a dial timeout
/// here, a fast failure there — which put it anywhere from ten to twenty
/// seconds after the last packet.
const DEAD_AFTER: Duration = Duration::from_secs(5);
/// Peers with pending failure evidence re-probed per tick, concurrently and
/// beside the gossip partner — dead detection converges in seconds for the
/// discovering node regardless of mesh size, then spreads through the
/// liveness snapshots (MASTERPLAN §4.3). Sixteen rather than the three a
/// one-at-a-time loop could afford (ENG-7): a rack losing power is several
/// peers failing at once.
const VERDICT_PROBES: usize = 16;
/// The most liveness entries a snapshot carries, or a merge reads (SEC-2).
///
/// The table is bounded by membership now, so this is a backstop rather
/// than the bound: at ~50 bytes an entry it keeps a snapshot far inside the
/// 4 MiB frame cap, which is the limit a flood of keys used to push every
/// sync past. Far above any mesh this codebase has been certified at.
pub const MAX_LIVENESS_ENTRIES: usize = 20_000;

/// How long a pressure reading stays usable (M9 §5.2). Pressure refreshes
/// once per gossip tick per partner, so in a large mesh most readings are
/// older than this — deliberately: a stale reading is reported as absent,
/// and absent pressure is neutral in placement, never disqualifying.
const PRESSURE_TTL: Duration = Duration::from_secs(30);

/// Readings passed on in one telemetry answer.
///
/// Spreading readings costs bandwidth that pulling them did not, and the
/// cost is per tick per node: uncapped, a 1,000-node mesh would put ~150 KB
/// on every answer and spend more on describing itself than on doing
/// anything. Capped, an answer stays a few KB and the epidemic still
/// converges — that is the property epidemics have. The **freshest** go
/// first because a reading near the end of its TTL is about to be replaced
/// by its owner being asked directly anyway, so spending the budget on it
/// buys the least.
const TELEMETRY_FANOUT: usize = 64;

/// Gossip interval auto-tuning (MASTERPLAN §4.3, D12): small meshes sync
/// every second; large meshes back off gently so a 1,000-node mesh gossips
/// at a sustainable aggregate rate while still converging in seconds
/// (epidemic push-pull needs ~log₂(n) rounds).
pub fn sync_interval_for(members: usize) -> Duration {
    let base = SYNC_INTERVAL.as_millis() as u64;
    let scaled = match members {
        0..=128 => base,
        129..=512 => base * 3 / 2,
        _ => base * 5 / 2,
    };
    Duration::from_millis(scaled)
}

/// Why a sync failed: the peer, or this node (ENG-4).
///
/// Only the first is evidence about the peer. A node whose own disk was
/// full counted its failures to write what peers sent as *their* failures,
/// declared the whole mesh suspect, and set off job adoption everywhere.
#[derive(Debug)]
enum SyncError {
    Peer(crate::CoreError),
    Local(crate::CoreError),
    /// The peer answered, and refused what it was sent — a node that
    /// cannot write, most likely. An answer is proof of life.
    Refused(String),
}

impl From<crate::CoreError> for SyncError {
    fn from(e: crate::CoreError) -> Self {
        SyncError::Peer(e)
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Peer(e) => write!(f, "{e}"),
            SyncError::Local(e) => write!(f, "on this node: {e}"),
            SyncError::Refused(message) => write!(f, "refused by the peer: {message}"),
        }
    }
}

/// One sync response, normalized across the V1 and V2 wire shapes so the
/// caller never branches on which leg answered.
struct SyncBatch {
    ops: Vec<OpEnvelope>,
    more: bool,
    vv: VersionVector,
    liveness: Vec<LivenessEntry>,
}

#[derive(Debug, Clone, Copy)]
struct LiveInfo {
    liveness: Liveness,
    observed_hlc: Hlc,
    fails: u32,
    /// When this peer **stopped** being `Alive` (N1 P4).
    ///
    /// Distinct from `observed_hlc`, which is stamped on every observation
    /// including the failed probes that repeat once a second while a node
    /// is down — so it answers "when did we last look", not "how long has
    /// it been gone". A deployment's `lost_after` needs the second
    /// question, and only the second question: the whole point of the
    /// field is to distinguish a node that blinked from one that left.
    ///
    /// Set on the transition away from `Alive`, cleared on the transition
    /// back, and never refreshed in between.
    not_alive_since: Option<Hlc>,
    /// When the current run of failed probes began, on this machine's
    /// monotonic clock; `None` while the peer answers (ENG-7).
    failing_since: Option<std::time::Instant>,
    /// The latest evidence of life: a sync that worked, or somebody else's
    /// claim of one. What this node says about a peer it still counts as
    /// alive (ENG-7).
    ///
    /// `observed_hlc` is not that: a failed probe stamps it too, so a node
    /// two failures into a streak used to tell everyone "alive, as of just
    /// now" — and every peer that heard it reset its own count, and nobody
    /// reached three.
    alive_hlc: Hlc,
}

impl LiveInfo {
    /// Fold a new verdict in, keeping `not_alive_since` honest across the
    /// transition. The one place that edge is handled, so no caller has to
    /// remember it.
    fn set_liveness(&mut self, to: Liveness, at: Hlc) {
        match (self.liveness, to) {
            (Liveness::Alive, Liveness::Alive) => {}
            (Liveness::Alive, _) => self.not_alive_since = Some(at),
            (_, Liveness::Alive) => self.not_alive_since = None,
            _ => {}
        }
        self.liveness = to;
    }
}

/// A peer's last pressure reading, with the local instant it arrived —
/// monotonic, so a wall-clock jump cannot make a reading look fresh.
#[derive(Debug, Clone)]
struct PressureEntry {
    snapshot: PressureSnapshot,
    /// The live telemetry half, when the peer answered the telemetry
    /// question (`TELEMETRY_MINOR`). `None` for an older peer, whose
    /// `snapshot` still arrives — first-hand only, as it always did.
    system: Option<mesh_proto::SystemSnapshot>,
    /// Per-device telemetry, when the peer answered the V2 question
    /// (C5 P5). Empty for a peer below `ACCEL_MINOR` and for one with no
    /// accelerators — the same value for two different reasons, which is
    /// fine because both mean "no device signal to place on" and the
    /// scheduler treats that as neutral either way.
    devices: Vec<mesh_proto::DevicePressure>,
    received: std::time::Instant,
}

/// What a pre-1.7 peer is being denied (F0 §3.3). Recorded when this node
/// serves it a downgraded sync batch, so `meshnode nodes` can mark it
/// `stale (pre-1.7)` and `doctor` can say how far behind it is and why.
///
/// Ephemeral, exactly like liveness and pressure: it describes the state
/// of a conversation, not a fact about the mesh, and it is rebuilt within
/// one gossip tick of a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyLag {
    /// The minor this peer negotiated.
    pub minor: u16,
    /// Authors whose sequence it can no longer follow.
    pub authors: usize,
    /// Ops in this node's log it can never be sent.
    pub ops: u64,
}

/// How far one peer trailed this node's op log at its last sync request.
///
/// Ephemeral in exactly the way [`LegacyLag`] and pressure are: it
/// describes the state of a conversation rather than a fact about the
/// mesh, it is never persisted and never gossiped transitively, and it is
/// rebuilt within one gossip tick of a restart. Recording it costs nothing
/// — every sync request already carries the requester's version vector.
#[derive(Debug, Clone, Copy)]
pub struct SyncLag {
    /// Ops in this node's log the peer had not received.
    pub behind: u64,
    /// Ops in this node's log at that moment.
    pub total: u64,
    /// Authors the peer was behind on.
    pub authors: u32,
    /// Local monotonic instant of the observation — monotonic so a
    /// wall-clock jump cannot make a stale reading look fresh, the same
    /// reason [`PressureEntry`] holds one.
    pub observed: std::time::Instant,
}

#[derive(Debug)]
pub struct Gossip {
    engine: Arc<StateEngine>,
    transport: Arc<Transport>,
    live: Mutex<HashMap<NodeId, LiveInfo>>,
    /// Live pressure, beside the liveness map and just as ephemeral
    /// (M9 §5.2): in memory, freshest-wins, never persisted, never gossiped
    /// transitively — each reading comes straight from the node it
    /// describes.
    pressure: Mutex<HashMap<NodeId, PressureEntry>>,
    /// Peers served a downgraded sync batch because they predate F0.
    legacy: Mutex<HashMap<NodeId, LegacyLag>>,
    /// How far behind each peer was when it last asked us for ops.
    sync: Mutex<HashMap<NodeId, SyncLag>>,
    /// Peers still to be visited in this gossip cycle, in a random order
    /// drawn when the cycle began (ENG-7).
    cycle: Mutex<Vec<NodeId>>,
    /// Notifies listeners (Director event bus later) that state changed.
    pub changed_tx: mpsc::Sender<()>,
}

impl Gossip {
    pub fn new(
        engine: Arc<StateEngine>,
        transport: Arc<Transport>,
    ) -> (Arc<Self>, mpsc::Receiver<()>) {
        let (changed_tx, changed_rx) = mpsc::channel(16);
        (
            Arc::new(Self {
                engine,
                transport,
                live: Mutex::new(HashMap::new()),
                pressure: Mutex::new(HashMap::new()),
                legacy: Mutex::new(HashMap::new()),
                sync: Mutex::new(HashMap::new()),
                cycle: Mutex::new(Vec::new()),
                changed_tx,
            }),
            changed_rx,
        )
    }

    pub fn liveness_of(&self, id: &NodeId) -> Liveness {
        if *id == self.engine.node_id() {
            return Liveness::Alive;
        }
        self.live
            .lock()
            .get(id)
            .map(|l| l.liveness)
            .unwrap_or(Liveness::Suspect)
    }

    /// How long a peer has been anything other than `Alive`, in wall
    /// micros, or `None` while it is up (N1 P4).
    ///
    /// A peer this node has never heard of reads as `Suspect` from
    /// [`Self::liveness_of`] but has no entry here, and so answers `None`:
    /// "we have never seen it" is not "it has been gone since a moment
    /// ago", and a `lost_after` timer that started the instant a member
    /// record arrived would declare instances lost on a node that was
    /// still booting.
    pub fn not_alive_since_micros(&self, id: &NodeId) -> Option<u64> {
        if *id == self.engine.node_id() {
            return None;
        }
        self.live
            .lock()
            .get(id)
            .and_then(|l| l.not_alive_since)
            .map(|h| h.wall_micros)
    }

    /// A peer's last pressure reading, or `None` when it is unknown or
    /// stale. Callers must treat `None` as neutral (M9 §5.2).
    pub fn pressure_of(&self, id: &NodeId) -> Option<PressureSnapshot> {
        let pressure = self.pressure.lock();
        let entry = pressure.get(id)?;
        (entry.received.elapsed() < PRESSURE_TTL).then(|| entry.snapshot.clone())
    }

    /// A peer's last per-device telemetry (C5 P5), or empty when it is
    /// unknown, stale, or the peer has no accelerators.
    pub fn device_pressure_of(&self, id: &NodeId) -> Vec<mesh_proto::DevicePressure> {
        let pressure = self.pressure.lock();
        pressure
            .get(id)
            .filter(|e| e.received.elapsed() < PRESSURE_TTL)
            .map(|e| e.devices.clone())
            .unwrap_or_default()
    }

    /// Record a reading straight from the node it describes. Freshest
    /// observation wins, by the reporter's own HLC — an out-of-order
    /// response can never move a peer's pressure backwards.
    pub fn record_pressure(&self, id: NodeId, snapshot: PressureSnapshot) {
        self.record_pressure_v2(id, snapshot, Vec::new());
    }

    /// [`Gossip::record_pressure`] with the device half attached (C5 P5).
    pub fn record_pressure_v2(
        &self,
        id: NodeId,
        snapshot: PressureSnapshot,
        devices: Vec<mesh_proto::DevicePressure>,
    ) {
        let mut pressure = self.pressure.lock();
        if let Some(existing) = pressure.get(&id)
            && existing.snapshot.hlc > snapshot.hlc
        {
            return;
        }
        pressure.insert(
            id,
            PressureEntry {
                snapshot,
                system: None,
                devices,
                received: std::time::Instant::now(),
            },
        );
    }

    /// Merge a batch of telemetry entries, freshest-wins by HLC
    /// (`TELEMETRY_MINOR`).
    ///
    /// The same merge rule `merge_remote` applies to liveness, for the same
    /// reason: readings arrive by more than one path once they spread, and
    /// two paths carrying the same node's reading must converge rather than
    /// alternate. Claims about *this* node are dropped — we measure
    /// ourselves, and hearsay about our own load is always staler than the
    /// `/proc` read we could do instead.
    ///
    /// Entries from a pre-1.35 peer arrive converted, their readings unnamed
    /// ([`mesh_proto::reading_for`] says what that costs).
    pub fn merge_telemetry(&self, entries: Vec<mesh_proto::TelemetryEntryV2>) {
        let me = self.engine.node_id();
        let now = std::time::Instant::now();
        let mut pressure = self.pressure.lock();
        for e in entries {
            if e.node_id == me {
                continue;
            }
            if let Some(existing) = pressure.get(&e.node_id)
                && existing.snapshot.hlc >= e.system.base.hlc
            {
                continue;
            }
            pressure.insert(
                e.node_id,
                PressureEntry {
                    snapshot: e.system.base.clone(),
                    system: Some(e.system),
                    devices: e.devices,
                    received: now,
                },
            );
        }
    }

    /// Everything this node holds that is still fresh, ready to pass on.
    ///
    /// Only entries that arrived *as telemetry* — a first-hand
    /// `Response::Pressure` from an older peer describes a node this one
    /// cannot vouch for in the richer shape, and inventing the missing
    /// fields to fill the struct would be reporting measurements nobody
    /// took.
    pub fn telemetry_heard(&self) -> Vec<mesh_proto::TelemetryEntryV2> {
        let pressure = self.pressure.lock();
        let mut fresh: Vec<(&NodeId, &PressureEntry)> = pressure
            .iter()
            .filter(|(_, e)| e.received.elapsed() < PRESSURE_TTL && e.system.is_some())
            .collect();
        fresh.sort_by_key(|(_, e)| e.received);
        fresh
            .into_iter()
            .rev()
            .take(TELEMETRY_FANOUT)
            .filter_map(|(id, e)| {
                Some(mesh_proto::TelemetryEntryV2 {
                    node_id: *id,
                    system: e.system.clone()?,
                    devices: e.devices.clone(),
                })
            })
            .collect()
    }

    /// The live picture placement scores on: the rich reading when the peer
    /// speaks `TELEMETRY_MINOR`, and the bare pressure lifted into the same
    /// shape when it does not.
    ///
    /// One accessor rather than two so the scheduler has a single input
    /// whose unknowns are explicit. An older peer arrives with `cpu_pct:
    /// None`, which the CPU term already treats as neutral — so it scores
    /// exactly as it did before this existed, which is the property that
    /// makes a mixed-version mesh safe to leave half-upgraded.
    pub fn live_of(&self, id: &NodeId) -> Option<mesh_proto::SystemSnapshot> {
        let pressure = self.pressure.lock();
        let entry = pressure.get(id)?;
        if entry.received.elapsed() >= PRESSURE_TTL {
            return None;
        }
        Some(match &entry.system {
            Some(system) => system.clone(),
            None => mesh_proto::SystemSnapshot::from_pressure(entry.snapshot.clone()),
        })
    }

    /// A peer's full telemetry, when it is fresh and the peer speaks the
    /// richer shape.
    pub fn system_of(&self, id: &NodeId) -> Option<mesh_proto::SystemSnapshot> {
        let pressure = self.pressure.lock();
        let entry = pressure.get(id)?;
        (entry.received.elapsed() < PRESSURE_TTL).then(|| entry.system.clone())?
    }

    /// Record that a pre-1.7 peer was served a downgraded batch, or clear
    /// the record when it can be served in full again — which is what
    /// happens the moment it upgrades, without anyone having to notice.
    pub fn note_legacy_lag(&self, peer: NodeId, lag: Option<LegacyLag>) {
        let mut legacy = self.legacy.lock();
        match lag {
            Some(lag) => {
                legacy.insert(peer, lag);
            }
            None => {
                legacy.remove(&peer);
            }
        }
    }

    /// What this peer cannot receive, if anything.
    pub fn legacy_lag(&self, peer: &NodeId) -> Option<LegacyLag> {
        self.legacy.lock().get(peer).copied()
    }

    /// Record how far a peer trailed us at the sync request it just made.
    ///
    /// Called on the *serving* side, which is the only side that knows: the
    /// requester's version vector is the request, and this node's own log
    /// is the thing it is being measured against.
    pub fn note_sync_lag(&self, peer: NodeId, behind: u64, total: u64, authors: u32) {
        self.sync.lock().insert(
            peer,
            SyncLag {
                behind,
                total,
                authors,
                observed: std::time::Instant::now(),
            },
        );
    }

    /// How far this peer trailed us, if it has asked since we started.
    pub fn sync_lag(&self, peer: &NodeId) -> Option<SyncLag> {
        self.sync.lock().get(peer).copied()
    }

    /// Drop pressure for anyone no longer a gossip peer — a member that
    /// left, was revoked, or lost its address. Keeps the map bounded by
    /// live membership rather than by history.
    fn retain_pressure(&self, peers: &[MemberInfo]) {
        // **One set, three `retain`s** (**M-3**). Each `retain` was a linear
        // scan of the peer list *per entry*, so this was three O(N²) passes
        // every gossip tick — three million comparisons at a thousand
        // members, to discard nothing on almost every tick.
        //
        // Built lazily: the common case is that all three maps are already
        // no larger than the membership, and then there is nothing to do.
        let needed = {
            let legacy = self.legacy.lock();
            let sync = self.sync.lock();
            let pressure = self.pressure.lock();
            !legacy.is_empty() || !sync.is_empty() || pressure.len() > peers.len()
        };
        if !needed {
            return;
        }
        let live: HashSet<NodeId> = peers.iter().map(|p| p.node_id).collect();
        {
            let mut legacy = self.legacy.lock();
            if !legacy.is_empty() {
                legacy.retain(|id, _| live.contains(id));
            }
        }
        {
            let mut sync = self.sync.lock();
            if !sync.is_empty() {
                sync.retain(|id, _| live.contains(id));
            }
        }
        let mut pressure = self.pressure.lock();
        if pressure.len() <= peers.len() {
            return;
        }
        pressure.retain(|id, _| live.contains(id));
    }

    /// Drop liveness for anything that is not an active member (SEC-2).
    ///
    /// The table used to grow with every key that ever opened a connection
    /// and never shrink, and every sync carries all of it. Keyed on
    /// membership rather than on the dialable peer list, so a member that
    /// has joined but not yet announced an address keeps its entry.
    fn retain_live(&self, members: &HashSet<NodeId>) {
        let changed = {
            let mut live = self.live.lock();
            let before = live.len();
            live.retain(|id, _| members.contains(id));
            live.len() != before
        };
        if changed {
            self.note_liveness_change();
        }
    }

    /// What this node believes about who is up, for a sync.
    ///
    /// Active members only, and at most [`MAX_LIVENESS_ENTRIES`] of them,
    /// freshest first (SEC-2). Before, this was the whole table, which any
    /// key could add to — so enough junk keys pushed every sync past the
    /// frame cap, and a mesh whose syncs all fail decides every node is
    /// dead.
    pub fn snapshot(&self) -> Vec<LivenessEntry> {
        let mut out: Vec<LivenessEntry> = self
            .live
            .lock()
            .iter()
            .map(|(id, l)| LivenessEntry {
                node_id: *id,
                liveness: l.liveness,
                // A claim of life is dated by the life, not by the latest
                // look — see `LiveInfo::alive_hlc`.
                observed_hlc: if l.liveness == Liveness::Alive {
                    l.alive_hlc
                } else {
                    l.observed_hlc
                },
            })
            .collect();
        // Membership is read after the table lock is released: nothing
        // here may hold `live` while waiting on the state lock.
        self.engine
            .read(|s| out.retain(|e| s.is_active_member(&e.node_id)));
        if out.len() >= MAX_LIVENESS_ENTRIES {
            out.sort_unstable_by_key(|e| std::cmp::Reverse(e.observed_hlc));
            out.truncate(MAX_LIVENESS_ENTRIES - 1);
        }
        out.push(LivenessEntry {
            node_id: self.engine.node_id(),
            liveness: Liveness::Alive,
            observed_hlc: self.engine.clock().now(),
        });
        out
    }

    /// Tell the change signal that this node's view of who is up moved
    /// (**H-8**).
    ///
    /// Liveness is ephemeral by design and authors no op, so the `Nodes`
    /// counter — a fold over `OpBody` — cannot see it. Every `NodeRow`
    /// carries a liveness, so a client that trusts the counter shows a
    /// membership-era snapshot of it for ever; see
    /// [`StateEngine::note_ephemeral_change`].
    ///
    /// Called only on an observed transition, never per tick: the three
    /// callers each compare against what [`Self::liveness_of`] would have
    /// answered a moment ago, so a steady mesh — every peer alive and
    /// agreeing — bumps nothing and stays as free to poll as it was.
    fn note_liveness_change(&self) {
        self.engine
            .note_ephemeral_change(mesh_proto::StateTopic::Nodes);
    }

    /// Merge remote liveness observations: freshest observation wins.
    /// Claims about ourselves are ignored — we know we're alive.
    ///
    /// Only about active members, and only the first
    /// [`MAX_LIVENESS_ENTRIES`] of them (SEC-2): a member's hearsay about a
    /// key this mesh has never admitted is not something to store, let
    /// alone to repeat to every other node.
    pub fn merge_remote(&self, entries: &[LivenessEntry]) {
        let self_id = self.engine.node_id();
        let entries: Vec<&LivenessEntry> = self.engine.read(|s| {
            entries
                .iter()
                .filter(|e| e.node_id != self_id && s.is_active_member(&e.node_id))
                .take(MAX_LIVENESS_ENTRIES)
                .collect()
        });
        let mut changed = false;
        {
            let mut live = self.live.lock();
            for e in entries {
                match live.get_mut(&e.node_id) {
                    Some(local) if local.observed_hlc >= e.observed_hlc => {}
                    Some(local) => {
                        // Never resurrect a locally-failing peer from hearsay
                        // alone unless the observation is genuinely newer.
                        changed |= local.liveness != e.liveness;
                        local.set_liveness(e.liveness, e.observed_hlc);
                        local.observed_hlc = e.observed_hlc;
                        if e.liveness == Liveness::Alive {
                            local.fails = 0;
                            local.failing_since = None;
                            local.alive_hlc = e.observed_hlc;
                        }
                    }
                    None => {
                        // A peer with no entry already reads as `Suspect`
                        // (`liveness_of`), so first hearing of one only
                        // changes an answer when it says something else.
                        changed |= e.liveness != Liveness::Suspect;
                        live.insert(
                            e.node_id,
                            LiveInfo {
                                liveness: e.liveness,
                                observed_hlc: e.observed_hlc,
                                fails: 0,
                                failing_since: None,
                                not_alive_since: (e.liveness != Liveness::Alive)
                                    .then_some(e.observed_hlc),
                                alive_hlc: if e.liveness == Liveness::Alive {
                                    e.observed_hlc
                                } else {
                                    Hlc::default()
                                },
                            },
                        );
                    }
                }
            }
        }
        if changed {
            self.note_liveness_change();
        }
    }

    /// Direct evidence of life (an inbound request or successful sync).
    pub fn mark_alive(&self, id: &NodeId) {
        if *id == self.engine.node_id() {
            return;
        }
        let hlc = self.engine.clock().now();
        let changed = {
            let mut live = self.live.lock();
            let before = live.get(id).map(|l| l.liveness);
            live.insert(
                *id,
                LiveInfo {
                    liveness: Liveness::Alive,
                    observed_hlc: hlc,
                    fails: 0,
                    not_alive_since: None,
                    failing_since: None,
                    alive_hlc: hlc,
                },
            );
            // Absent reads as `Suspect`, so first contact is a transition
            // too — and it is the one that matters most, because it is the
            // one a node makes about every peer just after it boots.
            before != Some(Liveness::Alive)
        };
        if changed {
            self.note_liveness_change();
        }
    }

    /// Record one failed probe of `id`, as a sync that failed would (tests).
    #[doc(hidden)]
    pub fn mark_failure_for_test(&self, id: &NodeId) {
        self.mark_failure(id);
    }

    fn mark_failure(&self, id: &NodeId) {
        let hlc = self.engine.clock().now();
        let changed = {
            let mut live = self.live.lock();
            // What `liveness_of` answers *now* — an absent peer reads as
            // `Suspect`, so that, and not the `or_insert` default below, is
            // what a transition has to be measured against.
            let before = live.get(id).map_or(Liveness::Suspect, |l| l.liveness);
            let entry = live.entry(*id).or_insert(LiveInfo {
                liveness: Liveness::Alive,
                observed_hlc: hlc,
                fails: 0,
                not_alive_since: None,
                failing_since: None,
                // Never seen answering: no claim of life to repeat.
                alive_hlc: Hlc::default(),
            });
            let since = *entry
                .failing_since
                .get_or_insert_with(std::time::Instant::now);
            entry.fails += 1;
            entry.observed_hlc = hlc;
            let counted = if entry.fails >= FAILS_DEAD
                || (entry.fails >= FAILS_SUSPECT && since.elapsed() >= DEAD_AFTER)
            {
                Liveness::Dead
            } else if entry.fails >= FAILS_SUSPECT {
                Liveness::Suspect
            } else {
                entry.liveness
            };
            // A failed probe is evidence *against* a peer: it may escalate
            // the standing it already has and must never improve it. Taken
            // verbatim, the count-derived verdict promoted a peer that
            // gossip had already put at `Dead` back to `Suspect` on its
            // third failure, then oscillated for as long as it stayed down
            // (see `Liveness::worst`).
            let new_liveness = entry.liveness.worst(counted);
            if new_liveness != entry.liveness {
                info!(peer = %id.short(), from = ?entry.liveness, to = ?new_liveness,
                    "liveness transition");
                entry.set_liveness(new_liveness, hlc);
            }
            entry.liveness != before
        };
        if changed {
            self.note_liveness_change();
        }
    }

    /// Background loop: each tick, sync with one randomly-chosen member
    /// (epidemic push-pull), plus re-probe peers with pending failure
    /// evidence so suspicion resolves fast at any mesh size.
    ///
    /// `pulse` beats once a round and after every sync (ENG-1): a round
    /// that stops coming round is a node that has stopped converging, and
    /// the watchdog should hear about it.
    pub async fn run(
        self: Arc<Self>,
        shutdown: tokio::sync::watch::Receiver<bool>,
        pulse: crate::vitals::Pulse,
    ) {
        // Two loops, side by side (ENG-7): the partner loop spreads state,
        // and the probe loop settles the peers that have stopped answering.
        // In one loop, a partner that took its whole deadline held up every
        // probe behind it, and detection was paced by the slowest peer.
        tokio::join!(
            self.partner_loop(shutdown.clone(), &pulse),
            self.probe_loop(shutdown, &pulse),
        );
    }

    /// The members this node gossips with, and the set of active members.
    fn peers(&self) -> (Vec<MemberInfo>, HashSet<NodeId>) {
        let self_id = self.engine.node_id();
        self.engine.read(|s| {
            let active: Vec<&MemberInfo> = s
                .members
                .values()
                .filter(|m| !m.left && m.info.node_id != self_id)
                .filter(|m| !s.revoked.contains_key(&m.info.node_id))
                .map(|m| &m.info)
                .collect();
            (
                active
                    .iter()
                    // A member with no announced address (joined but not yet
                    // booted) cannot be dialed — don't waste ticks on it.
                    .filter(|m| !m.addrs.is_empty())
                    .map(|m| (*m).clone())
                    .collect(),
                active.iter().map(|m| m.node_id).collect(),
            )
        })
    }

    /// Is this peer one the probe loop is looking after?
    fn is_failing(&self, id: &NodeId) -> bool {
        self.live
            .lock()
            .get(id)
            .is_some_and(|l| l.fails > 0 && l.liveness != Liveness::Dead)
    }

    /// Each tick, one bidirectional sync with the next partner in the
    /// cycle (epidemic push-pull).
    async fn partner_loop(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        pulse: &crate::vitals::Pulse,
    ) {
        loop {
            pulse.beat();
            let (peers, members) = self.peers();
            self.retain_pressure(&peers);
            self.retain_live(&members);
            // Size the connection pool to the mesh (**M-2**): gossip visits
            // every peer in turn, so a pool smaller than the set guarantees
            // a miss-and-evict cycle.
            self.transport.set_pool_target(peers.len() + 1);
            let interval = sync_interval_for(peers.len() + 1);
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => return,
            }
            // A peer that is failing is the probe loop's; the partner is
            // the next one that is not.
            let partner = (0..peers.len())
                .map(|_| self.next_partner(&peers))
                .find(|&i| !self.is_failing(&peers[i].node_id));
            if let Some(i) = partner {
                self.sync_once(peers[i].clone(), FIRST_ANSWER_DEADLINE)
                    .await;
            }
        }
    }

    /// Sticky verdict probes: a peer that failed a sync is probed again
    /// every tick, all such peers at once and each on the short deadline,
    /// until it answers or is declared dead (ENG-7).
    async fn probe_loop(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        pulse: &crate::vitals::Pulse,
    ) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(SYNC_INTERVAL) => {}
                _ = shutdown.changed() => return,
            }
            let (peers, _) = self.peers();
            let failing: Vec<MemberInfo> = peers
                .into_iter()
                .filter(|p| self.is_failing(&p.node_id))
                .take(VERDICT_PROBES)
                .collect();
            if failing.is_empty() {
                continue;
            }
            futures::future::join_all(
                failing
                    .into_iter()
                    .map(|target| self.sync_once(target, PROBE_DEADLINE)),
            )
            .await;
            pulse.beat();
        }
    }

    /// The index in `peers` of the next gossip partner (ENG-7).
    ///
    /// Every peer once per cycle, in a fresh random order each cycle. The
    /// partner used to be drawn uniformly at random every tick, which is the
    /// same on average and much worse at the tail: a peer that had just gone
    /// silent could go unasked for many ticks, and the failure detector could
    /// not start until somebody asked. Here nobody waits more than two cycles.
    fn next_partner(&self, peers: &[MemberInfo]) -> usize {
        let mut cycle = self.cycle.lock();
        loop {
            if cycle.is_empty() {
                let mut ids: Vec<NodeId> = peers.iter().map(|p| p.node_id).collect();
                shuffle(&mut ids);
                *cycle = ids;
            }
            let Some(id) = cycle.pop() else {
                return 0;
            };
            // A peer that has left since the cycle began is skipped.
            if let Some(i) = peers.iter().position(|p| p.node_id == id) {
                return i;
            }
        }
    }

    /// One gossip round with `target`, liveness bookkeeping and all (tests).
    #[doc(hidden)]
    pub async fn sync_once_for_test(&self, target: MemberInfo) {
        self.sync_once(target, FIRST_ANSWER_DEADLINE).await;
    }

    async fn sync_once(&self, target: MemberInfo, first_answer: Duration) {
        match self.sync_bounded(&target, first_answer).await {
            Ok(()) => {}
            Err(SyncError::Peer(e)) => {
                debug!(peer = %target.node_id.short(), "sync failed: {e}");
                self.mark_failure(&target.node_id);
                self.transport.drop_conn(&target.node_id);
            }
            // The peer answered; what failed was here. Nothing against it.
            Err(SyncError::Local(e)) => {
                debug!(peer = %target.node_id.short(), "sync failed on this node: {e}");
                self.mark_alive(&target.node_id);
            }
            Err(e @ SyncError::Refused(_)) => {
                debug!(peer = %target.node_id.short(), "sync incomplete: {e}");
                self.mark_alive(&target.node_id);
            }
        }
    }

    /// One full bidirectional sync with a member.
    ///
    /// Two legs, one engine. Against a peer that negotiated
    /// [`OPLOG_MINOR`] both directions carry [`OpEnvelope`]s; against an
    /// older peer both carry the V1 `Vec<Op>` and the reverse leg applies
    /// the downgrade rule (F0 §3.3). Which leg ran is invisible above
    /// this function.
    pub async fn sync_with(&self, target: &MemberInfo) -> crate::error::Result<()> {
        self.sync_bounded(target, FIRST_ANSWER_DEADLINE)
            .await
            .map_err(|e| match e {
                SyncError::Peer(e) | SyncError::Local(e) => e,
                SyncError::Refused(message) => crate::CoreError::Transport(message),
            })
    }

    /// [`Self::sync_with`], held to deadlines (ENG-7): `first_answer` for
    /// the dial and the peer's first answer together, and
    /// [`REQUEST_DEADLINE`] for every request after that.
    async fn sync_bounded(
        &self,
        target: &MemberInfo,
        first_answer: Duration,
    ) -> std::result::Result<(), SyncError> {
        let first_by = tokio::time::Instant::now() + first_answer;
        let late = |what: &str| {
            crate::CoreError::Transport(format!(
                "{} did not answer {what} in time",
                target.node_id.short()
            ))
        };
        // The dial runs on its own task, so a deadline stops *waiting* for
        // it without stopping it: a peer that is slow to reach — a punched
        // or relayed route takes seconds — is pooled by the time the next
        // round asks, instead of being cut off at the same point forever.
        let dial = {
            let transport = self.transport.clone();
            let (id, addrs) = (target.node_id, target.addrs.clone());
            tokio::spawn(async move { transport.member_conn_background(id, &addrs).await })
        };
        let conn = match tokio::time::timeout_at(first_by, dial).await {
            Ok(Ok(conn)) => conn?,
            Ok(Err(e)) => {
                return Err(SyncError::Local(crate::CoreError::State(format!(
                    "dial task: {e}"
                ))));
            }
            Err(_) => {
                // A dial that never finished is a failed dial: without this,
                // the next probe dials again and waits the whole deadline
                // again, instead of failing fast like any other failed dial.
                self.transport
                    .note_dial_timeout(target.node_id, &target.addrs);
                return Err(late("the dial").into());
            }
        };
        let v2 = self
            .transport
            .peer_minor(&target.node_id)
            .is_some_and(|m| m >= OPLOG_MINOR);

        // Loop while the peer has more than one batch for us.
        let mut caught_up_with: Option<VersionVector> = None;
        for round in 0..8 {
            // The first answer is the liveness question and gets the short
            // leash; later ones only have to start within the long one.
            let answer_by = if round == 0 {
                first_by
            } else {
                tokio::time::Instant::now() + REQUEST_DEADLINE
            };
            let batch = self.pull_once(&conn, v2, answer_by).await?;
            // Off the reactor, as on the push side (**H-3**): verifying and
            // storing a pulled batch is CPU and disk, and the gossip task
            // shares its thread with everything else on this node.
            let engine = self.engine.clone();
            let ops = batch.ops;
            let applied = tokio::task::spawn_blocking(move || engine.ingest(ops))
                .await
                .map_err(|e| {
                    SyncError::Local(crate::CoreError::State(format!("ingest task failed: {e}")))
                })?
                .map_err(SyncError::Local)?;
            self.merge_remote(&batch.liveness);

            // Reverse leg: push what the peer is missing.
            tokio::time::timeout(REQUEST_DEADLINE, self.push_missing(&conn, &batch.vv, v2))
                .await
                .map_err(|_| late("a push"))??;
            if applied > 0 {
                let _ = self.changed_tx.try_send(());
            }
            if !batch.more {
                caught_up_with = Some(batch.vv);
                break;
            }
        }
        // A restored node writes again once a peer has shown it everything
        // of its own that the peer holds (ENG-13) — and only after a
        // complete sync, since a partial one proves nothing.
        if let Some(theirs) = caught_up_with
            && self.engine.restore_hold().is_some()
            && self.engine.holds_own_history_of(&theirs)
            && let Err(e) = self
                .engine
                .release_restore_hold(&format!("synced with {}", target.name))
        {
            tracing::warn!("cannot lift the restore hold: {e}");
        }
        self.mark_alive(&target.node_id);
        // The telemetry question is a courtesy: an answer that does not
        // come in time is simply not a reading.
        let _ = tokio::time::timeout(
            REQUEST_DEADLINE,
            self.probe_pressure(&target.node_id, &conn),
        )
        .await;
        Ok(())
    }

    /// The forward leg: ask for what we are missing, normalized to
    /// envelopes whichever wire shape answered.
    async fn pull_once(
        &self,
        conn: &quinn::Connection,
        v2: bool,
        answer_by: tokio::time::Instant,
    ) -> crate::error::Result<SyncBatch> {
        let vv = self.engine.version_vector();
        let liveness = self.snapshot();
        let req = if v2 {
            Request::SyncV2(SyncRequestV2 { vv, liveness })
        } else {
            Request::Sync(SyncRequest { vv, liveness })
        };
        match self
            .transport
            .request_answered_by(conn, &req, answer_by, REQUEST_DEADLINE)
            .await?
        {
            Response::SyncV2(s) => Ok(SyncBatch {
                ops: s.ops,
                more: s.more,
                vv: s.vv,
                liveness: s.liveness,
            }),
            Response::Sync(s) => Ok(SyncBatch {
                ops: s.ops.iter().map(OpEnvelope::from_op).collect(),
                more: s.more,
                vv: s.vv,
                liveness: s.liveness,
            }),
            Response::Error { message } => Err(crate::error::CoreError::Transport(message)),
            other => Err(crate::error::CoreError::Transport(format!(
                "unexpected sync response: {other:?}"
            ))),
        }
    }

    /// The reverse leg: push what the peer told us it lacks. Against a
    /// pre-1.7 peer, ops it has no variant for are dropped here — pushing
    /// one would fail its whole frame, and everything behind it would park.
    async fn push_missing(
        &self,
        conn: &quinn::Connection,
        their_vv: &VersionVector,
        v2: bool,
    ) -> std::result::Result<(), SyncError> {
        let (missing, _) = self
            .engine
            .missing_for(their_vv)
            .map_err(SyncError::Local)?;
        if missing.is_empty() {
            return Ok(());
        }
        let req = if v2 {
            Request::PushOpsV2 { ops: missing }
        } else {
            let (ops, _cut) = mesh_proto::downgrade_for_v1(&missing);
            if ops.is_empty() {
                return Ok(());
            }
            Request::PushOps { ops }
        };
        if let Response::Error { message } = self.transport.request_one(conn, &req).await? {
            return Err(SyncError::Refused(message));
        }
        Ok(())
    }

    /// Ask the peer we just synced with how loaded it is (M9 §5.2). This
    /// rides the connection the gossip tick already opened — no extra dial,
    /// no new pooled connection — and is best-effort: a failure here is not
    /// failure evidence, because pressure is an optimisation and liveness
    /// is not.
    ///
    /// Two peers are never asked. One below `COMPUTE_MINOR` could not
    /// decode the request. One reachable only through a relay would make
    /// every tick cost a relay hop for an advisory signal, and relays are
    /// soft traffic hubs the design already treats as expensive (M9 §3.5,
    /// §5.5). Both simply leave that peer's pressure unknown, which
    /// placement treats as neutral.
    async fn probe_pressure(&self, peer: &NodeId, conn: &quinn::Connection) {
        let Some(pooled) = self.transport.pooled_conn(peer) else {
            return;
        };
        if pooled.minor < COMPUTE_MINOR || pooled.kind != ConnKind::Direct {
            return;
        }
        // One question or the other, never both: `PressureV2` carries the
        // V1 snapshot inside it, so a peer at or above `ACCEL_MINOR`
        // answers everything in one round trip and a peer below it answers
        // the question it has code for. A tick that asked twice would
        // double the cost of an advisory signal to learn nothing extra.
        let request = if pooled.minor >= mesh_proto::DEVICE_KEY_MINOR {
            Request::TelemetryV2
        } else if pooled.minor >= mesh_proto::TELEMETRY_MINOR {
            Request::Telemetry
        } else if pooled.minor >= mesh_proto::ACCEL_MINOR {
            Request::PressureV2
        } else {
            Request::Pressure
        };
        match self.transport.request_one(conn, &request).await {
            Ok(Response::Pressure(snapshot)) => self.record_pressure(*peer, snapshot),
            Ok(Response::PressureV2(v2)) => {
                let devices = v2.devices.into_iter().map(Into::into).collect();
                self.record_pressure_v2(*peer, v2.base, devices);
            }
            Ok(Response::Telemetry(entries)) => {
                self.merge_telemetry(entries.into_iter().map(Into::into).collect());
            }
            Ok(Response::TelemetryV2(entries)) => self.merge_telemetry(entries),
            Ok(other) => debug!(peer = %peer.short(), "unexpected pressure response: {other:?}"),
            Err(e) => debug!(peer = %peer.short(), "pressure probe failed: {e}"),
        }
    }
}

/// Random walk step for gossip partner selection, from the OS entropy
/// source (no rand crate needed for one index).
/// Shuffle in place (Fisher–Yates), from the system's random source.
fn shuffle<T>(items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let mut buf = [0u8; 8];
        let r = if getrandom::fill(&mut buf).is_ok() {
            usize::from_le_bytes(buf)
        } else {
            i
        };
        items.swap(i, r % (i + 1));
    }
}
