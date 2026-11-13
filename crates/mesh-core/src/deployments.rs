//! Deployments: parsing, validation, signing, and the reconciliation the
//! supervisor runs every tick (C2 §6).
//!
//! Like [`crate::schedules`], everything here is a pure function of a
//! definition and the mesh's replicated state. The *firing* — authoring
//! instance jobs, cancelling them, publishing health — lives in the daemon;
//! the *decisions* live here, where they can be tested without a mesh.
//!
//! The two non-obvious rules in this file:
//!
//! - **[`elected_reconciler`]** — a deployment is owned by whichever live
//!   node rendezvous-hashing picks for its *name*, recomputed every tick,
//!   not by the node that created it. A deployment lives for years and the
//!   registering laptop is the one most likely to be permanently gone
//!   (§6.2). This is the same election `gitsync::elected_watcher` and the
//!   sink exporter use; it is not a new supervisor lease.
//!
//! - **[`plan_reconcile`]** — the whole supervisor, as arithmetic. It reads
//!   desired count, the derived ids of the instances that *should* exist,
//!   and what actually exists in `jobs` and `deployment_health`, and
//!   returns a plan of starts and cancels. Restart backoff, the failure
//!   budget, health-based replacement and rolling updates are all decided
//!   here and executed blindly by the daemon.

use std::collections::BTreeMap;
use std::sync::Arc;

use mesh_proto::{
    Count, Deployment, HealthCheck, HealthKind, JobId, JobState, MemberInfo, NodeId, OpBody,
    RestartPolicy, UpdatePolicy, UpdateStrategy, UserId,
    deployment::{DeploymentInstanceRow, DeploymentRow, DeploymentStatusView},
    is_valid_deployment_name,
};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::membership::Selector;
use crate::scheduler;
use crate::state::{MeshState, StateEngine};

/// Rendezvous context for reconciler election. Its own string, so a node's
/// rank for a deployment is unrelated to its rank for a schedule or a git
/// source — coinciding owners would look like a bug and concentrate load.
const ELECTION_CONTEXT: &[u8] = b"mw-deployment-v1";

/// How many prior generations the reconciler looks back at for leftover
/// instances to retire during a roll. A roll advances by one generation, so
/// two is already generous; an interrupted roll that stacked more than this
/// leaves the very oldest to be cancelled unconditionally on sight.
const GEN_LOOKBACK: u32 = 2;

/// How many of a slot's **newest** incarnations a walk looks at, per
/// generation ([`recent_incarnations`]). Far above any healthy service's
/// churn, and enough to hold every failure a restart budget's window can
/// count.
///
/// It used to be the *first* 64, counted up from incarnation 0 — and that
/// was a trap for exactly the slot that most needed watching. A slot that
/// had failed 64 times had an incarnation 64 no walk ever reached: the
/// planner asked to start it on every tick, found the job already there and
/// skipped it, so the slot was never tried again, and the status showed it
/// as never placed.
const MAX_INCARNATION_SCAN: u32 = 64;

// ---------------------------------------------------------------------------
// Definition source (TOML, the shape the sketch uses; YAML/JSON also parse)
// ---------------------------------------------------------------------------

/// A deployment as a human writes it (§6.3).
///
/// `deny_unknown_fields` because a typo in an unattended supervisor is a
/// service that silently does the wrong thing forever — the same reason a
/// schedule refuses unknown fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentSource {
    #[serde(default)]
    pub deployment: DeploymentHeader,
}

/// The `[deployment]` table, with `restart`/`health`/`update` nested under
/// it exactly as the sketch writes them (`[deployment.restart]`, §6.3).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentHeader {
    pub name: String,
    /// `collector@7` — an existing immutable service version. A floating
    /// head (`collector`) is refused: what stays up cannot be a moving
    /// target (see [`build`]).
    ///
    /// Optional since `[deployment.container]`: a file that describes its
    /// own image names no service, because it *is* one. `build` refuses a
    /// file that has neither, and one that has both.
    #[serde(default)]
    pub service: String,
    /// `system` or an integer.
    #[serde(default)]
    pub count: Option<toml::Value>,
    #[serde(default)]
    pub targets: Option<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub description: String,
    /// Only honoured on a rollout/rollback path; a fresh `put` starts at 0.
    #[serde(default)]
    pub generation: Option<u32>,
    #[serde(default)]
    pub restart: Option<RestartSource>,
    #[serde(default)]
    pub health: Option<HealthSource>,
    #[serde(default)]
    pub update: Option<UpdateSource>,
    #[serde(default)]
    pub disconnect: Option<DisconnectSource>,
    #[serde(default)]
    pub network: Option<NetworkSource>,
    /// `[deployment.register]`. **Not `[deployment.service]`**: the header
    /// already has a scalar `service = "web@1"` naming what to run, and
    /// TOML will not have both — the collision is a duplicate-key error
    /// with no hint about which one to rename. `register` also says the
    /// verb, which is what the block does.
    #[serde(default)]
    pub register: Option<RegisterSource>,
    /// `[[deployment.volume]]` (N5) — directories these instances claim.
    #[serde(default)]
    pub volume: Vec<VolumeSource>,
    /// `[deployment.resources]` — what each instance needs.
    ///
    /// Nomad's `resources` block, and the same job: these are a *filter*
    /// and a *ceiling*, not a suggestion. A node without the memory is not
    /// a candidate, and on a node that can enforce them (cgroups v2) they
    /// bound the instance rather than advising it.
    ///
    /// Absent means unstated, which is not the same as zero: an
    /// unmeasured requirement never disqualifies a node, where `memory =
    /// 0` would be a claim that it needs none.
    #[serde(default)]
    pub resources: Option<ResourceSource>,
    /// `[deployment.container]` — the image, **inline**.
    ///
    /// This is the block that makes a deployment a whole job description
    /// rather than half of one. Before it, running `nginx` meant pushing a
    /// folder the image would never read, inventing a service name for it,
    /// publishing that, and only then writing the deployment that named it
    /// — three steps and two concepts to describe one container.
    ///
    /// With it, `service` is not required: meshware derives a service from
    /// this block, named after the deployment, and versions it by content.
    /// Change the image and re-publish and you get a new version and a
    /// rollout, which is what `nomad job run` does with an edited file.
    ///
    /// The service concept is **not** going away — a named, versioned,
    /// signed artifact is the right unit for anything anybody runs twice,
    /// and `service:` still names one. This is for the case where the
    /// artifact *is* the image and the indirection bought nothing.
    #[serde(default)]
    pub container: Option<ContainerSource>,
    /// `distinct_hosts = true` (Phase C, E7) — at most one instance per
    /// machine, as a rule rather than the preference it otherwise is.
    #[serde(default)]
    pub distinct_hosts: Option<bool>,
    /// `[deployment.distinct_property]` (Phase C, E7).
    #[serde(default)]
    pub distinct_property: Option<DistinctPropertySource>,
    /// `[[deployment.affinity]]` (Phase C, E8).
    #[serde(default)]
    pub affinity: Vec<AffinitySource>,
    /// `[deployment.spread]` (Phase C, E8).
    #[serde(default)]
    pub spread: Option<SpreadSource>,
    /// `[deployment.migrate]` (Phase C, E9) — how instances leave a machine
    /// being drained.
    #[serde(default)]
    pub migrate: Option<MigrateSource>,
    /// `priority = 70` (Phase C, E10): 1 to 100, 50 when absent. Orders an
    /// instance in an executor's queue, and above the default lets it take a
    /// busy machine's slot from a `--preemptible` job at least 10 below.
    #[serde(default)]
    pub priority: Option<u32>,
}

/// `[deployment.migrate]` — Nomad's `migrate` block, with its defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrateSource {
    /// How many instances may be moving at once. 1 when absent.
    #[serde(default)]
    pub max_parallel: Option<u32>,
    /// How long a replacement must be healthy before the original stops.
    /// `10s` when absent.
    #[serde(default)]
    pub min_healthy_time: Option<String>,
    /// How long a replacement has to become healthy before the next
    /// instance may start moving anyway. `5m` when absent.
    #[serde(default)]
    pub healthy_deadline: Option<String>,
}

/// `[deployment.distinct_property]` — at most `limit` instances on machines
/// that share one value of `attribute` (Nomad's `distinct_property`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistinctPropertySource {
    /// `tag.rack`, `fact.site`, `name`.
    pub attribute: String,
    /// One when absent.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `[[deployment.affinity]]` — machines to prefer, or with a negative
/// weight to avoid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffinitySource {
    /// A selector, in the grammar `targets` takes.
    pub select: String,
    /// -100 to 100, not 0. Fifty when absent, as in Nomad.
    #[serde(default)]
    pub weight: Option<i32>,
}

/// `[deployment.spread]` — share the instances out across an attribute's
/// values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpreadSource {
    pub attribute: String,
    /// `value = percent`. Absent or empty spreads evenly over every value the
    /// candidates have; values not named share what is left.
    #[serde(default)]
    pub targets: BTreeMap<String, u32>,
}

/// `[deployment.resources]` — CPU, memory and disk per instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSource {
    /// Whole CPUs. Nomad counts MHz; meshware counts cores, because that
    /// is what a node advertises and what an operator can picture.
    #[serde(default)]
    pub cpus: Option<u32>,
    #[serde(default)]
    pub memory_mb: Option<u64>,
    #[serde(default)]
    pub disk_mb: Option<u64>,
}

/// `[deployment.container]` — an image, run as these instances.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSource {
    /// `nginx:alpine`, `ghcr.io/acme/api:1.4`.
    pub image: String,
    /// Override the image's entry point. Absent runs what the image was
    /// built to run, which is the common case and the reason an image
    /// exists.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory **inside the image**.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Static environment for every instance.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// `[[deployment.volume]]` — a volume these instances claim by name.
///
/// The name must be one a node *offers* (`[[volume]]` in its own
/// `meshnode.toml`), which is what makes placement able to filter on it: a
/// stateful deployment goes to the machines that have its data, and nowhere
/// else.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeSource {
    pub name: String,
    /// Where to mount it **inside a container**. A process runtime gets the
    /// host path in `MESHWARE_VOLUME_<NAME>` instead — there is no mount
    /// namespace to put it in.
    #[serde(default)]
    pub path: Option<String>,
    /// `rw` (default) or `ro`.
    #[serde(default)]
    pub mode: Option<String>,
}

/// `[deployment.register]` (N3) — register these instances under a name, so
/// `<name>.mesh` resolves to the healthy ones.
///
/// **Opt-in, and absent by default.** A mesh where every deployment
/// appeared in DNS automatically would be a mesh where DNS is a list of
/// everything rather than an answer, and the name a thing answers to is a
/// decision worth typing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterSource {
    /// The name clients resolve. Becomes `<name>.mesh`.
    pub name: String,
    /// Free-form, for filtering: `canary`, `v2`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Which declared port label to publish. Absent = all of them, which
    /// is right for the common single-port case.
    #[serde(default)]
    pub port: Option<String>,
    /// How long an instance stays **deregistered** before it is signalled:
    /// `10s` (N3 P2).
    ///
    /// This window is the whole point of the ordering. Deregister, wait,
    /// then signal — so a load balancer notices the instance has left
    /// rotation before its process dies, rather than after.
    #[serde(default)]
    pub shutdown_delay: Option<String>,
    /// How long a signalled instance has to exit before it is force-killed.
    #[serde(default)]
    pub kill_timeout: Option<String>,
}

/// `[deployment.network]` (N2 P2): the ports this deployment's instances
/// need.
///
/// **The block is sugar, and deliberately so.** What it produces is the
/// reserved job-environment key `MESHWARE_TASK_PORTS`, which is the same
/// thing `meshnode run --port` and a service's own `--env` produce — so a
/// port request needs no wire change, reaches an executor by the path every
/// other resource requirement already takes, and does not become a third
/// encoding of one idea.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSource {
    /// `host` — the only mode N2 ships.
    ///
    /// Bridge and CNI are Linux-only even in Nomad and are a much larger
    /// project; refusing an unknown word here is what stops a file written
    /// for a future mode from silently getting host networking.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub port: Vec<PortSource>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortSource {
    /// `http`, `metrics`. Becomes `MESHWARE_PORT_HTTP` and friends.
    pub label: String,
    /// The port inside the job — what its config file says it listens on.
    #[serde(default)]
    pub to: Option<u16>,
    /// A fixed host port. `0` or absent means dynamic.
    #[serde(default, rename = "static")]
    pub fixed: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartSource {
    /// `1s..5m` — min and max backoff.
    #[serde(default)]
    pub backoff: Option<String>,
    /// `5 in 10m` — failures allowed in a window before `degraded`.
    #[serde(default)]
    pub budget: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSource {
    /// `exec` | `tcp` | `http`.
    pub kind: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    /// The port to probe: a **number**, or the **label** of a port this
    /// deployment declares — `port = "http"`, which is what Nomad's check
    /// block takes and the only form that can work with an allocated port.
    ///
    /// Absent, on a deployment that declares exactly one port, means that
    /// one: a check with nothing to probe but the single port the instance
    /// was given has only one sensible reading.
    #[serde(default)]
    pub port: Option<toml::Value>,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub grace: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub failures: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSource {
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub max_unavailable: Option<u32>,
    /// `health` gates each batch on the new instances passing health;
    /// anything else gates on running.
    #[serde(default)]
    pub gate: Option<String>,
    // -- N1 P3. These land in `DeploymentPolicy`, not in `UpdatePolicy`:
    // the file is one thing to a person and two records on the wire, for
    // the signature reason `DeploymentPolicy`'s own doc gives.
    /// Place N new instances *without* stopping any old ones.
    #[serde(default)]
    pub canary: Option<u32>,
    /// Roll back to the last generation that went fully healthy when this
    /// rollout fails.
    #[serde(default)]
    pub auto_revert: Option<bool>,
    /// `10m` — how long the roll may make no progress before it fails.
    #[serde(default)]
    pub progress_deadline: Option<String>,
    /// `30s` — how long healthy counts as healthy.
    #[serde(default)]
    pub min_healthy_time: Option<String>,
    /// Phase B, E2: `false` holds settled canaries until `deployment
    /// promote`. Lands in `DeploymentKnobs`, for the reason that type gives.
    #[serde(default)]
    pub auto_promote: Option<bool>,
}

/// `[deployment.disconnect]` (N1 P4): what happens to instances on the far
/// side of a partition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisconnectSource {
    /// `10m` — how long an unreachable node's instances stay `unknown`
    /// before they are considered lost.
    #[serde(default)]
    pub lost_after: Option<String>,
    /// Start a replacement while the original is still `unknown`.
    #[serde(default)]
    pub replace: Option<bool>,
    /// `keep_original` | `keep_replacement` | `best_score` |
    /// `longest_running`.
    #[serde(default)]
    pub reconcile: Option<String>,
}

/// The JSON Schema for a deployment file (Phase B, E4).
///
/// Hand-written, and held to the parser by a test in both directions: every
/// key the schema names must be one a `*Source` struct accepts, and every
/// key a struct accepts must be in the schema. The struct side is read out
/// of serde's own `deny_unknown_fields` refusal, so no schema-derivation
/// crate joins the dependency tree — the property `meshnode` guards — and
/// the two still cannot drift without a test saying so by name.
pub const SCHEMA: &str = include_str!("deployment.schema.json");

/// A complete, commented starting file (Phase B, E4) — `meshnode deployment
/// init` and Director's *Deploy an example* print the same one.
///
/// YAML because it is the shape people paste, and every block in it is one
/// a first deployment plausibly wants. It parses as written, and a test
/// holds it to that: an example that does not work is a trap with a
/// friendly face.
pub fn example(name: &str, image: &str) -> String {
    format!(
        concat!(
            "# A meshware deployment: what should stay up, and how it rolls.\n",
            "#   plan it:     meshnode deployment put --dry-run {name}.yaml\n",
            "#   publish it: meshnode deployment put {name}.yaml\n",
            "deployment:\n",
            "  name: {name}\n",
            "  count: 2                    # or \"system\": one on every matching node\n",
            "  targets: any                # tag=edge, name=a|name=b, fact.os=linux\n",
            "\n",
            "  # The image, inline. Or name a published service: `service: {name}@1`\n",
            "  container:\n",
            "    image: {image}\n",
            "\n",
            "  network:\n",
            "    port:\n",
            "      - label: http\n",
            "        to: 80                # the port inside the container\n",
            "\n",
            "  resources:\n",
            "    cpus: 1\n",
            "    memory_mb: 128\n",
            "\n",
            "  register:\n",
            "    name: {name}               # answers at {name}.mesh\n",
            "\n",
            "  health:\n",
            "    kind: http\n",
            "    port: http                # by label, so it follows the allocated port\n",
            "    path: /\n",
            "    interval: 10s\n",
            "    grace: 10s\n",
            "\n",
            "  update:\n",
            "    max_unavailable: 1\n",
            "    canary: 1                 # new instances prove themselves beside the old\n",
            "    auto_revert: true         # a stalled roll goes back to the last good version\n",
            "    progress_deadline: 10m    # the default whenever canary or auto_revert is set\n",
            "    min_healthy_time: 30s\n",
            "\n",
            "  disconnect:\n",
            "    lost_after: 60s           # the default; \"never\" waits for the node\n",
        ),
        name = name,
        image = image,
    )
}

/// What a parsed file will do, as label/value rows (Phase B, E4).
///
/// For `meshnode deployment validate` and Director's inline check — one
/// renderer, so the offline answer to "what does this file mean" is worded
/// the same on both surfaces. Everything here is decided by the file alone;
/// what the *mesh* thinks of it is the dry run's job.
pub fn describe(parsed: &Parsed) -> Vec<(String, String)> {
    let dep = &parsed.deployment;
    let policy = &parsed.policy;
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut row = |k: &str, v: String| rows.push((k.to_string(), v));
    row("name", dep.name.clone());
    row(
        "runs",
        match &parsed.inline {
            Some(inline) => format!("image {}", inline.image),
            None => dep.service_ref(),
        },
    );
    row(
        "count",
        match dep.count {
            Count::System => "one on every matching node".into(),
            Count::Fixed(n) => n.to_string(),
        },
    );
    row("targets", dep.targets.clone());
    let ports = policy.ports();
    if !ports.is_empty() {
        row(
            "ports",
            ports
                .0
                .iter()
                .map(|p| match (p.fixed, p.to) {
                    (Some(fixed), _) => format!("{} static {fixed}", p.label),
                    (None, to) if to > 0 => format!("{} → {to}", p.label),
                    _ => p.label.clone(),
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if let Some(name) = policy.env_value(mesh_proto::REQ_SERVICE) {
        row("registers", format!("{name}.{}", mesh_proto::DNS_SUFFIX));
    }
    row(
        "health",
        match &dep.health {
            None => "none — an instance counts as up while its job runs".into(),
            Some(h) => match h.kind {
                HealthKind::Exec => format!("exec `{}` every {}s", h.command, h.interval_secs),
                HealthKind::Tcp => format!("tcp every {}s", h.interval_secs),
                HealthKind::Http => format!("http {} every {}s", h.path, h.interval_secs),
            },
        },
    );
    let mut rollout = vec![format!("{} at a time", dep.update.max_unavailable.max(1))];
    if policy.canary > 0 {
        rollout.push(format!("{} canary", policy.canary));
    }
    if policy.auto_revert {
        rollout.push("auto-revert".into());
    }
    if !parsed.knobs.auto_promote() {
        rollout.push("promoted by hand".into());
    }
    if policy.progress_deadline_secs > 0 {
        rollout.push(format!(
            "fails after {} without progress",
            mesh_proto::duration_words(policy.progress_deadline_secs)
        ));
    }
    row("rollout", rollout.join(", "));
    let placement = mesh_proto::PlacementSpec::from_env(&policy.instance_env);
    if !placement.is_empty() {
        row("placement", placement_words(&placement));
    }
    if policy.env_value(mesh_proto::REQ_PRIORITY).is_some() {
        let p = mesh_proto::priority_of_env(&policy.instance_env);
        row(
            "priority",
            if p > mesh_proto::DEFAULT_PRIORITY {
                format!(
                    "{p} — may take a busy machine's slot from a --preemptible job at {} or below",
                    p - mesh_proto::PREEMPT_MARGIN
                )
            } else {
                format!("{p} — preempts nothing")
            },
        );
    }
    row("failover", mesh_proto::availability(&dep.count, policy));
    rows
}

/// Parse a definition. TOML when it does not start with `{` and contains an
/// `=` before any `:`, otherwise JSON/YAML — but the sketch is TOML, so TOML
/// is the default and the one the docs show.
/// A definition file, as the two records it becomes.
///
/// One file to a person; a [`Deployment`] and a [`DeploymentPolicy`] on the
/// wire. The split is not a modelling choice — it is forced by the
/// signature rule [`DeploymentPolicy`] documents — so it is hidden here
/// and every caller handles both halves together.
#[derive(Debug, Clone)]
pub struct Parsed {
    /// The service this deployment's `[deployment.container]` block
    /// describes, when it has one — for the caller to publish before the
    /// deployment that names it.
    ///
    /// Returned rather than published here because `parse` is pure: it
    /// takes text and gives back records, and the op-log write belongs to
    /// whoever holds the signing key. [`resolve_inline`] does the rest.
    pub inline: Option<InlineContainer>,
    pub deployment: Deployment,
    /// Inert (`DeploymentPolicy::is_inert`) when the file asked for
    /// nothing new, which is the common case and the one that authors no
    /// second op.
    pub policy: mesh_proto::DeploymentPolicy,
    /// Settings past the policy's fixed fields (Phase B, E2). Sent only by a
    /// whole-file publish; empty when the file asked for every default.
    pub knobs: mesh_proto::DeploymentKnobs,
}

pub fn parse(source: &str, created_by: UserId) -> Result<Parsed> {
    let trimmed = source.trim_start();
    let parsed: DeploymentSource = if trimmed.starts_with('{') {
        serde_json::from_str(source)
            .map_err(|e| CoreError::State(format!("deployment is not valid JSON: {e}")))?
    } else if looks_like_toml(trimmed) {
        toml::from_str(source)
            .map_err(|e| CoreError::State(format!("deployment is not valid TOML: {e}")))?
    } else {
        serde_saphyr::from_str(source)
            .map_err(|e| CoreError::State(format!("deployment is not valid YAML: {e}")))?
    };
    build(parsed, created_by)
}

/// A `[deployment]` header or a `key = value` line is TOML; a `key:` line is
/// YAML. The same first-character heuristic the rest of the CLI uses, one
/// format wider because the sketch writes TOML.
///
/// Judged on the first line that is not blank or a comment (2026-09-14). It
/// used to test the header only when it was the very first character of the
/// file, so a TOML file that opened with comments and then `[deployment]` —
/// the way anybody documents a file — was read as YAML and refused at its
/// own header. A deployment's top level is a mapping, so no YAML deployment
/// has a first real line starting with `[`.
fn looks_like_toml(s: &str) -> bool {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .is_some_and(|l| l.starts_with('[') || (l.contains('=') && !l.trim_end().ends_with(':')))
}

/// Build a record from a parsed definition, applying every rule that can be
/// checked without the mesh.
/// A container described inline, ready to become a service version.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineContainer {
    /// The service name this will be published as — the deployment's own
    /// name, so there is one obvious place to look for it.
    pub service: String,
    pub image: String,
    pub working_dir: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl InlineContainer {
    /// The reserved `MESHWARE_TASK_*` entries plus the operator's own,
    /// which is what a derived service's environment is made of.
    pub fn service_env(&self) -> Vec<(String, String)> {
        let mut env = mesh_proto::command_env(self.command.as_deref(), &self.args);
        env.extend(self.env.iter().cloned());
        env
    }

    /// Is `have` already exactly this container?
    ///
    /// Compared on the fields that decide what runs, so re-publishing an
    /// unchanged file reuses the version rather than minting one that
    /// differs from its predecessor in nothing but its number — and does
    /// not roll a deployment that nobody changed.
    pub fn matches_parts(&self, have: &Existing) -> bool {
        have.runtime == mesh_proto::Runtime::Container
            && have.entrypoint == self.image
            && have.working_dir == self.working_dir
            && have.bundle_id.is_none()
            && have.env == self.service_env()
    }
}

/// The newest published version of a service, as much of it as the version
/// rule needs.
///
/// A struct rather than a `&MeshState` lookup so the **clients** can answer
/// it: the CLI and Director hold the signing key and do the publishing, and
/// they see the mesh through `IpcResponse::ServiceShow`, not through state.
/// One rule, three callers, no second implementation.
#[derive(Debug, Clone)]
pub struct Existing {
    pub runtime: mesh_proto::Runtime,
    pub version: u32,
    pub entrypoint: String,
    pub working_dir: String,
    pub bundle_id: mesh_proto::BundleId,
    pub env: Vec<(String, String)>,
}

impl From<&mesh_proto::ServiceRow> for Existing {
    fn from(r: &mesh_proto::ServiceRow) -> Self {
        Existing {
            runtime: r.runtime,
            version: r.versions.iter().copied().max().unwrap_or(r.version),
            entrypoint: r.entrypoint.clone(),
            working_dir: r.working_dir.clone(),
            bundle_id: r.bundle_id,
            env: r.env.clone(),
        }
    }
}

/// Does going from `prev` to `next` change **what actually runs**?
///
/// The question a generation exists to answer. Rollout knobs — how many
/// canaries, how long to wait — are deliberately not part of it: raising
/// `canary` from 1 to 2 changes how the *next* change rolls, and restarting
/// a healthy service to record that would be absurd. Nor is `paused`, which
/// is a switch on the supervisor rather than a change to the work.
pub fn rolls(
    prev: &Deployment,
    prev_env: &[(String, String)],
    next: &Deployment,
    next_env: &[(String, String)],
) -> bool {
    // `instance_env` carries ports, volumes, resources and the operator's
    // own environment — every one of which changes the process. Sorted
    // before comparing: it is assembled by walking several blocks, and the
    // order it comes out in is not a fact about the deployment.
    //
    // **Placement and migration rules are the exception (Phase C).** They
    // ride the same carrier and decide where the *next* instance goes and how
    // one leaves a drained machine; changing an affinity must not restart
    // every healthy instance to obey it.
    let norm = |e: &[(String, String)]| {
        let mut v: Vec<(String, String)> = e
            .iter()
            .filter(|(k, _)| {
                k.as_str() != mesh_proto::REQ_PLACEMENT
                    && k.as_str() != mesh_proto::REQ_MIGRATE
                    && k.as_str() != mesh_proto::REQ_PRIORITY
            })
            .cloned()
            .collect();
        v.sort();
        v
    };
    prev.service != next.service
        || prev.service_version != next.service_version
        // **Not the size (Phase B, E3).** Growing or shrinking a fixed
        // count adds or retires replicas at the same generation — the
        // reconciler already starts missing slots and cancels the ones past
        // the count — so a resize is `nomad job scale`, not a redeploy of
        // everything that was already fine. Switching between `system` and
        // a number does roll: the slots stop being the same slots.
        || matches!(prev.count, Count::System) != matches!(next.count, Count::System)
        || prev.targets != next.targets
        || prev.inputs != next.inputs
        || prev.health != next.health
        || prev.restart != next.restart
        || norm(prev_env) != norm(next_env)
}

/// `90s`, `5m`, `1h` — a delay as somebody would say it.
fn humanise_secs(secs: u64) -> String {
    match secs {
        0 => "moment".into(),
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

/// The longest a slot ever waits between attempts (N7).
///
/// An hour, which is Nomad's `max_delay` default for a service job's
/// reschedule policy. Long enough that a deployment nothing can run costs
/// the mesh almost nothing; short enough that fixing the cause — installing
/// podman, freeing a port, pushing a working image — is picked up without
/// anybody touching the deployment.
pub const RESCHEDULE_MAX_SECS: u64 = 3600;

/// How long this slot waits before its next attempt.
///
/// Two regimes, and the boundary is the restart budget:
///
/// - **Inside the budget**, the deployment's own `backoff` — a crash loop
///   that will settle on its own settles quickly.
/// - **Past it**, the backoff maximum doubled once per excess failure and
///   capped at [`RESCHEDULE_MAX_SECS`]. This is the reschedule half of
///   Nomad's model: the slot has proved it cannot start where it has been
///   trying, so the mesh keeps trying — somewhere else, and slowly.
///
/// It never returns to "never". A slot that stops being retried is a
/// replica the operator asked for and did not get, and the only thing worse
/// than a crash loop is a crash loop that went quiet.
pub fn reschedule_delay(
    restart: &mesh_proto::RestartPolicy,
    incarnation: u32,
    failures: u32,
) -> u64 {
    if failures <= restart.budget_failures {
        return restart.backoff_secs(incarnation.saturating_sub(1));
    }
    let over = failures.saturating_sub(restart.budget_failures);
    let base = restart.backoff_secs(u32::MAX).max(1);
    let grown = base.saturating_mul(1u64 << over.min(20));
    grown.min(RESCHEDULE_MAX_SECS)
}

/// The generation a `put` should carry.
///
/// **The bug this exists to fix.** A fresh put always said generation 0,
/// so re-publishing an edited file over a live deployment reused the
/// generation already running: the reconciler saw those instances as
/// current, replaced nothing, and the CLI printed `✓ rollout complete` over
/// a mesh that had not changed. Editing the image and publishing — the
/// single most ordinary thing anybody does — silently did nothing and
/// claimed success.
///
/// It also inherited that generation's *failures*. A version that crash
/// looped burned the restart budget, and the fix published over it counted
/// against the same window, so the mesh refused to deploy the repair for
/// ten minutes. Generations are how the budget is scoped, so a new one
/// starts clean.
///
/// An unchanged re-publish keeps its generation and rolls nothing, which is
/// what `nomad job run` does with an unmodified file.
pub fn generation_for(
    existing: Option<(&Deployment, &[(String, String)])>,
    next: &Deployment,
    next_env: &[(String, String)],
) -> u32 {
    match existing {
        None => 0,
        Some((prev, prev_env)) => {
            if rolls(prev, prev_env, next, next_env) {
                prev.generation.saturating_add(1)
            } else {
                prev.generation
            }
        }
    }
}

/// The deployment a published record stands for, for comparison against a
/// file about to replace it.
///
/// Re-parses the record's own definition, so the comparison is between two
/// files rather than between a file and a view of one: the fields that
/// decide whether this is a roll — `[deployment.network]`, volumes,
/// resources, the health block — all survive that round trip, and a view
/// carries only some of them.
///
/// **The row wins where it knows better.** The stored file is re-parsed for
/// what only a file carries, but its `generation` and service version may
/// predate a `rollout` that moved them, and for a file with an inline
/// container the parsed version is a placeholder rather than the number the
/// service settled on. Both come off the row instead.
///
/// `None` means there is nothing published to compare against — a first
/// publish, whether because the deployment is new or because its record is
/// too old to parse. Either way the next generation is 0, and refusing to
/// publish over an unreadable record would turn a stale row into an outage.
pub fn previous_from_row(
    row: &mesh_proto::DeploymentRow,
    user: mesh_proto::UserId,
) -> Option<(Deployment, Vec<(String, String)>)> {
    let source = row.source.as_deref()?;
    let parsed = parse(source, user).ok()?;
    let mut prev = parsed.deployment;
    prev.generation = row.generation;
    if let Some((name, ver)) = row.service.rsplit_once('@')
        && let Ok(v) = ver.parse::<u32>()
    {
        prev.service = name.to_string();
        prev.service_version = v;
    }
    Some((prev, parsed.policy.instance_env))
}

/// What [`resolve_inline`] decided.
#[derive(Debug, Clone)]
pub enum InlineOutcome {
    /// Nothing to do: the deployment names a service that already exists.
    Named,
    /// This version is already published and identical; point at it.
    Reuse { version: u32 },
    /// Publish this, then the deployment.
    Publish {
        version: Box<mesh_proto::ServiceVersion>,
    },
}

/// Turn an inline container into a concrete service version, against the
/// mesh as it is now.
///
/// **Shared by both front ends on purpose.** The CLI and Director each hold
/// the signing key and each publish, so each could have grown its own
/// version-numbering rule — and two rules for "what version is this" is two
/// answers to a question that must have one.
///
/// The rule: if the newest version of the derived service is already
/// exactly this container, reuse it. Otherwise publish the next number.
/// Re-publishing an unchanged file therefore rolls nothing, and changing
/// the image rolls once — which is what an operator means by editing a file
/// and publishing it again.
pub fn resolve_inline(
    existing: Option<&Existing>,
    parsed: &mut Parsed,
    created_by: UserId,
) -> Result<InlineOutcome> {
    let Some(inline) = parsed.inline.clone() else {
        return Ok(InlineOutcome::Named);
    };
    // A service of this name that somebody else published from a bundle is
    // not ours to version. Refusing beats silently publishing a container
    // version of somebody's script under the same name.
    if let Some(e) = existing
        && e.runtime != mesh_proto::Runtime::Container
    {
        return Err(CoreError::State(format!(
            "a deployment with an inline container publishes a service named after it, \
             and {:?} already names a {} service. Rename the deployment, or point it at \
             that service with `service = \"{}@{}\"`",
            inline.service,
            e.runtime.as_str(),
            inline.service,
            e.version
        )));
    }
    let next = existing.map(|e| e.version).unwrap_or(0);
    if let Some(e) = existing
        && inline.matches_parts(e)
    {
        parsed.deployment.service_version = e.version;
        return Ok(InlineOutcome::Reuse { version: e.version });
    }
    let version = next + 1;
    parsed.deployment.service_version = version;
    Ok(InlineOutcome::Publish {
        version: Box::new(mesh_proto::ServiceVersion {
            name: inline.service.clone(),
            version,
            runtime: mesh_proto::Runtime::Container,
            // The whole point: an image needs nothing from the content
            // store, so there is nothing to push and nothing to fetch.
            bundle_id: mesh_proto::BundleId::NONE,
            working_dir: inline.working_dir.clone(),
            entrypoint: inline.image.clone(),
            contract: None,
            env: inline.service_env(),
            description: format!("inline container for deployment {:?}", inline.service),
            tags: Vec::new(),
            created_at_micros: mesh_proto::now_micros(),
            created_by,
            source: None,
            signature: Vec::new(),
        }),
    })
}

/// Validate a `[deployment.container]` block into something publishable.
fn build_inline(deployment: &str, c: &ContainerSource) -> Result<InlineContainer> {
    let image = c.image.trim();
    if image.is_empty() {
        return Err(CoreError::State(
            "[deployment.container] needs an `image`, as in `image = \"nginx:alpine\"`".into(),
        ));
    }
    // The same check the executor applies, run here so a typo is a
    // create-time error rather than a pull failure on six nodes at once.
    if !crate::containers::is_plausible_image(image) {
        return Err(CoreError::State(format!(
            "{image:?} is not a usable image reference — try `nginx:alpine` or \
             `ghcr.io/acme/api:1.4`"
        )));
    }
    let working_dir = c.working_dir.clone().unwrap_or_default().trim().to_string();
    if !working_dir.is_empty() && !crate::containers::is_plausible_container_path(&working_dir) {
        return Err(CoreError::State(format!(
            "working_dir {working_dir:?} is a path inside the image, and must be absolute"
        )));
    }
    let command = c
        .command
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(InlineContainer {
        service: deployment.to_string(),
        image: image.to_string(),
        working_dir,
        command,
        args: c.args.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    })
}

pub fn build(src: DeploymentSource, created_by: UserId) -> Result<Parsed> {
    let h = src.deployment;
    if !is_valid_deployment_name(&h.name) {
        return Err(CoreError::State(format!(
            "deployment name {:?} must be 1-64 characters of letters, digits, '-', '_' or '.'",
            h.name
        )));
    }
    // **Exactly one of `service:` and `[deployment.container]`.**
    //
    // Both would be two answers to "what runs here" with no rule for which
    // wins; neither leaves the deployment describing nothing.
    let inline = match (&h.container, h.service.trim().is_empty()) {
        (Some(_), false) => {
            return Err(CoreError::State(
                "a deployment names `service` or carries a [deployment.container] block, \
                 not both — the container block *is* the service when it is there"
                    .into(),
            ));
        }
        (None, true) => {
            return Err(CoreError::State(
                "this deployment says nothing about what to run. Either name a published \
                 service (`service = \"api@3\"`) or describe an image inline:\n\n  \
                 [deployment.container]\n  image = \"nginx:alpine\""
                    .into(),
            ));
        }
        (Some(c), true) => Some(build_inline(&h.name, c)?),
        (None, false) => None,
    };
    // A derived service is named after its deployment and always starts at
    // version 1 here; `resolve_inline` replaces the number with whatever
    // the mesh actually has before anything is signed.
    let (service, service_version) = match &inline {
        Some(c) => (c.service.clone(), 1),
        None => split_service_ref(&h.service)?,
    };
    let count = parse_count(h.count.as_ref())?;
    let targets = h.targets.unwrap_or_else(|| "any".to_string());
    // A malformed selector is a create-time error, not a supervisor that
    // quietly matches nothing at 03:00. This used to say so above a bare
    // `Selector::parse`, which cannot fail — so a typo in a `fact.` test
    // published and placed nothing (Phase C, E7).
    Selector::check(&targets).map_err(|why| CoreError::State(format!("targets: {why}")))?;

    let ports = parse_network(h.network.as_ref())?;
    let registration = parse_registration(h.register.as_ref(), &ports)?;
    let volumes = parse_volumes(&h.volume)?;
    // Phase C, E7/E8. Read before the port rule below, which one of them
    // relaxes.
    let placement = parse_placement(
        h.distinct_hosts,
        h.distinct_property.as_ref(),
        &h.affinity,
        h.spread.as_ref(),
        &count,
    )?;
    let migrate = parse_migrate(h.migrate.as_ref(), &count)?;
    let priority = match h.priority {
        None => None,
        Some(p) if (1..=100).contains(&p) => Some(p),
        Some(p) => {
            return Err(CoreError::State(format!(
                "priority {p} must be between 1 and 100; a file that names none gets {}",
                mesh_proto::DEFAULT_PRIORITY
            )));
        }
    };
    // **Landmine 1 of N2 §8, refused at the one moment it is cheap.**
    //
    // Two instances on one node cannot both bind :8080, and nothing in the
    // placement model promises they will land on different nodes — the
    // sibling spread is a preference, which on a single-node mesh puts
    // every replica on the same node. The alternative to refusing here is a
    // second instance that crash-loops on `EADDRINUSE` with a message about
    // a port its operator never typed.
    //
    // …unless `distinct_hosts` promises exactly that (Phase C): one instance
    // per machine is what makes a static port Nomad's ordinary shape.
    if ports.any_static()
        && !placement.distinct_hosts
        && let Count::Fixed(n) = count
        && n > 1
    {
        return Err(static_port_refusal(n, &ports));
    }

    let restart = parse_restart(h.restart.as_ref())?;
    let (health, health_port_label) = parse_health(h.health.as_ref(), &ports)?;
    let update = parse_update(h.update.as_ref())?;
    let mut policy = parse_policy(
        &h.name,
        &count,
        h.update.as_ref(),
        h.disconnect.as_ref(),
        created_by,
    )?;
    // Everything a block declares becomes the reserved environment the
    // instances carry — the same entries an equivalent `meshnode run`
    // would produce, which is what makes the two provably identical.
    policy.instance_env = ports.to_env();
    policy.instance_env.extend(registration);
    policy.instance_env.extend(volumes.to_env());
    // The health check's port label (N6), on the side-car rather than on
    // the check: `HealthCheck` lives inside the signed `Deployment`, and a
    // sixth field on it would invalidate every signature ever made over one.
    if let Some(label) = &health_port_label {
        policy
            .instance_env
            .push((mesh_proto::REQ_HEALTH_PORT.to_string(), label.clone()));
    }
    // `[deployment.resources]` rides the same carrier as ports and volumes:
    // the reserved `MESHWARE_TASK_*` keys the executor and the placement
    // filter already read. No wire change, and one encoding whether the
    // requirement came from a deployment, a `meshnode run --cpus`, or a
    // service definition.
    if let Some(r) = &h.resources {
        let reqs = mesh_proto::Requirements {
            cpus: r.cpus.unwrap_or(0),
            mem_mb: r.memory_mb.unwrap_or(0),
            disk_mb: r.disk_mb.unwrap_or(0),
        };
        policy.instance_env.extend(reqs.to_env());
    }
    // The placement rules ride the same carrier, so a version keeps them and
    // a rollback restores them (Phase C).
    policy.instance_env.extend(placement.to_env());
    policy.instance_env.extend(migrate.to_env());
    if let Some(p) = priority {
        policy
            .instance_env
            .push((mesh_proto::REQ_PRIORITY.to_string(), p.to_string()));
    }

    let knobs = parse_knobs(&h.name, h.update.as_ref(), &policy, created_by)?;

    let inputs = h
        .inputs
        .into_iter()
        .map(|(k, v)| (k, toml_scalar(&v)))
        .collect();

    let deployment = Deployment {
        name: h.name,
        service,
        service_version,
        count,
        targets,
        restart,
        health,
        update,
        generation: h.generation.unwrap_or(0),
        inputs,
        paused: h.paused,
        description: h.description,
        created_by,
        created_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    Ok(Parsed {
        inline,
        deployment,
        policy,
        knobs,
    })
}

/// `distinct_hosts`, `[deployment.distinct_property]`, `[[deployment.affinity]]`
/// and `[deployment.spread]` → the placement rules (Phase C, E7/E8), checked
/// while somebody is watching.
fn parse_placement(
    distinct_hosts: Option<bool>,
    distinct_property: Option<&DistinctPropertySource>,
    affinity: &[AffinitySource],
    spread: Option<&SpreadSource>,
    count: &Count,
) -> Result<mesh_proto::PlacementSpec> {
    let attribute = |what: &str, text: &str| -> Result<String> {
        let text = text.trim();
        if mesh_proto::is_valid_attribute(text) {
            Ok(text.to_string())
        } else {
            Err(CoreError::State(format!(
                concat!(
                    "{} attribute {:?} must be `name`, `tag.<key>` (the value of a ",
                    "`<key>=<value>` tag, such as tag.rack for rack=b) or `fact.<key>`"
                ),
                what, text
            )))
        }
    };
    let mut spec = mesh_proto::PlacementSpec {
        distinct_hosts: distinct_hosts.unwrap_or(false),
        ..Default::default()
    };
    if let Some(d) = distinct_property {
        let limit = d.limit.unwrap_or(1);
        if limit == 0 {
            return Err(CoreError::State(
                "distinct_property limit must be at least 1 — a limit of 0 places nothing".into(),
            ));
        }
        spec.distinct_property = Some(mesh_proto::DistinctProperty {
            attribute: attribute("distinct_property", &d.attribute)?,
            limit,
        });
    }
    if affinity.len() > mesh_proto::MAX_AFFINITIES {
        return Err(CoreError::State(format!(
            "a deployment may declare at most {} affinities (got {})",
            mesh_proto::MAX_AFFINITIES,
            affinity.len()
        )));
    }
    for a in affinity {
        Selector::check(&a.select)
            .map_err(|why| CoreError::State(format!("affinity select: {why}")))?;
        let weight = a.weight.unwrap_or(50);
        if weight == 0 || !(-100..=100).contains(&weight) {
            return Err(CoreError::State(format!(
                concat!(
                    "affinity weight {} for {:?} must be between -100 and 100 and not 0 — ",
                    "positive prefers the machines it selects, negative avoids them"
                ),
                weight, a.select
            )));
        }
        spec.affinity.push(mesh_proto::Affinity {
            select: a.select.trim().to_string(),
            weight,
        });
    }
    if let Some(s) = spread {
        let attribute = attribute("spread", &s.attribute)?;
        if let Some((value, pct)) = s.targets.iter().find(|(_, p)| **p == 0 || **p > 100) {
            return Err(CoreError::State(format!(
                "spread target {value:?} is {pct}%; each target is 1 to 100"
            )));
        }
        let named: u32 = s.targets.values().sum();
        if named > 100 {
            return Err(CoreError::State(format!(
                "spread targets add up to {named}%, more than all of the instances"
            )));
        }
        spec.spread = Some(mesh_proto::Spread {
            attribute,
            targets: s.targets.clone(),
        });
    }
    if matches!(count, Count::System) && !spec.is_empty() {
        return Err(CoreError::State(
            concat!(
                "count = \"system\" runs one instance on every matching machine, so there is ",
                "nothing to place — narrow it with `targets` rather than distinct_hosts, ",
                "distinct_property, affinity or spread"
            )
            .into(),
        ));
    }
    Ok(spec)
}

/// `[deployment.migrate]` → how instances leave a drained machine (Phase C,
/// E9). Nothing written when every value is the default.
fn parse_migrate(src: Option<&MigrateSource>, count: &Count) -> Result<mesh_proto::MigratePolicy> {
    let mut migrate = mesh_proto::MigratePolicy::default();
    let Some(s) = src else {
        return Ok(migrate);
    };
    if matches!(count, Count::System) {
        return Err(CoreError::State(
            concat!(
                "count = \"system\" instances do not move — each one's machine is its slot. ",
                "A drain stops them, or leaves them with --keep-system; drop [deployment.migrate]"
            )
            .into(),
        ));
    }
    if let Some(n) = s.max_parallel {
        if n == 0 {
            return Err(CoreError::State(
                "migrate.max_parallel must be at least 1 — 0 would move nothing and hold every \
                 drain to its deadline"
                    .into(),
            ));
        }
        migrate.max_parallel = n;
    }
    if let Some(text) = &s.min_healthy_time {
        migrate.min_healthy_secs = parse_duration_secs(text, "migrate.min_healthy_time")?;
    }
    if let Some(text) = &s.healthy_deadline {
        migrate.healthy_deadline_secs = parse_duration_secs(text, "migrate.healthy_deadline")?;
    }
    if migrate.min_healthy_secs >= migrate.healthy_deadline_secs {
        return Err(CoreError::State(format!(
            concat!(
                "migrate.min_healthy_time ({}s) must be shorter than migrate.healthy_deadline ",
                "({}s), or no replacement can prove itself in time"
            ),
            migrate.min_healthy_secs, migrate.healthy_deadline_secs
        )));
    }
    Ok(migrate)
}

/// A deployment's placement rules in one line (Phase C) — `validate`,
/// Director's inline check and `deployment show` word them the same.
pub fn placement_words(spec: &mesh_proto::PlacementSpec) -> String {
    let mut parts: Vec<String> = Vec::new();
    if spec.distinct_hosts {
        parts.push("one per machine".into());
    }
    if let Some(d) = &spec.distinct_property {
        parts.push(format!("at most {} per {}", d.limit, d.attribute));
    }
    for a in &spec.affinity {
        let verb = if a.weight > 0 { "prefers" } else { "avoids" };
        parts.push(format!("{verb} {} ({:+})", a.select, a.weight));
    }
    if let Some(s) = &spec.spread {
        if s.targets.is_empty() {
            parts.push(format!("spread evenly over {}", s.attribute));
        } else {
            let shares: Vec<String> = s.targets.iter().map(|(v, p)| format!("{v} {p}%")).collect();
            parts.push(format!(
                "spread over {} ({})",
                s.attribute,
                shares.join(", ")
            ));
        }
    }
    parts.join("; ")
}

/// `update.auto_promote` → the knobs side-car (Phase B, E2).
///
/// Only a `false` is written: the default says nothing, so a file that never
/// mentions promotion publishes no knobs at all.
fn parse_knobs(
    name: &str,
    update: Option<&UpdateSource>,
    policy: &mesh_proto::DeploymentPolicy,
    set_by: UserId,
) -> Result<mesh_proto::DeploymentKnobs> {
    let mut knobs = mesh_proto::DeploymentKnobs {
        deployment: name.to_string(),
        set_by,
        set_at_micros: mesh_proto::now_micros(),
        ..Default::default()
    };
    if update.and_then(|u| u.auto_promote) == Some(false) {
        if policy.canary == 0 {
            return Err(CoreError::State(
                concat!(
                    "auto_promote = false holds canaries for a person to promote, and this ",
                    "file has no canary — set update.canary, or drop auto_promote"
                )
                .into(),
            ));
        }
        knobs.entries.push((
            mesh_proto::KNOB_AUTO_PROMOTE.to_string(),
            "false".to_string(),
        ));
    }
    Ok(knobs)
}

/// `[[deployment.volume]]` → the volume request (N5).
fn parse_volumes(src: &[VolumeSource]) -> Result<mesh_proto::VolumeRequests> {
    if src.len() > mesh_proto::MAX_VOLUMES_PER_JOB {
        return Err(CoreError::State(format!(
            "a deployment may claim at most {} volumes (got {})",
            mesh_proto::MAX_VOLUMES_PER_JOB,
            src.len()
        )));
    }
    let mut out: Vec<mesh_proto::VolumeRequest> = Vec::with_capacity(src.len());
    for v in src {
        if !mesh_proto::volumes::is_valid_volume_name(&v.name) {
            return Err(CoreError::State(format!(
                concat!(
                    "volume name {:?} must be 1-48 letters, digits, '-' or '_', ",
                    "starting with a letter or digit — it becomes MESHWARE_VOLUME_<NAME>",
                ),
                v.name
            )));
        }
        if out.iter().any(|o| o.name == v.name) {
            return Err(CoreError::State(format!(
                "volume {:?} is claimed twice; one claim cannot be two modes",
                v.name
            )));
        }
        let mode = match v.mode.as_deref() {
            None => mesh_proto::volumes::Mode::ReadWrite,
            Some(m) => mesh_proto::volumes::Mode::parse(m).ok_or_else(|| {
                CoreError::State(format!("volume mode must be \"rw\" or \"ro\" (got {m:?})"))
            })?,
        };
        out.push(mesh_proto::VolumeRequest {
            name: v.name.clone(),
            path: v.path.clone().unwrap_or_default(),
            mode,
        });
    }
    Ok(mesh_proto::VolumeRequests(out))
}

/// `[deployment.register]` → the registration environment (N3).
///
/// Like the port request, this becomes reserved job-environment keys rather
/// than a new field on a signed record — so `meshnode run --register` and a
/// deployment ask for the same thing in the same way, and the executor has
/// one thing to read.
fn parse_registration(
    src: Option<&RegisterSource>,
    ports: &mesh_proto::PortRequests,
) -> Result<Vec<(String, String)>> {
    let Some(r) = src else {
        return Ok(Vec::new());
    };
    if !crate::registry::is_valid_service_name(&r.name) {
        return Err(CoreError::State(format!(
            "service name {:?} must be a DNS label: 1-63 letters, digits or '-', not \
             starting or ending with '-', and not all digits — it becomes {:?}",
            r.name,
            format!("{}.{}", r.name, mesh_proto::DNS_SUFFIX)
        )));
    }
    // A registration with no port is a name that resolves to nothing.
    // Caught here rather than as a warning on the executor at 03:00.
    if ports.is_empty() {
        return Err(CoreError::State(format!(
            "[deployment.register] names {:?} but the deployment declares no port — \
             add a [[deployment.network.port]] block, or there is no address to publish",
            r.name
        )));
    }
    if let Some(label) = &r.port
        && !ports.0.iter().any(|p| &p.label == label)
    {
        return Err(CoreError::State(format!(
            "[deployment.register] publishes port {label:?}, which is not declared. \
             Declared: {}",
            ports
                .0
                .iter()
                .map(|p| p.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let mut env = vec![(mesh_proto::REQ_SERVICE.to_string(), r.name.clone())];
    if !r.tags.is_empty() {
        env.push((mesh_proto::REQ_SERVICE_TAGS.to_string(), r.tags.join(",")));
    }
    if let Some(label) = &r.port {
        env.push((mesh_proto::REQ_SERVICE_PORT.to_string(), label.clone()));
    }
    if let Some(text) = &r.shutdown_delay {
        let secs = parse_duration_secs(text, "shutdown_delay")?;
        if secs > mesh_proto::MAX_SHUTDOWN_DELAY_SECS {
            return Err(CoreError::State(format!(
                "shutdown_delay is {secs}s; the ceiling is {}s. A drain window is seconds — \
                 long enough for a load balancer to notice, short enough that one stopping \
                 instance cannot hold a rolling update open",
                mesh_proto::MAX_SHUTDOWN_DELAY_SECS
            )));
        }
        env.push((mesh_proto::REQ_SHUTDOWN_DELAY.to_string(), secs.to_string()));
    }
    if let Some(text) = &r.kill_timeout {
        let secs = parse_duration_secs(text, "kill_timeout")?;
        env.push((mesh_proto::REQ_KILL_TIMEOUT.to_string(), secs.to_string()));
    }
    Ok(env)
}

/// `[deployment.network]` → the port request (N2 P2).
///
/// Returns the parsed set; the caller turns it into the reserved
/// environment entry the instances carry.
fn parse_network(src: Option<&NetworkSource>) -> Result<mesh_proto::PortRequests> {
    let Some(n) = src else {
        return Ok(mesh_proto::PortRequests::default());
    };
    match n.mode.as_deref().map(str::trim) {
        None | Some("") | Some("host") => {}
        Some(other) => {
            return Err(CoreError::State(format!(
                "network mode must be \"host\" (got {other:?}). Bridge and CNI are Linux-only \
                 even in Nomad and are a much larger project; N2 ships host networking"
            )));
        }
    }
    if n.port.len() > mesh_proto::MAX_PORTS_PER_JOB {
        return Err(CoreError::State(format!(
            "a deployment may declare at most {} ports (got {})",
            mesh_proto::MAX_PORTS_PER_JOB,
            n.port.len()
        )));
    }
    let mut out = Vec::with_capacity(n.port.len());
    for p in &n.port {
        if !mesh_proto::ports::is_valid_label(&p.label) {
            return Err(CoreError::State(format!(
                "port label {:?} must be 1-32 characters of letters, digits, '-' or '_', \
                 starting with a letter or digit — it becomes MESHWARE_PORT_<LABEL>",
                p.label
            )));
        }
        if out
            .iter()
            .any(|q: &mesh_proto::PortRequest| q.label == p.label)
        {
            return Err(CoreError::State(format!(
                "port label {:?} is declared twice",
                p.label
            )));
        }
        let fixed = p.fixed.filter(|f| *f > 0);
        out.push(mesh_proto::PortRequest {
            label: p.label.clone(),
            to: p.to.or(fixed).unwrap_or(0),
            fixed,
        });
    }
    Ok(mesh_proto::PortRequests(out))
}

/// The `[deployment.update]` and `[deployment.disconnect]` fields that
/// become the side-car record (N1 P3/P4).
///
/// Every default here is today's behaviour, so a file that says nothing
/// new produces an inert policy and the deployment reconciles exactly as
/// it always did.
fn parse_policy(
    name: &str,
    count: &Count,
    update: Option<&UpdateSource>,
    disconnect: Option<&DisconnectSource>,
    set_by: UserId,
) -> Result<mesh_proto::DeploymentPolicy> {
    let mut policy = mesh_proto::DeploymentPolicy {
        deployment: name.to_string(),
        set_by,
        set_at_micros: mesh_proto::now_micros(),
        ..Default::default()
    };
    let mut deadline_defaulted = false;
    if let Some(u) = update {
        policy.canary = u.canary.unwrap_or(0);
        policy.auto_revert = u.auto_revert.unwrap_or(false);
        match u.progress_deadline.as_deref() {
            Some(text) => {
                policy.progress_deadline_secs = parse_timer_secs(text, "progress_deadline")?;
            }
            // **Phase A, §2.4.** `auto_revert` fires only on a stall and a
            // stall needs a deadline, so a file asking for a revert with no
            // deadline was asking for something that could not happen — and
            // nothing said so. A canary set has the same hole: nothing ends
            // a proof that never finishes. Nomad's default, filled in here
            // where it can be rendered back and shown in the plan, and
            // `never` still says "no deadline" for anyone who means it.
            None if policy.auto_revert || policy.canary > 0 => {
                policy.progress_deadline_secs = mesh_proto::DEFAULT_PROGRESS_DEADLINE_SECS;
                deadline_defaulted = true;
            }
            None => {}
        }
        if let Some(text) = &u.min_healthy_time {
            policy.min_healthy_secs = parse_duration_secs(text, "min_healthy_time")?;
        }
    }
    // **Phase A, §2.1.** A fixed-count deployment whose file says nothing
    // about partitions used to get `lost_after = 0`, and zero meant *never*:
    // an instance on a node that went away and stayed away was waited for
    // forever, and the slot refilled only if that node came back. Nomad
    // replaces it in about twenty seconds, unasked. The default is written
    // into the record here rather than assumed by the reconciler, so the
    // signed side-car keeps its meaning for every node that reads it.
    //
    // A `system` slot *is* its node — there is nowhere else for it to go —
    // so it keeps the zero.
    policy.lost_after_secs = match (disconnect.and_then(|d| d.lost_after.as_deref()), count) {
        (Some(text), _) => parse_timer_secs(text, "lost_after")?,
        (None, Count::Fixed(_)) => mesh_proto::DEFAULT_LOST_AFTER_SECS,
        (None, Count::System) => 0,
    };
    if let Some(d) = disconnect {
        policy.replace_unknown = d.replace.unwrap_or(false);
        if let Some(text) = &d.reconcile {
            policy.reconcile = mesh_proto::Reconcile::parse(text).ok_or_else(|| {
                CoreError::State(format!(
                    "reconcile {text:?} is not one of keep_original, keep_replacement, \
                     best_score, longest_running"
                ))
            })?;
        }
    }
    // `min_healthy_time` longer than the deadline can never be satisfied:
    // the instance is still proving itself when the roll is failed for not
    // progressing. Caught here rather than at 03:00 on the fourth batch.
    if policy.progress_deadline_secs > 0 && policy.min_healthy_secs >= policy.progress_deadline_secs
    {
        return Err(CoreError::State(format!(
            concat!(
                "min_healthy_time ({}s) must be shorter than progress_deadline ({}s{}), ",
                "or no instance can ever be healthy before the roll is failed"
            ),
            policy.min_healthy_secs,
            policy.progress_deadline_secs,
            // A default nobody typed has to be named, or the refusal points
            // at a number that is not in the file.
            if deadline_defaulted {
                " — the default whenever auto_revert or canary is set; name a longer one"
            } else {
                ""
            },
        )));
    }
    Ok(policy)
}

fn toml_scalar(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn split_service_ref(s: &str) -> Result<(String, u32)> {
    let (name, ver) = s.split_once('@').ok_or_else(|| {
        CoreError::State(format!(
            "a deployment must pin a service version, e.g. {s}@7 — a floating head cannot be \
             what stays up, because a rollout has to be an explicit act, not a head bump \
             happening underneath the deployment"
        ))
    })?;
    let version: u32 = ver
        .trim()
        .parse()
        .map_err(|_| CoreError::State(format!("service version {ver:?} is not a number")))?;
    if name.is_empty() {
        return Err(CoreError::State("service name is empty".into()));
    }
    Ok((name.to_string(), version))
}

fn parse_count(v: Option<&toml::Value>) -> Result<Count> {
    match v {
        None => Ok(Count::Fixed(1)),
        Some(toml::Value::Integer(n)) if *n >= 1 => Ok(Count::Fixed(*n as u32)),
        Some(toml::Value::Integer(n)) => Err(CoreError::State(format!(
            "count must be at least 1 (got {n}); a deployment of zero instances is a removal"
        ))),
        Some(toml::Value::String(s)) if s == "system" => Ok(Count::System),
        Some(toml::Value::String(s)) => {
            // Allow a stringified integer, since JSON/YAML may hand one over.
            if let Ok(n) = s.parse::<u32>()
                && n >= 1
            {
                return Ok(Count::Fixed(n));
            }
            Err(CoreError::State(format!(
                "count must be an integer or \"system\" (got {s:?})"
            )))
        }
        Some(other) => Err(CoreError::State(format!(
            "count must be an integer or \"system\" (got {other})"
        ))),
    }
}

fn parse_restart(src: Option<&RestartSource>) -> Result<RestartPolicy> {
    let mut policy = RestartPolicy::default();
    let Some(src) = src else { return Ok(policy) };
    if let Some(backoff) = &src.backoff {
        let (min, max) = backoff.split_once("..").ok_or_else(|| {
            CoreError::State(format!(
                "backoff must be MIN..MAX, e.g. 1s..5m (got {backoff:?})"
            ))
        })?;
        policy.backoff_min_secs = parse_duration_secs(min, "backoff min")?;
        policy.backoff_max_secs = parse_duration_secs(max, "backoff max")?;
        if policy.backoff_max_secs < policy.backoff_min_secs {
            return Err(CoreError::State(
                "backoff max must be at least backoff min".into(),
            ));
        }
    }
    if let Some(budget) = &src.budget {
        // `5 in 10m`
        let (n, window) = budget.split_once(" in ").ok_or_else(|| {
            CoreError::State(format!(
                "budget must be `N in WINDOW`, e.g. 5 in 10m (got {budget:?})"
            ))
        })?;
        policy.budget_failures = n
            .trim()
            .parse()
            .map_err(|_| CoreError::State(format!("budget count {n:?} is not a number")))?;
        policy.budget_window_secs = parse_duration_secs(window, "budget window")?;
        if policy.budget_failures == 0 {
            return Err(CoreError::State(
                "a budget of 0 failures means the service may never restart at all".into(),
            ));
        }
    }
    Ok(policy)
}

/// A health check's port, as written.
enum CheckPort {
    Literal(u16),
    /// The label of a declared port, resolved on the executor once the
    /// node has allocated it.
    Label(String),
    Unset,
}

fn read_check_port(v: Option<&toml::Value>) -> Result<CheckPort> {
    let Some(v) = v else {
        return Ok(CheckPort::Unset);
    };
    if let Some(n) = v.as_integer() {
        return u16::try_from(n)
            .map(CheckPort::Literal)
            .map_err(|_| CoreError::State(format!("health port {n} is not a port number")));
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        // A quoted number is still a number — YAML and TOML disagree about
        // when `80` is a string, and an operator means the same thing both
        // ways.
        if let Ok(n) = s.parse::<u16>() {
            return Ok(CheckPort::Literal(n));
        }
        if !mesh_proto::ports::is_valid_label(s) {
            return Err(CoreError::State(format!(
                "health port {s:?} is neither a port number nor a usable port label"
            )));
        }
        return Ok(CheckPort::Label(s.to_string()));
    }
    Err(CoreError::State(
        "health `port` must be a number or a port label like \"http\"".into(),
    ))
}

/// `(check, port label)` — the label is `None` for a literal port, and is
/// carried to the executor on the policy side-car.
fn parse_health(
    src: Option<&HealthSource>,
    ports: &mesh_proto::PortRequests,
) -> Result<(Option<HealthCheck>, Option<String>)> {
    let Some(src) = src else {
        return Ok((None, None));
    };
    let kind = HealthKind::parse(&src.kind).ok_or_else(|| {
        CoreError::State(format!(
            "health kind {:?} must be exec, tcp or http",
            src.kind
        ))
    })?;
    let mut hc = HealthCheck {
        kind,
        ..HealthCheck::default()
    };
    // `port = 0` in the signed record means *resolve it on the executor*;
    // the label travels beside it.
    let mut label: Option<String> = None;
    match kind {
        HealthKind::Exec => {
            hc.command = src
                .command
                .clone()
                .ok_or_else(|| CoreError::State("an exec health check needs a `command`".into()))?;
        }
        HealthKind::Tcp | HealthKind::Http => {
            match read_check_port(src.port.as_ref())? {
                CheckPort::Literal(p) if p > 0 => hc.port = p,
                CheckPort::Literal(_) => {
                    return Err(CoreError::State(format!(
                        "a {} health check needs a `port`",
                        kind.as_str()
                    )));
                }
                CheckPort::Label(l) => {
                    if !ports.0.iter().any(|p| p.label == l) {
                        return Err(CoreError::State(format!(
                            "health check names port {l:?}, which this deployment does not \
                             declare. Add it under [deployment.network], or name a literal \
                             port number"
                        )));
                    }
                    label = Some(l);
                }
                // Nothing said, and exactly one port declared: that one.
                // Anything else has to be named, because guessing which of
                // three ports is "the" one is how a check silently probes
                // the wrong thing forever.
                CheckPort::Unset => match ports.0.len() {
                    1 => label = Some(ports.0[0].label.clone()),
                    0 => {
                        return Err(CoreError::State(format!(
                            "a {} health check needs a `port` — either a number, or the \
                             label of a port this deployment declares under \
                             [deployment.network]",
                            kind.as_str()
                        )));
                    }
                    n => {
                        let labels: Vec<&str> = ports.0.iter().map(|p| p.label.as_str()).collect();
                        return Err(CoreError::State(format!(
                            "this deployment declares {n} ports ({}), so the health check \
                             has to say which one to probe: `port = \"{}\"`",
                            labels.join(", "),
                            labels[0]
                        )));
                    }
                },
            }
            if kind == HealthKind::Http {
                hc.path = src.path.clone().unwrap_or_else(|| "/".to_string());
            }
        }
    }
    if let Some(i) = &src.interval {
        hc.interval_secs = parse_duration_secs(i, "health interval")?;
    }
    if let Some(g) = &src.grace {
        hc.grace_secs = parse_duration_secs(g, "health grace")?;
    }
    if let Some(t) = &src.timeout {
        hc.timeout_secs = parse_duration_secs(t, "health timeout")?;
    }
    if let Some(f) = src.failures {
        hc.failures_to_unhealthy = f.max(1);
    }
    if hc.interval_secs == 0 {
        return Err(CoreError::State("health interval must be non-zero".into()));
    }
    Ok((Some(hc), label))
}

fn parse_update(src: Option<&UpdateSource>) -> Result<UpdatePolicy> {
    let mut policy = UpdatePolicy::default();
    let Some(src) = src else { return Ok(policy) };
    if let Some(strategy) = &src.strategy {
        if strategy != "rolling" {
            return Err(CoreError::State(format!(
                "update strategy {strategy:?} is not supported; only `rolling` exists in C2"
            )));
        }
        policy.strategy = UpdateStrategy::Rolling;
    }
    if let Some(mu) = src.max_unavailable {
        policy.max_unavailable = mu.max(1);
    }
    if let Some(gate) = &src.gate {
        policy.gate_on_health = gate == "health";
    }
    Ok(policy)
}

/// A timer field — `lost_after`, `progress_deadline` — which is a duration
/// or the word `never` (Phase A).
///
/// Its own function rather than a `never` arm in [`parse_duration_secs`],
/// because a restart backoff or a health interval has no "never" and must
/// keep refusing the word.
fn parse_timer_secs(text: &str, what: &str) -> Result<u64> {
    if text.trim().eq_ignore_ascii_case(mesh_proto::NEVER) {
        return Ok(0);
    }
    parse_duration_secs(text, what)
}

fn parse_duration_secs(text: &str, what: &str) -> Result<u64> {
    humantime::parse_duration(text.trim())
        .map(|d| d.as_secs())
        .map_err(|e| CoreError::State(format!("{what} {text:?} is not a duration: {e}")))
}

// ---------------------------------------------------------------------------
// Validation against the mesh
// ---------------------------------------------------------------------------

/// One problem with a deployment, judged against replicated state.
#[derive(Debug, Clone)]
pub struct Issue {
    pub message: String,
}

/// Everything wrong with a deployment that only the mesh can see: a service
/// version that is not published, a selector that matches nothing, a count
/// bigger than the pool. Surfaced at create time (`--dry-run`), where a
/// person is watching.
pub fn validate(state: &MeshState, dep: &Deployment) -> Vec<Issue> {
    let mut issues = Vec::new();
    match state.services.get(&dep.service) {
        None => issues.push(Issue {
            // **Names the fix, not just the fault.** This is the first
            // refusal anybody hits — a deployment names a service and a
            // pasted example names one you have not created — and "no
            // service named X is published" left a reader with nowhere to
            // go. It now says where services come from, and lists the ones
            // that exist, because the real mistake is usually a typo or a
            // half-finished setup rather than a missing concept.
            message: format!(
                "no service named {:?} is published. A deployment names a service; \
                 it does not carry the image or the script itself. Publish one first \
                 — Scripts → New service in Director, or `meshnode service create`{}",
                dep.service,
                published_hint(state)
            ),
        }),
        Some(entry) if entry.removed => issues.push(Issue {
            message: format!(
                "service {:?} has been retired. Publish it again to bring it back, \
                 or point this deployment at a service that is still live{}",
                dep.service,
                published_hint(state)
            ),
        }),
        Some(entry) if !entry.versions.contains_key(&dep.service_version) => issues.push(Issue {
            // The one refusal that can simply answer the question it
            // raises: the versions are right here.
            message: format!(
                "service {} has no version {}. Published: {}",
                dep.service,
                dep.service_version,
                {
                    let mut v: Vec<String> = entry.versions.keys().map(|n| n.to_string()).collect();
                    v.sort_by_key(|n| n.parse::<u32>().unwrap_or(0));
                    if v.is_empty() {
                        "none yet".to_string()
                    } else {
                        v.join(", ")
                    }
                }
            ),
        }),
        Some(_) => {}
    }
    issues
}

/// `" (published: a, b, c)"`, or an empty string when the mesh has none.
///
/// Bounded at six names: the point is to catch a typo, and a reader
/// scanning two hundred service names for theirs is being given a haystack
/// rather than a hint.
pub(crate) fn published_hint(state: &MeshState) -> String {
    let mut names: Vec<&str> = state
        .services
        .iter()
        .filter(|(_, e)| !e.removed)
        .map(|(n, _)| n.as_str())
        .collect();
    if names.is_empty() {
        return ". This mesh has no services yet".to_string();
    }
    names.sort_unstable();
    let total = names.len();
    names.truncate(6);
    if total > 6 {
        format!(
            ". Published here: {} and {} more",
            names.join(", "),
            total - 6
        )
    } else {
        format!(". Published here: {}", names.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Signing and publication
// ---------------------------------------------------------------------------

pub fn sign(user: &Keypair, mut dep: Deployment) -> Deployment {
    dep.created_by = user.user_id();
    dep.signature = Vec::new();
    dep.signature = user.sign(&dep.signing_bytes()).to_vec();
    dep
}

pub fn sign_remove(user: &Keypair, name: &str) -> Vec<u8> {
    user.sign(&mesh_proto::deployment_remove_signing_bytes(
        name,
        &user.user_id(),
    ))
    .to_vec()
}

pub fn put(engine: &Arc<StateEngine>, dep: Deployment) -> Result<()> {
    engine.author_op(OpBody::DeploymentPut {
        deployment: Box::new(dep),
    })?;
    Ok(())
}

/// Sign the side-car policy (N1 P3/P4).
///
/// Its own signature over its own domain-tagged bytes, so it can be edited
/// without re-signing the deployment and a deployment can be re-signed
/// without touching it.
pub fn sign_policy(
    user: &Keypair,
    mut policy: mesh_proto::DeploymentPolicy,
) -> mesh_proto::DeploymentPolicy {
    policy.set_by = user.user_id();
    policy.set_at_micros = mesh_proto::now_micros();
    policy.signature = Vec::new();
    policy.signature = user.sign(&policy.signing_bytes()).to_vec();
    policy
}

/// Publish the policy beside the deployment.
///
/// **An inert policy still authors an op**, and that is deliberate:
/// deleting a `[deployment.update]` block from the file has to be able to
/// *lift* a canary, and a put that skipped the write would leave the old
/// policy standing while the file said otherwise. The op is one small LWW
/// record per edit and compaction keys it by name.
pub fn put_policy(engine: &Arc<StateEngine>, policy: mesh_proto::DeploymentPolicy) -> Result<()> {
    engine.author_op(OpBody::DeploymentPolicyPut {
        policy: Box::new(policy),
    })?;
    Ok(())
}

/// Live members that will **not** apply a deployment policy, with the
/// minor they speak (N1 P3/P4, cross-cutting landmine 8).
///
/// The `presence::readiness` shape, and for the same reason: a capability
/// minor gates in two directions and both can be wrong, so the only useful
/// report is *which machines*, by name. Here the failure is the
/// door-left-open one — a node below [`mesh_proto::DEPLOY_POLICY_MINOR`]
/// carries the policy op unread and rolls the deployment the old way, with
/// no canary and no progress deadline — and it is invisible from the roll
/// itself, because a roll without a canary looks exactly like a roll.
///
/// `None` for a minor means this node has never negotiated with that peer,
/// which for this purpose is the same answer: not known to apply it is not
/// applying it.
pub fn policy_readiness(
    state: &MeshState,
    self_id: NodeId,
    needed: u16,
    minor_of: impl Fn(&NodeId) -> Option<u16>,
) -> Vec<(NodeId, String, Option<u16>)> {
    let mut behind = Vec::new();
    for entry in state.members.values() {
        if entry.left || state.revoked.contains_key(&entry.info.node_id) {
            continue;
        }
        let id = entry.info.node_id;
        // This node speaks the current minor by construction — it is this
        // build — and `minor_of` answers `None` about ourselves.
        let minor = if id == self_id {
            Some(mesh_proto::PROTOCOL_MINOR)
        } else {
            minor_of(&id)
        };
        if !minor.is_some_and(|m| m >= needed) {
            behind.push((id, entry.info.name.clone(), minor));
        }
    }
    behind
}

/// The policy in force for a deployment, or the inert default.
///
/// One lookup, so no caller has to remember that an absent record means
/// "today's behaviour" rather than "zero of everything".
/// Record a published version: the record and its side-car together
/// (Phase B, E1). Authored after both, so it never names a publish that
/// did not happen.
pub fn put_version(
    engine: &Arc<StateEngine>,
    version: mesh_proto::DeploymentVersion,
) -> Result<()> {
    engine.author_op(OpBody::DeploymentVersionPut {
        version: Box::new(version),
    })?;
    Ok(())
}

/// Sign a tag (Phase B, E1).
pub fn sign_tag(user: &Keypair, mut tag: mesh_proto::DeploymentTag) -> mesh_proto::DeploymentTag {
    tag.set_by = user.user_id();
    tag.set_at_micros = mesh_proto::now_micros();
    tag.signature = Vec::new();
    tag.signature = user.sign(&tag.signing_bytes()).to_vec();
    tag
}

/// Put or remove a tag (Phase B, E1), refused here by name for the reason
/// [`remove`] gives: a tag on a typo would otherwise surface as an op the
/// engine rejected.
pub fn put_tag(engine: &Arc<StateEngine>, tag: mesh_proto::DeploymentTag) -> Result<()> {
    if !mesh_proto::is_valid_tag_name(&tag.tag) {
        return Err(CoreError::State(format!(
            "{:?} is not a tag: letters, digits, '.', '-' and '_', up to 64 of them",
            tag.tag
        )));
    }
    if tag
        .version
        .as_ref()
        .is_some_and(|v| v.deployment.name != tag.deployment)
    {
        return Err(CoreError::State(format!(
            "the version being tagged is not one of {}'s",
            tag.deployment
        )));
    }
    let missing = engine.read(|s| {
        (!s.deployments
            .get(&tag.deployment)
            .is_some_and(|e| !e.removed))
        .then(|| deployment_hint(s))
    });
    if let Some(hint) = missing {
        return Err(CoreError::State(format!(
            "no deployment named {:?}{hint}",
            tag.deployment
        )));
    }
    if tag.version.is_none() {
        let known = engine.read(|s| {
            s.deployment_history
                .get(&tag.deployment)
                .and_then(|h| h.tags.get(&tag.tag))
                .is_some_and(|t| t.tag.version.is_some())
        });
        if !known {
            return Err(CoreError::State(format!(
                "{} has no tag {:?}",
                tag.deployment, tag.tag
            )));
        }
    }
    engine.author_op(OpBody::DeploymentTagPut { tag: Box::new(tag) })?;
    Ok(())
}

/// `deployment history`: the versions a deployment keeps, newest first, each
/// with what changed from the one before it (Phase B, E1).
///
/// The last [`mesh_proto::DEPLOYMENT_HISTORY_KEEP`], and any older version a
/// tag names, listed from the tag's own copy. `None` for a name with no live
/// deployment.
pub fn history(
    state: &MeshState,
    founder: &[u8; 32],
    name: &str,
) -> Option<mesh_proto::DeploymentHistoryView> {
    let entry = state.deployments.get(name).filter(|e| !e.removed)?;
    let current = &entry.deployment;
    let kept = state.deployment_history.get(name);
    // generation → (version, published at, listed only because of a tag)
    let mut versions: BTreeMap<u32, (mesh_proto::DeploymentVersion, u64, bool)> = kept
        .map(|h| {
            h.versions
                .iter()
                .map(|(g, (v, at))| (*g, (v.clone(), at.wall_micros, false)))
                .collect()
        })
        .unwrap_or_default();
    // A deployment last published before its node recorded versions has none
    // yet, and what it runs now is still one.
    versions.entry(current.generation).or_insert_with(|| {
        (
            mesh_proto::DeploymentVersion {
                deployment: current.clone(),
                policy: policy_for(state, name),
            },
            entry.updated_hlc.wall_micros,
            false,
        )
    });
    let mut tags = Vec::new();
    let mut tags_at: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (label, t) in kept.map(|h| &h.tags).into_iter().flatten() {
        let Some(version) = &t.tag.version else {
            continue;
        };
        let generation = version.deployment.generation;
        tags_at.entry(generation).or_default().push(label.clone());
        versions
            .entry(generation)
            .or_insert_with(|| (version.clone(), version.deployment.created_at_micros, true));
        tags.push(mesh_proto::DeploymentTagRow {
            tag: label.clone(),
            generation,
            description: t.tag.description.clone(),
            set_by_username: crate::rbac::username_of(state, founder, &t.tag.set_by),
            set_at_micros: t.tag.set_at_micros,
            version: version.clone(),
        });
    }
    let newest_first: Vec<_> = versions.into_iter().rev().collect();
    let rows = newest_first
        .iter()
        .enumerate()
        .map(|(i, (generation, (v, at, by_tag)))| {
            let changes = newest_first
                .get(i + 1)
                .map(|(_, (older, _, _))| {
                    plan_changes(&older.deployment, &older.policy, &v.deployment, &v.policy)
                })
                .unwrap_or_default()
                .into_iter()
                .filter(|c| c.field != "generation")
                .collect();
            mesh_proto::DeploymentVersionRow {
                generation: *generation,
                published_by_username: crate::rbac::username_of(
                    state,
                    founder,
                    &v.deployment.created_by,
                ),
                published_at_micros: *at,
                tags: tags_at.get(generation).cloned().unwrap_or_default(),
                current: *generation == current.generation,
                kept_by_tag: *by_tag,
                changes,
                source: to_toml(&v.deployment, &v.policy),
                version: v.clone(),
            }
        })
        .collect();
    Some(mesh_proto::DeploymentHistoryView {
        deployment: name.to_string(),
        versions: rows,
        tags,
    })
}

/// The version `--generation` or `--tag` names, out of a history (Phase B,
/// E1). Neither means the current one.
pub fn find_version(
    history: &mesh_proto::DeploymentHistoryView,
    generation: Option<u32>,
    tag: Option<&str>,
) -> Result<mesh_proto::DeploymentVersion> {
    let name = &history.deployment;
    if let Some(tag) = tag {
        return history
            .tags
            .iter()
            .find(|t| t.tag == tag)
            .map(|t| t.version.clone())
            .ok_or_else(|| {
                CoreError::State(format!(
                    "{name} has no tag {tag:?} — `meshnode deployment history {name}` lists its tags"
                ))
            });
    }
    let found = match generation {
        Some(g) => history.versions.iter().find(|v| v.generation == g),
        None => history.versions.iter().find(|v| v.current),
    };
    found.map(|v| v.version.clone()).ok_or_else(|| {
        CoreError::State(format!(
            concat!(
                "{0} does not keep generation {1}: it keeps its last {2} versions and any ",
                "a tag names — `meshnode deployment history {0}` lists them"
            ),
            name,
            generation.unwrap_or_default(),
            mesh_proto::DEPLOYMENT_HISTORY_KEEP
        ))
    })
}

/// Point a deployment back at one of its versions (Phase B, E1) —
/// `deployment rollback --generation` and `--tag`, and the Director's
/// "Roll back to this".
///
/// **The whole file comes back**, not only the service version: the record
/// and its side-car, so ports, resources and rollout knobs return with the
/// image they were published with. Two things stay as they are now — the
/// name, and whether it is paused, because a rollback that quietly resumed
/// a paused deployment would start work somebody stopped.
///
/// The generation moves on only when the difference replaces instances
/// (the [`rolls`] rule): going back to a version that differed only in its
/// count is a resize, and one that differed in its description is an edit.
pub fn revert_to(parsed: &mut Parsed, version: &mesh_proto::DeploymentVersion) -> Result<()> {
    let current = &parsed.deployment;
    let mut next = version.deployment.clone();
    next.name = current.name.clone();
    next.paused = current.paused;
    let mut policy = version.policy.clone();
    policy.deployment = current.name.clone();
    let unchanged = plan_changes(current, &parsed.policy, &next, &policy)
        .iter()
        .all(|c| c.field == "generation");
    if unchanged {
        return Err(CoreError::State(format!(
            "{} already matches generation {}",
            current.name, version.deployment.generation
        )));
    }
    next.generation = if rolls(
        current,
        &parsed.policy.instance_env,
        &next,
        &policy.instance_env,
    ) {
        current.generation.saturating_add(1)
    } else {
        current.generation
    };
    parsed.deployment = next;
    parsed.policy = policy;
    Ok(())
}

/// A deployment's knobs, or every default (Phase B, E2).
pub fn knobs_for(state: &MeshState, name: &str) -> mesh_proto::DeploymentKnobs {
    state
        .deployment_knobs
        .get(name)
        .map(|e| e.knobs.clone())
        .unwrap_or_else(|| mesh_proto::DeploymentKnobs {
            deployment: name.to_string(),
            ..Default::default()
        })
}

/// May this deployment's canaries cut over once they settle (Phase B, E2)?
/// Always, unless its knobs turned `auto_promote` off; then only once
/// somebody has promoted the generation rolling now.
pub fn promoted(state: &MeshState, dep: &Deployment) -> bool {
    let manual = state
        .deployment_knobs
        .get(&dep.name)
        .is_some_and(|k| !k.knobs.auto_promote());
    !manual
        || state
            .deployment_promotions
            .get(&dep.name)
            .is_some_and(|p| p.promotion.generation == dep.generation)
}

pub fn sign_knobs(
    user: &Keypair,
    mut knobs: mesh_proto::DeploymentKnobs,
) -> mesh_proto::DeploymentKnobs {
    knobs.set_by = user.user_id();
    knobs.set_at_micros = mesh_proto::now_micros();
    knobs.signature = Vec::new();
    knobs.signature = user.sign(&knobs.signing_bytes()).to_vec();
    knobs
}

/// Publish a file's knobs when they say something the mesh does not already
/// hold (Phase B, E2). `Ok(false)` when nothing needed authoring — the
/// common case, a file that never mentions them publishing over one that
/// never did.
pub fn put_knobs(engine: &Arc<StateEngine>, knobs: mesh_proto::DeploymentKnobs) -> Result<bool> {
    let same = engine.read(|s| match s.deployment_knobs.get(&knobs.deployment) {
        Some(cur) => cur.knobs.entries == knobs.entries,
        None => knobs.entries.is_empty(),
    });
    if same {
        return Ok(false);
    }
    engine.author_op(OpBody::DeploymentKnobsPut {
        knobs: Box::new(knobs),
    })?;
    Ok(true)
}

pub fn sign_promotion(
    user: &Keypair,
    deployment: &str,
    generation: u32,
) -> mesh_proto::DeploymentPromotion {
    let mut promotion = mesh_proto::DeploymentPromotion {
        deployment: deployment.to_string(),
        generation,
        by: user.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    promotion.signature = user.sign(&promotion.signing_bytes()).to_vec();
    promotion
}

/// Promote the canaries rolling now (Phase B, E2), refused by name when the
/// promotion could not mean anything: another generation, a deployment that
/// promotes itself, or one already promoted.
pub fn promote(
    engine: &Arc<StateEngine>,
    promotion: mesh_proto::DeploymentPromotion,
) -> Result<()> {
    let refusal = engine.read(|s| {
        let Some(entry) = s
            .deployments
            .get(&promotion.deployment)
            .filter(|e| !e.removed)
        else {
            return Some(format!(
                "no deployment named {:?}{}",
                promotion.deployment,
                deployment_hint(s)
            ));
        };
        let dep = &entry.deployment;
        if dep.generation != promotion.generation {
            return Some(format!(
                "{} is at generation {} now, not {}",
                dep.name, dep.generation, promotion.generation
            ));
        }
        if knobs_for(s, &dep.name).auto_promote() {
            return Some(format!(
                concat!(
                    "{} promotes its canaries itself once they settle — set ",
                    "update.auto_promote = false to hold them for a person"
                ),
                dep.name
            ));
        }
        if promoted(s, dep) {
            return Some(format!(
                "generation {} of {} is already promoted",
                dep.generation, dep.name
            ));
        }
        None
    });
    if let Some(why) = refusal {
        return Err(CoreError::State(why));
    }
    engine.author_op(OpBody::DeploymentPromote {
        promotion: Box::new(promotion),
    })?;
    Ok(())
}

/// [`to_toml`] with the knobs that are not the default (Phase B, E2) — what
/// `show` renders, so a definition read back for a scale or an edit and
/// published again keeps them.
pub fn to_toml_with(
    dep: &Deployment,
    policy: &mesh_proto::DeploymentPolicy,
    knobs: &mesh_proto::DeploymentKnobs,
) -> String {
    let out = to_toml(dep, policy);
    if knobs.auto_promote() {
        return out;
    }
    let line = "auto_promote    = false";
    let mut lines: Vec<&str> = out.lines().collect();
    match lines.iter().position(|l| *l == "[deployment.update]") {
        Some(start) => {
            let mut at = start + 1;
            while at < lines.len() && !lines[at].is_empty() && !lines[at].starts_with('[') {
                at += 1;
            }
            lines.insert(at, line);
            let mut s = lines.join("\n");
            s.push('\n');
            s
        }
        None => format!("{out}\n[deployment.update]\n{line}\n"),
    }
}

// ---------------------------------------------------------------------------
// Placement that cannot get stuck (Phase B hardening)
// ---------------------------------------------------------------------------

/// The runtime a deployment's instances need: its service's, or a container
/// for a file with an inline image that has not been published yet (T3).
pub fn runtime_for(
    state: &MeshState,
    dep: &Deployment,
    inline_container: bool,
) -> Option<mesh_proto::Runtime> {
    state
        .services
        .get(&dep.service)
        .and_then(|e| e.versions.get(&dep.service_version))
        .map(|def| def.runtime)
        .or(inline_container.then_some(mesh_proto::Runtime::Container))
}

/// Everything an instance asks a machine for, in the order admission reads
/// it: the service definition's environment, then the deployment's own
/// side-car — where `[deployment.resources]`, ports and volumes live, and
/// which wins where both say something.
///
/// Reading only the service's half is how a deployment asking for four
/// cores was explained, and planned, as though it asked for nothing.
pub fn instance_request_env(state: &MeshState, dep: &Deployment) -> Vec<(String, String)> {
    let mut env = state
        .services
        .get(&dep.service)
        .and_then(|e| e.versions.get(&dep.service_version))
        .map(|def| def.env.clone())
        .unwrap_or_default();
    env.extend(policy_for(state, &dep.name).instance_env);
    env
}

/// The machines that could ever run this deployment's instances.
///
/// Every gate here is one waiting does not change: the selector, liveness,
/// the owner's veto, `no-execute` and `draining`, a machine too small for
/// what an instance asks, no port range when ports are wanted, a volume
/// nobody lends there, devices, the runtime. **Load is not one of them**: a
/// busy machine is settled at admission, which refuses, and the start
/// spills to the next machine.
///
/// **The bug this shape fixes.** Only the runtime used to be filtered, on
/// the theory that everything else is settled at admission and spills. A
/// deployment's start never spilled, and a machine with fewer cores than an
/// instance asks for is not busy — it is small. The planner, whose choice is
/// a deterministic hash, sent the same slot to the same 2-core machine every
/// five seconds for as long as the deployment existed, and the slot read
/// `absent` with no reason given anywhere.
///
/// An empty pool places nothing, and `status` explains why through the
/// placement refusal, which reads the same request.
pub fn placement_pool(
    state: &MeshState,
    dep: &Deployment,
    self_id: NodeId,
    liveness_of: impl Fn(&NodeId) -> mesh_proto::Liveness,
    inline_container: bool,
) -> Vec<MemberInfo> {
    let env = instance_request_env(state, dep);
    let reqs = mesh_proto::Requirements::from_env(&env);
    let devices = mesh_proto::DeviceRequest::from_env(&env);
    let ports = mesh_proto::PortRequests::from_env(&env);
    let volumes = mesh_proto::VolumeRequests::from_env(&env);
    crate::membership::audit_feasible_full(
        state,
        &Selector::parse(&dep.targets),
        self_id,
        liveness_of,
        &reqs,
        devices.as_ref(),
        &ports,
        &volumes,
        runtime_for(state, dep, inline_container),
        // Capacity, never load: unknown pressure is neutral, so this asks
        // only the permanent question.
        |_| None,
    )
    .0
}

/// What the elected reconciler has learned about placing instances that
/// replicated state cannot tell it — and must not, because it is about the
/// last minute on one node.
#[derive(Debug, Clone, Default)]
pub struct PlacementHints {
    /// Machines that refused this instance recently. Skipped the way a
    /// machine the slot has failed on is, because an admission refusal
    /// leaves no job record to learn it from.
    pub avoid: BTreeMap<JobId, Vec<NodeId>>,
    /// The machine that accepted this instance — or may have — before its
    /// record reached the reconciler. The start stays there and never
    /// spills: sending one job id to a second machine is how an instance
    /// runs twice.
    pub pinned: BTreeMap<JobId, NodeId>,
    /// The reconciling node's `[compute] placement` (Phase C, E8): instances
    /// pack or spread the way a `run` from that node does. Planning without
    /// hints spreads.
    pub fit: mesh_proto::FitPolicy,
}

/// The reconciler's short memory of placing instances, from which
/// [`PlacementHints`] are drawn each tick.
///
/// Local on purpose. A reconciler that takes over starts without it, and
/// pays for that with at most one refusal per instance.
#[derive(Debug, Default)]
pub struct PlacementMemory {
    refused: BTreeMap<JobId, Vec<(NodeId, u64)>>,
    placed: BTreeMap<JobId, (NodeId, u64)>,
}

impl PlacementMemory {
    /// How long a refusal steers placement away from a machine: long enough
    /// that a busy machine is not asked every tick, short enough that one
    /// which freed up is used again within the minute.
    pub const REFUSAL_TTL_MICROS: u64 = 60_000_000;
    /// How long an accepted start stays pinned while its record travels.
    pub const PLACED_TTL_MICROS: u64 = 120_000_000;

    pub fn refused(&mut self, job: JobId, node: NodeId, now: u64) {
        let list = self.refused.entry(job).or_default();
        list.retain(|(n, _)| *n != node);
        list.push((node, now));
        // A machine that refuses the instance it was pinned to never had it.
        if self.placed.get(&job).is_some_and(|(n, _)| *n == node) {
            self.placed.remove(&job);
        }
    }

    pub fn placed(&mut self, job: JobId, node: NodeId, now: u64) {
        self.placed.insert(job, (node, now));
    }

    /// Forget what has expired, and everything about an instance whose
    /// record has arrived — from then on, state is the evidence.
    pub fn prune(&mut self, now: u64, recorded: impl Fn(&JobId) -> bool) {
        self.refused.retain(|id, list| {
            list.retain(|(_, at)| now.saturating_sub(*at) < Self::REFUSAL_TTL_MICROS);
            !list.is_empty() && !recorded(id)
        });
        self.placed.retain(|id, (_, at)| {
            now.saturating_sub(*at) < Self::PLACED_TTL_MICROS && !recorded(id)
        });
    }

    pub fn hints(&self, now: u64) -> PlacementHints {
        PlacementHints {
            fit: mesh_proto::FitPolicy::default(),
            avoid: self
                .refused
                .iter()
                .map(|(id, list)| {
                    let nodes: Vec<NodeId> = list
                        .iter()
                        .filter(|(_, at)| now.saturating_sub(*at) < Self::REFUSAL_TTL_MICROS)
                        .map(|(n, _)| *n)
                        .collect();
                    (*id, nodes)
                })
                .filter(|(_, nodes)| !nodes.is_empty())
                .collect(),
            pinned: self
                .placed
                .iter()
                .filter(|(_, (_, at))| now.saturating_sub(*at) < Self::PLACED_TTL_MICROS)
                .map(|(id, (n, _))| (*id, *n))
                .collect(),
        }
    }
}

/// The audit action that says why an instance could not be placed.
pub const UNPLACED_ACTION: &str = "deployment.unplaced";
/// An identical reason is recorded at most this often: the log replicates to
/// every node and is never compacted.
pub const UNPLACED_DEDUP_MICROS: u64 = 30 * 60 * 1_000_000;
/// How recent a reason must be for `status` to show it beside a slot.
const UNPLACED_SHOWN_MICROS: u64 = 45 * 60 * 1_000_000;

/// `generation 3 slot 1: ` — the prefix a reason for one slot is filed under.
pub fn unplaced_prefix(generation: u32, slot_label: &str) -> String {
    format!("generation {generation} slot {slot_label}: ")
}

/// Why a slot has no instance: the newest recent `deployment.unplaced` row
/// for it, scoped to this generation and to after the name was last removed.
/// Replicated, so every node's `status` says what the reconciler learned.
fn unplaced_reason(dep: &Deployment, state: &MeshState, slot: &Slot) -> Option<String> {
    let prefix = unplaced_prefix(dep.generation, &slot.label(state));
    let floor = state
        .deployment_history
        .get(&dep.name)
        .map(|h| h.floor.wall_micros)
        .unwrap_or(0);
    let now = mesh_proto::now_micros();
    state
        .audit
        .values()
        .rev()
        .take(500)
        .find(|e| {
            e.action == UNPLACED_ACTION
                && e.target == dep.name
                && e.at_micros > floor
                && now.saturating_sub(e.at_micros) < UNPLACED_SHOWN_MICROS
                && e.detail.starts_with(&prefix)
        })
        .map(|e| format!("not placed — {}", &e.detail[prefix.len()..]))
}

pub fn policy_for(state: &MeshState, name: &str) -> mesh_proto::DeploymentPolicy {
    state
        .deployment_policies
        .get(name)
        .map(|e| e.policy.clone())
        .unwrap_or_default()
}

pub fn remove(engine: &Arc<StateEngine>, name: &str, by: UserId, signature: Vec<u8>) -> Result<()> {
    // **Refused here, by name, rather than by the merge.** A removal of a
    // deployment that does not exist is rejected downstream as an op whose
    // subject is missing, and the engine reports that the only way it can:
    // *locally authored op was rejected — this is a bug*. Which is a true
    // sentence about the op layer and a useless one for the person who
    // typed a name slightly wrong, and it accuses the software of a defect
    // over what is ordinarily a typo.
    let missing = engine.read(|s| (!s.deployments.contains_key(name)).then(|| deployment_hint(s)));
    if let Some(hint) = missing {
        return Err(CoreError::State(format!(
            "no deployment named {name:?}{hint}"
        )));
    }
    engine.author_op(OpBody::DeploymentRemove {
        name: name.to_string(),
        by,
        signature,
    })?;
    Ok(())
}

/// The deployments that do exist, for a refusal naming one that does not.
///
/// [`published_hint`]'s shape for the other noun, and bounded for the same
/// reason: the point is to catch a typo, and a reader scanning two hundred
/// names for theirs has been given a haystack rather than a hint.
fn deployment_hint(state: &MeshState) -> String {
    let mut names: Vec<&str> = state.deployments.keys().map(String::as_str).collect();
    if names.is_empty() {
        return ". This mesh has no deployments yet".to_string();
    }
    names.sort_unstable();
    const MAX: usize = 6;
    let more = names.len().saturating_sub(MAX);
    names.truncate(MAX);
    format!(
        ". This mesh has: {}{}",
        names.join(", "),
        if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        }
    )
}

// ---------------------------------------------------------------------------
// Election
// ---------------------------------------------------------------------------

/// The node currently elected to reconcile `name`: rendezvous hashing over
/// every live member, recomputed from each node's own view every tick.
///
/// **Every live member, `no-execute` ones included** — reconciling is
/// *coordination*, not execution, exactly as firing a schedule is. The
/// reconciler authors instance jobs and each is then placed by the ordinary
/// scheduler, which still refuses a `no-execute` node as a candidate.
pub fn elected_reconciler(
    state: &MeshState,
    name: &str,
    liveness_of: impl Fn(&NodeId) -> mesh_proto::Liveness,
) -> Option<NodeId> {
    let pool: Vec<NodeId> = state
        .members
        .values()
        .filter(|m| state.is_active_member(&m.info.node_id))
        .filter(|m| matches!(liveness_of(&m.info.node_id), mesh_proto::Liveness::Alive))
        .map(|m| m.info.node_id)
        .collect();
    scheduler::rendezvous_rank(ELECTION_CONTEXT, name.as_bytes(), pool)
        .into_iter()
        .next()
}

/// Is this deployment's creator still allowed to run what it starts?
///
/// Asked afresh on every tick and again on every executor, the same
/// pre-authorization-by-record rule schedules use: revoking the user or
/// taking away their role stops the deployment everywhere without recalling
/// a signature.
pub fn creator_may_still_run(
    state: &MeshState,
    founder: &[u8; 32],
    dep: &Deployment,
    at_micros: u64,
) -> bool {
    crate::rbac::is_user_active(state, founder, &dep.created_by)
        && crate::rbac::has_role(
            state,
            founder,
            &dep.created_by,
            crate::rbac::DEPLOYMENTS,
            at_micros,
        )
}

// ---------------------------------------------------------------------------
// Reconciliation — the supervisor as arithmetic
// ---------------------------------------------------------------------------

/// A slot is the identity a supervisor tracks across restarts and rollouts:
/// a replica index for a fixed-count deployment, or a node for a `system`
/// one. Its bytes seed the derived instance id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Replica(u32),
    Node(NodeId),
}

impl Slot {
    fn key_bytes(&self) -> Vec<u8> {
        match self {
            Slot::Replica(i) => {
                let mut v = b"replica:".to_vec();
                v.extend_from_slice(&i.to_le_bytes());
                v
            }
            Slot::Node(n) => {
                let mut v = b"node:".to_vec();
                v.extend_from_slice(&n.0);
                v
            }
        }
    }

    pub fn label(&self, state: &MeshState) -> String {
        match self {
            Slot::Replica(i) => i.to_string(),
            Slot::Node(n) => state
                .members
                .get(n)
                .map(|m| m.info.name.clone())
                .unwrap_or_else(|| n.short()),
        }
    }
}

/// The instance id for `(deployment, generation, slot, incarnation)`.
///
/// The incarnation is what makes a restart a *new* job rather than a
/// re-dispatch of a terminal one — a terminal id is adopted, never re-run,
/// so a crash-looping instance needs a fresh id each attempt (§6.2, P2).
pub fn instance_id(name: &str, generation: u32, slot: &Slot, incarnation: u32) -> JobId {
    let mut key = slot.key_bytes();
    key.extend_from_slice(b":inc:");
    key.extend_from_slice(&incarnation.to_le_bytes());
    JobId::derive_instance(name, generation, &key)
}

/// One instance to start.
#[derive(Debug, Clone)]
pub struct StartInstance {
    pub job_id: JobId,
    pub generation: u32,
    pub slot: Slot,
    pub incarnation: u32,
    /// The node to place it on. For `system` this is the slot's node; for a
    /// fixed count it is the rendezvous-chosen candidate.
    pub node: NodeId,
    /// Where the start goes, in order, when `node` refuses it — empty for a
    /// `system` slot, and for a start pinned by [`PlacementHints::pinned`].
    pub alternates: Vec<NodeId>,
    /// How the machines were scored for it (Phase C, E8), staged for `jobs
    /// explain` once one of them accepts. `None` for a `system` slot and a
    /// pinned start, where nothing was compared.
    pub explain: Option<mesh_proto::PlacementExplain>,
}

/// One instance to cancel, with the node running it and why.
#[derive(Debug, Clone)]
pub struct CancelInstance {
    pub job_id: JobId,
    pub node: NodeId,
    pub why: String,
}

/// The reconciler's decision for one deployment on one tick.
#[derive(Debug, Clone, Default)]
pub struct ReconcilePlan {
    pub starts: Vec<StartInstance>,
    pub cancels: Vec<CancelInstance>,
    /// Set when the failure budget is exceeded: the daemon records the event
    /// once and stops restarting. `None` while healthy.
    pub degraded: Option<String>,
    /// Set when the roll has made no progress for `progress_deadline`
    /// (N1 P3). Distinct from `degraded`, which means "this is crash
    /// looping": a stalled roll may have nothing crashing at all — a new
    /// version that starts, stays up and never passes its probe stalls
    /// without failing once.
    pub stalled: Option<String>,
    /// True while the canary set is up and still proving itself: new
    /// instances are running beside the old ones and nothing has been cut
    /// over yet (N1 P3).
    pub canarying: bool,
    /// The canary set has settled and holds for `deployment promote`
    /// (Phase B, E2).
    pub awaiting_promotion: bool,
    /// `stalled` is about instances that were never started, while every
    /// one that did start is healthy. A placement problem, not the
    /// release's: `auto_revert` must not answer it by rolling a good version
    /// back to reach the same machines.
    pub stall_is_placement: bool,
    /// Slots the deployment's own placement rules left nowhere to go this
    /// tick (Phase C, E7). The daemon records each as `deployment.unplaced`,
    /// as it does a refused start, so `status` says why beside the slot.
    pub unplaced: Vec<UnplacedSlot>,
}

/// One slot a placement rule could not place (Phase C, E7).
#[derive(Debug, Clone)]
pub struct UnplacedSlot {
    pub generation: u32,
    pub slot: Slot,
    pub why: String,
}

/// Rendezvous context for placing an instance. Its own string, so a node's
/// rank for an instance is unrelated to its rank for reconciling.
const PLACE_CONTEXT: &[u8] = b"mw-deployment-place-v1";

/// The node a deployment's scores are computed *from* (Phase C, E8): none.
///
/// `score_terms` gives the node asking a free round trip and every node on a
/// live connection its measured one. Either would make the ranking depend on
/// which node is elected reconciler, so a new reconciler could send a slot
/// somewhere the last one would not have — the disagreement rendezvous
/// placement was chosen to rule out. From no node, every candidate's `rtt` is
/// the same unknown and separates nothing.
const NO_OBSERVER: NodeId = NodeId([0u8; 32]);

/// A member by id: from the candidates when it is one, otherwise from state
/// (the executor of an instance being replaced need not be a candidate).
fn member_info<'a>(
    by_id: &BTreeMap<NodeId, &'a MemberInfo>,
    state: &'a MeshState,
    node: &NodeId,
) -> Option<&'a MemberInfo> {
    by_id
        .get(node)
        .copied()
        .or_else(|| state.members.get(node).map(|m| &m.info))
}

/// Why a slot's placement rules left it nowhere to go (Phase C, E7).
fn rules_refusal(spec: &mesh_proto::PlacementSpec, considered: usize) -> String {
    let machines = if considered == 1 {
        "machine"
    } else {
        "machines"
    };
    match (&spec.distinct_property, spec.distinct_hosts) {
        (Some(d), true) => format!(
            concat!(
                "distinct_hosts and distinct_property: each of the {} {} that could run it ",
                "already runs an instance, or has no {} value with room under its limit of {}"
            ),
            considered, machines, d.attribute, d.limit
        ),
        (Some(d), false) => format!(
            concat!(
                "distinct_property: none of the {} {} that could run it has a {} value ",
                "holding fewer than {} instance{}"
            ),
            considered,
            machines,
            d.attribute,
            d.limit,
            if d.limit == 1 { "" } else { "s" }
        ),
        (None, _) => format!(
            concat!(
                "distinct_hosts: each of the {} {} that could run it already runs an ",
                "instance of this deployment"
            ),
            considered, machines
        ),
    }
}

/// What a deployment's instances are scored on, gathered once per plan and
/// only when a start needs it (Phase C, E8).
///
/// **Instances go through the scheduler's own terms now**, not a hash alone.
/// A hash spreads well on an empty mesh and knows nothing else: it sent an
/// instance to the busiest machine as readily as to an idle one, and `jobs
/// explain` had nothing to say about an instance because nothing had been
/// compared. Rendezvous stays, as the tie-break — so on a mesh where every
/// machine scores the same, placement is exactly what it was.
///
/// **Only what every node agrees on.** Load, bundle warmth and recent
/// failures are read from replicated state; affinity and spread from the
/// record and the members. Round trip, memory, disk and CPU are one node's
/// measurements, so they stand at the neutral unknown for every candidate:
/// whichever node is elected computes the same ranking from the same state.
struct PlacementScorer<'a> {
    state: &'a MeshState,
    load: scheduler::ExecutorLoad,
    bundle: Option<mesh_proto::BundleId>,
    fit: mesh_proto::FitPolicy,
    affinities: Vec<(Selector, i32)>,
    spread: Option<&'a mesh_proto::Spread>,
    /// For an even spread: how many values the candidates have between them.
    spread_values: usize,
    slots: f64,
}

impl<'a> PlacementScorer<'a> {
    fn new(
        state: &'a MeshState,
        dep: &Deployment,
        spec: &'a mesh_proto::PlacementSpec,
        fit: mesh_proto::FitPolicy,
        candidates: &[MemberInfo],
        slots: usize,
    ) -> Self {
        let spread_values = spec.spread.as_ref().map_or(0, |s| {
            candidates
                .iter()
                .filter_map(|m| crate::membership::node_attribute(state, m, &s.attribute))
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        });
        PlacementScorer {
            state,
            load: scheduler::ExecutorLoad::of(state),
            bundle: state
                .services
                .get(&dep.service)
                .and_then(|e| e.versions.get(&dep.service_version))
                .map(|def| def.bundle_id)
                .filter(|b| !b.is_none()),
            fit,
            affinities: spec
                .affinity
                .iter()
                .map(|a| (Selector::parse(&a.select), a.weight))
                .collect(),
            spread: spec.spread.as_ref(),
            spread_values,
            slots: slots as f64,
        }
    }

    /// `(instances holding this value already, instances it should hold)`.
    fn share(
        &self,
        spread: &mesh_proto::Spread,
        value: &str,
        used: &BTreeMap<String, u32>,
    ) -> (f64, f64) {
        let held = |v: &str| f64::from(used.get(v).copied().unwrap_or(0));
        if spread.targets.is_empty() {
            return (held(value), self.slots / self.spread_values.max(1) as f64);
        }
        if let Some(pct) = spread.targets.get(value) {
            return (held(value), f64::from(*pct) / 100.0 * self.slots);
        }
        // Values the file did not name share what is left, as one group.
        let named: u32 = spread.targets.values().sum();
        let rest = f64::from(100u32.saturating_sub(named)) / 100.0 * self.slots;
        let others: f64 = used
            .iter()
            .filter(|(v, _)| !spread.targets.contains_key(*v))
            .map(|(_, n)| f64::from(*n))
            .sum();
        (others, rest)
    }

    fn terms(
        &self,
        member: &MemberInfo,
        spread_used: &BTreeMap<String, u32>,
    ) -> Vec<mesh_proto::ScoreTerm> {
        let mut terms = scheduler::score_terms(
            self.state,
            &self.load,
            member,
            self.bundle.as_ref(),
            NO_OBSERVER,
            &|_| None,
            None,
            None,
            None,
            self.fit,
        );
        let term = |name: &str, raw: f64, weight: f64| mesh_proto::ScoreTerm {
            name: name.to_string(),
            raw,
            weighted: raw * weight,
            weight,
            unknown: false,
        };
        if !self.affinities.is_empty() {
            let matched: i64 = self
                .affinities
                .iter()
                .filter(|(select, _)| select.matches_in(self.state, member))
                .map(|(_, weight)| i64::from(*weight))
                .sum();
            terms.push(term(
                "affinity",
                scheduler::affinity_penalty(matched),
                scheduler::W_AFFINITY,
            ));
        }
        if let Some(spread) = self.spread {
            // A machine with no value belongs to no share: the last place a
            // spread would choose, and never excluded, because a spread is a
            // preference.
            let raw = match crate::membership::node_attribute(self.state, member, &spread.attribute)
            {
                Some(value) => {
                    let (used, desired) = self.share(spread, &value, spread_used);
                    scheduler::spread_penalty(used, desired)
                }
                None => 1.0,
            };
            terms.push(term("spread", raw, scheduler::W_SPREAD));
        }
        terms
    }

    /// `nodes`, best first: the lowest total, then this instance's rendezvous
    /// order, so equal scores fall back to exactly the hash placement used to
    /// be.
    fn rank(
        &self,
        key: &JobId,
        nodes: &[NodeId],
        by_id: &BTreeMap<NodeId, &MemberInfo>,
        spread_used: &BTreeMap<String, u32>,
    ) -> Vec<mesh_proto::CandidateScore> {
        let mut scored: Vec<(usize, mesh_proto::CandidateScore)> =
            scheduler::rendezvous_rank(PLACE_CONTEXT, &key.0, nodes.iter().copied())
                .into_iter()
                .enumerate()
                .map(|(order, node)| {
                    let (name, terms) = match member_info(by_id, self.state, &node) {
                        Some(m) => (m.name.clone(), self.terms(m, spread_used)),
                        None => (node.short(), Vec::new()),
                    };
                    let total = terms.iter().map(|t| t.weighted).sum();
                    (
                        order,
                        mesh_proto::CandidateScore {
                            node,
                            name,
                            total,
                            terms,
                        },
                    )
                })
                .collect();
        scored.sort_by(|a, b| a.1.total.total_cmp(&b.1.total).then(a.0.cmp(&b.0)));
        scored.into_iter().map(|(_, c)| c).collect()
    }
}

/// A compact read of an instance job, gathered from replicated state so the
/// planner is a pure function of it.
struct InstanceFacts {
    state: JobState,
    executor: NodeId,
    finished_at_micros: Option<u64>,
}

fn instance_facts(state: &MeshState, id: &JobId) -> Option<InstanceFacts> {
    let job = state.jobs.get(id)?;
    Some(InstanceFacts {
        state: job.state,
        executor: job.executor,
        finished_at_micros: job.finished_at_micros,
    })
}

/// The first incarnation of `slot` at `generation` that has no job — the
/// one the planner starts next, one past the newest there is.
///
/// Incarnations are made one at a time and in order, so the ones that exist
/// are a prefix `0..n`. `n` is found by doubling and then halving: a
/// handful of lookups however long the slot's history, where walking up
/// from 0 cost one per incarnation.
fn next_incarnation(state: &MeshState, name: &str, generation: u32, slot: &Slot) -> u32 {
    let exists = |inc: u32| {
        state
            .jobs
            .contains_key(&instance_id(name, generation, slot, inc))
    };
    if !exists(0) {
        return 0;
    }
    // `exists(lo)`, and — once the doubling stops — `!exists(hi)`.
    let (mut lo, mut hi) = (0u32, 1u32);
    while exists(hi) {
        lo = hi;
        match hi.checked_mul(2) {
            Some(next) => hi = next,
            None => return u32::MAX,
        }
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if exists(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

/// The incarnations of `slot` at `generation` worth looking at: the newest
/// [`MAX_INCARNATION_SCAN`] of them, oldest first. Every live incarnation
/// is among them — a new one is made only once the one before has ended,
/// or beside it during a partition — and so is every failure a restart
/// budget's window can count.
fn recent_incarnations(
    state: &MeshState,
    name: &str,
    generation: u32,
    slot: &Slot,
) -> std::ops::Range<u32> {
    let next = next_incarnation(state, name, generation, slot);
    next.saturating_sub(MAX_INCARNATION_SCAN)..next
}

/// A flat `job_id -> healthy` view over the per-node health records.
/// Health per instance, with **when that state was entered**.
///
/// `since_micros` was already on the wire ([`mesh_proto::InstanceHealth`])
/// and unread here. N1 P3 needs it twice: `min_healthy_time` is "healthy,
/// and has been for N seconds", and the progress deadline's clock is the
/// most recent moment any instance of this generation *became* healthy.
/// Both fall out of the same field, which is why neither needed new state
/// on the reconciler — it stays a pure function of replicated state, which
/// is the property that lets any node take over mid-roll.
struct HealthIndex(BTreeMap<JobId, (bool, u64)>);

impl HealthIndex {
    fn build(state: &MeshState) -> Self {
        let mut map = BTreeMap::new();
        for node in state.deployment_health.values() {
            for inst in &node.instances {
                map.insert(inst.job_id, (inst.healthy, inst.since_micros));
            }
        }
        HealthIndex(map)
    }
    fn get(&self, id: &JobId) -> Option<bool> {
        self.0.get(id).map(|(ok, _)| *ok)
    }
    /// Healthy, and healthy for long enough to count (N1 P3).
    ///
    /// `min_healthy_secs == 0` is today's behaviour: the first passing
    /// probe counts. Anything else catches the process that answers one
    /// probe and then dies — the failure `min_healthy_time` exists for.
    fn settled(&self, id: &JobId, min_healthy_secs: u64, now_micros: u64) -> Option<bool> {
        let (ok, since) = self.0.get(id).copied()?;
        if !ok || min_healthy_secs == 0 {
            return Some(ok);
        }
        Some(now_micros.saturating_sub(since) >= min_healthy_secs * 1_000_000)
    }
    /// When an instance last entered its current health state.
    fn since(&self, id: &JobId) -> Option<u64> {
        self.0.get(id).map(|(_, since)| *since)
    }
}

/// Compute the plan for one deployment (§6.2).
///
/// `candidates` is the set of matching, live, executable nodes the daemon
/// resolved (it needs liveness, which is not in [`MeshState`]). For a
/// `system` count these *are* the slots; for a fixed count they are the pool
/// instances are placed across.
pub fn plan_reconcile(
    dep: &Deployment,
    state: &MeshState,
    candidates: &[MemberInfo],
    now_micros: u64,
    policy: &mesh_proto::DeploymentPolicy,
    not_alive_since: impl Fn(&NodeId) -> Option<u64>,
) -> ReconcilePlan {
    plan_reconcile_hinted(
        dep,
        state,
        candidates,
        now_micros,
        policy,
        not_alive_since,
        &PlacementHints::default(),
    )
}

/// [`plan_reconcile`], with what the elected reconciler has learned placing
/// instances (Phase B hardening). Every other caller — `status`, the list,
/// the dry run — plans without it, and differs only in which machine an
/// unplaced slot would be offered to next.
pub fn plan_reconcile_hinted(
    dep: &Deployment,
    state: &MeshState,
    candidates: &[MemberInfo],
    now_micros: u64,
    policy: &mesh_proto::DeploymentPolicy,
    not_alive_since: impl Fn(&NodeId) -> Option<u64>,
    hints: &PlacementHints,
) -> ReconcilePlan {
    let mut plan = ReconcilePlan::default();
    let health = HealthIndex::build(state);
    // N1 P4. An instance whose executor stopped being reachable more than
    // `lost_after` ago is *lost*, not merely quiet: cancel it so the slot
    // can be refilled somewhere that answers.
    //
    // `lost_after_secs == 0` is today's behaviour — the instance is left
    // alone and the mesh-wide Dead constant is the only clock. That was
    // never a decision, it was an absence, and it is the one this phase
    // exists to give an operator a say in.
    let lost = |node: &NodeId| -> bool {
        if policy.lost_after_secs == 0 {
            return false;
        }
        not_alive_since(node).is_some_and(|since| {
            now_micros.saturating_sub(since) >= policy.lost_after_secs * 1_000_000
        })
    };
    // Unreachable, but not yet lost. `replace` buys availability during the
    // partition at the price of running both — the original is still up on
    // its own side and still serving whoever can reach it.
    let unknown = |node: &NodeId| -> bool { not_alive_since(node).is_some() && !lost(node) };
    // A node being drained (MASTERPLAN §12; Phase C, E9) is still alive and
    // reachable but must shed work: an instance on it is migrated rather than
    // waited out (C2 P5, §6.4) — a replacement first, then the original, under
    // the deployment's `migrate` rules, below. `placement_pool` already leaves
    // a draining node out, so a replacement never lands back on it. A node
    // announcing `draining` with no deadline, which is every build before
    // Phase C, still means "move everything now".
    let drain_of = |node: &NodeId| -> Option<mesh_proto::NodeDrain> {
        state
            .members
            .get(node)
            .and_then(|m| mesh_proto::drain_of(&m.info.tags))
    };
    let is_draining = |node: &NodeId| -> bool { drain_of(node).is_some() };
    let cur_gen = dep.generation;
    let has_check = dep.health.is_some();
    let gate_on_health = dep.update.gate_on_health && has_check;

    // The slots the deployment wants filled.
    let slots: Vec<Slot> = match dep.count {
        Count::System => candidates.iter().map(|m| Slot::Node(m.node_id)).collect(),
        Count::Fixed(n) => (0..n).map(Slot::Replica).collect(),
    };

    // Failure budget: count instance failures across all slots in the
    // window. A supervisor that restarts forever is a fork bomb (§6.2).
    let window_start = now_micros.saturating_sub(dep.restart.budget_window_secs * 1_000_000);
    let mut failures_in_window = 0u32;

    // Per-slot pass 1: classify the current incarnation and count failures.
    struct SlotView {
        slot: Slot,
        // The current (highest) incarnation for this slot at `cur_gen`.
        incarnation: u32,
        // The current instance's facts, if it exists.
        current: Option<(JobId, InstanceFacts)>,
        // Further live incarnations of the *same* slot and generation.
        // Normally empty. Non-empty exactly when a partition produced a
        // replacement beside an original (N1 P4), which is the one case
        // the `reconcile` rule exists to settle.
        siblings: Vec<(JobId, InstanceFacts)>,
        // Non-terminal instances at prior generations (for rolling cutover).
        old: Vec<(JobId, NodeId)>,
        // When the most recent terminal-failed incarnation at `cur_gen` ended.
        last_failure_end: Option<u64>,
        // Failures of **this slot** inside the budget window (N7).
        //
        // Per slot, not per deployment. One counter for the whole
        // deployment meant a slot that could not start anywhere spent the
        // budget and then stopped *every other slot* from ever restarting —
        // so a single bad replica took a healthy one down with it the next
        // time it died.
        failures: u32,
        // Nodes a recent incarnation of this slot failed on (N7).
        //
        // The evidence that outranks every placement preference below.
        // Without it the sibling-spread rule sent each retry straight back
        // to the node that had just failed — six times in a row on a
        // two-node mesh, because the *other* node was "taken" by the
        // healthy sibling.
        failed_on: Vec<NodeId>,
    }

    // A deployment removed and created again under the same name derives the
    // same instance ids. The removed one's failures are history: they must
    // not back the new deployment off, or steer it away from a machine.
    // Their records still hold their ids, so the incarnation walk passes over
    // them either way.
    let lineage_floor = state
        .deployment_history
        .get(&dep.name)
        .map(|h| h.floor.wall_micros)
        .unwrap_or(0);
    let mut views: Vec<SlotView> = Vec::with_capacity(slots.len());
    for slot in &slots {
        let mut incarnation = 0u32;
        let mut current: Option<(JobId, InstanceFacts)> = None;
        let mut siblings: Vec<(JobId, InstanceFacts)> = Vec::new();
        let mut last_failure_end: Option<u64> = None;
        let mut slot_failures = 0u32;
        let mut failed_on: Vec<NodeId> = Vec::new();
        // Walk incarnations until we find one that is missing: the highest
        // present one is the "current" incarnation for this slot.
        for inc in recent_incarnations(state, &dep.name, cur_gen, slot) {
            let id = instance_id(&dep.name, cur_gen, slot, inc);
            match instance_facts(state, &id) {
                Some(facts) => {
                    if facts.state.is_terminal() {
                        if matches!(facts.state, JobState::Failed | JobState::TimedOut)
                            && let Some(end) = facts.finished_at_micros
                            && end > lineage_floor
                        {
                            if end >= window_start {
                                failures_in_window += 1;
                                slot_failures += 1;
                                if !failed_on.contains(&facts.executor) {
                                    failed_on.push(facts.executor);
                                }
                            }
                            last_failure_end = Some(end.max(last_failure_end.unwrap_or(0)));
                        }
                        incarnation = inc + 1;
                        current = None;
                    } else if current.is_none() {
                        incarnation = inc;
                        current = Some((id, facts));
                        // **Do not stop here** (N1 P4). A `replace` during
                        // a partition leaves two live incarnations of one
                        // slot, and the walk that stopped at the first
                        // could not see the second — so the reconcile rule
                        // had nothing to choose between. Keep scanning
                        // until an id is genuinely absent.
                    } else {
                        incarnation = inc;
                        siblings.push((id, facts));
                    }
                }
                None => break,
            }
        }

        // Leftover instances at prior generations, any incarnation.
        let mut old = Vec::new();
        let lo = cur_gen.saturating_sub(GEN_LOOKBACK);
        for g in lo..cur_gen {
            for inc in recent_incarnations(state, &dep.name, g, slot) {
                let id = instance_id(&dep.name, g, slot, inc);
                match state.jobs.get(&id) {
                    Some(job) if !job.state.is_terminal() => old.push((id, job.executor)),
                    Some(_) => {}
                    None => break,
                }
            }
        }

        views.push(SlotView {
            slot: slot.clone(),
            incarnation,
            current,
            siblings,
            old,
            last_failure_end,
            failures: slot_failures,
            failed_on,
        });
    }

    // **Said, not enforced (N7).** `degraded` is the operator-facing signal
    // that something is not staying up; it is no longer the switch that
    // stops work. The per-slot delay above is what throttles a slot that
    // cannot start, and it throttles only that slot — a deployment-wide
    // switch meant one bad replica stopped a healthy one from ever
    // restarting, which is the opposite of what a budget is for.
    let degraded = failures_in_window > dep.restart.budget_failures;
    if degraded {
        let worst = views.iter().map(|v| v.failures).max().unwrap_or(0);
        let waiting = views
            .iter()
            .filter(|v| v.failures > dep.restart.budget_failures)
            .map(|v| reschedule_delay(&dep.restart, v.incarnation, v.failures))
            .max()
            .unwrap_or(0);
        plan.degraded = Some(format!(
            "{failures_in_window} failures in the last {}s exceeded the budget of {} \
             (worst slot: {worst}); still retrying, now every {}",
            dep.restart.budget_window_secs,
            dep.restart.budget_failures,
            humanise_secs(waiting)
        ));
    }

    // Readiness of a slot's current instance.
    let ready = |v: &SlotView| -> bool {
        match &v.current {
            None => false,
            Some((id, f)) => {
                if !matches!(f.state, JobState::Running | JobState::Queued) {
                    return false;
                }
                if gate_on_health {
                    health
                        .settled(id, policy.min_healthy_secs, now_micros)
                        .unwrap_or(false)
                } else {
                    matches!(f.state, JobState::Running)
                }
            }
        }
    };

    // How many slots are mid-roll: a current instance exists but is not yet
    // ready. This is the batch the rolling update advances one bounded group
    // at a time (§6.2, P4).
    let rolling = views
        .iter()
        .filter(|v| v.current.is_some() && !ready(v))
        .count() as u32;
    let max_unavailable = dep.update.max_unavailable.max(1);
    let mut rolling_budget = max_unavailable.saturating_sub(rolling);

    // ---------------------------------------------------------------
    // N1 P3 — the canary phase
    // ---------------------------------------------------------------
    //
    // A roll is in flight when some slot still carries a prior-generation
    // instance. With `canary = N`, the first N of those slots bring their
    // new instance up **beside** the old one and nothing is cut over until
    // all N have settled; then the roll proceeds normally and the ordinary
    // cutover retires the old set. `canary == count` is blue/green, with no
    // separate strategy: every slot is a canary, so the whole new set comes
    // up beside the whole old one.
    //
    // **Promotion is health, not a person.** Nomad gates this on an
    // operator running `deployment promote`, and the fields this phase
    // already has — `min_healthy_time` to say what "proved itself" means
    // and `progress_deadline` to bound the wait — answer the same question
    // without needing somebody awake. Phase B added the person as an
    // opt-in (`auto_promote = false`, below) without changing this shape —
    // and a roll that waits for one says so, in `status` and in the watch.
    let rolling_slots: Vec<usize> = views
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.old.is_empty())
        .map(|(i, _)| i)
        .collect();
    let canary_set: std::collections::BTreeSet<usize> = if policy.canary == 0 {
        Default::default()
    } else {
        rolling_slots
            .iter()
            .take(policy.canary as usize)
            .copied()
            .collect()
    };
    // Still proving: at least one canary slot has not yet settled. A slot
    // whose new instance has not even started counts as unsettled, which is
    // what keeps the phase open until all N are actually up.
    let proving = !canary_set.is_empty() && canary_set.iter().any(|i| !ready(&views[*i]));
    // **Phase B, E2 — unless a person was asked for.** A deployment whose
    // file set `auto_promote = false` holds a settled canary set until
    // somebody promotes *this* generation; health still decides when there
    // is anything to promote. Both halves come from replicated state, so
    // whichever node is elected reconciler holds the same line.
    let awaiting_promotion = !canary_set.is_empty() && !proving && !promoted(state, dep);
    let canarying = proving || awaiting_promotion;
    plan.canarying = canarying;
    plan.awaiting_promotion = awaiting_promotion;
    if canarying {
        // Exactly N new instances, and no more, however generous
        // `max_unavailable` is. The canary count is the ceiling during the
        // phase — that is the entire promise.
        let canaries_up = canary_set
            .iter()
            .filter(|i| views[**i].current.is_some())
            .count() as u32;
        rolling_budget = policy.canary.saturating_sub(canaries_up);
    }

    // ---------------------------------------------------------------
    // N1 P3 — the progress deadline
    // ---------------------------------------------------------------
    //
    // The clock is derived, not kept: the most recent moment any instance
    // of this generation *became* healthy, falling back to the earliest
    // moment one started when none has. Nothing is stored, so a reconciler
    // that takes over mid-roll inherits the same deadline the last one had
    // rather than restarting it — which a stored timer would have done
    // every time the election moved, making the deadline unfireable on a
    // mesh with a flapping node.
    if policy.progress_deadline_secs > 0 {
        let converged = views.iter().all(ready);
        let last_progress = views
            .iter()
            .filter_map(|v| v.current.as_ref())
            .filter_map(|(id, _)| {
                if health.get(id) == Some(true) {
                    health.since(id)
                } else {
                    state.jobs.get(id).and_then(|j| j.started_at_micros)
                }
            })
            .max()
            // A promotion is progress (Phase B, E2). Without it, promoting a
            // canary set that had been healthy for longer than the deadline
            // would stall the roll on the very next tick.
            .max(
                state
                    .deployment_promotions
                    .get(&dep.name)
                    .filter(|p| p.promotion.generation == dep.generation)
                    .map(|p| p.updated_hlc.wall_micros),
            );
        // **A hold is bounded too (Phase B, E2).** A canary set waiting for a
        // person stalls here like any roll — and reverts, with `auto_revert` —
        // so a hold nobody was told about ends instead of waiting forever,
        // which is §14's objection to a promote verb. `progress_deadline =
        // "never"` is how a file says to wait.
        if !converged
            && let Some(last) = last_progress
            && now_micros.saturating_sub(last) >= policy.progress_deadline_secs * 1_000_000
        {
            // A slot with no instance and nothing rolling from was never
            // started. When that is the whole story — every instance that did
            // start is healthy — the sentence has to say so, or somebody
            // reverts a good release.
            let unstarted = views
                .iter()
                .filter(|v| v.current.is_none() && v.old.is_empty())
                .count();
            let started_are_healthy = views
                .iter()
                .filter_map(|v| v.current.as_ref())
                .all(|(id, _)| health.get(id) == Some(true));
            plan.stall_is_placement = unstarted > 0 && started_are_healthy;
            plan.stalled = Some(if plan.stall_is_placement {
                format!(
                    "{unstarted} of {} instances could not be started in {}s (progress deadline); \
                     every instance that did start is healthy — the instance rows say why",
                    views.len(),
                    policy.progress_deadline_secs
                )
            } else {
                format!(
                    "no instance has reached healthy for {}s (progress deadline)",
                    policy.progress_deadline_secs
                )
            });
        }
    }

    // Where this deployment's other live instances already are (N6).
    //
    // **Siblings spread, because that is the whole reason to ask for two.**
    // Rendezvous hashing places each slot independently, so with two
    // replicas and two nodes it lands both on one machine about half the
    // time — which looks like it worked and buys no redundancy at all.
    // Nomad's scheduler avoids co-locating allocations of one group for the
    // same reason; this is the same idea, kept deterministic so two
    // reconcilers still agree.
    //
    // A *preference*, never a constraint: three replicas on two nodes must
    // still place, so the filter falls back to the full pool the moment it
    // would empty it.
    let occupied: std::collections::HashSet<NodeId> = views
        .iter()
        .filter_map(|v| v.current.as_ref().map(|(_, f)| f.executor))
        .collect();
    let mut taken = occupied.clone();

    // **Phase C, E7/E8 — what this deployment says about where it goes.**
    // Read off the side-car; an older record has none, and places exactly as
    // it did.
    let spec = mesh_proto::PlacementSpec::from_env(&policy.instance_env);
    let by_id: BTreeMap<NodeId, &MemberInfo> = candidates.iter().map(|m| (m.node_id, m)).collect();
    let attribute_of = |node: &NodeId, attribute: &str| -> Option<String> {
        member_info(&by_id, state, node)
            .and_then(|m| crate::membership::node_attribute(state, m, attribute))
    };
    // How many instances each value already holds. An instance whose node is
    // lost or draining is on its way out, and holds no share against its own
    // replacement.
    let mut property_used: BTreeMap<String, u32> = BTreeMap::new();
    let mut spread_used: BTreeMap<String, u32> = BTreeMap::new();
    for (_, facts) in views.iter().filter_map(|v| v.current.as_ref()) {
        if lost(&facts.executor) || is_draining(&facts.executor) {
            continue;
        }
        if let Some(d) = &spec.distinct_property
            && let Some(value) = attribute_of(&facts.executor, &d.attribute)
        {
            *property_used.entry(value).or_default() += 1;
        }
        if let Some(s) = &spec.spread
            && let Some(value) = attribute_of(&facts.executor, &s.attribute)
        {
            *spread_used.entry(value).or_default() += 1;
        }
    }
    // Built on the first start that needs it: the fold over every job in
    // state is not free, and a steady deployment starts nothing.
    let mut scorer: Option<PlacementScorer> = None;
    let new_scorer = || PlacementScorer::new(state, dep, &spec, hints.fit, candidates, slots.len());

    // **Phase C, E9 — leaving a drained machine without an outage.**
    //
    // Before this a drain cancelled every instance on the machine in the same
    // tick, and the replacements started after: two instances of a service on
    // one machine was a service with none, for as long as a start takes.
    let migrate = mesh_proto::MigratePolicy::from_env(&policy.instance_env);
    // Fit to stand in for an instance being moved: running, healthy when
    // there is a check, and so for `migrate.min_healthy_secs`.
    let stands_in = |id: &JobId, facts: &InstanceFacts| -> bool {
        if !matches!(facts.state, JobState::Running) {
            return false;
        }
        let min = migrate.min_healthy_secs.max(policy.min_healthy_secs);
        if has_check {
            health.settled(id, min, now_micros).unwrap_or(false)
        } else {
            state
                .jobs
                .get(id)
                .and_then(|j| j.started_at_micros)
                .is_some_and(|at| now_micros.saturating_sub(at) >= min * 1_000_000)
        }
    };
    // The live incarnation standing in for a slot's instance on a drained
    // machine: one that is not itself on a drained machine.
    let replacement_of = |v: &SlotView| -> Option<(JobId, InstanceFacts)> {
        v.siblings
            .iter()
            .find(|(_, f)| !is_draining(&f.executor))
            .map(|(id, f)| {
                (
                    *id,
                    InstanceFacts {
                        state: f.state,
                        executor: f.executor,
                        finished_at_micros: f.finished_at_micros,
                    },
                )
            })
    };
    // A replacement that has not proved itself within `healthy_deadline`
    // stops holding a place, so the next instance can start moving.
    let overdue = |id: &JobId| -> bool {
        state.jobs.get(id).is_some_and(|j| {
            now_micros.saturating_sub(j.created_at_micros)
                >= migrate.healthy_deadline_secs * 1_000_000
        })
    };
    let mut moving = u32::try_from(
        views
            .iter()
            .filter(|v| {
                v.current
                    .as_ref()
                    .is_some_and(|(_, f)| is_draining(&f.executor))
                    && replacement_of(v).is_some_and(|(id, f)| !stands_in(&id, &f) && !overdue(&id))
            })
            .count(),
    )
    .unwrap_or(u32::MAX);

    for v in &views {
        // An instance on a machine being drained. Stopping it is never a
        // failure — it never counts against the budget (only `Failed` and
        // `TimedOut` do).
        let mut moving_this = false;
        if let Some((id, facts)) = &v.current
            && let Some(drain) = drain_of(&facts.executor)
        {
            let past = drain.deadline_micros.is_some_and(|at| now_micros >= at);
            match replacement_of(v) {
                // Its replacement is up elsewhere: stop the original once the
                // replacement has proved itself, or once time is up.
                Some((rid, rf)) => {
                    if past || stands_in(&rid, &rf) {
                        plan.cancels.push(CancelInstance {
                            job_id: *id,
                            node: facts.executor,
                            why: if past {
                                "node is draining and its deadline has passed; stopping the instance"
                            } else {
                                "node is draining and the instance's replacement is healthy elsewhere"
                            }
                            .into(),
                        });
                    }
                    continue;
                }
                // Time is up, or the machine is shutting down and there is
                // nowhere else to go: the old behaviour, and the only honest
                // one left.
                None if past || (drain.leaving && candidates.is_empty()) => {
                    plan.cancels.push(CancelInstance {
                        job_id: *id,
                        node: facts.executor,
                        why: "node is draining; moving the instance".into(),
                    });
                    continue;
                }
                None if moving >= migrate.max_parallel.max(1) => continue,
                // Start its replacement, through the start path below.
                None => {
                    moving += 1;
                    moving_this = true;
                }
            }
        }

        // N1 P4 — the partition healed and the slot has two instances.
        //
        // Settled here rather than by whoever reconnects first, because
        // both sides of a partition run this same function over the same
        // replicated state and must reach the same verdict. Every rule
        // below is a total order over facts already in state, so they do.
        //
        // Only once **every** instance's node answers again: cancelling
        // the loser while it is still unreachable would leave the mesh
        // believing one instance is gone while it keeps serving on its own
        // side, which is the exact confusion `reconcile` exists to end.
        if !v.siblings.is_empty()
            && !moving_this
            && let Some((cur_id, cur_facts)) = &v.current
        {
            let all_back = not_alive_since(&cur_facts.executor).is_none()
                && v.siblings
                    .iter()
                    .all(|(_, f)| not_alive_since(&f.executor).is_none());
            if all_back {
                let started = |id: &JobId| state.jobs.get(id).and_then(|j| j.started_at_micros);
                // Every live incarnation of this slot, in incarnation
                // order: the original first, the replacement after it.
                let all: Vec<(JobId, NodeId, Option<u64>)> =
                    std::iter::once((*cur_id, cur_facts.executor, started(cur_id)))
                        .chain(
                            v.siblings
                                .iter()
                                .map(|(id, f)| (*id, f.executor, started(id))),
                        )
                        .collect();
                // The winner. `KeepOriginal` is the lowest incarnation,
                // which is the one that was there before the partition,
                // because ids are derived in incarnation order and the
                // replacement is always the later one.
                // **Phase A, §2.2.** A replacement that exists because
                // `lost_after` passed — not because `replace` asked for one
                // — keeps the slot. The operator said the original was lost;
                // handing the slot back to it the moment its node reappears
                // would undo the one decision they made, and `reconcile`'s
                // `keep_original` default was chosen for `replace`, where
                // both copies were deliberate.
                let rule = if policy.replace_unknown {
                    policy.reconcile
                } else {
                    mesh_proto::Reconcile::KeepReplacement
                };
                let winner = match rule {
                    mesh_proto::Reconcile::KeepOriginal => all.first().map(|(id, _, _)| *id),
                    mesh_proto::Reconcile::KeepReplacement => all.last().map(|(id, _, _)| *id),
                    // Longest running is the *earliest* start. An instance
                    // with no recorded start has not run long at all, so it
                    // sorts last rather than winning on a `None`.
                    mesh_proto::Reconcile::LongestRunning => all
                        .iter()
                        .min_by_key(|(_, _, started)| started.unwrap_or(u64::MAX))
                        .map(|(id, _, _)| *id),
                    // Best score: the node placement would choose today,
                    // through the same scorer that placed the slot in the
                    // first place — so this cannot drift from placement.
                    mesh_proto::Reconcile::BestScore => {
                        let pool: Vec<NodeId> = all.iter().map(|(_, n, _)| *n).collect();
                        let key = instance_id(&dep.name, cur_gen, &v.slot, 0);
                        scorer
                            .get_or_insert_with(&new_scorer)
                            .rank(&key, &pool, &by_id, &spread_used)
                            .first()
                            .map(|best| best.node)
                            .and_then(|best| {
                                all.iter()
                                    .find(|(_, n, _)| *n == best)
                                    .map(|(id, _, _)| *id)
                            })
                    }
                };
                if let Some(winner) = winner {
                    for (id, node, _) in &all {
                        if *id != winner {
                            plan.cancels.push(CancelInstance {
                                job_id: *id,
                                node: *node,
                                why: format!("partition healed; reconcile = {}", rule.as_str()),
                            });
                        }
                    }
                    continue;
                }
            }
        }

        // N1 P4 — the far side of a partition, and what Phase A made of it.
        //
        // **Lost means vacated.** This used to author a cancel and stop
        // there, on the theory that "the op replicates, and the node applies
        // it when it comes back". It does not: a cancel is a request *to the
        // executor*, which is unreachable by definition, and nothing else may
        // close an instance's job record. So the instance stayed non-terminal
        // in replicated state, the slot kept it as `current`, and
        // `lost_after` on its own refilled nothing — the partition control
        // this whole block exists for did not work unless `replace` was set
        // beside it (`docs/NOMAD-ENHANCEMENTS.md` §2.2).
        //
        // Now a lost slot is refilled on a node that answers, through the
        // same start path `replace` takes, and the cancel is re-sent every
        // tick so it lands the moment the node is back. Until then the
        // original keeps serving whoever *can* reach it, which on a CGNAT
        // mesh is frequently somebody; when the node returns, the reconcile
        // rule above keeps the replacement, because the operator said the
        // original was lost.
        let lost_now = v
            .current
            .as_ref()
            .is_some_and(|(_, facts)| lost(&facts.executor));
        if let Some((id, facts)) = &v.current
            && lost_now
        {
            plan.cancels.push(CancelInstance {
                job_id: *id,
                node: facts.executor,
                why: format!(
                    "node unreachable for more than lost_after ({}s); the instance is lost",
                    policy.lost_after_secs
                ),
            });
            // Replaced once, not once a tick: a sibling is the replacement.
            if !v.siblings.is_empty() {
                continue;
            }
        }

        let slot_ready = ready(v);

        // Unreachable but not yet lost. With `replace` the slot gets a
        // second instance now and the two are reconciled when the partition
        // heals; without it the slot waits for `lost_after`.
        //
        // **Once**, not once a tick: a slot that already has a sibling has
        // been replaced, and the next incarnation would be a third copy of
        // one service every time this ran.
        let mut replacing_unknown = lost_now;
        if let Some((_, facts)) = &v.current
            && unknown(&facts.executor)
        {
            if !policy.replace_unknown || !v.siblings.is_empty() {
                continue;
            }
            replacing_unknown = true;
            // Fall through to the start path below, which brings up the
            // next incarnation on a node that answers. The original is
            // deliberately *not* cancelled — that is what `replace` means.
        } else if slot_ready && !lost_now && !moving_this {
            // `!lost_now`: a lost instance's last published health may
            // still say healthy — it is the one fact about it that can no
            // longer change — and taking that at its word here would skip
            // the start the lost branch just asked for.
            // Cutover: once the target instance is ready, retire leftovers.
            //
            // Suppressed for the whole deployment while the canary set is
            // still proving itself (N1 P3): a canary that cut over the
            // moment it went healthy would be an ordinary rolling update
            // with extra steps, and the old set has to stay up for the
            // canary to be worth anything.
            if canarying {
                continue;
            }
            for (id, node) in &v.old {
                plan.cancels.push(CancelInstance {
                    job_id: *id,
                    node: *node,
                    why: "superseded by a healthy new-generation instance".into(),
                });
            }
            continue;
        }

        // A current instance exists but is not ready yet — it is starting or
        // being health-gated. Leave it; it counts against the batch. Unless
        // this is a `replace`, where the point is precisely to start a
        // second one beside an instance that already exists.
        if v.current.is_some() && !replacing_unknown && !moving_this {
            continue;
        }
        // From here a drain's replacement is a replacement like any other.
        let replacing_unknown = replacing_unknown || moving_this;

        // The incarnation the new instance takes. A replacement is the one
        // *after* the instance it stands in for — ids are derived, so
        // reusing the number would derive the id already in state and the
        // start would be silently adopted as the original rather than
        // placed.
        let incarnation = if replacing_unknown {
            v.incarnation.saturating_add(1)
        } else {
            v.incarnation
        };

        // No current instance for this slot at `cur_gen`. Decide when to
        // start the next one.
        //
        // **Over budget slows this slot down; it does not stop it (N7).**
        //
        // It used to stop it, permanently and for the whole deployment: a
        // replica that could not start anywhere burned the budget and was
        // then never tried again, on any node, even with an idle machine
        // sitting right there that could have run it. A deployment whose
        // headline is *work that is supposed to stay up, kept up* answered
        // `1/1/2` and stayed there.
        //
        // Nomad separates the two and is right to: `restart` retries on the
        // same node, `reschedule` moves it elsewhere — and for a service
        // job `reschedule` defaults to `unlimited = true` with exponential
        // delay capped at an hour. Meshware had only the first half, so
        // exhausting it meant giving up rather than trying somewhere else.
        //
        // So the budget now buys *patience* instead of silence: past it the
        // delay grows exponentially to [`RESCHEDULE_MAX_SECS`], and the
        // placement below prefers a node this slot has not already failed
        // on. `degraded` still says so, loudly, on every surface — it just
        // no longer means "stopped".
        if let Some(end) = v.last_failure_end {
            let delay = reschedule_delay(&dep.restart, v.incarnation, v.failures) * 1_000_000;
            if now_micros < end.saturating_add(delay) {
                continue;
            }
        }

        // Placement: for `system` the slot names the node. For a fixed count
        // the deployment's rules come first, the scheduler's terms rank what
        // is left, and rendezvous breaks ties — deterministic, so two
        // reconcilers agree (Phase C).
        let (node, alternates, explain) = match &v.slot {
            Slot::Node(n) => {
                // A `system` slot whose node dropped out of the candidate
                // set has already been removed as a slot; if it is here the
                // node still matches. It has nowhere else to go, so a recent
                // refusal there is waited out rather than asked again.
                let key = instance_id(&dep.name, cur_gen, &v.slot, incarnation);
                if hints.avoid.get(&key).is_some_and(|r| r.contains(n)) {
                    continue;
                }
                (*n, Vec::new(), None)
            }
            Slot::Replica(_) => {
                let key = instance_id(&dep.name, cur_gen, &v.slot, incarnation);
                if let Some(n) = hints.pinned.get(&key) {
                    // Accepted there, or possibly: see `PlacementHints::pinned`.
                    (*n, Vec::new(), None)
                } else {
                    // A replacement must not land back on the node it is
                    // replacing — the same rule the drain path already applies,
                    // for the same reason.
                    let away = v.current.as_ref().map(|(_, f)| f.executor);
                    let pool: Vec<NodeId> = candidates
                        .iter()
                        .map(|m| m.node_id)
                        .filter(|n| !replacing_unknown || Some(*n) != away)
                        .collect();
                    // **Evidence first: skip machines this slot has already
                    // failed on (N7).**
                    //
                    // Ahead of the spread preference below, and that order is
                    // the whole fix. A node that just failed this exact slot is
                    // worse than a node that merely already holds a sibling —
                    // but the spread rule ranked it *better*, so on a two-node
                    // mesh every retry went straight back to the machine that
                    // had failed, six times in a row, while the other node sat
                    // idle and able. The preference was fighting the evidence
                    // and winning.
                    // **And skip machines that refused it moments ago** (Phase B
                    // hardening). An admission refusal leaves no job record, so
                    // without the reconciler's memory the planner — a hash —
                    // chose the refusing machine again every tick, forever. When
                    // every machine has refused it, wait for the refusals to age
                    // out rather than ask the same machines every five seconds.
                    let refused: &[NodeId] =
                        hints.avoid.get(&key).map(Vec::as_slice).unwrap_or(&[]);
                    let fresh: Vec<NodeId> = pool
                        .iter()
                        .copied()
                        .filter(|n| !refused.contains(n))
                        .collect();
                    if fresh.is_empty() && !pool.is_empty() {
                        continue;
                    }
                    let elsewhere: Vec<NodeId> = fresh
                        .iter()
                        .copied()
                        .filter(|n| !v.failed_on.contains(n))
                        .collect();
                    // Never narrow to nothing: when every candidate has failed
                    // this slot, the least-bad answer is to try again rather
                    // than to place nothing and say nothing. The backoff above
                    // is what keeps that from being a hot loop.
                    let pool = if elsewhere.is_empty() {
                        fresh
                    } else {
                        elsewhere
                    };
                    // **The deployment's own rules (Phase C, E7).** Hard, unlike
                    // everything above and below: a machine that breaks one is
                    // not tried at all, and a slot they leave nowhere to go
                    // waits and says why rather than doubling up.
                    let considered = pool.len();
                    let pool: Vec<NodeId> = pool
                        .into_iter()
                        .filter(|n| {
                            if spec.distinct_hosts && taken.contains(n) {
                                return false;
                            }
                            spec.distinct_property.as_ref().is_none_or(|d| {
                                attribute_of(n, &d.attribute).is_some_and(|value| {
                                    property_used.get(&value).copied().unwrap_or(0) < d.limit
                                })
                            })
                        })
                        .collect();
                    if pool.is_empty() {
                        if considered > 0 {
                            plan.unplaced.push(UnplacedSlot {
                                generation: cur_gen,
                                slot: v.slot.clone(),
                                why: rules_refusal(&spec, considered),
                            });
                        }
                        continue; // nothing matches the selector right now
                    }
                    // Then prefer a machine this deployment is not already on.
                    let free: Vec<NodeId> = pool
                        .iter()
                        .copied()
                        .filter(|n| !taken.contains(n))
                        .collect();
                    let preferred = if free.is_empty() { pool.clone() } else { free };
                    let ranker = scorer.get_or_insert_with(&new_scorer);
                    let ranked = ranker.rank(&key, &preferred, &by_id, &spread_used);
                    let Some(first) = ranked.first().map(|c| c.node) else {
                        continue;
                    };
                    // Where the start spills if `first` refuses: the rest of the
                    // preferred machines in the same order, then the ones the
                    // spread preference passed over.
                    let mut alternates: Vec<NodeId> =
                        ranked.iter().skip(1).map(|c| c.node).collect();
                    for c in ranker.rank(&key, &pool, &by_id, &spread_used) {
                        if c.node != first && !alternates.contains(&c.node) {
                            alternates.push(c.node);
                        }
                    }
                    let explain = mesh_proto::PlacementExplain {
                        considered: u32::try_from(ranked.len()).unwrap_or(u32::MAX),
                        candidates: ranked
                            .into_iter()
                            .take(mesh_proto::EXPLAIN_CANDIDATES)
                            .collect(),
                        // Filled in by whoever takes it, which may not be
                        // `first` — see `PlacementExplain::chosen`.
                        chosen: None,
                        spilled: false,
                        at_micros: now_micros,
                    };
                    (first, alternates, Some(explain))
                }
            }
        };

        // Rolling gate: if this slot still has an old-generation instance
        // serving, starting the new one is a roll step — spend from the
        // batch budget. A slot with nothing serving (initial fill, or a
        // crash with no predecessor) is not a roll and starts freely.
        if !v.old.is_empty() && !moving_this {
            if rolling_budget == 0 {
                continue;
            }
            rolling_budget -= 1;
        }

        // Claimed within this tick too, not just against what is already
        // running: two empty slots planned in one pass would otherwise both
        // see the same free node and both take it — which is the exact
        // co-location this is here to prevent, one tick earlier.
        taken.insert(node);
        // …and so are the shares the rules and the spread count (Phase C).
        if let Some(d) = &spec.distinct_property
            && let Some(value) = attribute_of(&node, &d.attribute)
        {
            *property_used.entry(value).or_default() += 1;
        }
        if let Some(s) = &spec.spread
            && let Some(value) = attribute_of(&node, &s.attribute)
        {
            *spread_used.entry(value).or_default() += 1;
        }

        plan.starts.push(StartInstance {
            job_id: instance_id(&dep.name, cur_gen, &v.slot, incarnation),
            generation: cur_gen,
            slot: v.slot.clone(),
            incarnation: v.incarnation,
            node,
            alternates,
            explain,
        });
    }

    // Retire instances of slots that no longer exist at all — a `system`
    // node that left, or replicas beyond a reduced count. Scan the current
    // generation for slot ids not in the desired set.
    // (Handled implicitly for prior generations by the cutover above; here
    // we cover shrink of the same generation.)
    if let Count::Fixed(n) = dep.count {
        // Replicas at or above `n` should not run.
        for extra in n..(n + max_unavailable + 8).min(n + 64) {
            let slot = Slot::Replica(extra);
            for inc in recent_incarnations(state, &dep.name, cur_gen, &slot) {
                let id = instance_id(&dep.name, cur_gen, &slot, inc);
                match state.jobs.get(&id) {
                    Some(job) if !job.state.is_terminal() => plan.cancels.push(CancelInstance {
                        job_id: id,
                        node: job.executor,
                        why: "instance count was reduced".into(),
                    }),
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    // System deployments: a draining node drops out of the candidate set, so
    // it is no longer a slot above — but its instance is still running and
    // must be moved. Retire instances on draining nodes that the current
    // slot pass could not reach because the node is no longer a candidate.
    if matches!(dep.count, Count::System) {
        for member in state.members.values() {
            let node = member.info.node_id;
            // `keep-system` (Phase C, E9, Nomad's `-ignore-system`): a drain
            // that leaves the machine's per-node services running.
            if drain_of(&node).is_none_or(|d| d.keep_system) {
                continue;
            }
            let slot = Slot::Node(node);
            for inc in recent_incarnations(state, &dep.name, dep.generation, &slot) {
                let id = instance_id(&dep.name, dep.generation, &slot, inc);
                match state.jobs.get(&id) {
                    Some(job) if !job.state.is_terminal() => plan.cancels.push(CancelInstance {
                        job_id: id,
                        node: job.executor,
                        why: "node is draining; moving the instance".into(),
                    }),
                    Some(_) => continue,
                    None => break,
                }
            }
        }
    }

    plan
}

/// Cancels for every instance of a deployment that has been removed
/// (§6.2): "instances it started are ordinary jobs, cancelled by the
/// reconciler on its next pass." The tombstone lingers in state, so the
/// elected node can still find and retire them before the entry decays.
pub fn plan_teardown(
    dep: &Deployment,
    state: &MeshState,
    candidates: &[MemberInfo],
) -> Vec<CancelInstance> {
    let mut cancels = Vec::new();
    // For teardown, cast wider than the selector: a removed deployment's
    // instances may sit on nodes that no longer match. System slots are
    // every member; fixed slots reach a little past the count in case it
    // was shrunk before removal.
    let _ = candidates;
    let slots: Vec<Slot> = match dep.count {
        Count::System => state.members.keys().copied().map(Slot::Node).collect(),
        Count::Fixed(n) => (0..n.saturating_add(8)).map(Slot::Replica).collect(),
    };
    let lo = dep.generation.saturating_sub(GEN_LOOKBACK);
    for slot in &slots {
        for g in lo..=dep.generation {
            for inc in recent_incarnations(state, &dep.name, g, slot) {
                let id = instance_id(&dep.name, g, slot, inc);
                match state.jobs.get(&id) {
                    Some(job) if !job.state.is_terminal() => cancels.push(CancelInstance {
                        job_id: id,
                        node: job.executor,
                        why: "deployment removed".into(),
                    }),
                    Some(_) => continue,
                    None => break,
                }
            }
        }
    }
    cancels
}

// ---------------------------------------------------------------------------
// Row / status / check builders (pure over state)
// ---------------------------------------------------------------------------

/// Desired/running/healthy counts for a deployment, from replicated state.
pub fn counts(dep: &Deployment, state: &MeshState, candidates: &[MemberInfo]) -> (u32, u32, u32) {
    let health = HealthIndex::build(state);
    let slots: Vec<Slot> = match dep.count {
        Count::System => candidates.iter().map(|m| Slot::Node(m.node_id)).collect(),
        Count::Fixed(n) => (0..n).map(Slot::Replica).collect(),
    };
    let desired = slots.len() as u32;
    let mut running = 0u32;
    let mut healthy = 0u32;
    for slot in &slots {
        if let Some((id, facts)) = current_instance(dep, state, slot)
            && matches!(facts.state, JobState::Running | JobState::Queued)
        {
            running += 1;
            let ok = if dep.health.is_some() {
                health.get(&id).unwrap_or(false)
            } else {
                matches!(facts.state, JobState::Running)
            };
            if ok {
                healthy += 1;
            }
        }
    }
    (desired, running, healthy)
}

fn current_instance(
    dep: &Deployment,
    state: &MeshState,
    slot: &Slot,
) -> Option<(JobId, InstanceFacts)> {
    for inc in recent_incarnations(state, &dep.name, dep.generation, slot) {
        let id = instance_id(&dep.name, dep.generation, slot, inc);
        match instance_facts(state, &id) {
            Some(f) if !f.state.is_terminal() => return Some((id, f)),
            Some(_) => continue,
            None => return None,
        }
    }
    None
}

/// Every machine, with the deployment instances running on it and what each
/// asked for (Phase C, U8) — `meshnode deployment placement` and Director's
/// Placement panel.
///
/// Read out of replicated state, so every node draws the same map, and only
/// the current generation's live instances: a roll's outgoing instances are
/// the Deployments screen's business, and drawing both would show a machine
/// fuller than it is about to be. Consoles are left out — their machine is
/// already in the map under its own node.
pub fn placement_map(
    state: &MeshState,
    liveness_of: impl Fn(&NodeId) -> String,
) -> Vec<mesh_proto::PlacementNodeRow> {
    let health = HealthIndex::build(state);
    let mut by_node: BTreeMap<NodeId, Vec<mesh_proto::PlacementInstanceRow>> = BTreeMap::new();
    for entry in state.deployments.values().filter(|e| e.is_active()) {
        let dep = &entry.deployment;
        let asked = mesh_proto::Requirements::from_env(&instance_request_env(state, dep));
        let slots: Vec<Slot> = match dep.count {
            Count::System => state.members.keys().map(|n| Slot::Node(*n)).collect(),
            Count::Fixed(n) => (0..n).map(Slot::Replica).collect(),
        };
        for slot in &slots {
            let Some((id, facts)) = current_instance(dep, state, slot) else {
                continue;
            };
            let health_word = match health.get(&id) {
                Some(true) => "healthy",
                Some(false) => "unhealthy",
                None => "unknown",
            };
            by_node
                .entry(facts.executor)
                .or_default()
                .push(mesh_proto::PlacementInstanceRow {
                    deployment: dep.name.clone(),
                    slot: slot.label(state),
                    generation: dep.generation,
                    state: facts.state.as_str().to_string(),
                    health: health_word.to_string(),
                    cpus: asked.cpus,
                    mem_mb: asked.mem_mb,
                });
        }
    }
    let mut rows: Vec<mesh_proto::PlacementNodeRow> = state
        .members
        .values()
        .filter(|m| !m.left && !state.revoked.contains_key(&m.info.node_id))
        .map(|m| {
            let info = &m.info;
            let cap = mesh_proto::Capacity::from_tags(&info.tags);
            let drain = mesh_proto::drain_of(&info.tags);
            let mut instances = by_node.remove(&info.node_id).unwrap_or_default();
            instances.sort_by(|a, b| {
                (a.deployment.as_str(), a.slot.as_str())
                    .cmp(&(b.deployment.as_str(), b.slot.as_str()))
            });
            mesh_proto::PlacementNodeRow {
                node: info.node_id,
                name: info.name.clone(),
                liveness: liveness_of(&info.node_id),
                cores: cap.as_ref().map_or(0, |c| c.cores),
                mem_mb: cap.as_ref().map_or(0, |c| c.mem_mb),
                eligible: drain.is_none()
                    && state.may_execute(&info.node_id)
                    && !info.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG),
                draining: drain.is_some(),
                leaving: drain.is_some_and(|d| d.leaving),
                drain_deadline_secs: drain
                    .and_then(|d| d.deadline_micros)
                    .map(|at| at / 1_000_000),
                instances,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.node.cmp(&b.node)));
    rows
}

/// The overall status word for a deployment.
pub fn status_word(
    dep: &Deployment,
    degraded: bool,
    desired: u32,
    healthy: u32,
    updating: bool,
) -> String {
    if dep.paused {
        "paused".into()
    } else if degraded {
        "degraded".into()
    } else if updating {
        "updating".into()
    } else if healthy >= desired {
        "active".into()
    } else {
        "progressing".into()
    }
}

/// Build a display row for a deployment.
pub fn row(
    dep: &Deployment,
    state: &MeshState,
    founder: &[u8; 32],
    candidates: &[MemberInfo],
    degraded: bool,
    source: Option<String>,
) -> DeploymentRow {
    let (desired, running, healthy) = counts(dep, state, candidates);
    let updating = has_prior_generation_instances(dep, state);
    DeploymentRow {
        name: dep.name.clone(),
        service: dep.service_ref(),
        count: dep.count.as_display(),
        targets: dep.targets.clone(),
        generation: dep.generation,
        status: status_word(dep, degraded, desired, healthy, updating),
        desired,
        running,
        healthy,
        created_by_username: crate::rbac::username_of(state, founder, &dep.created_by),
        created_at_micros: dep.created_at_micros,
        source,
    }
}

fn has_prior_generation_instances(dep: &Deployment, state: &MeshState) -> bool {
    if dep.generation == 0 {
        return false;
    }
    let lo = dep.generation.saturating_sub(GEN_LOOKBACK);
    // Cheap-ish probe: any non-terminal instance at a prior cur_gen for replica 0
    // or any candidate node. We scan the jobs map for deployment membership
    // via derived ids over a bounded slot range instead of every job.
    for g in lo..dep.generation {
        for i in 0..64u32 {
            let id = instance_id(&dep.name, g, &Slot::Replica(i), 0);
            if let Some(job) = state.jobs.get(&id)
                && !job.state.is_terminal()
            {
                return true;
            }
        }
    }
    false
}

/// Build the full status view for `deployments status <name>` (§6.3).
#[allow(clippy::too_many_arguments)] // every input is a separate slice of state the view joins
pub fn status_view(
    dep: &Deployment,
    state: &MeshState,
    _founder: &[u8; 32],
    candidates: &[MemberInfo],
    reconciler: Option<String>,
    degraded_reason: Option<String>,
    stalled_reason: Option<String>,
    canarying: bool,
    awaiting_promotion: bool,
    policy_blind: Vec<String>,
    port_labels: Vec<String>,
    placement: Option<mesh_proto::PlacementRefusal>,
    policy: &mesh_proto::DeploymentPolicy,
    // Phase A, U4: the reconciler's own notion of "unknown" for an
    // executor — not Alive — so this view and the reconciler cannot
    // disagree about which instances are gone.
    unreachable: impl Fn(&NodeId) -> bool,
) -> DeploymentStatusView {
    let health = HealthIndex::build(state);
    let (desired, running, healthy) = counts(dep, state, candidates);
    let slots: Vec<Slot> = match dep.count {
        Count::System => candidates.iter().map(|m| Slot::Node(m.node_id)).collect(),
        Count::Fixed(n) => (0..n).map(Slot::Replica).collect(),
    };
    let mut instances = Vec::new();
    let mut updating_from: Option<u32> = None;
    for slot in &slots {
        // Current-generation instance.
        if let Some((id, facts)) = current_instance(dep, state, slot) {
            let away = unreachable(&facts.executor);
            instances.push(instance_row(dep, state, &health, slot, id, &facts, away));
        } else if let Some(row) = failed_row(dep, state, slot, mesh_proto::now_micros()) {
            instances.push(row);
        } else {
            instances.push(DeploymentInstanceRow {
                job_id: JobId([0u8; 16]),
                slot: slot.label(state),
                node: String::new(),
                generation: dep.generation,
                state: "absent".into(),
                health: "unknown".into(),
                health_detail: unplaced_reason(dep, state, slot).unwrap_or_default(),
                started_at_micros: None,
            });
        }
        // Any lingering prior-generation instance for this slot, so a roll
        // in progress is visible rather than implied.
        let lo = dep.generation.saturating_sub(GEN_LOOKBACK);
        for g in lo..dep.generation {
            for inc in recent_incarnations(state, &dep.name, g, slot) {
                let id = instance_id(&dep.name, g, slot, inc);
                match state.jobs.get(&id) {
                    Some(job) if !job.state.is_terminal() => {
                        updating_from = Some(g.max(updating_from.unwrap_or(0)));
                        let facts = InstanceFacts {
                            state: job.state,
                            executor: job.executor,
                            finished_at_micros: job.finished_at_micros,
                        };
                        let away = unreachable(&facts.executor);
                        instances.push(instance_row(dep, state, &health, slot, id, &facts, away));
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }
    let degraded = degraded_reason.is_some();
    let updating = updating_from.is_some();
    let status = if canarying {
        // Ahead of `updating`, because it is the more specific truth: a
        // canary *is* an update in progress, and saying only "updating"
        // would leave somebody waiting for a cutover that is deliberately
        // not coming yet.
        "canarying".to_string()
    } else {
        status_word(dep, degraded, desired, healthy, updating)
    };
    let unreachable = instances
        .iter()
        .filter(|i| i.health == crate::registry::UNREACHABLE)
        .count() as u32;
    DeploymentStatusView {
        name: dep.name.clone(),
        service: dep.service_ref(),
        count: dep.count.as_display(),
        targets: dep.targets.clone(),
        generation: dep.generation,
        status,
        desired,
        running,
        healthy,
        reconciler,
        updating_from,
        degraded_reason,
        canarying,
        awaiting_promotion,
        stalled_reason,
        policy_blind,
        port_labels,
        instances,
        placement,
        unreachable,
        availability: mesh_proto::availability(&dep.count, policy),
        runtime: state
            .services
            .get(&dep.service)
            .and_then(|entry| entry.versions.get(&dep.service_version))
            .map(|version| version.runtime.as_str().to_string())
            .unwrap_or_default(),
    }
}

/// The row for a slot with nothing running whose newest incarnation failed.
///
/// That slot used to be reported exactly like one no machine would take —
/// `absent`, no node, and in Director "looking for a machine that fits" —
/// while the planner was starting it on a machine, watching it fail, and
/// waiting longer each time. On a real mesh a container that podman could
/// not create sat like that for an hour with nobody told anything. The
/// copy's own record says all of it: where it ran, how it ended, and so
/// when it is tried again — from the same count and the same
/// [`reschedule_delay`] the planner backs off by.
fn failed_row(
    dep: &Deployment,
    state: &MeshState,
    slot: &Slot,
    now_micros: u64,
) -> Option<DeploymentInstanceRow> {
    let failed =
        |j: &mesh_proto::JobSummary| matches!(j.state, JobState::Failed | JobState::TimedOut);
    let recent = recent_incarnations(state, &dep.name, dep.generation, slot);
    let next = recent.end;
    let id = instance_id(&dep.name, dep.generation, slot, next.checked_sub(1)?);
    let job = state.jobs.get(&id).filter(|j| failed(j))?;
    // A failure from before the deployment was last made is history, and the
    // planner does not back off for it (`plan_reconcile`, `lineage_floor`).
    let lineage_floor = state
        .deployment_history
        .get(&dep.name)
        .map(|h| h.floor.wall_micros)
        .unwrap_or(0);
    let end = job.finished_at_micros.filter(|&end| end > lineage_floor)?;
    let window_start = now_micros.saturating_sub(dep.restart.budget_window_secs * 1_000_000);
    let failures = recent
        .filter_map(|inc| {
            state
                .jobs
                .get(&instance_id(&dep.name, dep.generation, slot, inc))
        })
        .filter(|j| failed(j))
        .filter_map(|j| j.finished_at_micros)
        .filter(|&end| end > lineage_floor && end >= window_start)
        .count() as u32;
    let retry_at = end.saturating_add(reschedule_delay(&dep.restart, next, failures) * 1_000_000);

    // A container engine's own exit statuses say whose fault it was, and
    // "exited 126" says nothing to anybody who has not read podman's manual.
    let container = job.runtime == mesh_proto::Runtime::Container.as_str();
    let how = match (job.state, job.exit_code) {
        (JobState::TimedOut, _) => "it timed out".to_string(),
        (_, Some(125)) if container => {
            "the container engine could not start it (exit 125)".to_string()
        }
        (_, Some(126)) if container => {
            "the container engine could not start it (exit 126)".to_string()
        }
        (_, Some(127)) if container => {
            "its command was not found in the image (exit 127)".to_string()
        }
        (_, Some(code)) => format!("it exited {code}"),
        (_, None) => "it failed".to_string(),
    };
    let times = if failures > 1 {
        format!(
            ", {failures} times in the last {}",
            humanise_secs(dep.restart.budget_window_secs)
        )
    } else {
        String::new()
    };
    let again = match retry_at.saturating_sub(now_micros) / 1_000_000 {
        0 => "starting it again now".to_string(),
        secs => format!("starting it again in {}", humanise_secs(secs)),
    };
    Some(DeploymentInstanceRow {
        job_id: id,
        slot: slot.label(state),
        node: state
            .members
            .get(&job.executor)
            .map(|m| m.info.name.clone())
            .unwrap_or_else(|| job.executor.short()),
        generation: dep.generation,
        state: job.state.as_str().to_string(),
        health: "unknown".into(),
        health_detail: format!("{how}{times} — {again}. Its log says why"),
        started_at_micros: job.started_at_micros,
    })
}

fn instance_row(
    dep: &Deployment,
    state: &MeshState,
    health: &HealthIndex,
    slot: &Slot,
    id: JobId,
    facts: &InstanceFacts,
    unreachable: bool,
) -> DeploymentInstanceRow {
    let node = state
        .members
        .get(&facts.executor)
        .map(|m| m.info.name.clone())
        .unwrap_or_else(|| facts.executor.short());
    let (health_word, detail) = if unreachable {
        // Phase A, U4. Ahead of every other answer: whatever health this
        // instance's node last published is the one fact about it that can
        // no longer be updated, so repeating it would be repeating a guess.
        (
            crate::registry::UNREACHABLE.to_string(),
            "its node is not answering".to_string(),
        )
    } else if dep.health.is_none() {
        ("unknown".to_string(), "no health check".to_string())
    } else {
        match health.get(&id) {
            Some(true) => ("healthy".to_string(), String::new()),
            Some(false) => ("unhealthy".to_string(), String::new()),
            None => ("starting".to_string(), String::new()),
        }
    };
    let started = state.jobs.get(&id).and_then(|j| j.started_at_micros);
    DeploymentInstanceRow {
        job_id: id,
        slot: slot.label(state),
        node,
        generation: dep.generation,
        state: facts.state.as_str().to_string(),
        health: health_word,
        health_detail: detail,
        started_at_micros: started,
    }
}

/// The dry-run answer: what the record parsed to and what it would do.
pub fn check_view(
    dep: &Deployment,
    policy: &mesh_proto::DeploymentPolicy,
    state: &MeshState,
    matching_nodes: u32,
    placement: Option<mesh_proto::PlacementRefusal>,
    inline_service: bool,
) -> mesh_proto::DeploymentCheckView {
    // A rehearsal for a file with `[deployment.container]` publishes
    // nothing, so the service it describes is not there yet — and reporting
    // it as missing would be the check contradicting the line above it,
    // which has just said it would create it.
    let issues = if inline_service {
        Vec::new()
    } else {
        validate(state, dep)
            .into_iter()
            .map(|i| i.message)
            .collect::<Vec<_>>()
    };
    // Phase B, E5: the diff against what is published. `rolls` is read off
    // the generation rather than off the diff, because both front ends
    // decide the generation with `generation_for` before a dry run is sent —
    // so this says what will actually happen, not what the diff implies.
    let (changes, rolls, first_publish) =
        match state.deployments.get(&dep.name).filter(|e| !e.removed) {
            None => (Vec::new(), false, true),
            Some(entry) => (
                plan_changes(
                    &entry.deployment,
                    &policy_for(state, &dep.name),
                    dep,
                    policy,
                ),
                entry.deployment.generation != dep.generation,
                false,
            ),
        };
    mesh_proto::DeploymentCheckView {
        changes,
        rolls,
        first_publish,
        name: dep.name.clone(),
        service: dep.service_ref(),
        count: dep.count.as_display(),
        targets: dep.targets.clone(),
        matching_nodes,
        health: dep.health.as_ref().map(|h| match h.kind {
            HealthKind::Exec => format!("exec `{}`", h.command),
            HealthKind::Tcp => format!("tcp :{}", h.port),
            HealthKind::Http => format!("http :{}{}", h.port, h.path),
        }),
        issues: issues
            .into_iter()
            .chain(distinct_hosts_issue(dep, policy, matching_nodes))
            .collect(),
        placement,
        // The policy *this file* produces, so the sentence covers the
        // defaults nobody typed — the ones a person most needs told.
        availability: mesh_proto::availability(&dep.count, policy),
    }
}

/// What changes between the published record and a file about to replace
/// it, field by field (Phase B, E5) — `nomad job plan`'s diff, for a
/// deployment.
///
/// Both sides are rendered with [`to_toml`] and flattened, rather than
/// compared struct field by struct field: the renderer is already held to
/// being a total round trip (`the_whole_record_round_trips`), so any field
/// it knows about is a field this diff knows about, and there is no second
/// list of fields to fall behind the first.
pub fn plan_changes(
    prev: &Deployment,
    prev_policy: &mesh_proto::DeploymentPolicy,
    next: &Deployment,
    next_policy: &mesh_proto::DeploymentPolicy,
) -> Vec<mesh_proto::PlanChange> {
    let before = flatten(prev, prev_policy);
    let after = flatten(next, next_policy);
    let fields: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    fields
        .into_iter()
        .filter_map(|field| {
            let from = before.get(field).cloned().unwrap_or_default();
            let to = after.get(field).cloned().unwrap_or_default();
            (from != to).then(|| mesh_proto::PlanChange {
                effect: change_effect(field, &from, &to).to_string(),
                field: field.clone(),
                from,
                to,
            })
        })
        .collect()
}

/// Which of the things a changed field does (Phase B, E5).
///
/// Decided by the block it lives in, which is the same line [`rolls`]
/// draws: the rollout and partition knobs sit outside it, so changing them
/// restarts nothing, and a description or a pause is only the record. A
/// count is a resize (E3) — unless one side is `system`, where the slots
/// stop being the same slots and everything rolls.
/// `distinct_hosts` asking for more instances than there are machines to hold
/// one each (Phase C, E7) — refused before publishing, as Nomad's plan does,
/// rather than published with a slot that can never be placed.
fn distinct_hosts_issue(
    dep: &Deployment,
    policy: &mesh_proto::DeploymentPolicy,
    matching: u32,
) -> Option<String> {
    let Count::Fixed(n) = dep.count else {
        return None;
    };
    (mesh_proto::PlacementSpec::from_env(&policy.instance_env).distinct_hosts && n > matching).then(
        || {
            format!(
                concat!(
                    "distinct_hosts puts at most one instance on a machine, and count = {} but ",
                    "only {} machine{} could take one — lower the count, widen the targets, or ",
                    "drop distinct_hosts"
                ),
                n,
                matching,
                if matching == 1 { "" } else { "s" }
            )
        },
    )
}

fn change_effect(field: &str, from: &str, to: &str) -> &'static str {
    match field.split('.').next().unwrap_or(field) {
        "update" | "disconnect" | "migrate" => mesh_proto::PLAN_KNOB,
        "distinct_hosts" | "distinct_property" | "affinity" | "spread" | "priority" => {
            mesh_proto::PLAN_PLACEMENT
        }
        "description" | "paused" => mesh_proto::PLAN_RECORD,
        "count" if from != "\"system\"" && to != "\"system\"" => mesh_proto::PLAN_SCALE,
        _ => mesh_proto::PLAN_ROLLS,
    }
}

/// A rendered deployment as dotted `field → value` pairs.
fn flatten(dep: &Deployment, policy: &mesh_proto::DeploymentPolicy) -> BTreeMap<String, String> {
    fn walk(prefix: &str, table: &toml::Table, out: &mut BTreeMap<String, String>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) => walk(&path, inner, out),
                // `[[network.port]]` and `[[volume]]`: keyed by what names
                // them rather than by position, so reordering two ports is
                // not two changes and adding one in the middle is one.
                toml::Value::Array(items)
                    if !items.is_empty() && items.iter().all(toml::Value::is_table) =>
                {
                    for item in items.iter().filter_map(toml::Value::as_table) {
                        let (id_key, id) = ["label", "name", "select"]
                            .iter()
                            .find_map(|k| {
                                item.get(*k).and_then(toml::Value::as_str).map(|v| (*k, v))
                            })
                            .unwrap_or(("", "?"));
                        let at = format!("{path}.{id}");
                        out.insert(at.clone(), "declared".into());
                        let rest: toml::Table = item
                            .iter()
                            .filter(|(k, _)| k.as_str() != id_key)
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        walk(&at, &rest, out);
                    }
                }
                toml::Value::String(s) => {
                    out.insert(path, format!("{s:?}"));
                }
                other => {
                    out.insert(path, other.to_string());
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    if let Ok(root) = toml::from_str::<toml::Table>(&to_toml(dep, policy))
        && let Some(toml::Value::Table(body)) = root.get("deployment")
    {
        walk("", body, &mut out);
    }
    // Not changes a person makes: the name is how the two sides were
    // matched, and the generation is what this is deciding.
    out.remove("name");
    out.remove("generation");
    out
}

/// The refusal for a static port on more than one instance, in one place
/// (N2 landmine 1): a file that asks for it and a `scale` that would cause
/// it say the same sentence.
fn static_port_refusal(count: u32, ports: &mesh_proto::PortRequests) -> CoreError {
    let labels: Vec<&str> = ports
        .0
        .iter()
        .filter(|p| p.is_static())
        .map(|p| p.label.as_str())
        .collect();
    CoreError::State(format!(
        concat!(
            "count = {} with a static port ({}) cannot work: two instances on one node ",
            "cannot both bind it, and nothing guarantees they land on different nodes. ",
            "Use a dynamic port (drop `static`) and read MESHWARE_PORT_{}, set ",
            "distinct_hosts = true so no two share a machine, or set count = 1"
        ),
        count,
        labels.join(", "),
        labels
            .first()
            .map(|l| mesh_proto::ports::label_key_suffix(l))
            .unwrap_or_default()
    ))
}

/// Resize a fixed deployment (Phase B, E3) — `meshnode deployment scale`
/// and Director's *Scale*. Returns the size it had.
///
/// Only the count changes, and [`rolls`] does not count a count, so the
/// generation stays where it is: the reconciler starts the slots that are
/// new and retires the ones past the end, and nothing that stays restarts.
pub fn scale(parsed: &mut Parsed, count: u32) -> Result<u32> {
    let name = parsed.deployment.name.clone();
    let Count::Fixed(previous) = parsed.deployment.count else {
        return Err(CoreError::State(format!(
            concat!(
                "{} runs one instance on every matching node (count = \"system\"), so ",
                "there is no size to scale — change its targets to change where it runs"
            ),
            name
        )));
    };
    if count == 0 {
        return Err(CoreError::State(format!(
            concat!(
                "{0} cannot scale to 0: a deployment of zero instances is a removal. ",
                "`deployment pause {0}` stops reconciling it; `deployment rm {0}` retires it"
            ),
            name
        )));
    }
    let ports = parsed.policy.ports();
    let distinct = mesh_proto::PlacementSpec::from_env(&parsed.policy.instance_env).distinct_hosts;
    if count > 1 && ports.any_static() && !distinct {
        return Err(static_port_refusal(count, &ports));
    }
    parsed.deployment.count = Count::Fixed(count);
    Ok(previous)
}

/// Replace every instance without changing the definition (Phase B, E3) —
/// `meshnode deployment restart` and Director's *Restart*: the next
/// generation of the same record, rolled under its own policy.
///
/// Refused while paused, because the roll would wait for a reconciler that
/// is not running and nothing would say so.
pub fn restart(parsed: &mut Parsed) -> Result<()> {
    if parsed.deployment.paused {
        return Err(CoreError::State(format!(
            concat!(
                "{0} is paused, so a restart would wait for a reconciler that is not ",
                "running. `deployment resume {0}` first"
            ),
            parsed.deployment.name
        )));
    }
    parsed.deployment.generation = parsed.deployment.generation.saturating_add(1);
    Ok(())
}

/// The running instance in one slot of a status view (Phase B, E3) — what
/// `restart --slot` replaces. A slot is a replica number, or the node name
/// of a `system` deployment's instance, exactly as `status` prints it.
pub fn instance_in_slot<'a>(
    view: &'a DeploymentStatusView,
    slot: &str,
) -> Result<&'a DeploymentInstanceRow> {
    let running =
        |i: &&DeploymentInstanceRow| i.generation == view.generation && i.state != "absent";
    view.instances
        .iter()
        .filter(running)
        .find(|i| i.slot == slot)
        .ok_or_else(|| {
            let slots: Vec<&str> = view
                .instances
                .iter()
                .filter(running)
                .map(|i| i.slot.as_str())
                .collect();
            CoreError::State(if slots.is_empty() {
                format!("{} has no running instance to restart", view.name)
            } else {
                format!(
                    "{} has no running instance in slot {slot:?}; its slots are {}",
                    view.name,
                    slots.join(", ")
                )
            })
        })
}

/// The instance a command about *one* instance means (Phase B, E3): the
/// named slot, or — when none is named — the only running one. A deployment
/// of several refuses to guess and lists the slots to choose from, because
/// streaming the logs of whichever instance sorted first is an answer to a
/// question nobody asked.
pub fn pick_instance<'a>(
    view: &'a DeploymentStatusView,
    slot: Option<&str>,
) -> Result<&'a DeploymentInstanceRow> {
    if let Some(slot) = slot {
        return instance_in_slot(view, slot);
    }
    let running: Vec<&DeploymentInstanceRow> = view
        .instances
        .iter()
        .filter(|i| i.generation == view.generation && i.state != "absent")
        .collect();
    match running.as_slice() {
        [only] => Ok(only),
        [] => Err(CoreError::State(format!(
            "{} has no running instance",
            view.name
        ))),
        many => Err(CoreError::State(format!(
            "{} has {} instances; name one with --slot: {}",
            view.name,
            many.len(),
            many.iter()
                .map(|i| i.slot.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The runtime a deployment's instances run under — `container`, `bash`, …
/// — from its published service version; empty when that version is not
/// published here.
pub fn runtime_of(dep: &Deployment, state: &MeshState) -> String {
    state
        .services
        .get(&dep.service)
        .and_then(|entry| entry.versions.get(&dep.service_version))
        .map(|version| version.runtime.as_str().to_string())
        .unwrap_or_default()
}

/// One instance slot up close (Phase B, E3 and U1) — `deployment instance`
/// and Director's instance panel.
///
/// Every incarnation the slot has had, across the current generation and
/// the prior ones still in reach, oldest first — which is the timeline:
/// *started on beta, lost, replaced on alpha*. The live row comes from the
/// same builder `status` uses. `stats` is left for the caller, because it is
/// measured on the executor and never read from state.
///
/// `None` when `slot` names no slot this deployment can have: not a number
/// for a fixed count, or no member of that name for a `system` one.
pub fn instance_detail(
    dep: &Deployment,
    state: &MeshState,
    slot: &str,
    unreachable: impl Fn(&NodeId) -> bool,
) -> Option<mesh_proto::DeploymentInstanceDetail> {
    let slot_id = match dep.count {
        Count::Fixed(_) => Slot::Replica(slot.parse::<u32>().ok()?),
        Count::System => state
            .members
            .values()
            .find(|m| m.info.name == slot || m.info.node_id.short() == slot)
            .map(|m| Slot::Node(m.info.node_id))?,
    };
    let name_of = |node: &NodeId| {
        state
            .members
            .get(node)
            .map(|m| m.info.name.clone())
            .unwrap_or_else(|| node.short())
    };
    let mut incarnations = Vec::new();
    for generation in dep.generation.saturating_sub(GEN_LOOKBACK)..=dep.generation {
        for incarnation in recent_incarnations(state, &dep.name, generation, &slot_id) {
            let id = instance_id(&dep.name, generation, &slot_id, incarnation);
            let Some(job) = state.jobs.get(&id) else {
                break;
            };
            incarnations.push(mesh_proto::IncarnationRow {
                job_id: id,
                generation,
                incarnation,
                node: name_of(&job.executor),
                state: job.state.as_str().to_string(),
                created_at_micros: job.created_at_micros,
                started_at_micros: job.started_at_micros,
                finished_at_micros: job.finished_at_micros,
                exit_code: job.exit_code,
            });
        }
    }
    let health = HealthIndex::build(state);
    let current = current_instance(dep, state, &slot_id).map(|(id, facts)| {
        let away = unreachable(&facts.executor);
        instance_row(dep, state, &health, &slot_id, id, &facts, away)
    });
    Some(mesh_proto::DeploymentInstanceDetail {
        deployment: dep.name.clone(),
        slot: slot_id.label(state),
        generation: dep.generation,
        runtime: runtime_of(dep, state),
        current,
        incarnations,
        stats: None,
    })
}

/// What a `deployment.stalled` audit row appends to name the version that
/// stalled (Phase A, §2.5a): `… (version 7)`.
pub const STALLED_VERSION_TAG: &str = "(version ";

/// The service versions a deployment's audit history says stalled a
/// rollout (Phase A, §2.5a).
///
/// Two kinds of row say so. A `deployment.stalled` row names its version at
/// the end; a `deployment.auto_revert` row names the version it reverted
/// *from*, which is how a stall recorded before the version was written
/// down still counts. A row that does not parse is ignored: an unreadable
/// row makes the answer less informed, never wrong.
pub fn stalled_versions<'a>(
    rows: impl Iterator<Item = (&'a str, &'a str)>,
) -> std::collections::BTreeSet<u32> {
    let mut out = std::collections::BTreeSet::new();
    for (action, detail) in rows {
        let found = match action {
            "deployment.stalled" => number_between(detail, STALLED_VERSION_TAG, ")"),
            "deployment.auto_revert" => number_between(detail, "reverted ", " ->"),
            _ => None,
        };
        out.extend(found);
    }
    out
}

/// The number between the *last* `open` in `text` and the next `close`.
/// Last, because the free-text reason in front of it could contain either.
fn number_between(text: &str, open: &str, close: &str) -> Option<u32> {
    let start = text.rfind(open)? + open.len();
    let rest = &text[start..];
    rest[..rest.find(close)?].trim().parse().ok()
}

/// Where `auto_revert` goes from `current` (Phase A, §2.5a).
///
/// The newest published version below `current` whose rollout never
/// stalled. When every earlier version stalled too, the one directly below
/// it — and `true`, so the revert can say it is going somewhere that has
/// failed before rather than pretending otherwise. With nothing published
/// to consult, one below, as it always was.
pub fn revert_target(
    current: u32,
    published: &[u32],
    stalled: &std::collections::BTreeSet<u32>,
) -> (Option<u32>, bool) {
    let mut earlier: Vec<u32> = published
        .iter()
        .copied()
        .filter(|v| *v >= 1 && *v < current)
        .collect();
    if published.is_empty() {
        return (current.checked_sub(1).filter(|v| *v >= 1), false);
    }
    earlier.sort_unstable();
    match earlier.iter().rev().find(|v| !stalled.contains(v)) {
        Some(v) => (Some(*v), false),
        None => (earlier.last().copied(), earlier.last().is_some()),
    }
}

/// Render a deployment record back to the TOML a human would edit — the
/// `to_yaml`-for-schedules convention, so `--output` round-trips into `put`.
pub fn to_toml(dep: &Deployment, policy: &mesh_proto::DeploymentPolicy) -> String {
    let mut out = String::new();
    out.push_str("[deployment]\n");
    out.push_str(&format!("name    = {:?}\n", dep.name));
    out.push_str(&format!("service = {:?}\n", dep.service_ref()));
    out.push_str(&format!(
        "count   = {}\n",
        match dep.count {
            Count::System => "\"system\"".to_string(),
            Count::Fixed(n) => n.to_string(),
        }
    ));
    out.push_str(&format!("targets = {:?}\n", dep.targets));
    if !dep.description.is_empty() {
        out.push_str(&format!("description = {:?}\n", dep.description));
    }
    if dep.paused {
        out.push_str("paused = true\n");
    }
    out.push_str(&format!("generation = {}\n", dep.generation));
    // Phase C. A scalar of `[deployment]`, so it is written before the first
    // sub-table or it would land inside one.
    let placement = mesh_proto::PlacementSpec::from_env(&policy.instance_env);
    if placement.distinct_hosts {
        out.push_str("distinct_hosts = true\n");
    }
    if let Some(p) = policy.env_value(mesh_proto::REQ_PRIORITY) {
        out.push_str(&format!("priority = {p}\n"));
    }
    if !dep.inputs.is_empty() {
        out.push_str("\n[deployment.inputs]\n");
        for (k, v) in &dep.inputs {
            out.push_str(&format!("{k} = {v:?}\n"));
        }
    }
    out.push_str("\n[deployment.restart]\n");
    out.push_str(&format!(
        "backoff = \"{}s..{}s\"\n",
        dep.restart.backoff_min_secs, dep.restart.backoff_max_secs
    ));
    out.push_str(&format!(
        "budget  = \"{} in {}s\"\n",
        dep.restart.budget_failures, dep.restart.budget_window_secs
    ));
    if let Some(h) = &dep.health {
        // **A check that names its port by label must render the label.**
        // `HealthCheck.port` is 0 for one of those — the number is not
        // known until a node allocates it — and printing that 0 produced a
        // record that would not re-parse (`a http health check needs a
        // `port``). Which made `previous_from_row` see every re-publish as
        // a first one: no generation bump, no rollout, `✓ rollout complete`
        // over an unchanged mesh. The same silent no-op N6 was written to
        // kill, arriving by way of the renderer.
        let label = policy.env_value(mesh_proto::REQ_HEALTH_PORT);
        let port = match &label {
            Some(l) => format!("{l:?}"),
            None => h.port.to_string(),
        };
        out.push_str("\n[deployment.health]\n");
        out.push_str(&format!("kind     = {:?}\n", h.kind.as_str()));
        match h.kind {
            HealthKind::Exec => out.push_str(&format!("command  = {:?}\n", h.command)),
            HealthKind::Tcp => out.push_str(&format!("port     = {port}\n")),
            HealthKind::Http => {
                out.push_str(&format!("path     = {:?}\n", h.path));
                out.push_str(&format!("port     = {port}\n"));
            }
        }
        out.push_str(&format!("interval = \"{}s\"\n", h.interval_secs));
        out.push_str(&format!("grace    = \"{}s\"\n", h.grace_secs));
        if h.timeout_secs > 0 {
            out.push_str(&format!("timeout  = \"{}s\"\n", h.timeout_secs));
        }
        if h.failures_to_unhealthy > 0 {
            out.push_str(&format!("failures = {}\n", h.failures_to_unhealthy));
        }
    }
    out.push_str("\n[deployment.update]\n");
    out.push_str(&format!(
        "strategy        = {:?}\n",
        dep.update.strategy.as_str()
    ));
    out.push_str(&format!(
        "max_unavailable = {}\n",
        dep.update.max_unavailable
    ));
    out.push_str(&format!(
        "gate            = {:?}\n",
        if dep.update.gate_on_health {
            "health"
        } else {
            "running"
        }
    ));
    // N1 P3. The same `[deployment.update]` table the person wrote them
    // in, even though they are a second record on the wire — the split is
    // a signature constraint, not something a reader should have to know
    // about, and a second `[deployment.update]` header would not even be
    // valid TOML.
    if policy.canary > 0 {
        out.push_str(&format!("canary          = {}\n", policy.canary));
    }
    if policy.auto_revert {
        out.push_str("auto_revert     = true\n");
    }
    if policy.progress_deadline_secs > 0 {
        out.push_str(&format!(
            "progress_deadline = \"{}s\"\n",
            policy.progress_deadline_secs
        ));
    } else if policy.auto_revert || policy.canary > 0 {
        // A deliberate "no deadline", spelled back as the word — otherwise
        // the re-parse fills in the default this file declined, and every
        // publish compares as a change to the policy (Phase A, §2.4).
        out.push_str(&format!("progress_deadline = {:?}\n", mesh_proto::NEVER));
    }
    if policy.min_healthy_secs > 0 {
        out.push_str(&format!(
            "min_healthy_time  = \"{}s\"\n",
            policy.min_healthy_secs
        ));
    }
    // N2 P2. Rendered from the environment the policy carries, parsed back
    // by the same function an executor uses — so `show --output toml` can
    // only ever print what the instances are actually started with.
    let declared = policy.ports();
    if !declared.is_empty() {
        out.push_str("\n[deployment.network]\nmode = \"host\"\n");
        for port in &declared.0 {
            out.push_str("\n[[deployment.network.port]]\n");
            out.push_str(&format!("label  = {:?}\n", port.label));
            if port.to > 0 {
                out.push_str(&format!("to     = {}\n", port.to));
            }
            if let Some(fixed) = port.fixed {
                out.push_str(&format!("static = {fixed}\n"));
            }
        }
    }
    // `[deployment.resources]`. Absent from this render until it was
    // noticed that a re-parse then saw *no* requirement where the file had
    // declared one — the opposite failure to the health port above, and
    // just as bad: a comparison against it makes every unchanged
    // re-publish look like a change and rolls the whole deployment.
    let reqs = mesh_proto::Requirements::from_env(&policy.instance_env);
    if !reqs.is_unstated() {
        out.push_str("\n[deployment.resources]\n");
        if reqs.cpus > 0 {
            out.push_str(&format!("cpus      = {}\n", reqs.cpus));
        }
        if reqs.mem_mb > 0 {
            out.push_str(&format!("memory_mb = {}\n", reqs.mem_mb));
        }
        if reqs.disk_mb > 0 {
            out.push_str(&format!("disk_mb   = {}\n", reqs.disk_mb));
        }
    }
    // N5 — the volumes.
    for v in &mesh_proto::VolumeRequests::from_env(&policy.instance_env).0 {
        out.push_str("\n[[deployment.volume]]\n");
        out.push_str(&format!("name = {:?}\n", v.name));
        if !v.path.is_empty() {
            out.push_str(&format!("path = {:?}\n", v.path));
        }
        out.push_str(&format!("mode = {:?}\n", v.mode.as_str()));
    }
    // N3 — the registration.
    if let Some(name) = policy.env_value(mesh_proto::REQ_SERVICE) {
        out.push_str("\n[deployment.register]\n");
        out.push_str(&format!("name = {name:?}\n"));
        if let Some(tags) = policy.env_value(mesh_proto::REQ_SERVICE_TAGS) {
            let list: Vec<String> = tags.split(',').map(|t| format!("{t:?}")).collect();
            out.push_str(&format!("tags = [{}]\n", list.join(", ")));
        }
        if let Some(label) = policy.env_value(mesh_proto::REQ_SERVICE_PORT) {
            out.push_str(&format!("port = {label:?}\n"));
        }
        if let Some(secs) = policy.env_value(mesh_proto::REQ_SHUTDOWN_DELAY) {
            out.push_str(&format!("shutdown_delay = \"{secs}s\"\n"));
        }
        if let Some(secs) = policy.env_value(mesh_proto::REQ_KILL_TIMEOUT) {
            out.push_str(&format!("kill_timeout = \"{secs}s\"\n"));
        }
    }
    // N1 P4. A fixed-count policy whose `lost_after` is zero renders the
    // word `never`: with nothing in the file the re-parse would fill in the
    // Phase A default instead (§2.1). That is also how a deployment
    // published before Phase A reads back — accurately, since waiting for
    // its node forever is what it has been doing.
    let never_lost = matches!(dep.count, Count::Fixed(_)) && policy.lost_after_secs == 0;
    if policy.lost_after_secs > 0
        || never_lost
        || policy.replace_unknown
        || policy.reconcile != mesh_proto::Reconcile::KeepOriginal
    {
        out.push_str("\n[deployment.disconnect]\n");
        if policy.lost_after_secs > 0 {
            out.push_str(&format!("lost_after = \"{}s\"\n", policy.lost_after_secs));
        } else if never_lost {
            out.push_str(&format!("lost_after = {:?}\n", mesh_proto::NEVER));
        }
        if policy.replace_unknown {
            out.push_str("replace    = true\n");
        }
        out.push_str(&format!("reconcile  = {:?}\n", policy.reconcile.as_str()));
    }
    // Phase C, E9. Only when it says something the defaults do not.
    let migrate = mesh_proto::MigratePolicy::from_env(&policy.instance_env);
    if migrate != mesh_proto::MigratePolicy::default() {
        out.push_str("\n[deployment.migrate]\n");
        out.push_str(&format!("max_parallel     = {}\n", migrate.max_parallel));
        out.push_str(&format!(
            "min_healthy_time = \"{}s\"\n",
            migrate.min_healthy_secs
        ));
        out.push_str(&format!(
            "healthy_deadline = \"{}s\"\n",
            migrate.healthy_deadline_secs
        ));
    }
    // Phase C, E7/E8 — rendered from the side-car entry and read back by the
    // same parse, so `show` prints the rules instances are placed by.
    if let Some(d) = &placement.distinct_property {
        out.push_str("\n[deployment.distinct_property]\n");
        out.push_str(&format!("attribute = {:?}\n", d.attribute));
        out.push_str(&format!("limit     = {}\n", d.limit));
    }
    for a in &placement.affinity {
        out.push_str("\n[[deployment.affinity]]\n");
        out.push_str(&format!("select = {:?}\n", a.select));
        out.push_str(&format!("weight = {}\n", a.weight));
    }
    if let Some(s) = &placement.spread {
        out.push_str("\n[deployment.spread]\n");
        out.push_str(&format!("attribute = {:?}\n", s.attribute));
        if !s.targets.is_empty() {
            out.push_str("\n[deployment.spread.targets]\n");
            for (value, pct) in &s.targets {
                out.push_str(&format!("{value:?} = {pct}\n"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// `plan_reconcile` with the inert policy and a mesh where every node
    /// is reachable — what every test written before N1 P3/P4 assumed, and
    /// still assumes.
    fn plan(dep: &Deployment, state: &MeshState, cands: &[MemberInfo], now: u64) -> ReconcilePlan {
        plan_reconcile(
            dep,
            state,
            cands,
            now,
            &mesh_proto::DeploymentPolicy::default(),
            |_| None,
        )
    }

    use super::*;
    use crate::state::{MemberEntry, ServiceEntry};
    use mesh_proto::{Hlc, JobSummary};

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    fn hlc() -> Hlc {
        Hlc {
            wall_micros: 1,
            logical: 0,
        }
    }

    fn member(state: &mut MeshState, n: u8, name: &str, tags: Vec<String>) {
        let info = MemberInfo {
            node_id: node(n),
            name: name.to_string(),
            addrs: vec![],
            tags,
            slots: 4,
            joined_at_micros: 1,
        };
        state.members.insert(
            node(n),
            MemberEntry {
                announced_tags: info.tags.clone(),
                info,
                added_hlc: hlc(),
                updated_hlc: hlc(),
                left: false,
                left_hlc: None,
            },
        );
    }

    fn service(state: &mut MeshState, name: &str, version: u32) {
        let def = mesh_proto::ServiceVersion {
            name: name.to_string(),
            version,
            runtime: mesh_proto::Runtime::Bash,
            bundle_id: mesh_proto::BundleId([0u8; 32]),
            working_dir: String::new(),
            entrypoint: "run.sh".into(),
            contract: None,
            env: vec![],
            description: String::new(),
            tags: vec![],
            created_at_micros: 1,
            created_by: UserId([0u8; 32]),
            source: None,
            signature: vec![],
        };
        let entry = state
            .services
            .entry(name.to_string())
            .or_insert_with(|| ServiceEntry {
                versions: BTreeMap::new(),
                head: version,
                removed: false,
            });
        entry.versions.insert(version, def);
        entry.head = entry.head.max(version);
    }

    fn put_job(
        state: &mut MeshState,
        id: JobId,
        executor: NodeId,
        st: JobState,
        finished: Option<u64>,
    ) {
        state.jobs.insert(
            id,
            JobSummary {
                job_id: id,
                cmd_preview: "svc".into(),
                runtime: "bash".into(),
                submitter: node(1),
                submitter_user: UserId([0u8; 32]),
                executor,
                state: st,
                exit_code: None,
                created_at_micros: 1,
                started_at_micros: Some(1),
                finished_at_micros: finished,
                fanout: None,
                updated_hlc: hlc(),
            },
        );
    }

    fn deployment(name: &str, count: Count) -> Deployment {
        Deployment {
            name: name.into(),
            service: "collector".into(),
            service_version: 7,
            count,
            targets: "any".into(),
            restart: RestartPolicy::default(),
            health: None,
            update: UpdatePolicy::default(),
            generation: 0,
            inputs: vec![],
            paused: false,
            description: String::new(),
            created_by: UserId([0u8; 32]),
            created_at_micros: 1,
            signature: vec![],
        }
    }

    fn candidates(state: &MeshState) -> Vec<MemberInfo> {
        state.members.values().map(|m| m.info.clone()).collect()
    }

    // ---- Phase B hardening: placement that cannot get stuck --------------

    fn three_nodes() -> (MeshState, Deployment) {
        let mut state = MeshState::default();
        for n in 1..=3u8 {
            member(&mut state, n, &format!("n{n}"), vec![]);
        }
        service(&mut state, "collector", 7);
        (state, deployment("web", Count::Fixed(1)))
    }

    /// **The bug, as it was found on a real mesh.** A deployment asked for
    /// four cores; one of four machines had two. The planner sent a slot to
    /// that machine every tick and it refused every time. A machine that can
    /// never fit the request is not a candidate at all.
    #[test]
    fn a_machine_too_small_for_the_request_is_not_in_the_pool() {
        let mut state = MeshState::default();
        member(
            &mut state,
            1,
            "big",
            vec!["cap.cores=16".into(), "cap.mem_mb=65536".into()],
        );
        member(
            &mut state,
            2,
            "small",
            vec!["cap.cores=2".into(), "cap.mem_mb=16384".into()],
        );
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(2));
        state.deployment_policies.insert(
            "web".into(),
            crate::state::DeploymentPolicyEntry {
                policy: mesh_proto::DeploymentPolicy {
                    deployment: "web".into(),
                    instance_env: mesh_proto::Requirements {
                        cpus: 4,
                        ..Default::default()
                    }
                    .to_env(),
                    ..Default::default()
                },
                updated_hlc: hlc(),
            },
        );
        let pool: Vec<NodeId> = placement_pool(
            &state,
            &dep,
            node(1),
            |_| mesh_proto::Liveness::Alive,
            false,
        )
        .iter()
        .map(|m| m.node_id)
        .collect();
        assert_eq!(
            pool,
            vec![node(1)],
            "a 2-core machine never fits a 4-core instance"
        );
    }

    /// A refusal steers the next plan; every start knows where to spill; a
    /// slot every machine refused waits; and a start that may already be
    /// running stays where it is and never spills.
    #[test]
    fn a_refusal_steers_the_plan_and_a_pinned_start_never_spills() {
        let (state, dep) = three_nodes();
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy::default();
        let first = plan_with(&dep, &state, &cands, 1_000_000, &policy, &[]);
        let start = first.starts[0].clone();
        assert_eq!(
            start.alternates.len(),
            2,
            "the other two machines, in order"
        );
        assert!(!start.alternates.contains(&start.node));

        let mut hints = PlacementHints::default();
        hints.avoid.insert(start.job_id, vec![start.node]);
        let next =
            plan_reconcile_hinted(&dep, &state, &cands, 1_000_000, &policy, |_| None, &hints);
        assert_eq!(
            next.starts[0].node, start.alternates[0],
            "the refusing machine is not asked first again"
        );

        hints
            .avoid
            .insert(start.job_id, cands.iter().map(|m| m.node_id).collect());
        let waiting =
            plan_reconcile_hinted(&dep, &state, &cands, 1_000_000, &policy, |_| None, &hints);
        assert!(
            waiting.starts.is_empty(),
            "every machine refused a moment ago: wait, do not hammer"
        );

        let mut pinned = PlacementHints::default();
        pinned.pinned.insert(start.job_id, start.alternates[1]);
        let stay =
            plan_reconcile_hinted(&dep, &state, &cands, 1_000_000, &policy, |_| None, &pinned);
        assert_eq!(stay.starts[0].node, start.alternates[1]);
        assert!(
            stay.starts[0].alternates.is_empty(),
            "a start that may be running never spills"
        );
    }

    /// The same name, removed and created again, derives the same ids; the
    /// removed deployment's failures must not hold the new one back.
    #[test]
    fn failures_from_before_the_name_was_removed_do_not_hold_a_new_deployment_back() {
        let (mut state, dep) = three_nodes();
        let old = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, old, node(1), JobState::Failed, Some(100));
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy::default();
        let held = plan_with(&dep, &state, &cands, 200, &policy, &[]);
        assert!(
            held.starts.is_empty(),
            "a failure a moment ago backs the slot off"
        );

        state
            .deployment_history
            .entry("web".into())
            .or_default()
            .floor = mesh_proto::Hlc {
            wall_micros: 150,
            logical: 0,
        };
        let fresh = plan_with(&dep, &state, &cands, 200, &policy, &[]);
        assert_eq!(
            fresh.starts.len(),
            1,
            "that failure was the removed deployment's"
        );
        assert_eq!(
            fresh.starts[0].job_id,
            instance_id("web", 0, &Slot::Replica(0), 1),
            "its record still holds its id"
        );
    }

    /// A stall where the missing instance was never started, and the one
    /// that did start is healthy, blames placement — not the release.
    #[test]
    fn a_stall_from_a_slot_never_started_blames_placement_not_the_release() {
        let (mut state, mut dep) = three_nodes();
        dep.count = Count::Fixed(2);
        dep.health = Some(mesh_proto::HealthCheck::default());
        let running = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, running, node(1), JobState::Running, None);
        healthy(&mut state, node(1), running, "web", 0, 1_000_000);
        let policy = mesh_proto::DeploymentPolicy {
            progress_deadline_secs: 60,
            ..Default::default()
        };
        let cands = candidates(&state);
        let mut hints = PlacementHints::default();
        hints.avoid.insert(
            instance_id("web", 0, &Slot::Replica(1), 0),
            cands.iter().map(|m| m.node_id).collect(),
        );
        let plan =
            plan_reconcile_hinted(&dep, &state, &cands, 62_000_000, &policy, |_| None, &hints);
        assert!(plan.stall_is_placement, "{:?}", plan.stalled);
        assert!(
            plan.stalled
                .as_deref()
                .is_some_and(|s| s.contains("could not be started")),
            "{:?}",
            plan.stalled
        );
    }

    #[test]
    fn placement_memory_forgets_what_expired_and_what_arrived() {
        let mut mem = PlacementMemory::default();
        let a = instance_id("web", 0, &Slot::Replica(0), 0);
        let b = instance_id("web", 0, &Slot::Replica(1), 0);
        mem.refused(a, node(1), 0);
        mem.placed(b, node(2), 0);
        let h = mem.hints(1);
        assert_eq!(h.avoid.get(&a), Some(&vec![node(1)]));
        assert_eq!(h.pinned.get(&b), Some(&node(2)));
        mem.refused(b, node(2), 5);
        assert!(
            !mem.hints(6).pinned.contains_key(&b),
            "refused by its own machine: never had it"
        );
        let later = PlacementMemory::REFUSAL_TTL_MICROS + 10;
        mem.prune(later, |id| *id == b);
        let h = mem.hints(later);
        assert!(h.avoid.is_empty() && h.pinned.is_empty(), "{h:?}");
    }

    // ---- Phase B, E1: history -------------------------------------------

    fn at(wall: u64) -> mesh_proto::Hlc {
        mesh_proto::Hlc {
            wall_micros: wall,
            logical: 0,
        }
    }

    fn version_at(generation: u32) -> mesh_proto::DeploymentVersion {
        let mut d = deployment("web", Count::Fixed(1));
        d.generation = generation;
        mesh_proto::DeploymentVersion {
            deployment: d,
            policy: mesh_proto::DeploymentPolicy {
                deployment: "web".into(),
                ..Default::default()
            },
        }
    }

    /// The kept set is the newest ten whatever order the ops arrive in, and
    /// a removal scopes it. The order that nearly diverged: the removed
    /// deployment's higher generations arriving *after* the new one's low
    /// ones, which a set ordered by generation number would have let evict
    /// them.
    #[test]
    fn history_is_the_newest_ten_in_any_order_and_a_removal_resets_it() {
        use crate::state::DeploymentHistory;
        let old: Vec<_> = (0..15u32)
            .map(|g| (version_at(g), at(100 + u64::from(g))))
            .collect();
        let removal = at(500);
        let new: Vec<_> = (0..3u32)
            .map(|g| (version_at(g), at(600 + u64::from(g))))
            .collect();
        let keys = |h: &DeploymentHistory| h.versions.keys().copied().collect::<Vec<_>>();

        let mut forward = DeploymentHistory::default();
        for (v, t) in &old {
            forward.record(v, *t);
        }
        assert_eq!(keys(&forward), (5..15).collect::<Vec<_>>());
        let mut backward = DeploymentHistory::default();
        for (v, t) in old.iter().rev() {
            backward.record(v, *t);
        }
        assert_eq!(keys(&backward), keys(&forward));

        forward.raise_floor(removal);
        for (v, t) in &new {
            forward.record(v, *t);
        }
        let mut late = DeploymentHistory::default();
        for (v, t) in new.iter().chain(&old) {
            late.record(v, *t);
        }
        late.raise_floor(removal);
        for h in [&forward, &late] {
            assert_eq!(keys(h), vec![0, 1, 2]);
            assert_eq!(
                h.versions[&0].1,
                at(600),
                "generation 0 is the new deployment's"
            );
        }
    }

    /// A version is superseded by a re-publish of its own generation and by
    /// one twenty on — never by the next roll, which is what history needs.
    #[test]
    fn a_version_compacts_per_generation_on_a_ring_of_twenty() {
        let retention = mesh_proto::RetentionPolicy::default();
        let class = |g: u32| {
            crate::compact::compaction(
                &OpBody::DeploymentVersionPut {
                    version: Box::new(version_at(g)),
                },
                &retention,
            )
        };
        assert_eq!(class(3), class(3));
        assert_ne!(class(3), class(4));
        assert_ne!(class(3), class(13));
        assert_eq!(class(3), class(23));
    }

    /// Rolling back brings back the side-car too, keeps a pause, and moves
    /// the generation only when instances would be replaced.
    #[test]
    fn a_rollback_to_a_version_restores_the_whole_file() {
        let mut older = version_at(2);
        older.deployment.service_version = 5;
        older.policy.canary = 1;
        let mut current = deployment("web", Count::Fixed(1));
        current.generation = 4;
        current.paused = true;
        let mut parsed = Parsed {
            inline: None,
            knobs: Default::default(),
            deployment: current.clone(),
            policy: mesh_proto::DeploymentPolicy {
                deployment: "web".into(),
                ..Default::default()
            },
        };
        revert_to(&mut parsed, &older).unwrap();
        assert_eq!(parsed.deployment.service_version, 5);
        assert_eq!(parsed.policy.canary, 1, "the side-car came back with it");
        assert!(parsed.deployment.paused, "a rollback does not resume");
        assert_eq!(parsed.deployment.generation, 5, "a new version rolls");

        // Differing only in its count: a resize at the same generation.
        let mut sized = version_at(3);
        sized.deployment.count = Count::Fixed(3);
        let mut parsed = Parsed {
            inline: None,
            knobs: Default::default(),
            deployment: current.clone(),
            policy: sized.policy.clone(),
        };
        revert_to(&mut parsed, &sized).unwrap();
        assert_eq!(parsed.deployment.generation, 4);
        assert_eq!(parsed.deployment.count, Count::Fixed(3));

        // Nothing to go back to.
        let mut same = version_at(1);
        same.deployment.paused = true;
        let mut parsed = Parsed {
            inline: None,
            knobs: Default::default(),
            deployment: current,
            policy: same.policy.clone(),
        };
        assert!(revert_to(&mut parsed, &same).is_err());
    }

    // ---- N7: restart is not the same decision as reschedule -----------

    /// **The bug, exactly as it was reported.** Slot 0 failed six times on
    /// node `b`, the deployment went degraded, and it stayed `1/1/2` with
    /// node `a` sitting idle and able.
    ///
    /// Two things were wrong and they compounded. The budget gate stopped
    /// restarts for the *whole deployment* until the failures aged out of
    /// the window — so retries came in a burst every ten minutes rather
    /// than never, which looks identical from the outside. And every one of
    /// those retries went straight back to node `b`, because the
    /// sibling-spread preference ranked "a machine this deployment is not
    /// already on" above "a machine that just failed this exact slot".
    #[test]
    fn a_slot_over_budget_is_still_retried_somewhere_else() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        let mut dep = deployment("flap", Count::Fixed(2));
        dep.restart.budget_failures = 5;
        dep.restart.budget_window_secs = 600;
        dep.restart.backoff_min_secs = 1;
        dep.restart.backoff_max_secs = 5;

        // Slot 1 is healthy on node `a`. Slot 0 has failed six times, every
        // one of them on node `b`.
        put_job(
            &mut state,
            instance_id("flap", 0, &Slot::Replica(1), 0),
            node(1),
            JobState::Running,
            None,
        );
        let failed_at = 1_000_000u64;
        for inc in 0..6 {
            put_job(
                &mut state,
                instance_id("flap", 0, &Slot::Replica(0), inc),
                node(2),
                JobState::Failed,
                Some(failed_at),
            );
        }

        // Past the grown delay (budget 5, six failures → one doubling of
        // the 5s maximum = 10s) but still inside the 600s window, so the
        // failures still count and the deployment is still degraded. This
        // is the state the report was taken in.
        let now = failed_at + 15 * 1_000_000;
        let plan = plan(&dep, &state, &candidates(&state), now);

        assert!(
            plan.degraded.is_some(),
            "the operator must still be told this is not staying up"
        );
        let started: Vec<NodeId> = plan.starts.iter().map(|s| s.node).collect();
        assert_eq!(started.len(), 1, "slot 0 must be tried again: {plan:?}");
        assert_eq!(
            started[0],
            node(1),
            "and on the node it has *not* already failed on — the evidence \
             outranks the spread preference, which was sending every retry \
             straight back to the machine that had just failed"
        );
    }

    /// The newest incarnation is found however many there are: the walk
    /// that counted up from 0 stopped at 64.
    #[test]
    fn the_next_incarnation_is_found_past_the_old_ceiling() {
        for n in [0u32, 1, 2, 3, 63, 64, 65, 200] {
            let mut state = MeshState::default();
            member(&mut state, 1, "a", vec![]);
            for inc in 0..n {
                put_job(
                    &mut state,
                    instance_id("flap", 0, &Slot::Replica(0), inc),
                    node(1),
                    JobState::Failed,
                    Some(1),
                );
            }
            assert_eq!(next_incarnation(&state, "flap", 0, &Slot::Replica(0)), n);
            assert_eq!(
                recent_incarnations(&state, "flap", 0, &Slot::Replica(0)),
                n.saturating_sub(MAX_INCARNATION_SCAN)..n
            );
        }
    }

    /// **A slot that had failed 64 times was never tried again.** The walk
    /// stopped at incarnation 63, so the planner asked for 64 on every tick
    /// — a job that already existed, which the daemon skips — and the slot
    /// went quiet for the rest of its generation.
    #[test]
    fn a_slot_past_sixty_four_failures_is_still_retried() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        let dep = deployment("flap", Count::Fixed(1));
        let failed_at = 1_000_000u64;
        for inc in 0..70 {
            put_job(
                &mut state,
                instance_id("flap", 0, &Slot::Replica(0), inc),
                node(1),
                JobState::Failed,
                Some(failed_at),
            );
        }
        let now = failed_at + (RESCHEDULE_MAX_SECS + 60) * 1_000_000;
        let plan = plan(&dep, &state, &candidates(&state), now);
        let started: Vec<u32> = plan.starts.iter().map(|s| s.incarnation).collect();
        assert_eq!(started, [70], "{plan:?}");

        // …and one that is running past 64 is the slot's current copy.
        put_job(
            &mut state,
            instance_id("flap", 0, &Slot::Replica(0), 70),
            node(1),
            JobState::Running,
            None,
        );
        assert!(plan_starts_nothing(&dep, &state, now));
        let (id, _) = current_instance(&dep, &state, &Slot::Replica(0)).expect("seen");
        assert_eq!(id, instance_id("flap", 0, &Slot::Replica(0), 70));
    }

    fn plan_starts_nothing(dep: &Deployment, state: &MeshState, now: u64) -> bool {
        plan(dep, state, &candidates(state), now).starts.is_empty()
    }

    /// A slot backing off after a failure says so — where it failed, how,
    /// how often, and when it is tried again — instead of reading as a slot
    /// no machine would take.
    #[test]
    fn a_failing_slot_is_reported_as_failing_not_as_unplaced() {
        let mut state = MeshState::default();
        member(&mut state, 2, "merope", vec![]);
        let mut dep = deployment("whoami", Count::System);
        dep.restart.budget_failures = 5;
        dep.restart.budget_window_secs = 600;
        dep.restart.backoff_min_secs = 1;
        dep.restart.backoff_max_secs = 300;
        let slot = Slot::Node(node(2));
        let failed_at = 10_000_000_000u64;
        for inc in 0..3 {
            let id = instance_id("whoami", 0, &slot, inc);
            put_job(&mut state, id, node(2), JobState::Failed, Some(failed_at));
            let job = state.jobs.get_mut(&id).expect("put");
            job.runtime = "container".into();
            job.exit_code = Some(126);
        }

        let row = failed_row(&dep, &state, &slot, failed_at + 1_000_000).expect("a row");
        assert_eq!(row.state, "failed");
        assert_eq!(row.node, "merope");
        assert_eq!(row.job_id, instance_id("whoami", 0, &slot, 2));
        assert!(
            row.health_detail
                .starts_with("the container engine could not start it (exit 126), 3 times"),
            "{}",
            row.health_detail
        );
        // Incarnation 3 next and three failures inside the budget: a 4s
        // delay, 1s of which has passed.
        assert!(
            row.health_detail.contains("starting it again in 3s"),
            "{}",
            row.health_detail
        );
        let later = failed_row(&dep, &state, &slot, failed_at + 60 * 1_000_000).expect("a row");
        assert!(
            later.health_detail.contains("starting it again now"),
            "{}",
            later.health_detail
        );

        // Nothing failed: no such row, and the slot reads as before.
        let empty = MeshState::default();
        assert!(failed_row(&dep, &empty, &slot, failed_at).is_none());
        // Running: the current-instance row speaks for it instead.
        put_job(
            &mut state,
            instance_id("whoami", 0, &slot, 3),
            node(2),
            JobState::Running,
            None,
        );
        assert!(failed_row(&dep, &state, &slot, failed_at).is_none());
    }

    /// The same, through the view Director and `deployment status` read.
    #[test]
    fn the_status_view_carries_the_failing_row() {
        let mut state = MeshState::default();
        member(&mut state, 2, "merope", vec![]);
        let dep = deployment("whoami", Count::System);
        let slot = Slot::Node(node(2));
        put_job(
            &mut state,
            instance_id("whoami", 0, &slot, 0),
            node(2),
            JobState::Failed,
            Some(mesh_proto::now_micros()),
        );
        let view = status_view(
            &dep,
            &state,
            &[0u8; 32],
            &candidates(&state),
            None,
            None,
            None,
            false,
            false,
            vec![],
            vec![],
            None,
            &mesh_proto::DeploymentPolicy::default(),
            |_| false,
        );
        let row = &view.instances[0];
        assert_eq!(
            (row.state.as_str(), row.node.as_str()),
            ("failed", "merope")
        );
        assert!(
            row.health_detail.contains("starting it again"),
            "{}",
            row.health_detail
        );
    }

    /// A slot inside the budget still waits out its ordinary backoff, so
    /// the fix above did not turn a crash loop into a hot loop.
    #[test]
    fn a_slot_inside_its_backoff_is_left_alone() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        let mut dep = deployment("flap", Count::Fixed(1));
        dep.restart.backoff_min_secs = 30;
        dep.restart.backoff_max_secs = 300;
        let failed_at = 1_000_000u64;
        put_job(
            &mut state,
            instance_id("flap", 0, &Slot::Replica(0), 0),
            node(1),
            JobState::Failed,
            Some(failed_at),
        );
        let early = plan(&dep, &state, &candidates(&state), failed_at + 5 * 1_000_000);
        assert!(early.starts.is_empty(), "5s into a 30s backoff");
        let late = plan(
            &dep,
            &state,
            &candidates(&state),
            failed_at + 45 * 1_000_000,
        );
        assert_eq!(late.starts.len(), 1, "45s into a 30s backoff");
    }

    /// **One bad replica must not take a healthy one down with it.** The
    /// budget was counted across the whole deployment and gated the whole
    /// deployment, so a slot that could not start anywhere stopped every
    /// other slot from ever restarting the next time it died.
    #[test]
    fn a_failing_slot_does_not_stop_a_healthy_one_restarting() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        let mut dep = deployment("flap", Count::Fixed(2));
        dep.restart.budget_failures = 5;
        dep.restart.budget_window_secs = 600;
        dep.restart.backoff_min_secs = 1;
        dep.restart.backoff_max_secs = 2;

        let failed_at = 1_000_000u64;
        // Slot 0: hopeless, six failures, well over budget.
        for inc in 0..6 {
            put_job(
                &mut state,
                instance_id("flap", 0, &Slot::Replica(0), inc),
                node(2),
                JobState::Failed,
                Some(failed_at),
            );
        }
        // Slot 1: died once, just now. It is nowhere near its own budget.
        put_job(
            &mut state,
            instance_id("flap", 0, &Slot::Replica(1), 0),
            node(1),
            JobState::Failed,
            Some(failed_at),
        );

        let now = failed_at + 10 * 1_000_000;
        let plan = plan(&dep, &state, &candidates(&state), now);
        let slots: Vec<u32> = plan
            .starts
            .iter()
            .filter_map(|s| match s.slot {
                Slot::Replica(i) => Some(i),
                Slot::Node(_) => None,
            })
            .collect();
        assert!(
            slots.contains(&1),
            "the healthy slot's own restart must not be held hostage by the \
             other slot's budget: {plan:?}"
        );
    }

    /// The delay function, at its three interesting points.
    #[test]
    fn the_delay_grows_past_the_budget_and_stops_growing() {
        let mut r = RestartPolicy {
            backoff_min_secs: 1,
            backoff_max_secs: 300,
            budget_failures: 5,
            budget_window_secs: 600,
        };
        // Inside the budget: the ordinary backoff, which is what a crash
        // loop that settles on its own needs.
        assert_eq!(reschedule_delay(&r, 1, 1), r.backoff_secs(0));
        assert_eq!(reschedule_delay(&r, 4, 5), r.backoff_secs(3));
        // One past it: the backoff maximum, doubled once.
        assert_eq!(reschedule_delay(&r, 6, 6), 600);
        // Far past it: capped, never unbounded and never zero.
        assert_eq!(reschedule_delay(&r, 40, 40), RESCHEDULE_MAX_SECS);
        // …and it is never "never".
        assert!(reschedule_delay(&r, u32::MAX, u32::MAX) > 0);

        // A deployment with a tiny backoff still reaches a sane ceiling
        // rather than hammering the mesh every second forever.
        r.backoff_max_secs = 1;
        assert_eq!(reschedule_delay(&r, 30, 30), RESCHEDULE_MAX_SECS);
    }

    #[test]
    fn initial_fill_starts_desired_count() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        member(&mut state, 3, "c", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("edge", Count::Fixed(3));
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 1_000_000);
        assert_eq!(plan.starts.len(), 3, "three replicas should be started");
        assert!(plan.cancels.is_empty());
        assert!(plan.degraded.is_none());
        // Placement is spread across candidate nodes (no node picked twice
        // when the pool is large enough is not guaranteed by rendezvous, but
        // every chosen node must be a candidate).
        for s in &plan.starts {
            assert!(cands.iter().any(|c| c.node_id == s.node));
        }
    }

    #[test]
    fn steady_state_is_a_no_op() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("edge", Count::Fixed(2));
        for i in 0..2 {
            let id = instance_id("edge", 0, &Slot::Replica(i), 0);
            put_job(&mut state, id, node(1), JobState::Running, None);
        }
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 2_000_000);
        assert!(plan.starts.is_empty(), "nothing to start when count is met");
        assert!(plan.cancels.is_empty());
    }

    #[test]
    fn a_failed_instance_is_replaced_after_backoff() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("edge", Count::Fixed(1));
        // Incarnation 0 failed 10s ago; min backoff is 1s, so it is due.
        let id0 = instance_id("edge", 0, &Slot::Replica(0), 0);
        put_job(&mut state, id0, node(1), JobState::Failed, Some(1_000_000));
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 11_000_000);
        assert_eq!(plan.starts.len(), 1);
        assert_eq!(plan.starts[0].incarnation, 1, "next incarnation restarts");
    }

    #[test]
    fn backoff_holds_a_recent_failure() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("edge", Count::Fixed(1));
        let id0 = instance_id("edge", 0, &Slot::Replica(0), 0);
        // Failed 0.2s ago; min backoff 1s -> not yet due.
        put_job(&mut state, id0, node(1), JobState::Failed, Some(1_000_000));
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 1_200_000);
        assert!(plan.starts.is_empty(), "backoff should hold the restart");
    }

    #[test]
    fn exceeding_the_budget_goes_degraded_and_stops() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        let mut dep = deployment("edge", Count::Fixed(1));
        dep.restart.budget_failures = 3;
        dep.restart.budget_window_secs = 600;
        // Six failed incarnations, all in the window.
        for inc in 0..6u32 {
            let id = instance_id("edge", 0, &Slot::Replica(0), inc);
            put_job(&mut state, id, node(1), JobState::Failed, Some(2_000_000));
        }
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 3_000_000);
        assert!(plan.degraded.is_some(), "budget exceeded -> degraded");
        assert!(
            plan.starts.is_empty(),
            "a degraded deployment stops restarting"
        );
    }

    #[test]
    fn rolling_update_moves_one_batch_at_a_time() {
        let mut state = MeshState::default();
        for n in 1..=4u8 {
            member(&mut state, n, &format!("n{n}"), vec![]);
        }
        service(&mut state, "collector", 7);
        service(&mut state, "collector", 8);
        // Deployment now at generation 1 (service 8); old gen-0 instances of
        // service 7 are still running healthy on all four replicas.
        let mut dep = deployment("edge", Count::Fixed(4));
        dep.service_version = 8;
        dep.generation = 1;
        dep.update.max_unavailable = 1;
        dep.update.gate_on_health = false; // no health check; ready == running
        for i in 0..4 {
            let old = instance_id("edge", 0, &Slot::Replica(i), 0);
            put_job(&mut state, old, node(1), JobState::Running, None);
        }
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 5_000_000);
        assert_eq!(
            plan.starts.len(),
            1,
            "max_unavailable=1 rolls exactly one new instance at a time"
        );
        assert_eq!(plan.starts[0].generation, 1);
    }

    #[test]
    fn rolling_update_cuts_over_when_new_is_ready() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        service(&mut state, "collector", 8);
        let mut dep = deployment("edge", Count::Fixed(1));
        dep.service_version = 8;
        dep.generation = 1;
        dep.update.gate_on_health = false;
        // Old gen-0 still running; new gen-1 also running (ready).
        let old = instance_id("edge", 0, &Slot::Replica(0), 0);
        let new = instance_id("edge", 1, &Slot::Replica(0), 0);
        put_job(&mut state, old, node(1), JobState::Running, None);
        put_job(&mut state, new, node(1), JobState::Running, None);
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 6_000_000);
        assert!(
            plan.starts.is_empty(),
            "target is ready; nothing new to start"
        );
        assert_eq!(plan.cancels.len(), 1, "old instance is retired on cutover");
        assert_eq!(plan.cancels[0].job_id, old);
    }

    #[test]
    fn draining_node_instance_is_moved() {
        let mut state = MeshState::default();
        member(
            &mut state,
            1,
            "a",
            vec![mesh_proto::DRAINING_TAG.to_string()],
        );
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("edge", Count::Fixed(1));
        // The single replica happens to be on the draining node.
        let id0 = instance_id("edge", 0, &Slot::Replica(0), 0);
        put_job(&mut state, id0, node(1), JobState::Running, None);
        let cands = candidates(&state); // includes only non-draining in real
        // Emulate resolve_targets excluding the draining node:
        let live: Vec<MemberInfo> = cands.into_iter().filter(|m| m.node_id != node(1)).collect();
        let plan = plan(&dep, &state, &live, 7_000_000);
        assert_eq!(plan.cancels.len(), 1, "the draining instance is retired");
        assert_eq!(plan.cancels[0].job_id, id0);
    }

    #[test]
    fn system_count_is_one_per_matching_node() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        member(&mut state, 3, "c", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("edge", Count::System);
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 1_000_000);
        assert_eq!(plan.starts.len(), 3, "one instance per matching node");
        // Each start's slot is a node slot placed on its own node.
        for s in &plan.starts {
            match &s.slot {
                Slot::Node(n) => assert_eq!(s.node, *n),
                _ => panic!("system slots are node slots"),
            }
        }
    }

    #[test]
    fn reducing_count_cancels_extra_replicas() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        // Was 3; now 1. Replicas 1 and 2 should be cancelled.
        let dep = deployment("edge", Count::Fixed(1));
        for i in 0..3 {
            let id = instance_id("edge", 0, &Slot::Replica(i), 0);
            put_job(&mut state, id, node(1), JobState::Running, None);
        }
        let cands = candidates(&state);
        let plan = plan(&dep, &state, &cands, 1_000_000);
        assert_eq!(plan.cancels.len(), 2, "replicas 1 and 2 are retired");
    }

    #[test]
    fn count_and_service_ref_parse() {
        let src = r#"
[deployment]
name    = "edge-collector"
service = "collector@7"
count   = "system"
targets = "tag=edge"

[deployment.restart]
backoff = "1s..5m"
budget  = "5 in 10m"

[deployment.health]
kind     = "http"
path     = "/healthz"
port     = 9095
interval = "10s"
grace    = "30s"

[deployment.update]
strategy        = "rolling"
max_unavailable = 1
gate            = "health"
"#;
        let dep = parse(src, UserId([0u8; 32])).expect("parse").deployment;
        assert_eq!(dep.name, "edge-collector");
        assert_eq!(dep.service, "collector");
        assert_eq!(dep.service_version, 7);
        assert_eq!(dep.count, Count::System);
        assert_eq!(dep.restart.backoff_min_secs, 1);
        assert_eq!(dep.restart.backoff_max_secs, 300);
        assert_eq!(dep.restart.budget_failures, 5);
        assert_eq!(dep.restart.budget_window_secs, 600);
        let h = dep.health.expect("health");
        assert_eq!(h.kind, HealthKind::Http);
        assert_eq!(h.port, 9095);
        assert_eq!(dep.update.max_unavailable, 1);
        assert!(dep.update.gate_on_health);
    }

    #[test]
    fn a_floating_head_is_refused() {
        let src = "[deployment]\nname=\"x\"\nservice=\"collector\"\n";
        assert!(parse(src, UserId([0u8; 32])).is_err(), "must pin a version");
    }

    #[test]
    fn to_toml_round_trips_through_parse() {
        // The rollout / rollback / pause path fetches a deployment as TOML
        // and re-parses it, so `to_toml` must produce something `parse`
        // accepts with every field preserved.
        let src = r#"
[deployment]
name    = "edge"
service = "collector@9"
count   = 4
targets = "tag=edge"
generation = 3

[deployment.restart]
backoff = "2s..1m"
budget  = "7 in 15m"

[deployment.health]
kind = "tcp"
port = 8080

[deployment.update]
max_unavailable = 2
gate            = "running"
"#;
        let parsed = parse(src, UserId([0u8; 32])).expect("parse");
        let dep = parsed.deployment;
        let round = parse(&to_toml(&dep, &parsed.policy), UserId([0u8; 32]))
            .expect("re-parse")
            .deployment;
        assert_eq!(round.name, dep.name);
        assert_eq!(round.service, dep.service);
        assert_eq!(round.service_version, 9);
        assert_eq!(round.count, Count::Fixed(4));
        assert_eq!(round.generation, 3);
        assert_eq!(round.restart, dep.restart);
        assert_eq!(round.health, dep.health);
        assert_eq!(round.update.max_unavailable, 2);
        assert!(!round.update.gate_on_health);
    }

    /// **The whole record, not a hand-picked list of fields.**
    ///
    /// The two tests around this one assert the fields somebody thought of
    /// while writing them, which is why the render could lose
    /// `[deployment.resources]` entirely and print `port = 0` for a
    /// label-named health check — both of them passing throughout. A
    /// partial assertion over a total property is the same shape of bug as
    /// a type with no caller: it passes every test it has.
    ///
    /// This matters beyond `--output toml` now. `previous_from_row`
    /// re-parses this render to decide whether a put is a roll, so a field
    /// the render drops is a field that looks *changed* on every publish
    /// (churn), and a render that will not parse at all looks like a first
    /// publish (no roll, reported as a successful one).
    #[test]
    fn the_whole_record_round_trips() {
        let user = UserId([0u8; 32]);
        // Each of these once made it through the pair of tests below while
        // losing something on the way through this one.
        let cases: &[(&str, &str)] = &[
            (
                "the shipped example: inline image, label-named check, resources",
                concat!(
                    "deployment:\n",
                    "  name: web\n",
                    "  count: 2\n",
                    "  container:\n",
                    "    image: docker.io/library/nginx:alpine\n",
                    "  network:\n",
                    "    port:\n",
                    "      - label: http\n",
                    "        to: 80\n",
                    "  resources:\n",
                    "    cpus: 1\n",
                    "    memory_mb: 128\n",
                    "  register:\n",
                    "    name: web\n",
                    "  health:\n",
                    "    kind: http\n",
                    "    port: http\n",
                    "    path: /\n",
                    "    interval: 10s\n",
                    "    grace: 10s\n",
                    "    timeout: 3s\n",
                    "    failures: 2\n",
                ),
            ),
            (
                "a numeric check, a fixed port, a volume, a disk floor",
                concat!(
                    "deployment:\n",
                    "  name: db\n",
                    "  service: postgres@4\n",
                    "  count: system\n",
                    "  targets: tag=storage\n",
                    "  network:\n",
                    "    port:\n",
                    "      - label: sql\n",
                    "        static: 5432\n",
                    "  volume:\n",
                    "    - name: data\n",
                    "      path: /var/lib/postgresql\n",
                    "      mode: rw\n",
                    "  resources:\n",
                    "    memory_mb: 2048\n",
                    "    disk_mb: 40960\n",
                    "  health:\n",
                    "    kind: tcp\n",
                    "    port: 5432\n",
                ),
            ),
            (
                "registration knobs, a tcp check by label, disconnect policy",
                concat!(
                    "deployment:\n",
                    "  name: api\n",
                    "  service: api@2\n",
                    "  count: 3\n",
                    "  description: the public one\n",
                    "  network:\n",
                    "    port:\n",
                    "      - label: grpc\n",
                    "        to: 9000\n",
                    "      - label: metrics\n",
                    "        to: 9100\n",
                    "  register:\n",
                    "    name: api\n",
                    "    tags: [canary, v2]\n",
                    "    port: grpc\n",
                    "    shutdown_delay: 10s\n",
                    "    kill_timeout: 30s\n",
                    "  health:\n",
                    "    kind: tcp\n",
                    "    port: grpc\n",
                    "  disconnect:\n",
                    "    lost_after: 5m\n",
                    "    replace: true\n",
                    "    reconcile: keep_replacement\n",
                    "  update:\n",
                    "    canary: 1\n",
                    "    auto_revert: true\n",
                    "    progress_deadline: 10m\n",
                    "    min_healthy_time: 30s\n",
                ),
            ),
            (
                "an exec check, inputs, and a paused deployment",
                concat!(
                    "deployment:\n",
                    "  name: batch\n",
                    "  service: batch@1\n",
                    "  count: 1\n",
                    "  paused: true\n",
                    "  inputs:\n",
                    "    target: prod\n",
                    "    retries: 3\n",
                    "  restart:\n",
                    "    backoff: 2s..1m\n",
                    "    budget: 7 in 15m\n",
                    "  health:\n",
                    "    kind: exec\n",
                    "    command: /bin/true\n",
                    "    interval: 30s\n",
                    "    grace: 60s\n",
                ),
            ),
        ];
        for (what, src) in cases {
            let first = parse(src, user).unwrap_or_else(|e| panic!("{what}: parse: {e}"));
            let rendered = to_toml(&first.deployment, &first.policy);
            let again = parse(&rendered, user).unwrap_or_else(|e| {
                panic!("{what}: the render will not re-parse: {e}\n{rendered}")
            });

            // `created_at_micros` is stamped at parse time and is the one
            // field that is *meant* to differ; everything else is the record.
            let mut a = first.deployment.clone();
            let mut b = again.deployment.clone();
            a.created_at_micros = 0;
            b.created_at_micros = 0;
            assert_eq!(a, b, "{what}: the deployment changed on the way through");

            // The side-car carries ports, volumes, resources, the
            // registration and the health port label — order is not a fact
            // about it, membership is.
            let sorted = |e: &[(String, String)]| {
                let mut v = e.to_vec();
                v.sort();
                v
            };
            assert_eq!(
                sorted(&first.policy.instance_env),
                sorted(&again.policy.instance_env),
                "{what}: the instance environment changed\n{rendered}"
            );

            // …and the rollout knobs, which live on the policy beside it.
            let knobs = |p: &mesh_proto::DeploymentPolicy| {
                (
                    p.canary,
                    p.auto_revert,
                    p.progress_deadline_secs,
                    p.min_healthy_secs,
                    p.lost_after_secs,
                    p.replace_unknown,
                    p.reconcile,
                )
            };
            assert_eq!(
                knobs(&first.policy),
                knobs(&again.policy),
                "{what}: a rollout knob changed"
            );

            // The property the whole thing exists for: a re-publish of an
            // unchanged file must not roll anything.
            assert!(
                !rolls(
                    &a,
                    &first.policy.instance_env,
                    &b,
                    &again.policy.instance_env
                ),
                "{what}: an unchanged file would roll the deployment"
            );
        }
    }

    /// N1 P3/P4 ride the same file and must survive the same round trip —
    /// `deployments show --output toml` piping back into `put` is a stated
    /// property, and a policy that evaporated on the way through would
    /// quietly lift a canary.
    #[test]
    fn the_policy_round_trips_through_the_file_too() {
        let src = r#"
[deployment]
name    = "web"
service = "web@9"
count   = 6
targets = "any"

[deployment.update]
max_unavailable   = 2
canary            = 2
auto_revert       = true
progress_deadline = "10m"
min_healthy_time  = "30s"

[deployment.disconnect]
lost_after = "15m"
replace    = true
reconcile  = "longest_running"
"#;
        let parsed = parse(src, UserId([0u8; 32])).expect("parse");
        assert_eq!(parsed.policy.canary, 2);
        assert!(parsed.policy.auto_revert);
        assert_eq!(parsed.policy.progress_deadline_secs, 600);
        assert_eq!(parsed.policy.min_healthy_secs, 30);
        assert_eq!(parsed.policy.lost_after_secs, 900);
        assert!(parsed.policy.replace_unknown);
        assert_eq!(
            parsed.policy.reconcile,
            mesh_proto::Reconcile::LongestRunning
        );

        let round = parse(
            &to_toml(&parsed.deployment, &parsed.policy),
            UserId([0u8; 32]),
        )
        .expect("re-parse");
        assert_eq!(round.policy.canary, 2);
        assert!(round.policy.auto_revert);
        assert_eq!(round.policy.progress_deadline_secs, 600);
        assert_eq!(round.policy.min_healthy_secs, 30);
        assert_eq!(round.policy.lost_after_secs, 900);
        assert!(round.policy.replace_unknown);
        assert_eq!(
            round.policy.reconcile,
            mesh_proto::Reconcile::LongestRunning
        );
    }

    /// A fixed-count file that says nothing new gets the Phase A
    /// `lost_after` default and nothing else (§2.1). It used to get zero,
    /// which meant an instance on a node that went away was waited for
    /// forever.
    #[test]
    fn a_file_with_no_new_blocks_gets_only_the_lost_after_default() {
        let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 1
"#;
        let parsed = parse(src, UserId([0u8; 32])).expect("parse");
        assert_eq!(
            parsed.policy.lost_after_secs,
            mesh_proto::DEFAULT_LOST_AFTER_SECS
        );
        let without_it = mesh_proto::DeploymentPolicy {
            lost_after_secs: 0,
            ..parsed.policy.clone()
        };
        assert!(
            without_it.is_inert(),
            "nothing but the default: {without_it:?}"
        );
        assert_eq!(parsed.policy.deployment, "web");
    }

    /// `never` is how a file keeps waiting for the node, and a `system`
    /// deployment keeps it without asking, because its slot *is* its node.
    #[test]
    fn never_and_system_keep_waiting_for_the_node() {
        let never = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 2

[deployment.disconnect]
lost_after = "never"
"#;
        let parsed = parse(never, UserId([0u8; 32])).expect("parse");
        assert!(parsed.policy.is_inert(), "{:?}", parsed.policy);

        let system = r#"
[deployment]
name    = "agent"
service = "agent@1"
count   = "system"
"#;
        let parsed = parse(system, UserId([0u8; 32])).expect("parse");
        assert!(parsed.policy.is_inert(), "{:?}", parsed.policy);
    }

    /// §2.4: `auto_revert` or `canary` with no deadline gets Nomad's, so
    /// the revert it asks for can actually fire; `never` still declines one,
    /// and a file asking for neither gets no deadline from nowhere.
    #[test]
    fn auto_revert_and_canary_get_a_progress_deadline() {
        let with = |update: &str| {
            format!(
                "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n[deployment.update]\n{update}\n"
            )
        };
        for block in ["auto_revert = true", "canary = 1"] {
            let parsed = parse(&with(block), UserId([0u8; 32])).expect("parse");
            assert_eq!(
                parsed.policy.progress_deadline_secs,
                mesh_proto::DEFAULT_PROGRESS_DEADLINE_SECS,
                "{block}"
            );
        }
        let declined = parse(
            &with("auto_revert = true\nprogress_deadline = \"never\""),
            UserId([0u8; 32]),
        )
        .expect("parse");
        assert_eq!(declined.policy.progress_deadline_secs, 0);

        let plain = parse(&with("max_unavailable = 2"), UserId([0u8; 32])).expect("parse");
        assert_eq!(plain.policy.progress_deadline_secs, 0);

        // A min_healthy_time the default cannot hold is refused by naming
        // the default, since it is not in the file to be found.
        let err = parse(
            &with("auto_revert = true\nmin_healthy_time = \"15m\""),
            UserId([0u8; 32]),
        )
        .expect_err("15m cannot fit inside a 10m deadline")
        .to_string();
        assert!(err.contains("the default"), "{err}");
    }

    /// Both `never`s and both defaults survive a render and a re-parse —
    /// the property `generation_for` stands on, since it decides whether a
    /// put is a roll by re-parsing the published record.
    #[test]
    fn nevers_and_defaults_round_trip() {
        let user = UserId([0u8; 32]);
        let cases = [
            "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n",
            "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n[deployment.disconnect]\nlost_after = \"never\"\n",
            "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n[deployment.update]\nauto_revert = true\n",
            "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n[deployment.update]\ncanary = 1\nprogress_deadline = \"never\"\n",
        ];
        for src in cases {
            let parsed = parse(src, user).expect("parse");
            let rendered = to_toml(&parsed.deployment, &parsed.policy);
            let round = parse(&rendered, user).unwrap_or_else(|e| panic!("{e}\n{rendered}"));
            assert_eq!(
                round.policy.lost_after_secs, parsed.policy.lost_after_secs,
                "{rendered}"
            );
            assert_eq!(
                round.policy.progress_deadline_secs, parsed.policy.progress_deadline_secs,
                "{rendered}"
            );
        }
    }

    /// An instance that can never finish proving itself before the roll is
    /// failed is a file that cannot work, and it is caught while somebody
    /// is looking at it.
    #[test]
    fn min_healthy_time_past_the_deadline_is_refused() {
        let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 1

[deployment.update]
progress_deadline = "30s"
min_healthy_time  = "60s"
"#;
        let err = parse(src, UserId([0u8; 32])).expect_err("must refuse");
        assert!(
            err.to_string().contains("min_healthy_time"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn backoff_doubles_and_saturates() {
        let p = RestartPolicy {
            backoff_min_secs: 1,
            backoff_max_secs: 300,
            budget_failures: 5,
            budget_window_secs: 600,
        };
        assert_eq!(p.backoff_secs(0), 1);
        assert_eq!(p.backoff_secs(1), 2);
        assert_eq!(p.backoff_secs(2), 4);
        assert_eq!(p.backoff_secs(20), 300, "saturates at the ceiling");
    }

    // -----------------------------------------------------------------
    // N1 P3/P4 — the policy beside the deployment
    // -----------------------------------------------------------------

    /// `plan_reconcile` with a policy and a liveness view.
    fn plan_with(
        dep: &Deployment,
        state: &MeshState,
        cands: &[MemberInfo],
        now: u64,
        policy: &mesh_proto::DeploymentPolicy,
        down: &[(NodeId, u64)],
    ) -> ReconcilePlan {
        plan_reconcile(dep, state, cands, now, policy, |id| {
            down.iter().find(|(n, _)| n == id).map(|(_, since)| *since)
        })
    }

    fn healthy(
        state: &mut MeshState,
        on: NodeId,
        id: JobId,
        dep: &str,
        generation: u32,
        since: u64,
    ) {
        let entry =
            state
                .deployment_health
                .entry(on)
                .or_insert_with(|| mesh_proto::NodeInstanceHealth {
                    node: on,
                    instances: Vec::new(),
                    published_at_micros: 1,
                });
        entry.instances.push(mesh_proto::InstanceHealth {
            job_id: id,
            deployment: dep.to_string(),
            generation,
            healthy: true,
            since_micros: since,
            detail: String::new(),
        });
    }

    fn rolling_four() -> (MeshState, Deployment) {
        let mut state = MeshState::default();
        for n in 1..=4u8 {
            member(&mut state, n, &format!("n{n}"), vec![]);
        }
        service(&mut state, "collector", 7);
        service(&mut state, "collector", 8);
        let mut dep = deployment("web", Count::Fixed(4));
        dep.service_version = 8;
        dep.generation = 1;
        dep.update.gate_on_health = false;
        dep.update.max_unavailable = 4;
        // All four slots still carry a gen-0 instance.
        for slot in 0..4u32 {
            let old = instance_id("web", 0, &Slot::Replica(slot), 0);
            put_job(
                &mut state,
                old,
                node((slot + 1) as u8),
                JobState::Running,
                None,
            );
        }
        (state, dep)
    }

    /// The promise: exactly N new instances come up, however generous
    /// `max_unavailable` is.
    #[test]
    fn a_canary_starts_only_n_however_wide_the_batch_is() {
        let (state, dep) = rolling_four();
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            canary: 2,
            ..Default::default()
        };
        let plan = plan_with(&dep, &state, &cands, 6_000_000, &policy, &[]);
        assert_eq!(
            plan.starts.len(),
            2,
            "max_unavailable is 4, but canary is 2: {:?}",
            plan.starts.len()
        );
        assert!(plan.canarying, "the canary set is up and unproven");
        assert!(
            plan.cancels.is_empty(),
            "nothing is cut over while canaries are proving themselves"
        );
    }

    /// Phase B, E2: a canary set held for a person holds, and the hold is
    /// bounded by `progress_deadline` like any roll — with a promotion
    /// counting as progress, so promoting late does not stall at once.
    #[test]
    fn a_held_canary_set_holds_until_promoted_or_its_deadline() {
        let (mut state, dep) = rolling_four();
        let policy = mesh_proto::DeploymentPolicy {
            canary: 1,
            progress_deadline_secs: 600,
            ..Default::default()
        };
        state.deployment_knobs.insert(
            "web".into(),
            crate::state::DeploymentKnobsEntry {
                knobs: mesh_proto::DeploymentKnobs {
                    deployment: "web".into(),
                    entries: vec![(mesh_proto::KNOB_AUTO_PROMOTE.into(), "false".into())],
                    ..Default::default()
                },
                updated_hlc: hlc(),
            },
        );
        let secs = |s: u64| s * 1_000_000;
        // Slot 0's canary is up, and healthy since t = 10 s.
        let canary = instance_id("web", 1, &Slot::Replica(0), 0);
        put_job(&mut state, canary, node(1), JobState::Running, None);
        healthy(&mut state, node(1), canary, "web", 1, secs(10));
        let cands = candidates(&state);

        let plan = plan_with(&dep, &state, &cands, secs(60), &policy, &[]);
        assert!(
            plan.awaiting_promotion && plan.canarying,
            "a settled canary set holds"
        );
        assert!(
            plan.starts.is_empty() && plan.cancels.is_empty(),
            "nothing moves while held"
        );
        assert!(plan.stalled.is_none());

        let plan = plan_with(&dep, &state, &cands, secs(10 + 601), &policy, &[]);
        assert!(
            plan.stalled.is_some(),
            "an unattended hold ends at the deadline"
        );

        state.deployment_promotions.insert(
            "web".into(),
            crate::state::DeploymentPromotionEntry {
                promotion: mesh_proto::DeploymentPromotion {
                    deployment: "web".into(),
                    generation: 1,
                    by: UserId([0u8; 32]),
                    at_micros: secs(700),
                    signature: vec![],
                },
                updated_hlc: mesh_proto::Hlc {
                    wall_micros: secs(700),
                    logical: 0,
                },
            },
        );
        let plan = plan_with(&dep, &state, &cands, secs(701), &policy, &[]);
        assert!(!plan.awaiting_promotion, "promoted");
        assert!(plan.stalled.is_none(), "a promotion is progress");
        assert!(
            !plan.cancels.is_empty() || !plan.starts.is_empty(),
            "and the roll moves"
        );
    }

    /// The other half of the promise: the old set keeps serving until the
    /// canaries settle. This is the whole reason to canary at all, and a
    /// cutover here would make it an ordinary rolling update.
    #[test]
    fn a_healthy_canary_does_not_cut_over_until_all_of_them_are() {
        let (mut state, dep) = rolling_four();
        // Canary of 2: slot 0's new instance is up and ready, slot 1's is
        // not yet started.
        let new0 = instance_id("web", 1, &Slot::Replica(0), 0);
        put_job(&mut state, new0, node(1), JobState::Running, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            canary: 2,
            ..Default::default()
        };
        let plan = plan_with(&dep, &state, &cands, 6_000_000, &policy, &[]);
        assert!(plan.canarying);
        assert!(
            plan.cancels.is_empty(),
            "slot 0 is ready but its predecessor stays up: {:?}",
            plan.cancels
        );
        assert_eq!(plan.starts.len(), 1, "the second canary starts");
    }

    /// Once every canary has settled the phase ends and the ordinary roll
    /// takes over — including retiring the instances the canaries replaced.
    #[test]
    fn the_roll_resumes_once_every_canary_has_settled() {
        let (mut state, dep) = rolling_four();
        for slot in 0..2u32 {
            let new = instance_id("web", 1, &Slot::Replica(slot), 0);
            put_job(
                &mut state,
                new,
                node((slot + 1) as u8),
                JobState::Running,
                None,
            );
        }
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            canary: 2,
            ..Default::default()
        };
        let plan = plan_with(&dep, &state, &cands, 6_000_000, &policy, &[]);
        assert!(!plan.canarying, "both canaries are ready");
        assert_eq!(plan.cancels.len(), 2, "the two replaced instances retire");
        assert_eq!(plan.starts.len(), 2, "and the remaining slots roll");
    }

    /// A process that answers one probe and dies must not count as
    /// healthy, and `min_healthy_time` is what says so.
    #[test]
    fn min_healthy_time_holds_an_instance_back_until_it_has_settled() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        service(&mut state, "collector", 8);
        let mut dep = deployment("web", Count::Fixed(1));
        dep.service_version = 8;
        dep.generation = 1;
        dep.health = Some(mesh_proto::HealthCheck {
            kind: mesh_proto::HealthKind::Tcp,
            command: String::new(),
            path: String::new(),
            port: 8080,
            interval_secs: 10,
            grace_secs: 5,
            timeout_secs: 2,
            failures_to_unhealthy: 3,
        });
        let old = instance_id("web", 0, &Slot::Replica(0), 0);
        let new = instance_id("web", 1, &Slot::Replica(0), 0);
        put_job(&mut state, old, node(1), JobState::Running, None);
        put_job(&mut state, new, node(1), JobState::Running, None);
        // Healthy as of t=10s.
        healthy(&mut state, node(1), new, "web", 1, 10_000_000);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            min_healthy_secs: 30,
            ..Default::default()
        };
        // t=20s: healthy for 10s, which is not 30.
        let early = plan_with(&dep, &state, &cands, 20_000_000, &policy, &[]);
        assert!(
            early.cancels.is_empty(),
            "10s of health is not 30s: {:?}",
            early.cancels
        );
        // t=45s: settled.
        let late = plan_with(&dep, &state, &cands, 45_000_000, &policy, &[]);
        assert_eq!(late.cancels.len(), 1, "35s of health counts");
        assert_eq!(late.cancels[0].job_id, old);
    }

    /// A version that starts, stays up and never passes its probe fails
    /// nothing and crash-loops never — so the failure budget cannot see
    /// it and only the progress deadline can.
    #[test]
    fn a_roll_that_never_goes_healthy_stalls_without_ever_failing() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 8);
        let mut dep = deployment("web", Count::Fixed(1));
        dep.service_version = 8;
        dep.generation = 1;
        dep.update.gate_on_health = false;
        let new = instance_id("web", 1, &Slot::Replica(0), 0);
        // Queued, so `ready` is false and the roll has not converged.
        put_job(&mut state, new, node(1), JobState::Queued, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            progress_deadline_secs: 60,
            ..Default::default()
        };
        let early = plan_with(&dep, &state, &cands, 30_000_000, &policy, &[]);
        assert!(early.stalled.is_none(), "30s into a 60s deadline");
        assert!(early.degraded.is_none(), "nothing has failed");
        let late = plan_with(&dep, &state, &cands, 120_000_000, &policy, &[]);
        assert!(late.stalled.is_some(), "no progress for 120s");
        assert!(
            late.degraded.is_none(),
            "a stall is not a crash loop; they are different words"
        );
    }

    /// A converged deployment never stalls, however long it has been up —
    /// the deadline bounds a roll, not a service's lifetime.
    #[test]
    fn a_converged_deployment_never_trips_the_deadline() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let id = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, id, node(1), JobState::Running, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            progress_deadline_secs: 60,
            ..Default::default()
        };
        let plan = plan_with(&dep, &state, &cands, 86_400_000_000, &policy, &[]);
        assert!(plan.stalled.is_none(), "a day up is not a stalled roll");
    }

    /// Today's behaviour with no policy: an instance on a node nobody can
    /// reach is left exactly alone.
    #[test]
    fn without_lost_after_an_unreachable_instance_is_left_alone() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let id = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, id, node(1), JobState::Running, None);
        let cands = candidates(&state);
        let plan = plan_with(
            &dep,
            &state,
            &cands,
            3_600_000_000,
            &mesh_proto::DeploymentPolicy::default(),
            &[(node(1), 1_000_000)],
        );
        assert!(plan.cancels.is_empty() && plan.starts.is_empty());
    }

    /// With `lost_after`, the same instance is given up on — and the slot
    /// refilled — once the timer expires and not a second before.
    #[test]
    fn lost_after_gives_up_on_an_instance_and_refills_the_slot() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let id = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, id, node(1), JobState::Running, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            lost_after_secs: 600,
            ..Default::default()
        };
        // Gone for five minutes of a ten-minute grace.
        let waiting = plan_with(
            &dep,
            &state,
            &cands,
            301_000_000,
            &policy,
            &[(node(1), 1_000_000)],
        );
        assert!(
            waiting.cancels.is_empty(),
            "five minutes is not ten: {:?}",
            waiting.cancels
        );
        // Gone for eleven.
        let lost = plan_with(
            &dep,
            &state,
            &cands,
            661_000_000,
            &policy,
            &[(node(1), 1_000_000)],
        );
        assert_eq!(lost.cancels.len(), 1);
        assert_eq!(lost.cancels[0].job_id, id);
        assert!(lost.cancels[0].why.contains("lost_after"));
        // **Phase A, §2.2 — and the slot is refilled.** This test used to
        // stop at the cancel, which is exactly the half that could not work
        // on its own: the cancel goes to a node nobody can reach.
        assert_eq!(
            lost.starts.len(),
            1,
            "a replacement starts on a node that answers"
        );
        assert_eq!(
            lost.starts[0].job_id,
            instance_id("web", 0, &Slot::Replica(0), 1)
        );
        assert_eq!(lost.starts[0].node, node(2));
    }

    /// Once the replacement exists the slot is not replaced again every
    /// tick — but the cancel keeps going out, so it lands the moment the
    /// node is back.
    #[test]
    fn a_lost_instance_is_replaced_once_and_cancelled_until_it_goes() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let original = instance_id("web", 0, &Slot::Replica(0), 0);
        let replacement = instance_id("web", 0, &Slot::Replica(0), 1);
        put_job(&mut state, original, node(1), JobState::Running, None);
        put_job(&mut state, replacement, node(2), JobState::Running, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            lost_after_secs: 600,
            ..Default::default()
        };
        let plan = plan_with(
            &dep,
            &state,
            &cands,
            661_000_000,
            &policy,
            &[(node(1), 1_000_000)],
        );
        assert!(plan.starts.is_empty(), "no third copy: {:?}", plan.starts);
        assert_eq!(plan.cancels.len(), 1);
        assert_eq!(plan.cancels[0].job_id, original);
    }

    /// Phase B, E3/U1: a slot's timeline is every incarnation it has had,
    /// oldest first — here, one lost on `b` and its replacement running on
    /// `a` — and the live row is the one `status` would show.
    #[test]
    fn an_instance_detail_is_the_slots_timeline() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(2));
        let lost = instance_id("web", 0, &Slot::Replica(1), 0);
        let replacement = instance_id("web", 0, &Slot::Replica(1), 1);
        put_job(&mut state, lost, node(2), JobState::Failed, Some(5));
        put_job(&mut state, replacement, node(1), JobState::Running, None);

        let detail = instance_detail(&dep, &state, "1", |_| false).expect("slot 1 exists");
        assert_eq!(detail.slot, "1");
        let timeline: Vec<(u32, &str, &str)> = detail
            .incarnations
            .iter()
            .map(|i| (i.incarnation, i.node.as_str(), i.state.as_str()))
            .collect();
        assert_eq!(timeline, vec![(0, "b", "failed"), (1, "a", "running")]);
        assert_eq!(detail.current.as_ref().map(|c| c.job_id), Some(replacement));
        assert_eq!(detail.runtime, "bash");
        assert!(
            detail.stats.is_none(),
            "stats are the executor's to measure"
        );
        assert!(instance_detail(&dep, &state, "one", |_| false).is_none());
    }

    /// When a lost instance's node comes back, the replacement keeps the
    /// slot: the operator said the original was lost. `reconcile`'s
    /// `keep_original` default belongs to `replace`, where both copies were
    /// deliberate — applied here it would hand the slot back to the very
    /// instance that was given up on.
    #[test]
    fn a_lost_instance_that_comes_back_gives_way_to_its_replacement() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let original = instance_id("web", 0, &Slot::Replica(0), 0);
        let replacement = instance_id("web", 0, &Slot::Replica(0), 1);
        put_job(&mut state, original, node(1), JobState::Running, None);
        put_job(&mut state, replacement, node(2), JobState::Running, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            lost_after_secs: 600,
            ..Default::default()
        };
        assert_eq!(policy.reconcile, mesh_proto::Reconcile::KeepOriginal);
        // Nothing is away any more.
        let plan = plan_with(&dep, &state, &cands, 700_000_000, &policy, &[]);
        assert_eq!(plan.cancels.len(), 1);
        assert_eq!(plan.cancels[0].job_id, original);
    }

    /// Phase A, §2.5a: a revert skips every version that stalled before,
    /// read from the audit rows that already say so.
    #[test]
    fn auto_revert_skips_versions_that_stalled() {
        use std::collections::BTreeSet;
        let rows = [
            (
                "deployment.stalled",
                "generation 4 no progress for 600s (version 6)",
            ),
            // A stall from before the version was written down, still
            // counted through the revert that followed it.
            (
                "deployment.auto_revert",
                "generation 3 stalled (no progress); reverted 5 -> 4",
            ),
            (
                "deployment.stalled",
                "generation 1 an old row with no version",
            ),
            ("deployment.degraded", "generation 2 crash loop (version 3)"),
        ];
        let stalled = stalled_versions(rows.iter().copied());
        assert_eq!(stalled.iter().copied().collect::<Vec<_>>(), vec![5, 6]);

        // 7 is the one stalling now; 6 and 5 stalled before; 4 did not.
        assert_eq!(
            revert_target(7, &[1, 2, 3, 4, 5, 6, 7], &stalled),
            (Some(4), false)
        );
        // Every earlier version stalled: one back, and say so.
        let all: BTreeSet<u32> = [1, 2, 3].into();
        assert_eq!(revert_target(4, &[1, 2, 3, 4], &all), (Some(3), true));
        // Nothing to consult: one back, as it always was.
        assert_eq!(revert_target(4, &[], &BTreeSet::new()), (Some(3), false));
        assert_eq!(revert_target(1, &[1], &BTreeSet::new()), (None, false));
    }

    /// `replace` buys availability during the partition: a second instance
    /// comes up while the first is still believed to be running.
    #[test]
    fn replace_starts_a_second_instance_without_cancelling_the_first() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let id = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, id, node(1), JobState::Running, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            lost_after_secs: 600,
            replace_unknown: true,
            ..Default::default()
        };
        let plan = plan_with(
            &dep,
            &state,
            &cands,
            60_000_000,
            &policy,
            &[(node(1), 1_000_000)],
        );
        assert_eq!(plan.starts.len(), 1, "a replacement comes up now");
        assert!(
            plan.cancels.is_empty(),
            "the original keeps serving its own side of the partition"
        );
    }

    /// When the partition heals, the slot has two live instances and the
    /// `reconcile` rule picks one. Both sides run this same function over
    /// the same state, so both reach the same verdict.
    #[test]
    fn reconcile_settles_a_healed_partition_the_same_way_on_both_sides() {
        let build = |choice: mesh_proto::Reconcile| {
            let mut state = MeshState::default();
            member(&mut state, 1, "a", vec![]);
            member(&mut state, 2, "b", vec![]);
            service(&mut state, "collector", 7);
            let dep = deployment("web", Count::Fixed(1));
            let original = instance_id("web", 0, &Slot::Replica(0), 0);
            let replacement = instance_id("web", 0, &Slot::Replica(0), 1);
            put_job(&mut state, original, node(1), JobState::Running, None);
            put_job(&mut state, replacement, node(2), JobState::Running, None);
            let cands = candidates(&state);
            let policy = mesh_proto::DeploymentPolicy {
                lost_after_secs: 600,
                replace_unknown: true,
                reconcile: choice,
                ..Default::default()
            };
            // Nothing is down any more: the partition healed.
            let plan = plan_with(&dep, &state, &cands, 60_000_000, &policy, &[]);
            (plan, original, replacement)
        };

        let (plan, original, replacement) = build(mesh_proto::Reconcile::KeepOriginal);
        assert_eq!(plan.cancels.len(), 1, "one of the two goes");
        assert_eq!(plan.cancels[0].job_id, replacement);

        let (plan, original2, _) = build(mesh_proto::Reconcile::KeepReplacement);
        assert_eq!(plan.cancels.len(), 1);
        assert_eq!(plan.cancels[0].job_id, original2);
        assert_eq!(original, original2);
    }

    /// The loser is not cancelled while its node is still unreachable:
    /// "you are gone" delivered to somebody who is still serving is the
    /// confusion `reconcile` exists to end, not to cause.
    #[test]
    fn reconcile_waits_for_every_side_to_come_back() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        member(&mut state, 2, "b", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let original = instance_id("web", 0, &Slot::Replica(0), 0);
        let replacement = instance_id("web", 0, &Slot::Replica(0), 1);
        put_job(&mut state, original, node(1), JobState::Running, None);
        put_job(&mut state, replacement, node(2), JobState::Running, None);
        let cands = candidates(&state);
        let policy = mesh_proto::DeploymentPolicy {
            lost_after_secs: 600,
            replace_unknown: true,
            reconcile: mesh_proto::Reconcile::KeepReplacement,
            ..Default::default()
        };
        let plan = plan_with(
            &dep,
            &state,
            &cands,
            60_000_000,
            &policy,
            &[(node(1), 1_000_000)],
        );
        assert!(
            plan.cancels.is_empty(),
            "node 1 is still away; nothing is settled yet: {:?}",
            plan.cancels
        );
    }

    // -----------------------------------------------------------------
    // N2 P2 — the [deployment.network] block
    // -----------------------------------------------------------------

    /// The block becomes the reserved environment key, and round-trips
    /// through the file the way every other block does.
    #[test]
    fn a_network_block_becomes_the_port_request_and_round_trips() {
        let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 3

[deployment.network]
mode = "host"

[[deployment.network.port]]
label  = "http"
to     = 9095

[[deployment.network.port]]
label  = "metrics"
"#;
        let parsed = parse(src, UserId([0u8; 32])).expect("parse");
        assert_eq!(parsed.policy.ports().len(), 2);
        assert_eq!(parsed.policy.ports().0[0].label, "http");
        assert_eq!(parsed.policy.ports().0[0].to, 9095);
        assert!(!parsed.policy.ports().0[0].is_static());
        assert!(
            !parsed.policy.is_inert(),
            "declaring a port is declaring something"
        );

        // What an instance actually carries.
        let env = parsed.policy.ports().to_env();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, mesh_proto::REQ_PORTS);
        assert_eq!(env[0].1, "http:9095,metrics:0");

        let round = parse(
            &to_toml(&parsed.deployment, &parsed.policy),
            UserId([0u8; 32]),
        )
        .expect("re-parse");
        assert_eq!(round.policy.ports(), parsed.policy.ports());
    }

    /// **N2 landmine 1.** Two instances on one node cannot both bind
    /// :8080, and nothing promises they land on different nodes — replicas
    /// are placed by rendezvous over the candidate pool, which on a
    /// one-node mesh is the same node every time. Refused while somebody
    /// is looking, rather than discovered as a crash loop.
    #[test]
    fn a_static_port_with_more_than_one_instance_is_refused_loudly() {
        let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 3

[[deployment.network.port]]
label  = "http"
static = 8080
"#;
        let err = parse(src, UserId([0u8; 32])).expect_err("must refuse");
        let text = err.to_string();
        assert!(text.contains("static port"), "{text}");
        assert!(text.contains("http"), "names the offending label: {text}");
        assert!(
            text.contains("MESHWARE_PORT_HTTP"),
            "and says what to do instead: {text}"
        );
    }

    /// One instance with a static port is the ordinary case and must not
    /// be caught by the same rule.
    #[test]
    fn a_static_port_with_one_instance_is_fine() {
        let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 1

[[deployment.network.port]]
label  = "http"
static = 8080
"#;
        let parsed = parse(src, UserId([0u8; 32])).expect("parse");
        assert_eq!(parsed.policy.ports().0[0].fixed, Some(8080));
    }

    /// A `system` deployment runs one instance per node by construction, so
    /// a static port is safe there however many nodes there are.
    #[test]
    fn a_static_port_on_a_system_deployment_is_fine() {
        let src = r#"
[deployment]
name    = "agent"
service = "agent@1"
count   = "system"

[[deployment.network.port]]
label  = "http"
static = 8080
"#;
        let parsed = parse(src, UserId([0u8; 32])).expect("parse");
        assert_eq!(parsed.deployment.count, Count::System);
        assert_eq!(parsed.policy.ports().0[0].fixed, Some(8080));
    }

    /// A file written for a mode this build does not have must not
    /// silently get host networking.
    #[test]
    fn an_unknown_network_mode_is_refused() {
        let src = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 1

[deployment.network]
mode = "bridge"
"#;
        let err = parse(src, UserId([0u8; 32])).expect_err("must refuse");
        assert!(err.to_string().contains("host"), "{err}");
    }

    /// A label becomes an environment key, so it has to be one — and a
    /// repeated label is ambiguous about which port a job binds.
    #[test]
    fn a_bad_or_repeated_label_is_refused() {
        let bad = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 1

[[deployment.network.port]]
label  = "not a label"
"#;
        assert!(parse(bad, UserId([0u8; 32])).is_err());

        let dup = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 1

[[deployment.network.port]]
label  = "http"

[[deployment.network.port]]
label  = "http"
"#;
        let err = parse(dup, UserId([0u8; 32])).expect_err("must refuse");
        assert!(err.to_string().contains("twice"), "{err}");
    }

    // ---- Phase C, E7/E8: placement rules and scored placement -----------

    fn policy_with(spec: mesh_proto::PlacementSpec) -> mesh_proto::DeploymentPolicy {
        mesh_proto::DeploymentPolicy {
            deployment: "web".into(),
            instance_env: spec.to_env(),
            ..Default::default()
        }
    }

    fn plan_placed(
        dep: &Deployment,
        state: &MeshState,
        policy: &mesh_proto::DeploymentPolicy,
    ) -> ReconcilePlan {
        plan_reconcile(dep, state, &candidates(state), 1_000_000, policy, |_| None)
    }

    fn names_of(state: &MeshState, plan: &ReconcilePlan) -> Vec<String> {
        let mut out: Vec<String> = plan
            .starts
            .iter()
            .map(|s| state.members[&s.node].info.name.clone())
            .collect();
        out.sort();
        out
    }

    /// A preference doubles up when it must; the rule never does, and the
    /// slot it leaves unplaced says which rule.
    #[test]
    fn distinct_hosts_never_doubles_up_and_says_why() {
        let mut state = MeshState::default();
        member(&mut state, 1, "only", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(2));

        let preference = plan_placed(&dep, &state, &policy_with(Default::default()));
        assert_eq!(preference.starts.len(), 2, "without the rule both place");

        let rule = plan_placed(
            &dep,
            &state,
            &policy_with(mesh_proto::PlacementSpec {
                distinct_hosts: true,
                ..Default::default()
            }),
        );
        assert_eq!(rule.starts.len(), 1);
        assert_eq!(rule.unplaced.len(), 1);
        assert!(
            rule.unplaced[0].why.starts_with("distinct_hosts:"),
            "{}",
            rule.unplaced[0].why
        );
    }

    #[test]
    fn distinct_property_holds_its_limit_per_value() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a1", vec!["rack=a".into()]);
        member(&mut state, 2, "a2", vec!["rack=a".into()]);
        member(&mut state, 3, "b1", vec!["rack=b".into()]);
        member(&mut state, 4, "untagged", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(3));
        let plan = plan_placed(
            &dep,
            &state,
            &policy_with(mesh_proto::PlacementSpec {
                distinct_property: Some(mesh_proto::DistinctProperty {
                    attribute: "tag.rack".into(),
                    limit: 1,
                }),
                ..Default::default()
            }),
        );
        let placed = names_of(&state, &plan);
        assert_eq!(placed.len(), 2, "{placed:?}");
        assert!(placed.contains(&"b1".to_string()), "{placed:?}");
        assert!(
            !placed.contains(&"untagged".to_string()),
            "a machine with no rack is not a candidate"
        );
        assert_eq!(plan.unplaced.len(), 1);
        assert!(plan.unplaced[0].why.contains("tag.rack"));
    }

    /// Whatever the hash would have chosen, a full-strength affinity picks
    /// the machine it selects, and a negative one picks the other.
    #[test]
    fn an_affinity_pulls_and_a_negative_one_pushes() {
        let mut state = MeshState::default();
        member(&mut state, 1, "ssd", vec!["ssd".into()]);
        member(&mut state, 2, "spinning", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let with_weight = |weight| {
            policy_with(mesh_proto::PlacementSpec {
                affinity: vec![mesh_proto::Affinity {
                    select: "tag=ssd".into(),
                    weight,
                }],
                ..Default::default()
            })
        };
        assert_eq!(
            names_of(&state, &plan_placed(&dep, &state, &with_weight(100))),
            ["ssd"]
        );
        assert_eq!(
            names_of(&state, &plan_placed(&dep, &state, &with_weight(-100))),
            ["spinning"]
        );
    }

    /// Four instances, a 75/25 target, more machines than instances in both
    /// groups: three land in the 75 and one in the 25, whichever way the hash
    /// would have gone.
    #[test]
    fn a_spread_follows_its_targets() {
        let mut state = MeshState::default();
        for n in 1..=4u8 {
            member(&mut state, n, &format!("home{n}"), vec!["site=home".into()]);
        }
        for n in 5..=7u8 {
            member(
                &mut state,
                n,
                &format!("office{n}"),
                vec!["site=office".into()],
            );
        }
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(4));
        let plan = plan_placed(
            &dep,
            &state,
            &policy_with(mesh_proto::PlacementSpec {
                spread: Some(mesh_proto::Spread {
                    attribute: "tag.site".into(),
                    targets: [("office".to_string(), 75), ("home".to_string(), 25)]
                        .into_iter()
                        .collect(),
                }),
                ..Default::default()
            }),
        );
        let placed = names_of(&state, &plan);
        let office = placed.iter().filter(|n| n.starts_with("office")).count();
        assert_eq!((office, placed.len() - office), (3, 1), "{placed:?}");
    }

    /// **The precondition E8 names.** A hash placed an instance on a
    /// saturated machine as readily as an idle one. With both idle the hash
    /// still decides, so an empty mesh places exactly as it did.
    #[test]
    fn a_busy_machine_loses_to_an_idle_one_and_a_tie_is_the_hash() {
        let mut state = MeshState::default();
        member(&mut state, 1, "n1", vec![]);
        member(&mut state, 2, "n2", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let key = instance_id("web", 0, &Slot::Replica(0), 0);
        let hashed = scheduler::rendezvous_rank(PLACE_CONTEXT, &key.0, [node(1), node(2)])[0];
        let idle = plan_placed(&dep, &state, &policy_with(Default::default()));
        assert_eq!(idle.starts[0].node, hashed, "a tie falls back to the hash");

        let busy = hashed;
        for _ in 0..4 {
            put_job(&mut state, JobId::generate(), busy, JobState::Running, None);
        }
        let loaded = plan_placed(&dep, &state, &policy_with(Default::default()));
        assert_ne!(loaded.starts[0].node, busy, "four of four slots busy");
    }

    /// `jobs explain` has something to say about an instance: the machines
    /// compared, with the deployment's own terms among the scheduler's.
    #[test]
    fn a_start_carries_how_its_machines_were_scored() {
        let mut state = MeshState::default();
        member(&mut state, 1, "ssd", vec!["ssd".into()]);
        member(&mut state, 2, "spinning", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let plan = plan_placed(
            &dep,
            &state,
            &policy_with(mesh_proto::PlacementSpec {
                affinity: vec![mesh_proto::Affinity {
                    select: "tag=ssd".into(),
                    weight: 60,
                }],
                ..Default::default()
            }),
        );
        let explain = plan.starts[0].explain.as_ref().expect("an explanation");
        assert_eq!(explain.considered, 2);
        assert_eq!(explain.candidates[0].node, plan.starts[0].node);
        let names: Vec<&str> = explain.candidates[0]
            .terms
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert!(
            names.contains(&"load") && names.contains(&"affinity"),
            "{names:?}"
        );
        assert!(
            !names.contains(&"spread"),
            "a term the deployment did not ask for is not charted"
        );
    }

    const PLACED_FILE: &str = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 3
distinct_hosts = true

[[deployment.network.port]]
label  = "http"
static = 8080

[deployment.distinct_property]
attribute = "tag.rack"
limit     = 2

[[deployment.affinity]]
select = 'fact.os~/^(debian|ubuntu)$/'
weight = -40

[[deployment.affinity]]
select = "tag=ssd"

[deployment.spread]
attribute = "tag.site"

[deployment.spread.targets]
home = 60
"#;

    /// Every rule survives `show`, a change to any of them is a placement
    /// change that restarts nothing, and `distinct_hosts` is what lets a
    /// static port run more than once.
    #[test]
    fn placement_rules_round_trip_and_move_nothing() {
        let parsed = parse(PLACED_FILE, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}"));
        let spec = mesh_proto::PlacementSpec::from_env(&parsed.policy.instance_env);
        assert!(spec.distinct_hosts);
        assert_eq!(spec.affinity[1].weight, 50, "the default weight");
        assert_eq!(spec.spread.as_ref().unwrap().targets["home"], 60);

        let rendered = to_toml(&parsed.deployment, &parsed.policy);
        let again =
            parse(&rendered, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}\n{rendered}"));
        assert_eq!(
            mesh_proto::PlacementSpec::from_env(&again.policy.instance_env),
            spec,
            "{rendered}"
        );

        let edited = parse(
            &PLACED_FILE.replace("weight = -40", "weight = -90"),
            UserId([0u8; 32]),
        )
        .unwrap();
        assert!(!rolls(
            &parsed.deployment,
            &parsed.policy.instance_env,
            &edited.deployment,
            &edited.policy.instance_env
        ));
        let changes = plan_changes(
            &parsed.deployment,
            &parsed.policy,
            &edited.deployment,
            &edited.policy,
        );
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].effect, mesh_proto::PLAN_PLACEMENT);
        assert!(changes[0].field.starts_with("affinity."), "{changes:?}");

        let err = parse(
            &PLACED_FILE.replace("distinct_hosts = true\n", ""),
            UserId([0u8; 32]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("distinct_hosts = true"), "{err}");
    }

    #[test]
    fn a_placement_rule_that_cannot_mean_anything_is_refused() {
        let with = |extra: &str| {
            parse(
                &format!("[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n{extra}"),
                UserId([0u8; 32]),
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        };
        for (extra, says) in [
            (
                "\n[[deployment.affinity]]\nselect = \"tag=ssd\"\nweight = 0\n",
                "not 0",
            ),
            (
                "\n[[deployment.affinity]]\nselect = \"fact.os\"\n",
                "affinity select",
            ),
            ("\n[deployment.spread]\nattribute = \"rack\"\n", "tag.<key>"),
            (
                "\n[deployment.spread]\nattribute = \"tag.site\"\n[deployment.spread.targets]\na = 70\nb = 40\n",
                "110%",
            ),
            (
                "\n[deployment.distinct_property]\nattribute = \"name\"\nlimit = 0\n",
                "at least 1",
            ),
        ] {
            let err = with(extra).expect_err(extra);
            assert!(err.contains(says), "{extra}: {err}");
        }
        let err = parse(
            "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = \"system\"\ndistinct_hosts = true\n",
            UserId([0u8; 32]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing to place"), "{err}");
    }

    /// More instances than machines under `distinct_hosts` is refused before
    /// publishing, not published with a slot that can never place.
    #[test]
    fn distinct_hosts_past_the_machines_is_a_plan_issue() {
        let mut state = MeshState::default();
        member(&mut state, 1, "a", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(3));
        let policy = policy_with(mesh_proto::PlacementSpec {
            distinct_hosts: true,
            ..Default::default()
        });
        let view = check_view(&dep, &policy, &state, 2, None, false);
        assert!(
            view.issues
                .iter()
                .any(|i| i.contains("distinct_hosts") && i.contains("only 2 machines")),
            "{:?}",
            view.issues
        );
        let fits = check_view(&dep, &policy, &state, 3, None, false);
        assert!(!fits.issues.iter().any(|i| i.contains("distinct_hosts")));
    }

    // ---- Phase C, E9: leaving a drained machine without an outage -------

    const NOW: u64 = 1_000_000_000_000;

    /// `drained` is being emptied with a deadline an hour out; `spare` takes
    /// work. The candidates are what `placement_pool` gives, which leaves a
    /// drained machine out.
    fn drained_mesh(extra: &[&str]) -> (MeshState, Vec<MemberInfo>) {
        let mut state = MeshState::default();
        let mut tags = mesh_proto::drain_tags(Some(NOW + 3_600_000_000), false);
        tags.extend(extra.iter().map(|t| t.to_string()));
        member(&mut state, 1, "drained", tags);
        member(&mut state, 2, "spare", vec![]);
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(1));
        let cands: Vec<MemberInfo> = placement_pool(
            &state,
            &dep,
            node(9),
            |_| mesh_proto::Liveness::Alive,
            false,
        );
        (state, cands)
    }

    fn plan_at(
        dep: &Deployment,
        state: &MeshState,
        cands: &[MemberInfo],
        now: u64,
        policy: &mesh_proto::DeploymentPolicy,
    ) -> ReconcilePlan {
        plan_reconcile(dep, state, cands, now, policy, |_| None)
    }

    /// **The outage E9 names.** The instance on the drained machine keeps
    /// running while its replacement starts elsewhere, and stops only once
    /// the replacement has been up for `min_healthy_time`.
    #[test]
    fn a_drained_instance_is_replaced_before_it_stops() {
        let (mut state, cands) = drained_mesh(&[]);
        assert_eq!(
            cands.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["spare"],
            "a drained machine takes no new work"
        );
        let dep = deployment("web", Count::Fixed(1));
        let policy = mesh_proto::DeploymentPolicy::default();
        let original = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, original, node(1), JobState::Running, None);

        let first = plan_at(&dep, &state, &cands, NOW, &policy);
        assert!(
            first.cancels.is_empty(),
            "nothing stops before a replacement"
        );
        assert_eq!(first.starts.len(), 1);
        assert_eq!(first.starts[0].node, node(2));
        let replacement = first.starts[0].job_id;
        assert_ne!(replacement, original);

        // Just started: still proving itself, so the original stays.
        put_job(&mut state, replacement, node(2), JobState::Running, None);
        if let Some(j) = state.jobs.get_mut(&replacement) {
            j.created_at_micros = NOW - 2_000_000;
            j.started_at_micros = Some(NOW - 2_000_000);
        }
        let waiting = plan_at(&dep, &state, &cands, NOW, &policy);
        assert!(waiting.cancels.is_empty() && waiting.starts.is_empty());

        // Up for longer than the default ten seconds: the original stops.
        let proven = plan_at(&dep, &state, &cands, NOW + 20_000_000, &policy);
        assert_eq!(proven.cancels.len(), 1);
        assert_eq!(proven.cancels[0].job_id, original);
        assert!(proven.cancels[0].why.contains("replacement is healthy"));
        assert!(proven.starts.is_empty());
    }

    #[test]
    fn at_the_deadline_the_original_stops_whatever_the_replacement_is_doing() {
        let (mut state, cands) = drained_mesh(&[]);
        let dep = deployment("web", Count::Fixed(1));
        let original = instance_id("web", 0, &Slot::Replica(0), 0);
        put_job(&mut state, original, node(1), JobState::Running, None);
        let replacement = instance_id("web", 0, &Slot::Replica(0), 1);
        put_job(&mut state, replacement, node(2), JobState::Queued, None);
        let late = plan_at(
            &dep,
            &state,
            &cands,
            NOW + 3_600_000_001,
            &mesh_proto::DeploymentPolicy::default(),
        );
        assert_eq!(late.cancels.len(), 1);
        assert!(
            late.cancels[0].why.contains("deadline"),
            "{}",
            late.cancels[0].why
        );
    }

    #[test]
    fn max_parallel_bounds_how_many_move_at_once() {
        let (mut state, cands) = drained_mesh(&[]);
        member(&mut state, 3, "spare2", vec![]);
        let cands: Vec<MemberInfo> = cands
            .into_iter()
            .chain(state.members.get(&node(3)).map(|m| m.info.clone()))
            .collect();
        let dep = deployment("web", Count::Fixed(2));
        for slot in 0..2 {
            put_job(
                &mut state,
                instance_id("web", 0, &Slot::Replica(slot), 0),
                node(1),
                JobState::Running,
                None,
            );
        }
        let one = plan_at(&dep, &state, &cands, NOW, &Default::default());
        assert_eq!(one.starts.len(), 1, "one at a time by default");
        let two = plan_at(
            &dep,
            &state,
            &cands,
            NOW,
            &mesh_proto::DeploymentPolicy {
                instance_env: mesh_proto::MigratePolicy {
                    max_parallel: 2,
                    ..Default::default()
                }
                .to_env(),
                ..Default::default()
            },
        );
        assert_eq!(two.starts.len(), 2);
    }

    /// A machine shutting down with nowhere for its instance to go does not
    /// wait an hour for a machine to appear — that was `meshnode drain` on a
    /// one-node mesh. A machine an admin is emptying stays up, so its instance
    /// keeps serving until the deadline.
    #[test]
    fn nowhere_to_go_stops_a_shutdown_and_waits_for_an_admin_drain() {
        let dep = deployment("web", Count::Fixed(1));
        let original = instance_id("web", 0, &Slot::Replica(0), 0);
        for (extra, stops) in [(&[mesh_proto::DRAINING_TAG][..], true), (&[][..], false)] {
            let (mut state, _) = drained_mesh(extra);
            put_job(&mut state, original, node(1), JobState::Running, None);
            let plan = plan_at(&dep, &state, &[], NOW, &Default::default());
            assert_eq!(!plan.cancels.is_empty(), stops, "{extra:?}");
        }
    }

    #[test]
    fn keep_system_leaves_a_system_instance_where_it_is() {
        let dep = deployment("agent", Count::System);
        let id = instance_id("agent", 0, &Slot::Node(node(1)), 0);
        for (extra, stops) in [
            (&[][..], true),
            (&[mesh_proto::DRAIN_KEEP_SYSTEM_TAG][..], false),
        ] {
            let (mut state, cands) = drained_mesh(extra);
            put_job(&mut state, id, node(1), JobState::Running, None);
            let plan = plan_at(&dep, &state, &cands, NOW, &Default::default());
            assert_eq!(
                plan.cancels.iter().any(|c| c.job_id == id),
                stops,
                "{extra:?}"
            );
        }
    }

    #[test]
    fn a_migrate_block_round_trips_moves_nothing_and_refuses_nonsense() {
        let src = "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n\
                   [deployment.migrate]\nmax_parallel = 2\nmin_healthy_time = \"30s\"\n";
        let parsed = parse(src, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}"));
        let migrate = mesh_proto::MigratePolicy::from_env(&parsed.policy.instance_env);
        assert_eq!((migrate.max_parallel, migrate.min_healthy_secs), (2, 30));
        assert_eq!(migrate.healthy_deadline_secs, 300, "the default");
        let rendered = to_toml(&parsed.deployment, &parsed.policy);
        let again =
            parse(&rendered, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}\n{rendered}"));
        assert_eq!(
            mesh_proto::MigratePolicy::from_env(&again.policy.instance_env),
            migrate
        );
        let plain = parse(&src[..src.find("\n\n").unwrap()], UserId([0u8; 32])).unwrap();
        assert!(!rolls(
            &plain.deployment,
            &plain.policy.instance_env,
            &parsed.deployment,
            &parsed.policy.instance_env
        ));
        let changes = plan_changes(
            &plain.deployment,
            &plain.policy,
            &parsed.deployment,
            &parsed.policy,
        );
        assert!(
            changes.iter().all(|c| c.effect == mesh_proto::PLAN_KNOB),
            "{changes:?}"
        );
        for (bad, says) in [
            ("max_parallel = 0", "at least 1"),
            ("min_healthy_time = \"10m\"", "shorter than"),
        ] {
            let err = parse(
                &format!("[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n[deployment.migrate]\n{bad}\n"),
                UserId([0u8; 32]),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains(says), "{bad}: {err}");
        }
    }

    // ---- Phase C, E10: priority -----------------------------------------

    #[test]
    fn a_priority_is_bounded_round_trips_and_restarts_nothing() {
        let src = "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\npriority = 80\n";
        let parsed = parse(src, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(mesh_proto::priority_of_env(&parsed.policy.instance_env), 80);
        let rendered = to_toml(&parsed.deployment, &parsed.policy);
        let again =
            parse(&rendered, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}\n{rendered}"));
        assert_eq!(mesh_proto::priority_of_env(&again.policy.instance_env), 80);

        let plain = parse(&src.replace("priority = 80\n", ""), UserId([0u8; 32])).unwrap();
        assert_eq!(
            mesh_proto::priority_of_env(&plain.policy.instance_env),
            mesh_proto::DEFAULT_PRIORITY
        );
        assert!(!rolls(
            &plain.deployment,
            &plain.policy.instance_env,
            &parsed.deployment,
            &parsed.policy.instance_env
        ));
        let changes = plan_changes(
            &plain.deployment,
            &plain.policy,
            &parsed.deployment,
            &parsed.policy,
        );
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].effect, mesh_proto::PLAN_PLACEMENT);
        assert!(
            describe(&parsed)
                .iter()
                .any(|(k, v)| k == "priority" && v.contains("--preemptible")),
            "{:?}",
            describe(&parsed)
        );
        for bad in ["0", "101"] {
            let err = parse(&src.replace("80", bad), UserId([0u8; 32]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("between 1 and 100"), "{bad}: {err}");
        }
    }

    // ---- Phase C, U8: the placement map ---------------------------------

    /// Each machine lists the instances it runs with what they asked for,
    /// and a drained machine says so and takes no new work.
    #[test]
    fn the_placement_map_puts_each_instance_on_its_machine() {
        let mut state = MeshState::default();
        member(
            &mut state,
            1,
            "busy",
            vec!["cap.cores=8".into(), "cap.mem_mb=16384".into()],
        );
        member(
            &mut state,
            2,
            "drained",
            mesh_proto::drain_tags(Some(NOW + 60_000_000), false),
        );
        service(&mut state, "collector", 7);
        let dep = deployment("web", Count::Fixed(2));
        state.deployments.insert(
            "web".into(),
            crate::state::DeploymentEntry {
                deployment: dep,
                removed: false,
                updated_hlc: hlc(),
            },
        );
        state.deployment_policies.insert(
            "web".into(),
            crate::state::DeploymentPolicyEntry {
                policy: mesh_proto::DeploymentPolicy {
                    deployment: "web".into(),
                    instance_env: mesh_proto::Requirements {
                        cpus: 2,
                        mem_mb: 512,
                        ..Default::default()
                    }
                    .to_env(),
                    ..Default::default()
                },
                updated_hlc: hlc(),
            },
        );
        for slot in 0..2 {
            put_job(
                &mut state,
                instance_id("web", 0, &Slot::Replica(slot), 0),
                node(1),
                JobState::Running,
                None,
            );
        }
        let map = placement_map(&state, |_| "alive".into());
        assert_eq!(
            map.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["busy", "drained"]
        );
        let busy = &map[0];
        assert_eq!((busy.cores, busy.mem_mb, busy.eligible), (8, 16_384, true));
        assert_eq!(busy.instances.len(), 2);
        assert!(
            busy.instances
                .iter()
                .all(|i| i.deployment == "web" && i.cpus == 2 && i.mem_mb == 512),
            "{:?}",
            busy.instances
        );
        let drained = &map[1];
        assert!(drained.draining && !drained.eligible && !drained.leaving);
        assert!(drained.instances.is_empty());
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    fn dep(service: &str, version: u32, count: u32, generation: u32) -> Deployment {
        Deployment {
            name: "web".into(),
            service: service.into(),
            service_version: version,
            count: mesh_proto::Count::Fixed(count),
            targets: "any".into(),
            restart: mesh_proto::RestartPolicy::default(),
            health: None,
            update: mesh_proto::UpdatePolicy::default(),
            generation,
            inputs: Vec::new(),
            paused: false,
            description: String::new(),
            created_by: mesh_proto::UserId([1; 32]),
            created_at_micros: 0,
            signature: Vec::new(),
        }
    }

    /// **The bug.** A fresh put always carried generation 0, so
    /// re-publishing an edited file reused the generation already running:
    /// the reconciler saw those instances as current, replaced nothing, and
    /// the CLI printed `✓ rollout complete` over a mesh that had not
    /// changed. Verified against a live node before this existed — the old
    /// image was still serving after a "successful" roll.
    #[test]
    fn a_changed_file_starts_a_new_generation() {
        let live = dep("web", 1, 2, 0);
        let next = dep("web", 2, 2, 0);
        assert_eq!(generation_for(Some((&live, &[])), &next, &[]), 1);

        // …and again from there, rather than back to 1.
        let live = dep("web", 2, 2, 1);
        let next = dep("web", 3, 2, 0);
        assert_eq!(generation_for(Some((&live, &[])), &next, &[]), 2);
    }

    /// An unchanged re-publish rolls nothing — what `nomad job run` does
    /// with an unmodified file. Churning every instance because somebody
    /// pressed Publish twice would be its own outage.
    #[test]
    fn an_unchanged_file_rolls_nothing() {
        let live = dep("web", 2, 2, 4);
        let mut next = dep("web", 2, 2, 0);
        // Fields a person may edit without changing what runs.
        next.description = "now with a comment".into();
        next.paused = live.paused;
        assert_eq!(generation_for(Some((&live, &[])), &next, &[]), 4);
    }

    #[test]
    fn a_first_publish_is_generation_zero() {
        assert_eq!(generation_for(None, &dep("web", 1, 1, 0), &[]), 0);
    }

    /// Ports, volumes, resources and environment all live in the policy's
    /// `instance_env`, so a change there is a change to the process even
    /// when the deployment record is byte-identical.
    #[test]
    fn changing_a_port_or_a_limit_is_a_roll() {
        let live = dep("web", 1, 2, 0);
        let next = dep("web", 1, 2, 0);
        let before = [("MESHWARE_TASK_MEM_MB".to_string(), "128".to_string())];
        let after = [("MESHWARE_TASK_MEM_MB".to_string(), "256".to_string())];
        assert_eq!(generation_for(Some((&live, &before)), &next, &after), 1);

        // Order is not a fact about the deployment: the env is assembled by
        // walking several blocks, and two walks that produce the same set
        // in a different order must not roll anything.
        let a = [
            ("MESHWARE_TASK_CPUS".to_string(), "1".to_string()),
            ("MESHWARE_TASK_MEM_MB".to_string(), "128".to_string()),
        ];
        let b = [
            ("MESHWARE_TASK_MEM_MB".to_string(), "128".to_string()),
            ("MESHWARE_TASK_CPUS".to_string(), "1".to_string()),
        ];
        assert_eq!(generation_for(Some((&live, &a)), &next, &b), 0);
    }

    /// Rollout knobs describe how the *next* change rolls. Restarting a
    /// healthy service to record that you raised the canary count would be
    /// an outage caused by editing a comment about outages.
    #[test]
    fn changing_how_it_rolls_does_not_roll_it() {
        let live = dep("web", 1, 2, 3);
        let next = dep("web", 1, 2, 0);
        // `rolls` reads the deployment and `instance_env` only — the policy
        // knobs are deliberately not among its arguments.
        assert!(!rolls(&live, &[], &next, &[]));
        assert_eq!(generation_for(Some((&live, &[])), &next, &[]), 3);
    }

    fn row(source: &str, service: &str, generation: u32) -> mesh_proto::DeploymentRow {
        mesh_proto::DeploymentRow {
            name: "web".into(),
            service: service.into(),
            count: "2".into(),
            targets: "any".into(),
            generation,
            status: "active".into(),
            desired: 2,
            running: 2,
            healthy: 2,
            created_by_username: "sam".into(),
            created_at_micros: 0,
            source: Some(source.into()),
        }
    }

    /// **The row wins where it knows better.** A `rollout` moves the live
    /// generation and service version without rewriting the stored file, so
    /// believing the file's copy of either would compare the new put
    /// against a record that stopped being true two rolls ago.
    #[test]
    fn the_row_overrides_the_stored_file() {
        let src = "deployment:\n  name: web\n  service: web@1\n  count: 2\n";
        let (prev, _) = previous_from_row(&row(src, "web@4", 7), mesh_proto::UserId([1; 32]))
            .expect("a parsable record");
        assert_eq!(
            prev.service_version, 4,
            "the rollout's version, not the file's"
        );
        assert_eq!(prev.generation, 7);

        // And so the put after it continues the sequence.
        let mut next = dep("web", 4, 2, 0);
        next.created_by = prev.created_by;
        assert_eq!(generation_for(Some((&prev, &[])), &next, &[]), 7);
        let next = dep("web", 5, 2, 0);
        assert_eq!(generation_for(Some((&prev, &[])), &next, &[]), 8);
    }

    /// An inline container's stored file says `image:`, never a version —
    /// re-parsing it yields a placeholder. Comparing two placeholders is
    /// how the silent no-op got in, so the row's real version is what the
    /// comparison sees.
    #[test]
    fn an_inline_container_takes_its_version_from_the_row() {
        let src = "deployment:\n  name: web\n  count: 2\n  container:\n    image: nginx:alpine\n";
        let (prev, _) = previous_from_row(&row(src, "web@3", 2), mesh_proto::UserId([1; 32]))
            .expect("a parsable record");
        assert_eq!(prev.service, "web");
        assert_eq!(prev.service_version, 3);
    }

    /// A record with no stored source, or one this build can no longer
    /// parse, is a first publish rather than a refusal — otherwise one
    /// unreadable row would block the deployment that repairs it.
    #[test]
    fn an_unreadable_record_publishes_as_a_first_one() {
        let user = mesh_proto::UserId([1; 32]);
        let mut r = row("deployment:\n  name: web\n  service: web@1\n", "web@1", 3);
        r.source = None;
        assert!(previous_from_row(&r, user).is_none());

        let r = row("{{ not a definition", "web@1", 3);
        assert!(previous_from_row(&r, user).is_none());
        assert_eq!(generation_for(None, &dep("web", 1, 2, 0), &[]), 0);
    }
}

#[cfg(test)]
mod inline_container_tests {
    use super::*;

    fn parse_yaml(src: &str) -> Result<Parsed> {
        parse(src, mesh_proto::UserId([1; 32]))
    }

    const NGINX: &str =
        "deployment:\n  name: web\n  count: 2\n  container:\n    image: nginx:alpine\n";

    /// The whole point: a file that describes its own image names no
    /// service, needs no bundle, and is the only thing an operator writes.
    #[test]
    fn a_file_with_an_image_needs_nothing_else() {
        let p = parse_yaml(NGINX).expect("parse");
        let inline = p.inline.expect("an inline container");
        assert_eq!(inline.image, "nginx:alpine");
        // Named after the deployment, so there is one obvious place to look.
        assert_eq!(inline.service, "web");
        assert_eq!(p.deployment.service, "web");
    }

    /// A TOML file that opens with comments is still TOML (2026-09-14). It
    /// was read as YAML and refused at its own `[deployment]` header. A YAML
    /// file that opens with comments stays YAML.
    #[test]
    fn a_file_may_open_with_comments_in_either_format() {
        let toml = "# the site\n\n[deployment]\nname = \"web\"\n\n[deployment.container]\nimage = \"nginx:alpine\"\n";
        assert_eq!(
            parse_yaml(toml)
                .expect("commented TOML parses")
                .deployment
                .name,
            "web"
        );
        let yaml = format!("# the site\n\n{NGINX}");
        assert_eq!(
            parse_yaml(&yaml)
                .expect("commented YAML parses")
                .deployment
                .name,
            "web"
        );
    }

    /// Two answers to "what runs here" with no rule for which wins, and
    /// none at all, are both refused — with the fix in the message.
    #[test]
    fn a_deployment_says_what_runs_exactly_once() {
        let both = "deployment:\n  name: web\n  service: api@3\n  container:\n    image: nginx\n";
        let e = parse_yaml(both).expect_err("both must be refused");
        assert!(e.to_string().contains("not both"), "{e}");

        let neither = "deployment:\n  name: web\n  count: 2\n";
        let e = parse_yaml(neither).expect_err("neither must be refused");
        assert!(e.to_string().contains("[deployment.container]"), "{e}");
        assert!(
            e.to_string().contains("nginx:alpine"),
            "the refusal shows the shape: {e}"
        );
    }

    #[test]
    fn an_image_that_is_not_one_is_refused_at_write_time() {
        let bad = "deployment:\n  name: web\n  container:\n    image: \"nginx; rm -rf /\"\n";
        let e = parse_yaml(bad).expect_err("must refuse");
        assert!(
            e.to_string().contains("not a usable image reference"),
            "{e}"
        );

        let empty = "deployment:\n  name: web\n  container:\n    image: \"\"\n";
        let e = parse_yaml(empty).expect_err("must refuse");
        assert!(e.to_string().contains("needs an `image`"), "{e}");
    }

    fn existing(image: &str, version: u32, env: Vec<(String, String)>) -> Existing {
        Existing {
            runtime: mesh_proto::Runtime::Container,
            version,
            entrypoint: image.to_string(),
            working_dir: String::new(),
            bundle_id: mesh_proto::BundleId::NONE,
            env,
        }
    }

    /// Publishing an unchanged file must not roll anything, and changing
    /// the image must roll exactly once. This is the rule that makes
    /// "edit the file and publish it again" mean what it does in Nomad.
    #[test]
    fn the_version_follows_the_content_not_the_publish() {
        let user = mesh_proto::UserId([1; 32]);

        // Nothing published yet: version 1.
        let mut p = parse_yaml(NGINX).expect("parse");
        match resolve_inline(None, &mut p, user).expect("resolve") {
            InlineOutcome::Publish { version } => {
                assert_eq!(version.version, 1);
                assert_eq!(version.entrypoint, "nginx:alpine");
                // The bundle that does not exist, which is the point.
                assert!(version.bundle_id.is_none());
                assert_eq!(version.runtime, mesh_proto::Runtime::Container);
            }
            other => panic!("expected a publish, got {other:?}"),
        }
        assert_eq!(p.deployment.service_version, 1);

        // Same file again, against what that published: reuse, no roll.
        let have = existing("nginx:alpine", 1, Vec::new());
        let mut p = parse_yaml(NGINX).expect("parse");
        match resolve_inline(Some(&have), &mut p, user).expect("resolve") {
            InlineOutcome::Reuse { version } => assert_eq!(version, 1),
            other => panic!("an unchanged file must not publish: {other:?}"),
        }
        assert_eq!(p.deployment.service_version, 1);

        // A different image: the next version.
        let changed = NGINX.replace("nginx:alpine", "nginx:1.27-alpine");
        let mut p = parse_yaml(&changed).expect("parse");
        match resolve_inline(Some(&have), &mut p, user).expect("resolve") {
            InlineOutcome::Publish { version } => assert_eq!(version.version, 2),
            other => panic!("a changed image must publish: {other:?}"),
        }
        assert_eq!(p.deployment.service_version, 2);
    }

    /// A command override changes what runs without changing the image, so
    /// it has to count — it lives in the environment, not the entrypoint,
    /// which is exactly the case a naive comparison would miss.
    #[test]
    fn changing_only_the_command_still_bumps_the_version() {
        let user = mesh_proto::UserId([1; 32]);
        let have = existing("nginx:alpine", 1, Vec::new());
        let with_cmd = "deployment:\n  name: web\n  container:\n    image: nginx:alpine\n    command: sh\n    args: [\"-c\", \"echo hi\"]\n";
        let mut p = parse_yaml(with_cmd).expect("parse");
        match resolve_inline(Some(&have), &mut p, user).expect("resolve") {
            InlineOutcome::Publish { version } => {
                assert_eq!(version.version, 2);
                assert!(
                    version
                        .env
                        .iter()
                        .any(|(k, _)| k == mesh_proto::REQ_COMMAND),
                    "the override rides the environment: {:?}",
                    version.env
                );
            }
            other => panic!("a changed command must publish: {other:?}"),
        }
    }

    /// A name already used by a script service is not ours to version.
    #[test]
    fn an_inline_container_will_not_overwrite_someone_elses_service() {
        let user = mesh_proto::UserId([1; 32]);
        let have = Existing {
            runtime: mesh_proto::Runtime::Bash,
            version: 4,
            entrypoint: "deploy.sh".into(),
            working_dir: String::new(),
            bundle_id: mesh_proto::BundleId([7; 32]),
            env: Vec::new(),
        };
        let mut p = parse_yaml(NGINX).expect("parse");
        let e = resolve_inline(Some(&have), &mut p, user).expect_err("must refuse");
        assert!(
            e.to_string().contains("already names a bash service"),
            "{e}"
        );
        assert!(e.to_string().contains("web@4"), "it names the way out: {e}");
    }
}

#[cfg(test)]
mod file_help_tests {
    //! Phase B, E4: the schema, the example and the description are three
    //! statements about the file format, and each is held to the parser.
    use super::*;
    use std::collections::BTreeSet;

    /// The keys a `*Source` struct accepts, read out of serde's own
    /// `deny_unknown_fields` refusal — "unknown field `x`, expected one of
    /// `a`, `b`" — so the parser is asked rather than a list copied from it.
    fn accepted<T: serde::de::DeserializeOwned>() -> BTreeSet<String> {
        let err = match serde_json::from_str::<T>(r#"{"zz-probe-zz": 0}"#) {
            Ok(_) => panic!("a struct that takes an unknown key has lost deny_unknown_fields"),
            Err(e) => e.to_string(),
        };
        let expected = err
            .split("expected")
            .nth(1)
            .unwrap_or_else(|| panic!("no field list in {err:?}"));
        expected
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }

    fn declared(schema: &serde_json::Value, def: &str) -> BTreeSet<String> {
        let ptr = if def.is_empty() {
            "/properties".to_string()
        } else {
            format!("/$defs/{def}/properties")
        };
        schema
            .pointer(&ptr)
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| panic!("the schema has no {ptr}"))
            .keys()
            .cloned()
            .collect()
    }

    /// Both directions at once. A key the schema offers that the parser
    /// refuses sends an editor user into a refusal; a key the parser takes
    /// that the schema lacks gets flagged as a typo in a file that works.
    #[test]
    fn the_schema_names_exactly_the_keys_the_parser_takes() {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).expect("the schema is JSON");
        let pairs: Vec<(&str, BTreeSet<String>)> = vec![
            ("", accepted::<DeploymentSource>()),
            ("deployment", accepted::<DeploymentHeader>()),
            ("resources", accepted::<ResourceSource>()),
            ("container", accepted::<ContainerSource>()),
            ("volume", accepted::<VolumeSource>()),
            ("register", accepted::<RegisterSource>()),
            ("network", accepted::<NetworkSource>()),
            ("port", accepted::<PortSource>()),
            ("restart", accepted::<RestartSource>()),
            ("health", accepted::<HealthSource>()),
            ("update", accepted::<UpdateSource>()),
            ("disconnect", accepted::<DisconnectSource>()),
            ("distinct_property", accepted::<DistinctPropertySource>()),
            ("affinity", accepted::<AffinitySource>()),
            ("spread", accepted::<SpreadSource>()),
            ("migrate", accepted::<MigrateSource>()),
        ];
        for (def, parser) in pairs {
            assert!(!parser.is_empty(), "read no keys for `{def}`");
            assert_eq!(
                declared(&schema, def),
                parser,
                "the schema's `{}` and the parser disagree about which keys exist",
                if def.is_empty() { "(root)" } else { def }
            );
        }
    }

    /// Phase B, E2: `auto_promote = false` is a knob, needs a canary, and
    /// survives being rendered and read back — a scale that re-reads the
    /// definition must not quietly promote.
    #[test]
    fn manual_promotion_is_a_knob_that_round_trips() {
        let src = concat!(
            "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 2\n\n",
            "[deployment.update]\ncanary = 1\nauto_promote = false\n"
        );
        let parsed = parse(src, UserId([0u8; 32])).unwrap();
        assert!(!parsed.knobs.auto_promote());
        let rendered = to_toml_with(&parsed.deployment, &parsed.policy, &parsed.knobs);
        let round =
            parse(&rendered, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}\n{rendered}"));
        assert!(!round.knobs.auto_promote(), "{rendered}");
        assert_eq!(round.policy.canary, 1);
        let plain = parse(
            &src.replace("auto_promote = false\n", ""),
            UserId([0u8; 32]),
        )
        .unwrap();
        assert!(
            plain.knobs.entries.is_empty(),
            "the default publishes nothing"
        );
        let err = parse(&src.replace("canary = 1\n", ""), UserId([0u8; 32]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no canary"), "{err}");
    }

    /// Phase C, E7: a `fact.` test that does not parse is refused where it
    /// was written. `Selector::parse` cannot fail, so this used to publish
    /// and place nothing.
    #[test]
    fn a_malformed_target_is_refused_at_parse() {
        let src = |targets: &str| {
            format!(
                "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 1\ntargets = {targets:?}\n"
            )
        };
        let err = parse(&src("tag=edge, fact.os_id"), UserId([0u8; 32]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("targets:") && err.contains("not a fact test"),
            "{err}"
        );
        let ok = parse(
            &src(r"tag=edge, fact.kernel:version>=6.1, fact.os~/^(debian|ubuntu)$/"),
            UserId([0u8; 32]),
        );
        assert!(ok.is_ok(), "{:?}", ok.err());
    }

    /// `init` and *Deploy an example* hand people a file that works as
    /// written, and `describe` says what it does in words.
    #[test]
    fn the_example_parses_and_describes_itself() {
        let src = example("web", "docker.io/library/nginx:alpine");
        let parsed = parse(&src, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}\n{src}"));
        assert_eq!(parsed.deployment.name, "web");
        assert!(parsed.inline.is_some(), "the example carries its image");
        assert_eq!(parsed.policy.canary, 1);
        assert_eq!(parsed.policy.lost_after_secs, 60);
        let rows = describe(&parsed);
        let get = |k: &str| {
            rows.iter()
                .find(|(label, _)| label == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("runs"), "image docker.io/library/nginx:alpine");
        assert_eq!(get("registers"), "web.mesh");
        assert!(get("rollout").contains("1 canary"), "{rows:?}");
        assert!(get("failover").contains("after 1 min"), "{rows:?}");
    }

    const BASE: &str = r#"
[deployment]
name    = "web"
service = "web@1"
count   = 2

[deployment.health]
kind = "http"
path = "/"
port = 8080

[[deployment.network.port]]
label = "http"
to    = 80
"#;

    fn parsed(src: &str) -> Parsed {
        parse(src, UserId([0u8; 32])).unwrap_or_else(|e| panic!("{e}\n{src}"))
    }

    /// E5: publishing the file that is already published is not a plan
    /// with changes in it.
    #[test]
    fn an_unchanged_file_changes_nothing() {
        let a = parsed(BASE);
        let changes = plan_changes(&a.deployment, &a.policy, &a.deployment, &a.policy);
        assert!(changes.is_empty(), "{changes:?}");
    }

    /// E3: a resize is not a roll — the reconciler adds and retires slots at
    /// the same generation — but switching to or from `system` is, and the
    /// plan says which.
    #[test]
    fn a_resize_does_not_roll_and_a_switch_to_system_does() {
        let env = |p: &Parsed| p.policy.instance_env.clone();
        let two = parsed(BASE);
        let three = parsed(&BASE.replace("count   = 2", "count   = 3"));
        let system = parsed(&BASE.replace("count   = 2", "count   = \"system\""));
        assert!(!rolls(
            &two.deployment,
            &env(&two),
            &three.deployment,
            &env(&three)
        ));
        assert!(rolls(
            &two.deployment,
            &env(&two),
            &system.deployment,
            &env(&system)
        ));

        let resize = plan_changes(
            &two.deployment,
            &two.policy,
            &three.deployment,
            &three.policy,
        );
        assert_eq!(resize.len(), 1, "{resize:?}");
        assert_eq!(resize[0].effect, mesh_proto::PLAN_SCALE);
        let switch = plan_changes(
            &two.deployment,
            &two.policy,
            &system.deployment,
            &system.policy,
        );
        assert!(
            switch
                .iter()
                .any(|c| c.field == "count" && c.effect == mesh_proto::PLAN_ROLLS),
            "{switch:?}"
        );
    }

    /// E3: `scale` refuses what cannot work — in the same words a file gets
    /// for a static port — changes nothing when it refuses, and says what
    /// the size was when it does not.
    #[test]
    fn scale_refuses_what_cannot_work() {
        let mut p = parsed(BASE);
        assert_eq!(scale(&mut p, 5).expect("scale"), 2);
        assert_eq!(p.deployment.count, Count::Fixed(5));
        let zero = scale(&mut p, 0).expect_err("zero").to_string();
        assert!(zero.contains("removal"), "{zero}");

        let mut system = parsed(&BASE.replace("count   = 2", "count   = \"system\""));
        let err = scale(&mut system, 3).expect_err("system").to_string();
        assert!(err.contains("no size to scale"), "{err}");

        let mut pinned = parsed(
            &BASE
                .replace("count   = 2", "count   = 1")
                .replace("to    = 80", "static = 8080"),
        );
        let err = scale(&mut pinned, 2).expect_err("static port").to_string();
        assert!(err.contains("static port"), "{err}");
        assert_eq!(pinned.deployment.count, Count::Fixed(1));
    }

    /// E3: `restart` is the next generation of the same record, and is
    /// refused while paused, when nothing would act on it.
    #[test]
    fn restart_is_the_next_generation_and_not_while_paused() {
        let mut p = parsed(BASE);
        let before = p.deployment.generation;
        restart(&mut p).expect("restart");
        assert_eq!(p.deployment.generation, before + 1);
        p.deployment.paused = true;
        let err = restart(&mut p).expect_err("paused").to_string();
        assert!(err.contains("paused"), "{err}");
    }

    /// E5: each changed field is named by its path in the file, and says
    /// whether it restarts anything — including the default a change pulls
    /// in without being typed.
    #[test]
    fn each_change_says_what_it_does() {
        let a = parsed(BASE);
        let edited = BASE
            .replace("path = \"/\"", "path = \"/healthz\"")
            .replace("to    = 80", "to    = 8080")
            .replacen(
                "count   = 2\n",
                "count   = 2\ndescription = \"the public one\"\n",
                1,
            )
            + "\n[deployment.update]\ncanary = 1\n";
        let b = parsed(&edited);
        let changes = plan_changes(&a.deployment, &a.policy, &b.deployment, &b.policy);
        let effect = |field: &str| {
            changes
                .iter()
                .find(|c| c.field == field)
                .map(|c| c.effect.clone())
                .unwrap_or_else(|| panic!("no change to {field}: {changes:?}"))
        };
        assert_eq!(effect("health.path"), mesh_proto::PLAN_ROLLS);
        assert_eq!(effect("network.port.http.to"), mesh_proto::PLAN_ROLLS);
        assert_eq!(effect("update.canary"), mesh_proto::PLAN_KNOB);
        assert_eq!(effect("update.progress_deadline"), mesh_proto::PLAN_KNOB);
        assert_eq!(effect("description"), mesh_proto::PLAN_RECORD);
        let path = changes
            .iter()
            .find(|c| c.field == "health.path")
            .expect("path");
        assert_eq!(
            (path.from.as_str(), path.to.as_str()),
            ("\"/\"", "\"/healthz\"")
        );
    }
}
