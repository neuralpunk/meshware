//! What a node tells the mesh it can be reached at (ENG-22).
//!
//! The rule the network doc states — *"nodes advertise every address they
//! observe about themselves"* — was not what the code did. A node
//! advertised exactly two things: the source address of its IPv4 default
//! route, and `127.0.0.1`. That is right for the single-NIC IPv4 machine
//! and wrong for two kinds of host that are not unusual:
//!
//! - **IPv6-only.** The probe binds `0.0.0.0` and connects to a v4
//!   address, so on a host with no IPv4 it fails and the node advertises
//!   loopback and nothing else. It joins, it gossips over whatever
//!   connection it opened outward, and no peer can ever dial it.
//! - **Multi-NIC.** An HPC or GPU host has a management NIC holding the
//!   default route and a fast fabric that mesh traffic should be on. The
//!   default-route probe finds the management NIC every time, because
//!   that is the definition of the thing it looks for.
//!
//! ## What is advertised, and in what order
//!
//! Every address on every interface, minus the ones that cannot work,
//! best first — because [`crate::transport::Transport::race_direct`] dials
//! candidates on a stagger, so the order is a latency budget and not just
//! a list.
//!
//! - **The default route's own address first**, when there is one. It is
//!   the one address already proven to carry traffic off this machine.
//! - Then global unicast, then private and unique-local, then loopback.
//! - **Link-local is never advertised.** IPv6 link-local needs a zone
//!   (`fe80::1%eth0`) to be dialable at all, and a zone is an index into
//!   *this* machine's interface table that means something different on
//!   the peer reading it. IPv4 link-local (169.254/16) is an
//!   autoconfiguration address and reaches nothing.
//! - Loopback goes last and is always present: a single-node mesh, and
//!   every test, dial it.
//! - **Only families the listener serves.** A node bound to `0.0.0.0` used
//!   to advertise `[::1]` alongside `127.0.0.1` — an address nothing on it
//!   was listening on, and a dial slot and a timeout for every peer that
//!   tried it. See [`Families`].
//!
//! ## `[network] advertise_interfaces`
//!
//! Names the interfaces mesh traffic belongs on — the fabric, not the
//! management NIC — and it is also the answer to the other thing
//! enumerating every interface finds. A host with Docker on it has
//! `docker0` at `172.17.0.1` and a `veth` per container, all of them
//! perfectly good addresses that reach nothing from outside the machine.
//! They are advertised, because a bridge is not distinguishable from a
//! second real NIC by looking at it, and [`MAX_ADVERTISED`] bounds what
//! that can cost. An operator who cares says which interfaces.

use std::net::IpAddr;

/// Which families a listener actually serves, so a node never advertises
/// an address nothing is listening on.
///
/// A peer dials every candidate on a stagger, so a wrong one is not just
/// noise — it is a dial slot and a timeout on the path to reaching this
/// node. A node bound to `0.0.0.0` was advertising `[::1]` before ENG-22,
/// because the enumeration and the listener were never compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Families {
    pub v4: bool,
    pub v6: bool,
}

impl Families {
    /// What a listener bound to `listen_ip` serves.
    ///
    /// `[::]` is dual-stack on Linux as shipped, so it serves IPv4 too —
    /// unless `net.ipv6.bindv6only` says otherwise, which `dual_stack`
    /// carries. `0.0.0.0` is IPv4 and nothing else, whatever the sysctl
    /// says.
    pub fn of(listen_ip: IpAddr, dual_stack: bool) -> Families {
        match listen_ip {
            IpAddr::V4(_) => Families {
                v4: true,
                v6: false,
            },
            IpAddr::V6(_) => Families {
                v4: dual_stack,
                v6: true,
            },
        }
    }

    fn allows(self, ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(_) => self.v4,
            IpAddr::V6(_) => self.v6,
        }
    }
}

/// Does a socket bound to `[::]` also accept IPv4 here?
///
/// `net.ipv6.bindv6only` is 0 as every distribution ships it, which makes
/// `[::]` dual-stack. Read rather than assumed because the node that has
/// it set to 1 is exactly the node whose operator would never think to
/// look here, and the symptom is peers dialling IPv4 candidates that
/// nothing answers.
pub fn dual_stack() -> bool {
    std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only")
        .map(|s| s.trim() != "1")
        .unwrap_or(true)
}

/// Addresses a node will advertise, after ranking.
///
/// A bound rather than a belief that no machine has more: each one is a
/// staggered dial attempt on every peer that wants to reach this node, so
/// a build host with forty `veth` interfaces would make every connection
/// to it slow. Eight is past any real NIC count and short enough that the
/// race is over quickly.
pub const MAX_ADVERTISED: usize = 8;

/// One address this machine holds, and the interface it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub interface: String,
    pub ip: IpAddr,
}

/// Every address on every interface of this machine.
///
/// `getifaddrs`, which on Linux is netlink underneath — the same call
/// `pairing::local_v4` already makes, and already allowed by the packaged
/// unit. An empty list on failure: a node that cannot enumerate its
/// interfaces still has its default route and its loopback.
pub fn enumerate() -> Vec<Local> {
    if_addrs::get_if_addrs()
        .map(|ifaces| {
            ifaces
                .into_iter()
                .map(|i| Local {
                    interface: i.name,
                    ip: i.addr.ip(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The source address of this machine's default route, per family.
///
/// No packets are sent: connecting a UDP socket only asks the kernel which
/// source address it *would* use. Both families are probed, because the
/// v4-only probe is precisely what left an IPv6-only node advertising
/// loopback.
pub fn default_route_ips() -> Vec<IpAddr> {
    // TEST-NET-1 and the documentation prefix: reserved, never routed,
    // and picked so that a packet leaking from a future change cannot
    // reach anything real.
    [("0.0.0.0:0", "192.0.2.1:9"), ("[::]:0", "[2001:db8::1]:9")]
        .into_iter()
        .filter_map(|(bind, probe)| {
            let sock = std::net::UdpSocket::bind(bind).ok()?;
            sock.connect(probe).ok()?;
            Some(sock.local_addr().ok()?.ip())
        })
        .collect()
}

/// Can this address be dialed by a peer at all?
fn is_dialable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_link_local() && !v4.is_broadcast(),
        // A zone-less link-local is not an address anybody else can use,
        // and the member record has nowhere to put a zone that would mean
        // the same thing on the node reading it.
        IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_unicast_link_local(),
    }
}

/// Lower sorts first. Ties keep enumeration order, which is `getifaddrs`'
/// order, which is stable for a given machine — so a node's advertised
/// list does not reshuffle on every restart for no reason.
fn rank(ip: &IpAddr, default_routes: &[IpAddr]) -> u8 {
    if default_routes.contains(ip) {
        return 0;
    }
    match ip {
        _ if ip.is_loopback() => 9,
        IpAddr::V4(v4) => {
            if v4.is_private() {
                2
            } else {
                1
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unique_local() {
                2
            } else {
                1
            }
        }
    }
}

/// Format one address as the `host:port` a member record carries. IPv6 is
/// bracketed, which is what `resolve` and `host_port_problem` require.
pub fn socket_string(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

/// Choose what to advertise, from what this machine has.
///
/// Pure, so the ranking can be tested without a machine that happens to
/// have two NICs: [`enumerate`] and [`default_route_ips`] supply the two
/// facts about the world, and everything after them is this.
///
/// `only` restricts to named interfaces (`[network] advertise_interfaces`)
/// — except that an empty result falls back to every interface rather than
/// to nothing. A typo'd interface name must not leave a node undialable
/// and silent about why; it warns and advertises what it has.
pub fn choose(
    locals: &[Local],
    default_routes: &[IpAddr],
    only: &[String],
    families: Families,
    port: u16,
) -> Vec<String> {
    let usable = |l: &&Local| is_dialable(&l.ip) && families.allows(&l.ip);
    let named = |l: &Local| only.contains(&l.interface);
    let mut chosen: Vec<&Local> = locals
        .iter()
        .filter(usable)
        .filter(|l| only.is_empty() || named(l))
        .collect();
    if chosen.is_empty() && !only.is_empty() {
        tracing::warn!(
            interfaces = ?only,
            "[network] advertise_interfaces names no interface on this machine that has a \
             usable address — advertising every interface instead. Check the names against \
             `ip -brief addr`."
        );
        chosen = locals.iter().filter(usable).collect();
    }

    // Stable sort: the rank decides, and enumeration order breaks ties.
    chosen.sort_by_key(|l| rank(&l.ip, default_routes));

    let mut out: Vec<String> = Vec::new();
    for l in chosen {
        let s = socket_string(l.ip, port);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.len() > MAX_ADVERTISED {
        tracing::warn!(
            found = out.len(),
            kept = MAX_ADVERTISED,
            "this machine has more usable addresses than a peer should have to dial through; \
             advertising the best {MAX_ADVERTISED}. Name the ones mesh traffic belongs on with \
             [network] advertise_interfaces."
        );
        out.truncate(MAX_ADVERTISED);
    }

    // Always dialable from this machine, and the only address a one-node
    // mesh has. Appended after the cap so restricting the list can never
    // be the thing that removes it — and in whichever family the listener
    // actually serves, because on a v6-only listener `127.0.0.1` is an
    // address nothing is bound to.
    let loopback = if families.v4 {
        socket_string(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
    } else {
        socket_string(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), port)
    };
    if !out.contains(&loopback) {
        out.push(loopback);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(interface: &str, ip: &str) -> Local {
        Local {
            interface: interface.into(),
            ip: ip.parse().expect("test address"),
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address")
    }

    /// A dual-stack listener: what `[::]` is on Linux as shipped, and the
    /// case that exercises the ranking rather than the family filter.
    const BOTH: Families = Families { v4: true, v6: true };

    /// The IPv6-only host: the v4 default-route probe finds nothing, and
    /// before ENG-22 that left the node advertising loopback and nothing
    /// else — joined, gossiping, and unreachable by every peer.
    #[test]
    fn an_ipv6_only_host_advertises_its_ipv6() {
        let out = choose(
            &[
                local("lo", "127.0.0.1"),
                local("lo", "::1"),
                local("eth0", "2001:db8:1::5"),
                local("eth0", "fe80::1"),
            ],
            &[ip("2001:db8:1::5")],
            &[],
            BOTH,
            7777,
        );
        assert_eq!(
            out.first().map(String::as_str),
            Some("[2001:db8:1::5]:7777")
        );
        assert!(out.contains(&"127.0.0.1:7777".to_string()));
        // Link-local is not an address a peer can use: a zone index means
        // something different on the machine reading it.
        assert!(
            !out.iter().any(|a| a.contains("fe80")),
            "link-local was advertised: {out:?}"
        );
    }

    /// The multi-NIC host. Both NICs are advertised — the default route
    /// first, because it is the one already proven to carry traffic — so a
    /// peer on the fabric can reach this node over the fabric.
    #[test]
    fn every_nic_is_advertised_best_first() {
        let out = choose(
            &[
                local("lo", "127.0.0.1"),
                local("ib0", "10.99.0.5"),
                local("eth0", "192.168.1.10"),
            ],
            &[ip("192.168.1.10")],
            &[],
            BOTH,
            7777,
        );
        assert_eq!(
            out,
            vec!["192.168.1.10:7777", "10.99.0.5:7777", "127.0.0.1:7777"]
        );
    }

    /// A global address outranks a private one, whichever order the
    /// kernel listed them in: it is the one that can be dialed from off
    /// the LAN.
    #[test]
    fn a_routable_address_outranks_a_private_one() {
        let out = choose(
            &[
                local("docker0", "172.17.0.1"),
                local("eth0", "203.0.113.7"),
                local("lo", "127.0.0.1"),
            ],
            &[],
            &[],
            BOTH,
            7777,
        );
        assert_eq!(out.first().map(String::as_str), Some("203.0.113.7:7777"));
        assert_eq!(out.last().map(String::as_str), Some("127.0.0.1:7777"));
    }

    /// `[network] advertise_interfaces` — the HPC case: keep mesh traffic
    /// off the management NIC.
    #[test]
    fn named_interfaces_are_the_only_ones_advertised() {
        let locals = [
            local("eth0", "192.168.1.10"),
            local("ib0", "10.99.0.5"),
            local("docker0", "172.17.0.1"),
            local("lo", "127.0.0.1"),
        ];
        let out = choose(&locals, &[ip("192.168.1.10")], &["ib0".into()], BOTH, 7777);
        assert_eq!(out, vec!["10.99.0.5:7777", "127.0.0.1:7777"]);
    }

    /// …and a name that matches nothing falls back rather than leaving the
    /// node undialable. A typo in a config key must not silently remove a
    /// machine from the mesh.
    #[test]
    fn a_misspelt_interface_name_does_not_silence_the_node() {
        let locals = [local("eth0", "192.168.1.10"), local("lo", "127.0.0.1")];
        let out = choose(&locals, &[], &["ibo".into()], BOTH, 7777);
        assert!(
            out.contains(&"192.168.1.10:7777".to_string()),
            "fell back to nothing: {out:?}"
        );
    }

    /// A build host with a container per job has a `veth` per container.
    /// Every one is a real address and none of them reaches anything, and
    /// a peer would dial all of them on a stagger before giving up.
    #[test]
    fn a_host_with_many_interfaces_does_not_make_every_peer_dial_them_all() {
        let mut locals = vec![local("eth0", "192.168.1.10")];
        for i in 0..40u8 {
            locals.push(local(&format!("veth{i}"), &format!("172.20.{i}.1")));
        }
        locals.push(local("lo", "127.0.0.1"));
        let out = choose(&locals, &[ip("192.168.1.10")], &[], BOTH, 7777);
        assert_eq!(out.len(), MAX_ADVERTISED + 1, "{out:?}");
        // The real NIC survives the cap, and so does loopback — which is
        // appended after it, so a truncation can never be what removes the
        // one address this machine can always be reached at.
        assert_eq!(out.first().map(String::as_str), Some("192.168.1.10:7777"));
        assert_eq!(out.last().map(String::as_str), Some("127.0.0.1:7777"));
    }

    /// IPv6 has to be bracketed or the member record is unparseable, and
    /// `host_port_problem` refuses it with that exact advice.
    #[test]
    fn ipv6_is_written_bracketed() {
        let out = choose(&[local("eth0", "2001:db8::1")], &[], &[], BOTH, 7777);
        assert!(out.contains(&"[2001:db8::1]:7777".to_string()), "{out:?}");
        for a in &out {
            assert!(
                a.parse::<std::net::SocketAddr>().is_ok(),
                "{a} is not a socket address"
            );
        }
    }

    /// The same address on two interfaces is one candidate, not two dials.
    #[test]
    fn a_duplicate_address_is_advertised_once() {
        let out = choose(
            &[local("eth0", "10.0.0.1"), local("eth0:1", "10.0.0.1")],
            &[],
            &[],
            BOTH,
            7777,
        );
        assert_eq!(out, vec!["10.0.0.1:7777", "127.0.0.1:7777"]);
    }

    /// A node bound to `0.0.0.0` listens on IPv4 and nothing else, so an
    /// IPv6 candidate in its member record is a dial slot and a timeout
    /// for every peer that tries to reach it. It used to advertise `[::1]`.
    #[test]
    fn a_v4_listener_never_advertises_a_v6_address() {
        let locals = [
            local("lo", "127.0.0.1"),
            local("lo", "::1"),
            local("eth0", "192.168.1.10"),
            local("eth0", "2001:db8::1"),
        ];
        let v4 = Families::of(ip("0.0.0.0"), true);
        let out = choose(&locals, &[], &[], v4, 7777);
        assert_eq!(out, vec!["192.168.1.10:7777", "127.0.0.1:7777"]);
    }

    /// …and the other way. A v6-only listener's loopback is `[::1]`, not
    /// `127.0.0.1`: appending the v4 one unconditionally would put an
    /// address in the record that nothing on the machine is bound to.
    #[test]
    fn a_v6_only_listener_advertises_the_v6_loopback() {
        let locals = [
            local("lo", "127.0.0.1"),
            local("lo", "::1"),
            local("eth0", "2001:db8::1"),
        ];
        let v6 = Families::of(ip("::"), false);
        let out = choose(&locals, &[], &[], v6, 7777);
        assert_eq!(out, vec!["[2001:db8::1]:7777", "[::1]:7777"]);
    }

    /// `[::]` on Linux as shipped is dual-stack, and a node bound to it
    /// really can be reached on both.
    #[test]
    fn a_dual_stack_listener_advertises_both() {
        let both = Families::of(ip("::"), true);
        assert_eq!(both, BOTH);
        let out = choose(
            &[local("eth0", "192.168.1.10"), local("eth0", "2001:db8::1")],
            &[],
            &[],
            both,
            7777,
        );
        assert!(out.contains(&"192.168.1.10:7777".to_string()), "{out:?}");
        assert!(out.contains(&"[2001:db8::1]:7777".to_string()), "{out:?}");
    }

    /// This machine, whatever it is: the enumeration must at least run,
    /// and everything it returns must be a parseable candidate.
    #[test]
    fn this_machine_enumerates_into_usable_candidates() {
        let out = choose(&enumerate(), &default_route_ips(), &[], BOTH, 7777);
        assert!(!out.is_empty(), "not even loopback");
        for a in &out {
            assert!(
                a.parse::<std::net::SocketAddr>().is_ok(),
                "{a} is not a socket address"
            );
        }
        assert!(out.contains(&"127.0.0.1:7777".to_string()));
    }
}
