//! Userspace NAT for in-process tests (MASTERPLAN §16 testkit, M7).
//!
//! A node "behind" a [`TestNat`] owns no kernel socket at all: the
//! transport's [`SocketFactory`] hands it a shim whose `local_addr` is a
//! loopback port the NAT merely *reserves* (bound, never read — a black
//! hole, exactly like a LAN address seen from the WAN). Every datagram the
//! node sends goes to the NAT, which forwards it from an external loopback
//! socket; replies are delivered back to the shim if the profile's filter
//! allows them.
//!
//! Profiles:
//! - **Open** — one external socket per client, no filtering (a public
//!   node, just routed through the harness);
//! - **Cone** — endpoint-independent mapping (one external socket per
//!   client) with *port-restricted* filtering: inbound from `ip:port` is
//!   accepted only after the client sent to that `ip:port`. Reflexive
//!   discovery yields a stable, dialable address; unsolicited dials are
//!   dropped until the client sends toward the dialer — which is exactly
//!   what a coordinated punch arranges (both sides send);
//! - **Symmetric** — a fresh external socket per destination: observers
//!   see different ports, nothing published is dialable, punches fail,
//!   and only a relay gets through.
//!
//! (A drop-all-inbound profile would brick the node: handshake replies
//! are inbound too.)

use std::collections::{HashMap, HashSet};
use std::io::{self, IoSliceMut};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use mesh_core::transport::SocketFactory;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatProfile {
    Open,
    Cone,
    Symmetric,
}

/// Datagrams parked for a client before drop-on-full (UDP semantics).
const CLIENT_QUEUE: usize = 2048;

#[derive(Debug, Default)]
pub struct NatStats {
    pub forwarded_out: AtomicU64,
    pub forwarded_in: AtomicU64,
    pub filtered_in: AtomicU64,
}

#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
enum MappingKey {
    Any,
    Dest(SocketAddr),
}

struct Mapping {
    external: Arc<tokio::net::UdpSocket>,
    filter: Arc<Mutex<HashSet<SocketAddr>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

struct Client {
    internal: SocketAddr,
    /// Reserves the internal port and black-holes anything sent to it.
    _placeholder: std::net::UdpSocket,
    recv_tx: mpsc::Sender<(SocketAddr, Vec<u8>)>,
    mappings: Mutex<HashMap<MappingKey, Arc<Mapping>>>,
}

pub struct TestNat {
    profile: NatProfile,
    clients: Mutex<HashMap<SocketAddr, Weak<Client>>>,
    pub stats: NatStats,
}

impl std::fmt::Debug for TestNat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestNat({:?})", self.profile)
    }
}

fn normalize(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => addr,
        },
        v4 => v4,
    }
}

impl TestNat {
    pub fn new(profile: NatProfile) -> Arc<Self> {
        Arc::new(TestNat {
            profile,
            clients: Mutex::new(HashMap::new()),
            stats: NatStats::default(),
        })
    }

    pub fn profile(&self) -> NatProfile {
        self.profile
    }

    /// A [`SocketFactory`] that puts every endpoint it builds behind this NAT.
    pub fn client_factory(self: &Arc<Self>) -> SocketFactory {
        let nat = self.clone();
        SocketFactory(Arc::new(move |bind: SocketAddr| nat.new_client(bind)))
    }

    fn new_client(self: &Arc<Self>, bind: SocketAddr) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        // The internal address: loopback, the requested port (0 = any).
        // Bound but never read → black hole for anyone dialing it directly.
        let placeholder = std::net::UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            bind.port(),
        )))?;
        let internal = placeholder.local_addr()?;
        let (recv_tx, recv_rx) = mpsc::channel(CLIENT_QUEUE);
        let client = Arc::new(Client {
            internal,
            _placeholder: placeholder,
            recv_tx,
            mappings: Mutex::new(HashMap::new()),
        });
        self.clients
            .lock()
            .expect("nat clients lock")
            .insert(internal, Arc::downgrade(&client));
        Ok(Arc::new(NatClientSocket {
            nat: self.clone(),
            client,
            recv_rx: Mutex::new(recv_rx),
        }))
    }

    /// The external address a client (identified by its internal/listen
    /// address) is mapped to. Cone/Open: the single mapping. Symmetric:
    /// `None` — there is no one answer.
    pub fn external_addr(&self, internal: SocketAddr) -> Option<SocketAddr> {
        let client = self
            .clients
            .lock()
            .expect("nat clients lock")
            .get(&normalize(internal))?
            .upgrade()?;
        let mappings = client.mappings.lock().expect("mappings lock");
        mappings
            .get(&MappingKey::Any)
            .and_then(|m| m.external.local_addr().ok())
    }

    /// Every external address currently mapped for a client.
    pub fn external_addrs(&self, internal: SocketAddr) -> Vec<SocketAddr> {
        let Some(client) = self
            .clients
            .lock()
            .expect("nat clients lock")
            .get(&normalize(internal))
            .and_then(Weak::upgrade)
        else {
            return Vec::new();
        };
        let mappings = client.mappings.lock().expect("mappings lock");
        mappings
            .values()
            .filter_map(|m| m.external.local_addr().ok())
            .collect()
    }

    fn mapping_for(
        self: &Arc<Self>,
        client: &Arc<Client>,
        dest: SocketAddr,
    ) -> io::Result<Arc<Mapping>> {
        let key = match self.profile {
            NatProfile::Open | NatProfile::Cone => MappingKey::Any,
            NatProfile::Symmetric => MappingKey::Dest(dest),
        };
        let mut mappings = client.mappings.lock().expect("mappings lock");
        if let Some(m) = mappings.get(&key) {
            return Ok(m.clone());
        }
        let std_sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
        std_sock.set_nonblocking(true)?;
        let external = Arc::new(tokio::net::UdpSocket::from_std(std_sock)?);
        let filter = Arc::new(Mutex::new(HashSet::new()));
        let reader = tokio::spawn(Self::reader(
            self.clone(),
            external.clone(),
            filter.clone(),
            Arc::downgrade(client),
        ));
        let mapping = Arc::new(Mapping {
            external,
            filter,
            reader,
        });
        mappings.insert(key, mapping.clone());
        Ok(mapping)
    }

    async fn reader(
        nat: Arc<TestNat>,
        external: Arc<tokio::net::UdpSocket>,
        filter: Arc<Mutex<HashSet<SocketAddr>>>,
        client: Weak<Client>,
    ) {
        let mut buf = vec![0u8; 65_536];
        loop {
            let (n, src) = match external.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(_) => continue,
            };
            let src = normalize(src);
            let Some(client) = client.upgrade() else {
                return;
            };
            let allowed = match nat.profile {
                NatProfile::Open => true,
                NatProfile::Cone | NatProfile::Symmetric => {
                    filter.lock().expect("filter lock").contains(&src)
                }
            };
            if !allowed {
                nat.stats.filtered_in.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            nat.stats.forwarded_in.fetch_add(1, Ordering::Relaxed);
            let _ = client.recv_tx.try_send((src, buf[..n].to_vec()));
        }
    }

    fn send(
        self: &Arc<Self>,
        client: &Arc<Client>,
        dest: SocketAddr,
        bytes: &[u8],
    ) -> io::Result<()> {
        let dest = normalize(dest);
        let mapping = self.mapping_for(client, dest)?;
        mapping.filter.lock().expect("filter lock").insert(dest);
        // UDP semantics: a send that cannot be queued is simply dropped.
        if mapping.external.try_send_to(bytes, dest).is_ok() {
            self.stats.forwarded_out.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

struct NatClientSocket {
    nat: Arc<TestNat>,
    client: Arc<Client>,
    recv_rx: Mutex<mpsc::Receiver<(SocketAddr, Vec<u8>)>>,
}

impl std::fmt::Debug for NatClientSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NatClientSocket({})", self.client.internal)
    }
}

#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for NatClientSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let seg = transmit
            .segment_size
            .unwrap_or(transmit.contents.len())
            .max(1);
        for chunk in transmit.contents.chunks(seg) {
            self.nat.send(&self.client, transmit.destination, chunk)?;
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut rx = self.recv_rx.lock().expect("nat recv lock");
        let mut n = 0;
        for (buf, m) in bufs.iter_mut().zip(meta.iter_mut()) {
            match rx.poll_recv(cx) {
                Poll::Ready(Some((src, payload))) => {
                    let len = payload.len().min(buf.len());
                    buf[..len].copy_from_slice(&payload[..len]);
                    *m = RecvMeta {
                        addr: src,
                        len,
                        stride: len,
                        ecn: None,
                        dst_ip: None,
                    };
                    n += 1;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(io::Error::other("nat client closed")));
                }
                Poll::Pending => break,
            }
        }
        if n > 0 {
            Poll::Ready(Ok(n))
        } else {
            Poll::Pending
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.client.internal)
    }

    fn may_fragment(&self) -> bool {
        true
    }
}
