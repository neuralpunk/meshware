//! WAN reachability (MASTERPLAN M7): reflexive address discovery, the
//! relay server/client roles, candidate announcement, and coordinated
//! punch handling.
//!
//! A background task periodically asks up to three direct peers what
//! address they see us from (`Request::Observe`). Observations that match
//! one of our own addresses are local evidence (a LAN peer reached us
//! directly); the non-local ones classify the NAT in front of us:
//!
//! - every observation local (or `[wan] advertise` set) → **Public**;
//! - non-local observers agree on one `ip:port` → **Natted** (cone-ish);
//!   that address is published as a reflexive candidate;
//! - non-local observers see different ports → **SymmetricNat**; nothing
//!   is published (the mapping is per destination, useless to others);
//! - nobody answered yet → **Unknown**.
//!
//! A Public node with `[wan] relay = "on"` enables the relay hub and
//! announces the reserved `relay` tag. A NATed node with `relay_client`
//! registers with up to two relay-tagged members (rendezvous-hashed for
//! stability), announces `relay:<id>` candidates, and dials back directly
//! when a punch request arrives through a registration. All of this is
//! announced through ordinary `MemberUpdate`s — no new op schema.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};
use tracing::{debug, info, warn};

use crate::config::WanConfig;
use crate::gossip::Gossip;
use crate::scheduler;
use crate::state::StateEngine;
use crate::transport::{ConnKind, RelayRegistration, Transport};
use mesh_proto::{Liveness, MemberInfo, NodeId, OpBody, RELAY_TAG, WAN_MINOR, WanInfo};

/// Observers asked per round.
const OBSERVERS_PER_ROUND: usize = 3;
/// Observations older than this are forgotten.
const OBSERVATION_TTL: Duration = Duration::from_secs(300);
/// Tick cadence: fast while the node is young (discovery converges in
/// seconds, tests included), then relaxed.
const FAST_TICK: Duration = Duration::from_secs(2);
const FAST_PHASE: Duration = Duration::from_secs(60);
const SLOW_TICK: Duration = Duration::from_secs(15);
/// Relays a NATed node registers with.
const RELAYS_WANTED: usize = 2;
/// Rendezvous context for relay selection.
const RELAY_PICK_CONTEXT: &[u8] = b"mw-relay-pick-v1";
/// Punch dedupe window per requester.
const PUNCH_DEDUPE: Duration = Duration::from_secs(5);

/// What the node has learned about the network in front of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    Unknown,
    Public,
    Natted,
    SymmetricNat,
}

impl Reachability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reachability::Unknown => "unknown",
            Reachability::Public => "public",
            Reachability::Natted => "natted",
            Reachability::SymmetricNat => "symmetric-nat",
        }
    }
}

#[derive(Debug, Clone)]
struct Observation {
    observer: NodeId,
    addr: SocketAddr,
    at: Instant,
}

struct Inner {
    observations: Vec<Observation>,
    reachability: Reachability,
    reflexive: Option<SocketAddr>,
    relay_server: bool,
    registrations: HashMap<NodeId, RelayRegistration>,
    recent_punches: HashMap<NodeId, Instant>,
    last_announced: Option<(Vec<String>, bool)>,
    ticks: u64,
}

pub struct Wan {
    engine: Arc<StateEngine>,
    transport: Arc<Transport>,
    gossip: Arc<Gossip>,
    listen_port: u16,
    /// Loopback + LAN + configured IP with the real port: the direct
    /// candidates every node has regardless of WAN state.
    base_addrs: Vec<String>,
    config: Mutex<WanConfig>,
    inner: Mutex<Inner>,
    kick: Notify,
    started: Instant,
}

impl std::fmt::Debug for Wan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Wan({:?})", self.reachability())
    }
}

impl Wan {
    pub fn new(
        engine: Arc<StateEngine>,
        transport: Arc<Transport>,
        gossip: Arc<Gossip>,
        listen_port: u16,
        base_addrs: Vec<String>,
        config: WanConfig,
    ) -> Arc<Self> {
        let wan = Arc::new(Wan {
            engine,
            transport,
            gossip,
            listen_port,
            base_addrs,
            config: Mutex::new(config),
            inner: Mutex::new(Inner {
                observations: Vec::new(),
                reachability: Reachability::Unknown,
                reflexive: None,
                relay_server: false,
                registrations: HashMap::new(),
                recent_punches: HashMap::new(),
                last_announced: None,
                ticks: 0,
            }),
            kick: Notify::new(),
            started: Instant::now(),
        });
        wan.transport.set_own_candidates(wan.direct_candidates());
        // Inbound punch requests (through our relay registrations).
        let weak = Arc::downgrade(&wan);
        wan.transport
            .set_punch_handler(Arc::new(move |from, candidates| {
                if let Some(w) = weak.upgrade() {
                    w.handle_punch(from, candidates);
                }
            }));
        wan
    }

    fn inner(&self) -> parking_lot::MutexGuard<'_, Inner> {
        self.inner.lock()
    }

    pub fn config(&self) -> WanConfig {
        self.config.lock().clone()
    }

    /// Hot-reload (SIGHUP / `apply_config`): re-evaluate at once.
    pub fn set_config(&self, new: WanConfig) {
        *self.config.lock() = new;
        self.transport.set_own_candidates(self.direct_candidates());
        self.kick.notify_one();
    }

    /// Re-evaluate now (tests, config changes).
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub fn reachability(&self) -> Reachability {
        self.inner().reachability
    }

    pub fn reflexive_addr(&self) -> Option<SocketAddr> {
        self.inner().reflexive
    }

    pub fn is_relay_server(&self) -> bool {
        self.inner().relay_server
    }

    /// Relays this node is currently registered with.
    pub fn relays(&self) -> Vec<NodeId> {
        let mut v: Vec<NodeId> = self
            .inner()
            .registrations
            .iter()
            .filter(|(_, r)| r.is_active())
            .map(|(id, _)| *id)
            .collect();
        v.sort();
        v
    }

    /// Status summary for `meshnode status` / Director.
    pub fn info(&self) -> WanInfo {
        let cfg = self.config();
        let relay = self.transport.relay().snapshot();
        let inner = self.inner();
        let relays = inner
            .registrations
            .iter()
            .filter(|(_, r)| r.is_active())
            .map(|(id, _)| id.short())
            .collect();
        WanInfo {
            reachability: inner.reachability.as_str().to_string(),
            reflexive_addr: inner.reflexive.map(|a| a.to_string()),
            advertise: cfg.advertise.clone(),
            relay_server: inner.relay_server,
            relay_clients: u32::try_from(relay.clients).unwrap_or(u32::MAX),
            relays,
        }
    }

    /// Our direct candidates, LAN-first: base (loopback, LAN IP), then
    /// operator-advertised, then the reflexive address. Never `relay:`.
    pub fn direct_candidates(&self) -> Vec<String> {
        let cfg = self.config();
        let mut out = self.base_addrs.clone();
        for a in &cfg.advertise {
            if !out.contains(a) {
                out.push(a.clone());
            }
        }
        if let Some(r) = self.inner().reflexive {
            let s = r.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    /// Direct candidates ordered public-first (for invite rendezvous sets).
    pub fn public_first_candidates(&self) -> Vec<String> {
        let mut v = self.direct_candidates();
        v.sort_by_key(|a| {
            match a.parse::<SocketAddr>() {
                Ok(sa) => {
                    let ip = sa.ip();
                    if is_public_ip(ip) {
                        0
                    } else if ip.is_loopback() {
                        2
                    } else {
                        1
                    }
                }
                Err(_) => 0, // a hostname: the operator's choice, assume public
            }
        });
        v
    }

    /// Everything we announce in `MemberInfo::addrs`: direct candidates plus
    /// one `relay:<id>` per live relay registration.
    pub fn announce_addrs(&self) -> Vec<String> {
        let mut out = self.direct_candidates();
        for relay in self.relays() {
            out.push(mesh_proto::relay_candidate(&relay));
        }
        out
    }

    /// Background loop.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        // First round shortly after boot: our boot announcement needs a
        // moment to land on a peer we can ask.
        let mut next = Duration::from_secs(1);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(next) => {}
                _ = self.kick.notified() => {}
                _ = shutdown.changed() => {
                    self.shutdown();
                    return;
                }
            }
            self.tick().await;
            next = if self.started.elapsed() < FAST_PHASE {
                FAST_TICK
            } else {
                SLOW_TICK
            };
        }
    }

    fn shutdown(&self) {
        let regs: Vec<RelayRegistration> =
            self.inner().registrations.drain().map(|(_, r)| r).collect();
        for r in regs {
            r.close();
        }
    }

    /// One round: observe, classify, (de)activate the relay roles,
    /// announce if anything changed.
    pub async fn tick(&self) {
        let ticks = {
            let mut inner = self.inner();
            inner.ticks += 1;
            inner.ticks
        };
        // Observe every fast tick, every other slow tick (~30 s).
        let observe = self.started.elapsed() < FAST_PHASE || ticks % 2 == 0;
        if observe {
            self.observe_round().await;
        }
        self.classify();
        self.apply_relay_roles().await;
        self.transport.set_own_candidates(self.direct_candidates());
        self.maybe_announce();
    }

    fn me(&self) -> NodeId {
        self.engine.node_id()
    }

    /// Members we could ask: alive, not us, with at least one direct
    /// candidate. Those with a pooled direct 1.5 connection come first.
    fn observer_candidates(&self) -> Vec<MemberInfo> {
        let me = self.me();
        let mut peers: Vec<MemberInfo> = self.engine.read(|s| {
            s.members
                .values()
                .filter(|m| s.is_active_member(&m.info.node_id) && m.info.node_id != me)
                .filter(|m| {
                    m.info
                        .addrs
                        .iter()
                        .any(|a| mesh_proto::parse_relay_candidate(a).is_none())
                })
                .filter(|m| {
                    matches!(
                        self.gossip.liveness_of(&m.info.node_id),
                        Liveness::Alive | Liveness::Suspect
                    )
                })
                .map(|m| m.info.clone())
                .collect()
        });
        // Cached: the key reads the live connection pool, which changes
        // while a mesh is forming. Recomputed per comparison, one peer could
        // order differently against two others mid-sort, and the standard
        // sort panics on an order that is not total (it did, on a loaded CI
        // runner joining 100 nodes). One snapshot per peer, taken once.
        peers.sort_by_cached_key(|m| {
            let pooled = self.transport.pooled_conn(&m.node_id);
            let direct_15 = pooled
                .as_ref()
                .is_some_and(|c| c.kind == ConnKind::Direct && c.minor >= WAN_MINOR);
            let relay_tagged = m.tags.iter().any(|t| t == RELAY_TAG);
            (!direct_15, !relay_tagged, m.node_id)
        });
        peers
    }

    async fn observe_round(&self) {
        let candidates = self.observer_candidates();
        let mut asked = 0usize;
        for peer in candidates {
            if asked >= OBSERVERS_PER_ROUND {
                break;
            }
            let id = peer.node_id;
            // Only direct connections tell us anything; dial if needed.
            let pooled = self.transport.pooled_conn(&id);
            if pooled.is_none() {
                let direct: Vec<String> = peer
                    .addrs
                    .iter()
                    .filter(|a| mesh_proto::parse_relay_candidate(a).is_none())
                    .cloned()
                    .collect();
                if self
                    .transport
                    .member_conn_direct(id, &direct, false)
                    .await
                    .is_err()
                {
                    continue;
                }
            }
            match self.transport.observe(&id).await {
                Ok(Some(addr)) => {
                    asked += 1;
                    let mut inner = self.inner();
                    inner.observations.retain(|o| o.observer != id);
                    inner.observations.push(Observation {
                        observer: id,
                        addr,
                        at: Instant::now(),
                    });
                }
                Ok(None) => {}
                Err(e) => debug!(peer = %id.short(), "observe failed: {e}"),
            }
        }
        let mut inner = self.inner();
        inner
            .observations
            .retain(|o| o.at.elapsed() < OBSERVATION_TTL);
    }

    /// Local IPs: everything in our base/advertised candidates.
    fn local_ips(&self) -> Vec<IpAddr> {
        let cfg = self.config();
        self.base_addrs
            .iter()
            .chain(cfg.advertise.iter())
            .filter_map(|a| a.parse::<SocketAddr>().ok().map(|s| s.ip()))
            .collect()
    }

    fn is_local_observation(&self, addr: SocketAddr, local_ips: &[IpAddr]) -> bool {
        let ip = addr.ip();
        if local_ips.contains(&ip) && addr.port() == self.listen_port {
            return true;
        }
        // A private/loopback/link-local address on our own listen port is
        // one of our interfaces seen from the LAN (CGNAT 100.64/10 is not
        // private here and counts as non-local).
        let lan = match ip {
            IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
            }
        };
        lan && addr.port() == self.listen_port
    }

    fn classify(&self) {
        let cfg = self.config();
        let local_ips = self.local_ips();
        let mut inner = self.inner();
        let observations = inner.observations.clone();
        let non_local: Vec<&Observation> = observations
            .iter()
            .filter(|o| !self.is_local_observation(o.addr, &local_ips))
            .collect();
        let (reach, reflexive) = if observations.is_empty() {
            (Reachability::Unknown, None)
        } else if non_local.is_empty() {
            (Reachability::Public, None)
        } else {
            let mut by_addr: HashMap<SocketAddr, usize> = HashMap::new();
            for o in &non_local {
                *by_addr.entry(o.addr).or_default() += 1;
            }
            let distinct_ports: std::collections::HashSet<(IpAddr, u16)> = non_local
                .iter()
                .map(|o| (o.addr.ip(), o.addr.port()))
                .collect();
            if non_local.len() >= 2 && distinct_ports.len() >= 2 {
                // Different observers, different mappings: per-destination
                // NAT. Nothing we publish would be dialable.
                (Reachability::SymmetricNat, None)
            } else {
                let needed = 2.min(non_local.len()).max(1);
                match by_addr.iter().find(|(_, n)| **n >= needed) {
                    Some((addr, _)) => (Reachability::Natted, Some(*addr)),
                    None => (Reachability::Unknown, None),
                }
            }
        };
        // Operator-declared public endpoints count as verified public.
        let reach = if !cfg.advertise.is_empty() && reach != Reachability::SymmetricNat {
            Reachability::Public
        } else {
            reach
        };
        if reach != inner.reachability || reflexive != inner.reflexive {
            info!(
                reachability = reach.as_str(),
                reflexive = ?reflexive,
                observers = observations.len(),
                "wan: reachability updated"
            );
        }
        inner.reachability = reach;
        inner.reflexive = reflexive;
    }

    /// Relay server: on when configured and verified public. Relay client:
    /// when not public (and not a relay) and allowed, keep registrations
    /// with the two rendezvous-chosen live relays.
    async fn apply_relay_roles(&self) {
        let cfg = self.config();
        let (reach, was_server) = {
            let inner = self.inner();
            (inner.reachability, inner.relay_server)
        };
        let server = cfg.relay && reach == Reachability::Public;
        if server != was_server {
            info!(relay_server = server, "wan: relay role changed");
        }
        self.transport.relay().set_enabled(server);
        self.inner().relay_server = server;

        let want_client = cfg.relay_client
            && !server
            && matches!(
                reach,
                Reachability::Natted | Reachability::SymmetricNat | Reachability::Unknown
            );
        if !want_client {
            let regs: Vec<RelayRegistration> =
                self.inner().registrations.drain().map(|(_, r)| r).collect();
            for r in regs {
                r.close();
            }
            return;
        }
        // Prune dead registrations.
        {
            let mut inner = self.inner();
            let dead: Vec<NodeId> = inner
                .registrations
                .iter()
                .filter(|(_, r)| !r.is_active())
                .map(|(id, _)| *id)
                .collect();
            for id in dead {
                warn!(relay = %id.short(), "wan: relay registration ended");
                inner.registrations.remove(&id);
            }
        }
        let me = self.me();
        let relays: Vec<MemberInfo> = self.engine.read(|s| {
            s.members
                .values()
                .filter(|m| s.is_active_member(&m.info.node_id) && m.info.node_id != me)
                .filter(|m| m.info.tags.iter().any(|t| t == RELAY_TAG))
                .filter(|m| !m.info.tags.iter().any(|t| t == mesh_proto::DRAINING_TAG))
                .filter(|m| self.gossip.liveness_of(&m.info.node_id) != Liveness::Dead)
                .map(|m| m.info.clone())
                .collect()
        });
        let ranked =
            scheduler::rendezvous_rank(RELAY_PICK_CONTEXT, &me.0, relays.iter().map(|m| m.node_id));
        let wanted: Vec<NodeId> = ranked.into_iter().take(RELAYS_WANTED).collect();
        // Drop registrations no longer wanted.
        {
            let mut inner = self.inner();
            let stale: Vec<NodeId> = inner
                .registrations
                .keys()
                .filter(|id| !wanted.contains(id))
                .copied()
                .collect();
            for id in stale {
                if let Some(r) = inner.registrations.remove(&id) {
                    info!(relay = %id.short(), "wan: leaving relay");
                    r.close();
                }
            }
        }
        // Register with the missing ones.
        for id in wanted {
            if self.inner().registrations.contains_key(&id) {
                continue;
            }
            let Some(info) = relays.iter().find(|m| m.node_id == id) else {
                continue;
            };
            let direct: Vec<String> = info
                .addrs
                .iter()
                .filter(|a| mesh_proto::parse_relay_candidate(a).is_none())
                .cloned()
                .collect();
            match self.transport.register_with_relay(id, &direct).await {
                Ok(reg) => {
                    self.inner().registrations.insert(id, reg);
                }
                Err(e) => debug!(relay = %id.short(), "wan: relay registration failed: {e}"),
            }
        }
    }

    /// Re-announce when our candidate set or relay tag changed.
    fn maybe_announce(&self) {
        let me = self.me();
        let addrs = self.announce_addrs();
        let relay_tag = self.inner().relay_server;
        {
            let inner = self.inner();
            if inner.last_announced.as_ref() == Some(&(addrs.clone(), relay_tag)) {
                // Still verify state carries it (a concurrent tag update
                // might have raced us).
                let in_state = self.engine.read(|s| {
                    s.members.get(&me).map(|m| {
                        (
                            m.info.addrs.clone(),
                            m.info.tags.iter().any(|t| t == RELAY_TAG),
                        )
                    })
                });
                if in_state == Some((addrs.clone(), relay_tag)) {
                    return;
                }
            }
        }
        let Some(info) = self
            .engine
            .read(|s| s.members.get(&me).map(|m| m.info.clone()))
        else {
            return;
        };
        let mut tags: Vec<String> = info
            .tags
            .iter()
            .filter(|t| *t != RELAY_TAG)
            .cloned()
            .collect();
        if relay_tag {
            tags.push(RELAY_TAG.to_string());
        }
        if info.addrs == addrs && info.tags == tags {
            self.inner().last_announced = Some((addrs, relay_tag));
            return;
        }
        match self.engine.author_op(OpBody::MemberUpdate {
            node_id: me,
            name: info.name,
            addrs: addrs.clone(),
            tags,
            slots: info.slots,
        }) {
            Ok(_) => {
                info!(?addrs, relay = relay_tag, "wan: announced candidates");
                self.inner().last_announced = Some((addrs, relay_tag));
            }
            Err(e) => warn!("wan: cannot announce candidates: {e}"),
        }
    }

    /// A punch request arrived through one of our relay registrations:
    /// validate the requester is an active member, dedupe, and dial back
    /// directly (bounded by the transport).
    pub fn handle_punch(self: &Arc<Self>, from: NodeId, candidates: Vec<String>) {
        if from == self.me() {
            return;
        }
        let active = self.engine.read(|s| s.is_active_member(&from));
        if !active {
            debug!(from = %from.short(), "wan: punch from non-member ignored");
            return;
        }
        {
            let mut inner = self.inner();
            let now = Instant::now();
            inner
                .recent_punches
                .retain(|_, at| now.duration_since(*at) < PUNCH_DEDUPE);
            if inner.recent_punches.contains_key(&from) {
                return;
            }
            inner.recent_punches.insert(from, now);
        }
        if self
            .transport
            .pooled_conn(&from)
            .is_some_and(|c| c.kind == ConnKind::Direct)
        {
            return; // already directly connected
        }
        let transport = self.transport.clone();
        tokio::spawn(async move {
            match transport.dial_back(from, &candidates).await {
                Ok(_) => info!(peer = %from.short(), "wan: punch dial-back succeeded"),
                Err(e) => debug!(peer = %from.short(), "wan: punch dial-back failed: {e}"),
            }
        });
    }
}

/// Globally routable address (best effort on stable APIs): not loopback,
/// private, link-local, CGNAT, unspecified, or synthetic.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let cgnat = o[0] == 100 && (64..=127).contains(&o[1]);
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || cgnat
                || crate::meshsock::is_synthetic_ip(ip))
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| !is_public_ip(IpAddr::V4(v4))))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn public_ip_classification() {
        assert!(!is_public_ip("203.0.113.5".parse().unwrap())); // documentation range
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_public_ip("10.1.2.3".parse().unwrap()));
        assert!(!is_public_ip("192.168.0.1".parse().unwrap()));
        assert!(!is_public_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("240.0.0.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700::1111".parse().unwrap()));
        assert!(!is_public_ip("fd00::1".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
    }
}
