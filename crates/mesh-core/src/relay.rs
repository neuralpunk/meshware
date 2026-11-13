//! Relay hub (MASTERPLAN M7): a relay-capable member switches opaque
//! frames between NATed peers, DERP-style.
//!
//! Two kinds of long-lived streams arrive on the relay's ordinary member
//! connections:
//!
//! - **registration** (`Request::RelayRegister`): a NATed node parks here;
//!   frames addressed to it arrive on this stream, frames it sends to any
//!   peer leave on it;
//! - **open** (`Request::RelayOpen { target }`): an initiator asks for a
//!   path to a registered `target`; frames it writes go to the target's
//!   registration stream, frames the target sends back arrive here.
//!
//! The relay stamps every delivered frame with the TLS-proven sender
//! (`RelayServerFrame::Data { from, .. }`); payloads are opaque inner-QUIC
//! ciphertext it cannot read. Delivery is best-effort with UDP semantics:
//! bounded per-stream queues, drop on full, frames capped at
//! [`MAX_RELAY_FRAME`]. `PunchRequest`s are forwarded to the target's
//! registration stream under a per-requester rate limit (bounded
//! amplification: the target dials back at most 8 candidates, directly,
//! never recursively).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::debug;

use crate::error::{CoreError, Result};
use mesh_proto::{
    MAX_RELAY_FRAME, NodeId, RelayFrame, RelayServerFrame, Response, read_frame, write_frame,
};

/// Frames parked per stream before drop-on-full (UDP semantics).
const STREAM_QUEUE: usize = 512;
/// Punch forwards allowed per requester per window.
const PUNCH_LIMIT: u32 = 20;
const PUNCH_WINDOW: Duration = Duration::from_secs(10);
/// Candidates forwarded per punch (the target dials back at most this many).
const MAX_PUNCH_CANDIDATES: usize = 8;

struct ClientEntry {
    tx: mpsc::Sender<RelayServerFrame>,
    generation: u64,
}

#[derive(Default)]
struct Tables {
    /// Registered (parked) clients by node id.
    clients: HashMap<NodeId, ClientEntry>,
    /// Open paths: (initiator, target) → initiator's stream.
    opens: HashMap<(NodeId, NodeId), (mpsc::Sender<RelayServerFrame>, u64)>,
    /// Punch rate limiter: requester → (window start, count).
    punches: HashMap<NodeId, (Instant, u32)>,
    generation: u64,
}

/// Counters for `/metrics` and tests.
#[derive(Debug, Default)]
pub struct RelayStats {
    pub frames_relayed: AtomicU64,
    pub bytes_relayed: AtomicU64,
    pub frames_dropped: AtomicU64,
    pub punches_forwarded: AtomicU64,
    pub registrations_total: AtomicU64,
}

/// Point-in-time view.
#[derive(Debug, Clone, Default)]
pub struct RelaySnapshot {
    pub enabled: bool,
    pub clients: usize,
    pub opens: usize,
    pub frames_relayed: u64,
    pub bytes_relayed: u64,
    pub frames_dropped: u64,
    pub punches_forwarded: u64,
}

pub struct RelayHub {
    enabled: AtomicBool,
    tables: Mutex<Tables>,
    pub stats: RelayStats,
}

impl std::fmt::Debug for RelayHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RelayHub(enabled={})", self.is_enabled())
    }
}

impl Default for RelayHub {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayHub {
    pub fn new() -> Self {
        RelayHub {
            enabled: AtomicBool::new(false),
            tables: Mutex::new(Tables::default()),
            stats: RelayStats::default(),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn tables(&self) -> parking_lot::MutexGuard<'_, Tables> {
        self.tables.lock()
    }

    pub fn snapshot(&self) -> RelaySnapshot {
        let t = self.tables();
        RelaySnapshot {
            enabled: self.is_enabled(),
            clients: t.clients.len(),
            opens: t.opens.len(),
            frames_relayed: self.stats.frames_relayed.load(Ordering::Relaxed),
            bytes_relayed: self.stats.bytes_relayed.load(Ordering::Relaxed),
            frames_dropped: self.stats.frames_dropped.load(Ordering::Relaxed),
            punches_forwarded: self.stats.punches_forwarded.load(Ordering::Relaxed),
        }
    }

    pub fn is_registered(&self, node: &NodeId) -> bool {
        self.tables().clients.contains_key(node)
    }

    /// Serve a registration stream until the client goes away.
    pub async fn serve_register(
        self: &Arc<Self>,
        peer: NodeId,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
    ) -> Result<()> {
        if !self.is_enabled() {
            write_frame(
                &mut send,
                &Response::Error {
                    message: "this node is not a relay".into(),
                },
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        }
        write_frame(&mut send, &Response::RelayReady).await?;
        let (tx, rx) = mpsc::channel::<RelayServerFrame>(STREAM_QUEUE);
        let generation = {
            let mut t = self.tables();
            t.generation += 1;
            let g = t.generation;
            // A re-registration replaces the previous stream (its writer
            // ends when the old sender drops).
            t.clients.insert(peer, ClientEntry { tx, generation: g });
            g
        };
        self.stats
            .registrations_total
            .fetch_add(1, Ordering::Relaxed);
        debug!(client = %peer.short(), "relay: client registered");

        let writer = tokio::spawn(Self::pump_out(rx, send));
        while let Ok(frame) = read_frame::<_, RelayFrame>(&mut recv).await {
            self.deliver(peer, frame.peer, frame.payload);
        }
        {
            let mut t = self.tables();
            if t.clients
                .get(&peer)
                .is_some_and(|c| c.generation == generation)
            {
                t.clients.remove(&peer);
            }
        }
        writer.abort();
        debug!(client = %peer.short(), "relay: client left");
        Ok(())
    }

    /// Serve an open-path stream from `peer` to `target`.
    pub async fn serve_open(
        self: &Arc<Self>,
        peer: NodeId,
        target: NodeId,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
    ) -> Result<()> {
        if !self.is_enabled() {
            write_frame(
                &mut send,
                &Response::Error {
                    message: "this node is not a relay".into(),
                },
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        }
        if !self.is_registered(&target) {
            write_frame(
                &mut send,
                &Response::Error {
                    message: format!("{} is not registered at this relay", target.short()),
                },
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        }
        write_frame(&mut send, &Response::RelayReady).await?;
        let (tx, rx) = mpsc::channel::<RelayServerFrame>(STREAM_QUEUE);
        let generation = {
            let mut t = self.tables();
            t.generation += 1;
            let g = t.generation;
            t.opens.insert((peer, target), (tx, g));
            g
        };
        debug!(from = %peer.short(), to = %target.short(), "relay: path opened");
        let writer = tokio::spawn(Self::pump_out(rx, send));
        while let Ok(frame) = read_frame::<_, RelayFrame>(&mut recv).await {
            if frame.peer == target {
                self.deliver(peer, target, frame.payload);
            } else {
                self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        {
            let mut t = self.tables();
            if t.opens
                .get(&(peer, target))
                .is_some_and(|(_, g)| *g == generation)
            {
                t.opens.remove(&(peer, target));
            }
        }
        writer.abort();
        debug!(from = %peer.short(), to = %target.short(), "relay: path closed");
        Ok(())
    }

    async fn pump_out(mut rx: mpsc::Receiver<RelayServerFrame>, mut send: quinn::SendStream) {
        while let Some(frame) = rx.recv().await {
            if write_frame(&mut send, &frame).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    }

    /// Switch one frame: prefer the initiator's open stream for this pair,
    /// else the destination's registration stream, else drop.
    fn deliver(&self, from: NodeId, to: NodeId, payload: Vec<u8>) {
        if payload.is_empty() || payload.len() > MAX_RELAY_FRAME {
            self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let len = payload.len() as u64;
        let tx = {
            let t = self.tables();
            t.opens
                .get(&(to, from))
                .map(|(tx, _)| tx.clone())
                .or_else(|| t.clients.get(&to).map(|c| c.tx.clone()))
        };
        let Some(tx) = tx else {
            self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match tx.try_send(RelayServerFrame::Data { from, payload }) {
            Ok(()) => {
                self.stats.frames_relayed.fetch_add(1, Ordering::Relaxed);
                self.stats.bytes_relayed.fetch_add(len, Ordering::Relaxed);
            }
            Err(_) => {
                self.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Forward a punch request to a registered target. Rate-limited per
    /// requester; candidates truncated to the dial-back bound.
    pub fn punch(&self, from: NodeId, target: NodeId, mut candidates: Vec<String>) -> Result<()> {
        if !self.is_enabled() {
            return Err(CoreError::Transport("this node is not a relay".into()));
        }
        candidates.truncate(MAX_PUNCH_CANDIDATES);
        let tx = {
            let mut t = self.tables();
            let now = Instant::now();
            let entry = t.punches.entry(from).or_insert((now, 0));
            if now.duration_since(entry.0) > PUNCH_WINDOW {
                *entry = (now, 0);
            }
            entry.1 += 1;
            if entry.1 > PUNCH_LIMIT {
                return Err(CoreError::Transport("punch rate limit exceeded".into()));
            }
            t.clients.get(&target).map(|c| c.tx.clone())
        };
        let Some(tx) = tx else {
            return Err(CoreError::Transport(format!(
                "{} is not registered at this relay",
                target.short()
            )));
        };
        tx.try_send(RelayServerFrame::Punch { from, candidates })
            .map_err(|_| CoreError::Transport("target's relay stream is saturated".into()))?;
        self.stats.punches_forwarded.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
