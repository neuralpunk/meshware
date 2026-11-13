//! The service registry: where a running thing actually is, and whether it
//! is healthy (N3 §9).
//!
//! ## The one thing Nomad got wrong, and this exists to get right
//!
//! Nomad's native catalogue has **no health field at all**. A maintainer,
//! on an issue open since January 2023: *"this is a known and designed
//! current limitation as the health check state is only stored on the Nomad
//! client that is performing the checking."* The downstream cost is total —
//! no health-filtered discovery, and Traefik's Nomad provider registers
//! every service as healthy immediately, with a TODO in the code where the
//! check should be.
//!
//! Meshware already replicates far more than a health bit. Putting check
//! **results** in the op log is a decision made when the record is
//! designed, not a feature added afterwards, and this is that decision.
//!
//! ## Results, never checks
//!
//! `facts.rs` learned this first and states it as *"properties, not
//! measurements"*. What replicates is the **conclusion** — passing,
//! critical, and when it last changed — never the probe, never a latency
//! series, never a per-check log. A flapping service must not author one op
//! per flap, so [`NodeRegistrations`] is republished on **state
//! transitions** and set membership, and never on a tick that found nothing
//! new.
//!
//! ## One record per node, not one per instance
//!
//! The third application of a shape this codebase has already proved twice
//! — `NodeReservations` and `NodeInstanceHealth`. The write rate is bounded
//! by *node count and instance churn* rather than by instance count, it
//! compacts to one live op per node, and an instance that ends drops out of
//! the vec on the next publish, so the record is self-trimming rather than
//! grow-only.

use serde::{Deserialize, Serialize};

use crate::{JobId, NodeId};

/// The DNS suffix a registered service answers on: `webapp.mesh`.
///
/// Not a configurable: a mesh whose suffix differs per node is a mesh where
/// `curl webapp.mesh` means different things depending on where you typed
/// it, which is the failure the whole feature exists to prevent. `.mesh` is
/// not delegated by IANA and cannot collide with a real zone.
pub const DNS_SUFFIX: &str = "mesh";

/// The default DNS port.
///
/// **Not 53, and that is deliberate.** Binding 53 needs root or
/// `CAP_NET_BIND_SERVICE`, and this product's whole posture is that a node
/// does not need root. 8600 is Consul's DNS port — a precedent an operator
/// will recognise, and unmistakably "the DNS port" in a `ss -lnp`.
pub const DEFAULT_DNS_PORT: u16 = 8600;

/// Job-environment key naming the service a job registers itself as.
///
/// Reserved, and absent by default: a job registers only when something
/// asked it to. `[deployment.service]` sets it, `meshnode run --register`
/// sets it, and nothing sets it implicitly — a mesh where every ad-hoc
/// command appeared in DNS would be a mesh where DNS means nothing.
pub const REQ_SERVICE: &str = "MESHWARE_TASK_SERVICE";
/// Comma-separated tags for the registration: `MESHWARE_TASK_SERVICE_TAGS`.
pub const REQ_SERVICE_TAGS: &str = "MESHWARE_TASK_SERVICE_TAGS";
/// Which port label the registration publishes, when a job has several.
pub const REQ_SERVICE_PORT: &str = "MESHWARE_TASK_SERVICE_PORT";

/// How long a stopping instance stays deregistered before it is signalled,
/// in seconds: `MESHWARE_TASK_SHUTDOWN_DELAY` (N3 P2).
pub const REQ_SHUTDOWN_DELAY: &str = "MESHWARE_TASK_SHUTDOWN_DELAY";
/// How long a signalled instance has to exit before it is force-killed.
pub const REQ_KILL_TIMEOUT: &str = "MESHWARE_TASK_KILL_TIMEOUT";

/// The ceiling on `shutdown_delay`, in seconds.
///
/// A drain window is seconds, not hours: the point is to let a load
/// balancer notice the deregistration before the process dies. An
/// unbounded value would let one stopping instance hold a rolling update
/// open indefinitely, which is a deadlock wearing a configuration's
/// clothes.
pub const MAX_SHUTDOWN_DELAY_SECS: u64 = 300;

/// What a check concluded. **A conclusion, never a measurement.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Health {
    /// No check is defined, or none has reported yet. Resolvable, because
    /// "nobody is looking" is not "it is broken" — the same rule the
    /// scheduler applies to an unmeasured dimension.
    Unknown,
    /// Starting: within its grace period, not yet judged.
    Starting,
    /// Passing.
    Passing,
    /// Failing. **Excluded from discovery**, which is the entire point of
    /// the field being here rather than on the node that measured it.
    Critical,
}

impl Health {
    /// Should discovery hand this out?
    ///
    /// `Unknown` resolves and `Starting` does not, and the asymmetry is
    /// deliberate. An unchecked service has nobody to say it is broken, so
    /// refusing to resolve it would make declaring a health check a
    /// prerequisite for being findable. A *starting* one has somebody
    /// looking who has not finished — and handing out an address that is
    /// still binding its socket is how a rolling update drops requests.
    pub fn resolvable(&self) -> bool {
        matches!(self, Health::Unknown | Health::Passing)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Unknown => "unknown",
            Health::Starting => "starting",
            Health::Passing => "passing",
            Health::Critical => "critical",
        }
    }
}

/// One registered instance of one service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// The name clients resolve: `webapp` answers at `webapp.mesh`.
    pub service: String,
    /// The job behind it. The registration's identity, with `label`.
    pub instance: JobId,
    /// Which labelled port this publishes, for a job with several.
    pub label: String,
    /// Where to reach it. An address, not a hostname — the node knows its
    /// own reachable address and a second name to resolve would be a
    /// second thing to be wrong.
    pub address: String,
    pub port: u16,
    /// Free-form, for filtering: `canary`, `v2`.
    pub tags: Vec<String>,
    pub health: Health,
    /// When this health state was entered — how "critical for 30s" is
    /// answered without a second timestamp stream, the same field
    /// `InstanceHealth` carries for the same reason.
    pub since_micros: u64,
    /// The deployment this instance belongs to, empty for an ad-hoc job.
    pub deployment: String,
}

impl Registration {
    /// The fully-qualified name this answers to: `webapp.mesh`.
    pub fn fqdn(&self) -> String {
        format!("{}.{DNS_SUFFIX}", self.service)
    }
}

/// Every service instance one node currently runs, with its health.
///
/// **One record per node, LWW, republished only when the set or a health
/// conclusion changes** — the shape [`crate::NodeReservations`] and
/// [`crate::NodeInstanceHealth`] already have, and for the same reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRegistrations {
    pub node: NodeId,
    pub services: Vec<Registration>,
    /// When the node last recomputed this. Guards the LWW merge and lets a
    /// reader discount a node that has gone silent.
    pub published_at_micros: u64,
}

/// One row of `meshnode registry` — a service name, and what is behind it.
///
/// Deliberately **not** `ServiceRow`, which is already the Library's word
/// for a published service *definition*. These are different nouns that
/// would look identical in an import list: one is "what this mesh can run",
/// the other is "what is running, and where". Letting them collide is how a
/// reader ends up sure they understand a screen they have misread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryRow {
    pub service: String,
    /// Instances that discovery will hand out.
    pub healthy: u32,
    /// Instances registered at all.
    pub total: u32,
    /// Distinct nodes it runs on.
    pub nodes: u32,
    /// Every tag any instance carries, deduplicated.
    pub tags: Vec<String>,
}

/// One instance, as `meshnode registry show` lists it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryInstanceRow {
    pub instance: JobId,
    pub node: String,
    pub address: String,
    pub port: u16,
    pub label: String,
    pub health: String,
    pub since_micros: u64,
    pub tags: Vec<String>,
    pub deployment: String,
    /// Would discovery hand this one out?
    pub resolvable: bool,
}

/// What the DNS responder is doing, for `doctor` and the Director panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsStatusView {
    pub enabled: bool,
    /// The address it is bound to, when it is.
    pub bound: Option<String>,
    pub port: u16,
    /// True when bound to 53, which needs a capability this product does
    /// not take by default.
    pub privileged: bool,
    /// The suffix it answers for.
    pub suffix: String,
    /// Names it can answer right now.
    pub names: u32,
    /// Why it is not running, when it is not.
    pub detail: String,
    /// Whether this machine's resolver sends `.mesh` to the responder. When
    /// it does not, `dig web.mesh` through the system resolver finds
    /// nothing however healthy the mesh is.
    pub resolver_routed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The asymmetry that makes health-filtered discovery usable rather
    /// than a second thing to configure.
    #[test]
    fn unknown_resolves_and_starting_does_not() {
        assert!(
            Health::Unknown.resolvable(),
            "an unchecked service is findable"
        );
        assert!(Health::Passing.resolvable());
        assert!(
            !Health::Starting.resolvable(),
            "still binding its socket — handing this out drops requests mid-roll"
        );
        assert!(
            !Health::Critical.resolvable(),
            "this is the whole point of the field being replicated"
        );
    }

    #[test]
    fn a_registration_names_itself_in_the_mesh_zone() {
        let r = Registration {
            service: "webapp".into(),
            instance: JobId([0; 16]),
            label: "http".into(),
            address: "10.0.0.4".into(),
            port: 20000,
            tags: vec![],
            health: Health::Passing,
            since_micros: 0,
            deployment: "web".into(),
        };
        assert_eq!(r.fqdn(), "webapp.mesh");
    }

    /// Ordered worst-last, so a "worst of" fold over an instance set is a
    /// `max` and cannot be written backwards.
    #[test]
    fn health_orders_from_unknown_to_critical() {
        assert!(Health::Unknown < Health::Passing);
        assert!(Health::Passing < Health::Critical);
        assert_eq!(
            [Health::Passing, Health::Critical, Health::Unknown]
                .into_iter()
                .max(),
            Some(Health::Critical)
        );
    }
}
