//! Network partitions for in-process tests (E1-P1).
//!
//! ## Why this exists
//!
//! meshware's headline is *"a cluster scheduler with no control plane"*,
//! and the first thing a competent reviewer does with that claim is ask
//! what happens when the network splits. Until this module the honest
//! answer was that the mechanism is right — a signed op-log, LWW/OR-set
//! CRDTs, an HLC, and no quorum anywhere — and that **nothing exercised
//! it**. A grep for `partition` across the whole test tree returned one
//! comment. MASTERPLAN §16 already said so: *"the harness has no
//! latency/partition injection"*.
//!
//! An architectural claim with no test behind it is a slogan, which is the
//! same rule `cold-start.sh` applies to the operational-cost table.
//!
//! ## How it works
//!
//! The seam is the one M7 already built for NAT profiles: every node's
//! transport takes an optional [`SocketFactory`], and quinn will speak
//! through any `AsyncUdpSocket` we hand it. [`Netsplit`] hands out sockets
//! that are **real kernel sockets** — unlike [`crate::nat`], nothing is
//! black-holed and every address stays genuinely dialable — wrapped in a
//! shim that consults one shared table before each datagram leaves.
//!
//! Each node is assigned a **side**, an arbitrary `u8`. A datagram is
//! dropped when the table says *this side may not currently reach that
//! side*. That is all a partition is: a router that stops forwarding.
//!
//! ## Send-side filtering only, and why that is enough
//!
//! The shim filters on `try_send` and never on `poll_recv`. In a symmetric
//! split both halves hold their own shim and each blocks its own outbound
//! traffic, so nothing crosses in either direction — the same observable
//! behaviour as a dropped link, reached with far less machinery than
//! rewriting quinn's receive path.
//!
//! It also buys the **asymmetric** case for free, which matters because
//! asymmetric connectivity is a real failure mode and one the enterprise
//! review asks about by name. [`Netsplit::cut_one_way`] blocks A→B while
//! leaving B→A intact: a node that can still hear its peers but cannot be
//! heard. That is the shape that produces the nastiest membership bugs, and
//! it costs one directional entry rather than a second filter.
//!
//! ## What this is not
//!
//! **It is not deterministic time.** Tests here still wait on wall-clock
//! with [`crate::wait_until`], exactly as the rest of the testkit does.
//! Injecting a clock would mean threading one through the daemon, the
//! scheduler, the HLC and every lease in the design; it is worth doing and
//! it is not this. The consequence is that these tests are written around
//! *convergence*, which is a stable property, rather than around timing,
//! which is not.
//!
//! **It does not model latency or loss.** A packet either crosses or it
//! does not. Jitter and partial loss are a different harness and would want
//! deterministic time first.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

use mesh_core::transport::SocketFactory;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

/// A side of a potential split. Arbitrary; nodes on the same side always
/// reach each other.
pub type Side = u8;

/// The shared partition table plus the counters a test asserts on.
///
/// Cheap to clone (`Arc`), and every method is safe to call from any task
/// at any time — including while traffic is in flight, which is the point:
/// a test cuts the link *during* a job.
pub struct Netsplit {
    /// Directional blocks: `(from, to)` present means "dropped".
    cuts: RwLock<HashSet<(Side, Side)>>,
    /// Which side each bound socket address belongs to.
    sides: RwLock<HashMap<SocketAddr, Side>>,
    dropped: AtomicU64,
    delivered: AtomicU64,
    /// Sockets handed out, so `sides` can be filled in after bind.
    registered: Mutex<Vec<SocketAddr>>,
}

impl std::fmt::Debug for Netsplit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cuts = self.cuts.read().expect("cuts lock");
        write!(
            f,
            "Netsplit({} cut(s), {} dropped, {} delivered)",
            cuts.len(),
            self.dropped.load(Ordering::Relaxed),
            self.delivered.load(Ordering::Relaxed)
        )
    }
}

/// `127.0.0.1:x` and `::ffff:127.0.0.1:x` are the same endpoint; the table
/// is keyed on the v4 form so a lookup never misses on spelling. Same rule
/// [`crate::nat`] applies, for the same reason.
fn normalize(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => addr,
        },
        v4 => v4,
    }
}

impl Netsplit {
    pub fn new() -> Arc<Self> {
        Arc::new(Netsplit {
            cuts: RwLock::new(HashSet::new()),
            sides: RwLock::new(HashMap::new()),
            dropped: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            registered: Mutex::new(Vec::new()),
        })
    }

    /// A [`SocketFactory`] that puts every endpoint it builds on `side`.
    ///
    /// Pass the result to a node's `config.socket_factory`, or use
    /// [`crate::spawn_founder_split`] / [`crate::join_new_node_split`],
    /// which do it for you.
    pub fn factory(self: &Arc<Self>, side: Side) -> SocketFactory {
        let split = self.clone();
        SocketFactory(Arc::new(move |bind: SocketAddr| {
            split.new_socket(bind, side)
        }))
    }

    fn new_socket(
        self: &Arc<Self>,
        bind: SocketAddr,
        side: Side,
    ) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        use quinn::Runtime as _;
        // A real socket on a real port. Nothing here is a black hole: the
        // whole point is that addresses stay dialable and the *only*
        // variable is whether we forward.
        let std_sock = std::net::UdpSocket::bind(bind)?;
        let local = normalize(std_sock.local_addr()?);
        let inner = quinn::TokioRuntime.wrap_udp_socket(std_sock)?;
        self.sides.write().expect("sides lock").insert(local, side);
        self.registered.lock().expect("registered lock").push(local);
        Ok(Arc::new(SplitSocket {
            split: self.clone(),
            inner,
            side,
            local,
        }))
    }

    /// Would a datagram from `from` to `dest` be dropped right now?
    ///
    /// An address this harness has never handed out is **never blocked** —
    /// it belongs to something outside the simulated network, and silently
    /// swallowing its traffic would make an unrelated bug look like a
    /// partition.
    fn blocked(&self, from: Side, dest: SocketAddr) -> bool {
        let Some(&to) = self.sides.read().expect("sides lock").get(&normalize(dest)) else {
            return false;
        };
        if from == to {
            return false;
        }
        self.cuts.read().expect("cuts lock").contains(&(from, to))
    }

    /// Cut the link between two sides, both ways. The ordinary partition.
    pub fn cut(&self, a: Side, b: Side) {
        let mut cuts = self.cuts.write().expect("cuts lock");
        cuts.insert((a, b));
        cuts.insert((b, a));
    }

    /// Cut `from` → `to` only, leaving the reverse path intact.
    ///
    /// Asymmetric connectivity: a node whose packets stop arriving but
    /// which still hears everybody. Failure detectors find this much harder
    /// than a clean split, which is exactly why it is worth testing.
    pub fn cut_one_way(&self, from: Side, to: Side) {
        self.cuts.write().expect("cuts lock").insert((from, to));
    }

    /// Restore every link.
    pub fn heal(&self) {
        self.cuts.write().expect("cuts lock").clear();
    }

    /// Is anything currently cut?
    pub fn is_cut(&self) -> bool {
        !self.cuts.read().expect("cuts lock").is_empty()
    }

    /// `(dropped, delivered)` datagram counts since the harness started.
    ///
    /// `dropped` climbing while a partition is up is how a test proves the
    /// cut is actually doing something rather than the mesh having gone
    /// quiet for an unrelated reason — a distinction worth an assertion,
    /// because a test that passes on a silent network proves nothing.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.dropped.load(Ordering::Relaxed),
            self.delivered.load(Ordering::Relaxed),
        )
    }

    /// Every address this harness has bound, in bind order.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.registered.lock().expect("registered lock").clone()
    }

    /// Move an already-bound node to a different side.
    ///
    /// For the rejoin case: park a node alone, let the mesh move on, then
    /// put it back and watch it catch up.
    pub fn set_side(&self, addr: SocketAddr, side: Side) {
        self.sides
            .write()
            .expect("sides lock")
            .insert(normalize(addr), side);
    }
}

struct SplitSocket {
    split: Arc<Netsplit>,
    inner: Arc<dyn AsyncUdpSocket>,
    side: Side,
    local: SocketAddr,
}

impl std::fmt::Debug for SplitSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SplitSocket({} side {})", self.local, self.side)
    }
}

#[derive(Debug)]
struct InnerPoller(Pin<Box<dyn UdpPoller>>);

impl UdpPoller for InnerPoller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.0.as_mut().poll_writable(cx)
    }
}

impl AsyncUdpSocket for SplitSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(InnerPoller(self.inner.clone().create_io_poller()))
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        // The side this node currently sits on, not the one it was built
        // with: `set_side` must take effect on a socket already in use.
        let side = self
            .split
            .sides
            .read()
            .expect("sides lock")
            .get(&self.local)
            .copied()
            .unwrap_or(self.side);

        if self.split.blocked(side, transmit.destination) {
            // UDP's own semantics for a dropped datagram: the send
            // *succeeded*, the packet just never arrives. Returning an
            // error here would surface as a connection error rather than
            // as loss, and quinn would react to the wrong thing.
            self.split.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        self.split.delivered.fetch_add(1, Ordering::Relaxed);
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Deliberately unfiltered — see the module docs. Both halves of a
        // split block their own sends, so nothing crosses to be received.
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
