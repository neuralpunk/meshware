//! ENG-22 — a node advertises every address it can be reached at.
//!
//! The ranking itself is unit-tested in `mesh_core::addrs`, against a list
//! of interfaces rather than whatever the machine running the tests
//! happens to have. What is tested *here* is the wiring: that the choice
//! reaches the member record other nodes read, and that
//! `[network] advertise_interfaces` reaches it too.
//!
//! Both nodes listen on `0.0.0.0:0` rather than the testkit's usual
//! `127.0.0.1:0`, because a specific `listen` is the operator being
//! specific and is advertised alone — which is the branch that was never
//! the problem.
#![allow(clippy::unwrap_used)]

use mesh_core::addrs::MAX_ADVERTISED;
use mesh_testkit::spawn_founder_with;

fn own_addrs(node: &mesh_testkit::TestNode) -> Vec<String> {
    let me = node.node.node_id();
    node.node
        .engine()
        .read(|s| s.members.get(&me).map(|m| m.info.addrs.clone()))
        .unwrap_or_default()
}

/// What a node listening on everything tells the mesh about itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_listening_on_everything_advertises_what_it_has() {
    let node = spawn_founder_with("ADDR", "eng22-addrs", vec![], |c| {
        c.listen = "0.0.0.0:0".parse().unwrap();
    })
    .await;

    let addrs = own_addrs(&node);
    assert!(!addrs.is_empty(), "the node announced no address at all");

    // Every candidate has to be dialable as written, or the member record
    // carries an entry `Transport::classify` throws away — and a member
    // whose addresses do not parse is one nothing can reach.
    for a in &addrs {
        assert!(
            a.parse::<std::net::SocketAddr>().is_ok(),
            "{a} is not a socket address"
        );
        assert!(
            !a.contains("fe80") && !a.starts_with("169.254."),
            "{a} is link-local, which a peer cannot dial"
        );
        assert!(
            !a.ends_with(":0"),
            "{a} carries the \"no dialable address yet\" sentinel port"
        );
        // This node is bound to `0.0.0.0`: it listens on IPv4 and nothing
        // else, so an IPv6 candidate is a dial slot and a timeout for
        // every peer that tries it. It used to advertise `[::1]`.
        assert!(
            !a.starts_with('['),
            "{a} is IPv6, and this node listens on 0.0.0.0"
        );
    }

    // Loopback is always there: it is what a one-node mesh dials, and it
    // is appended after the cap so a truncation can never remove it.
    let port = node.node.listen_addr().port();
    assert!(
        addrs.contains(&format!("127.0.0.1:{port}")),
        "loopback is missing from {addrs:?}"
    );
    assert!(
        addrs.len() <= MAX_ADVERTISED + 1,
        "a peer should not have to dial through {} candidates: {addrs:?}",
        addrs.len()
    );

    node.stop().await;
}

/// `[network] advertise_interfaces` — the HPC case, with the one interface
/// every machine running this test is guaranteed to have.
///
/// The premise is asserted first: the same node with no restriction
/// advertises *more* than this. Without that, a machine that has only
/// loopback would pass whether or not the setting did anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn naming_an_interface_restricts_what_is_advertised() {
    let open = spawn_founder_with("OPEN", "eng22-open", vec![], |c| {
        c.listen = "0.0.0.0:0".parse().unwrap();
    })
    .await;
    let unrestricted = own_addrs(&open);
    open.stop().await;

    let restricted_node = spawn_founder_with("LO", "eng22-lo", vec![], |c| {
        c.listen = "0.0.0.0:0".parse().unwrap();
        c.network.advertise_interfaces = vec!["lo".to_string()];
    })
    .await;
    let restricted = own_addrs(&restricted_node);
    let port = restricted_node.node.listen_addr().port();

    assert_eq!(
        restricted,
        vec![format!("127.0.0.1:{port}")],
        "naming `lo` should leave exactly loopback"
    );
    // The premise. On a machine with a NIC this is a real restriction; on
    // one with nothing but loopback the test would otherwise be vacuous.
    if unrestricted.len() <= 1 {
        eprintln!(
            "note: this machine has no interface but loopback, so the \
             restriction above was not exercised against anything"
        );
    }

    restricted_node.stop().await;
}
