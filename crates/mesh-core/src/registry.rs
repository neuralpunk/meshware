//! The service registry, node side (N3 §9).
//!
//! The record and its rules live in [`mesh_proto::registry`]; this is the
//! part that needs the node — validating what may be registered, and
//! folding every node's record into the catalogue a resolver reads.

use std::collections::BTreeMap;

use crate::state::MeshState;

/// The health an instance reads as when the node that publishes it is Dead
/// (Phase A, §2.3). A view word, never a replicated one: `Health` is inside
/// a signed op and does not grow a variant for a conclusion only the reader
/// can draw.
pub const UNREACHABLE: &str = "unreachable";

/// Is this a name a service may register under?
///
/// It becomes a DNS label, so it has to be one: letters, digits and
/// hyphens, not starting or ending with a hyphen, at most 63 octets. A name
/// that cannot be a label would register fine and then be unresolvable —
/// the worst of both, findable in a table and absent from the thing the
/// table exists to feed.
pub fn is_valid_service_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        // All-digits is a legal DNS label and a terrible service name:
        // `curl 8.mesh` is ambiguous with an address to anything that
        // guesses, so it is refused where the refusal is cheap.
        && !name.chars().all(|c| c.is_ascii_digit())
}

/// The registrations that still stand behind something (2026-09-14).
///
/// A node's record is only ever replaced by that node, so two kinds of row
/// outlived the work behind them with nobody able to remove them: a node
/// that has left the mesh or been revoked never publishes again, and a copy
/// whose job has ended stays in its node's last record until the node says
/// something new — which, for a node that came back from a restart running
/// nothing, it had no reason to. Found on a real mesh as a failed copy of
/// `web` still listed as passing. Replicated state already knows both
/// things: the membership, and the job's own terminal state.
///
/// A node that is merely Dead keeps its rows — it is still a member and
/// may come back — and they read `unreachable` (Phase A, §2.3).
fn standing(
    state: &MeshState,
) -> impl Iterator<Item = (&mesh_proto::NodeRegistrations, &mesh_proto::Registration)> + '_ {
    state
        .registry
        .values()
        .filter(move |n| {
            !state.revoked.contains_key(&n.node)
                && !state.members.get(&n.node).is_some_and(|m| m.left)
        })
        .flat_map(move |n| {
            n.services
                .iter()
                .filter(move |r| {
                    !state
                        .jobs
                        .get(&r.instance)
                        .is_some_and(|j| j.state.is_terminal())
                })
                .map(move |r| (n, r))
        })
}

/// Every registration in the mesh, by service name.
///
/// **The union across every node's record**, which is why any node can
/// answer authoritatively with no round trip — the property Nomad
/// structurally cannot have, because its check results never leave the
/// client that measured them.
///
/// Sorted within each service by instance, so two nodes asked the same
/// question return the same order and a client that takes the first answer
/// is not silently load-balancing on map iteration order.
pub fn catalogue(state: &MeshState) -> BTreeMap<String, Vec<mesh_proto::Registration>> {
    let mut out: BTreeMap<String, Vec<mesh_proto::Registration>> = BTreeMap::new();
    for (_, reg) in standing(state) {
        out.entry(reg.service.clone())
            .or_default()
            .push(reg.clone());
    }
    for regs in out.values_mut() {
        regs.sort_by(|a, b| (a.instance.0, &a.label).cmp(&(b.instance.0, &b.label)));
    }
    out
}

/// The instances discovery will actually hand out for `service`.
///
/// Health-filtered, which is the entire reason the health conclusion is in
/// replicated state rather than on the node that measured it — and
/// **liveness-filtered**, which is the reason that is not enough on its own
/// (Phase A, `docs/NOMAD-ENHANCEMENTS.md` §2.3).
///
/// A health result is only as current as the node that publishes it. A node
/// that dies with its instances `passing` can never publish anything again,
/// so its last word stays `passing` in every other node's catalogue for as
/// long as the mesh lives, and `web.mesh` keeps answering with an address
/// nobody is behind. `dead` is this node's own liveness verdict for the
/// publishing node: **Dead is excluded, Suspect is not**, because a Suspect
/// verdict on a relayed or CGNAT link is usually a blip, and a DNS answer
/// that flapped with every lost probe would be worse than a stale one.
pub fn resolvable(
    state: &MeshState,
    service: &str,
    dead: impl Fn(&mesh_proto::NodeId) -> bool,
) -> Vec<mesh_proto::Registration> {
    let mut out: Vec<mesh_proto::Registration> = standing(state)
        .filter(|(n, r)| !dead(&n.node) && r.service == service && r.health.resolvable())
        .map(|(_, r)| r.clone())
        .collect();
    out.sort_by(|a, b| (a.instance.0, &a.label).cmp(&(b.instance.0, &b.label)));
    out
}

/// [`resolvable`], with the node that runs each instance — which is what an
/// address to reach it is worked out from.
pub fn resolvable_on(
    state: &MeshState,
    service: &str,
    dead: impl Fn(&mesh_proto::NodeId) -> bool,
) -> Vec<(mesh_proto::NodeId, mesh_proto::Registration)> {
    let mut out: Vec<(mesh_proto::NodeId, mesh_proto::Registration)> = standing(state)
        .filter(|(n, r)| !dead(&n.node) && r.service == service && r.health.resolvable())
        .map(|(n, r)| (n.node, r.clone()))
        .collect();
    out.sort_by(|(_, a), (_, b)| (a.instance.0, &a.label).cmp(&(b.instance.0, &b.label)));
    out
}

/// The address `asker` should use to reach an instance that `node` runs.
///
/// **Not simply the address the instance published.** A node listening on
/// every interface — every real node, `0.0.0.0:7777` — has no one address of
/// its own to put there, and published `127.0.0.1`: right on that machine and
/// on no other. `web.mesh` answered `127.0.0.1` on every machine in the mesh,
/// so a machine running no copy was sent to itself, and every surface that
/// showed an instance's address showed loopback.
///
/// In order: an address the executor was configured to advertise; loopback
/// when the instance runs on the asking machine; otherwise an address the
/// running node announces to the mesh, IPv4 first, never loopback.
pub fn reachable_address(
    state: &MeshState,
    asker: mesh_proto::NodeId,
    node: mesh_proto::NodeId,
    reg: &mesh_proto::Registration,
) -> String {
    let published = reg.address.parse::<std::net::IpAddr>().ok();
    if published.is_some_and(|ip| !ip.is_loopback() && !ip.is_unspecified()) {
        return reg.address.clone();
    }
    if node == asker {
        return "127.0.0.1".to_string();
    }
    let announced: Vec<std::net::IpAddr> = state
        .members
        .get(&node)
        .map(|m| {
            m.info
                .addrs
                .iter()
                .filter_map(|a| a.parse::<std::net::SocketAddr>().ok())
                .map(|a| a.ip())
                .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
                .collect()
        })
        .unwrap_or_default();
    announced
        .iter()
        .find(|ip| ip.is_ipv4())
        .or_else(|| announced.first())
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| reg.address.clone())
}

/// Rows for `meshnode registry`.
///
/// `healthy` counts what discovery would hand out, so it applies the same
/// liveness rule [`resolvable`] does; `total` is still every registration,
/// so a service whose instances sit on a dead node reads `0 of 2` rather
/// than disappearing from the list.
pub fn rows(
    state: &MeshState,
    dead: impl Fn(&mesh_proto::NodeId) -> bool,
) -> Vec<mesh_proto::RegistryRow> {
    catalogue(state)
        .into_iter()
        .map(|(service, regs)| {
            let mut tags: Vec<String> = regs.iter().flat_map(|r| r.tags.clone()).collect();
            tags.sort();
            tags.dedup();
            let mut nodes: Vec<mesh_proto::NodeId> = Vec::new();
            let mut healthy = 0u32;
            for (node, r) in standing(state).filter(|(_, r)| r.service == service) {
                if !dead(&node.node) && r.health.resolvable() {
                    healthy += 1;
                }
                nodes.push(node.node);
            }
            nodes.sort();
            nodes.dedup();
            mesh_proto::RegistryRow {
                healthy,
                total: regs.len() as u32,
                nodes: nodes.len() as u32,
                service,
                tags,
            }
        })
        .collect()
}

/// Rows for `meshnode registry show <service>`.
///
/// An instance on a Dead node keeps its row — the registration is real and
/// somebody debugging needs to see it — but reads `unreachable` and not
/// resolvable, rather than the `passing` its node last said before it went.
pub fn instance_rows(
    state: &MeshState,
    service: &str,
    asker: mesh_proto::NodeId,
    dead: impl Fn(&mesh_proto::NodeId) -> bool,
) -> Vec<mesh_proto::RegistryInstanceRow> {
    let mut out = Vec::new();
    for (node, reg) in standing(state).filter(|(_, r)| r.service == service) {
        let name = state
            .members
            .get(&node.node)
            .map(|m| m.info.name.clone())
            .unwrap_or_else(|| node.node.short());
        let gone = dead(&node.node);
        out.push(mesh_proto::RegistryInstanceRow {
            instance: reg.instance,
            node: name,
            address: reachable_address(state, asker, node.node, reg),
            port: reg.port,
            label: reg.label.clone(),
            health: if gone {
                UNREACHABLE.to_string()
            } else {
                reg.health.as_str().to_string()
            },
            since_micros: reg.since_micros,
            tags: reg.tags.clone(),
            deployment: reg.deployment.clone(),
            resolvable: !gone && reg.health.resolvable(),
        });
    }
    out.sort_by_key(|a| (a.node.clone(), a.instance.0));
    out
}

/// Live members that will not publish a registration, with the minor they
/// speak (N3 P1, cross-cutting landmine 8).
///
/// The failure this names is the quiet one: a rolling upgrade through
/// [`mesh_proto::REGISTRY_MINOR`] makes a service's instance count appear
/// to shrink in DNS while nothing is actually down.
pub fn readiness(
    state: &MeshState,
    self_id: mesh_proto::NodeId,
    minor_of: impl Fn(&mesh_proto::NodeId) -> Option<u16>,
) -> Vec<(mesh_proto::NodeId, String, Option<u16>)> {
    let mut behind = Vec::new();
    for entry in state.members.values() {
        if entry.left || state.revoked.contains_key(&entry.info.node_id) {
            continue;
        }
        let id = entry.info.node_id;
        let minor = if id == self_id {
            Some(mesh_proto::PROTOCOL_MINOR)
        } else {
            minor_of(&id)
        };
        if !minor.is_some_and(|m| m >= mesh_proto::REGISTRY_MINOR) {
            behind.push((id, entry.info.name.clone(), minor));
        }
    }
    behind
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::{Health, JobId, NodeId, NodeRegistrations, Registration};

    fn reg(service: &str, inst: u8, health: Health) -> Registration {
        Registration {
            service: service.into(),
            instance: JobId([inst; 16]),
            label: "http".into(),
            address: format!("10.0.0.{inst}"),
            port: 20000 + u16::from(inst),
            tags: vec!["v1".into()],
            health,
            since_micros: 0,
            deployment: "web".into(),
        }
    }

    fn mesh() -> MeshState {
        let mut s = MeshState::default();
        s.registry.insert(
            NodeId([1; 32]),
            NodeRegistrations {
                node: NodeId([1; 32]),
                services: vec![
                    reg("webapp", 1, Health::Passing),
                    reg("api", 9, Health::Passing),
                ],
                published_at_micros: 1,
            },
        );
        s.registry.insert(
            NodeId([2; 32]),
            NodeRegistrations {
                node: NodeId([2; 32]),
                services: vec![
                    reg("webapp", 2, Health::Critical),
                    reg("webapp", 3, Health::Unknown),
                    reg("webapp", 4, Health::Starting),
                ],
                published_at_micros: 1,
            },
        );
        s
    }

    /// The catalogue is the union across nodes — which is what lets any
    /// node answer without asking anyone.
    #[test]
    fn the_catalogue_is_every_nodes_record_folded_together() {
        let c = catalogue(&mesh());
        assert_eq!(c.len(), 2);
        assert_eq!(c["webapp"].len(), 4);
        assert_eq!(c["api"].len(), 1);
    }

    /// **The whole point of the phase.** A critical instance is excluded
    /// from discovery, which Nomad cannot do because its check results
    /// never leave the client that measured them.
    #[test]
    fn discovery_hands_out_only_what_is_resolvable() {
        let s = mesh();
        let live = resolvable(&s, "webapp", |_| false);
        assert_eq!(
            live.len(),
            2,
            "passing and unknown, not critical or starting"
        );
        assert!(live.iter().all(|r| r.health.resolvable()));
        assert!(
            !live.iter().any(|r| r.health == Health::Critical),
            "a failing instance must never be handed out"
        );
        assert!(
            !live.iter().any(|r| r.health == Health::Starting),
            "nor one still binding its socket"
        );
    }

    /// Two nodes asked the same question answer in the same order, so a
    /// client taking the first address is not load-balancing on map
    /// iteration order without knowing it.
    #[test]
    fn resolution_order_is_deterministic() {
        let s = mesh();
        assert_eq!(
            resolvable(&s, "webapp", |_| false),
            resolvable(&s, "webapp", |_| false)
        );
        let ids: Vec<[u8; 16]> = resolvable(&s, "webapp", |_| false)
            .iter()
            .map(|r| r.instance.0)
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn a_row_counts_healthy_against_total() {
        let rows = rows(&mesh(), |_| false);
        let web = rows.iter().find(|r| r.service == "webapp").expect("webapp");
        assert_eq!(web.total, 4);
        assert_eq!(web.healthy, 2);
        assert_eq!(web.nodes, 2);
        assert_eq!(web.tags, vec!["v1".to_string()]);
    }

    #[test]
    fn an_unknown_service_resolves_to_nothing_rather_than_erroring() {
        assert!(resolvable(&mesh(), "nope", |_| false).is_empty());
    }

    /// **Phase A, §2.3.** A node that dies with its instances passing can
    /// never publish again, so without this its last word would resolve for
    /// as long as the mesh lives.
    #[test]
    fn a_dead_nodes_instances_do_not_resolve() {
        let s = mesh();
        let node_one_is_dead = |id: &NodeId| *id == NodeId([1; 32]);
        let live = resolvable(&s, "webapp", node_one_is_dead);
        assert_eq!(live.len(), 1, "only node 2's unchecked instance is left");
        assert_eq!(live[0].instance, JobId([3; 16]));
        assert!(resolvable(&s, "api", node_one_is_dead).is_empty());
        // The name still exists — NOERROR with no answers rather than
        // NXDOMAIN, which a resolver would cache as "never registered".
        assert!(catalogue(&s).contains_key("api"));
    }

    /// Counted out of `healthy` and kept in `total`, and shown by name as
    /// unreachable rather than as the `passing` its node last published.
    #[test]
    fn a_dead_nodes_instances_are_shown_as_unreachable() {
        let s = mesh();
        let node_one_is_dead = |id: &NodeId| *id == NodeId([1; 32]);
        let api = rows(&s, node_one_is_dead)
            .into_iter()
            .find(|r| r.service == "api")
            .expect("api row");
        assert_eq!((api.healthy, api.total), (0, 1));
        let shown = instance_rows(&s, "api", NodeId([9; 32]), node_one_is_dead);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].health, UNREACHABLE);
        assert!(!shown[0].resolvable);
    }

    fn job(id: JobId, state: mesh_proto::JobState) -> mesh_proto::JobSummary {
        mesh_proto::JobSummary {
            job_id: id,
            cmd_preview: String::new(),
            runtime: "container".into(),
            submitter: NodeId([1; 32]),
            submitter_user: mesh_proto::UserId([7; 32]),
            executor: NodeId([1; 32]),
            state,
            exit_code: None,
            created_at_micros: 0,
            started_at_micros: None,
            finished_at_micros: None,
            fanout: None,
            updated_hlc: Default::default(),
        }
    }

    /// **As found on a real mesh (2026-09-14):** a failed copy of `web`
    /// still listed as passing, because its node's last record named it and
    /// nothing had replaced that record. A registration whose job has ended
    /// is gone from every view; one whose job is running stays.
    #[test]
    fn a_registration_whose_job_has_ended_is_not_listed() {
        let mut s = mesh();
        let ended = JobId([1; 16]);
        s.jobs
            .insert(ended, job(ended, mesh_proto::JobState::Failed));
        assert_eq!(
            catalogue(&s)["webapp"].len(),
            3,
            "the failed copy left the catalogue"
        );
        assert!(
            resolvable(&s, "webapp", |_| false)
                .iter()
                .all(|r| r.instance != ended)
        );
        assert!(
            instance_rows(&s, "webapp", NodeId([9; 32]), |_| false)
                .iter()
                .all(|r| r.instance != ended)
        );
        let web = rows(&s, |_| false)
            .into_iter()
            .find(|r| r.service == "webapp")
            .expect("webapp");
        assert_eq!((web.total, web.nodes), (3, 1));

        let running = JobId([2; 16]);
        s.jobs
            .insert(running, job(running, mesh_proto::JobState::Running));
        assert_eq!(
            catalogue(&s)["webapp"].len(),
            3,
            "a running copy stays listed"
        );
    }

    /// A node that has left or been revoked never publishes again, so
    /// nothing it last said may stand — unlike a Dead member, which may come
    /// back and is shown as unreachable.
    #[test]
    fn a_node_that_left_or_was_revoked_lists_nothing() {
        let mut s = with_members(mesh());
        s.members.get_mut(&NodeId([1; 32])).expect("n1").left = true;
        assert!(!catalogue(&s).contains_key("api"));
        assert_eq!(catalogue(&s)["webapp"].len(), 3);

        let mut s = with_members(mesh());
        s.revoked
            .insert(NodeId([2; 32]), ("gone".into(), Default::default()));
        assert_eq!(catalogue(&s)["webapp"].len(), 1);
        assert!(rows(&s, |_| false).iter().all(|r| r.nodes == 1));
    }

    fn with_members(mut s: MeshState) -> MeshState {
        let hosts: [(u8, &[&str]); 2] = [
            (1, &["192.168.20.201:7777", "127.0.0.1:7777"]),
            (2, &["[fd00::2]:7777", "10.9.0.2:7777"]),
        ];
        for (n, addrs) in hosts {
            let info = mesh_proto::MemberInfo {
                node_id: NodeId([n; 32]),
                name: format!("n{n}"),
                addrs: addrs.iter().map(|a| a.to_string()).collect(),
                tags: vec![],
                slots: 4,
                joined_at_micros: 1,
            };
            s.members.insert(
                NodeId([n; 32]),
                crate::state::MemberEntry {
                    announced_tags: vec![],
                    info,
                    added_hlc: Default::default(),
                    updated_hlc: Default::default(),
                    left: false,
                    left_hlc: None,
                },
            );
        }
        s
    }

    /// **The bug, as it was found on a real mesh.** Every node listens on
    /// every interface and published `127.0.0.1`, so `web.mesh` sent every
    /// machine to itself. Another machine gets the running node's own
    /// address; only the machine running the instance gets loopback.
    #[test]
    fn an_instance_is_reached_at_its_nodes_address_not_loopback() {
        let s = with_members(mesh());
        let (one, two, three) = (NodeId([1; 32]), NodeId([2; 32]), NodeId([3; 32]));
        let mut r = reg("web", 1, Health::Passing);
        r.address = "127.0.0.1".into();
        assert_eq!(reachable_address(&s, three, one, &r), "192.168.20.201");
        assert_eq!(reachable_address(&s, one, one, &r), "127.0.0.1");
        assert_eq!(
            reachable_address(&s, three, two, &r),
            "10.9.0.2",
            "IPv4 first"
        );
        r.address = "203.0.113.7".into();
        assert_eq!(
            reachable_address(&s, three, one, &r),
            "203.0.113.7",
            "an address the executor was told to advertise wins"
        );

        let found = resolvable_on(&s, "webapp", |_| false);
        let nodes: Vec<(u8, u8)> = found
            .iter()
            .map(|(n, r)| (n.0[0], r.instance.0[0]))
            .collect();
        assert_eq!(
            nodes,
            vec![(1, 1), (2, 3)],
            "each instance with the node that runs it"
        );
        let rows = instance_rows(&s, "webapp", three, |_| false);
        assert!(
            rows.iter().all(|r| r.address != "127.0.0.1"),
            "no row shows loopback to another machine: {rows:?}"
        );
    }

    #[test]
    fn a_service_name_must_be_a_dns_label() {
        assert!(is_valid_service_name("webapp"));
        assert!(is_valid_service_name("web-app-2"));
        assert!(!is_valid_service_name(""));
        assert!(!is_valid_service_name("-lead"));
        assert!(!is_valid_service_name("trail-"));
        assert!(!is_valid_service_name("has space"));
        assert!(!is_valid_service_name("under_score"));
        assert!(!is_valid_service_name("dotted.name"));
        assert!(!is_valid_service_name(&"x".repeat(64)));
        assert!(!is_valid_service_name("8"), "ambiguous with an address");
    }
}
