//! QUIC transport with key-pinned TLS 1.3 (MASTERPLAN §6).
//!
//! There is no CA. Each node presents a self-signed X.509 cert whose SPKI is
//! its Ed25519 node key; custom rustls verifiers accept exactly that shape and
//! surface the key as the peer's [`NodeId`]. TLS proves possession of the key;
//! the application layer decides what that identity may do.
//!
//! Connection lifecycle: the dialer opens one handshake stream and exchanges
//! `ConnHello`/`ConnHelloAck` once. Every later bi-stream carries one
//! `Request` frame and one or more `Response` frames. Business logic lives in
//! the daemon: inbound requests are forwarded as [`RequestEvent`]s over a
//! channel, keeping the transport free of state concerns.
//!
//! WAN (MASTERPLAN M7, protocol 1.5): both sides keep the negotiated minor;
//! accepted member connections from 1.5 peers are **pooled** like dialed
//! ones and either side may open request streams on any pooled connection
//! (a NATed node's outbound connection is how a public peer reaches it).
//! `member_conn` is a tiered racing dialer — direct candidates staggered,
//! a coordinated punch through the peer's relay, then a relayed path — with
//! a per-peer dial-state cache so gossip ticks stay cheap. The single UDP
//! socket is wrapped in a [`MeshSocket`] so relayed paths appear as
//! synthetic Class E addresses to the inner key-pinned QUIC session.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio::sync::{Notify, mpsc, watch};
use tracing::{debug, info, warn};

use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::meshsock::{self, MeshSocket};
use crate::relay::RelayHub;
use mesh_proto::{
    ConnHello, ConnHelloAck, ConnPurpose, MeshId, NodeId, Ping, PingResult, RelayFrame,
    RelayServerFrame, Request, Response, WAN_MINOR, parse_relay_candidate, read_frame,
    read_frame_limited, write_frame,
};

/// SNI placeholder — verification is key-based, the name is irrelevant.
const SERVER_NAME: &str = "meshware";

/// How long a connection may carry nothing before it is declared dead, and
/// how often each end says it is still there (ENG-6). quinn's default idle
/// timeout is 30 s, and until it ran out a crashed peer's pooled connection
/// looked open: every request sent on it waited the whole 30 s to fail.
/// Both ends send keep-alives, so an idle but healthy connection is never
/// the one that times out.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(3);

fn liveness_config(transport: &mut quinn::TransportConfig) {
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    if let Ok(idle) = quinn::IdleTimeout::try_from(IDLE_TIMEOUT) {
        transport.max_idle_timeout(Some(idle));
    }
}

/// Handshake timeout for unpinned dials (join, probe, pair) and the inner
/// relayed session.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-candidate timeout inside the direct race (MASTERPLAN M7): long
/// enough for a WAN handshake, short enough that a black-holed LAN
/// candidate does not hold the dial hostage.
const DIRECT_DIAL_TIMEOUT: Duration = Duration::from_secs(2);
/// Stagger between direct candidates (happy-eyeballs style).
const DIAL_STAGGER: Duration = Duration::from_millis(250);
/// How long to wait for the peer's dial-back after a punch request.
const PUNCH_WINDOW: Duration = Duration::from_secs(3);
/// Dial-back bounds on the punched side: per candidate / overall. The
/// per-candidate bound leaves room for QUIC's first Initial retransmit
/// (the first packet through a freshly punched NAT is often the one that
/// opens the filter, not the one that gets through).
const PUNCH_BACK_TIMEOUT: Duration = Duration::from_secs(2);
const PUNCH_BACK_WINDOW: Duration = Duration::from_secs(3);
const PUNCH_BACK_STAGGER: Duration = Duration::from_millis(100);
const MAX_PUNCH_CANDIDATES: usize = 8;
/// After a full dial fails, callers fail fast for this long (gossip ticks
/// and verdict probes must not each pay the whole race).
const NEGATIVE_TTL: Duration = Duration::from_secs(5);
/// Bounded outbound connection pool (MASTERPLAN D12): enough for gossip +
/// active job streams at any mesh size; colder peers are re-dialed on use.
const MAX_POOLED_CONNS: usize = 64;
/// Ceiling on the membership-scaled pool (**M-2**).
///
/// A file descriptor and some QUIC state per entry, so this is generous
/// rather than unbounded — a thousand-node mesh keeps a thousand warm
/// connections only if it has actually talked to a thousand peers.
const MAX_POOLED_CONNS_CEILING: usize = 1024;

/// Connections whose negotiated minor is remembered at once (ENG-24).
///
/// Above the pool's ceiling, because non-pooled connections — joins,
/// pairings, probes, relayed paths — are remembered too. Forgetting one
/// costs a check that did not exist before this, so the bound is a memory
/// bound and not a correctness one.
const MAX_REMEMBERED_MINORS: usize = 4096;
/// Frames parked per relay stream before drop-on-full (UDP semantics).
const RELAY_STREAM_QUEUE: usize = 512;

// -- what a stranger may cost (SEC-10) ---------------------------------------
//
// The listener ran on quinn's defaults: no cap on connections overall or
// from one address, no deadline on the handshake, a hundred streams a
// connection, each allowed a 4 MiB frame before anything looked at what it
// was for. Anyone who can reach UDP 7777 could spend this node's memory and
// CPU. Members are bounded by membership; everyone else is bounded here.

/// Handshakes in flight at once. A connection past this is refused before
/// any cryptography is done for it.
const MAX_PENDING_HANDSHAKES: usize = 256;
/// How long a peer has to finish the TLS handshake and send its hello.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Open connections from peers that are not members of this mesh.
const MAX_STRANGER_CONNS: usize = 512;
/// …of which from one address. Joining takes one; a NAT with many machines
/// behind it that joined moments ago takes one each until this node merges
/// their admission.
const MAX_STRANGER_CONNS_PER_IP: usize = 32;
/// Concurrent streams a stranger's connection may open.
const STRANGER_STREAMS: u32 = 4;
/// Largest frame read from a stranger: a join, a pairing step, a ping.
const STRANGER_FRAME_LEN: u32 = 256 * 1024;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// What this node announces about itself in handshakes.
#[derive(Debug, Clone)]
pub struct LocalInfo {
    pub node_id: NodeId,
    pub node_name: String,
    pub mesh_id: Option<MeshId>,
    pub mesh_name: Option<String>,
}

/// How a connection reaches its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnKind {
    /// A real UDP path.
    Direct,
    /// Through a relay member (synthetic address on this end).
    Relayed,
}

fn kind_of(addr: SocketAddr) -> ConnKind {
    if meshsock::is_synthetic(addr) {
        ConnKind::Relayed
    } else {
        ConnKind::Direct
    }
}

/// Per-connection peer context, established at handshake.
#[derive(Debug, Clone)]
pub struct PeerCtx {
    pub node_id: NodeId,
    pub node_name: String,
    pub remote_addr: SocketAddr,
    pub purpose: ConnPurpose,
    /// Negotiated protocol minor (`min(ours, theirs)`).
    pub minor: u16,
    pub kind: ConnKind,
}

/// An inbound request, dispatched to the daemon. Send zero or more
/// responses through `reply`; dropping it ends the response stream.
#[derive(Debug)]
pub struct RequestEvent {
    pub peer: PeerCtx,
    pub request: Request,
    pub reply: mpsc::Sender<Response>,
}

/// Application-layer admission control, supplied by the daemon: may this
/// node open a connection with this purpose?
pub type AdmissionCheck =
    Arc<dyn Fn(&NodeId, ConnPurpose) -> std::result::Result<(), String> + Send + Sync>;

/// Resolves a member's candidate addresses (the transport needs a relay's
/// direct addresses to follow a `relay:<id>` candidate; the daemon answers
/// from replicated state).
pub type MemberLookup = Arc<dyn Fn(&NodeId) -> Option<Vec<String>> + Send + Sync>;

/// Handles an inbound punch request delivered over a relay registration:
/// `(from, candidates)`. The WAN layer validates membership and dials back.
pub type PunchHandler = Arc<dyn Fn(NodeId, Vec<String>) + Send + Sync>;

/// Takes over a stream as a session (M17 §3.3).
///
/// The `set_punch_handler` idiom (R3) applied to the `serve_register` shape
/// (R2): the transport stays free of state concerns — it does not know what
/// a shell is, what RBAC means, or which tags refuse one — and the daemon
/// supplies all of it. The handler is given **both halves** of the stream
/// and owns them for the session's life, which is exactly what
/// `RequestEvent`'s one-way reply channel cannot express and why a session
/// is not an ordinary request.
pub type SessionHandler = Arc<
    dyn Fn(PeerCtx, mesh_proto::SessionSpec, quinn::SendStream, quinn::RecvStream) + Send + Sync,
>;

/// What a node on this build says to `Request::ServiceConnect`, the retired
/// mesh proxy's hand-off. An older node's proxy reads it as a refused
/// instance and moves on to dialling the instance directly.
pub const SERVICE_CONNECT_RETIRED: &str = "this node no longer runs the mesh proxy — \
     reach the instance at its node's address and port";

/// Builds the UDP socket an endpoint binds on. Production uses the kernel
/// socket; `mesh-testkit` installs userspace NATs through this seam.
#[derive(Clone)]
pub struct SocketFactory(
    pub Arc<dyn Fn(SocketAddr) -> std::io::Result<Arc<dyn quinn::AsyncUdpSocket>> + Send + Sync>,
);

impl std::fmt::Debug for SocketFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SocketFactory")
    }
}

/// Options for [`Transport::bind`].
#[derive(Debug, Clone, Default)]
pub struct BindOptions {
    pub socket_factory: Option<SocketFactory>,
    /// Advertise this protocol minor instead of the real one (tests:
    /// simulate an older peer). Never set in production.
    pub advertised_minor: Option<u16>,
}

/// A pooled member connection.
#[derive(Debug, Clone)]
pub struct PooledConn {
    pub conn: quinn::Connection,
    pub minor: u16,
    pub kind: ConnKind,
    /// Held by a long-lived user (relay registration): never evicted under
    /// pool pressure.
    pub pinned: bool,
    /// Accepted rather than dialed.
    pub inbound: bool,
    /// When this connection was last handed out (**M-2**).
    ///
    /// The eviction key. quinn does not expose a live stream count, so
    /// "busy" is approximated by recency — and it approximates it well,
    /// because the things that must not be cut (a followed log, a bundle
    /// transfer, an open session) are precisely the things that reach for
    /// their connection constantly, while a gossip partner from four ticks
    /// ago does not.
    pub last_used: Instant,
}

/// The route that last reached a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialRoute {
    Direct(SocketAddr),
    Relay(NodeId),
}

#[derive(Debug, Default, Clone)]
struct DialState {
    last_good: Option<DialRoute>,
    /// When the last full dial failed, and against which candidate set
    /// (sorted). A changed set — a newly learned relay or reflexive
    /// candidate — is always worth a fresh try.
    last_failure: Option<(Instant, Vec<String>)>,
}

fn sorted(addrs: &[String]) -> Vec<String> {
    let mut v = addrs.to_vec();
    v.sort();
    v.dedup();
    v
}

// ---------------------------------------------------------------------------
// Certificate plumbing
// ---------------------------------------------------------------------------

fn make_cert(key: &Keypair) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let pkcs8 = key.to_pkcs8_der()?;
    let rcgen_key = rcgen::KeyPair::try_from(pkcs8.as_slice())
        .map_err(|e| CoreError::Tls(format!("keypair import: {e}")))?;
    let params = rcgen::CertificateParams::new(vec![SERVER_NAME.to_string()])
        .map_err(|e| CoreError::Tls(format!("cert params: {e}")))?;
    let cert = params
        .self_signed(&rcgen_key)
        .map_err(|e| CoreError::Tls(format!("self-sign: {e}")))?;
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(pkcs8)).clone_key();
    Ok((cert.der().clone(), key_der))
}

fn node_id_from_cert(cert: &CertificateDer<'_>) -> std::result::Result<NodeId, String> {
    use x509_parser::prelude::*;
    let (_, parsed) =
        X509Certificate::from_der(cert.as_ref()).map_err(|e| format!("bad certificate: {e}"))?;
    let spki = parsed.public_key();
    if spki.algorithm.algorithm != x509_parser::oid_registry::OID_SIG_ED25519 {
        return Err("peer certificate is not Ed25519".to_string());
    }
    let key_bytes: &[u8] = spki.subject_public_key.data.as_ref();
    let arr: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| format!("unexpected Ed25519 key length {}", key_bytes.len()))?;
    Ok(NodeId(arr))
}

fn peer_node_id(conn: &quinn::Connection) -> Result<NodeId> {
    let identity = conn
        .peer_identity()
        .ok_or_else(|| CoreError::Transport("peer presented no certificate".into()))?;
    let certs = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| CoreError::Transport("unexpected peer identity type".into()))?;
    let leaf = certs
        .first()
        .ok_or_else(|| CoreError::Transport("empty peer certificate chain".into()))?;
    node_id_from_cert(leaf).map_err(CoreError::Transport)
}

// ---------------------------------------------------------------------------
// Verifiers
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct KeyVerifier {
    provider: Arc<CryptoProvider>,
    expected: Option<NodeId>,
}

impl KeyVerifier {
    fn check(&self, end_entity: &CertificateDer<'_>) -> std::result::Result<(), rustls::Error> {
        let id = node_id_from_cert(end_entity)
            .map_err(|e| rustls::Error::General(format!("meshware identity: {e}")))?;
        if let Some(expected) = self.expected
            && expected != id
        {
            return Err(rustls::Error::General(format!(
                "peer key mismatch: expected {}, got {}",
                expected.short(),
                id.short()
            )));
        }
        Ok(())
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ServerCertVerifier for KeyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for KeyVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

fn build_server_config(
    key: &Keypair,
    provider: Arc<CryptoProvider>,
) -> Result<quinn::ServerConfig> {
    let (cert, key_der) = make_cert(key)?;
    let verifier = Arc::new(KeyVerifier {
        provider: provider.clone(),
        expected: None,
    });
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| CoreError::Tls(format!("tls versions: {e}")))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert], key_der)
        .map_err(|e| CoreError::Tls(format!("server tls: {e}")))?;
    tls.alpn_protocols = vec![mesh_proto::ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls)
        .map_err(|e| CoreError::Tls(format!("quic server config: {e}")))?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    // Every inbound connection starts with a stranger's stream budget
    // (SEC-10), raised once the peer is known to be a member. It has to
    // start low: QUIC stream credit is cumulative, and a limit lowered after
    // the handshake does not take back the hundred streams already granted.
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(STRANGER_STREAMS.into());
    transport.max_concurrent_uni_streams(0u32.into());
    liveness_config(&mut transport);
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
}

/// Streams a member's connection may have open at once (ENG-17).
///
/// quinn's default was a hundred, and a followed remote job holds a stream
/// for its whole life on the same pooled connection gossip uses. So a
/// hundred jobs followed from one node on one big executor used up the
/// pair's credit, and that pair's gossip and every new submission between
/// them waited behind the hundredth. Set on both ends — the accepting side
/// when a member connects, the dialing side for the streams a peer opens
/// back — once the peer is known to be a member; a stranger keeps its
/// handful (SEC-10).
const MEMBER_STREAMS: u32 = 4096;

fn build_client_config(
    key: &Keypair,
    provider: Arc<CryptoProvider>,
    expected: Option<NodeId>,
) -> Result<quinn::ClientConfig> {
    let (cert, key_der) = make_cert(key)?;
    let verifier = Arc::new(KeyVerifier {
        provider: provider.clone(),
        expected,
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| CoreError::Tls(format!("tls versions: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert], key_der)
        .map_err(|e| CoreError::Tls(format!("client tls: {e}")))?;
    tls.alpn_protocols = vec![mesh_proto::ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls)
        .map_err(|e| CoreError::Tls(format!("quic client config: {e}")))?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    let mut transport = quinn::TransportConfig::default();
    liveness_config(&mut transport);
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

pub struct Transport {
    endpoint: quinn::Endpoint,
    sock: Arc<MeshSocket>,
    key: Keypair,
    local: LocalInfo,
    provider: Arc<CryptoProvider>,
    admission: AdmissionCheck,
    events_tx: mpsc::Sender<RequestEvent>,
    pool: Mutex<HashMap<NodeId, PooledConn>>,
    /// How many connections the pool may hold (**M-2**).
    ///
    /// Fixed at [`MAX_POOLED_CONNS`] until membership says otherwise.
    /// Gossip picks a **random** partner each tick, so with N members and a
    /// fixed pool of 64 the chance the chosen partner is already pooled
    /// falls as 64/N — about a third at 200 members, an eighth at 500 — and
    /// every miss is a fresh QUIC handshake *and* a forced close of
    /// whatever was evicted to make room. Sizing the pool to the mesh turns
    /// that back into a hit.
    pool_target: std::sync::atomic::AtomicUsize,
    /// Fires whenever the pool gains an entry (the punch window waits on it).
    pool_changed: Notify,
    dial_state: Mutex<HashMap<NodeId, DialState>>,
    /// Single-flight per peer: concurrent callers share one race.
    dial_locks: Mutex<HashMap<NodeId, Arc<tokio::sync::Mutex<()>>>>,
    relay: Arc<RelayHub>,
    members: Mutex<Option<MemberLookup>>,
    /// Handshake permits (SEC-10).
    handshakes: Arc<tokio::sync::Semaphore>,
    /// Open stranger connections, overall and per address (SEC-10).
    strangers: Arc<Mutex<StrangerCount>>,
    punch_handler: Mutex<Option<PunchHandler>>,
    session_handler: Mutex<Option<SessionHandler>>,
    /// Our own direct candidates, sent in `PunchRequest`s.
    own_candidates: Mutex<Vec<String>>,
    advertised_minor: AtomicU16,
    /// The protocol minor negotiated on each live connection, by quinn's
    /// stable id (ENG-24).
    ///
    /// **Why a second map when the pool already records a minor.** The
    /// pool is keyed by *member*, and half the connections that carry
    /// requests are not in it — a join, a pairing exchange, a probe, a
    /// relayed path — while `request_one` is handed a `Connection` and
    /// nothing else. This is the one place that can answer "what does the
    /// peer on the other end of *this* connection understand" for every
    /// caller, which is what makes a single check possible instead of a
    /// sender-side `if minor >= …` at each of the thirty-odd call sites.
    ///
    /// Bounded and pruned oldest-first rather than tied to connection
    /// lifetimes, because quinn exposes no hook for a closed connection
    /// and no user data to hang this on. A missing entry skips the check,
    /// which is what every one of those call sites did before this — so a
    /// pruned entry costs a check, never a wrong answer.
    minors: Mutex<HashMap<usize, (u16, Instant)>>,
    self_ref: OnceLock<Weak<Transport>>,
    /// The accept loop's pulse, for the node's [`crate::vitals::Vitals`]
    /// (ENG-2): a transport that has stopped accepting is a node nobody
    /// can reach, however healthy the rest of it looks.
    accept_monitor: crate::vitals::Monitor,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Transport(node={})", self.local.node_id.short())
    }
}

/// A stream of responses from a peer (e.g. FollowJob).
#[derive(Debug)]
pub struct ResponseStream {
    recv: quinn::RecvStream,
}

impl ResponseStream {
    /// Next response frame; `None` on clean end-of-stream.
    pub async fn next(&mut self) -> Result<Option<Response>> {
        match read_frame::<_, Response>(&mut self.recv).await {
            Ok(resp) => Ok(Some(resp)),
            Err(mesh_proto::ProtoError::Io(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// A live relay registration (MASTERPLAN M7): the stream stays open until
/// dropped/closed; frames for us arrive through it, frames we send to any
/// peer via that relay leave on it.
#[derive(Debug)]
pub struct RelayRegistration {
    pub relay: NodeId,
    done: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl RelayRegistration {
    /// Still serving frames?
    pub fn is_active(&self) -> bool {
        !*self.done.borrow() && !self.task.is_finished()
    }

    /// Tear the registration down.
    pub fn close(&self) {
        self.task.abort();
    }
}

impl Drop for RelayRegistration {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Open stranger connections (SEC-10).
#[derive(Debug, Default)]
struct StrangerCount {
    total: usize,
    by_ip: HashMap<std::net::IpAddr, usize>,
}

/// One stranger connection's place in the count, given back when it ends.
struct StrangerSlot {
    count: Arc<Mutex<StrangerCount>>,
    ip: std::net::IpAddr,
}

impl StrangerSlot {
    fn take(count: &Arc<Mutex<StrangerCount>>, ip: std::net::IpAddr) -> Option<StrangerSlot> {
        let mut c = count.lock();
        let here = c.by_ip.get(&ip).copied().unwrap_or(0);
        if c.total >= MAX_STRANGER_CONNS || here >= MAX_STRANGER_CONNS_PER_IP {
            return None;
        }
        c.total += 1;
        c.by_ip.insert(ip, here + 1);
        Some(StrangerSlot {
            count: count.clone(),
            ip,
        })
    }
}

impl Drop for StrangerSlot {
    fn drop(&mut self) {
        let mut c = self.count.lock();
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.by_ip.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.by_ip.remove(&self.ip);
            }
        }
    }
}

impl Transport {
    /// Bind and start accepting. Inbound requests are delivered on the
    /// returned receiver; the daemon owns dispatch.
    pub async fn bind(
        bind: SocketAddr,
        key: Keypair,
        local: LocalInfo,
        admission: AdmissionCheck,
        opts: BindOptions,
    ) -> Result<(Arc<Self>, mpsc::Receiver<RequestEvent>)> {
        use quinn::Runtime as _;
        let provider = provider();
        let server_config = build_server_config(&key, provider.clone())?;
        let raw: Arc<dyn quinn::AsyncUdpSocket> = match &opts.socket_factory {
            Some(factory) => (factory.0)(bind)
                .map_err(|e| CoreError::Transport(format!("bind {bind} (factory): {e}")))?,
            None => {
                let std_sock = std::net::UdpSocket::bind(bind)
                    .map_err(|e| CoreError::Transport(format!("bind {bind}: {e}")))?;
                quinn::TokioRuntime
                    .wrap_udp_socket(std_sock)
                    .map_err(|e| CoreError::Transport(format!("socket setup {bind}: {e}")))?
            }
        };
        let bound = raw
            .local_addr()
            .map_err(|e| CoreError::Transport(format!("local_addr: {e}")))?;
        let sock = Arc::new(MeshSocket::new(raw, bound.is_ipv6()));
        let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
        let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(server_config),
            sock.clone(),
            runtime,
        )
        .map_err(|e| CoreError::Transport(format!("endpoint {bind}: {e}")))?;
        endpoint.set_default_client_config(build_client_config(&key, provider.clone(), None)?);
        let (events_tx, events_rx) = mpsc::channel(256);
        let accept_pulse = crate::vitals::Pulse::detached("transport accept");
        let transport = Arc::new(Self {
            endpoint,
            sock,
            key,
            local,
            provider,
            admission,
            events_tx,
            pool: Mutex::new(HashMap::new()),
            minors: Mutex::new(HashMap::new()),
            pool_target: std::sync::atomic::AtomicUsize::new(MAX_POOLED_CONNS),
            pool_changed: Notify::new(),
            dial_state: Mutex::new(HashMap::new()),
            dial_locks: Mutex::new(HashMap::new()),
            relay: Arc::new(RelayHub::new()),
            members: Mutex::new(None),
            punch_handler: Mutex::new(None),
            session_handler: Mutex::new(None),
            handshakes: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES)),
            strangers: Arc::new(Mutex::new(StrangerCount::default())),
            own_candidates: Mutex::new(Vec::new()),
            advertised_minor: AtomicU16::new(
                opts.advertised_minor.unwrap_or(mesh_proto::PROTOCOL_MINOR),
            ),
            self_ref: OnceLock::new(),
            accept_monitor: accept_pulse.monitor(),
        });
        let _ = transport.self_ref.set(Arc::downgrade(&transport));
        transport.clone().spawn_accept_loop(accept_pulse);
        Ok((transport, events_rx))
    }

    /// The accept loop's pulse (ENG-2).
    pub fn accept_monitor(&self) -> crate::vitals::Monitor {
        self.accept_monitor.clone()
    }

    fn arc(&self) -> Option<Arc<Self>> {
        self.self_ref.get().and_then(Weak::upgrade)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .map_err(|e| CoreError::Transport(format!("local_addr: {e}")))
    }

    pub fn node_id(&self) -> NodeId {
        self.local.node_id
    }

    /// The protocol minor this node advertises (the real one unless a test
    /// simulates an older peer).
    pub fn advertised_minor(&self) -> u16 {
        self.advertised_minor.load(Ordering::Relaxed)
    }

    /// The relay hub (enabled by the WAN layer when this node is a relay).
    pub fn relay(&self) -> &Arc<RelayHub> {
        &self.relay
    }

    /// The socket wrapper (relay data-plane counters + routes).
    pub fn meshsock(&self) -> &Arc<MeshSocket> {
        &self.sock
    }

    /// Install the member-address resolver used to follow `relay:<id>`
    /// candidates.
    pub fn set_member_lookup(&self, lookup: MemberLookup) {
        *self.members.lock() = Some(lookup);
    }

    /// Is `id` an active member, as far as the daemon has told this
    /// transport? A transport with no daemon behind it knows no members.
    fn is_member(&self, id: &NodeId) -> bool {
        let lookup = self.members.lock().clone();
        lookup.is_some_and(|l| l(id).is_some())
    }

    /// Install the handler for inbound punch requests (relay registrations
    /// deliver them).
    pub fn set_punch_handler(&self, handler: PunchHandler) {
        *self.punch_handler.lock() = Some(handler);
    }

    /// Install the handler that takes over a stream as a session (M17
    /// §3.3). Absent it — a node built without the daemon layer, or one
    /// still starting — `SessionOpen` is refused by name rather than
    /// hanging.
    pub fn set_session_handler(&self, handler: SessionHandler) {
        *self.session_handler.lock() = Some(handler);
    }

    /// Our own direct candidates, offered to peers in punch requests.
    pub fn set_own_candidates(&self, candidates: Vec<String>) {
        *self.own_candidates.lock() = candidates;
    }

    fn lookup_member(&self, id: &NodeId) -> Option<Vec<String>> {
        let lookup = self.members.lock().clone();
        lookup.and_then(|f| f(id))
    }

    /// Why a relay service (register / open / punch) must refuse this
    /// peer, if it must: relayed connections cannot chain, and only active
    /// members (per the daemon's lookup, when installed) are served.
    fn relay_refusal(&self, peer: &PeerCtx) -> Option<String> {
        if peer.kind == ConnKind::Relayed {
            return Some("relay services are not available over a relayed connection".into());
        }
        let has_lookup = self.members.lock().is_some();
        if has_lookup && self.lookup_member(&peer.node_id).is_none() {
            return Some("relay services are for active mesh members".into());
        }
        None
    }

    fn hello(&self, purpose: ConnPurpose) -> ConnHello {
        ConnHello {
            proto_major: mesh_proto::PROTOCOL_MAJOR,
            proto_minor: self.advertised_minor(),
            purpose,
            node_id: self.local.node_id,
            node_name: self.local.node_name.clone(),
            mesh_id: self.local.mesh_id,
            mesh_name: self.local.mesh_name.clone(),
        }
    }

    // -- pool --------------------------------------------------------------

    fn pooled_open(&self, node_id: &NodeId) -> Option<PooledConn> {
        let mut pool = self.pool.lock();
        let entry = pool.get_mut(node_id)?;
        if entry.conn.close_reason().is_some() {
            return None;
        }
        // Every handout is a use (**M-2**), which is what makes the LRU
        // victim below the genuinely coldest connection.
        entry.last_used = Instant::now();
        Some(entry.clone())
    }

    /// The pooled connection to a member, if one is open (no dialing).
    pub fn pooled_conn(&self, node_id: &NodeId) -> Option<PooledConn> {
        self.pooled_open(node_id)
    }

    /// Negotiated protocol minor with a member, from its pooled connection.
    pub fn peer_minor(&self, node_id: &NodeId) -> Option<u16> {
        self.pooled_open(node_id).map(|c| c.minor)
    }

    /// Remember what the peer on `conn` negotiated (ENG-24). Called once
    /// per handshake, on both the dialling and the accepting side.
    fn note_minor(&self, conn: &quinn::Connection, minor: u16) {
        let mut minors = self.minors.lock();
        if minors.len() >= MAX_REMEMBERED_MINORS {
            // Oldest half, in one pass: pruning one entry per insert on a
            // full map would make every handshake pay for a scan.
            let mut ages: Vec<Instant> = minors.values().map(|(_, at)| *at).collect();
            ages.sort_unstable();
            if let Some(cutoff) = ages.get(ages.len() / 2).copied() {
                minors.retain(|_, (_, at)| *at >= cutoff);
            }
        }
        minors.insert(conn.stable_id(), (minor, Instant::now()));
    }

    /// What the peer on `conn` negotiated, if this node still remembers.
    fn conn_minor(&self, conn: &quinn::Connection) -> Option<u16> {
        self.minors.lock().get(&conn.stable_id()).map(|(m, _)| *m)
    }

    /// Refuse a request the peer could not decode, before it is sent
    /// (ENG-24).
    ///
    /// **The one place this is checked.** The receiver has always dropped
    /// the stream on a too-new request, which is correct and useless to
    /// diagnose: the sender sees a reset or a timeout and has no idea a
    /// version is the reason. The senders checked too — in about thirty
    /// separate `if minor >= SOME_MINOR` guards, each one a place to
    /// forget a check when a request variant is added, and `min_minor()`
    /// already knows the answer for every variant in the enum.
    ///
    /// Silent when the minor is unknown, which is what those call sites
    /// did anyway.
    fn refuse_if_too_old(&self, conn: &quinn::Connection, req: &Request) -> Result<()> {
        let Some(minor) = self.conn_minor(conn) else {
            return Ok(());
        };
        let needs = req.min_minor();
        if needs <= minor {
            return Ok(());
        }
        Err(CoreError::PeerTooOld(format!(
            "this request needs protocol 1.{needs} and the peer negotiated 1.{minor} — upgrade it, or see the version-skew window in docs/operating/upgrade.md"
        )))
    }

    /// How the pooled connection to a member reaches it.
    pub fn conn_kind(&self, node_id: &NodeId) -> Option<ConnKind> {
        self.pooled_open(node_id).map(|c| c.kind)
    }

    /// The pool's current ceiling (**M-2**).
    fn pool_cap(&self) -> usize {
        self.pool_target
            .load(std::sync::atomic::Ordering::Relaxed)
            .clamp(MAX_POOLED_CONNS, MAX_POOLED_CONNS_CEILING)
    }

    /// Tell the pool how big the mesh is (**M-2**).
    ///
    /// Called from the gossip tick, which already counts dialable peers.
    /// One entry per member is the point: a pool smaller than the set
    /// gossip draws from cannot stop thrashing, whatever its eviction
    /// policy.
    pub fn set_pool_target(&self, members: usize) {
        self.pool_target
            .store(members, std::sync::atomic::Ordering::Relaxed);
    }

    /// Mark the pooled connection to `node_id` as pinned (never evicted).
    pub fn pin_conn(&self, node_id: &NodeId, pinned: bool) {
        if let Some(c) = self.pool.lock().get_mut(node_id) {
            c.pinned = pinned;
        }
    }

    /// Insert into the pool under the duplicate-connection rule: keep the
    /// existing entry unless it is closed or the new connection is Direct
    /// replacing Relayed (or pins an unpinned entry). Returns whether the
    /// new connection became the pool entry.
    fn pool_insert(&self, node_id: NodeId, new: PooledConn) -> bool {
        let inserted = {
            let mut pool = self.pool.lock();
            if let Some(existing) = pool.get(&node_id) {
                let open = existing.conn.close_reason().is_none();
                let upgrade = existing.kind == ConnKind::Relayed && new.kind == ConnKind::Direct;
                if open && !upgrade {
                    return false;
                }
                if open && upgrade {
                    // The relayed path is superseded; free the relay's
                    // resources. Either end closing suffices.
                    existing
                        .conn
                        .close(0u32.into(), b"superseded by direct path");
                    self.sock.drop_route_addr(existing.conn.remote_address());
                }
            } else if pool.len() >= self.pool_cap() {
                // Bounded pool (MASTERPLAN D12): evict closed conns first,
                // then the **least recently used** unpinned victim
                // (**M-2**).
                //
                // It used to take whichever unpinned entry the hash map
                // happened to yield first, which could just as easily be
                // the connection carrying a log somebody is watching as the
                // idle one — `close(.., b"pool full")` cuts the stream
                // either way. Past ~64 members, random-partner gossip made
                // that a coin flip every tick.
                pool.retain(|_, c| c.conn.close_reason().is_none());
                if pool.len() >= self.pool_cap()
                    && let Some(victim) = pool
                        .iter()
                        .filter(|(k, c)| **k != node_id && !c.pinned)
                        .min_by_key(|(_, c)| c.last_used)
                        .map(|(k, _)| *k)
                    && let Some(old) = pool.remove(&victim)
                {
                    old.conn.close(0u32.into(), b"pool full");
                    if old.kind == ConnKind::Relayed {
                        self.sock.drop_route_addr(old.conn.remote_address());
                    }
                }
            }
            pool.insert(node_id, new);
            true
        };
        if inserted {
            self.dial_state
                .lock()
                .entry(node_id)
                .or_default()
                .last_failure = None;
            self.pool_changed.notify_waiters();
        }
        inserted
    }

    /// Adopt a freshly dialed member connection: pool it (or yield to an
    /// existing entry) and, for 1.5 peers, serve the streams it may open
    /// toward us. Returns the connection callers should use.
    fn adopt_outbound(
        &self,
        node_id: NodeId,
        conn: quinn::Connection,
        ack: &ConnHelloAck,
        minor: u16,
        pinned: bool,
    ) -> quinn::Connection {
        let kind = kind_of(conn.remote_address());
        let entry = PooledConn {
            conn: conn.clone(),
            minor,
            kind,
            pinned,
            inbound: false,
            last_used: Instant::now(),
        };
        if !self.pool_insert(node_id, entry) {
            if let Some(existing) = self.pooled_open(&node_id) {
                // A concurrent (punched/inbound) connection won; don't leak
                // ours — keep-alives would hold it open forever. A pin
                // request transfers to the survivor.
                conn.close(0u32.into(), b"duplicate");
                if kind == ConnKind::Relayed {
                    self.sock.drop_route_addr(conn.remote_address());
                }
                if pinned {
                    self.pin_conn(&node_id, true);
                }
                return existing.conn;
            }
            // Closed entry raced us; take over.
            self.pool.lock().insert(
                node_id,
                PooledConn {
                    conn: conn.clone(),
                    minor,
                    kind,
                    pinned,
                    inbound: false,
                    last_used: Instant::now(),
                },
            );
            self.pool_changed.notify_waiters();
        }
        if minor >= WAN_MINOR
            && let Some(this) = self.arc()
        {
            let peer = PeerCtx {
                node_id,
                node_name: ack.node_name.clone(),
                remote_addr: conn.remote_address(),
                purpose: ConnPurpose::Member,
                minor,
                kind,
            };
            tokio::spawn(this.serve_streams(conn.clone(), peer));
        }
        conn
    }

    /// Close every pooled connection whose relayed path runs through
    /// `relay` (its registration or open stream ended): the inner session
    /// would otherwise linger until its idle timeout.
    fn close_relayed_via(&self, relay: &NodeId) {
        let victims: Vec<NodeId> = {
            let pool = self.pool.lock();
            pool.iter()
                .filter(|(_, c)| c.kind == ConnKind::Relayed)
                .filter(|(_, c)| {
                    self.sock
                        .route_by_addr(c.conn.remote_address())
                        .is_some_and(|(r, _)| r == *relay)
                })
                .map(|(id, _)| *id)
                .collect()
        };
        for id in victims {
            debug!(peer = %id.short(), relay = %relay.short(), "closing relayed connection: relay path ended");
            self.drop_conn(&id);
        }
    }

    /// Close the pooled connection to `node_id` if it rides on `synthetic`.
    fn close_relayed_conn(&self, node_id: &NodeId, synthetic: SocketAddr) {
        let hit = self
            .pooled_open(node_id)
            .is_some_and(|c| c.kind == ConnKind::Relayed && c.conn.remote_address() == synthetic);
        if hit {
            self.drop_conn(node_id);
        }
    }

    pub fn drop_conn(&self, node_id: &NodeId) {
        if let Some(entry) = self.pool.lock().remove(node_id) {
            entry.conn.close(0u32.into(), b"dropped");
            if entry.kind == ConnKind::Relayed {
                self.sock.drop_route_addr(entry.conn.remote_address());
            }
        }
    }

    /// Smoothed RTT to a member over an existing pooled connection, if one
    /// is open. Free scheduler signal (MASTERPLAN §10.1) — never dials.
    pub fn rtt_micros(&self, node_id: &NodeId) -> Option<u64> {
        let c = self.pooled_open(node_id)?;
        Some(u64::try_from(c.conn.rtt().as_micros()).unwrap_or(u64::MAX))
    }

    fn dial_lock(&self, node_id: NodeId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.dial_locks.lock();
        if locks.len() > 4096 {
            locks.retain(|_, l| Arc::strong_count(l) > 1);
        }
        locks
            .entry(node_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn record_dial(&self, node_id: NodeId, addrs: &[String], outcome: Option<Option<DialRoute>>) {
        let mut states = self.dial_state.lock();
        if states.len() > 4096 {
            states.clear();
        }
        let st = states.entry(node_id).or_default();
        match outcome {
            Some(route) => {
                if route.is_some() {
                    st.last_good = route;
                }
                st.last_failure = None;
            }
            None => st.last_failure = Some((Instant::now(), sorted(addrs))),
        }
    }

    /// Record that a dial to `node_id` was given up on before it finished
    /// (ENG-7), so background callers fail fast for [`NEGATIVE_TTL`] the way
    /// they do after a dial that failed outright.
    pub fn note_dial_timeout(&self, node_id: NodeId, addrs: &[String]) {
        self.record_dial(node_id, addrs, None);
    }

    /// Last route that reached a peer (tests and status).
    pub fn last_good_route(&self, node_id: &NodeId) -> Option<DialRoute> {
        self.dial_state
            .lock()
            .get(node_id)
            .and_then(|s| s.last_good)
    }

    // -- outbound ----------------------------------------------------------

    /// Dial + handshake. Returns the connection, the peer's ack, and the
    /// negotiated protocol minor.
    async fn connect_raw(
        &self,
        addr: SocketAddr,
        expected: Option<NodeId>,
        purpose: ConnPurpose,
        timeout: Duration,
    ) -> Result<(quinn::Connection, ConnHelloAck, u16)> {
        let connecting = if expected.is_some() {
            let cfg = build_client_config(&self.key, self.provider.clone(), expected)?;
            self.endpoint.connect_with(cfg, addr, SERVER_NAME)
        } else {
            self.endpoint.connect(addr, SERVER_NAME)
        }
        .map_err(|e| CoreError::Transport(format!("connect {addr}: {e}")))?;

        let conn = tokio::time::timeout(timeout, connecting)
            .await
            .map_err(|_| CoreError::Transport(format!("dial {addr}: timed out")))?
            .map_err(|e| CoreError::Transport(format!("handshake with {addr}: {e}")))?;

        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::Transport(format!("open handshake stream: {e}")))?;
        write_frame(&mut send, &self.hello(purpose)).await?;
        let _ = send.finish();
        let ack: ConnHelloAck = tokio::time::timeout(timeout, read_frame(&mut recv))
            .await
            .map_err(|_| CoreError::Transport(format!("handshake with {addr}: timed out")))??;
        // The peer's claimed identity must match its TLS key.
        let tls_peer = peer_node_id(&conn)?;
        if ack.node_id != tls_peer {
            return Err(CoreError::HandshakeRefused(format!(
                "peer claims {} but TLS key is {}",
                ack.node_id.short(),
                tls_peer.short()
            )));
        }
        let minor = mesh_proto::negotiate(
            (mesh_proto::PROTOCOL_MAJOR, self.advertised_minor()),
            (ack.proto_major, ack.proto_minor),
        )
        .map_err(CoreError::HandshakeRefused)?;
        // Every dialled connection, not only the ones that reach the pool
        // (ENG-24): a join, a pairing and a probe all carry requests, and
        // `request_one` has nothing but the connection to go on.
        self.note_minor(&conn, minor);
        Ok((conn, ack, minor))
    }

    /// One direct member dial: pinned handshake, adopt into the pool.
    async fn dial_direct_one(
        &self,
        node_id: NodeId,
        addr: SocketAddr,
        timeout: Duration,
        pinned: bool,
    ) -> Result<quinn::Connection> {
        let (conn, ack, minor) = self
            .connect_raw(addr, Some(node_id), ConnPurpose::Member, timeout)
            .await?;
        if !ack.accepted {
            return Err(CoreError::HandshakeRefused(
                ack.reason.unwrap_or_else(|| "refused".into()),
            ));
        }
        Ok(self.adopt_outbound(node_id, conn, &ack, minor, pinned))
    }

    /// Split candidates into resolved direct addresses and relay ids.
    async fn classify(addrs: &[String]) -> (Vec<SocketAddr>, Vec<NodeId>, Option<CoreError>) {
        let mut direct = Vec::new();
        let mut relays = Vec::new();
        let mut last_err = None;
        for a in addrs {
            if let Some(relay) = parse_relay_candidate(a) {
                if !relays.contains(&relay) {
                    relays.push(relay);
                }
                continue;
            }
            match resolve(a).await {
                Ok(sa) if sa.port() == 0 => {
                    last_err = Some(CoreError::Transport(
                        "member has not announced a dialable address yet".into(),
                    ));
                }
                Ok(sa) => {
                    if !direct.contains(&sa) {
                        direct.push(sa);
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        (direct, relays, last_err)
    }

    /// Race direct candidates (staggered, per-candidate timeout, overall
    /// window). First accepted handshake wins; the rest are dropped.
    async fn race_direct(
        &self,
        node_id: NodeId,
        direct: &[SocketAddr],
        per_timeout: Duration,
        stagger: Duration,
        window: Option<Duration>,
        pinned: bool,
    ) -> Result<quinn::Connection> {
        if direct.is_empty() {
            return Err(CoreError::Transport(format!(
                "no direct address for {}",
                node_id.short()
            )));
        }
        let mut race = FuturesUnordered::new();
        for (i, addr) in direct.iter().copied().enumerate() {
            race.push(async move {
                if i > 0 {
                    tokio::time::sleep(stagger * u32::try_from(i).unwrap_or(u32::MAX)).await;
                }
                self.dial_direct_one(node_id, addr, per_timeout, pinned)
                    .await
            });
        }
        let deadline = window.map(|w| Instant::now() + w);
        let mut last_err =
            CoreError::Transport(format!("no reachable address for {}", node_id.short()));
        loop {
            let next = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    match tokio::time::timeout(left, race.next()).await {
                        Ok(n) => n,
                        Err(_) => {
                            return Err(CoreError::Transport(format!(
                                "dial {}: window elapsed",
                                node_id.short()
                            )));
                        }
                    }
                }
                None => race.next().await,
            };
            match next {
                Some(Ok(conn)) => return Ok(conn),
                Some(Err(e)) => last_err = e,
                None => return Err(last_err),
            }
        }
    }

    /// Direct-only member connection (no punch, no relay): used to reach
    /// relays themselves — the recursion fence — and for punch dial-backs.
    pub async fn member_conn_direct(
        &self,
        node_id: NodeId,
        addrs: &[String],
        pinned: bool,
    ) -> Result<quinn::Connection> {
        if let Some(c) = self.pooled_open(&node_id)
            && c.kind == ConnKind::Direct
        {
            if pinned && !c.pinned {
                self.pin_conn(&node_id, true);
            }
            return Ok(c.conn);
        }
        let (direct, _relays, err) = Self::classify(addrs).await;
        if direct.is_empty() {
            return Err(err.unwrap_or_else(|| {
                CoreError::Transport(format!("no direct address for {}", node_id.short()))
            }));
        }
        self.race_direct(
            node_id,
            &direct,
            DIRECT_DIAL_TIMEOUT,
            DIAL_STAGGER,
            None,
            pinned,
        )
        .await
    }

    /// Punch dial-back (MASTERPLAN M7): a peer asked, through a relay, to
    /// be dialed on these candidates. Direct-only, key-pinned, bounded
    /// (≤ 8 candidates, ~1 s each, 3 s window) — no recursion, bounded
    /// amplification. Success pools the connection; the peer adopts it as
    /// an inbound connection on its side.
    pub async fn dial_back(
        &self,
        node_id: NodeId,
        candidates: &[String],
    ) -> Result<quinn::Connection> {
        let capped: Vec<String> = candidates
            .iter()
            .take(MAX_PUNCH_CANDIDATES)
            .cloned()
            .collect();
        let (direct, _relays, err) = Self::classify(&capped).await;
        if direct.is_empty() {
            return Err(err.unwrap_or_else(|| {
                CoreError::Transport("punch carried no direct candidates".into())
            }));
        }
        self.race_direct(
            node_id,
            &direct,
            PUNCH_BACK_TIMEOUT,
            PUNCH_BACK_STAGGER,
            Some(PUNCH_BACK_WINDOW),
            false,
        )
        .await
    }

    /// Ask `relay` to have `target` dial us back, then wait for the
    /// resulting inbound connection to land in the pool.
    async fn punch_via(&self, relay: NodeId, target: NodeId) -> Result<quinn::Connection> {
        let own = self.own_candidates.lock().clone();
        if own.is_empty() {
            return Err(CoreError::Transport(
                "no own candidates to punch with".into(),
            ));
        }
        let relay_addrs = self
            .lookup_member(&relay)
            .ok_or_else(|| CoreError::Transport(format!("relay {} unknown", relay.short())))?;
        let rconn = self.member_conn_direct(relay, &relay_addrs, false).await?;
        if self.peer_minor(&relay).unwrap_or(0) < WAN_MINOR {
            return Err(CoreError::Transport(format!(
                "relay {} predates protocol 1.{WAN_MINOR}",
                relay.short()
            )));
        }
        // Arm the notification before asking so an instant dial-back is
        // not missed.
        let notified = self.pool_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        match self
            .request_one(
                &rconn,
                &Request::PunchRequest {
                    target,
                    candidates: own,
                },
            )
            .await?
        {
            Response::PunchSent => {}
            Response::Error { message } => {
                return Err(CoreError::Transport(format!(
                    "relay {}: {message}",
                    relay.short()
                )));
            }
            other => {
                return Err(CoreError::Transport(format!(
                    "unexpected punch response: {other:?}"
                )));
            }
        }
        let deadline = Instant::now() + PUNCH_WINDOW;
        loop {
            if let Some(c) = self.pooled_open(&target) {
                return Ok(c.conn);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(CoreError::Transport(format!(
                    "punch toward {} via {}: no dial-back within {PUNCH_WINDOW:?}",
                    target.short(),
                    relay.short()
                )));
            }
            if tokio::time::timeout(left, notified.as_mut()).await.is_err() {
                return Err(CoreError::Transport(format!(
                    "punch toward {} via {}: no dial-back within {PUNCH_WINDOW:?}",
                    target.short(),
                    relay.short()
                )));
            }
            notified.set(self.pool_changed.notified());
            notified.as_mut().enable();
        }
    }

    /// Relayed dial (MASTERPLAN M7): open a path to `target` through
    /// `relay`, then run an ordinary key-pinned QUIC handshake over it via
    /// a synthetic address. The relay sees only ciphertext.
    pub async fn dial_via_relay(&self, relay: NodeId, target: NodeId) -> Result<quinn::Connection> {
        let relay_addrs = self
            .lookup_member(&relay)
            .ok_or_else(|| CoreError::Transport(format!("relay {} unknown", relay.short())))?;
        let rconn = self.member_conn_direct(relay, &relay_addrs, false).await?;
        if self.peer_minor(&relay).unwrap_or(0) < WAN_MINOR {
            return Err(CoreError::Transport(format!(
                "relay {} predates protocol 1.{WAN_MINOR}",
                relay.short()
            )));
        }
        let (mut send, mut recv) = rconn
            .open_bi()
            .await
            .map_err(|e| CoreError::Transport(format!("open relay stream: {e}")))?;
        write_frame(&mut send, &Request::RelayOpen { target }).await?;
        match tokio::time::timeout(DIAL_TIMEOUT, read_frame::<_, Response>(&mut recv))
            .await
            .map_err(|_| CoreError::Transport(format!("relay {}: no answer", relay.short())))??
        {
            Response::RelayReady => {}
            Response::Error { message } => {
                return Err(CoreError::Transport(format!(
                    "relay {}: {message}",
                    relay.short()
                )));
            }
            other => {
                return Err(CoreError::Transport(format!(
                    "unexpected relay response: {other:?}"
                )));
            }
        }
        let (tx, mut rx) = mpsc::channel::<RelayFrame>(RELAY_STREAM_QUEUE);
        let (synthetic, generation) = self.sock.add_route(relay, target, tx);
        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_frame(&mut send, &frame).await.is_err() {
                    break;
                }
            }
            let _ = send.finish();
        });
        let sock = self.sock.clone();
        let this = self.arc();
        tokio::spawn(async move {
            loop {
                match read_frame::<_, RelayServerFrame>(&mut recv).await {
                    Ok(RelayServerFrame::Data { from, payload }) if from == target => {
                        sock.inject(relay, from, payload);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            // The path is gone: the inner session over it is dead too —
            // unless a newer path for the same pair already replaced us.
            if sock.route_generation(relay, target) == Some(generation) {
                if let Some(t) = this {
                    t.close_relayed_conn(&target, synthetic);
                }
                sock.drop_route_if(relay, target, generation);
            }
            writer.abort();
        });
        match self
            .connect_raw(synthetic, Some(target), ConnPurpose::Member, DIAL_TIMEOUT)
            .await
        {
            Ok((conn, ack, minor)) if ack.accepted => {
                info!(peer = %target.short(), relay = %relay.short(), "connected via relay");
                Ok(self.adopt_outbound(target, conn, &ack, minor, false))
            }
            Ok((_, ack, _)) => {
                self.sock.drop_route_if(relay, target, generation);
                Err(CoreError::HandshakeRefused(
                    ack.reason.unwrap_or_else(|| "refused".into()),
                ))
            }
            Err(e) => {
                self.sock.drop_route_if(relay, target, generation);
                Err(e)
            }
        }
    }

    /// Tiered racing dial (MASTERPLAN M7): last-good route → direct
    /// candidates raced (staggered) alongside a coordinated punch through
    /// the peer's relay → relayed path.
    async fn dial_member(
        &self,
        node_id: NodeId,
        addrs: &[String],
    ) -> Result<(quinn::Connection, Option<DialRoute>)> {
        let (direct, relays, classify_err) = Self::classify(addrs).await;
        if direct.is_empty() && relays.is_empty() {
            return Err(classify_err.unwrap_or_else(|| {
                CoreError::Transport(format!("no reachable address for {}", node_id.short()))
            }));
        }

        // Fast path: the route that worked last time.
        let last_good = self.last_good_route(&node_id);
        match last_good {
            Some(DialRoute::Direct(addr)) if direct.contains(&addr) => {
                if let Ok(conn) = self
                    .dial_direct_one(node_id, addr, DIRECT_DIAL_TIMEOUT, false)
                    .await
                {
                    return Ok((conn, Some(DialRoute::Direct(addr))));
                }
            }
            Some(DialRoute::Relay(r)) if relays.contains(&r) => {
                if let Ok(conn) = self.dial_via_relay(r, node_id).await {
                    return Ok((conn, Some(DialRoute::Relay(r))));
                }
            }
            _ => {}
        }

        // Tier 1 + 2: direct race and punch, concurrently.
        type Attempt<'a> = std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(quinn::Connection, Option<DialRoute>)>>
                    + Send
                    + 'a,
            >,
        >;
        let mut race: FuturesUnordered<Attempt<'_>> = FuturesUnordered::new();
        for (i, addr) in direct.iter().copied().enumerate() {
            race.push(Box::pin(async move {
                if i > 0 {
                    tokio::time::sleep(DIAL_STAGGER * u32::try_from(i).unwrap_or(u32::MAX)).await;
                }
                self.dial_direct_one(node_id, addr, DIRECT_DIAL_TIMEOUT, false)
                    .await
                    .map(|c| (c, Some(DialRoute::Direct(addr))))
            }));
        }
        if let Some(relay) = relays.first().copied() {
            race.push(Box::pin(async move {
                self.punch_via(relay, node_id).await.map(|c| (c, None))
            }));
        }
        let mut last_err = classify_err.unwrap_or_else(|| {
            CoreError::Transport(format!("no reachable address for {}", node_id.short()))
        });
        while let Some(outcome) = race.next().await {
            match outcome {
                Ok(x) => return Ok(x),
                Err(e) => last_err = e,
            }
        }
        drop(race);

        // Tier 3: relay.
        for relay in relays {
            match self.dial_via_relay(relay, node_id).await {
                Ok(conn) => return Ok((conn, Some(DialRoute::Relay(relay)))),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Pooled member connection: an open pooled connection (dialed or
    /// accepted) is returned at once; otherwise the tiered racing dialer
    /// runs, single-flight per peer. User-facing work (job dispatch, log
    /// follow, content fetch) always gets a full attempt.
    pub async fn member_conn(
        &self,
        node_id: NodeId,
        addrs: &[String],
    ) -> Result<quinn::Connection> {
        self.member_conn_with(node_id, addrs, false).await
    }

    /// [`member_conn`](Self::member_conn) for background callers (gossip
    /// ticks, verdict probes): after a failed dial of the same candidate
    /// set, fail fast for a few seconds instead of re-running the race on
    /// every tick. A changed candidate set is always retried.
    pub async fn member_conn_background(
        &self,
        node_id: NodeId,
        addrs: &[String],
    ) -> Result<quinn::Connection> {
        self.member_conn_with(node_id, addrs, true).await
    }

    async fn member_conn_with(
        &self,
        node_id: NodeId,
        addrs: &[String],
        fail_fast: bool,
    ) -> Result<quinn::Connection> {
        if let Some(c) = self.pooled_open(&node_id) {
            return Ok(c.conn);
        }
        let lock = self.dial_lock(node_id);
        let _guard = lock.lock().await;
        if let Some(c) = self.pooled_open(&node_id) {
            return Ok(c.conn);
        }
        let recently_failed = fail_fast
            && self
                .dial_state
                .lock()
                .get(&node_id)
                .and_then(|s| s.last_failure.clone())
                .is_some_and(|(t, failed_addrs)| {
                    t.elapsed() < NEGATIVE_TTL && failed_addrs == sorted(addrs)
                });
        if recently_failed {
            return Err(CoreError::Unreachable(format!(
                "{} (recent dial failed; retrying after {NEGATIVE_TTL:?})",
                node_id.short()
            )));
        }
        match self.dial_member(node_id, addrs).await {
            Ok((conn, route)) => {
                self.record_dial(node_id, addrs, Some(route));
                Ok(conn)
            }
            Err(e) => {
                self.record_dial(node_id, addrs, None);
                // Nothing was sent to a peer that was never reached.
                Err(match e {
                    CoreError::Unreachable(_) => e,
                    other => CoreError::Unreachable(format!("{}: {other}", node_id.short())),
                })
            }
        }
    }

    /// Register with a relay (MASTERPLAN M7): pinned direct connection,
    /// `RelayRegister`, then a pump that injects frames for us into the
    /// socket and forwards punch requests to the handler. Outbound frames
    /// toward any peer via this relay leave on the same stream.
    pub async fn register_with_relay(
        &self,
        relay: NodeId,
        relay_addrs: &[String],
    ) -> Result<RelayRegistration> {
        let rconn = self.member_conn_direct(relay, relay_addrs, true).await?;
        if self.peer_minor(&relay).unwrap_or(0) < WAN_MINOR {
            return Err(CoreError::Transport(format!(
                "relay {} predates protocol 1.{WAN_MINOR}",
                relay.short()
            )));
        }
        let (mut send, mut recv) = rconn
            .open_bi()
            .await
            .map_err(|e| CoreError::Transport(format!("open relay stream: {e}")))?;
        write_frame(&mut send, &Request::RelayRegister).await?;
        match tokio::time::timeout(DIAL_TIMEOUT, read_frame::<_, Response>(&mut recv))
            .await
            .map_err(|_| CoreError::Transport(format!("relay {}: no answer", relay.short())))??
        {
            Response::RelayReady => {}
            Response::Error { message } => {
                return Err(CoreError::Transport(format!(
                    "relay {}: {message}",
                    relay.short()
                )));
            }
            other => {
                return Err(CoreError::Transport(format!(
                    "unexpected relay response: {other:?}"
                )));
            }
        }
        let (tx, mut rx) = mpsc::channel::<RelayFrame>(RELAY_STREAM_QUEUE);
        self.sock.set_uplink(relay, tx);
        let (done_tx, done_rx) = watch::channel(false);
        let sock = self.sock.clone();
        let punch = self.punch_handler.lock().clone();
        let this = self.arc();
        let task = tokio::spawn(async move {
            let writer = tokio::spawn(async move {
                while let Some(frame) = rx.recv().await {
                    if write_frame(&mut send, &frame).await.is_err() {
                        break;
                    }
                }
                let _ = send.finish();
            });
            loop {
                match read_frame::<_, RelayServerFrame>(&mut recv).await {
                    Ok(RelayServerFrame::Data { from, payload }) => {
                        sock.inject(relay, from, payload);
                    }
                    Ok(RelayServerFrame::Punch { from, candidates }) => {
                        if let Some(h) = punch.as_ref() {
                            h(from, candidates);
                        }
                    }
                    Err(_) => break,
                }
            }
            writer.abort();
            if let Some(t) = this {
                t.close_relayed_via(&relay);
                t.pin_conn(&relay, false);
            }
            sock.drop_routes_via(&relay);
            let _ = done_tx.send(true);
        });
        info!(relay = %relay.short(), "registered with relay");
        Ok(RelayRegistration {
            relay,
            done: done_rx,
            task,
        })
    }

    /// Dial a bootstrap node for a join (unpinned: the joiner does not yet
    /// know the acceptor's key). Purpose is Join.
    pub async fn connect_for_join(
        &self,
        addr: SocketAddr,
    ) -> Result<(quinn::Connection, ConnHelloAck)> {
        let (conn, ack, _) = self
            .connect_raw(addr, None, ConnPurpose::Join, DIAL_TIMEOUT)
            .await?;
        Ok((conn, ack))
    }

    /// Race several bootstrap addresses for a join (staggered); the first
    /// accepted handshake wins. `relay:` candidates are skipped — a joiner
    /// is not a member and cannot use relays.
    pub async fn race_join(&self, addrs: &[String]) -> Result<(quinn::Connection, ConnHelloAck)> {
        let (direct, _relays, err) = Self::classify(addrs).await;
        if direct.is_empty() {
            return Err(err.unwrap_or_else(|| {
                CoreError::Membership("no bootstrap address was reachable".into())
            }));
        }
        let mut race = FuturesUnordered::new();
        for (i, addr) in direct.iter().copied().enumerate() {
            race.push(async move {
                if i > 0 {
                    tokio::time::sleep(DIAL_STAGGER * u32::try_from(i).unwrap_or(u32::MAX)).await;
                }
                self.connect_for_join(addr).await
            });
        }
        let mut last_err = CoreError::Membership("no bootstrap address was reachable".into());
        while let Some(outcome) = race.next().await {
            match outcome {
                Ok((conn, ack)) if ack.accepted => return Ok((conn, ack)),
                Ok((_, ack)) => {
                    last_err =
                        CoreError::HandshakeRefused(ack.reason.unwrap_or_else(|| "refused".into()));
                }
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Dial a candidate node waiting in `meshnode join --pair` (unpinned:
    /// neither side knows the other's key yet — the pairing code is what
    /// authenticates the exchange, MASTERPLAN §5.2).
    pub async fn connect_for_pair(
        &self,
        addr: SocketAddr,
    ) -> Result<(quinn::Connection, ConnHelloAck)> {
        let (conn, ack, _) = self
            .connect_raw(addr, None, ConnPurpose::Pair, DIAL_TIMEOUT)
            .await?;
        Ok((conn, ack))
    }

    /// One request, one response.
    ///
    /// A failure to open the stream or to write the request is
    /// [`CoreError::Unreachable`]: the frame never left whole, so the peer
    /// cannot have acted on it. A failure after that — no answer, a reset
    /// — is not, because the peer may have (ENG-6).
    pub async fn request_one(&self, conn: &quinn::Connection, req: &Request) -> Result<Response> {
        self.refuse_if_too_old(conn, req)?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::Unreachable(format!("open stream: {e}")))?;
        write_frame(&mut send, req)
            .await
            .map_err(|e| CoreError::Unreachable(format!("send: {e}")))?;
        let _ = send.finish();
        let resp: Response = read_frame(&mut recv).await?;
        Ok(resp)
    }

    /// [`Self::request_one`], held to two deadlines (ENG-7): the answer has
    /// to *start* by `answer_by`, and then has `rest` to finish.
    ///
    /// Split because the two mean different things. An answer that has not
    /// begun says the peer is not there; one that is still arriving says the
    /// link is slow, and a megabyte of ops on a thin link is not a failure.
    pub async fn request_answered_by(
        &self,
        conn: &quinn::Connection,
        req: &Request,
        answer_by: tokio::time::Instant,
        rest: Duration,
    ) -> Result<Response> {
        use tokio::io::AsyncReadExt as _;
        self.refuse_if_too_old(conn, req)?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::Unreachable(format!("open stream: {e}")))?;
        write_frame(&mut send, req)
            .await
            .map_err(|e| CoreError::Unreachable(format!("send: {e}")))?;
        let _ = send.finish();
        let mut prefix = [0u8; 4];
        match tokio::time::timeout_at(
            answer_by,
            tokio::io::AsyncReadExt::read_exact(&mut recv, &mut prefix),
        )
        .await
        {
            Ok(read) => read.map_err(mesh_proto::ProtoError::from)?,
            Err(_) => {
                return Err(CoreError::Transport(
                    "no answer started in time".to_string(),
                ));
            }
        };
        let mut framed = (&prefix[..]).chain(recv);
        match tokio::time::timeout(rest, read_frame::<_, Response>(&mut framed)).await {
            Ok(resp) => Ok(resp?),
            Err(_) => Err(CoreError::Transport(
                "the answer did not finish in time".to_string(),
            )),
        }
    }

    /// Open a session on a peer: write the request frame and **keep the
    /// write half** (M17 §3.3).
    ///
    /// The one deviation from `request_stream`, and the whole point of the
    /// milestone — `request_stream` calls `send.finish()` before it
    /// returns, which is precisely why keystrokes had nowhere to go (R1).
    pub async fn open_session(
        &self,
        conn: &quinn::Connection,
        spec: mesh_proto::SessionSpec,
    ) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::Transport(format!("open stream: {e}")))?;
        write_frame(&mut send, &Request::SessionOpen(Box::new(spec))).await?;
        Ok((send, recv))
    }

    /// One request, streaming responses.
    pub async fn request_stream(
        &self,
        conn: &quinn::Connection,
        req: &Request,
    ) -> Result<ResponseStream> {
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::Transport(format!("open stream: {e}")))?;
        write_frame(&mut send, req).await?;
        let _ = send.finish();
        Ok(ResponseStream { recv })
    }

    /// Reflexive address probe (MASTERPLAN M7): what address does this
    /// peer see us from? Only meaningful over a direct connection to a
    /// peer that negotiated 1.5; `None` otherwise.
    pub async fn observe(&self, node_id: &NodeId) -> Result<Option<SocketAddr>> {
        let Some(pooled) = self.pooled_open(node_id) else {
            return Ok(None);
        };
        if pooled.kind != ConnKind::Direct || pooled.minor < WAN_MINOR {
            return Ok(None);
        }
        match self.request_one(&pooled.conn, &Request::Observe).await? {
            Response::Observed { addr } => {
                Ok(addr.parse::<SocketAddr>().ok().map(meshsock::normalize))
            }
            Response::Error { message } => Err(CoreError::Transport(message)),
            other => Err(CoreError::Transport(format!(
                "unexpected observe response: {other:?}"
            ))),
        }
    }

    /// Diagnostic reachability probe: fresh connection, purpose Probe,
    /// allowed cross-mesh.
    pub async fn ping(&self, addr: SocketAddr) -> Result<PingResult> {
        let (conn, ack, _) = self
            .connect_raw(addr, None, ConnPurpose::Probe, DIAL_TIMEOUT)
            .await?;
        if !ack.accepted {
            return Err(CoreError::HandshakeRefused(
                ack.reason.unwrap_or_else(|| "refused".into()),
            ));
        }
        let mut nonce_bytes = [0u8; 8];
        getrandom::fill(&mut nonce_bytes).map_err(|e| CoreError::Transport(format!("rng: {e}")))?;
        let nonce = u64::from_le_bytes(nonce_bytes);
        let started = Instant::now();
        let resp = self
            .request_one(
                &conn,
                &Request::Ping(Ping {
                    nonce,
                    sent_at_micros: mesh_proto::now_micros(),
                }),
            )
            .await?;
        let rtt = started.elapsed();
        match resp {
            Response::Pong(p) if p.nonce == nonce => {}
            Response::Pong(_) => return Err(CoreError::Transport("pong nonce mismatch".into())),
            other => {
                return Err(CoreError::Transport(format!(
                    "expected Pong, got {other:?}"
                )));
            }
        }
        let same_mesh = match (self.local.mesh_id, ack.mesh_id) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        conn.close(0u32.into(), b"done");
        Ok(PingResult {
            peer_node_id: ack.node_id,
            peer_node_name: ack.node_name,
            peer_mesh_id: ack.mesh_id,
            peer_mesh_name: ack.mesh_name,
            same_mesh,
            rtt_micros: u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX),
            via: addr.to_string(),
        })
    }

    /// Reach a **member**, the way every other peer request reaches it:
    /// through the pooled, racing dialer — direct candidates first, then
    /// hole punch, then relay (MASTERPLAN M7 §4).
    ///
    /// This is the ping a caller wants for a node it already knows. Picking
    /// one address out of the announced list and probing that instead
    /// answers a different question: the list is a set of *candidates*, its
    /// first entry is the peer's own loopback, and only the dialer knows
    /// which of them (or which relay) currently works.
    ///
    /// The reply comes back over the pooled connection when one is open —
    /// confirming the path real traffic uses rather than a fresh one that
    /// proves nothing about it.
    pub async fn ping_member(&self, node_id: NodeId, addrs: &[String]) -> Result<(u64, String)> {
        let conn = self.member_conn(node_id, addrs).await?;
        let mut nonce_bytes = [0u8; 8];
        getrandom::fill(&mut nonce_bytes).map_err(|e| CoreError::Transport(format!("rng: {e}")))?;
        let nonce = u64::from_le_bytes(nonce_bytes);
        let started = Instant::now();
        let resp = self
            .request_one(
                &conn,
                &Request::Ping(Ping {
                    nonce,
                    sent_at_micros: mesh_proto::now_micros(),
                }),
            )
            .await?;
        let rtt = started.elapsed();
        match resp {
            Response::Pong(p) if p.nonce == nonce => {}
            Response::Pong(_) => return Err(CoreError::Transport("pong nonce mismatch".into())),
            other => {
                return Err(CoreError::Transport(format!(
                    "expected Pong, got {other:?}"
                )));
            }
        }
        // The connection is pooled and shared: leave it open. Where it goes
        // is read off the connection itself rather than off the dial cache,
        // which says nothing about a peer that dialed *us*.
        let remote = conn.remote_address();
        let via = match kind_of(remote) {
            ConnKind::Direct => remote.to_string(),
            // A synthetic address is a route, not a place: name the relay.
            ConnKind::Relayed => match self.last_good_route(&node_id) {
                Some(DialRoute::Relay(r)) => format!("relay {}", r.short()),
                _ => "relay".to_string(),
            },
        };
        Ok((u64::try_from(rtt.as_micros()).unwrap_or(u64::MAX), via))
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }

    // -- inbound -----------------------------------------------------------

    fn spawn_accept_loop(self: Arc<Self>, pulse: crate::vitals::Pulse) {
        let endpoint = self.endpoint.clone();
        tokio::spawn(async move {
            // Ends with the loop: `accept` answers `None` only once the
            // endpoint is closed.
            let _pulse = pulse;
            while let Some(incoming) = endpoint.accept().await {
                // Bounded, and refused before any cryptography when full
                // (SEC-10). A handshake that never finishes gives its permit
                // back at the deadline.
                let Ok(permit) = self.handshakes.clone().try_acquire_owned() else {
                    debug!(from = %incoming.remote_address(), "too many handshakes; refusing");
                    incoming.refuse();
                    continue;
                };
                let this = self.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                        Ok(Ok(conn)) => this.serve_connection(conn, permit).await,
                        Ok(Err(e)) => debug!("inbound connection failed: {e}"),
                        Err(_) => debug!("inbound handshake timed out"),
                    }
                });
            }
        });
    }

    async fn serve_connection(
        self: Arc<Self>,
        conn: quinn::Connection,
        handshake: tokio::sync::OwnedSemaphorePermit,
    ) {
        let tls_peer = match peer_node_id(&conn) {
            Ok(id) => id,
            Err(e) => {
                warn!("rejecting connection: {e}");
                conn.close(1u32.into(), b"identity");
                return;
            }
        };

        // Handshake stream, inside the same deadline as the TLS handshake,
        // and with a hello-sized frame (SEC-10).
        let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let (send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
            let hello: ConnHello = read_frame_limited(&mut recv, STRANGER_FRAME_LEN)
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((send, hello))
        })
        .await;
        let (mut hs_send, hello) = match hello {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                debug!("bad handshake: {e}");
                return;
            }
            Err(_) => {
                debug!(peer = %tls_peer.short(), "handshake stream timed out");
                conn.close(1u32.into(), b"timeout");
                return;
            }
        };
        drop(handshake);

        let mut reason: Option<String> = None;
        let mut minor = 0u16;
        match mesh_proto::negotiate(
            (mesh_proto::PROTOCOL_MAJOR, self.advertised_minor()),
            (hello.proto_major, hello.proto_minor),
        ) {
            Ok(m) => minor = m,
            Err(e) => reason = Some(e),
        }
        if reason.is_none() {
            if hello.node_id != tls_peer {
                reason = Some(format!(
                    "peer claims {} but TLS key is {}",
                    hello.node_id.short(),
                    tls_peer.short()
                ));
            } else if matches!(hello.purpose, ConnPurpose::Member | ConnPurpose::Join) {
                // Member and Join traffic must target our mesh.
                match (self.local.mesh_id, hello.mesh_id) {
                    (Some(a), Some(b)) if a == b => {}
                    _ => reason = Some("mesh mismatch".into()),
                }
            }
        }
        if reason.is_none()
            && let Err(e) = (self.admission)(&tls_peer, hello.purpose)
        {
            reason = Some(e);
        }
        // A peer that is not a member of this mesh takes a stranger's slot,
        // and is refused if there is none (SEC-10).
        let stranger = self.local.mesh_id.is_some() && !self.is_member(&tls_peer);
        let slot = if stranger && reason.is_none() {
            let ip = conn.remote_address().ip();
            let slot = StrangerSlot::take(&self.strangers, ip);
            if slot.is_none() {
                reason = Some("too many connections from outside this mesh".into());
            }
            slot
        } else {
            None
        };

        // The mesh's identity goes only to a peer that already knows it
        // (SEC-10). A probe used to hand it to anybody, and it is half of
        // what a stranger needs to open member connections. A peer in some
        // other mesh learns that this node is in one, and nothing more.
        let same_mesh = self.local.mesh_id.is_some() && hello.mesh_id == self.local.mesh_id;
        let accepted = reason.is_none();
        let ack = ConnHelloAck {
            proto_major: mesh_proto::PROTOCOL_MAJOR,
            proto_minor: self.advertised_minor(),
            node_id: self.local.node_id,
            node_name: self.local.node_name.clone(),
            mesh_id: if same_mesh { self.local.mesh_id } else { None },
            mesh_name: if same_mesh {
                self.local.mesh_name.clone()
            } else {
                self.local.mesh_name.as_ref().map(|_| String::new())
            },
            accepted,
            reason: reason.clone(),
        };
        if write_frame(&mut hs_send, &ack).await.is_err() {
            return;
        }
        let _ = hs_send.finish();
        if !accepted {
            info!(peer = %tls_peer.short(), "connection refused: {}", reason.unwrap_or_default());
            // Give the ack a moment to flush before closing.
            tokio::time::sleep(Duration::from_millis(50)).await;
            conn.close(2u32.into(), b"refused");
            return;
        }

        // An accepted connection carries requests in *both* directions —
        // that is how a public node reaches a NATed one — so this side has
        // to know the minor too (ENG-24).
        self.note_minor(&conn, minor);

        let kind = kind_of(conn.remote_address());
        let peer = PeerCtx {
            node_id: tls_peer,
            node_name: hello.node_name.clone(),
            remote_addr: conn.remote_address(),
            purpose: hello.purpose,
            minor,
            kind,
        };
        debug!(peer = %tls_peer.short(), purpose = ?hello.purpose, minor, ?kind, "connection accepted");

        // Inbound pooling (MASTERPLAN M7): a 1.5 member's connection to us
        // is usable in both directions — it is how a public node reaches a
        // NATed one. 1.4 dialers never accept streams, so they are not
        // pooled (a request would hang forever).
        if hello.purpose == ConnPurpose::Member && minor >= WAN_MINOR {
            self.pool_insert(
                tls_peer,
                PooledConn {
                    conn: conn.clone(),
                    minor,
                    kind,
                    pinned: false,
                    inbound: true,
                    last_used: Instant::now(),
                },
            );
        }

        if !stranger {
            conn.set_max_concurrent_bi_streams(MEMBER_STREAMS.into());
        }
        self.serve_streams_limited(conn, peer, slot).await;
    }

    /// Accept request streams on a connection until it ends. Runs for every
    /// accepted connection and for dialed connections to 1.5 peers.
    async fn serve_streams(self: Arc<Self>, conn: quinn::Connection, peer: PeerCtx) {
        self.serve_streams_limited(conn, peer, None).await;
    }

    /// [`Self::serve_streams`], held to a stranger's limits while it holds a
    /// stranger's slot — which it gives back the moment the peer turns out
    /// to be a member. A node that joined seconds ago may have dialled here
    /// before this node merged its admission, and that connection can live
    /// for hours.
    async fn serve_streams_limited(
        self: Arc<Self>,
        conn: quinn::Connection,
        peer: PeerCtx,
        mut slot: Option<StrangerSlot>,
    ) {
        loop {
            let (send, recv) = match conn.accept_bi().await {
                Ok(pair) => pair,
                Err(quinn::ConnectionError::ApplicationClosed(_))
                | Err(quinn::ConnectionError::ConnectionClosed(_))
                | Err(quinn::ConnectionError::LocallyClosed) => return,
                Err(e) => {
                    debug!(peer = %peer.node_id.short(), "connection ended: {e}");
                    return;
                }
            };
            // Judged as the stream arrives, not as the loop last began: the
            // wait for it can be long.
            if slot.is_some() && self.is_member(&peer.node_id) {
                slot = None;
                conn.set_max_concurrent_bi_streams(MEMBER_STREAMS.into());
            }
            let frame_len = if slot.is_some() {
                STRANGER_FRAME_LEN
            } else {
                mesh_proto::MAX_FRAME_LEN
            };
            let this = self.clone();
            let peer = peer.clone();
            tokio::spawn(async move {
                if let Err(e) = this.serve_request_stream(send, recv, peer, frame_len).await {
                    debug!("request stream error: {e}");
                }
            });
        }
    }

    async fn serve_request_stream(
        self: Arc<Self>,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        peer: PeerCtx,
        frame_len: u32,
    ) -> Result<()> {
        let request: Request = read_frame_limited(&mut recv, frame_len).await?;

        // A request newer than the negotiated minor is what a 1.4 node
        // would fail to decode: drop the stream (stream-scoped, the
        // connection lives on) exactly as that decoder would.
        if request.min_minor() > peer.minor {
            return Err(CoreError::Transport(format!(
                "peer {} negotiated 1.{} but sent a 1.{} request — dropping stream",
                peer.node_id.short(),
                peer.minor,
                request.min_minor()
            )));
        }

        // Purpose-scoped authorization at the transport edge.
        let allowed = match peer.purpose {
            ConnPurpose::Member => true,
            ConnPurpose::Join => {
                matches!(
                    request,
                    Request::Ping(_)
                        | Request::Join(_)
                        | Request::JoinV2(_)
                        | Request::JoinV3(_)
                        // Serves op history to whoever asks, so unlike its
                        // siblings it is *not* authorized by reaching here:
                        // the daemon additionally requires the peer to be a
                        // member, which a joiner becomes only by presenting
                        // an invite on this same connection.
                        | Request::JoinFetch { .. }
                )
            }
            ConnPurpose::Probe => matches!(request, Request::Ping(_)),
            ConnPurpose::Pair => matches!(
                request,
                Request::Ping(_)
                    | Request::PairInit(_)
                    | Request::PairReveal { .. }
                    | Request::PairConfirm
                    | Request::PairComplete(_)
                    | Request::PairCompleteV2(_)
                    | Request::PairCompleteV3(_)
            ),
        };
        if !allowed {
            write_frame(
                &mut send,
                &Response::Error {
                    message: format!("{:?} connections cannot make this request", peer.purpose),
                },
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        }

        // Stateless / transport-owned requests are answered here.
        match request {
            Request::Ping(p) => {
                write_frame(&mut send, &Response::Pong(p)).await?;
                let _ = send.finish();
                return Ok(());
            }
            Request::Observe => {
                let resp = if peer.kind == ConnKind::Relayed {
                    Response::Error {
                        message: "cannot observe over a relayed connection".into(),
                    }
                } else {
                    Response::Observed {
                        addr: meshsock::normalize(peer.remote_addr).to_string(),
                    }
                };
                write_frame(&mut send, &resp).await?;
                let _ = send.finish();
                return Ok(());
            }
            Request::RelayRegister => {
                if let Some(message) = self.relay_refusal(&peer) {
                    write_frame(&mut send, &Response::Error { message }).await?;
                    let _ = send.finish();
                    return Ok(());
                }
                return self.relay.serve_register(peer.node_id, send, recv).await;
            }
            Request::RelayOpen { target } => {
                if let Some(message) = self.relay_refusal(&peer) {
                    write_frame(&mut send, &Response::Error { message }).await?;
                    let _ = send.finish();
                    return Ok(());
                }
                return self
                    .relay
                    .serve_open(peer.node_id, target, send, recv)
                    .await;
            }
            Request::PunchRequest { target, candidates } => {
                let resp = if let Some(message) = self.relay_refusal(&peer) {
                    Response::Error { message }
                } else {
                    match self.relay.punch(peer.node_id, target, candidates) {
                        Ok(()) => Response::PunchSent,
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    }
                };
                write_frame(&mut send, &resp).await?;
                let _ = send.finish();
                return Ok(());
            }
            // M17 §3.3: hand both halves to the daemon and stop being a
            // request/response stream. Nothing is written here — not even
            // an acknowledgement — because from this point the frames are
            // the session's, and the daemon is the only party that knows
            // whether this one is allowed.
            // The retired mesh proxy: refused by name, so an older node's
            // proxy falls back to dialling the instance directly instead of
            // waiting out a dropped stream.
            Request::ServiceConnect { .. } => {
                write_frame(
                    &mut send,
                    &Response::Error {
                        message: SERVICE_CONNECT_RETIRED.into(),
                    },
                )
                .await?;
                let _ = send.finish();
                return Ok(());
            }
            Request::SessionOpen(spec) => {
                let handler = self.session_handler.lock().clone();
                match handler {
                    Some(handler) => {
                        handler(peer, *spec, send, recv);
                        return Ok(());
                    }
                    None => {
                        write_frame(
                            &mut send,
                            &Response::Error {
                                message: "this node is not accepting sessions".into(),
                            },
                        )
                        .await?;
                        let _ = send.finish();
                        return Ok(());
                    }
                }
            }
            _ => {}
        }

        // Everything else goes to the daemon.
        let (tx, mut rx) = mpsc::channel::<Response>(64);
        let event = RequestEvent {
            peer,
            request,
            reply: tx,
        };
        if self.events_tx.send(event).await.is_err() {
            write_frame(
                &mut send,
                &Response::Error {
                    message: "node is shutting down".into(),
                },
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        }
        while let Some(resp) = rx.recv().await {
            write_frame(&mut send, &resp).await?;
        }
        let _ = send.finish();
        Ok(())
    }
}

/// Resolve `host[:port]` to a socket address (default port 7777).
pub async fn resolve(addr: &str) -> Result<SocketAddr> {
    let candidate = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{}", crate::config::DEFAULT_PORT)
    };
    if let Ok(sa) = candidate.parse::<SocketAddr>() {
        return Ok(sa);
    }
    tokio::net::lookup_host(&candidate)
        .await
        .map_err(|e| CoreError::Transport(format!("resolve {candidate}: {e}")))?
        .next()
        .ok_or_else(|| CoreError::Transport(format!("no addresses for {candidate}")))
}
