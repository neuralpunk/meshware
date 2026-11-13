//! A DNS responder on every node (N3 P3).
//!
//! ## Why this ships with discovery rather than after it
//!
//! Nomad's DNS issue was opened **by HashiCorp** in April 2022, became the
//! most-upvoted open networking issue, and was closed out four years later
//! by declaring DNS permanently out of core and pointing at CoreDNS. The
//! documented alternative — render config files with a template engine —
//! fights nginx's upstream caching, as their own users document at length.
//!
//! A registry without DNS is a table. The thing an operator actually wants
//! is `curl webapp.mesh`, and every hop between those two is a component
//! somebody has to run, configure and watch.
//!
//! ## Why every node can answer
//!
//! Every node holds the whole catalogue, because [`crate::registry`] is a
//! fold over one replicated record per node. So a resolver answers from
//! local state with **no round trip and no upstream** — a property Nomad
//! structurally cannot have, since its check results never leave the client
//! that measured them.
//!
//! ## Hand-rolled, and small
//!
//! No dependency: `meshnode` is four crates, no C and static musl, and that
//! property is load-bearing (it is why `jsonwebtoken` was refused in
//! `oidc.rs`). An authoritative responder for one zone needs a header, a
//! question and an answer section — a few hundred lines — and every DNS
//! library in the ecosystem brings a resolver, a client and a recursive
//! cache this will never use.
//!
//! **It is authoritative for one suffix and nothing else.** Anything not
//! ending `.mesh` gets `REFUSED`, never a forward: a responder that
//! forwarded would be a recursive resolver on every node in the fleet,
//! which is a different product and a much larger attack surface.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use mesh_proto::{DNS_SUFFIX, Registration};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::state::StateEngine;

/// The largest question this will read.
///
/// A UDP DNS message is 512 bytes without EDNS(0); anything larger over UDP
/// is either an EDNS negotiation this does not advertise or somebody
/// probing. Read a little more than the classic limit so a legitimate
/// oversized query is *refused* rather than silently truncated into
/// nonsense, and no more.
const MAX_QUERY: usize = 1232;

/// The largest answer this will send over UDP before setting TC.
///
/// 512 is the pre-EDNS guarantee, and this responder does not advertise
/// EDNS — so a client that needs more is told to retry over TCP rather than
/// handed a datagram the network may drop.
const MAX_UDP_REPLY: usize = 512;

/// TTL on every answer, in seconds.
///
/// **Short, deliberately.** The whole point of the health field is that an
/// instance leaves rotation when it fails; a minute-long TTL would put a
/// dead address in a client's cache for a minute after the mesh already
/// knew better, which is the failure this phase exists to avoid. Five
/// seconds is the deployment tick, so a cache entry is never staler than
/// the registry it came from.
const TTL_SECS: u32 = 5;

// -- record and class constants ---------------------------------------------

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;

/// Response codes, as the header's low nibble.
const RCODE_OK: u8 = 0;
const RCODE_FORMERR: u8 = 1;
const RCODE_NXDOMAIN: u8 = 3;
const RCODE_NOTIMP: u8 = 4;
const RCODE_REFUSED: u8 = 5;

/// One parsed question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The labels, lowercased: `["webapp", "mesh"]`.
    pub labels: Vec<String>,
    pub qtype: u16,
    pub qclass: u16,
}

impl Question {
    /// The service this asks about: `webapp.mesh` → `webapp`.
    ///
    /// Two labels exactly. A deeper name is not a service — see
    /// [`Self::instance`] for the one deeper form this zone defines, and
    /// anything else is NXDOMAIN rather than guessed at, because guessing
    /// is how `db.webapp.mesh` silently becomes `webapp`.
    pub fn service(&self) -> Option<&str> {
        match self.labels.as_slice() {
            [name, suffix] if suffix == DNS_SUFFIX => Some(name.as_str()),
            _ => None,
        }
    }

    /// One instance of one service: `job-abc123.webapp.mesh`.
    ///
    /// **This form exists because SRV needs it.** An SRV record's target
    /// must be a *name*, not an address, and there is no per-instance
    /// hostname in this mesh to point at. Minting one here — from the job
    /// id, which is already unique and already a legal DNS label — gives
    /// SRV a target that actually resolves, and gives a client that
    /// ignores the additional section somewhere to ask.
    pub fn instance(&self) -> Option<(&str, &str)> {
        match self.labels.as_slice() {
            [inst, service, suffix] if suffix == DNS_SUFFIX => {
                Some((inst.as_str(), service.as_str()))
            }
            _ => None,
        }
    }

    /// Is this name inside the zone this responder is authoritative for?
    pub fn in_zone(&self) -> bool {
        self.labels.last().is_some_and(|l| l == DNS_SUFFIX)
    }
}

/// A query, decoded far enough to answer it.
#[derive(Debug, Clone)]
pub struct Query {
    pub id: u16,
    /// The header's second byte-pair, kept so the reply can echo RD.
    pub flags: u16,
    pub question: Option<Question>,
    /// The question section's bytes, echoed verbatim into the reply — so a
    /// name this build would have re-encoded differently still matches what
    /// the client asked.
    pub question_raw: Vec<u8>,
}

/// Parse a query far enough to answer or refuse it.
///
/// Deliberately partial: this reads the header and the **first** question
/// and stops. Additional questions are not answered (no resolver sends
/// them, and the RFC never defined how a response would report per-question
/// rcodes), and the additional/authority sections of a *query* are not
/// interesting to an authoritative server that does not do EDNS.
pub fn parse_query(buf: &[u8]) -> Option<Query> {
    if buf.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([buf[0], buf[1]]);
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    // QR set means this is a response, not a query. Answering one would
    // make two responders shout at each other forever.
    if flags & 0x8000 != 0 {
        return None;
    }
    if qdcount == 0 {
        return Some(Query {
            id,
            flags,
            question: None,
            question_raw: Vec::new(),
        });
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        let len = *buf.get(pos)? as usize;
        // **Compression pointers are refused in a question.** A pointer in
        // the question section of a query has no legitimate sender and is
        // the classic way to make a parser follow itself in a loop.
        if len & 0xC0 != 0 {
            return None;
        }
        pos += 1;
        if len == 0 {
            break;
        }
        if len > 63 || pos + len > buf.len() || labels.len() >= 16 {
            return None;
        }
        labels.push(
            String::from_utf8_lossy(&buf[pos..pos + len])
                .to_ascii_lowercase()
                .to_string(),
        );
        pos += len;
    }
    if pos + 4 > buf.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
    let qclass = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
    Some(Query {
        id,
        flags,
        question: Some(Question {
            labels,
            qtype,
            qclass,
        }),
        question_raw: buf[12..pos + 4].to_vec(),
    })
}

/// Build the reply to `query` from the resolvable instances of a service.
///
/// `answers` is what discovery would hand out — already health-filtered by
/// [`crate::registry::resolvable`], because a responder that filtered
/// separately would be a second place the rule lives.
pub fn build_reply(query: &Query, answers: &[Registration], exists: bool, udp: bool) -> Vec<u8> {
    let Some(q) = &query.question else {
        return header_only(query, RCODE_FORMERR);
    };
    if q.qclass != CLASS_IN && q.qclass != TYPE_ANY {
        return header_only(query, RCODE_NOTIMP);
    }
    // Not our zone: refuse, never forward. A forwarding responder on every
    // node is a recursive resolver fleet nobody asked for.
    if !q.in_zone() {
        return header_only(query, RCODE_REFUSED);
    }
    if q.service().is_none() && q.instance().is_none() {
        // In the zone but not a form we define — `a.b.c.d.mesh`. That is an
        // honest NXDOMAIN: we are authoritative and it does not exist.
        return header_only(query, RCODE_NXDOMAIN);
    }
    // **NXDOMAIN means the name does not exist; it does not mean "no
    // healthy instances".** The distinction is load-bearing because
    // resolvers cache NXDOMAIN negatively: returning it for a service whose
    // instances are all momentarily critical would keep the name dead in
    // caches after they came back. So a name nothing has *ever* registered
    // is NXDOMAIN, and a name whose instances are merely unhealthy is
    // NOERROR with an empty answer.
    if !exists {
        return header_only(query, RCODE_NXDOMAIN);
    }

    // Answers (pointing at the question's name), and the additional
    // section that makes an SRV reply useful in one round trip.
    let mut records: Vec<Vec<u8>> = Vec::new();
    let mut additional: Vec<Vec<u8>> = Vec::new();
    for reg in answers {
        let addr = reg.address.parse::<IpAddr>().ok();
        match addr {
            Some(IpAddr::V4(v4)) if q.qtype == TYPE_A || q.qtype == TYPE_ANY => {
                records.push(rr(TYPE_A, &v4.octets()));
            }
            Some(IpAddr::V6(v6)) if q.qtype == TYPE_AAAA || q.qtype == TYPE_ANY => {
                records.push(rr(TYPE_AAAA, &v6.octets()));
            }
            _ => {}
        }
        if q.qtype == TYPE_SRV || q.qtype == TYPE_ANY {
            // priority 0, weight 1, port, then the target.
            //
            // **The target is a name, because SRV requires one.** An
            // address encoded as a label is not a domain name — a resolver
            // renders it back with the dots escaped, which is what
            // `127\.0\.0\.1.mesh` in a first run of this looked like. The
            // instance form `<job>.<service>.mesh` is minted from the job
            // id, which is already unique and already a legal label, and
            // this responder answers A for it.
            let target = instance_labels(reg);
            let mut rdata = Vec::with_capacity(24);
            rdata.extend_from_slice(&0u16.to_be_bytes());
            rdata.extend_from_slice(&1u16.to_be_bytes());
            rdata.extend_from_slice(&reg.port.to_be_bytes());
            encode_name(
                &mut rdata,
                &target.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            records.push(rr(TYPE_SRV, &rdata));
            // The target's address, so one round trip is enough. A client
            // that ignores this can still ask for the name and get the
            // same answer.
            if let Some(ip) = addr {
                let mut extra = Vec::new();
                encode_name(
                    &mut extra,
                    &target.iter().map(String::as_str).collect::<Vec<_>>(),
                );
                match ip {
                    IpAddr::V4(v4) => extra.extend_from_slice(&rr(TYPE_A, &v4.octets())),
                    IpAddr::V6(v6) => extra.extend_from_slice(&rr(TYPE_AAAA, &v6.octets())),
                }
                additional.push(extra);
            }
        }
    }

    // An empty answer for a name that *exists* is NOERROR with no records,
    // never NXDOMAIN. The distinction is load-bearing: NXDOMAIN is cached
    // negatively by resolvers, so returning it for a service whose
    // instances are all unhealthy would keep the name dead in caches after
    // the instances came back.
    let mut out = Vec::with_capacity(64 + records.len() * 32);
    write_header_full(
        &mut out,
        query.id,
        query.flags,
        RCODE_OK,
        1,
        records.len() as u16,
        additional.len() as u16,
    );
    out.extend_from_slice(&query.question_raw);
    for r in &records {
        // Name: a pointer back to the question's name at offset 12, which
        // is what every authoritative server does and what keeps a reply
        // with eight answers inside one datagram.
        out.extend_from_slice(&[0xC0, 0x0C]);
        out.extend_from_slice(r);
    }
    for a in &additional {
        out.extend_from_slice(a);
    }

    if udp && out.len() > MAX_UDP_REPLY {
        // Truncate to the header and question and set TC, so the client
        // retries over TCP rather than acting on a partial answer.
        let mut short = Vec::with_capacity(12 + query.question_raw.len());
        write_header(&mut short, query.id, query.flags, RCODE_OK, 1, 0);
        // Set TC.
        short[2] |= 0x02;
        short.extend_from_slice(&query.question_raw);
        return short;
    }
    out
}

/// One resource record, minus its name (the caller writes a pointer).
fn rr(rtype: u16, rdata: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + rdata.len());
    out.extend_from_slice(&rtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&TTL_SECS.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
    out
}

fn encode_name(out: &mut Vec<u8>, labels: &[&str]) {
    for l in labels {
        let bytes = l.as_bytes();
        let n = bytes.len().min(63);
        out.push(n as u8);
        out.extend_from_slice(&bytes[..n]);
    }
    out.push(0);
}

fn header_only(query: &Query, rcode: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + query.question_raw.len());
    let qd = if query.question.is_some() { 1 } else { 0 };
    write_header(&mut out, query.id, query.flags, rcode, qd, 0);
    out.extend_from_slice(&query.question_raw);
    out
}

/// The labels of one instance's own name: `job-abc123.webapp.mesh`.
fn instance_labels(reg: &Registration) -> Vec<String> {
    vec![
        reg.instance.short(),
        reg.service.clone(),
        DNS_SUFFIX.to_string(),
    ]
}

fn write_header(out: &mut Vec<u8>, id: u16, req_flags: u16, rcode: u8, qd: u16, an: u16) {
    write_header_full(out, id, req_flags, rcode, qd, an, 0);
}

fn write_header_full(
    out: &mut Vec<u8>,
    id: u16,
    req_flags: u16,
    rcode: u8,
    qd: u16,
    an: u16,
    ar: u16,
) {
    out.extend_from_slice(&id.to_be_bytes());
    // QR=1 (response), AA=1 (authoritative — we are, for this zone),
    // RD echoed from the request, RA=0 (we never recurse), rcode.
    let opcode = req_flags & 0x7800;
    let rd = req_flags & 0x0100;
    let flags: u16 = 0x8000 | opcode | 0x0400 | rd | u16::from(rcode);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&qd.to_be_bytes());
    out.extend_from_slice(&an.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // NS
    out.extend_from_slice(&ar.to_be_bytes());
}

/// Answer one datagram from replicated state.
///
/// `dead` is this node's liveness verdict for a publishing node (Phase A,
/// §2.3): an instance on a Dead node is not handed out, however healthy its
/// node last said it was. The name still *exists* — `NOERROR` with no
/// answers, not `NXDOMAIN` — for the same negative-caching reason an
/// all-critical service does.
pub fn answer(
    engine: &Arc<StateEngine>,
    buf: &[u8],
    udp: bool,
    dead: &dyn Fn(&mesh_proto::NodeId) -> bool,
) -> Option<Vec<u8>> {
    let query = parse_query(buf)?;
    let me = engine.node_id();
    let (answers, exists) = match query.question.as_ref() {
        // A service name: every resolvable instance, and whether anything
        // has registered under it at all.
        //
        // **The addresses of the machines running it, and nothing in
        // front of them.** A and AAAA carry each healthy instance's node
        // address; only SRV carries the port each one was given. Getting a
        // stable name and port in front of that — `https://example.com` —
        // is a load balancer's job, pointed at these addresses, the same
        // split Nomad makes (docs/operating/network.md).
        Some(q) if q.service().is_some() => {
            let service = q.service().expect("checked").to_string();
            engine.read(|s| {
                let exists = crate::registry::catalogue(s).contains_key(&service);
                let found = reachable(s, me, crate::registry::resolvable_on(s, &service, dead));
                (found, exists)
            })
        }
        // One instance by its own name — the form SRV targets point at.
        // Health-filtered like any other answer: an instance that has gone
        // critical stops resolving under its own name too, or an SRV
        // target cached a moment earlier would outlive the filter.
        Some(q) if q.instance().is_some() => {
            let (inst, service) = q.instance().expect("checked");
            let (inst, service) = (inst.to_string(), service.to_string());
            engine.read(|s| {
                let exists = crate::registry::catalogue(s)
                    .get(&service)
                    .is_some_and(|rs| rs.iter().any(|r| r.instance.short() == inst));
                let live = reachable(s, me, crate::registry::resolvable_on(s, &service, dead))
                    .into_iter()
                    .filter(|r| r.instance.short() == inst)
                    .collect();
                (live, exists)
            })
        }
        _ => (Vec::new(), false),
    };
    Some(build_reply(&query, &answers, exists, udp))
}

/// Registrations carrying the address the asking machine should use, rather
/// than the one each executor published — see
/// [`crate::registry::reachable_address`] for why those differ.
fn reachable(
    state: &crate::state::MeshState,
    asker: mesh_proto::NodeId,
    found: Vec<(mesh_proto::NodeId, Registration)>,
) -> Vec<Registration> {
    found
        .into_iter()
        .map(|(node, mut reg)| {
            reg.address = crate::registry::reachable_address(state, asker, node, &reg);
            reg
        })
        .collect()
}

/// What a probe of the configured bind address found (N7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Nothing is listening — the responder did not start. `.mesh` resolves
    /// nowhere, however the resolver is configured.
    Silent,
    /// A responder answered and knew the name we asked about. This mesh's.
    Ours,
    /// Something answered but did not know a name **this node's own
    /// catalogue says is registered**.
    ///
    /// Almost always a second meshnode on the same machine — a Director
    /// beside a daemon, two nodes in a test — where the first to start owns
    /// the port and the second logs `Address already in use` to a journal
    /// nobody reads. Its answers are a different mesh's, so `.mesh` names
    /// resolve to the wrong thing or to nothing at all, while every other
    /// surface in the product cheerfully reports the service healthy.
    Foreign,
    /// Listening, but this node has nothing registered to check it against.
    Unverified,
}

/// Ask the configured responder about a name we believe in (N7).
///
/// **The state, not the intent.** `doctor` used to report the bind address
/// out of the config and call it `ok`, which says nothing about whether a
/// responder is there — and "configured for 8600" is exactly what a node
/// whose responder failed to start also looks like.
pub fn probe(bind: SocketAddr, known_service: Option<&str>) -> Probe {
    // Nothing holding the port at all means our own responder is not there:
    // it binds at startup and holds it for the life of the process, so a
    // successful bind here is proof of absence.
    if std::net::UdpSocket::bind(bind).is_ok() {
        return Probe::Silent;
    }
    let Some(service) = known_service else {
        return Probe::Unverified;
    };
    let sock = match std::net::UdpSocket::bind(("127.0.0.1", 0)) {
        Ok(s) => s,
        Err(_) => return Probe::Unverified,
    };
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(700)));
    let mut q: Vec<u8> = vec![0x4d, 0x57, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    for label in [service, DNS_SUFFIX] {
        if label.is_empty() || label.len() > 63 {
            return Probe::Unverified;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0x00, 0x01, 0x00, 0x01]);
    if sock.send_to(&q, bind).is_err() {
        return Probe::Unverified;
    }
    let mut buf = [0u8; 1500];
    let Ok((n, _)) = sock.recv_from(&mut buf) else {
        // Something holds the port and will not answer. Not ours either.
        return Probe::Foreign;
    };
    if n < 12 {
        return Probe::Foreign;
    }
    // RCODE 3 is NXDOMAIN. A responder that does not know a name this
    // node's own catalogue lists is answering for a different mesh.
    let rcode = buf[3] & 0x0f;
    if rcode == 0 {
        Probe::Ours
    } else {
        Probe::Foreign
    }
}

/// What a probe found wrong, as the sentence a person reads — `None` when
/// it found nothing wrong (Phase A, §2.5d).
///
/// One wording for `doctor` and for the DNS status that `registry --dns`
/// and Director's Registry banner show, so the three cannot drift: the
/// banner used to say "answering" on the strength of the configured
/// address while `doctor`, one screen away, correctly said otherwise.
pub fn probe_problem(found: &Probe, bind: SocketAddr, known: Option<&str>) -> Option<String> {
    match found {
        Probe::Silent => Some(format!(
            concat!(
                "nothing is listening on {}, so no .{} name resolves — ",
                "the responder did not start. {}"
            ),
            bind,
            DNS_SUFFIX,
            if bind.port() < 1024 {
                "Binding below 1024 needs CAP_NET_BIND_SERVICE."
            } else {
                "`meshnode up`'s log says why."
            }
        )),
        Probe::Foreign => Some(format!(
            concat!(
                "the responder on {} does not know {:?}, which this node has ",
                "registered — another meshnode on this machine owns the port and ",
                "is answering for its own mesh. `ss -lnup | grep {}` names it; ",
                "give one of them a different `[network] dns_port`."
            ),
            bind,
            known.unwrap_or(""),
            bind.port(),
        )),
        Probe::Ours | Probe::Unverified => None,
    }
}

/// Serve `.mesh` over UDP until shutdown.
pub async fn serve(
    engine: Arc<StateEngine>,
    bind: SocketAddr,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    dead: impl Fn(&mesh_proto::NodeId) -> bool + Send + 'static,
) {
    let sock = match UdpSocket::bind(bind).await {
        Ok(s) => s,
        Err(e) => {
            // Named, not swallowed. Port 53 without `CAP_NET_BIND_SERVICE`
            // is the likely cause and the message says so, because a DNS
            // responder that quietly did not start is indistinguishable
            // from a mesh where nothing is registered.
            warn!(
                "DNS responder not started on {bind}: {e}{}",
                if bind.port() < 1024 {
                    " — binding a port below 1024 needs CAP_NET_BIND_SERVICE; \
                     see docs/operating/limits.md"
                } else {
                    ""
                }
            );
            return;
        }
    };
    info!("DNS responder on {bind}, authoritative for .{DNS_SUFFIX}");
    let mut buf = vec![0u8; MAX_QUERY];
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            got = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = got else { continue };
                let Some(reply) = answer(&engine, &buf[..n], true, &dead) else {
                    // Unparseable: drop it. There is no safe reply to a
                    // datagram whose header we could not read, and an
                    // error to a spoofed source address is an amplifier.
                    debug!("dropped an unparseable DNS query from {from}");
                    continue;
                };
                if let Err(e) = sock.send_to(&reply, from).await {
                    debug!("DNS reply to {from} failed: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::{Health, JobId};

    /// `webapp.mesh`, type A, class IN — the query a resolver actually
    /// sends.
    fn query_for(name: &[&str], qtype: u16) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&0x1234u16.to_be_bytes()); // id
        q.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
        q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        q.extend_from_slice(&0u16.to_be_bytes());
        q.extend_from_slice(&0u16.to_be_bytes());
        q.extend_from_slice(&0u16.to_be_bytes());
        encode_name(&mut q, name);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    fn reg(addr: &str, port: u16) -> Registration {
        Registration {
            service: "webapp".into(),
            instance: JobId([1; 16]),
            label: "http".into(),
            address: addr.into(),
            port,
            tags: vec![],
            health: Health::Passing,
            since_micros: 0,
            deployment: String::new(),
        }
    }

    fn rcode(reply: &[u8]) -> u8 {
        reply[3] & 0x0F
    }
    fn ancount(reply: &[u8]) -> u16 {
        u16::from_be_bytes([reply[6], reply[7]])
    }

    #[test]
    fn a_query_for_a_service_parses_into_its_name() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_A)).expect("parse");
        assert_eq!(q.id, 0x1234);
        let question = q.question.expect("a question");
        assert_eq!(question.service(), Some("webapp"));
        assert_eq!(question.qtype, TYPE_A);
    }

    /// Case-insensitive, because DNS is and clients rely on it.
    #[test]
    fn a_name_resolves_whatever_its_case() {
        let q = parse_query(&query_for(&["WebApp", "MESH"], TYPE_A)).expect("parse");
        assert_eq!(q.question.unwrap().service(), Some("webapp"));
    }

    #[test]
    fn an_a_query_answers_with_the_address() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_A)).unwrap();
        let reply = build_reply(&q, &[reg("10.0.0.4", 20000)], true, true);
        assert_eq!(rcode(&reply), RCODE_OK);
        assert_eq!(ancount(&reply), 1);
        // The last four bytes of a single A answer are the address.
        assert_eq!(&reply[reply.len() - 4..], &[10, 0, 0, 4]);
        // The reply echoes the request id, or a resolver discards it.
        assert_eq!(u16::from_be_bytes([reply[0], reply[1]]), 0x1234);
        // AA is set: we are authoritative for this zone.
        assert_eq!(reply[2] & 0x04, 0x04);
    }

    #[test]
    fn every_healthy_instance_is_an_answer() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_A)).unwrap();
        let reply = build_reply(
            &q,
            &[reg("10.0.0.4", 1), reg("10.0.0.5", 2), reg("10.0.0.6", 3)],
            true,
            true,
        );
        assert_eq!(ancount(&reply), 3);
    }

    /// An SRV answer carries the port, which is the whole reason a client
    /// asks for one.
    #[test]
    fn an_srv_query_answers_with_the_port() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_SRV)).unwrap();
        let reply = build_reply(&q, &[reg("10.0.0.4", 20000)], true, true);
        assert_eq!(ancount(&reply), 1);
        assert!(
            reply.windows(2).any(|w| w == 20000u16.to_be_bytes()),
            "the port is in the answer"
        );
    }

    #[test]
    fn an_aaaa_query_answers_only_v6_addresses() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_AAAA)).unwrap();
        assert_eq!(
            ancount(&build_reply(&q, &[reg("10.0.0.4", 1)], true, true)),
            0,
            "a v4 address is not an AAAA answer"
        );
        assert_eq!(
            ancount(&build_reply(&q, &[reg("fd00::4", 1)], true, true)),
            1
        );
    }

    /// **NOERROR with no answers, never NXDOMAIN.** NXDOMAIN is cached
    /// negatively, so returning it for a service whose instances are all
    /// unhealthy would keep the name dead in resolver caches after they
    /// came back.
    #[test]
    fn a_service_with_no_healthy_instances_is_not_nxdomain() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_A)).unwrap();
        // `exists` is true: something has registered under this name, its
        // instances are just all unhealthy right now.
        let reply = build_reply(&q, &[], true, true);
        assert_eq!(
            rcode(&reply),
            RCODE_OK,
            "not NXDOMAIN — caches remember that"
        );
        assert_eq!(ancount(&reply), 0);
    }

    /// Refused, never forwarded. A forwarding responder on every node is a
    /// recursive resolver fleet nobody asked for.
    #[test]
    fn a_name_outside_the_zone_is_refused_rather_than_forwarded() {
        let q = parse_query(&query_for(&["example", "com"], TYPE_A)).unwrap();
        assert_eq!(rcode(&build_reply(&q, &[], false, true)), RCODE_REFUSED);
    }

    /// A name nothing has ever registered is NXDOMAIN — we are
    /// authoritative, so the honest answer is that it does not exist.
    #[test]
    fn a_name_nothing_registered_is_nxdomain() {
        for name in [
            vec!["nosuch", "mesh"],
            vec!["job-abc", "nosuch", "mesh"],
            // Too deep to be either form this zone defines.
            vec!["a", "b", "c", "mesh"],
        ] {
            let q = parse_query(&query_for(&name, TYPE_A)).unwrap();
            assert_eq!(
                rcode(&build_reply(&q, &[], false, true)),
                RCODE_NXDOMAIN,
                "{name:?}"
            );
        }
    }

    /// An SRV target must be a *name*. A first run of this encoded the
    /// address as one label, and a resolver rendered it back as
    /// `127\.0\.0\.1.mesh` — escaped dots, resolvable by nothing. The
    /// instance form is minted from the job id, and the responder answers
    /// A for it so the target is real.
    #[test]
    fn an_srv_target_is_a_resolvable_name_with_glue() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_SRV)).unwrap();
        let r = reg("10.0.0.4", 20000);
        let reply = build_reply(&q, std::slice::from_ref(&r), true, true);
        assert_eq!(ancount(&reply), 1);
        // The target's first label is the instance's short id, as its own
        // label — so there are no dots inside a label to escape.
        let short = r.instance.short();
        assert!(
            reply.windows(short.len()).any(|w| w == short.as_bytes()),
            "the target names the instance"
        );
        // And its address rides the additional section, so one round trip
        // is enough.
        assert_eq!(
            u16::from_be_bytes([reply[10], reply[11]]),
            1,
            "one additional record"
        );
        assert_eq!(
            &reply[reply.len() - 4..],
            &[10, 0, 0, 4],
            "the glue address"
        );
    }

    /// A pointer in a question has no legitimate sender and is the classic
    /// way to make a parser follow itself in a loop.
    #[test]
    fn a_compression_pointer_in_the_question_is_refused() {
        let mut q = query_for(&["webapp", "mesh"], TYPE_A);
        q[12] = 0xC0;
        q[13] = 0x0C;
        assert!(parse_query(&q).is_none());
    }

    /// A response is not a query. Answering one makes two responders shout
    /// at each other forever.
    #[test]
    fn a_response_is_never_answered() {
        let mut q = query_for(&["webapp", "mesh"], TYPE_A);
        q[2] |= 0x80;
        assert!(parse_query(&q).is_none());
    }

    #[test]
    fn truncated_and_empty_input_is_refused_rather_than_panicking() {
        assert!(parse_query(&[]).is_none());
        assert!(parse_query(&[0u8; 11]).is_none());
        let q = query_for(&["webapp", "mesh"], TYPE_A);
        for cut in 12..q.len() {
            // Must not panic at any truncation point.
            let _ = parse_query(&q[..cut]);
        }
    }

    /// Past 512 bytes a UDP reply sets TC and the client retries over TCP,
    /// rather than receiving a datagram the network may silently drop.
    #[test]
    fn a_large_answer_sets_tc_over_udp() {
        let q = parse_query(&query_for(&["webapp", "mesh"], TYPE_A)).unwrap();
        let many: Vec<Registration> = (0..100).map(|i| reg(&format!("10.0.{i}.1"), 1)).collect();
        let reply = build_reply(&q, &many, true, true);
        assert!(reply.len() <= MAX_UDP_REPLY);
        assert_eq!(reply[2] & 0x02, 0x02, "TC is set");
        assert_eq!(ancount(&reply), 0);
        // Over TCP the same answer is whole.
        let whole = build_reply(&q, &many, true, false);
        assert_eq!(ancount(&whole), 100);
    }
}
