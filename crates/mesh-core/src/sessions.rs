//! The fabric: sessions (M17).
//!
//! A session is a live bidirectional stream between an initiator and a
//! **host** node — a shell, a non-interactive exec, a file copy, or one
//! forwarded TCP connection. The protocol shape lives in
//! [`mesh_proto::sessions`]; this module is the machinery on both ends of
//! it.
//!
//! ## Two legs, not one
//!
//! Both the wire and the local IPC socket are request-in/responses-out with
//! the request half already finished (R1, R4), so an interactive feature
//! has to solve the same problem twice:
//!
//! ```text
//!   client  ──SessionFrame──▶  origin daemon  ──SessionFrame──▶  host node
//!   (unix socket)                  (splice)         (QUIC bi stream)
//!           ◀SessionServerFrame─               ◀SessionServerFrame─
//! ```
//!
//! The origin daemon is a **splice and not a translator**: the same two
//! frame enums cross both legs, so there is exactly one definition of what
//! a resize or a signal means. When the target is the origin itself the
//! wire leg simply is not there and the host handler runs against the IPC
//! stream directly — a shell into your own node costs no round trip.
//!
//! ## What lives here
//!
//! - [`Registry`] — live sessions, their ids, the per-peer cap, and the
//!   byte counters `/metrics` reports. A session is *live or gone*; what
//!   survives it is the audit entry, which is why none of this is
//!   replicated (§3.5).
//! - [`host`] — the host end: PTY, exec, copy, forward.
//! - [`splice`] — the origin end.
//! - [`pty_shim`] — the six lines that must run in the child between fork
//!   and exec, as a re-exec of this binary rather than an `unsafe`
//!   `pre_exec` closure. See its own docs.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::{Duration, Instant};

use mesh_proto::{NodeId, SessionEnd, SessionKind, SessionRow, UserId};

pub use mesh_proto::{
    MAX_SESSION_FRAME, SESSION_CHUNK, STREAM_STDERR, STREAM_STDOUT, SessionFrame,
    SessionServerFrame, SessionSignal,
};

#[cfg(unix)]
pub mod host;
#[cfg(unix)]
pub mod pty_shim;
pub mod splice;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Why a session ended, for the audit entry and the client's last frame.
#[derive(Debug, Clone)]
pub enum Ending {
    /// The far process exited.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// One side closed the stream.
    Closed,
    /// The node closed it — a drain, an operator, or a timeout.
    Evicted { end: SessionEnd, reason: String },
    /// It never started, or it broke.
    Failed { message: String },
}

impl Ending {
    pub fn outcome(&self) -> String {
        match self {
            Ending::Exited {
                code: Some(0),
                signal: None,
            } => "ok".into(),
            Ending::Exited { code, signal } => match (code, signal) {
                (_, Some(sig)) => format!("killed by signal {sig}"),
                (Some(code), _) => format!("exit {code}"),
                _ => "exit unknown".into(),
            },
            Ending::Closed => "closed".into(),
            Ending::Evicted { end, .. } => end.as_str().to_string(),
            Ending::Failed { message } => format!("failed: {message}"),
        }
    }
}

struct Entry {
    id: u64,
    kind: SessionKind,
    hosting: bool,
    peer: NodeId,
    peer_name: String,
    user: UserId,
    username: String,
    reason: String,
    os_user: String,
    opened_at_micros: u64,
    /// Micros of the last byte in either direction — what the idle timeout
    /// measures, and the only reason byte counting is not merely a metric.
    last_active: Arc<AtomicU64>,
    bytes_in: Arc<AtomicU64>,
    bytes_out: Arc<AtomicU64>,
    /// Closed by [`Registry::close`] and by a drain; every pump selects on
    /// it, so eviction is immediate rather than "at the next byte".
    evict: Arc<tokio::sync::Notify>,
    evicted_for: Arc<Mutex<Option<String>>>,
}

/// How long a session may live, and how long it may be quiet.
///
/// Both are v1-mandatory in the enterprise guidance, and both protect
/// against the same thing: a terminal somebody opened, walked away from,
/// and forgot. `None` disables one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Limits {
    pub idle: Option<Duration>,
    pub max: Option<Duration>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            idle: Some(Duration::from_secs(30 * 60)),
            max: Some(Duration::from_secs(4 * 60 * 60)),
        }
    }
}

/// Live sessions on this node, both hosted and initiated.
#[derive(Debug, Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    /// Cumulative counters for `/metrics` — sessions come and go, and a
    /// gauge alone cannot answer "has anyone ever shelled into this box".
    pub opened_total: AtomicU64,
    pub refused_total: AtomicU64,
    pub bytes_total: AtomicU64,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    live: HashMap<u64, Entry>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Inner(live={})", self.live.len())
    }
}

/// Handle on one registered session. Dropping it deregisters — there is no
/// "close" to forget, because every exit path from a session handler,
/// including a panic unwinding through it, runs this.
#[derive(Debug)]
pub struct Guard {
    registry: Arc<Registry>,
    id: u64,
    last_active: Arc<AtomicU64>,
    bytes_in: Arc<AtomicU64>,
    bytes_out: Arc<AtomicU64>,
    evict: Arc<tokio::sync::Notify>,
    evicted_for: Arc<Mutex<Option<String>>>,
    started: Instant,
    limits: Limits,
}

impl Guard {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Count bytes moving toward the host.
    pub fn count_in(&self, n: usize) {
        let n = n as u64;
        self.bytes_in.fetch_add(n, Ordering::Relaxed);
        self.registry.bytes_total.fetch_add(n, Ordering::Relaxed);
        self.touch();
    }

    /// Count bytes moving toward the initiator.
    pub fn count_out(&self, n: usize) {
        let n = n as u64;
        self.bytes_out.fetch_add(n, Ordering::Relaxed);
        self.registry.bytes_total.fetch_add(n, Ordering::Relaxed);
        self.touch();
    }

    /// Mark the session active now.
    ///
    /// **Output counts as activity, not just input.** A `tail -f` that is
    /// printing steadily is a session somebody is watching, and cutting it
    /// off at thirty minutes because nobody typed would be the wrong
    /// reading of "idle".
    pub fn touch(&self) {
        self.last_active
            .store(mesh_proto::now_micros(), Ordering::Relaxed);
    }

    /// Resolves when this session should stop, and says why.
    ///
    /// One future for three reasons — an operator closing it, a drain, and
    /// either timeout — so a host loop has a single `select!` arm to hold
    /// rather than three that could disagree about which fired first.
    pub async fn closed(&self) -> SessionEnd {
        loop {
            let now = Instant::now();
            let ran_for = now.saturating_duration_since(self.started);
            if let Some(max) = self.limits.max
                && ran_for >= max
            {
                return SessionEnd::MaxDuration;
            }
            let quiet = Duration::from_micros(
                mesh_proto::now_micros().saturating_sub(self.last_active.load(Ordering::Relaxed)),
            );
            if let Some(idle) = self.limits.idle
                && quiet >= idle
            {
                return SessionEnd::IdleTimeout;
            }
            // Wake at whichever bound is nearer, capped so a session that
            // is evicted between ticks still ends promptly.
            let until_max = self.limits.max.map(|m| m.saturating_sub(ran_for));
            let until_idle = self.limits.idle.map(|i| i.saturating_sub(quiet));
            let tick = [until_max, until_idle]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(Duration::from_secs(30))
                .clamp(Duration::from_millis(200), Duration::from_secs(30));
            tokio::select! {
                () = self.evict.notified() => {
                    return if self.eviction_reason().contains("draining") {
                        SessionEnd::Drained
                    } else {
                        SessionEnd::Terminated
                    };
                }
                () = tokio::time::sleep(tick) => {}
            }
        }
    }

    /// Why, once [`Self::evicted`] has fired.
    pub fn eviction_reason(&self) -> String {
        self.evicted_for
            .lock()
            .clone()
            .unwrap_or_else(|| "closed".into())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.registry.inner.lock().live.remove(&self.id);
    }
}

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Register a session, or refuse and say why.
    ///
    /// The cap is per *peer*, not per node: one busy operator must not be
    /// able to exhaust a host for everyone else, and the refusal names the
    /// number so the fix is obvious (§3.8).
    #[allow(clippy::too_many_arguments)]
    pub fn admit(
        self: &Arc<Self>,
        kind: SessionKind,
        hosting: bool,
        peer: NodeId,
        peer_name: String,
        user: UserId,
        username: String,
        max_per_peer: u32,
        reason: String,
        os_user: String,
        limits: Limits,
    ) -> std::result::Result<Guard, String> {
        let mut inner = self.inner.lock();
        if hosting {
            let held = inner
                .live
                .values()
                .filter(|e| e.hosting && e.peer == peer)
                .count();
            if held >= max_per_peer as usize {
                self.refused_total.fetch_add(1, Ordering::Relaxed);
                return Err(format!(
                    "{peer_name} already holds {held} sessions here, and the limit is \
                     {max_per_peer} ([sessions] max_per_peer)"
                ));
            }
        }
        inner.next_id += 1;
        let id = inner.next_id;
        let now = mesh_proto::now_micros();
        let last_active = Arc::new(AtomicU64::new(now));
        let bytes_in = Arc::new(AtomicU64::new(0));
        let bytes_out = Arc::new(AtomicU64::new(0));
        let evict = Arc::new(tokio::sync::Notify::new());
        let evicted_for = Arc::new(Mutex::new(None));
        inner.live.insert(
            id,
            Entry {
                id,
                kind,
                hosting,
                peer,
                peer_name,
                user,
                username,
                reason,
                os_user,
                opened_at_micros: now,
                last_active: last_active.clone(),
                bytes_in: bytes_in.clone(),
                bytes_out: bytes_out.clone(),
                evict: evict.clone(),
                evicted_for: evicted_for.clone(),
            },
        );
        drop(inner);
        self.opened_total.fetch_add(1, Ordering::Relaxed);
        Ok(Guard {
            registry: self.clone(),
            id,
            last_active,
            bytes_in,
            bytes_out,
            evict,
            evicted_for,
            started: Instant::now(),
            limits,
        })
    }

    pub fn rows(&self) -> Vec<SessionRow> {
        let inner = self.inner.lock();
        let mut rows: Vec<SessionRow> = inner
            .live
            .values()
            .map(|e| SessionRow {
                id: e.id,
                kind: kind_word(&e.kind).to_string(),
                detail: e.kind.describe(),
                hosting: e.hosting,
                peer: e.peer,
                peer_name: e.peer_name.clone(),
                user: e.user,
                username: e.username.clone(),
                reason: e.reason.clone(),
                os_user: e.os_user.clone(),
                opened_at_micros: e.opened_at_micros,
                last_active_micros: e.last_active.load(Ordering::Relaxed),
                bytes_in: e.bytes_in.load(Ordering::Relaxed),
                bytes_out: e.bytes_out.load(Ordering::Relaxed),
            })
            .collect();
        rows.sort_by_key(|r| r.id);
        rows
    }

    pub fn live_count(&self) -> usize {
        self.inner.lock().live.len()
    }

    /// Sessions this node is hosting — the ones a drain has to close.
    pub fn hosted_count(&self) -> usize {
        self.inner
            .lock()
            .live
            .values()
            .filter(|e| e.hosting)
            .count()
    }

    /// Ask one session to close. Returns false if there is no such id.
    pub fn close(&self, id: u64, reason: &str) -> bool {
        let inner = self.inner.lock();
        match inner.live.get(&id) {
            Some(entry) => {
                *entry.evicted_for.lock() = Some(reason.to_string());
                entry.evict.notify_waiters();
                true
            }
            None => false,
        }
    }

    /// Close every session this node hosts.
    ///
    /// **A drain does not wait for sessions** (§3.9). It waits for running
    /// and queued jobs and owned fan-out drives to reach zero; a human at a
    /// prompt could hold that open forever, and `meshnode drain` must not
    /// be a command that sometimes never returns. So it closes them, with a
    /// message naming the node and the reason, and the operator finds out
    /// rather than wondering.
    pub fn close_hosted(&self, reason: &str) -> usize {
        let inner = self.inner.lock();
        let mut n = 0;
        for entry in inner.live.values().filter(|e| e.hosting) {
            *entry.evicted_for.lock() = Some(reason.to_string());
            entry.evict.notify_waiters();
            n += 1;
        }
        n
    }
}

use mesh_proto::sessions::kind_word;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn shell() -> SessionKind {
        SessionKind::Shell {
            command: None,
            term: "xterm".into(),
            cols: 80,
            rows: 24,
        }
    }

    fn admit(reg: &Arc<Registry>, peer: NodeId, max: u32) -> std::result::Result<Guard, String> {
        reg.admit(
            shell(),
            true,
            peer,
            "hermes".into(),
            UserId([1u8; 32]),
            "ada".into(),
            max,
            "INC-1".into(),
            "meshnode".into(),
            Limits {
                idle: None,
                max: None,
            },
        )
    }

    #[test]
    fn the_cap_is_per_peer_and_the_refusal_names_it() {
        let reg = Arc::new(Registry::new());
        let a = NodeId([1u8; 32]);
        let b = NodeId([2u8; 32]);
        let _one = admit(&reg, a, 2).unwrap();
        let _two = admit(&reg, a, 2).unwrap();
        let refused = admit(&reg, a, 2).expect_err("the third is refused");
        assert!(refused.contains("max_per_peer"), "{refused}");
        assert!(refused.contains('2'), "{refused}");
        // A different peer is not affected by the first one's usage.
        let _other = admit(&reg, b, 2).unwrap();
        assert_eq!(reg.live_count(), 3);
        assert_eq!(reg.refused_total.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dropping_a_guard_deregisters() {
        let reg = Arc::new(Registry::new());
        let peer = NodeId([1u8; 32]);
        {
            let _g = admit(&reg, peer, 8).unwrap();
            assert_eq!(reg.live_count(), 1);
        }
        assert_eq!(reg.live_count(), 0);
        // …and the slot it held is free again, which is the property the
        // cap depends on.
        let _g = admit(&reg, peer, 1).unwrap();
    }

    #[test]
    fn initiated_sessions_do_not_count_against_the_hosts_cap() {
        // A node that opens eight shells outward has not spent anything a
        // *host* is protecting: the cap exists to bound what one peer can
        // make this machine do.
        let reg = Arc::new(Registry::new());
        let peer = NodeId([1u8; 32]);
        let mut held = Vec::new();
        for _ in 0..12 {
            held.push(
                reg.admit(
                    shell(),
                    false,
                    peer,
                    "hermes".into(),
                    UserId([1u8; 32]),
                    "ada".into(),
                    2,
                    "INC-1".into(),
                    String::new(),
                    Limits {
                        idle: None,
                        max: None,
                    },
                )
                .unwrap(),
            );
        }
        assert_eq!(reg.hosted_count(), 0);
        assert_eq!(reg.live_count(), 12);
    }

    #[tokio::test]
    async fn a_drain_closes_hosted_sessions_and_names_the_reason() {
        let reg = Arc::new(Registry::new());
        let peer = NodeId([1u8; 32]);
        let hosted = admit(&reg, peer, 8).unwrap();
        let outward = reg
            .admit(
                shell(),
                false,
                peer,
                "hermes".into(),
                UserId([1u8; 32]),
                "ada".into(),
                8,
                "INC-1".into(),
                String::new(),
                Limits {
                    idle: None,
                    max: None,
                },
            )
            .unwrap();
        let waiting = tokio::spawn({
            let evict = hosted.evict.clone();
            async move { evict.notified().await }
        });
        tokio::task::yield_now().await;
        assert_eq!(reg.close_hosted("alpha is draining"), 1);
        waiting.await.unwrap();
        assert_eq!(hosted.eviction_reason(), "alpha is draining");
        // The outward session is this node's own business and survives.
        assert!(outward.evicted_for.lock().is_none());
    }

    #[test]
    fn an_ending_reads_as_what_happened() {
        assert_eq!(
            Ending::Exited {
                code: Some(0),
                signal: None
            }
            .outcome(),
            "ok"
        );
        assert_eq!(
            Ending::Exited {
                code: Some(1),
                signal: None
            }
            .outcome(),
            "exit 1"
        );
        assert_eq!(
            Ending::Exited {
                code: None,
                signal: Some(9)
            }
            .outcome(),
            "killed by signal 9"
        );
    }
}
