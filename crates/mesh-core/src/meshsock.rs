//! `MeshSocket` — the one UDP socket a node owns, wrapped so a second,
//! synthetic address family rides on it (MASTERPLAN M7 relay data plane).
//!
//! Real destinations pass straight through to the kernel socket (one range
//! check per send). Destinations inside IPv4 Class E (`240.0.0.0/4`) are
//! *synthetic*: each maps to a `(relay, peer)` route, and packets for it
//! are framed as [`RelayFrame`]s onto a long-lived stream of the outer
//! QUIC connection to that relay (the registration uplink when we are the
//! relay's client, or the `RelayOpen` stream when we initiated the path).
//! Frames arriving from a relay are injected back into the endpoint as if
//! they had come from the synthetic address — so the *inner* QUIC session
//! between the two nodes is an ordinary key-pinned connection that happens
//! to see Class E on the wire, and the relay sees only ciphertext.
//!
//! Why Class E IPv4: quinn rejects IPv6 destinations on an IPv4-bound
//! endpoint, and the default bind is `0.0.0.0`; Class E is reserved,
//! never routed, and has 2²⁸ addresses. Synthetic addresses are stable per
//! `(relay, peer)` for the life of the route, so the inner session never
//! sees a spurious path migration.
//!
//! Semantics are UDP's: a frame that cannot be queued (no route, full
//! queue, oversize) is dropped, never blocked — `try_send` to a synthetic
//! destination never returns `WouldBlock`.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;

use mesh_proto::{MAX_RELAY_FRAME, NodeId, RelayFrame};

/// Fixed port of every synthetic address (the IP alone identifies the
/// route; a fixed port keeps the 4-tuple stable).
pub const SYNTHETIC_PORT: u16 = 7;
/// Bound on frames parked for the endpoint driver (UDP drop-on-full).
const INJECT_QUEUE: usize = 1024;

/// Is this address inside the synthetic (relay-routed) range? IPv4-mapped
/// IPv6 forms count too: a dual-stack endpoint sees `::ffff:240.x.y.z`.
pub fn is_synthetic(addr: SocketAddr) -> bool {
    is_synthetic_ip(addr.ip())
}

pub fn is_synthetic_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] >= 240 && v4 != Ipv4Addr::BROADCAST,
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .is_some_and(|v4| is_synthetic_ip(IpAddr::V4(v4))),
    }
}

/// Fold IPv4-mapped IPv6 back to plain IPv4 (what peers should see in
/// `Observed`, what candidates are written as).
pub fn normalize(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => addr,
        },
        v4 => v4,
    }
}

struct Route {
    relay: NodeId,
    peer: NodeId,
    ip: Ipv4Addr,
    /// Outbound frames for `peer` go here: the relay registration uplink
    /// (we are a client of `relay`) or the `RelayOpen` stream pump (we
    /// initiated the path through `relay`).
    tx: mpsc::Sender<RelayFrame>,
    /// Monotonic per-route generation: a stale pump tearing down "its"
    /// route must not remove a newer one for the same pair.
    generation: u64,
}

#[derive(Default)]
struct Tables {
    by_key: HashMap<(NodeId, NodeId), Arc<Route>>,
    by_ip: HashMap<Ipv4Addr, Arc<Route>>,
    /// Registration uplinks per relay: routes for peers reaching us through
    /// that relay are created on demand from these.
    uplinks: HashMap<NodeId, mpsc::Sender<RelayFrame>>,
    next: u32,
    generation: u64,
}

/// Relay data-plane counters (tests assert they stay flat on direct paths;
/// `/metrics` exports them).
#[derive(Debug, Default)]
pub struct MeshSockStats {
    pub relayed_sent: AtomicU64,
    pub relayed_recv: AtomicU64,
    pub dropped: AtomicU64,
}

pub struct MeshSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    /// Endpoint is IPv6-bound: inject synthetic sources in mapped form so
    /// the endpoint's address bookkeeping stays in one family.
    ipv6: bool,
    tables: Mutex<Tables>,
    queue: Mutex<VecDeque<(SocketAddr, Vec<u8>)>>,
    queued: AtomicUsize,
    waker: Mutex<Option<Waker>>,
    pub stats: MeshSockStats,
}

impl std::fmt::Debug for MeshSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MeshSocket(routes={})", self.route_count())
    }
}

impl MeshSocket {
    pub fn new(inner: Arc<dyn AsyncUdpSocket>, ipv6: bool) -> Self {
        MeshSocket {
            inner,
            ipv6,
            tables: Mutex::new(Tables::default()),
            queue: Mutex::new(VecDeque::new()),
            queued: AtomicUsize::new(0),
            waker: Mutex::new(None),
            stats: MeshSockStats::default(),
        }
    }

    fn tables(&self) -> parking_lot::MutexGuard<'_, Tables> {
        self.tables.lock()
    }

    fn socket_addr(&self, ip: Ipv4Addr) -> SocketAddr {
        if self.ipv6 {
            SocketAddr::V6(SocketAddrV6::new(ip.to_ipv6_mapped(), SYNTHETIC_PORT, 0, 0))
        } else {
            SocketAddr::V4(SocketAddrV4::new(ip, SYNTHETIC_PORT))
        }
    }

    fn alloc_ip(t: &mut Tables) -> Ipv4Addr {
        // 240.0.0.0/4 minus network/broadcast; sequential, skipping live
        // entries (wraps after 2²⁸ allocations — never in practice).
        loop {
            t.next = t.next.wrapping_add(1) & 0x0FFF_FFFF;
            if t.next == 0 || t.next == 0x0FFF_FFFF {
                continue;
            }
            let ip = Ipv4Addr::from(0xF000_0000u32 | t.next);
            if !t.by_ip.contains_key(&ip) {
                return ip;
            }
        }
    }

    /// Register (or replace) the uplink for `relay`: the sender feeding the
    /// registration stream. Peers that reach us through `relay` get routes
    /// created on demand from it.
    pub fn set_uplink(&self, relay: NodeId, tx: mpsc::Sender<RelayFrame>) {
        let mut t = self.tables();
        t.uplinks.insert(relay, tx.clone());
        // Existing on-demand routes via this relay switch to the new uplink.
        let keys: Vec<(NodeId, NodeId)> = t
            .by_key
            .keys()
            .filter(|(r, _)| *r == relay)
            .copied()
            .collect();
        for key in keys {
            if let Some(old) = t.by_key.get(&key).cloned()
                && old.tx.is_closed()
            {
                let route = Arc::new(Route {
                    relay,
                    peer: key.1,
                    ip: old.ip,
                    tx: tx.clone(),
                    generation: old.generation,
                });
                t.by_key.insert(key, route.clone());
                t.by_ip.insert(old.ip, route);
            }
        }
    }

    /// Add an explicit route (a `RelayOpen` path we initiated). Returns the
    /// synthetic address to dial and the route's generation. Replaces any
    /// existing route for the pair but keeps its address, so an inner
    /// session in flight is undisturbed.
    pub fn add_route(
        &self,
        relay: NodeId,
        peer: NodeId,
        tx: mpsc::Sender<RelayFrame>,
    ) -> (SocketAddr, u64) {
        let mut t = self.tables();
        let ip = match t.by_key.get(&(relay, peer)) {
            Some(existing) => existing.ip,
            None => Self::alloc_ip(&mut t),
        };
        t.generation += 1;
        let generation = t.generation;
        let route = Arc::new(Route {
            relay,
            peer,
            ip,
            tx,
            generation,
        });
        t.by_key.insert((relay, peer), route.clone());
        t.by_ip.insert(ip, route);
        (self.socket_addr(ip), generation)
    }

    /// Current generation of the route for `(relay, peer)`, if any.
    pub fn route_generation(&self, relay: NodeId, peer: NodeId) -> Option<u64> {
        self.tables()
            .by_key
            .get(&(relay, peer))
            .map(|r| r.generation)
    }

    /// Drop the route for `(relay, peer)` only if it is still `generation`.
    pub fn drop_route_if(&self, relay: NodeId, peer: NodeId, generation: u64) -> bool {
        let mut t = self.tables();
        let hit = t
            .by_key
            .get(&(relay, peer))
            .is_some_and(|r| r.generation == generation);
        if hit && let Some(r) = t.by_key.remove(&(relay, peer)) {
            t.by_ip.remove(&r.ip);
        }
        hit
    }

    /// The synthetic address for `(relay, peer)`, creating the route from
    /// the relay's uplink when we are registered there. `None` when no
    /// route exists and none can be made.
    pub fn route_for(&self, relay: NodeId, peer: NodeId) -> Option<SocketAddr> {
        let mut t = self.tables();
        if let Some(r) = t.by_key.get(&(relay, peer))
            && !r.tx.is_closed()
        {
            return Some(self.socket_addr(r.ip));
        }
        let uplink = t.uplinks.get(&relay)?.clone();
        let ip = match t.by_key.get(&(relay, peer)) {
            Some(stale) => stale.ip,
            None => Self::alloc_ip(&mut t),
        };
        t.generation += 1;
        let generation = t.generation;
        let route = Arc::new(Route {
            relay,
            peer,
            ip,
            tx: uplink,
            generation,
        });
        t.by_key.insert((relay, peer), route.clone());
        t.by_ip.insert(ip, route);
        Some(self.socket_addr(ip))
    }

    /// Reverse lookup: which `(relay, peer)` does a synthetic address map to?
    pub fn route_by_addr(&self, addr: SocketAddr) -> Option<(NodeId, NodeId)> {
        let SocketAddr::V4(v4) = normalize(addr) else {
            return None;
        };
        self.tables().by_ip.get(v4.ip()).map(|r| (r.relay, r.peer))
    }

    pub fn drop_route(&self, relay: NodeId, peer: NodeId) {
        let mut t = self.tables();
        if let Some(r) = t.by_key.remove(&(relay, peer)) {
            t.by_ip.remove(&r.ip);
        }
    }

    pub fn drop_route_addr(&self, addr: SocketAddr) {
        if let Some((relay, peer)) = self.route_by_addr(addr) {
            self.drop_route(relay, peer);
        }
    }

    /// Drop every route through `relay` (its registration or connection
    /// ended) and forget its uplink.
    pub fn drop_routes_via(&self, relay: &NodeId) {
        let mut t = self.tables();
        t.uplinks.remove(relay);
        let keys: Vec<(NodeId, NodeId)> = t
            .by_key
            .keys()
            .filter(|(r, _)| r == relay)
            .copied()
            .collect();
        for key in keys {
            if let Some(r) = t.by_key.remove(&key) {
                t.by_ip.remove(&r.ip);
            }
        }
    }

    pub fn route_count(&self) -> usize {
        self.tables().by_key.len()
    }

    /// A frame arrived from `relay`, sent by `from`: hand it to the
    /// endpoint as a datagram from the synthetic address of `(relay, from)`.
    pub fn inject(&self, relay: NodeId, from: NodeId, payload: Vec<u8>) {
        if payload.is_empty() || payload.len() > MAX_RELAY_FRAME {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(addr) = self.route_for(relay, from) else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        {
            let mut q = self.queue.lock();
            if q.len() >= INJECT_QUEUE {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            q.push_back((addr, payload));
            self.queued.store(q.len(), Ordering::Release);
        }
        self.stats.relayed_recv.fetch_add(1, Ordering::Relaxed);
        if let Some(w) = self.waker.lock().take() {
            w.wake();
        }
    }

    pub fn relayed_sent(&self) -> u64 {
        self.stats.relayed_sent.load(Ordering::Relaxed)
    }

    pub fn relayed_recv(&self) -> u64 {
        self.stats.relayed_recv.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.stats.dropped.load(Ordering::Relaxed)
    }
}

impl AsyncUdpSocket for MeshSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // Synthetic sends never block, so writability is the inner socket's.
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let dest = normalize(transmit.destination);
        if !is_synthetic(dest) {
            return self.inner.try_send(transmit);
        }
        let SocketAddr::V4(v4) = dest else {
            return Ok(());
        };
        let route = self.tables().by_ip.get(v4.ip()).cloned();
        let Some(route) = route else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        // GSO batches carry several datagrams back to back; each becomes
        // one frame (the relay and the far end see plain datagrams).
        let seg = transmit
            .segment_size
            .unwrap_or(transmit.contents.len())
            .max(1);
        for chunk in transmit.contents.chunks(seg) {
            if chunk.len() > MAX_RELAY_FRAME {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let frame = RelayFrame {
                peer: route.peer,
                payload: chunk.to_vec(),
            };
            match route.tx.try_send(frame) {
                Ok(()) => {
                    self.stats.relayed_sent.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Park the waker *before* checking the queue so an inject racing
        // this poll still wakes us.
        {
            let mut w = self.waker.lock();
            if !w.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                *w = Some(cx.waker().clone());
            }
        }
        if self.queued.load(Ordering::Acquire) > 0 {
            let mut q = self.queue.lock();
            let mut n = 0;
            for (buf, m) in bufs.iter_mut().zip(meta.iter_mut()) {
                let Some((addr, payload)) = q.pop_front() else {
                    break;
                };
                let len = payload.len().min(buf.len());
                buf[..len].copy_from_slice(&payload[..len]);
                *m = RecvMeta {
                    addr,
                    len,
                    stride: len,
                    ecn: None,
                    dst_ip: None,
                };
                n += 1;
            }
            self.queued.store(q.len(), Ordering::Release);
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
        }
        self.inner.poll_recv(cx, bufs, meta)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_range() {
        assert!(is_synthetic("240.0.0.1:7".parse().unwrap()));
        assert!(is_synthetic("250.1.2.3:7".parse().unwrap()));
        assert!(!is_synthetic("255.255.255.255:7".parse().unwrap()));
        assert!(!is_synthetic("239.255.255.255:7".parse().unwrap()));
        assert!(!is_synthetic("10.0.0.1:7777".parse().unwrap()));
        assert!(is_synthetic("[::ffff:240.0.0.9]:7".parse().unwrap()));
        assert!(!is_synthetic("[2001:db8::1]:7".parse().unwrap()));
        assert_eq!(
            normalize("[::ffff:10.0.0.1]:7777".parse().unwrap()),
            "10.0.0.1:7777".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn allocator_skips_reserved() {
        let mut t = Tables::default();
        let first = MeshSocket::alloc_ip(&mut t);
        assert_eq!(first, Ipv4Addr::new(240, 0, 0, 1));
        let second = MeshSocket::alloc_ip(&mut t);
        assert_eq!(second, Ipv4Addr::new(240, 0, 0, 2));
        assert!(is_synthetic_ip(IpAddr::V4(first)));
    }
}
