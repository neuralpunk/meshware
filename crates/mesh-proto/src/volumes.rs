//! Volumes, as leases (N5 §11).
//!
//! ## The rule this exists to get right
//!
//! **Never split claim state from mount state.** Every Nomad CSI failure is
//! the same shape: a claim counter in Raft that disagrees with the actual
//! kernel mount, with no reconciliation loop and only a `deregister
//! --force` ritual to break the tie. The canonical error — *"volume has
//! exhausted its available writer claims"* — spans 2020 to 2026 across at
//! least eight issues, and their own maintainer says the quiet part:
//!
//! > "we may never be able to fix all causes of desync between volumes and
//! > claims, because a node could potentially just up and disappear on us."
//!
//! A node **can** up and disappear. The mistake is not failing to prevent
//! that; it is holding the claim somewhere the disappearance cannot reach.
//!
//! ## So a claim is a lease, and it lives where the volume is
//!
//! A volume is on exactly one node. That node is the only one that can know
//! whether anything is actually using it, so **that node holds the claim**
//! — as a row in the same `Leases` table that already holds ports and
//! devices, released by the same table edit that releases them when a job
//! exits, is cancelled, times out or is `SIGKILL`ed.
//!
//! There is no counter to decrement, so there is nothing to get out of
//! step. A node that disappears takes its claims with it: its record in
//! replicated state goes stale and every claim it held is simply no longer
//! asserted by anybody. The reconciliation loop the CSI design needs does
//! not exist here because the thing it would reconcile does not exist.
//!
//! This is the gang lease's idiom (*"expiry needs no timer"*) applied to a
//! third resource, and the decision is made on the first commit precisely
//! so the bug class never opens.

use serde::{Deserialize, Serialize};

/// How a job wants a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// One writer at a time, and nobody else — the claim is exclusive.
    ReadWrite,
    /// Any number of readers together. A read-only claim never conflicts
    /// with another read-only claim, and always conflicts with a writer.
    ReadOnly,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::ReadWrite => "rw",
            Mode::ReadOnly => "ro",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "rw" | "read-write" | "read_write" => Some(Mode::ReadWrite),
            "ro" | "read-only" | "read_only" => Some(Mode::ReadOnly),
            _ => None,
        }
    }

    pub fn read_only(&self) -> bool {
        matches!(self, Mode::ReadOnly)
    }
}

/// The reserved capacity tag listing a node's volumes: `cap.volumes=data,cache`.
pub const CAP_VOLUMES: &str = "cap.volumes";

/// The `source` of a volume an operator declared by hand, as against the
/// name of the plugin that provisioned one (N5 P2).
///
/// A constant because it is load-bearing in two places that must agree:
/// the ledger shows it to a person, and `volume rm` uses it to refuse to
/// touch a `[[volume]]` block it did not create.
pub const SOURCE_STATIC: &str = "static";

/// The reserved job-environment key carrying a job's volume requests.
///
/// JSON, for the reason `MESHWARE_TASK_ARGS` is: a mount path may contain a
/// comma, and a separator-delimited encoding could not carry it.
pub const REQ_VOLUMES: &str = "MESHWARE_TASK_VOLUMES";

/// Prefix for the host path a granted volume landed on:
/// `MESHWARE_VOLUME_DATA`.
///
/// **This is how a non-container job uses a volume.** meshware does not put
/// a bash job in a mount namespace, so there is nothing to bind — the
/// honest thing is to tell the job where the directory actually is. A
/// container gets a real bind mount, because its runtime can do one.
pub const VOLUME_ENV_PREFIX: &str = "MESHWARE_VOLUME_";

/// How many volumes one job may ask for.
///
/// Bounded because the request arrives on an environment a submitter
/// controls and each one costs an exclusive row in a node-local table.
pub const MAX_VOLUMES_PER_JOB: usize = 8;

/// One volume a job is asking for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRequest {
    /// The volume's name, as the node declares it.
    pub name: String,
    /// Where to mount it **inside a container**. Ignored by process
    /// runtimes, which get the host path in `MESHWARE_VOLUME_<NAME>`
    /// instead — there is no mount namespace to put it in.
    pub path: String,
    pub mode: Mode,
}

/// A job's volume requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRequests(pub Vec<VolumeRequest>);

impl VolumeRequests {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The names asked for, for a placement filter.
    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|v| v.name.as_str()).collect()
    }

    pub fn from_env(env: &[(String, String)]) -> VolumeRequests {
        env.iter()
            .find(|(k, _)| k == REQ_VOLUMES)
            .and_then(|(_, v)| serde_json::from_str::<Vec<VolumeRequest>>(v).ok())
            .map(|mut v| {
                v.truncate(MAX_VOLUMES_PER_JOB);
                VolumeRequests(v)
            })
            .unwrap_or_default()
    }

    pub fn to_env(&self) -> Vec<(String, String)> {
        if self.0.is_empty() {
            return Vec::new();
        }
        vec![(
            REQ_VOLUMES.to_string(),
            serde_json::to_string(&self.0).unwrap_or_else(|_| "[]".into()),
        )]
    }
}

/// A volume this node offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    /// The directory on this node.
    pub path: String,
    /// Refuse write claims entirely, whatever a job asks for.
    pub read_only: bool,
    /// `static` for one declared in `meshnode.toml`, or the plugin that
    /// created it (N5 P2).
    pub source: String,
}

/// One volume, as `meshnode volumes` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeRow {
    pub name: String,
    pub node: String,
    pub path: String,
    pub read_only: bool,
    pub source: String,
    /// Who is holding it right now, short form.
    pub holders: Vec<String>,
    /// True when a writer holds it, so nothing else may write.
    pub write_held: bool,
    /// Could a writer take it right now?
    pub writable_now: bool,
}

/// Is this a name a volume may be declared under?
///
/// It becomes an environment-key suffix (`MESHWARE_VOLUME_<NAME>`) and a
/// selector value, so it is held to the same shape as a port label.
pub fn is_valid_volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

/// The environment-key suffix for a volume name.
pub fn key_suffix(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The volume names a node advertises, from its `cap.*` tags.
///
/// Absent means *this node offers none* — disqualifying for a job that
/// wants one, the device rule rather than the memory rule, and for the same
/// reason: every node running this build advertises its list, so a missing
/// tag is a statement rather than an absence of measurement.
pub fn from_tags<S: AsRef<str>>(tags: &[S]) -> Vec<String> {
    tags.iter()
        .find_map(|t| {
            t.as_ref()
                .strip_prefix(CAP_VOLUMES)
                .and_then(|rest| rest.strip_prefix('='))
        })
        .map(|list| {
            list.split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The capacity tag for a set of volume names, or nothing when there are
/// none.
pub fn cap_tag(names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| format!("{CAP_VOLUMES}={}", names.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_through_the_environment() {
        let reqs = VolumeRequests(vec![
            VolumeRequest {
                name: "data".into(),
                path: "/data".into(),
                mode: Mode::ReadWrite,
            },
            VolumeRequest {
                name: "cache".into(),
                path: "/var/cache".into(),
                mode: Mode::ReadOnly,
            },
        ]);
        assert_eq!(VolumeRequests::from_env(&reqs.to_env()), reqs);
        assert_eq!(reqs.names(), vec!["data", "cache"]);
    }

    /// A mount path may contain a comma, which is why this is JSON and not
    /// a delimited list.
    #[test]
    fn a_path_may_contain_a_separator() {
        let reqs = VolumeRequests(vec![VolumeRequest {
            name: "odd".into(),
            path: "/mnt/a,b/c d".into(),
            mode: Mode::ReadWrite,
        }]);
        assert_eq!(
            VolumeRequests::from_env(&reqs.to_env()).0[0].path,
            "/mnt/a,b/c d"
        );
    }

    #[test]
    fn the_request_is_bounded_and_junk_yields_nothing() {
        let many: Vec<VolumeRequest> = (0..50)
            .map(|i| VolumeRequest {
                name: format!("v{i}"),
                path: format!("/v{i}"),
                mode: Mode::ReadOnly,
            })
            .collect();
        let env = VolumeRequests(many).to_env();
        assert_eq!(VolumeRequests::from_env(&env).len(), MAX_VOLUMES_PER_JOB);

        let junk = vec![(REQ_VOLUMES.to_string(), "not json".to_string())];
        assert!(VolumeRequests::from_env(&junk).is_empty());
    }

    #[test]
    fn a_mode_round_trips_and_refuses_junk() {
        assert_eq!(Mode::parse("rw"), Some(Mode::ReadWrite));
        assert_eq!(Mode::parse("read-only"), Some(Mode::ReadOnly));
        assert_eq!(Mode::parse("maybe"), None);
        assert!(Mode::ReadOnly.read_only());
        assert!(!Mode::ReadWrite.read_only());
    }

    #[test]
    fn a_name_is_held_to_an_environment_key_shape() {
        assert!(is_valid_volume_name("data"));
        assert!(is_valid_volume_name("hot-cache_2"));
        assert!(!is_valid_volume_name(""));
        assert!(!is_valid_volume_name("-lead"));
        assert!(!is_valid_volume_name("has space"));
        assert!(!is_valid_volume_name("dotted.name"));
        assert!(!is_valid_volume_name(&"x".repeat(49)));
        assert_eq!(key_suffix("hot-cache"), "HOT_CACHE");
    }

    /// An unadvertised list disqualifies, unlike an unmeasured dimension.
    #[test]
    fn a_node_with_no_volume_tag_offers_none() {
        assert!(from_tags::<String>(&[]).is_empty());
        assert_eq!(
            from_tags(&[
                "cap.cores=8".to_string(),
                "cap.volumes=data,cache".to_string()
            ]),
            vec!["data".to_string(), "cache".to_string()]
        );
        assert_eq!(
            cap_tag(&["data".to_string(), "cache".to_string()]).as_deref(),
            Some("cap.volumes=data,cache")
        );
        assert_eq!(cap_tag(&[]), None);
    }
}

// ---------------------------------------------------------------------------
// N5 P2 — dynamic volumes, the plugin contract
// ---------------------------------------------------------------------------

/// Nomad's Dynamic Host Volume interface, copied on purpose — and copied
/// with its two holes fixed.
///
/// **Why copy it.** It is the best design in modern Nomad and deliberately
/// anti-CSI: *any executable*, the operation in `$1`, inputs as environment
/// variables, one JSON line on stdout. No gRPC, no sidecar, no controller
/// deployment, no CRDs. A plugin is a shell script somebody can read.
///
/// **The two holes.**
///
/// 1. **No configuration mechanism.** Theirs has none — the convention is
///    "put a file next to the binary and work it out". So a plugin that
///    needs a pool name, an endpoint or a credential has nowhere to be told
///    one, and every deployment invents a different answer.
///    [`PLUGIN_CONFIG_ENV`] is the declared path.
/// 2. **A request that chooses the plugin.** A Nomad create request names
///    a plugin, and one naming a path outside the plugin directory ran
///    another program as the agent (CVE-2026-7474, fixed in Nomad 2.0.1,
///    1.11.5 and 1.10.11). No request here carries a plugin at all: a node
///    runs its own `[runtime] volume_plugin`.
///
/// And, beyond Nomad's two: every field this side sends is validated before
/// it leaves — names by [`is_valid_volume_name`], everything else by
/// [`is_safe_plugin_value`] — so a plugin author who forgets to quote is
/// not handed a metacharacter to be careless with.
pub const PLUGIN_CONTRACT_VERSION: u32 = 1;

/// Where a plugin is told to find its own configuration (hole 1).
///
/// Set by meshware from `[runtime] volume_plugin_config`, so a plugin has a
/// declared place to read a pool name or an endpoint from instead of a
/// convention it has to guess.
pub const PLUGIN_CONFIG_ENV: &str = "MESHWARE_VOLUME_PLUGIN_CONFIG";

/// The operation, in `$1`: `create`, `delete`, or `fingerprint`.
pub const PLUGIN_OPS: [&str; 3] = ["create", "delete", "fingerprint"];

/// Environment a plugin is given for `create` and `delete`.
pub const PLUGIN_NAME_ENV: &str = "MESHWARE_VOLUME_NAME";
pub const PLUGIN_CAPACITY_ENV: &str = "MESHWARE_VOLUME_CAPACITY_MB";
pub const PLUGIN_PARAMS_ENV: &str = "MESHWARE_VOLUME_PARAMS";
/// Where meshware expects the volume to end up, when it has an opinion.
pub const PLUGIN_PATH_ENV: &str = "MESHWARE_VOLUME_PATH";

/// What a plugin prints on stdout for `create`: one JSON object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCreated {
    /// The directory the volume now lives at on this host.
    pub path: String,
    /// Bytes actually provisioned, when the plugin knows.
    #[serde(default)]
    pub capacity_mb: u64,
    /// Anything worth showing an operator.
    #[serde(default)]
    pub detail: String,
}

/// Is this value safe to hand a plugin through the environment?
///
/// **Validated on this side, before it leaves**: a plugin author who
/// forgets to quote `$MESHWARE_VOLUME_NAME` should get a broken plugin, not
/// a shell injection. Refusing the metacharacters here means the careless
/// plugin is merely careless.
pub fn is_safe_plugin_value(value: &str) -> bool {
    value.len() <= 1024
        && !value.contains([
            '\0', '\n', '\r', ';', '&', '|', '$', '`', '<', '>', '\'', '"', '\\', '*', '?', '(',
            ')', '{', '}', '[', ']', '!',
        ])
}

#[cfg(test)]
mod plugin_tests {
    use super::*;

    /// Hole 2, closed. A plugin author who forgets to quote gets a broken
    /// plugin rather than a shell injection.
    #[test]
    fn author_controlled_values_are_validated_before_they_leave() {
        for ok in ["pool-a", "fast/ssd", "10G", "tier=gold", "a.b-c_d"] {
            assert!(is_safe_plugin_value(ok), "{ok:?}");
        }
        for bad in [
            "a; rm -rf /",
            "a && curl evil",
            "$(id)",
            "`id`",
            "a|b",
            "a\nb",
            "a\0b",
            "a'b",
            "a\"b",
            "a*b",
            &"x".repeat(1025),
        ] {
            assert!(!is_safe_plugin_value(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_create_reply_decodes_with_only_a_path() {
        let r: PluginCreated = serde_json::from_str(r#"{"path":"/srv/dyn/data"}"#).expect("decode");
        assert_eq!(r.path, "/srv/dyn/data");
        assert_eq!(r.capacity_mb, 0);
        assert!(r.detail.is_empty());
    }
}
