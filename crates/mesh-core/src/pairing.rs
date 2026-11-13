//! LAN pairing — the AirDrop moment (MASTERPLAN §5.2).
//!
//! A fresh machine runs `meshnode join --pair`: it binds a QUIC endpoint on
//! its configured listen address — the port the daemon will use once it is a
//! member, so one firewall rule covers both — and announces itself over mDNS
//! (`_meshware-pair._udp`). Any mesh
//! member (CLI `meshnode invite approve`, or Director) discovers it, dials
//! it, and both sides derive the same **6-digit confirmation code** from a
//! commitment-based exchange bound to the two node keys that TLS proved
//! possession of. Both humans must confirm the code; then the approver
//! authors the membership exactly as a token join would (the admin's
//! single-use invite rides along for provenance) and delivers genesis + ops
//! over the same encrypted connection.
//!
//! Security model (numeric comparison, Bluetooth-NC style): the approver
//! commits to its nonce before the joiner reveals its own, node identities
//! are bound by key-pinned TLS (a MITM cannot present either victim's key),
//! so an attacker bridging two pairing connections shows each human an
//! independent uniformly-random code — they match with probability 10⁻⁶
//! per attempt. Confirmation is mandatory on both sides; there is no
//! TOFU fallback (MASTERPLAN §5 anti-goals).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::NodeConfig;
use crate::error::{CoreError, Result};
use crate::genesis::Genesis;
use crate::identity::Identity;
use crate::membership::{self, JoinReport};
use crate::state::StateEngine;
use crate::transport::{AdmissionCheck, LocalInfo, RequestEvent, Transport};
use mesh_proto::{
    BOOTSTRAP_MINOR, ConnPurpose, InviteToken, JoinRequest, NodeId, OpEnvelope, PairAccept,
    PairInit, Request, Response, SnapshotChunk,
};

/// mDNS service type candidates announce on while waiting to pair.
pub const PAIR_SERVICE: &str = "_meshware-pair._udp.local.";

/// Ceiling on the instalments one pairing delivers — the push-side twin of
/// `membership`'s pull-side bound, and there for the same reason: the mesh
/// keeps authoring while the snapshot streams, so `more` is allowed to stay
/// true for a while but not forever.
const MAX_PAIR_CHUNKS: usize = 10_000;

/// How long each side waits for its human (and the peer's) to confirm.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(300);
/// How long the joiner waits for an approver to show up at all.
const DEFAULT_WAIT: Duration = Duration::from_secs(600);
/// How long an approver's session holds the joiner before somebody else
/// may take its place, unless its code was confirmed (SEC-23). Without it,
/// anyone on the LAN who sent `PairInit` and went quiet held the joiner for
/// the whole wait, refusing the real approver as "already in progress".
const UNCONFIRMED_TTL: Duration = Duration::from_secs(30);
/// Joiners the background listener remembers at once, and addresses per
/// joiner (SEC-23). A LAN can announce more than a person can read.
const MAX_HEARD: usize = 256;
const MAX_ADDRS_PER_JOINER: usize = 16;

// ---------------------------------------------------------------------------
// The code: commitment + short-authentication-string
// ---------------------------------------------------------------------------

/// Commitment to the approver's nonce, sent before the joiner reveals its own.
pub fn commitment(nonce: &[u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mw-pair-commit-v1");
    hasher.update(nonce);
    *hasher.finalize().as_bytes()
}

/// The 6-digit confirmation code, derived identically on both sides from
/// the TLS-verified node keys and both nonces.
pub fn sas_code(
    approver: &NodeId,
    joiner: &NodeId,
    nonce_a: &[u8; 32],
    nonce_j: &[u8; 32],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mw-pair-sas-v1");
    hasher.update(&approver.0);
    hasher.update(&joiner.0);
    hasher.update(nonce_a);
    hasher.update(nonce_j);
    let digest = hasher.finalize();
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&digest.as_bytes()[..8]);
    format!("{:06}", u64::from_le_bytes(eight) % 1_000_000)
}

/// Render a code for humans: `123 456`.
///
/// Split on the *character* boundary, not the byte one (S-L2). A pairing
/// code this module produced is six ASCII digits, but this is reachable
/// from Director's `pair_code_view` command with an arbitrary string, and
/// `&code[..3]` on `"abécd"` — six bytes, five characters — panics.
pub fn format_code(code: &str) -> String {
    let mut chars = code.chars();
    let head: String = chars.by_ref().take(3).collect();
    let tail: String = chars.collect();
    if head.chars().count() == 3 && tail.chars().count() == 3 {
        format!("{head} {tail}")
    } else {
        code.to_string()
    }
}

/// What a pairing QR carries.
///
/// Not the bare digits. A scanner that shows `421733` has told the person
/// holding the phone nothing about what they are looking at or what to do
/// with it; a scheme prefix makes the photograph self-describing, and it
/// costs eight bytes in a symbol with room for forty-two.
pub fn qr_payload(code: &str) -> String {
    format!("mw-pair:{code}")
}

/// One pairing code, in every form a surface might show it.
///
/// **This exists so the two surfaces cannot drift.** The CLI on a headless
/// joiner and Director on the approver's laptop are the two screens a human
/// is being asked to compare, and a word pair or a QR that differed between
/// them would read to that human as an attack. There is one renderer, here,
/// and both call it. (Same rule as [`crate::release`]: one question, asked
/// in several places, answered in one.)
#[derive(Debug, Clone)]
pub struct CodeDisplay {
    /// The raw six digits.
    pub code: String,
    /// Grouped for reading aloud: `421 733`.
    pub grouped: String,
    /// The same six digits as two words (`crate::wordpair`) — a bijection,
    /// not a summary, so comparing the words *is* comparing the code.
    /// `None` only for a malformed code, which never reaches a screen.
    pub words: Option<String>,
    /// A QR of [`qr_payload`], for a console nobody can stand in front of.
    pub qr: Option<crate::qr::Symbol>,
}

/// Build every rendering of `code`.
pub fn display(code: &str) -> CodeDisplay {
    CodeDisplay {
        code: code.to_string(),
        grouped: format_code(code),
        words: crate::wordpair::phrase(code),
        // A QR that failed to encode is a missing picture, never a missing
        // pairing: the digits are the protocol and they are still on screen.
        qr: crate::qr::encode(qr_payload(code).as_bytes()).ok(),
    }
}

fn fresh_nonce() -> Result<[u8; 32]> {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce)
        .map_err(|e| CoreError::Membership(format!("system rng unavailable: {e}")))?;
    Ok(nonce)
}

// ---------------------------------------------------------------------------
// mDNS announce / browse
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Discovery: how an approver learns a joiner is waiting
// ---------------------------------------------------------------------------
//
// Two channels, and an approver listens on both:
//
//   * **mDNS** (`_meshware-pair._udp`) — the polite, standard one. It is
//     also the one that breaks: a Wi-Fi access point that filters
//     multicast, a firewall zone without the `mdns` service, a VPN
//     adapter that wins the default route, a second responder on 5353.
//     Every one of those failures is silent, and every one of them used to
//     read as "the Access page shows nothing".
//   * **A UDP beacon** on [`PAIR_BEACON_PORT`] — a small JSON datagram the
//     joiner broadcasts every couple of seconds, to the limited broadcast
//     address and to each interface's own. Broadcast needs no group
//     membership and no responder, so it survives most of what kills
//     multicast. It carries the same facts as the TXT record and, like the
//     TXT record, none of them are trusted: the pairing code is what
//     authenticates, and this only decides what the card says.
//
// Neither is polled in windows any more. A [`LanWatch`] runs for as long as
// somebody is in a position to say yes, keeps one record per joiner with
// when it was first and last heard, and ages a record out when the joiner
// goes quiet — which is what makes a card appear within a beacon interval
// of the wizard opening and disappear on its own when it closes.

/// UDP port joiners beacon on while waiting to pair — the broadcast twin
/// of the mDNS announcement, for networks where multicast does not arrive.
pub const PAIR_BEACON_PORT: u16 = 7778;
/// How often a waiting joiner beacons.
const BEACON_EVERY: Duration = Duration::from_millis(1_800);
/// A joiner that has not been heard from for this long has stopped
/// waiting: it paired, or the person closed the window.
const CANDIDATE_STALE: Duration = Duration::from_secs(12);

/// A candidate node discovered on the LAN, waiting to pair.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub node_name: String,
    /// Claimed id from the announcement — display only; the TLS handshake
    /// and the confirmation code are what actually authenticate.
    pub node_id: Option<NodeId>,
    /// Claimed platform (`linux/x86_64`) — display only.
    pub platform: Option<String>,
    /// Dialable pairing endpoints, best first: an address on a subnet this
    /// machine is also on, then other private addresses, then the rest.
    pub addrs: Vec<SocketAddr>,
    /// The person at the machine, as they typed it into the wizard. What
    /// the admin is really being asked about; the machine is incidental.
    pub person: Option<String>,
    /// The operating system's own name for the machine, which is not
    /// always the node name somebody chose.
    pub hostname: Option<String>,
    /// Heard over mDNS.
    pub via_mdns: bool,
    /// Heard over the UDP beacon.
    pub via_beacon: bool,
    /// When this joiner was first and most recently heard, as micros since
    /// the epoch — "waiting for 40s" is the difference between a card the
    /// admin just missed and one they have been ignoring.
    pub first_seen_micros: u64,
    pub last_seen_micros: u64,
}

/// What a running [`LanWatch`] knows right now.
#[derive(Debug, Clone, Default)]
pub struct LanReport {
    pub candidates: Vec<Candidate>,
    /// Why mDNS is not contributing, if it is not.
    pub mdns_error: Option<String>,
    /// Why the beacon listener is not contributing, if it is not.
    pub beacon_error: Option<String>,
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn os_platform() -> String {
    format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn os_hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The beacon datagram. JSON rather than postcard on purpose: it is read by
/// whatever version is on the other side, and a field the reader does not
/// know is simply ignored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Beacon {
    /// Always `"pair"`; anything else on the port is not ours.
    mw: String,
    v: u32,
    name: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    os: Option<String>,
    port: u16,
    #[serde(default)]
    who: Option<String>,
    #[serde(default)]
    host: Option<String>,
}

/// Every IPv4 interface with a usable address: `(ip, netmask, broadcast)`.
/// Loopback and link-local are out — nobody pairs over either.
fn local_v4() -> Vec<(
    std::net::Ipv4Addr,
    std::net::Ipv4Addr,
    Option<std::net::Ipv4Addr>,
)> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    ifaces
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4)
                if !v4.ip.is_loopback() && !v4.ip.is_link_local() && !v4.ip.is_unspecified() =>
            {
                Some((v4.ip, v4.netmask, v4.broadcast))
            }
            _ => None,
        })
        .collect()
}

fn same_subnet(a: std::net::Ipv4Addr, b: std::net::Ipv4Addr, mask: std::net::Ipv4Addr) -> bool {
    u32::from(a) & u32::from(mask) == u32::from(b) & u32::from(mask)
}

/// Order pairing addresses best-first and drop the ones that cannot work.
///
/// A joiner with Docker installed announces `172.17.0.1` alongside its real
/// address, and "dial the first one" used to pick whichever mDNS listed
/// first. On-link first, then private, then anything else; loopback and
/// link-local never.
pub fn rank_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    let local = local_v4();
    let score = |a: &SocketAddr| -> u8 {
        match a.ip() {
            std::net::IpAddr::V4(ip) => {
                if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
                    return 9;
                }
                if local.iter().any(|(l, m, _)| same_subnet(*l, ip, *m)) {
                    return 0;
                }
                if ip.is_private() {
                    return 1;
                }
                2
            }
            std::net::IpAddr::V6(ip) => {
                if ip.is_loopback() || ip.is_unspecified() || ip.is_unicast_link_local() {
                    9
                } else {
                    3
                }
            }
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(u8, SocketAddr)> = addrs
        .into_iter()
        .filter(|a| seen.insert(*a))
        .map(|a| (score(&a), a))
        .filter(|(s, _)| *s < 9)
        .collect();
    out.sort_by_key(|(s, _)| *s);
    out.into_iter().map(|(_, a)| a).collect()
}

/// The addresses a joiner listening on `port` can be dialled at — for the
/// wizard to show, so an admin whose Director cannot see the joiner can be
/// read one and type it in.
pub fn local_endpoints(port: u16) -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = local_v4()
        .into_iter()
        .map(|(ip, _, _)| SocketAddr::new(std::net::IpAddr::V4(ip), port))
        .collect();
    // Private before public, for the same reason as `rank_addrs`.
    v.sort_by_key(|a| match a.ip() {
        std::net::IpAddr::V4(ip) if ip.is_private() => 0,
        _ => 1,
    });
    v
}

/// One UDP socket that can share the beacon port with another listener on
/// this host (two Directors, or a Director beside a daemon) and, for the
/// sender, reach a broadcast address.
fn beacon_socket(bind: SocketAddr, broadcast: bool) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    if broadcast {
        sock.set_broadcast(true)?;
    }
    sock.set_nonblocking(true)?;
    sock.bind(&bind.into())?;
    tokio::net::UdpSocket::from_std(sock.into())
}

/// Announce this machine as pairable. Returns a guard; dropping it
/// withdraws the mDNS record and stops the beacon.
///
/// mDNS failing is not fatal here: the beacon still goes out, and the
/// node is still dialable by address. It is reported so the wizard can say
/// which channel is working.
pub struct Announcement {
    mdns: Option<(mdns_sd::ServiceDaemon, String)>,
    beacon: Option<tokio::task::JoinHandle<()>>,
    /// Why mDNS is not announcing, if it is not.
    pub mdns_error: Option<String>,
}

impl std::fmt::Debug for Announcement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.mdns {
            Some((_, name)) => write!(f, "Announcement({name})"),
            None => write!(f, "Announcement(beacon only)"),
        }
    }
}

impl Drop for Announcement {
    fn drop(&mut self) {
        if let Some((daemon, fullname)) = self.mdns.take() {
            let _ = daemon.unregister(&fullname);
            let _ = daemon.shutdown();
        }
        if let Some(task) = self.beacon.take() {
            task.abort();
        }
    }
}

/// Announce over mDNS and beacon over UDP until the guard is dropped.
/// `person` is who is sitting at the machine, if the joiner said.
pub fn announce(
    node_name: &str,
    node_id: &NodeId,
    port: u16,
    person: Option<&str>,
) -> Result<Announcement> {
    let hostname = os_hostname();
    let platform = os_platform();
    let person = person
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);

    let mdns_result = (|| -> Result<(mdns_sd::ServiceDaemon, String)> {
        let daemon = mdns_sd::ServiceDaemon::new()
            .map_err(|e| CoreError::Membership(format!("mdns daemon: {e}")))?;
        let host = format!("{node_name}.local.");
        let mut props: Vec<(&str, String)> = vec![
            ("id", node_id.to_display_string()),
            ("v", "1".to_string()),
            ("os", platform.clone()),
            ("host", hostname.clone()),
        ];
        if let Some(p) = &person {
            props.push(("who", p.clone()));
        }
        let info = mdns_sd::ServiceInfo::new(PAIR_SERVICE, node_name, &host, "", port, &props[..])
            .map_err(|e| CoreError::Membership(format!("mdns service: {e}")))?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon
            .register(info)
            .map_err(|e| CoreError::Membership(format!("mdns register: {e}")))?;
        debug!(service = %fullname, port, "announcing pairing candidate over mDNS");
        Ok((daemon, fullname))
    })();
    let (mdns, mdns_error) = match mdns_result {
        Ok(m) => (Some(m), None),
        Err(e) => {
            warn!("mDNS announce failed ({e}) — beaconing over UDP broadcast only");
            (None, Some(e.to_string()))
        }
    };

    let beacon = Beacon {
        mw: "pair".into(),
        v: 1,
        name: node_name.to_string(),
        id: Some(node_id.to_display_string()),
        os: Some(platform),
        port,
        who: person,
        host: Some(hostname),
    };
    let payload = serde_json::to_vec(&beacon).unwrap_or_default();
    let beacon = match beacon_socket("0.0.0.0:0".parse().expect("static addr"), true) {
        Ok(sock) => Some(tokio::spawn(async move {
            loop {
                // The limited broadcast leaves by the default route only;
                // each interface's directed broadcast covers the others.
                let mut targets: Vec<std::net::Ipv4Addr> = vec![std::net::Ipv4Addr::BROADCAST];
                for (_, _, b) in local_v4() {
                    if let Some(b) = b
                        && !targets.contains(&b)
                    {
                        targets.push(b);
                    }
                }
                for t in targets {
                    let _ = sock
                        .send_to(
                            &payload,
                            SocketAddr::new(std::net::IpAddr::V4(t), PAIR_BEACON_PORT),
                        )
                        .await;
                }
                tokio::time::sleep(BEACON_EVERY).await;
            }
        })),
        Err(e) => {
            warn!("pairing beacon socket failed ({e}) — mDNS only");
            None
        }
    };
    if mdns.is_none() && beacon.is_none() {
        return Err(CoreError::Membership(
            "could not announce this machine: neither mDNS nor UDP broadcast is available".into(),
        ));
    }
    Ok(Announcement {
        mdns,
        beacon,
        mdns_error,
    })
}

/// One joiner as the watch has heard it, with the raw addresses per
/// channel so a re-announcement can replace rather than accumulate.
#[derive(Debug, Clone)]
struct Heard {
    candidate: Candidate,
    mdns_addrs: Vec<SocketAddr>,
    beacon_addrs: Vec<SocketAddr>,
}

/// What one announcement or beacon claimed. Unverified, by design.
#[derive(Debug)]
struct Claim {
    node_name: String,
    node_id: Option<NodeId>,
    platform: Option<String>,
    person: Option<String>,
    hostname: Option<String>,
    addrs: Vec<SocketAddr>,
    /// Heard over the beacon (else mDNS).
    beacon: bool,
}

#[derive(Debug, Default)]
struct WatchState {
    heard: std::collections::HashMap<String, Heard>,
    mdns_error: Option<String>,
    beacon_error: Option<String>,
}

impl WatchState {
    /// The key one joiner is filed under: its claimed id where it has one,
    /// so two machines that happen to share a name stay two cards.
    fn key(node_id: Option<&NodeId>, node_name: &str) -> String {
        match node_id {
            Some(id) => id.to_display_string(),
            None => format!("name:{node_name}"),
        }
    }

    fn hear(&mut self, claim: Claim) {
        let Claim {
            node_name,
            node_id,
            platform,
            person,
            hostname,
            addrs,
            beacon,
        } = claim;
        let now = now_micros();
        let key = Self::key(node_id.as_ref(), &node_name);
        if !self.heard.contains_key(&key) && self.heard.len() >= MAX_HEARD {
            let stale = CANDIDATE_STALE.as_micros() as u64;
            self.heard
                .retain(|_, h| now.saturating_sub(h.candidate.last_seen_micros) <= stale);
            if self.heard.len() >= MAX_HEARD
                && let Some(oldest) = self
                    .heard
                    .iter()
                    .min_by_key(|(_, h)| h.candidate.last_seen_micros)
                    .map(|(k, _)| k.clone())
            {
                self.heard.remove(&oldest);
            }
        }
        let entry = self.heard.entry(key).or_insert_with(|| Heard {
            candidate: Candidate {
                node_name: node_name.clone(),
                node_id,
                platform: None,
                addrs: Vec::new(),
                person: None,
                hostname: None,
                via_mdns: false,
                via_beacon: false,
                first_seen_micros: now,
                last_seen_micros: now,
            },
            mdns_addrs: Vec::new(),
            beacon_addrs: Vec::new(),
        });
        let c = &mut entry.candidate;
        c.node_name = node_name;
        c.last_seen_micros = now;
        if platform.is_some() {
            c.platform = platform;
        }
        if person.is_some() {
            c.person = person;
        }
        if hostname.is_some() {
            c.hostname = hostname;
        }
        if beacon {
            c.via_beacon = true;
            for a in addrs {
                if !entry.beacon_addrs.contains(&a) {
                    entry.beacon_addrs.push(a);
                }
            }
            // The newest addresses are the ones worth trying.
            let excess = entry
                .beacon_addrs
                .len()
                .saturating_sub(MAX_ADDRS_PER_JOINER);
            entry.beacon_addrs.drain(..excess);
        } else {
            c.via_mdns = true;
            entry.mdns_addrs = addrs;
            entry.mdns_addrs.truncate(MAX_ADDRS_PER_JOINER);
        }
        entry.candidate.addrs = rank_addrs(
            entry
                .mdns_addrs
                .iter()
                .chain(entry.beacon_addrs.iter())
                .copied(),
        );
    }

    fn report(&mut self) -> LanReport {
        let now = now_micros();
        let stale = CANDIDATE_STALE.as_micros() as u64;
        self.heard
            .retain(|_, h| now.saturating_sub(h.candidate.last_seen_micros) <= stale);
        let mut candidates: Vec<Candidate> = self
            .heard
            .values()
            .filter(|h| !h.candidate.addrs.is_empty())
            .map(|h| h.candidate.clone())
            .collect();
        // Oldest first: the person who has been waiting longest is at the
        // top, and a new arrival never shoves the card somebody is about
        // to click.
        candidates.sort_by_key(|c| c.first_seen_micros);
        LanReport {
            candidates,
            mdns_error: self.mdns_error.clone(),
            beacon_error: self.beacon_error.clone(),
        }
    }
}

/// A long-lived listener for joiners on this network — see the module
/// section above. Start it once, ask it what it knows whenever a screen
/// wants to draw, drop it when nobody is in a position to approve.
pub struct LanWatch {
    state: Arc<parking_lot::Mutex<WatchState>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    mdns: Option<mdns_sd::ServiceDaemon>,
    beacon: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for LanWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanWatch")
            .field("mdns", &self.mdns.is_some())
            .field("beacon", &self.beacon.is_some())
            .finish()
    }
}

impl Drop for LanWatch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(daemon) = self.mdns.take() {
            let _ = daemon.stop_browse(PAIR_SERVICE);
            let _ = daemon.shutdown();
        }
        if let Some(task) = self.beacon.take() {
            task.abort();
        }
    }
}

impl LanWatch {
    /// Start listening on both channels. Never fails: a channel that
    /// cannot start is reported in every [`LanReport`] instead, because
    /// "why is nothing appearing" needs an answer on the screen that is
    /// showing nothing.
    pub fn start() -> LanWatch {
        let state = Arc::new(parking_lot::Mutex::new(WatchState::default()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // -- mDNS: one daemon, one browse, pumped on its own thread ------
        let mdns = match mdns_sd::ServiceDaemon::new()
            .map_err(|e| format!("mdns daemon: {e}"))
            .and_then(|d| {
                d.browse(PAIR_SERVICE)
                    .map(|rx| (d, rx))
                    .map_err(|e| format!("mdns browse: {e}"))
            }) {
            Ok((daemon, rx)) => {
                let st = Arc::clone(&state);
                let stop_flag = Arc::clone(&stop);
                std::thread::Builder::new()
                    .name("mw-pair-mdns".into())
                    .spawn(move || {
                        while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            match rx.recv_timeout(Duration::from_millis(500)) {
                                Ok(mdns_sd::ServiceEvent::ServiceResolved(svc)) => {
                                    let node_name = svc
                                        .fullname
                                        .strip_suffix(&format!(".{PAIR_SERVICE}"))
                                        .unwrap_or(&svc.fullname)
                                        .to_string();
                                    let txt = |k: &str| {
                                        svc.txt_properties
                                            .get_property_val_str(k)
                                            .map(str::to_string)
                                            .filter(|s| !s.is_empty())
                                    };
                                    let node_id = txt("id").and_then(|s| NodeId::parse(&s).ok());
                                    let hostname = txt("host").or_else(|| {
                                        Some(
                                            svc.get_hostname()
                                                .trim_end_matches('.')
                                                .trim_end_matches(".local")
                                                .to_string(),
                                        )
                                        .filter(|s| !s.is_empty())
                                    });
                                    let addrs: Vec<SocketAddr> = svc
                                        .get_addresses_v4()
                                        .into_iter()
                                        .map(|ip| {
                                            SocketAddr::new(std::net::IpAddr::V4(ip), svc.port)
                                        })
                                        .collect();
                                    if addrs.is_empty() {
                                        continue;
                                    }
                                    {
                                        let mut s = st.lock();
                                        s.hear(Claim {
                                            node_name,
                                            node_id,
                                            platform: txt("os"),
                                            person: txt("who"),
                                            hostname,
                                            addrs,
                                            beacon: false,
                                        });
                                    }
                                }
                                Ok(mdns_sd::ServiceEvent::ServiceRemoved(_, fullname)) => {
                                    // Withdrawn: it paired, or the wizard closed.
                                    // Aging out would get there too, but a card
                                    // that lingers after the machine is in is the
                                    // one thing this queue must not do.
                                    let node_name = fullname
                                        .strip_suffix(&format!(".{PAIR_SERVICE}"))
                                        .unwrap_or(&fullname)
                                        .to_string();
                                    {
                                        let mut s = st.lock();
                                        s.heard.retain(|_, h| {
                                            h.candidate.node_name != node_name
                                                || h.candidate.via_beacon
                                        });
                                        for h in s
                                            .heard
                                            .values_mut()
                                            .filter(|h| h.candidate.node_name == node_name)
                                        {
                                            h.candidate.via_mdns = false;
                                            h.mdns_addrs.clear();
                                        }
                                    }
                                }
                                Ok(_) => {}
                                Err(_) if rx.is_disconnected() => break,
                                Err(_) => {}
                            }
                        }
                    })
                    .ok();
                Some(daemon)
            }
            Err(e) => {
                warn!("LAN watch: {e} — listening for pairing beacons only");
                state.lock().mdns_error = Some(e);
                None
            }
        };

        // -- the beacon: one socket on the well-known port -----------------
        let beacon = match beacon_socket(
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                PAIR_BEACON_PORT,
            ),
            false,
        ) {
            Ok(sock) => {
                let st = Arc::clone(&state);
                Some(tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    loop {
                        let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            continue;
                        };
                        let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) else {
                            continue;
                        };
                        if b.mw != "pair" || b.name.is_empty() || b.port == 0 {
                            continue;
                        }
                        let addr = SocketAddr::new(from.ip(), b.port);
                        {
                            let mut s = st.lock();
                            s.hear(Claim {
                                node_name: b.name,
                                node_id: b.id.as_deref().and_then(|s| NodeId::parse(s).ok()),
                                platform: b.os,
                                person: b.who,
                                hostname: b.host,
                                addrs: vec![addr],
                                beacon: true,
                            });
                        }
                    }
                }))
            }
            Err(e) => {
                warn!(
                    "LAN watch: beacon port udp/{PAIR_BEACON_PORT} unavailable ({e}) — mDNS only"
                );
                state.lock().beacon_error = Some(format!("udp/{PAIR_BEACON_PORT}: {e}"));
                None
            }
        };

        LanWatch {
            state,
            stop,
            mdns,
            beacon,
        }
    }

    /// Everything heard recently, best address first, oldest joiner first.
    pub fn report(&self) -> LanReport {
        self.state.lock().report()
    }
}

/// Browse the LAN for pairing candidates for `wait` — a one-shot
/// [`LanWatch`], for the CLI and for anything else that asks once.
pub async fn browse(wait: Duration) -> Result<Vec<Candidate>> {
    let watch = LanWatch::start();
    tokio::time::sleep(wait).await;
    let report = watch.report();
    if report.candidates.is_empty()
        && let (Some(m), Some(b)) = (&report.mdns_error, &report.beacon_error)
    {
        return Err(CoreError::Membership(format!(
            "cannot listen for joiners on this network: {m}; {b}"
        )));
    }
    Ok(report.candidates)
}

// ---------------------------------------------------------------------------
// Approver side (runs inside the daemon, driven over IPC / by Director)
// ---------------------------------------------------------------------------

/// Progress the frontend needs to surface.
#[derive(Debug)]
pub enum PairEvent {
    /// Both sides can now display the code; the human must compare + decide.
    Code {
        code: String,
        joiner_id: NodeId,
        joiner_name: String,
    },
}

/// A successfully paired node.
#[derive(Debug, Clone)]
pub struct PairOutcome {
    pub node_id: NodeId,
    pub node_name: String,
}

/// Dial a candidate and run the approver side of the exchange. `events`
/// surfaces the code; `decision` resolves when the approving human accepts
/// or rejects it. On acceptance the membership is authored (via the invite
/// token, exactly like a token join) and genesis + ops are delivered.
pub async fn pair_approve(
    transport: &Arc<Transport>,
    engine: &Arc<StateEngine>,
    addr: SocketAddr,
    token: InviteToken,
    events: mpsc::Sender<PairEvent>,
    decision: oneshot::Receiver<bool>,
) -> Result<PairOutcome> {
    membership::validate_token(engine, &token)?;

    let (conn, ack) = transport.connect_for_pair(addr).await?;
    if !ack.accepted {
        return Err(CoreError::HandshakeRefused(
            ack.reason.unwrap_or_else(|| "refused".into()),
        ));
    }
    let joiner_id = ack.node_id;

    // Round 1: commit, and learn the joiner's nonce + join fields.
    let nonce_a = fresh_nonce()?;
    let init = Request::PairInit(PairInit {
        commitment: commitment(&nonce_a),
        mesh_name: engine.genesis().body.mesh_name.clone(),
        approver_name: transport.node_id().short(),
    });
    let accept: PairAccept = match transport.request_one(&conn, &init).await? {
        Response::PairAccept(a) => a,
        Response::Error { message } => return Err(CoreError::Membership(message)),
        other => {
            return Err(CoreError::Membership(format!(
                "unexpected pairing response: {other:?}"
            )));
        }
    };

    // Round 2: reveal; both sides can now derive the code.
    match transport
        .request_one(&conn, &Request::PairReveal { nonce: nonce_a })
        .await?
    {
        Response::PairReady => {}
        Response::Error { message } => return Err(CoreError::Membership(message)),
        other => {
            return Err(CoreError::Membership(format!(
                "unexpected pairing response: {other:?}"
            )));
        }
    }
    // A name the join would refuse is refused before anyone is asked to
    // compare a code under it (SEC-9).
    if let Some(problem) = mesh_proto::text::name_problem(&accept.node_name) {
        conn.close(0u32.into(), b"bad name");
        return Err(CoreError::Membership(format!(
            "the joining machine announced an unusable name: {problem}"
        )));
    }
    let code = sas_code(&transport.node_id(), &joiner_id, &nonce_a, &accept.nonce);
    let _ = events
        .send(PairEvent::Code {
            code,
            joiner_id,
            joiner_name: accept.node_name.clone(),
        })
        .await;

    // The approving human compares the codes.
    let approved = tokio::time::timeout(CONFIRM_TIMEOUT, decision)
        .await
        .map_err(|_| CoreError::Membership("pairing confirmation timed out".into()))?
        .unwrap_or(false);
    if !approved {
        conn.close(0u32.into(), b"declined");
        return Err(CoreError::Membership(
            "pairing declined on this side".into(),
        ));
    }

    // Round 3: our human said yes; ask for the joiner's verdict (this blocks
    // on their prompt).
    let their_verdict = tokio::time::timeout(
        CONFIRM_TIMEOUT,
        transport.request_one(&conn, &Request::PairConfirm),
    )
    .await
    .map_err(|_| CoreError::Membership("timed out waiting for the joiner to confirm".into()))??;
    match their_verdict {
        Response::PairDecision { accepted: true } => {}
        Response::PairDecision { accepted: false } => {
            return Err(CoreError::Membership(
                "the joining node declined the code".into(),
            ));
        }
        Response::Error { message } => return Err(CoreError::Membership(message)),
        other => {
            return Err(CoreError::Membership(format!(
                "unexpected pairing response: {other:?}"
            )));
        }
    }

    // Both humans confirmed: author the membership exactly like a token
    // join (validates + consumes the invite), then deliver genesis + ops.
    let join_req = JoinRequest {
        token,
        node_name: accept.node_name.clone(),
        listen_port: accept.listen_port,
        addrs: accept.addrs.clone(),
        extra_tags: accept.extra_tags.clone(),
        slots: accept.slots,
    };
    // The pairing handshake negotiated a minor like any other connection,
    // so the snapshot goes out in the shape the joiner can read (F0 §3.3).
    let minor = mesh_proto::negotiate(
        (mesh_proto::PROTOCOL_MAJOR, transport.advertised_minor()),
        (ack.proto_major, ack.proto_minor),
    )
    .map_err(CoreError::Membership)?;
    if minor < mesh_proto::OPLOG_MINOR
        && let Some(message) = membership::legacy_join_refusal(engine)
    {
        return Err(CoreError::Membership(message));
    }
    if minor >= BOOTSTRAP_MINOR {
        push_snapshot(transport, &conn, engine, joiner_id, &join_req).await?;
    } else {
        let join_accept =
            membership::handle_join(engine, joiner_id, conn.remote_address().ip(), &join_req)?;
        let complete = if minor >= mesh_proto::OPLOG_MINOR {
            if let Some(message) = membership::oversized_snapshot_refusal(&join_accept, minor) {
                return Err(CoreError::Membership(message));
            }
            Request::PairCompleteV2(Box::new(join_accept))
        } else {
            let downgraded = membership::downgrade_join(&join_accept);
            if let Some(message) = membership::oversized_snapshot_refusal(&downgraded, minor) {
                return Err(CoreError::Membership(message));
            }
            Request::PairComplete(Box::new(downgraded))
        };
        expect_ok(transport, &conn, &complete).await?;
    }
    conn.close(0u32.into(), b"paired");
    info!(joiner = %joiner_id.short(), name = %accept.node_name, "node paired into the mesh");
    Ok(PairOutcome {
        node_id: joiner_id,
        node_name: accept.node_name,
    })
}

// ---------------------------------------------------------------------------
// Joiner side (runs in `meshnode join --pair`, no daemon needed)
// ---------------------------------------------------------------------------

/// What the joiner's human sees when the code is ready.
#[derive(Debug, Clone)]
pub struct PairPrompt {
    pub code: String,
    pub mesh_name: String,
    pub approver_name: String,
    pub approver_id: NodeId,
}

/// Frontend hooks for the joiner side: `on_code` fires when the code is
/// displayable; `decision` must then resolve with the human's verdict.
/// `on_listening` (optional) reports the bound pairing endpoint — used by
/// tests (mDNS-free) and by Director's wizard to display it.
#[derive(Debug)]
pub struct JoinerUx {
    pub on_code: mpsc::Sender<PairPrompt>,
    pub decision: oneshot::Receiver<bool>,
    pub on_listening: Option<oneshot::Sender<SocketAddr>>,
    /// Who is at this machine, if they said — announced alongside the
    /// node name so the approver is asked about a person, not a hostname.
    pub person: Option<String>,
    /// Fires once announcing has been attempted, with why mDNS is not
    /// contributing if it is not — the wizard says which channel is live.
    pub on_announced: Option<oneshot::Sender<Option<String>>>,
}

struct Session {
    peer: NodeId,
    started: tokio::time::Instant,
    commitment: [u8; 32],
    nonce_j: [u8; 32],
    mesh_name: String,
    approver_name: String,
    /// Set once the code round completed and the local human approved.
    confirmed: bool,
    /// Snapshot instalments received so far. Held here rather than written
    /// as they arrive: genesis is verified before anything touches the data
    /// dir, so an abandoned pairing leaves no half-built mesh behind.
    genesis_json: Vec<u8>,
    ops: Vec<OpEnvelope>,
}

/// Deliver the snapshot to a 1.10 joiner in instalments.
///
/// Pairing dials the other way round from a token join — the approver is
/// the client here — so the loop lives on the *sending* side and the
/// joiner learns the transfer is complete from the terminal chunk's
/// `more == false` rather than by asking again.
async fn push_snapshot(
    transport: &Transport,
    conn: &quinn::Connection,
    engine: &StateEngine,
    joiner_id: NodeId,
    join_req: &JoinRequest,
) -> Result<()> {
    let mut chunk =
        membership::handle_join_chunked(engine, joiner_id, conn.remote_address().ip(), join_req)?;
    let mut sent = membership::SnapshotProgress::default();
    for n in 0..MAX_PAIR_CHUNKS {
        sent.observe(&chunk.ops);
        let more = chunk.more;
        debug!(
            chunk = n,
            ops = chunk.ops.len(),
            more,
            "delivering snapshot"
        );
        expect_ok(transport, conn, &Request::PairCompleteV3(Box::new(chunk))).await?;
        if !more {
            return Ok(());
        }
        chunk = membership::snapshot_chunk(engine, &sent.vv(), false)?;
    }
    Err(CoreError::Membership(format!(
        "this mesh is still producing history after {MAX_PAIR_CHUNKS} snapshot instalments"
    )))
}

/// Send a pairing request that the joiner answers with a bare acknowledgement.
async fn expect_ok(
    transport: &Transport,
    conn: &quinn::Connection,
    request: &Request,
) -> Result<()> {
    match transport.request_one(conn, request).await? {
        Response::Ok => Ok(()),
        Response::Error { message } => Err(CoreError::Membership(format!(
            "joiner failed to persist the mesh: {message}"
        ))),
        other => Err(CoreError::Membership(format!(
            "unexpected pairing response: {other:?}"
        ))),
    }
}

/// Wait on the LAN for a mesh member to pair with this machine, then join.
/// Binds a pairing-only QUIC endpoint on the configured listen address,
/// announces over mDNS, runs the exchange,
/// and persists genesis + ops on success — after this, `meshnode up`
/// participates as a member (mirrors [`membership::join_mesh`]).
pub async fn pair_join(
    config: &NodeConfig,
    ux: JoinerUx,
    wait: Option<Duration>,
) -> Result<JoinReport> {
    pair_join_until(config, ux, wait, std::future::pending()).await
}

/// [`pair_join`], given up the moment `cancel` completes (DIR-6).
///
/// The CLI ends a wait with Ctrl-C, which takes the whole process and its
/// sockets with it. Director's wizard is a window that stays open, and its
/// ten-minute wait used to have no way out: Back did nothing and there was
/// no Cancel, even on the screen telling people to go back and use an
/// invite. A cancel closes the pairing endpoint and stops announcing, the
/// way the deadline does, and ends in an error. Nothing has been written by
/// then — genesis is only persisted once the snapshot is complete — so the
/// data dir is as empty as it was.
///
/// A cancel while the code is on screen answers the approver "declined",
/// as pressing *They differ* would.
pub async fn pair_join_until(
    config: &NodeConfig,
    ux: JoinerUx,
    wait: Option<Duration>,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<JoinReport> {
    tokio::pin!(cancel);
    if Genesis::load(&config.data_dir)?.is_some() {
        return Err(CoreError::Membership(
            "this node is already in a mesh (genesis exists). Remove the data dir to start over."
                .into(),
        ));
    }
    let identity = Identity::load_or_generate(&config.data_dir)?;
    let me = identity.node.node_id();
    let local = LocalInfo {
        node_id: me,
        node_name: config.node_name.clone(),
        mesh_id: None,
        mesh_name: None,
    };
    // Pairing connections only; everything else is refused at admission.
    let admission: AdmissionCheck = Arc::new(|_, purpose| {
        if purpose == ConnPurpose::Pair {
            Ok(())
        } else {
            Err("this node is waiting to pair and accepts only pairing connections".into())
        }
    });
    // The pairing endpoint binds the address this node is *configured* to
    // listen on, so the firewall rule the daemon needs anyway is the same one
    // that admits the pairing dial — a WAN pairing costs no temporary hole on
    // an ephemeral port. Nothing can be holding it: pairing only runs before
    // genesis, so this data dir has no daemon yet. If something else on the
    // host does, fall back to an ephemeral port — mDNS announces whatever we
    // land on, and Director configures `:0` deliberately (it is a roaming
    // peer, not a rendezvous).
    let (transport, mut events) = match Transport::bind(
        config.listen,
        identity.node.clone(),
        local.clone(),
        admission.clone(),
        config.bind_options(),
    )
    .await
    {
        Ok(bound) => bound,
        Err(e) if config.listen.port() != 0 => {
            warn!(
                "pairing could not bind {} ({e}) — falling back to an ephemeral port",
                config.listen
            );
            Transport::bind(
                "0.0.0.0:0".parse().expect("valid bind"),
                identity.node.clone(),
                local,
                admission,
                config.bind_options(),
            )
            .await?
        }
        Err(e) => return Err(e),
    };
    let mut bound = transport.local_addr()?;
    if bound.ip().is_unspecified() {
        // Report a dialable endpoint (mDNS advertises the real interface
        // addresses; this one serves tests and the wizard display).
        bound.set_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    let JoinerUx {
        on_code,
        mut decision,
        on_listening,
        person,
        on_announced,
    } = ux;
    // Announcing is best-effort: without it the node is still dialable by
    // address (tests, networks that block both multicast and broadcast).
    let _announcement = match announce(&config.node_name, &me, bound.port(), person.as_deref()) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!("announce failed ({e}) — pair by address instead");
            None
        }
    };
    info!(port = bound.port(), node = %me.short(), "waiting to pair (announcing over mDNS and UDP beacon)");
    if let Some(tx) = on_announced {
        let _ = tx.send(match &_announcement {
            Some(a) => a.mdns_error.clone(),
            None => Some("could not announce on this network".into()),
        });
    }

    if let Some(tx) = on_listening {
        let _ = tx.send(bound);
    }
    let mut decision_answer: Option<bool> = None;
    let mut session: Option<Session> = None;
    // `--wait 1000000y` is a person saying "as long as it takes", not a
    // reason to panic on an overflowing clock (SEC-23).
    let now = tokio::time::Instant::now();
    let deadline = now
        .checked_add(wait.unwrap_or(DEFAULT_WAIT))
        .unwrap_or_else(|| now + Duration::from_secs(100 * 365 * 86_400));

    loop {
        let event: RequestEvent = tokio::select! {
            ev = events.recv() => match ev {
                Some(ev) => ev,
                None => {
                    return Err(CoreError::Membership("pairing endpoint closed".into()));
                }
            },
            _ = tokio::time::sleep_until(deadline) => {
                transport.close();
                return Err(CoreError::Membership(
                    "timed out waiting for a mesh member to pair".into(),
                ));
            }
            _ = &mut cancel => {
                transport.close();
                return Err(CoreError::Membership("pairing cancelled".into()));
            }
        };
        let RequestEvent {
            peer,
            request,
            reply,
        } = event;
        // One approver at a time: a different dialer mid-session is refused
        // — unless that session has sat unconfirmed past its time, in which
        // case a new one may start. The code both people compare is what
        // keeps a hijacked session from getting anywhere.
        if let Some(s) = &session
            && s.peer != peer.node_id
            && !s.confirmed
            && s.started.elapsed() >= UNCONFIRMED_TTL
            && matches!(request, Request::PairInit(_))
        {
            warn!(
                stale = %s.peer.short(),
                new = %peer.node_id.short(),
                "a pairing went unconfirmed for {}s; another approver takes its place",
                UNCONFIRMED_TTL.as_secs()
            );
            session = None;
        }
        if let Some(s) = &session
            && s.peer != peer.node_id
            && !matches!(request, Request::Ping(_))
        {
            let _ = reply
                .send(Response::Error {
                    message: "another pairing is already in progress".into(),
                })
                .await;
            continue;
        }
        match request {
            Request::PairInit(init) => {
                let nonce_j = fresh_nonce()?;
                session = Some(Session {
                    peer: peer.node_id,
                    started: tokio::time::Instant::now(),
                    commitment: init.commitment,
                    nonce_j,
                    // Unauthenticated text, about to be printed above the
                    // code a person compares (SEC-9).
                    mesh_name: mesh_proto::text::clean(
                        &init.mesh_name,
                        mesh_proto::text::MAX_NAME_CHARS,
                    ),
                    approver_name: mesh_proto::text::clean(
                        &init.approver_name,
                        mesh_proto::text::MAX_NAME_CHARS,
                    ),
                    confirmed: false,
                    genesis_json: Vec::new(),
                    ops: Vec::new(),
                });
                let _ = reply
                    .send(Response::PairAccept(PairAccept {
                        nonce: nonce_j,
                        node_name: config.node_name.clone(),
                        listen_port: config.listen.port(),
                        addrs: Vec::new(),
                        // No runtimes yet — see `membership`: the probes
                        // have not run on a node that is still pairing, and
                        // an absent `cap.rt` is *unknown*, not *none*.
                        extra_tags: crate::capacity::announced_tags(config, &[]),
                        slots: config.limits.slots,
                    }))
                    .await;
            }
            Request::PairReveal { nonce } => {
                let Some(s) = session.as_ref() else {
                    let _ = reply
                        .send(Response::Error {
                            message: "pairing not initiated".into(),
                        })
                        .await;
                    continue;
                };
                if commitment(&nonce) != s.commitment {
                    warn!(peer = %peer.node_id.short(), "pairing commitment mismatch — possible tampering");
                    let _ = reply
                        .send(Response::Error {
                            message: "commitment mismatch".into(),
                        })
                        .await;
                    session = None;
                    continue;
                }
                let code = sas_code(&peer.node_id, &me, &nonce, &s.nonce_j);
                let _ = reply.send(Response::PairReady).await;
                let _ = on_code
                    .send(PairPrompt {
                        code,
                        mesh_name: s.mesh_name.clone(),
                        approver_name: s.approver_name.clone(),
                        approver_id: peer.node_id,
                    })
                    .await;
            }
            Request::PairConfirm => {
                if session.is_none() {
                    let _ = reply
                        .send(Response::Error {
                            message: "pairing not initiated".into(),
                        })
                        .await;
                    continue;
                }
                // Await the local human's verdict (once; remembered after).
                let accepted = match decision_answer {
                    Some(v) => v,
                    None => {
                        let v = tokio::select! {
                            r = tokio::time::timeout(CONFIRM_TIMEOUT, &mut decision) => {
                                r.ok().and_then(|r| r.ok()).unwrap_or(false)
                            }
                            // Cancelled with the code on screen: the
                            // approver is told no, as "They differ" would.
                            _ = &mut cancel => false,
                        };
                        decision_answer = Some(v);
                        v
                    }
                };
                if let Some(s) = session.as_mut() {
                    s.confirmed = accepted;
                }
                let _ = reply.send(Response::PairDecision { accepted }).await;
                if !accepted {
                    transport.close();
                    return Err(CoreError::Membership(
                        "pairing declined on this side".into(),
                    ));
                }
            }
            Request::PairComplete(_) | Request::PairCompleteV2(_) | Request::PairCompleteV3(_) => {
                // Every shape reduces to the same thing: some genesis, some
                // ops, and whether the mesh is now complete. The 1.10 shape
                // is the only one that can answer "no" to the last part.
                let chunk = match &request {
                    Request::PairCompleteV3(c) => (**c).clone(),
                    Request::PairCompleteV2(a) => SnapshotChunk {
                        genesis_json: a.genesis_json.clone(),
                        ops: a.ops.clone(),
                        more: false,
                    },
                    Request::PairComplete(a) => SnapshotChunk {
                        genesis_json: a.genesis_json.clone(),
                        ops: a.ops.iter().map(OpEnvelope::from_op).collect(),
                        more: false,
                    },
                    _ => unreachable!("matched above"),
                };
                let Some(s) = session.as_mut() else {
                    let _ = reply
                        .send(Response::Error {
                            message: "pairing not initiated".into(),
                        })
                        .await;
                    continue;
                };
                if !s.confirmed {
                    let _ = reply
                        .send(Response::Error {
                            message: "code was never confirmed".into(),
                        })
                        .await;
                    continue;
                }
                if !chunk.genesis_json.is_empty() {
                    s.genesis_json = chunk.genesis_json;
                }
                s.ops.extend(chunk.ops);
                if chunk.more {
                    // Not the last instalment: acknowledge and keep waiting.
                    let _ = reply.send(Response::Ok).await;
                    continue;
                }
                let accept = mesh_proto::JoinAcceptV2 {
                    genesis_json: std::mem::take(&mut s.genesis_json),
                    ops: std::mem::take(&mut s.ops),
                };
                match persist_join(config, &identity, &accept, s.peer) {
                    Ok(report) => {
                        let _ = reply.send(Response::Ok).await;
                        // Give the ack a moment to flush before tearing down.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        transport.close();
                        return Ok(report);
                    }
                    Err(e) => {
                        let _ = reply
                            .send(Response::Error {
                                message: e.to_string(),
                            })
                            .await;
                        transport.close();
                        return Err(e);
                    }
                }
            }
            Request::Ping(p) => {
                let _ = reply.send(Response::Pong(p)).await;
            }
            other => {
                let _ = reply
                    .send(Response::Error {
                        message: format!("unexpected request during pairing: {other:?}"),
                    })
                    .await;
            }
        }
    }
}

/// Validate and persist the delivered genesis + ops (the tail of
/// `membership::join_via`, shared shape).
fn persist_join(
    config: &NodeConfig,
    identity: &Identity,
    accept: &mesh_proto::JoinAcceptV2,
    accepted_by: NodeId,
) -> Result<JoinReport> {
    let genesis: Genesis = serde_json::from_slice(&accept.genesis_json)
        .map_err(|e| CoreError::Membership(format!("bad genesis from acceptor: {e}")))?;
    genesis.verify()?;
    genesis.save(&config.data_dir)?;

    let engine = StateEngine::open(&config.data_dir, identity.node.clone(), genesis.clone())?;
    engine.ingest(accept.ops.clone())?;
    let me = identity.node.node_id();
    if !engine.read(|s| s.is_active_member(&me)) {
        return Err(CoreError::Membership(
            "pairing reply did not include our membership — try again".into(),
        ));
    }
    let members = engine.read(|s| s.members.len());
    Ok(JoinReport {
        mesh_name: genesis.body.mesh_name,
        accepted_by,
        members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A LAN that announces more joiners than a person can read, or one
    /// joiner from more addresses than it has, does not grow the table
    /// without end (SEC-23).
    #[test]
    fn the_join_queue_is_bounded() {
        let mut w = WatchState::default();
        for i in 0..(MAX_HEARD * 3) {
            w.hear(Claim {
                node_name: format!("m{i}"),
                node_id: None,
                platform: None,
                person: None,
                hostname: None,
                addrs: vec![
                    format!("10.0.{}.{}:7777", i / 250, i % 250)
                        .parse()
                        .unwrap(),
                ],
                beacon: true,
            });
        }
        assert_eq!(w.heard.len(), MAX_HEARD);
        for port in 0..200u16 {
            w.hear(Claim {
                node_name: "chatty".into(),
                node_id: None,
                platform: None,
                person: None,
                hostname: None,
                addrs: vec![format!("10.9.9.9:{}", 1000 + port).parse().unwrap()],
                beacon: true,
            });
        }
        let chatty = &w.heard["name:chatty"];
        assert_eq!(chatty.beacon_addrs.len(), MAX_ADDRS_PER_JOINER);
        // The newest are the ones kept.
        assert!(chatty.beacon_addrs.iter().any(|a| a.port() == 1199));
    }

    #[test]
    fn commitment_binds_nonce() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_ne!(commitment(&a), commitment(&b));
        assert_eq!(commitment(&a), commitment(&a));
    }

    #[test]
    fn sas_is_six_digits_and_symmetric_inputs_matter() {
        let approver = NodeId([3u8; 32]);
        let joiner = NodeId([4u8; 32]);
        let na = [5u8; 32];
        let nj = [6u8; 32];
        let code = sas_code(&approver, &joiner, &na, &nj);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        // Any perturbed input yields a (almost surely) different code — and
        // deterministically the same code for the same transcript.
        assert_eq!(code, sas_code(&approver, &joiner, &na, &nj));
        assert_ne!(code, sas_code(&joiner, &approver, &na, &nj));
    }

    #[test]
    fn code_formats_for_humans() {
        assert_eq!(format_code("123456"), "123 456");
    }
}
