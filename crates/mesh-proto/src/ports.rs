//! Ports as a schedulable resource (N2 §8).
//!
//! ## Why a port is modelled on a device and not on memory
//!
//! Memory is *divisible and approximate*: a node has 32 GB, a job wants 2,
//! and the scheduler subtracts. A port is neither. One holder, exactly, and
//! the wrong answer is not "slow" but `EADDRINUSE` — so the ledger this is
//! modelled on is the device ledger ([`crate::DeviceKey`] and
//! `mesh_core::gangs::Leases`), which already gets exclusivity, keyed
//! holds, and release-by-table-edit right.
//!
//! **Release by table edit is the load-bearing half.** A port freed by a
//! driver call would stay held while a `SIGKILL`ed job's executor noticed,
//! reaped, and called something; a port freed by dropping a row is reusable
//! the instant the executor sees the job is gone. Ports are the resource
//! where that gap is most visible, because the next instance of the same
//! service wants the *same* number.
//!
//! ## Why the request rides the environment
//!
//! `MESHWARE_TASK_PORTS` is a reserved job-environment key, exactly like
//! `MESHWARE_TASK_CPUS` — so a port request needs **no wire change at
//! all**, reaches an executor by the path every other resource requirement
//! already takes, and is available to `run`, to a service definition and to
//! a deployment's `[network]` block without three encodings of one idea.
//!
//! The allocation comes back the same way: `MESHWARE_PORT_<label>`,
//! `MESHWARE_ADDR_<label>` and `MESHWARE_IP_<label>` are written into the
//! job's environment at spawn, which is the only place that can know them —
//! a dynamic port is not decided until the executor takes the work.

use serde::{Deserialize, Serialize};

/// The port request, as a reserved job-environment key.
///
/// `http:9095,metrics:0,admin:8080!` — label, a colon, the port *inside*
/// the job, and a trailing `!` for "this exact host port, not a dynamic
/// one". `0` inside means the job does not care and reads
/// `MESHWARE_PORT_<label>`.
pub const REQ_PORTS: &str = "MESHWARE_TASK_PORTS";

/// Prefix for the allocated host port of a label: `MESHWARE_PORT_HTTP`.
pub const PORT_ENV_PREFIX: &str = "MESHWARE_PORT_";
/// Prefix for `MESHWARE_IP_<label>:MESHWARE_PORT_<label>`, ready to bind:
/// `MESHWARE_ADDR_HTTP`.
pub const ADDR_ENV_PREFIX: &str = "MESHWARE_ADDR_";
/// Prefix for the address a label's port should be bound on:
/// `MESHWARE_IP_HTTP`. The node's listen address — `0.0.0.0` on a node that
/// listens on every interface — so a job that binds it is reachable at its
/// node's address, which is where clients and load balancers find it.
/// Nomad's `NOMAD_IP_<label>`.
pub const IP_ENV_PREFIX: &str = "MESHWARE_IP_";

/// The advertised dynamic range, as a reserved capacity tag:
/// `cap.ports=20000-32000`.
pub const CAP_PORTS: &str = "cap.ports";

/// Which port label a deployment's health check should probe (N6).
///
/// Reserved key, carried on `DeploymentPolicy::instance_env` rather than
/// on the `HealthCheck` record — that one lives inside the **signed**
/// `Deployment`, and postcard encodes positionally, so appending a field
/// would invalidate every signature ever made over one and make an older
/// node misdecode every deployment from here on. The policy side-car is
/// the extension point built for exactly this, and `instance_env` is its
/// carrier.
///
/// Empty, or absent, means the check names a literal port.
pub const REQ_HEALTH_PORT: &str = "MESHWARE_TASK_HEALTH_PORT";

/// Nomad's range, and for Nomad's reason: high enough to sit above every
/// registered service port, wide enough that exhaustion is not a thing an
/// operator has to think about, and bounded so a node cannot be talked into
/// binding something privileged.
pub const DEFAULT_DYNAMIC_MIN: u16 = 20000;
pub const DEFAULT_DYNAMIC_MAX: u16 = 32000;

/// How many labelled ports one job may ask for.
///
/// Bounded because the request arrives as a string on a job environment
/// that a submitter controls, and every label costs an exclusive row in a
/// node-local table plus three environment entries. Sixteen is past any
/// real service and small enough that a malformed request cannot be a
/// denial of service.
pub const MAX_PORTS_PER_JOB: usize = 16;

/// One port a job is asking for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRequest {
    /// `http`, `metrics`. Becomes `MESHWARE_PORT_HTTP` and friends, so it
    /// is constrained to what makes a legal environment key.
    pub label: String,
    /// The port **inside** the job — what its config file says it listens
    /// on. `0` means the job takes whatever it is given and reads
    /// `MESHWARE_PORT_<label>` to find out.
    ///
    /// In host mode this is advisory: there is no translation, so a job
    /// that declares `to = 9095` and is given host port 21000 must actually
    /// read the environment. It is carried because it is the operator's
    /// statement of intent and N3's registry wants it, and because bridge
    /// mode — if it ever exists — is where it becomes load-bearing.
    pub to: u16,
    /// A fixed host port, demanded rather than allocated.
    ///
    /// The dangerous kind, and the reason [`Self::is_static`] exists as its
    /// own question: a static port cannot be held twice on one node, so a
    /// deployment asking for one is implicitly asking for at most one
    /// instance per node — which N2 refuses at submit time rather than
    /// discovering as a crash loop.
    pub fixed: Option<u16>,
}

impl PortRequest {
    pub fn is_static(&self) -> bool {
        self.fixed.is_some()
    }

    /// The environment-key suffix for this label — see [`label_key_suffix`].
    pub fn key_suffix(&self) -> String {
        label_key_suffix(&self.label)
    }
}

/// A parsed port request set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRequests(pub Vec<PortRequest>);

impl PortRequests {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Does any label demand a fixed host port?
    pub fn any_static(&self) -> bool {
        self.0.iter().any(PortRequest::is_static)
    }

    /// Recover the request from a job's environment.
    ///
    /// **Unparseable entries are dropped, never guessed.** The same rule
    /// [`crate::Requirements::from_env`] follows: a bad number must not
    /// turn into an accidental demand, and here it must not turn into an
    /// accidental *binding* either.
    pub fn from_env(env: &[(String, String)]) -> PortRequests {
        env.iter()
            .find(|(k, _)| k == REQ_PORTS)
            .map(|(_, v)| Self::parse(v))
            .unwrap_or_default()
    }

    /// `http:9095,metrics:0,admin:8080!`
    pub fn parse(text: &str) -> PortRequests {
        let mut out = Vec::new();
        for part in text.split(',') {
            let part = part.trim();
            if part.is_empty() || out.len() >= MAX_PORTS_PER_JOB {
                continue;
            }
            let Some((label, rest)) = part.split_once(':') else {
                continue;
            };
            let label = label.trim();
            if !is_valid_label(label) {
                continue;
            }
            let rest = rest.trim();
            let (num, fixed) = match rest.strip_suffix('!') {
                Some(n) => (n.trim(), true),
                None => (rest, false),
            };
            let Ok(port) = num.parse::<u16>() else {
                continue;
            };
            // A fixed port of zero is a contradiction: "bind exactly the
            // port I am not naming". Dropped rather than silently made
            // dynamic, because the two mean opposite things about whether a
            // second instance may share the node.
            if fixed && port == 0 {
                continue;
            }
            if out.iter().any(|p: &PortRequest| p.label == label) {
                continue;
            }
            out.push(PortRequest {
                label: label.to_string(),
                to: port,
                fixed: fixed.then_some(port),
            });
        }
        PortRequests(out)
    }

    /// Render back to the environment value, so a request round-trips.
    pub fn to_env_value(&self) -> String {
        self.0
            .iter()
            .map(|p| match p.fixed {
                Some(f) => format!("{}:{f}!", p.label),
                None => format!("{}:{}", p.label, p.to),
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The reserved environment entry, or nothing when no port is wanted.
    pub fn to_env(&self) -> Vec<(String, String)> {
        if self.0.is_empty() {
            return Vec::new();
        }
        vec![(REQ_PORTS.to_string(), self.to_env_value())]
    }
}

/// The environment-key suffix for a port label: uppercase, with anything
/// that is not alphanumeric folded to `_` — `admin-api` → `ADMIN_API`.
///
/// **One function for every writer and reader of the key.** The executor
/// used to upper-case the label and nothing more, so a job asking for
/// `admin-api` was told `MESHWARE_PORT_ADMIN-API` — a name no shell can
/// read — while the container runner and the service registry looked for
/// `MESHWARE_PORT_ADMIN_API`, found nothing, and published no port.
pub fn label_key_suffix(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// A label must make a legal environment-key suffix and be short enough to
/// read in a table.
pub fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 32
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && label
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

/// One allocated port, as the executor decided it.
///
/// Travels back on the job record so `deployment status`, the registry (N3)
/// and a person can all see where a thing actually is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortAllocation {
    pub label: String,
    /// The host port actually bound.
    pub host: u16,
    /// The port inside the job, as declared. Equal to `host` in host mode.
    pub to: u16,
}

/// A node's advertised dynamic range and what it will not give out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRange {
    pub min: u16,
    pub max: u16,
}

impl Default for PortRange {
    fn default() -> Self {
        PortRange {
            min: DEFAULT_DYNAMIC_MIN,
            max: DEFAULT_DYNAMIC_MAX,
        }
    }
}

impl PortRange {
    pub fn contains(&self, port: u16) -> bool {
        port >= self.min && port <= self.max
    }

    pub fn count(&self) -> u32 {
        if self.max < self.min {
            return 0;
        }
        u32::from(self.max - self.min) + 1
    }

    /// `20000-32000`, the capacity tag's value.
    pub fn as_tag_value(&self) -> String {
        format!("{}-{}", self.min, self.max)
    }

    /// Parse `20000-32000`. `None` for anything malformed or inverted —
    /// fail closed, so a typo advertises no range rather than a wrong one.
    pub fn parse(text: &str) -> Option<PortRange> {
        let (a, b) = text.trim().split_once('-')?;
        let min: u16 = a.trim().parse().ok()?;
        let max: u16 = b.trim().parse().ok()?;
        (min > 0 && max >= min).then_some(PortRange { min, max })
    }

    /// The range a node advertises, read off its tags. `None` when it
    /// advertises none — which for a port request is **disqualifying**,
    /// not neutral.
    ///
    /// This is the device rule, not the memory rule, and the inversion is
    /// deliberate for the same reason [`crate::DeviceRequest`] inverts it:
    /// an unmeasured dimension is neutral, but an *unadvertised* one is a
    /// statement. A node running this build always advertises a range, so
    /// an absent tag means a node too old to allocate a port — and treating
    /// that as neutral would place a web server on it and have the bind
    /// fail with nothing to say why.
    pub fn from_tags<S: AsRef<str>>(tags: &[S]) -> Option<PortRange> {
        tags.iter().find_map(|t| {
            t.as_ref()
                .strip_prefix(CAP_PORTS)
                .and_then(|rest| rest.strip_prefix('='))
                .and_then(PortRange::parse)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_through_the_environment() {
        let reqs = PortRequests::parse("http:9095,metrics:0,admin:8080!");
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs.0[0].label, "http");
        assert_eq!(reqs.0[0].to, 9095);
        assert!(!reqs.0[0].is_static());
        assert_eq!(reqs.0[1].to, 0);
        assert_eq!(reqs.0[2].fixed, Some(8080));
        assert!(reqs.any_static());

        let env = reqs.to_env();
        assert_eq!(PortRequests::from_env(&env), reqs);
    }

    /// A malformed entry is dropped, never guessed — the rule
    /// `Requirements::from_env` already follows, and here the stakes are
    /// higher: a guess would bind something.
    #[test]
    fn junk_is_dropped_rather_than_guessed() {
        // A bad port number, an empty label, a label with no colon, and a
        // label that is not a legal environment-key suffix.
        let reqs = PortRequests::parse("http:notanumber,:9095,ok:1234,nocolon,has space:1");
        assert_eq!(reqs.len(), 1, "{:?}", reqs.0);
        assert_eq!(reqs.0[0].label, "ok");
        assert_eq!(reqs.0[0].to, 1234);
    }

    /// Two *valid* entries for one label is the ambiguous case, and the
    /// first wins — a later one silently replacing it would mean the port
    /// a job binds depends on how far down a string somebody looked.
    ///
    /// A junk entry is not one of the two: it was already dropped, so a
    /// valid entry after it is the only claim on that label, not a
    /// duplicate of a claim that never existed.
    #[test]
    fn a_repeated_label_keeps_the_first_valid_claim() {
        let reqs = PortRequests::parse("http:1111,http:2222");
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs.0[0].to, 1111);

        let after_junk = PortRequests::parse("http:notanumber,http:2222");
        assert_eq!(after_junk.len(), 1);
        assert_eq!(
            after_junk.0[0].to, 2222,
            "the junk entry claimed nothing, so this is not a duplicate"
        );
    }

    /// "Bind exactly the port I am not naming" is a contradiction, and the
    /// two readings mean opposite things about whether a second instance
    /// may share the node — so it is dropped rather than made dynamic.
    #[test]
    fn a_fixed_port_of_zero_is_refused_rather_than_made_dynamic() {
        assert!(PortRequests::parse("http:0!").is_empty());
    }

    /// A submitter controls this string. Sixteen labels is past any real
    /// service; a thousand would be an exclusive row each.
    #[test]
    fn the_request_is_bounded() {
        let many = (0..100)
            .map(|i| format!("p{i}:0"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(PortRequests::parse(&many).len(), MAX_PORTS_PER_JOB);
    }

    #[test]
    fn a_label_becomes_a_legal_environment_key() {
        let p = PortRequest {
            label: "http-admin".into(),
            to: 1,
            fixed: None,
        };
        assert_eq!(p.key_suffix(), "HTTP_ADMIN");
        assert!(is_valid_label("http"));
        assert!(is_valid_label("http-2"));
        assert!(!is_valid_label(""));
        assert!(!is_valid_label("-leading"));
        assert!(!is_valid_label("has space"));
        assert!(!is_valid_label(&"x".repeat(33)));
    }

    #[test]
    fn a_range_round_trips_and_fails_closed() {
        let r = PortRange::default();
        assert_eq!(r.as_tag_value(), "20000-32000");
        assert_eq!(PortRange::parse(&r.as_tag_value()), Some(r.clone()));
        assert_eq!(r.count(), 12001);
        assert!(r.contains(20000) && r.contains(32000) && !r.contains(19999));
        // Inverted, zero and malformed all advertise nothing.
        assert!(PortRange::parse("32000-20000").is_none());
        assert!(PortRange::parse("0-100").is_none());
        assert!(PortRange::parse("nonsense").is_none());
        assert!(PortRange::parse("20000").is_none());
    }

    /// An unadvertised range disqualifies, unlike an unmeasured dimension.
    #[test]
    fn a_node_with_no_range_tag_advertises_none() {
        assert!(PortRange::from_tags::<String>(&[]).is_none());
        assert_eq!(
            PortRange::from_tags(&["cap.cores=8".to_string(), "cap.ports=100-200".to_string()]),
            Some(PortRange { min: 100, max: 200 })
        );
    }
}

/// One node's port ledger, as `meshnode ports` shows it (N2 P1).
///
/// **Node-local truth, and complete.** A dynamic port is decided on the
/// executor at spawn and `JobSummary` is an op body that cannot grow to
/// carry it back, so until N3's service registry replicates it this is the
/// only place the actual number lives. Asked of one node and answered by
/// that node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortLedgerView {
    /// The node answering, by name.
    pub node: String,
    /// The dynamic range it advertises, or `None` when it advertises none
    /// — which means it takes no work that wants a port.
    pub range: Option<PortRange>,
    /// Ports the operator withheld, whatever the range says.
    pub reserved: Vec<u16>,
    /// What is held right now, lowest first.
    pub held: Vec<PortHeldView>,
    /// How many dynamic ports remain. `0` with a `None` range means
    /// "advertises none", not "exhausted" — `range` is what tells them
    /// apart.
    pub free: u32,
}

/// One held port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortHeldView {
    pub port: u16,
    /// The label its holder knows it by.
    pub label: String,
    /// The job or gang holding it, short form.
    pub holder: String,
    /// `job` or `gang` — a gang hold expires on its own clock, a job hold
    /// ends when the job does, and an operator looking at a stuck port
    /// needs to know which kind of wait they are in.
    pub kind: String,
}
