//! What a node can actually run, and how it says so (T1 §4).
//!
//! ## The gap this closes
//!
//! Meshware fingerprints *hardware* — cores, memory, disk, architecture,
//! the dynamic port range, lent volumes, attached devices — announces it as
//! `cap.*` tags, and filters placement on every one of them. It fingerprints
//! nothing about **what this node can run**. So a Python job went to a node
//! with no `python3` and a container job went to a node with no container
//! engine, both placed successfully, and both discovered the truth at spawn
//! as an error with no scheduling consequence.
//!
//! Nomad has had the answer since the beginning: a driver reports
//! *undetected*, *unhealthy* or *healthy*, publishes `driver.docker = "1"`
//! as an ordinary node attribute, and `nomad node status -verbose` prints a
//! table that is the whole answer to "why did my job not run here".
//!
//! ## The shape
//!
//! A runtime is a **capability**, exactly like a GPU: detected by a probe
//! on the node, announced on the node's own record, filtered by the one
//! feasibility function, explained by the refusal that function builds, and
//! shown in `status`, `doctor` and Director. Nothing here is a new
//! mechanism; it is a sixth instance of a pattern the product already has
//! five of.

use serde::{Deserialize, Serialize};

use crate::content::Runtime;

/// The reserved tag listing the runtimes a node can serve (T3 §4.3).
///
/// `cap.rt=bash,exec,python,container`, in [`ALL`] order so two
/// announcements of the same machine compare equal byte-for-byte — the
/// property the re-announce path diffs on.
pub const CAP_RUNTIMES: &str = "cap.rt";

/// Every runtime, in the order a person should read them: the two that need
/// nothing, then the two that need an interpreter, then the one that needs
/// an engine.
pub const ALL: [Runtime; 5] = [
    Runtime::Exec,
    Runtime::Bash,
    Runtime::Python,
    Runtime::Ansible,
    Runtime::Container,
];

/// Whether a node can serve a runtime, and if not, which kind of "not".
///
/// **`Absent` and `Broken` are deliberately separate**, and this is the
/// distinction Nomad gets right: *Docker is installed but the daemon is
/// stopped* is a different sentence, with a different fix, from *Docker is
/// not installed*. Collapsing them is how somebody spends an afternoon
/// installing what they already have.
///
/// The scheduler does not care about the difference — both drop the runtime
/// from `cap.rt` — but the operator reading `doctor` at 3am does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Detection {
    /// Ready to run work of this kind, now.
    Ready,
    /// The dependency is not on this machine at all.
    Absent,
    /// Present but not usable: a daemon that is down, a socket this user
    /// cannot open, an interpreter that will not start.
    Broken,
}

impl Detection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Detection::Ready => "ready",
            Detection::Absent => "absent",
            Detection::Broken => "broken",
        }
    }

    /// Can work of this kind run here?
    pub fn is_ready(&self) -> bool {
        matches!(self, Detection::Ready)
    }
}

/// One runtime's answer on one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeReport {
    pub runtime: Runtime,
    pub detection: Detection,
    /// What was found: `podman 5.8.4 (rootless)`, `python3 3.13.1`. Empty
    /// when nothing was found.
    pub detail: String,
    /// Why not, and what to do about it — the sentence an operator acts on.
    /// Empty when [`Detection::Ready`].
    ///
    /// **Shaped as a headline, then a blank line, then the explanation.**
    /// A remedy for something like rootless podman is three paragraphs and
    /// four shell commands, and there are two places it has to appear: a
    /// table cell and a log line, neither of which can hold it. Putting the
    /// whole thing in both produced a `journalctl` entry rendered as an
    /// indented block wider than the terminal, truncated mid-sentence in
    /// the line that mattered, repeated on every capacity refresh.
    ///
    /// So the first line stands alone and is what those two places print —
    /// see [`RuntimeReport::headline`] — and everything after the first
    /// blank line is the long form, which `meshnode runtimes` prints below
    /// the table where there is room for it. A short remedy has no body and
    /// needs none.
    pub remedy: String,
}

impl RuntimeReport {
    pub fn ready(runtime: Runtime, detail: impl Into<String>) -> RuntimeReport {
        RuntimeReport {
            runtime,
            detection: Detection::Ready,
            detail: detail.into(),
            remedy: String::new(),
        }
    }

    pub fn absent(runtime: Runtime, remedy: impl Into<String>) -> RuntimeReport {
        RuntimeReport {
            runtime,
            detection: Detection::Absent,
            detail: String::new(),
            remedy: remedy.into(),
        }
    }

    pub fn broken(
        runtime: Runtime,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> RuntimeReport {
        RuntimeReport {
            runtime,
            detection: Detection::Broken,
            detail: detail.into(),
            remedy: remedy.into(),
        }
    }

    /// The one line that goes where only one line fits.
    ///
    /// A log entry and a table cell both get this; the rest is
    /// [`RuntimeReport::explanation`]. Trailing whitespace is trimmed so a
    /// remedy that was built by `concat!` and a remedy that was built by
    /// `format!` print identically.
    pub fn headline(&self) -> &str {
        self.remedy
            .split_once('\n')
            .map_or(self.remedy.as_str(), |(first, _)| first)
            .trim_end()
    }

    /// Everything after the headline's blank line, or `None`.
    ///
    /// `None` for a remedy that is a single sentence — most of them — so a
    /// caller can ask for the long form without having to know whether
    /// there is one.
    pub fn explanation(&self) -> Option<&str> {
        let rest = self.remedy.split_once('\n').map(|(_, rest)| rest.trim())?;
        (!rest.is_empty()).then_some(rest)
    }
}

/// What one node can run, as the mesh sees it.
///
/// **Only the list, never the detail.** A probe's `detail` and `remedy` are
/// node-local: they describe a machine this node has not touched, and
/// replicating them would be replicating a diagnosis that was true when the
/// other node booted. What *does* replicate is the conclusion — the
/// `cap.rt` tag — which is exactly what placement uses, so this view and
/// the scheduler agree by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRuntimes {
    pub node: String,
    /// `None` means the node announces no `cap.rt` — a build from before
    /// this existed. Rendered as *unknown*, never as *none*, because that
    /// is what it is and what placement treats it as.
    pub runtimes: Option<Vec<Runtime>>,
}

/// The answer to `meshnode runtimes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimesView {
    /// The node that answered, by name.
    pub node: String,
    /// Its own probes, in full — detail, and the remedy for anything
    /// missing. This is the half worth reading when something will not run.
    pub local: Vec<RuntimeReport>,
    /// Every node in the mesh, as its tags report it. Includes the node
    /// that answered, so one table describes the whole mesh.
    pub mesh: Vec<NodeRuntimes>,
}

impl RuntimesView {
    /// Nodes that announce nothing, by name.
    ///
    /// Surfaced rather than hidden: "unknown is neutral" is the right rule
    /// and it is also an ambiguity, so an operator should be able to see
    /// which machines are carrying it. Usually the answer is "the ones you
    /// have not upgraded yet".
    pub fn unknown_nodes(&self) -> Vec<&str> {
        self.mesh
            .iter()
            .filter(|n| n.runtimes.is_none())
            .map(|n| n.node.as_str())
            .collect()
    }

    /// How many nodes can run this, counting *unknown* as able — the same
    /// arithmetic placement does, so a count here predicts a placement.
    pub fn able(&self, runtime: Runtime) -> usize {
        self.mesh
            .iter()
            .filter(|n| n.runtimes.as_ref().is_none_or(|rs| rs.contains(&runtime)))
            .count()
    }
}

/// The runtimes a report set says this node can serve, in [`ALL`] order.
pub fn ready_of(reports: &[RuntimeReport]) -> Vec<Runtime> {
    ALL.iter()
        .copied()
        .filter(|r| {
            reports
                .iter()
                .any(|rep| rep.runtime == *r && rep.detection.is_ready())
        })
        .collect()
}

/// The `cap.rt` tag for a runtime list, or `None` for an empty one.
///
/// `None` should be unreachable on a real node — [`Runtime::Exec`] needs
/// nothing but the kernel — and that is load-bearing rather than incidental:
/// it is what makes *tag present* mean *this node fingerprints runtimes*.
/// See [`from_tags`].
pub fn cap_tag(runtimes: &[Runtime]) -> Option<String> {
    (!runtimes.is_empty()).then(|| {
        format!(
            "{CAP_RUNTIMES}={}",
            runtimes
                .iter()
                .map(Runtime::as_str)
                .collect::<Vec<_>>()
                .join(",")
        )
    })
}

/// The runtimes a member's tags say it can serve.
///
/// **`None` is not "none" — it is "unknown", and unknown is neutral.**
///
/// This is the opposite of the call N2 made for `cap.ports` and N5 made for
/// `cap.vol`, and the difference is the reason:
///
/// - A node that announces no port range genuinely cannot allocate a port.
///   There has never been another way to get one, so an absent tag is a
///   statement rather than an absence of measurement.
/// - A node that announces no `cap.rt` is running a build from before this
///   track. It may have a `container_plugin` configured and be running
///   containers perfectly well. Excluding it would mean **upgrading one
///   node silently shrinks the mesh's container capacity** — a deployment
///   shedding replicas with nothing anywhere saying why.
///
/// Every node on this build announces at least `cap.rt=exec`, so *tag
/// present* means *the list is complete* and *tag absent* means *too old to
/// say*. `doctor` names any node in the second state, so the ambiguity is
/// visible rather than silently carried.
pub fn from_tags<S: AsRef<str>>(tags: &[S]) -> Option<Vec<Runtime>> {
    let value = tags.iter().find_map(|t| {
        t.as_ref()
            .split_once('=')
            .filter(|(k, _)| *k == CAP_RUNTIMES)
            .map(|(_, v)| v.to_string())
    })?;
    // Fails closed per name, like the volume list: a word this build does
    // not recognise is a runtime it cannot run, so carrying it forward
    // would advertise work nothing here can take.
    Some(
        value
            .split(',')
            .map(str::trim)
            .filter_map(Runtime::parse)
            .collect(),
    )
}

/// Can a node with these tags run this runtime?
///
/// The single predicate the placement filter asks, so the "unknown is
/// neutral" rule lives in exactly one place rather than at each call site
/// that might forget it.
pub fn tags_can_run<S: AsRef<str>>(tags: &[S], want: Runtime) -> bool {
    match from_tags(tags) {
        None => true,
        Some(have) => have.contains(&want),
    }
}

/// The reserved job-environment key naming the runtime a job needs (T3).
///
/// **On the environment carrier, like every other requirement**, and for the
/// reason the carrier exists: `audit_feasible_for_env` derives every filter
/// from one place, so a new requirement reaches every existing call site
/// without one of them being edited — and therefore without one of them
/// being *missed*. N2's port filter and N5's volume filter were each
/// written, tested, and then never consulted, because every caller passed
/// `Default::default()` and nothing noticed for a release.
pub const REQ_RUNTIME: &str = "MESHWARE_TASK_RUNTIME";

/// The runtime a job's environment asks for, if it says.
pub fn from_env(env: &[(String, String)]) -> Option<Runtime> {
    env.iter()
        .find(|(k, _)| k == REQ_RUNTIME)
        .and_then(|(_, v)| Runtime::parse(v.trim()))
}

/// The environment entry a job carrying this runtime should have.
pub fn to_env(runtime: Runtime) -> (String, String) {
    (REQ_RUNTIME.to_string(), runtime.as_str().to_string())
}

/// An environment with its runtime requirement in it, for the feasibility
/// filter (T3).
///
/// The filter derives every requirement from one carrier, so a new one
/// reaches every existing call site without any of them being edited — and
/// therefore without any of them being *missed*. N2's port filter and N5's
/// volume filter were each written, tested, and then never consulted,
/// because every caller passed `Default::default()` and nothing noticed for
/// a release.
///
/// Borrowed when the key is already there, which is how a service that
/// declared its own requirement keeps it: an explicit
/// `MESHWARE_TASK_RUNTIME` in a service definition is the author saying
/// something the payload does not, and overwriting it would silently
/// discard it.
pub fn env_for_placement(
    env: &[(String, String)],
    runtime: Runtime,
) -> std::borrow::Cow<'_, [(String, String)]> {
    if env.iter().any(|(k, _)| k == REQ_RUNTIME) {
        return std::borrow::Cow::Borrowed(env);
    }
    let mut out = env.to_vec();
    out.push(to_env(runtime));
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {

    /// **A remedy has two audiences and only one of them has room.**
    ///
    /// A log line and a table cell get the first line; `meshnode runtimes`
    /// gets the rest. Before this split, journald rendered the whole thing
    /// as an indented block wider than the terminal, truncated on the line
    /// that mattered, once per capacity refresh — and the table truncated
    /// it to sixty characters, so the long form appeared nowhere useful at
    /// all.
    #[test]
    fn a_remedy_leads_with_a_line_that_stands_alone() {
        let long = RuntimeReport::broken(
            Runtime::Container,
            "podman is installed but not answering",
            "this unit's CapabilityBoundingSet has no CAP_DAC_OVERRIDE\n\n\
             Because the helper here is setuid-root.\n\n  sudo systemctl restart meshnode",
        );
        assert_eq!(
            long.headline(),
            "this unit's CapabilityBoundingSet has no CAP_DAC_OVERRIDE"
        );
        assert!(!long.headline().contains('\n'), "a headline is one line");
        let body = long.explanation().expect("a long remedy has a body");
        assert!(body.starts_with("Because"));
        assert!(body.contains("systemctl restart"));

        // The common case: one sentence, no body, and the headline is it.
        let short = RuntimeReport::absent(Runtime::Ansible, "install ansible-playbook");
        assert_eq!(short.headline(), "install ansible-playbook");
        assert_eq!(short.explanation(), None);

        // Ready reports carry no remedy at all and must not invent one.
        let ready = RuntimeReport::ready(Runtime::Bash, "bash 5.3.9");
        assert_eq!(ready.headline(), "");
        assert_eq!(ready.explanation(), None);
    }

    use super::*;

    #[test]
    fn the_tag_round_trips() {
        let have = vec![Runtime::Exec, Runtime::Bash, Runtime::Container];
        let tag = cap_tag(&have).expect("a tag");
        assert_eq!(tag, "cap.rt=exec,bash,container");
        assert_eq!(from_tags(&[tag]), Some(have));
    }

    /// The inversion, as a test rather than as a comment somebody can
    /// contradict: an old node that says nothing is still a candidate.
    #[test]
    fn a_node_that_says_nothing_can_still_run_anything() {
        let old: [String; 1] = ["cap.cores=8".into()];
        assert_eq!(from_tags(&old), None);
        for r in ALL {
            assert!(
                tags_can_run(&old, r),
                "a node too old to announce runtimes must stay a candidate"
            );
        }
    }

    /// …and a node that *does* say is taken at its word, both ways.
    #[test]
    fn a_node_that_says_is_believed() {
        let tags: [String; 2] = ["cap.cores=8".into(), "cap.rt=exec,bash".into()];
        assert!(tags_can_run(&tags, Runtime::Bash));
        assert!(tags_can_run(&tags, Runtime::Exec));
        assert!(!tags_can_run(&tags, Runtime::Container));
        assert!(!tags_can_run(&tags, Runtime::Python));
    }

    /// A word from a future build is dropped rather than carried: it names
    /// work this node has no code to run.
    #[test]
    fn an_unknown_runtime_name_is_dropped() {
        let tags: [String; 1] = ["cap.rt=exec,wasm,bash".into()];
        assert_eq!(from_tags(&tags), Some(vec![Runtime::Exec, Runtime::Bash]));
    }

    /// An empty list is *not* the same as an absent tag. A node that
    /// announced `cap.rt=` (which should never happen — `exec` always
    /// detects) is saying it can run nothing, and is believed.
    #[test]
    fn an_empty_list_is_a_statement_not_a_silence() {
        let tags: [String; 1] = ["cap.rt=".into()];
        assert_eq!(from_tags(&tags), Some(Vec::new()));
        assert!(!tags_can_run(&tags, Runtime::Bash));
    }

    #[test]
    fn the_requirement_rides_the_environment() {
        let env = vec![to_env(Runtime::Container)];
        assert_eq!(from_env(&env), Some(Runtime::Container));
        assert_eq!(from_env(&[]), None);
    }

    #[test]
    fn ready_is_filtered_and_ordered() {
        let reports = vec![
            RuntimeReport::ready(Runtime::Container, "podman 5.8.4"),
            RuntimeReport::absent(Runtime::Python, "install python3"),
            RuntimeReport::ready(Runtime::Exec, ""),
        ];
        // ALL order, not report order.
        assert_eq!(
            ready_of(&reports),
            vec![Runtime::Exec, Runtime::Container],
            "the list must be stable so a re-announce compares equal"
        );
    }
}
