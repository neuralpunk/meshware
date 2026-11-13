//! Workflows: the replicated "and then" (M10 §4).
//!
//! A workflow is a **content entity versioned exactly like a service**
//! (M10 §4.2): immutable user-signed versions, a movable head, a tombstone
//! for removal. Reusing the service triple means reusing `check_user_signed`,
//! the `Developer` role, `@latest` / `@3` resolution, and the rollback
//! story — none of which is new work.
//!
//! Two decisions shape everything here.
//!
//! **Exactly two outgoing lanes per step, `ok` and `fail`, and nothing
//! more.** A canvas makes branching feel free, which is precisely why the
//! branching vocabulary is fixed before the canvas exists. An edge declares
//! which lane it listens on; `fail` edges are how the compensation subgraph
//! is drawn, rather than a separate construct. `needs` stays the underlying
//! representation — a lane is just *which outcome the dependency waits for*
//! — so the YAML a human writes never mentions ports. An optional `when` on
//! the edge covers the rest. No general expression language, no third lane,
//! no per-port fan-out (§4.2).
//!
//! **Layout replicates but is not signed.** A version is immutable and
//! user-signed, so nudging a box 20 px must not mint a new version.
//! [`WorkflowVersion::signing_bytes`] excludes `layout`, exactly as
//! [`crate::SecretRecord::signing_bytes`] excludes `wraps` so late joiners
//! can be rewrapped without invalidating the record.

use serde::{Deserialize, Serialize};

use crate::ipc::SignedIpc;
use crate::{FanId, Hlc, JobId, JobPriority, NodeId, RunId, UserId};

/// Which outcome of an upstream step an edge waits for (§4.2).
///
/// Two variants is the whole vocabulary, and it is deliberately closed:
/// this enum is the concrete form of the "workflows grow into a programming
/// language" risk, and widening it is the thing to argue about, not to do
/// quietly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Lane {
    /// The upstream step succeeded. The default: `needs: [build]` means
    /// "after build, if build worked".
    #[default]
    Ok,
    /// The upstream step failed. How compensation is drawn.
    Fail,
}

impl Lane {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::Ok => "ok",
            Lane::Fail => "fail",
        }
    }

    pub fn parse(s: &str) -> Option<Lane> {
        match s {
            "ok" | "success" => Some(Lane::Ok),
            "fail" | "failure" | "error" => Some(Lane::Fail),
            _ => None,
        }
    }
}

/// One dependency edge: "wait for `step` to finish on `lane`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepEdge {
    pub step: String,
    pub lane: Lane,
    /// Optional guard, evaluated against resolved run state (M10 P5).
    /// Present in the definition from P1 so the format does not change
    /// under a published workflow when P5 lands.
    pub when: Option<String>,
}

impl StepEdge {
    /// The common case: wait for a step to succeed.
    pub fn after(step: impl Into<String>) -> StepEdge {
        StepEdge {
            step: step.into(),
            lane: Lane::Ok,
            when: None,
        }
    }
}

/// What a step runs (§4.2). Mirrors [`crate::JobPayload`], but by
/// *reference*: a workflow version pins a service version, and the
/// resolution to a `ServiceExec` happens at dispatch, not at publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepAction {
    /// An ad-hoc shell command, as `meshnode run` takes.
    Command(String),
    /// A registered service. `version: None` means `@latest` — resolved at
    /// dispatch, so a workflow can deliberately ride a service's head.
    Service {
        name: String,
        version: Option<u32>,
        /// `key=value` inputs, validated against the service's contract at
        /// dispatch. Values may contain `{{…}}` templates.
        inputs: Vec<(String, String)>,
    },
    /// A built-in task (M10 §4.6): a pure string-to-string function the run
    /// loop evaluates itself, on the coordinating node, without dispatching
    /// a job at all.
    ///
    /// Reshaping a value — uppercase it, base64 it, pull a field out of
    /// JSON — used to mean a shell command, which meant a scheduler
    /// decision and a round trip for `echo` with extra steps, and a
    /// workflow that only ran on nodes with the right `jq`. The catalogue
    /// lives in `mesh_core::tasks`; `task` names an entry in it.
    ///
    /// The variant is appended rather than inserted deliberately: postcard
    /// encodes an enum by variant index, and a workflow version's signature
    /// covers those bytes. `Command` and `Service` keep indices 0 and 1, so
    /// every already-published definition still verifies.
    Builtin {
        /// A `TaskDef::id`, e.g. `text.upper`.
        task: String,
        /// Inputs by name. Values may contain `{{…}}` templates.
        inputs: Vec<(String, String)>,
    },
    /// A script written into the workflow itself (§4.7) — a Python or bash
    /// body that dispatches like a command but runs through the runtime
    /// machinery a service uses, virtualenv and all.
    ///
    /// Appended, like `Builtin` before it, so every already-published
    /// definition keeps verifying.
    Script(Box<crate::InlineScript>),
    /// A play from a registered Ansible service, run against the mesh's
    /// **inventory** rather than against mesh members (M12 §6).
    ///
    /// A service step already runs an Ansible bundle, so this variant is
    /// not a second execution path — it resolves to exactly the same
    /// [`crate::ServiceExec`] at dispatch. What it adds is the three things
    /// a service reference cannot say: *which* playbook inside the bundle,
    /// *which hosts* the play targets, and the two flags (`--limit`,
    /// `--tags`, `--check`) that every Ansible operator reaches for. Without
    /// them, "run this play against the edge switches" meant registering one
    /// service per playbook and one more per host set.
    ///
    /// Appended, like `Script` before it: postcard encodes an enum by
    /// variant index and a workflow version's signature covers those bytes,
    /// so every already-published definition still verifies.
    Ansible(Box<AnsiblePlay>),
}

/// What an [`StepAction::Ansible`] step runs, and against what.
///
/// Boxed inside the variant for the same reason [`crate::InlineScript`] is:
/// an eight-field struct inlined into an enum makes every other variant pay
/// for its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsiblePlay {
    /// The service whose bundle carries the playbooks. It must be an
    /// `ansible`-runtime service; publishing refuses anything else, where
    /// the message can name the step.
    pub service: String,
    /// `None` = `@latest`, resolved at dispatch exactly as a service step's
    /// reference is — so a workflow can deliberately ride a bundle's head.
    pub version: Option<u32>,
    /// Which playbook inside the bundle, relative to its `working_dir`.
    /// Empty means the service version's own entrypoint, which is the
    /// common case and the one that needs no picker.
    pub playbook: String,
    /// The inventory selector — `hosts:group=edge`, `mesh:tag=prod`, or the
    /// legacy bare `tag=prod`. Empty means the bundle brings its own
    /// inventory, exactly as running the service directly would.
    ///
    /// Resolution happens on the **origin**, not the executor: it is what
    /// change control judges, what the audit log records, and what
    /// placement is chosen for (M12 §6.2).
    pub inventory: String,
    /// `--limit`. May contain `{{…}}` templates.
    pub limit: String,
    /// `--tags`. May contain `{{…}}` templates.
    pub tags: String,
    /// `--check`: Ansible's own dry run. Distinct from a workflow dry run,
    /// which dispatches nothing at all.
    pub check: bool,
    /// `-e key=value` extra vars. Values may contain `{{…}}` templates.
    pub inputs: Vec<(String, String)>,
}

impl AnsiblePlay {
    /// The service reference as a human writes it: `name` or `name@4`.
    pub fn reference(&self) -> String {
        match self.version {
            Some(v) => format!("{}@{v}", self.service),
            None => self.service.clone(),
        }
    }

    /// One line for a table or a canvas node label.
    pub fn preview(&self) -> String {
        let what = if self.playbook.trim().is_empty() {
            self.reference()
        } else {
            format!("{}/{}", self.reference(), self.playbook)
        };
        if self.inventory.trim().is_empty() {
            what
        } else {
            format!("{what} → {}", self.inventory)
        }
    }
}

impl StepAction {
    /// One line for a table or a canvas node label.
    pub fn preview(&self) -> String {
        const MAX: usize = 60;
        let raw = match self {
            StepAction::Command(c) => c.replace('\n', " "),
            StepAction::Service {
                name,
                version: Some(v),
                ..
            } => format!("{name}@{v}"),
            StepAction::Service { name, .. } => format!("{name}@latest"),
            StepAction::Builtin { task, .. } => task.clone(),
            StepAction::Script(script) => script.preview(),
            StepAction::Ansible(play) => play.preview(),
        };
        if raw.chars().count() <= MAX {
            raw
        } else {
            let cut: String = raw.chars().take(MAX - 1).collect();
            format!("{cut}…")
        }
    }

    /// The service this step pins, if any — what referential integrity is
    /// checked against.
    pub fn service(&self) -> Option<(&str, Option<u32>)> {
        match self {
            StepAction::Service { name, version, .. } => Some((name.as_str(), *version)),
            // An Ansible step pins a bundle by name and version like any
            // other service reference, so it gets the same referential
            // integrity check for free — which is the whole reason it
            // resolves to a `ServiceExec` rather than inventing a payload.
            StepAction::Ansible(play) => Some((play.service.as_str(), play.version)),
            StepAction::Command(_) | StepAction::Builtin { .. } | StepAction::Script(_) => None,
        }
    }

    /// The built-in task this step runs, if any. A built-in never reaches
    /// the scheduler, so every placement, capacity and selector path in the
    /// engine branches on this.
    pub fn builtin(&self) -> Option<&str> {
        match self {
            StepAction::Builtin { task, .. } => Some(task.as_str()),
            _ => None,
        }
    }

    /// The inline script this step runs, if any.
    pub fn script(&self) -> Option<&crate::InlineScript> {
        match self {
            StepAction::Script(script) => Some(script),
            _ => None,
        }
    }

    /// The Ansible play this step runs, if any. Every path that has to
    /// resolve an inventory before placement branches on this.
    pub fn ansible(&self) -> Option<&AnsiblePlay> {
        match self {
            StepAction::Ansible(play) => Some(play),
            _ => None,
        }
    }
}

/// Per-step retry (M10 P5). Carried in the definition from P1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Total attempts, including the first. 1 = no retry.
    pub attempts: u32,
    pub backoff_secs: u64,
}

/// Per-step fan-out controls (M10 P3), reusing the existing dispatcher.
///
/// Both fields stay **strings** rather than parsed numbers because §4.3
/// templates them (`max_parallel: "{{inputs.batch_size}}"`): the value is
/// not known until the run has its inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFanout {
    /// `all`, a count, or a `{{…}}` template.
    pub max_parallel: Option<String>,
    /// `N`, `N%`, or a `{{…}}` template.
    pub halt_on_fail: Option<String>,
}

/// Canvas geometry for one step. Replicated, mutable, **never signed**.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StepLayout {
    pub x: f32,
    pub y: f32,
    /// The user dragged this node: auto-layout must flow around it rather
    /// than move it (§4.4.2 C).
    pub pinned: bool,
}

/// One step of a workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Unique within its graph. Also the correlation key a running step
    /// sees as `MESHWARE_WF_STEP`.
    pub id: String,
    /// Incoming edges. Empty = a root; the graph may have several.
    pub needs: Vec<StepEdge>,
    /// Target selector for this step. `same-as:<step>` pins to the targets
    /// an earlier step actually ran on — a rolling upgrade is wrong if step
    /// two picks a different set of nodes than step one (§4.3).
    pub on: Option<String>,
    pub run: StepAction,
    /// Extra environment. `MESHWARE_WF_*` and `MESHWARE_TASK_*` are
    /// reserved and rejected here.
    pub env: Vec<(String, String)>,
    pub retry: Option<RetryPolicy>,
    pub timeout_secs: Option<u64>,
    pub fanout: Option<StepFanout>,
    /// Output keys this step declares (M10 P4). Declared at publish time so
    /// `{{steps.<id>.outputs.<key>}}` can be checked before a run starts,
    /// not three minutes into a production change (§4.4.2 F).
    pub outputs: Vec<String>,
    /// Treat a failure as success for the purpose of downstream `ok` edges.
    pub continue_on_error: bool,
    /// Canvas position. `None` = auto-laid-out on open, which is also what
    /// a hand-written YAML workflow gets and is not a degraded state.
    pub layout: Option<StepLayout>,
}

impl WorkflowStep {
    /// A minimal, immediately valid step: §4.4.2 B says nothing is ever red
    /// on creation, and this is what "immediately valid" means in code.
    pub fn new(id: impl Into<String>, run: StepAction) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            needs: Vec::new(),
            on: None,
            run,
            env: Vec::new(),
            retry: None,
            timeout_secs: None,
            fanout: None,
            outputs: Vec::new(),
            continue_on_error: false,
            layout: None,
        }
    }
}

/// An immutable workflow version definition, stored in replicated state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowVersion {
    pub name: String,
    pub version: u32,
    /// JSON Schema (2020-12) for the run's `inputs`, as a JSON string —
    /// the same shape a service contract takes, so Director's existing
    /// schema-driven form renders it unchanged (§4.4.1).
    pub contract: Option<String>,
    /// The main graph.
    pub steps: Vec<WorkflowStep>,
    /// The compensation graph: its own DAG, whose roots listen on the
    /// *run's* fail lane. Same two-lane vocabulary inside it (§4.2).
    pub on_failure: Vec<WorkflowStep>,
    pub description: String,
    pub tags: Vec<String>,
    pub created_at_micros: u64,
    /// The user who published this version (a developer or admin).
    pub created_by: UserId,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`],
    /// verified at merge time on every node.
    pub signature: Vec<u8>,
}

impl WorkflowVersion {
    /// The bytes the publisher's signature covers.
    ///
    /// **`WorkflowStep::layout` is excluded**, which is the whole reason
    /// nudging a box does not mint a version. The same escape hatch
    /// `SecretRecord` uses for `wraps`: a field that must stay mutable
    /// cannot be inside the signature.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        for step in unsigned
            .steps
            .iter_mut()
            .chain(unsigned.on_failure.iter_mut())
        {
            step.layout = None;
        }
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(b"mw-workflow-version-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// Every step, main graph then compensation.
    pub fn all_steps(&self) -> impl Iterator<Item = &WorkflowStep> {
        self.steps.iter().chain(self.on_failure.iter())
    }

    pub fn step(&self, id: &str) -> Option<&WorkflowStep> {
        self.all_steps().find(|s| s.id == id)
    }

    /// Service versions this workflow pins, deduplicated. Referential
    /// integrity checks against this: removing a pinned service version is
    /// refused (§4.2).
    pub fn pinned_services(&self) -> Vec<(String, Option<u32>)> {
        let mut out: Vec<(String, Option<u32>)> = Vec::new();
        for step in self.all_steps() {
            if let Some((name, version)) = step.run.service() {
                let entry = (name.to_string(), version);
                if !out.contains(&entry) {
                    out.push(entry);
                }
            }
        }
        out
    }
}

/// Layout for one workflow version, replicated through its own op so it
/// stays mutable without touching the signed definition (§4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowLayout {
    pub name: String,
    pub version: u32,
    /// `(step id, position)`. Steps absent from the list keep whatever the
    /// definition carried, or are auto-laid-out.
    pub positions: Vec<(String, StepLayout)>,
}

/// The working copy of a workflow: what somebody has typed but not yet
/// signed into a version.
///
/// **Why this exists.** Before it, editing was a free *local* draft in the
/// browser and every save minted a signed version. That made the two things
/// people do most — "save my work" and "make this the thing that runs" —
/// the same act, so an afternoon of editing produced forty versions and a
/// confirmation dialog forty times, and a draft you left on one machine was
/// invisible from every other one.
///
/// Splitting them costs one op. A draft replicates so your work follows you
/// around the mesh; a version stays immutable, signed and rare, because it
/// is still the only thing that can run.
///
/// **Source text, not a parsed definition.** A draft is allowed to be
/// half-finished — that is what makes it a draft. Parsing here would make
/// saving fail exactly when a person most wants it to succeed, so
/// validation stays where it belongs: at publish.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowDraft {
    pub name: String,
    /// YAML as authored, `<= MAX_DRAFT_SOURCE`.
    pub source: String,
    /// Who last saved it. Carried for the "edited by" line, *not* as an
    /// authorization claim — the op is unsigned, and the serving node has
    /// already gated the write on this user's content role.
    pub by: UserId,
}

/// A ceiling on draft source, for the same reason `MAX_FACT_DOC` exists: an
/// unsigned op that replicates to every node needs a bound that is not "as
/// much as the editor felt like sending". Generous enough that no real
/// workflow approaches it — the largest in the test corpus is under 8 KB.
pub const MAX_DRAFT_SOURCE: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Runs (M10 P2, §4.2)
// ---------------------------------------------------------------------------
//
// **A workflow run reuses the fan-out ownership model verbatim.** This is
// the single most important decision in M10 and it is why M10 is tractable:
// `WorkflowPlan` / `WorkflowAdopt` / `WorkflowClosed` mirror
// `FanoutPlan` / `FanoutAdopt` / `FanoutClosed` field for field in spirit,
// the origin node owns the run, successor election is the existing
// `scheduler::rendezvous_rank`, and **every step's job id is pre-allocated
// in the plan** exactly as `PlannedTarget` does — so adoption, resume and
// retry can never double-run a step. There is no new coordination
// mechanism here.
//
// Run *state* is its own entity rather than a mutation of `JobSummary`
// (`WorkflowRunUpsert`), so `meshnode jobs` keeps working and a workflow's
// steps appear there as ordinary jobs — which is correct, because that is
// what they are.

/// How a whole run ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// Failed, and the compensation graph ran to completion afterwards
    /// (M10 P5). Distinct from `Failed` because "we cleaned up" is a
    /// materially different thing to tell an operator.
    Compensated,
}

impl RunState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, RunState::Running)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
            RunState::Compensated => "failed (compensated)",
        }
    }
}

/// How one step ended up. A superset of [`crate::JobState`] because a step
/// has two outcomes a job does not: it can be *skipped* (an upstream
/// failure means its lane never fires) and it can be *blocked* (waiting on
/// dependencies that have not finished).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StepState {
    /// Dependencies not satisfied yet.
    #[default]
    Blocked,
    /// Dispatched; a job exists.
    Running,
    Succeeded,
    Failed,
    /// An upstream outcome means this step's lane never fires. Terminal and
    /// *not* a failure: a `fail`-lane step is skipped on a successful run,
    /// which is the normal case, not an error.
    Skipped,
    /// The run was cancelled before this step started.
    Cancelled,
}

impl StepState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, StepState::Blocked | StepState::Running)
    }

    /// Did this step's `ok` lane fire?
    pub fn is_ok(&self) -> bool {
        matches!(self, StepState::Succeeded)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            StepState::Blocked => "blocked",
            StepState::Running => "running",
            StepState::Succeeded => "succeeded",
            StepState::Failed => "failed",
            StepState::Skipped => "skipped",
            StepState::Cancelled => "cancelled",
        }
    }
}

/// One step's pre-allocated identity inside a plan.
///
/// The job id is fixed at plan time for the same reason `PlannedTarget`'s
/// is: a re-dispatch (adoption, resume after restart, retry) must be able
/// to recognise work it already started. Retries get their *own* ids, drawn
/// up front too, so attempt 2 is a distinct job rather than a second
/// dispatch of attempt 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedStep {
    pub step: String,
    /// Job ids per attempt, `attempts[0]` being the first try. Always at
    /// least one; longer when the step declares a retry policy.
    pub attempts: Vec<JobId>,
    /// For a **fan-out step**, the pre-allocated fan-out group per attempt,
    /// parallel to `attempts` (M10 P3).
    ///
    /// A fan-out step's work is a group of per-target jobs, and those job
    /// ids cannot be drawn here because the targets are not known until the
    /// selector is resolved against live membership. Pre-allocating the
    /// *group* id instead is enough: `FanoutPlan` is first-write-wins at
    /// merge, so an adopter that re-dispatches this step finds the group
    /// already planned and drives that plan rather than minting a second
    /// one. The existing fan-out driver then resumes it from replicated
    /// evidence exactly as it would after any other origin failure.
    pub groups: Vec<FanId>,
    /// True for a step in the compensation graph.
    pub compensation: bool,
}

/// Everything a successor needs to finish a run its origin started: the
/// definition (pinned by value, so a head move mid-run cannot change what
/// is executing), the user's signed authorization, and every step's
/// pre-allocated job ids. Replicated as [`crate::OpBody::WorkflowPlan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunPlan {
    pub run: RunId,
    /// The definition as it was when the run started. **By value, not by
    /// reference**: `workflow@latest` is resolved once, at submission, so a
    /// publish halfway through a rolling upgrade cannot swap the steps out
    /// from under it.
    pub definition: Box<WorkflowVersion>,
    /// Run inputs, already validated against the definition's contract.
    pub inputs: Vec<(String, String)>,
    /// The origin node — the initial owner (MASTERPLAN §10.3).
    pub submitter: NodeId,
    pub submitter_user: UserId,
    /// The user's signed request envelope; travels into every step's job
    /// spec, so an executor re-verifies the *user's* authority rather than
    /// trusting the owner node.
    pub auth: SignedIpc,
    pub steps: Vec<PlannedStep>,
    pub priority: JobPriority,
    /// Whole-run timeout, independent of any step's.
    pub timeout_secs: Option<u64>,
    pub created_at_micros: u64,
}

impl WorkflowRunPlan {
    pub fn planned(&self, step: &str) -> Option<&PlannedStep> {
        self.steps.iter().find(|s| s.step == step)
    }
}

/// One step's live state inside a run record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStep {
    pub step: String,
    pub state: StepState,
    /// The attempt currently running or last run, 0-based.
    pub attempt: u32,
    /// The job backing the current attempt, once dispatched. `None` for a
    /// fan-out step, whose work is a group rather than a job.
    pub job_id: Option<JobId>,
    /// The fan-out group backing the current attempt, for a fan-out step.
    pub group: Option<FanId>,
    pub executor: Option<NodeId>,
    pub exit_code: Option<i32>,
    pub started_at_micros: Option<u64>,
    pub finished_at_micros: Option<u64>,
    /// Declared outputs this step published (M10 P4). Values above the
    /// inline threshold are object refs rather than literals.
    pub outputs: Vec<(String, String)>,
    /// Targets this step resolved to, so `same-as:` can pin later steps to
    /// exactly the nodes an earlier one ran on (§4.3).
    pub targets: Vec<NodeId>,
    /// Why it ended up like this, when that is not obvious.
    pub detail: String,
}

impl RunStep {
    pub fn new(step: impl Into<String>) -> RunStep {
        RunStep {
            step: step.into(),
            state: StepState::Blocked,
            attempt: 0,
            job_id: None,
            group: None,
            executor: None,
            exit_code: None,
            started_at_micros: None,
            finished_at_micros: None,
            outputs: Vec::new(),
            targets: Vec::new(),
            detail: String::new(),
        }
    }
}

/// The replicated, mesh-visible view of a run. LWW by `updated_hlc`, the
/// same merge rule `JobSummary` uses — so a successor's view of a run wins
/// over a dead origin's last one without any coordination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunRecord {
    pub run: RunId,
    pub workflow: String,
    pub version: u32,
    pub state: RunState,
    pub steps: Vec<RunStep>,
    pub submitter: NodeId,
    pub submitter_user: UserId,
    pub created_at_micros: u64,
    pub finished_at_micros: Option<u64>,
    /// Set when the run ended for a reason worth naming: a cancel, a
    /// whole-run timeout, a step nobody could place.
    pub detail: String,
    pub updated_hlc: Hlc,
}

impl WorkflowRunRecord {
    pub fn step(&self, id: &str) -> Option<&RunStep> {
        self.steps.iter().find(|s| s.step == id)
    }

    pub fn step_mut(&mut self, id: &str) -> Option<&mut RunStep> {
        self.steps.iter_mut().find(|s| s.step == id)
    }

    /// Steps in each terminal bucket, for a one-line summary.
    pub fn tally(&self) -> (u32, u32, u32) {
        let mut succeeded = 0;
        let mut failed = 0;
        let mut skipped = 0;
        for step in &self.steps {
            match step.state {
                StepState::Succeeded => succeeded += 1,
                StepState::Failed => failed += 1,
                StepState::Skipped | StepState::Cancelled => skipped += 1,
                _ => {}
            }
        }
        (succeeded, failed, skipped)
    }
}

/// Signing bytes for a workflow head move (publish bumps it; rollback
/// rewinds). Mirrors [`crate::service_head_signing_bytes`].
pub fn workflow_head_signing_bytes(name: &str, version: u32, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-wf-head-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&version.to_le_bytes());
    buf.extend_from_slice(&by.0);
    buf
}

/// Signing bytes for a workflow removal.
pub fn workflow_remove_signing_bytes(name: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-wf-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&by.0);
    buf
}

// ---------------------------------------------------------------------------
// Step ↔ run correlation (§4.2)
// ---------------------------------------------------------------------------

/// Prefix owned by the workflow engine on the job environment, reserved the
/// same way `MESHWARE_TASK_*` is: set by the owner, rejected from
/// user-supplied env.
///
/// **Why the environment and not a `JobSpec` field.** `JobSpec` travels
/// positionally inside `Request::SubmitJob`, so growing it is the
/// field-vs-variant trap F0 exists to stop repeating. `JobSpec::env` is an
/// existing `Vec<(String, String)>`, so reserved keys inside it change no
/// encoding at all. It costs zero wire change, the executor needs no
/// workflow awareness whatsoever — only the owner correlates — and it has
/// the pleasant side effect that a step *knows its own run id*, which is
/// useful in scripts.
pub const WF_ENV_PREFIX: &str = "MESHWARE_WF_";

/// The run this job belongs to.
pub const WF_RUN: &str = "MESHWARE_WF_RUN";
/// The step id within that run.
pub const WF_STEP: &str = "MESHWARE_WF_STEP";
/// 1-based attempt number, so a retried step can tell.
pub const WF_ATTEMPT: &str = "MESHWARE_WF_ATTEMPT";

/// A fan-out step whose `env:` sets this key (to anything but `0`) has
/// every target's live output piped into the run stream as
/// [`crate::IpcResponse::RunFanChunk`] frames, attributed by node.
///
/// An env key rather than a `WorkflowStep` field for the reason spelled
/// out above [`WF_ENV_PREFIX`]: the step struct travels positionally
/// inside signed, replicated bytes, so growing it re-encodes every
/// published workflow, while a key in the existing `env` vec changes no
/// encoding at all. **Deliberately not under a reserved prefix** — it must
/// survive publish-time validation and the reserved-key strip, and landing
/// in the job's process environment as a stray variable is harmless.
///
/// Off by default because following every target's log across the mesh is
/// a real cost with no reader on most fan-outs; the `meshbench` workflow
/// is the case it exists for — a screen showing *which kernel on which
/// node, right now*.
pub const STREAM_ENV_KEY: &str = "MESHWARE_STREAM";

// ---------------------------------------------------------------------------
// Selectors that only mean something inside a workflow
// ---------------------------------------------------------------------------

/// `on: "same-as:drain"` — run on exactly the targets `drain` resolved to.
pub const SAME_AS_PREFIX: &str = "same-as:";

/// The step name in a `same-as:<step>` selector.
pub fn same_as_target(selector: &str) -> Option<&str> {
    selector.strip_prefix(SAME_AS_PREFIX).map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str) -> WorkflowStep {
        WorkflowStep::new(id, StepAction::Command("true".into()))
    }

    fn version() -> WorkflowVersion {
        WorkflowVersion {
            name: "rolling-upgrade".into(),
            version: 1,
            contract: None,
            steps: vec![step("drain"), step("upgrade")],
            on_failure: vec![step("rollback")],
            description: String::new(),
            tags: vec![],
            created_at_micros: 1,
            created_by: UserId([3u8; 32]),
            signature: vec![9; 64],
        }
    }

    /// The property that makes the canvas usable: moving a node cannot
    /// invalidate a published version's signature.
    #[test]
    fn layout_is_outside_the_signature() {
        let bare = version();
        let mut moved = version();
        moved.steps[0].layout = Some(StepLayout {
            x: 120.0,
            y: -40.5,
            pinned: true,
        });
        moved.on_failure[0].layout = Some(StepLayout {
            x: 0.0,
            y: 900.0,
            pinned: false,
        });
        assert_eq!(bare.signing_bytes(), moved.signing_bytes());
        assert_ne!(bare, moved, "the layout really did change");
    }

    /// Everything else about a version *is* signed — including the graph
    /// shape, which is the part that decides what runs.
    #[test]
    fn the_graph_is_inside_the_signature() {
        let bare = version();
        let mut rewired = version();
        rewired.steps[1].needs = vec![StepEdge::after("drain")];
        assert_ne!(bare.signing_bytes(), rewired.signing_bytes());

        let mut relaned = version();
        relaned.steps[1].needs = vec![StepEdge {
            step: "drain".into(),
            lane: Lane::Fail,
            when: None,
        }];
        assert_ne!(
            rewired.signing_bytes(),
            relaned.signing_bytes(),
            "which lane an edge listens on decides what runs; it must be signed"
        );
    }

    #[test]
    fn pinned_services_dedupe_and_keep_the_version() {
        let mut v = version();
        v.steps[0].run = StepAction::Service {
            name: "drain-node".into(),
            version: Some(4),
            inputs: vec![],
        };
        v.steps[1].run = StepAction::Service {
            name: "drain-node".into(),
            version: Some(4),
            inputs: vec![],
        };
        v.on_failure[0].run = StepAction::Service {
            name: "apply-release".into(),
            version: None,
            inputs: vec![],
        };
        assert_eq!(
            v.pinned_services(),
            vec![
                ("drain-node".to_string(), Some(4)),
                ("apply-release".to_string(), None)
            ]
        );
    }

    #[test]
    fn same_as_reads_its_target() {
        assert_eq!(same_as_target("same-as:drain"), Some("drain"));
        assert_eq!(same_as_target("same-as: drain "), Some("drain"));
        assert_eq!(same_as_target("env=prod"), None);
    }

    #[test]
    fn lanes_round_trip() {
        for lane in [Lane::Ok, Lane::Fail] {
            assert_eq!(Lane::parse(lane.as_str()), Some(lane));
        }
        assert_eq!(Lane::parse("maybe"), None);
    }
}
