//! Materialized mesh state and the StateEngine (MASTERPLAN §4.4).
//!
//! The op-log is the truth; this module replays it into typed maps with
//! deterministic merge rules:
//! - members: add-wins, revocation is terminal
//! - member attributes: LWW by HLC
//! - invites: create + grow-only consume set + terminal revoke; a consume
//!   or revoke that arrives first is held until the create does
//! - job summaries: LWW by `updated_hlc`
//! - users/grants: add + terminal revocation
//! - secrets: LWW by what the admin signed (version, created-at,
//!   signature); wraps are a union, one statement per healing node
//! - audit: grow-only
//!
//! Authorization is evaluated at apply time on every node: an op from an
//! unauthorized author is rejected everywhere, and governance bodies must
//! additionally carry a valid signature by a user whose role permits the
//! change — so a compromised node cannot smuggle privilege escalations into
//! replicated state (MASTERPLAN §4.4, §7).
//!
//! M3 authorization outcomes for user-signed bodies:
//! - invalid embedded signature, revoked signer, or insufficient role →
//!   the op is applied *as a no-op* (watermark advances, state untouched,
//!   loud warning) so a malicious author cannot wedge sync;
//! - signer or referenced entity not known *yet* → the op parks and is
//!   retried on later sync rounds (grant/user propagation lag).
//!
//! Known M3 caveat, tracked in the risk register: a revocation racing an
//! in-flight op within the gossip window can apply on some nodes and no-op
//! on others; property-based hardening lands with M5's simulation suite.

use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tokio::sync::watch;

use mesh_proto::{
    AuditEntry, AuditId, BundleId, BundleMeta, EnrolmentRequest, FanId, FanoutPlanRecord, GangId,
    GangOutcome, GangPlanRecord, GitSource, Grant, GrantId, Hlc, InviteId, InvitePublic, JobId,
    JobState, JobSummary, MemberInfo, NodeId, OpBody, OpEnvelope, ReleaseKey, ReleaseRecord, Role,
    RunId, RunState, SecretRecord, SecretScope, SecretWrap, ServiceVersion, UpgradeId, UpgradePlan,
    UserId, UserRecord, VersionVector, WorkflowRunPlan, WorkflowRunRecord,
    invite_revoke_signing_bytes, job_cancel_force_signing_bytes, jobs_clear_signing_bytes,
    release_key_revoke_signing_bytes, service_head_signing_bytes, service_remove_signing_bytes,
    upgrade_cancel_signing_bytes,
};
use tracing::{debug, warn};

use crate::error::{CoreError, Result};
use crate::genesis::Genesis;
use crate::hlc::HlcClock;
use crate::identity::{Keypair, verify_signature};
use crate::rbac;
use crate::store::OpStore;

#[derive(Debug, Clone)]
pub struct MemberEntry {
    /// The member as everything reads it: what the node announced, **with
    /// the admin-assigned tags merged in** (`OpBody::NodeTagsSet`).
    ///
    /// Merged here, once, rather than at every `tag=` in the product: the
    /// selector grammar, grants, policies, placement, deployments, git-sync
    /// election, the agent's context and every table all read
    /// `MemberInfo::tags`, and teaching each of them about a second list
    /// would be twenty places to forget one.
    pub info: MemberInfo,
    /// What the node itself last announced, before the merge.
    ///
    /// Kept because the merge has to be *redone*, not accumulated: the
    /// subject re-announces on every boot and every capacity refresh, and
    /// an admin unassigning a tag has to leave the node's own copy of it
    /// alone. Without the un-merged list there is no way to tell a tag the
    /// admin is withdrawing from one the machine is still asserting.
    pub announced_tags: Vec<String>,
    pub added_hlc: Hlc,
    pub updated_hlc: Hlc,
    pub left: bool,
    /// When this member left, if it has (ENG-16). `None` while it is here.
    ///
    /// **Not `updated_hlc`.** That one moves for every re-announce and every
    /// admin tag change, including ones that land after the departure — so
    /// reading it as "when did this node go" gives a different answer on
    /// each node and a *drifting* one over time. Jobs stranded on a
    /// departed executor are settled at this instant, and a settlement time
    /// that moved would put `finished_at_micros` on the wrong side of a
    /// history clear depending on which ops a node happened to have.
    ///
    /// Merged by max over the `MemberLeft` ops seen, so every node converges
    /// on the same instant however they arrive.
    pub left_hlc: Option<Hlc>,
}

/// Tags an admin assigned to one node, and who last said so.
#[derive(Debug, Clone)]
pub struct AssignedTagsEntry {
    pub tags: Vec<String>,
    pub by: UserId,
    pub at_micros: u64,
    pub hlc: Hlc,
}

/// The founder's reset of this mesh, once one has been applied
/// ([`mesh_proto::OpBody::MeshReset`]). Terminal: there is no op that
/// lifts it, because the mesh it describes stops existing.
#[derive(Debug, Clone)]
pub struct MeshResetEntry {
    pub reset: mesh_proto::MeshReset,
    /// The node that published it — the founder's machine, normally.
    pub author: NodeId,
    pub hlc: Hlc,
}

/// The owner's standing answer for one node, and who gave it.
#[derive(Debug, Clone)]
pub struct QuarantineEntry {
    pub reason: String,
    pub by: UserId,
    pub at_micros: u64,
    pub hlc: Hlc,
}

#[derive(Debug, Clone)]
pub struct NodeExecutionEntry {
    pub allowed: bool,
    pub decided_by: UserId,
    pub decided_at_micros: u64,
    pub reason: String,
    pub hlc: Hlc,
}

#[derive(Debug, Clone)]
pub struct InviteEntry {
    pub public: InvitePublic,
    pub consumed_by: BTreeSet<NodeId>,
    pub revoked: bool,
}

/// What arrived for an invite before its create did (SEC-17).
#[derive(Debug, Clone, Default)]
pub struct PendingInvite {
    pub consumed_by: BTreeSet<NodeId>,
    pub revoked: bool,
}

#[derive(Debug, Clone)]
pub struct BundleEntry {
    pub meta: BundleMeta,
    /// Nodes that have announced a complete copy.
    pub holders: BTreeSet<NodeId>,
}

#[derive(Debug, Clone, Default)]
pub struct ServiceEntry {
    /// version -> definition (immutable once added).
    pub versions: BTreeMap<u32, ServiceVersion>,
    /// Current "latest" pointer.
    pub head: u32,
    pub removed: bool,
}

impl ServiceEntry {
    pub fn current(&self) -> Option<&ServiceVersion> {
        if self.removed {
            return None;
        }
        self.versions.get(&self.head)
    }
}

/// A workflow's working copy: saved, replicated, never executable.
///
/// Kept in its own map rather than on [`WorkflowEntry`] because a draft
/// routinely exists for a workflow that has *no* versions yet — the first
/// save of something brand new — and hanging it off the version entry would
/// mean inventing an entry with no versions in it just to hold text.
#[derive(Debug, Clone)]
pub struct DraftEntry {
    pub source: String,
    pub by: UserId,
    /// LWW guard. Also the "last edited" timestamp the surface shows.
    pub at: Hlc,
    /// Cleared by a publish or a discard. The entry outlives the draft as a
    /// tombstone so a late `WorkflowDraftSet` cannot resurrect it.
    pub cleared: bool,
}

#[derive(Debug, Clone)]
pub struct UserEntry {
    pub record: UserRecord,
    /// Terminal: (reason, when). A revoked user key is dead forever.
    pub revoked: Option<(String, Hlc)>,
}

/// One key's place in the enrolment queue (M23 §3.2).
///
/// Keyed by the requesting key, not by a request id: a person asking twice
/// is one row in an admin's queue, not two, and the second ask should
/// replace the first rather than sit under it.
#[derive(Debug, Clone)]
pub struct EnrolmentEntry {
    pub request: EnrolmentRequest,
    /// Set when an admin declined this key: (who, reason, when).
    pub declined: Option<(UserId, String, u64)>,
    pub updated_hlc: Hlc,
}

impl EnrolmentEntry {
    /// Should this row be shown to an admin at `now`?
    ///
    /// Expiry and the decline tombstone are both applied on *read* rather
    /// than by mutating state, so every node answers the same question the
    /// same way without needing a timer that fires identically everywhere
    /// — which a gossip mesh cannot have.
    pub fn is_pending_at(&self, now_micros: u64) -> bool {
        !self.request.is_expired_at(now_micros) && !self.is_declined()
    }

    /// A decline suppresses requests made at or before it. A person who
    /// asks *again* afterwards is asking again, and an admin should see it.
    pub fn is_declined(&self) -> bool {
        self.declined
            .as_ref()
            .is_some_and(|(_, _, at)| self.request.created_at_micros <= *at)
    }
}

/// One standing authorization to roll a release across a ring (M24 §4).
///
/// **Inert.** Nothing in this entry causes anything to happen; it is what a
/// node consults when a driver asks it to upgrade, and the answer is "no"
/// unless every condition on it still holds. That is what separates
/// orchestration from auto-update.
#[derive(Debug, Clone)]
pub struct UpgradePlanEntry {
    pub plan: UpgradePlan,
    /// Published as a **forced** plan: re-install even on a node that
    /// already reports the version (M24 §4).
    ///
    /// A property of the op body that carried the plan rather than of the
    /// plan itself ([`mesh_proto::OpBody::UpgradePlanForce`]), so that
    /// adding it changed no plan's encoding and every older node goes on
    /// reading ordinary ones. It is covered by the signature through
    /// [`UpgradePlan::signing_bytes_forced`], checked above.
    pub force: bool,
    /// Set when an admin withdrew it: (who, when).
    pub cancelled: Option<(UserId, u64)>,
    pub updated_hlc: Hlc,
}

impl UpgradePlanEntry {
    /// Would a node act on this plan right now?
    pub fn is_live_at(&self, now_micros: u64) -> bool {
        self.cancelled.is_none() && !self.plan.is_expired_at(now_micros)
    }
}

/// A minisign public key this mesh trusts to sign releases (M24 §9).
///
/// Revocation is a tombstone rather than a removal, for the reason every
/// revocation in this system is: a key that vanished from state would come
/// back the moment a node that still held the `Put` gossiped it.
#[derive(Debug, Clone)]
pub struct ReleaseKeyEntry {
    pub key: ReleaseKey,
    /// Set when an admin withdrew trust: (who, when).
    pub revoked: Option<(UserId, u64)>,
    pub updated_hlc: Hlc,
}

#[derive(Debug, Clone)]
pub struct GrantEntry {
    pub grant: Grant,
    pub revoked: bool,
}

#[derive(Debug, Clone)]
pub struct SecretEntry {
    /// The winning record. Its `wraps` are rebuilt from [`Self::sealed`]
    /// and [`Self::heals`] whenever those change, so readers see every
    /// wrap in one list.
    pub record: SecretRecord,
    pub removed: bool,
    /// The HLC of the op that installed this record — informational; the
    /// merge orders by what the admin signed (SEC-17).
    pub updated_hlc: Hlc,
    /// The latest signed removal's `at_micros` seen for this name. The
    /// secret is removed while it is later than the record's signed
    /// `created_at_micros`.
    pub removed_at_micros: u64,
    /// The wraps that came with the record, keyed by (recipient, the node
    /// that authored the put). Copies of the same signed record add to
    /// this rather than replacing it (SEC-17).
    pub sealed: BTreeMap<(NodeId, NodeId), SecretWrap>,
    /// Each node's current statement for this record: the wraps it
    /// provides for late joiners, and whether it is asking to be healed
    /// itself (SEC-17).
    ///
    /// Wraps are not signed, so nothing at apply time can tell a good one
    /// from a planted one; the recipient can, by opening the value. Taking
    /// the first wrap per recipient from anybody let a member lock a new
    /// node out by getting there first, and made the outcome depend on
    /// arrival order. One statement per author, its latest, cannot shut the
    /// real wraps out and applies the same everywhere — and it is what
    /// per-author compaction of `SecretRewrap` already assumed it was.
    pub heals: BTreeMap<NodeId, HealStatement>,
}

/// One node's standing word on one secret record (SEC-17).
#[derive(Debug, Clone, Default)]
pub struct HealStatement {
    /// The wraps this node provides, by recipient.
    pub wraps: BTreeMap<NodeId, SecretWrap>,
    /// How many times this node has asked to be healed, while it cannot
    /// open the record; zero once it can. Each ask widens who heals it.
    pub wants: u32,
}

impl SecretEntry {
    fn rebuild_wraps(&mut self) {
        self.record.wraps = self
            .sealed
            .values()
            .chain(self.heals.values().flat_map(|h| h.wraps.values()))
            .cloned()
            .collect();
    }

    fn settle_removed(&mut self) {
        self.removed = self.removed_at_micros > self.record.created_at_micros;
    }

    /// How many times `node` is currently asking to be healed.
    pub fn wants(&self, node: &NodeId) -> u32 {
        self.heals.get(node).map_or(0, |h| h.wants)
    }
}

/// How two copies of a secret are ordered: by what the admin signed
/// (SEC-17). The op's HLC used to break version ties, and any member can
/// re-author a signed record under a later HLC — with whatever wraps it
/// likes, because wraps are not signed. Signed fields cannot be moved.
fn secret_order(r: &SecretRecord) -> (u32, u64, &[u8]) {
    (r.version, r.created_at_micros, r.signature.as_slice())
}

/// A replicated fan-out group (MASTERPLAN §10.3): the origin's plan, any
/// ownership claims made after origin failure, and the terminal outcome.
#[derive(Debug, Clone)]
pub struct FanoutEntry {
    pub plan: FanoutPlanRecord,
    /// Every node that has claimed ownership (grow-only). A claimant's later
    /// FanoutClosed is authorized against this set.
    pub adopters: BTreeSet<NodeId>,
    /// The freshest claim by HLC; None = the origin still owns the group.
    pub adopted: Option<(NodeId, Hlc)>,
    pub closed: Option<FanoutOutcome>,
    /// The earliest wall time any owner closed the group at (ENG-3): the
    /// start of its retention window. The earliest, not the first applied,
    /// so every node settles on the same instant whatever order the
    /// closing ops arrived in.
    pub closed_at_micros: Option<u64>,
}

impl FanoutEntry {
    /// The node currently responsible for driving this fan-out.
    pub fn owner(&self) -> NodeId {
        self.adopted.map(|(n, _)| n).unwrap_or(self.plan.submitter)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FanoutOutcome {
    pub succeeded: u32,
    pub failed: u32,
    pub skipped: u32,
}

/// One gang's replicated record (C0-C9 §8.2).
///
/// Structurally [`FanoutEntry`], because the ownership mechanism is the
/// same one: the origin publishes the plan before it does anything
/// observable, a rendezvous-elected successor adopts it if the origin dies,
/// and whoever owns it writes the terminal record. What differs is only
/// what "already dispatched" means — for a fan-out, some sub-jobs; for a
/// gang, either all of them or none.
#[derive(Debug, Clone)]
pub struct GangEntry {
    pub plan: GangPlanRecord,
    /// Every node that has claimed ownership (grow-only). A claimant's
    /// later `GangClosed` is authorized against this set.
    pub adopters: BTreeSet<NodeId>,
    /// The freshest claim by HLC; None = the origin still owns the gang.
    pub adopted: Option<(NodeId, Hlc)>,
    pub closed: Option<(GangOutcome, u32)>,
    /// When the gang was closed; see [`FanoutEntry::closed_at_micros`].
    pub closed_at_micros: Option<u64>,
}

impl GangEntry {
    /// The node currently responsible for driving this gang.
    pub fn owner(&self) -> NodeId {
        self.adopted.map(|(n, _)| n).unwrap_or(self.plan.submitter)
    }

    /// Did this gang ever get all `N` acceptances and start?
    ///
    /// Read from the members' job summaries rather than from a flag,
    /// because the flag would have to be written by the node that died. A
    /// gang that committed has journaled jobs; one that did not has none,
    /// and a successor can tell the two apart without asking anybody.
    pub fn committed(&self, jobs: &BTreeMap<JobId, JobSummary>) -> bool {
        self.plan
            .members
            .iter()
            .any(|m| jobs.contains_key(&m.job_id))
    }
}

/// The part of a run plan that is never released.
///
/// Small, scalar, and enough to render a row, filter a list or index a run
/// without touching the plan itself. Everything here is copied out of the
/// plan when it is first applied, so shedding the plan costs no read path
/// that only needs these.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run: RunId,
    pub workflow: String,
    pub version: u32,
    pub submitter: NodeId,
    pub submitter_user: UserId,
    pub priority: mesh_proto::JobPriority,
    pub timeout_secs: Option<u64>,
    pub created_at_micros: u64,
    /// Step ids in definition order — what a row shows for a run whose
    /// record never arrived.
    pub step_ids: Vec<String>,
    /// Which of those belong to the compensation graph.
    pub compensation: BTreeSet<String>,
}

impl RunSummary {
    fn of(plan: &WorkflowRunPlan) -> RunSummary {
        RunSummary {
            run: plan.run,
            workflow: plan.definition.name.clone(),
            version: plan.definition.version,
            submitter: plan.submitter,
            submitter_user: plan.submitter_user,
            priority: plan.priority,
            timeout_secs: plan.timeout_secs,
            created_at_micros: plan.created_at_micros,
            step_ids: plan.definition.all_steps().map(|s| s.id.clone()).collect(),
            compensation: plan
                .steps
                .iter()
                .filter(|s| s.compensation)
                .map(|s| s.step.clone())
                .collect(),
        }
    }
}

/// A replicated workflow run (M10 §4.2): the origin's plan, any ownership
/// claims made after origin failure, the live record, and the terminal
/// outcome. Structurally identical to [`FanoutEntry`] because it is the
/// same ownership model.
///
/// ## Shedding (M14 §5)
///
/// A `WorkflowRunPlan` carries the entire workflow definition by value —
/// including any inline scripts — plus the user's signed submission
/// envelope. That is correct and it is also, for a mesh that has been up
/// for a year, most of what a node is holding in memory.
///
/// All of it is *immutable* and all of it is already in the op-log, so once
/// a run is closed and cold the plan is released and `plan_op` is kept
/// instead: a `(author, seq)` locator that reads it back with a point
/// lookup. [`StateEngine::plan_of`] is the only correct way to ask.
///
/// The entry itself is **never removed**, and that is not an oversight.
/// The apply path parks an op for a run it does not know; parking blocks
/// the gapless per-author watermark; a parked op is re-fetched on every
/// gossip round forever. Dropping a run entry would therefore wedge that
/// author's stream. Shedding keeps the entry — LWW still merges, the
/// watermark still advances — and only releases the payload.
#[derive(Debug, Clone)]
pub struct RunEntry {
    /// The full plan while it is worth holding. `None` = shed.
    ///
    /// Private on purpose: every read went through `entry.plan.…` before
    /// this, and the compiler finding those sites is the only reliable way
    /// to be sure each one either needs no more than the summary or asks
    /// [`StateEngine::plan_of`] for the rest.
    plan: Option<Box<WorkflowRunPlan>>,
    /// Where the plan op lives in this node's log, for reading it back.
    /// `None` on a run whose plan was authored before locators were
    /// recorded, which simply means it is never shed.
    plan_op: Option<(NodeId, u64)>,
    /// The scalars, kept whether or not the plan is resident.
    pub summary: RunSummary,
    /// Every node that has claimed ownership (grow-only). A claimant's
    /// later `WorkflowClosed` is authorized against this set.
    pub adopters: BTreeSet<NodeId>,
    /// The freshest claim by HLC; None = the origin still owns the run.
    pub adopted: Option<(NodeId, Hlc)>,
    /// Live per-step state, LWW by `record.updated_hlc`.
    pub record: Option<WorkflowRunRecord>,
    pub closed: Option<RunOutcome>,
}

impl RunEntry {
    pub fn new(plan: WorkflowRunPlan, plan_op: Option<(NodeId, u64)>) -> RunEntry {
        RunEntry {
            summary: RunSummary::of(&plan),
            plan: Some(Box::new(plan)),
            plan_op,
            adopters: BTreeSet::new(),
            adopted: None,
            record: None,
            closed: None,
        }
    }

    /// The plan, if it is still resident. `None` means shed — ask
    /// [`StateEngine::plan_of`], which reads it back from the log.
    pub fn plan(&self) -> Option<&WorkflowRunPlan> {
        self.plan.as_deref()
    }

    pub fn is_shed(&self) -> bool {
        self.plan.is_none()
    }

    /// Release the plan. Returns false when there is nothing to release, or
    /// when no locator was recorded — a plan that could not be read back is
    /// never dropped, because a run that cannot be retried or inspected is
    /// worse than a run that costs memory.
    fn shed(&mut self) -> bool {
        if self.plan.is_none() || self.plan_op.is_none() {
            return false;
        }
        self.plan = None;
        true
    }

    /// The node currently responsible for driving this run.
    pub fn owner(&self) -> NodeId {
        self.adopted
            .map(|(n, _)| n)
            .unwrap_or(self.summary.submitter)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RunOutcome {
    pub state: RunState,
    pub succeeded: u32,
    pub failed: u32,
    pub skipped: u32,
}

/// A registered Git source plus its latest sync status (MASTERPLAN §8.1).
#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub source: GitSource,
    pub removed: bool,
    /// LWW guard for add/remove.
    pub updated_hlc: Hlc,
    pub last: Option<SourceSyncStatus>,
}

#[derive(Debug, Clone)]
pub struct SourceSyncStatus {
    pub commit: String,
    pub bundle_id: BundleId,
    pub by: NodeId,
    pub at: Hlc,
}

/// A sink plus its LWW guard (M31 B1). The schedule shape, verbatim.
#[derive(Debug, Clone)]
pub struct SinkEntry {
    pub sink: mesh_proto::Sink,
    pub removed: bool,
    /// LWW guard for put/remove.
    pub updated_hlc: Hlc,
}

impl SinkEntry {
    /// Is this sink eligible to export at all? Live, not tombstoned, not
    /// disabled.
    ///
    /// The same shape as `ScheduleEntry::is_armed`, and for the same
    /// reason: three different ways of being switched off should read as
    /// one question at every call site.
    pub fn is_armed(&self) -> bool {
        !self.removed && self.sink.enabled
    }
}

/// The mesh's retention policy plus its LWW guard (M31 A1).
///
/// One entry for the whole mesh rather than one per table, because it is
/// one decision and one screen. Absent from any map: it is always *present*
/// as the shipped defaults, which is what makes a mesh bounded before
/// anybody signs anything — and what makes an **upgraded** mesh bounded,
/// which is the case that matters, since the meshes with the growth problem
/// are the old ones.
#[derive(Debug, Clone, Default)]
pub struct RetentionEntry {
    pub policy: mesh_proto::RetentionPolicy,
    /// LWW guard across `RetentionPut` and `RetentionReset` alike: the two
    /// are different acts on the same entity, so they share one clock.
    pub updated_hlc: Hlc,
}

/// A deployment plus its LWW guard (C2 §6.2). The running instances are not
/// here — they are ordinary entries in `jobs`, found by their derived ids —
/// so this map holds only intent, exactly as `schedules` does.
#[derive(Debug, Clone)]
pub struct DeploymentEntry {
    pub deployment: mesh_proto::Deployment,
    pub removed: bool,
    /// LWW guard for put/remove.
    pub updated_hlc: Hlc,
}

/// A service schedule with its tombstone flag (N1 P5). The
/// [`DeploymentEntry`] shape, because it is the same entity pattern: a
/// named, user-signed record that is edited by replacement and retired by
/// tombstone.
#[derive(Debug, Clone)]
pub struct ServiceScheduleEntry {
    pub schedule: mesh_proto::ServiceSchedule,
    pub removed: bool,
    pub updated_hlc: Hlc,
}

impl ServiceScheduleEntry {
    /// Eligible to fire at all? Live, not tombstoned, not paused.
    pub fn is_active(&self) -> bool {
        !self.removed && !self.schedule.paused
    }
}

/// A deployment's rollout and partition policy (N1 P3/P4), on its own
/// clock.
///
/// **Its own map, keyed by deployment name, rather than a field on
/// [`DeploymentEntry`]** — for the ordinary reason and one specific one.
/// The ordinary reason is that it merges on its own HLC: editing a canary
/// count must not have to re-sign the deployment, and re-signing the
/// deployment must not silently drop a policy somebody else set. The
/// specific one is arrival order: ops are not ordered across authors, so
/// the policy can land before the deployment it names, and a field would
/// have needed a placeholder `Deployment` to hang it on. A policy naming
/// no deployment simply governs nothing.
#[derive(Debug, Clone)]
pub struct DeploymentPolicyEntry {
    pub policy: mesh_proto::DeploymentPolicy,
    pub updated_hlc: Hlc,
}

/// A deployment's recent versions and its tags (Phase B, E1).
///
/// Trimmed on every apply to the newest
/// [`mesh_proto::DEPLOYMENT_HISTORY_KEEP`] by HLC, so the set is the same
/// whatever order the ops arrived in: the newest ten of a union is the
/// newest ten of its parts' newest tens. Keyed by generation, and ordered by
/// HLC rather than by generation, because a deployment removed and created
/// again starts counting from the beginning.
#[derive(Debug, Clone, Default)]
pub struct DeploymentHistory {
    /// The newest removal of this name. A version or a tag from before it
    /// belongs to a deployment that no longer exists, whatever the new one
    /// sharing its name is called.
    pub floor: Hlc,
    /// Generation → the latest version published at it, and when.
    pub versions: BTreeMap<u32, (mesh_proto::DeploymentVersion, Hlc)>,
    pub tags: BTreeMap<String, DeploymentTagEntry>,
}

#[derive(Debug, Clone)]
pub struct DeploymentTagEntry {
    pub tag: mesh_proto::DeploymentTag,
    pub updated_hlc: Hlc,
}

/// A deployment's knobs (Phase B, E2), LWW by HLC.
#[derive(Debug, Clone)]
pub struct DeploymentKnobsEntry {
    pub knobs: mesh_proto::DeploymentKnobs,
    pub updated_hlc: Hlc,
}

/// The latest promotion of a deployment's canaries (Phase B, E2).
#[derive(Debug, Clone)]
pub struct DeploymentPromotionEntry {
    pub promotion: mesh_proto::DeploymentPromotion,
    pub updated_hlc: Hlc,
}

impl DeploymentHistory {
    /// Record one published version.
    pub fn record(&mut self, version: &mesh_proto::DeploymentVersion, hlc: Hlc) {
        if hlc <= self.floor {
            return;
        }
        let generation = version.deployment.generation;
        if self
            .versions
            .get(&generation)
            .is_some_and(|(_, at)| *at >= hlc)
        {
            return;
        }
        self.versions.insert(generation, (version.clone(), hlc));
        while self.versions.len() > mesh_proto::DEPLOYMENT_HISTORY_KEEP {
            let oldest = self
                .versions
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(g, _)| *g);
            match oldest {
                Some(g) => {
                    self.versions.remove(&g);
                }
                None => break,
            }
        }
    }

    /// The deployment was removed at `hlc`: forget everything from before.
    pub fn raise_floor(&mut self, hlc: Hlc) {
        if hlc <= self.floor {
            return;
        }
        self.floor = hlc;
        self.versions.retain(|_, (_, at)| *at > hlc);
        self.tags.retain(|_, t| t.updated_hlc > hlc);
    }
}

impl DeploymentEntry {
    /// Is this deployment eligible to reconcile at all? Live, not
    /// tombstoned, not paused.
    pub fn is_active(&self) -> bool {
        !self.removed && !self.deployment.paused
    }
}

/// A fact collector plus its LWW guard (M18 §4.5).
///
/// No tombstone bit, and that is the whole reason M18 needed two op indices
/// rather than three: a collector is retired by *pausing* it, pausing is an
/// edit, and an edit re-signs the record. M8 established the pattern for
/// schedules; this is the second use of it.
#[derive(Debug, Clone)]
pub struct FactCollectorEntry {
    pub collector: mesh_proto::FactCollector,
    pub updated_hlc: Hlc,
}

impl FactCollectorEntry {
    /// Is this collector eligible to run at all?
    pub fn is_armed(&self) -> bool {
        !self.collector.paused
    }
}

/// One collector's latest answer about one subject (M18 §4.4).
///
/// Latest, not every: the history of a fact **is the op-log**, which is
/// never compacted, so keeping older documents in memory as well would be
/// paying twice for the same record. `history.redb` indexes them for
/// `facts diff` (§4.8); this map is what a selector reads.
#[derive(Debug, Clone)]
pub struct FactEntry {
    /// Canonical JSON, exactly as the author hashed it.
    pub doc: String,
    pub digest: [u8; 32],
    /// When the collector observed it — not when the op arrived.
    pub collected_at_micros: u64,
    /// The node that observed and authored it.
    pub author: NodeId,
    /// LWW guard.
    pub updated_hlc: Hlc,
    /// The document parsed once, at apply time.
    ///
    /// Kept alongside the text because every read is a lookup into it — a
    /// selector over a thousand nodes would otherwise re-parse a thousand
    /// documents per placement decision, which is the kind of cost that
    /// only shows up at the scale where it matters.
    pub parsed: serde_json::Value,
}

impl FactEntry {
    /// Resolve a dotted path, as a `fact.*` selector does.
    pub fn get(&self, path: &str) -> Option<String> {
        crate::facts::lookup(&self.parsed, path)
    }
}

/// One co-signature over a release binary (Y6).
#[derive(Debug, Clone)]
pub struct CosignEntry {
    pub cosign: mesh_proto::ReleaseCosignature,
    /// The trusted key line whose signature this is, as verified at merge.
    /// Stored so a list can say *which* token signed without re-parsing,
    /// and so a key that stops being trusted visibly stops vouching.
    pub signed_by_key: String,
    /// A human physically touched the device. `false` for a key that
    /// cannot answer the question — never "unknown treated as yes".
    pub user_present: bool,
    pub updated_hlc: Hlc,
}

/// One trusted attestation authority (Y2/Y3). [`SourceEntry`]'s shape,
/// tombstone included: a root that stops being trusted has to leave
/// a mark, because the attestations it vouched for stop meaning what they
/// meant and an auditor will ask when that happened.
#[derive(Debug, Clone)]
pub struct AttestRootEntry {
    pub root: mesh_proto::attest::AttestRoot,
    pub removed: bool,
    pub updated_hlc: Hlc,
}

/// One person's verified hardware attestation (Y2/Y3).
///
/// **The claim and what verifying it established are both kept.** The claim
/// is the signed op, which is the evidence; `attested` is what this node
/// concluded from it, which is what a list prints. Keeping only the second
/// would leave an audit trail asserting a fact with the proof discarded.
#[derive(Debug, Clone)]
pub struct AttestationEntry {
    pub attestation: mesh_proto::attest::HardwareAttestation,
    pub attested: crate::attest::Attested,
    pub revoked: bool,
    pub revoked_reason: String,
    pub updated_hlc: Hlc,
}

/// The mesh-wide shed, and when it was last set (N6 §12).
#[derive(Debug, Clone)]
pub struct ShedEntry {
    pub shed: mesh_proto::shed::Shed,
    pub updated_hlc: Hlc,
}

/// A session policy plus its tombstone bit (M17 §3.4) — the
/// [`SourceEntry`] shape, for the same reason.
#[derive(Debug, Clone)]
pub struct SessionPolicyEntry {
    pub policy: mesh_proto::SessionPolicy,
    pub removed: bool,
    pub updated_hlc: Hlc,
}

/// The freshest claim on a singleton name.
#[derive(Debug, Clone)]
pub struct SingletonEntry {
    pub job: JobId,
    pub user: UserId,
    pub holder: NodeId,
    pub at_micros: u64,
    pub hlc: Hlc,
}

/// How long a claim is trusted before the job it names has shown up in
/// replicated state. The origin authors the claim and the `JobUpsert` in
/// the same breath, so this only matters on a partitioned mesh — and there
/// the safe reading is "held".
pub const SINGLETON_CLAIM_GRACE_MICROS: u64 = 10 * 60 * 1_000_000;

#[derive(Debug, Default)]
pub struct MeshState {
    pub members: BTreeMap<NodeId, MemberEntry>,
    /// Terminal tombstones: node -> (reason, when).
    pub revoked: BTreeMap<NodeId, (String, Hlc)>,
    /// The owner's standing decision about which nodes may be given work
    /// (`OpBody::NodeExecutionSet`). **Absence means allowed** — a mesh
    /// that has never expressed an opinion schedules exactly as it did
    /// before this record existed — so this map holds only nodes an admin
    /// has actually decided about, in either direction.
    pub node_execution: BTreeMap<NodeId, NodeExecutionEntry>,
    /// Nodes an admin has isolated pending investigation (E2.4).
    /// **Absence means fine** — a mesh that has never quarantined anything
    /// behaves exactly as it did before this existed.
    pub quarantined: BTreeMap<NodeId, QuarantineEntry>,
    /// Tags an admin assigned to nodes from anywhere (`OpBody::NodeTagsSet`).
    ///
    /// The authority for the assigned half; `MemberEntry::info.tags` is the
    /// merged view everything else reads. Kept as its own map so the
    /// assignment survives a node that has not joined yet, or has left and
    /// come back — a machine rebuilt overnight is the same machine to the
    /// operator who labelled it `prod`.
    pub assigned_tags: BTreeMap<NodeId, AssignedTagsEntry>,
    pub invites: BTreeMap<InviteId, InviteEntry>,
    /// Consumes and revokes of invites this node has no create for yet
    /// (SEC-17), folded in when the create lands.
    ///
    /// Ops replay per author, so the member that admitted a joiner can
    /// deliver its `InviteConsume` before the admin's node delivers the
    /// `InviteCreate` — and a revoke can come from a different admin
    /// entirely. Both were dropped, which left the invite with uses it had
    /// already spent and a revoke that never happened.
    pub pending_invites: BTreeMap<InviteId, PendingInvite>,
    /// Where each vault secret goes and who may use it (SEC-7), by name.
    /// Independent of the secret's versions: a scope outlives a new value,
    /// and can be set before the first one.
    pub secret_scopes: BTreeMap<String, SecretScope>,
    pub jobs: BTreeMap<JobId, JobSummary>,
    /// The last job-history clear (`OpBody::JobsClear`), as a wall clock,
    /// merged by max. Zero on a mesh nobody has cleared.
    ///
    /// **The clear lives here rather than only in the emptied map**: ops
    /// replay per author, not in global HLC order, so a `JobUpsert` for a
    /// forgotten job is replayed after the clear on every boot. The
    /// watermark is what it gets checked against, and is why forgetting a
    /// job stays forgotten.
    pub jobs_cleared_before: u64,
    /// Forced cancels (`OpBody::JobCancelForce`), job → the override's HLC.
    ///
    /// **A map in state for the same replay reason as the clear watermark
    /// above**: ops replay per author, and the executor's `JobUpsert` and
    /// another node's force-cancel are in different logs. Whichever lands
    /// second on a given boot must still produce the same summary, so the
    /// override is recorded here and `JobUpsert` checks against it — the
    /// force applies whether it arrives before the job or after it.
    ///
    /// **Entries outlive the jobs a clear forgets.** The force is what
    /// makes a silent job terminal, and a terminal state is what
    /// `cleared_by` needs — so the emptied map is not a record of it, and
    /// pruning the override on the watermark left the next boot's replay
    /// of the executor's `JobUpsert` with a `running` row nothing could
    /// settle and nothing could clear.
    pub jobs_force_cancelled: BTreeMap<JobId, Hlc>,
    pub bundles: BTreeMap<BundleId, BundleEntry>,
    pub services: BTreeMap<String, ServiceEntry>,
    /// Working copies, by workflow name (M10 P4). Unsigned, LWW by HLC, and
    /// deliberately separate from `workflows`: a draft can exist before any
    /// version does, and must never be reachable from anything that runs.
    pub workflow_drafts: BTreeMap<String, DraftEntry>,
    /// Mesh users (MASTERPLAN §7.1). The founder is implicit via genesis
    /// and usually also present here (self-registered at bootstrap).
    pub users: BTreeMap<UserId, UserEntry>,
    /// Which of those users are **machines** (E1-P4). A service account is
    /// a `UserRecord` plus this marker, so every existing RBAC path works
    /// on it unchanged and actor type stays a projection rather than a
    /// field on an already-signed struct.
    pub service_accounts: BTreeMap<UserId, mesh_proto::ServiceAccount>,
    /// Identity providers this mesh will believe (E1-P5), by name.
    pub oidc_providers: BTreeMap<String, (mesh_proto::OidcProvider, Hlc)>,
    /// The most recent verification a node did for each enrolling key.
    pub oidc_assertions: BTreeMap<UserId, (mesh_proto::OidcAssertion, Hlc)>,
    /// Keys asking to become users (M23 §3.2). Not a credential store and
    /// not an authorization input: nothing in this map grants anything,
    /// which is what makes replicating it safe. `users` is where a
    /// decision lands.
    pub enrolments: BTreeMap<UserId, EnrolmentEntry>,
    /// Standing authorizations to roll a release across a ring (M24 §4).
    /// Nothing here triggers anything — see [`UpgradePlanEntry`].
    pub upgrade_plans: BTreeMap<UpgradeId, UpgradePlanEntry>,
    /// meshnode releases published into the mesh, by bare version (§9).
    /// A catalogue: the bytes live in the content store and are fetched
    /// peer-to-peer, verified against the release key on arrival.
    pub releases: BTreeMap<String, ReleaseRecord>,
    /// Minisign public keys this mesh trusts to have signed those
    /// releases, by key line (§9). Replicated so that trusting a new key
    /// is one admin act rather than a file on every machine — and so a
    /// node that joins next month arrives already knowing.
    pub release_keys: BTreeMap<String, ReleaseKeyEntry>,
    /// Scoped role grants (MASTERPLAN §7.2).
    pub grants: BTreeMap<GrantId, GrantEntry>,
    /// Sealed secrets (MASTERPLAN §7.3). Values are ciphertext everywhere.
    pub secrets: BTreeMap<String, SecretEntry>,
    /// Grow-only audit log (MASTERPLAN §7.4).
    pub audit: BTreeMap<AuditId, AuditEntry>,
    /// Fan-out groups: plans, adoption claims, outcomes (MASTERPLAN §10.3).
    pub fanouts: BTreeMap<FanId, FanoutEntry>,
    /// Gangs: all-or-nothing reservations, their plans, adoption claims and
    /// outcomes (C0-C9 §8). The *leases* are deliberately not here — they
    /// are node-local and TTL-bounded, because a replicated lease would
    /// need a distributed cleanup path for an origin that died holding one,
    /// and this design's most important property is that no such path
    /// exists.
    pub gangs: BTreeMap<GangId, GangEntry>,
    /// `run --singleton NAME` claims (`OpBody::SingletonClaim`): name →
    /// the freshest claim. Whether a name is *held* is not stored here —
    /// it is read off the claimed job's replicated state, see
    /// [`MeshState::singleton_holder`].
    pub singletons: BTreeMap<String, SingletonEntry>,
    /// Registered Git sources (MASTERPLAN §8.1).
    pub sources: BTreeMap<String, SourceEntry>,
    /// Workflow runs: plans, adoption claims, live step state, outcomes
    /// (M10 §4.2).
    pub runs: BTreeMap<RunId, RunEntry>,
    /// The mesh-wide load shed (N6 §12). `None` until an admin has ever set
    /// one — which is not the same as a record with both switches clear,
    /// because that one names who lifted it and when.
    pub shed: Option<ShedEntry>,
    /// Second signatures over releases, by binary digest (Y6).
    ///
    /// Each is a `ssh-keygen -Y sign` document from a key on a token. Kept
    /// beside the releases rather than in them because a co-signature can
    /// arrive at any time and from anybody — the authority is the signing
    /// key inside the document, which the mesh checks against the SSH-form
    /// entries in [`Self::release_keys`].
    pub release_cosignatures: BTreeMap<String, CosignEntry>,
    /// Certificate authorities this mesh will accept hardware
    /// attestations from, by [`mesh_proto::attest::AttestRoot::id`]
    /// (Y2/Y3).
    ///
    /// Replicated rather than compiled in, for the reason
    /// [`Self::release_keys`] is: trusting a manufacturer is one admin act
    /// with a name on it, and a mesh whose organisation mints its own PIV
    /// cards can trust its own CA without a new build. **Empty is the
    /// default and it means no attestation verifies** — an unverified
    /// chain is worth less than no chain, because it looks like proof.
    pub attest_roots: BTreeMap<String, AttestRootEntry>,
    /// One attestation per person: "this key was made on hardware and
    /// cannot leave it", with the chain that proves it (Y2/Y3).
    ///
    /// Keyed by user rather than by key, because the `UserId` **is** the
    /// key — attesting a different key is attesting a different identity,
    /// which is `users rotate`, not a second row here.
    pub attestations: BTreeMap<UserId, AttestationEntry>,
    /// Session policies (M17 §3.4): who may open a terminal on which nodes.
    ///
    /// Separate from `policies` because they answer different questions —
    /// change control asks *may this work run now*, this asks *may this
    /// person have a prompt here* — and merging them would mean one record
    /// with two halves that are empty most of the time.
    pub session_policies: BTreeMap<String, SessionPolicyEntry>,
    /// How long this mesh keeps things (M31 A1). Never node config — C9.
    pub retention: RetentionEntry,
    /// Where else this mesh's history goes (M31 B1). Admin-signed and
    /// replicated rather than node config, because a sink is the easiest
    /// exfiltration channel in the product and *who created it* has to be
    /// auditable (C7).
    pub sinks: BTreeMap<String, SinkEntry>,
    /// How far each sink has exported, merged by max (M31 B2, §19 Q3).
    ///
    /// Replicated because the *gate* is not: compaction runs on every node
    /// and the cursor lives on one. Without this, "do not delete what the
    /// sink has not taken" would hold on a single machine and be a lie on
    /// the rest.
    pub sink_progress: BTreeMap<String, u64>,
    /// Deployments: standing declarations that N instances of a service
    /// version should stay up on matching nodes (C2 §6.2). The instances
    /// themselves live in `jobs` under derived ids; this holds only intent.
    pub deployments: BTreeMap<String, DeploymentEntry>,
    /// When each deployment name was last removed while this node held no
    /// record of it (2026-09-14) — the LWW guard a [`DeploymentEntry`] would
    /// otherwise carry. Compaction keeps a deployment's newest put and every
    /// removal, so after put → remove → put the removal replays against a
    /// name with no entry. This is what stops a put *older* than that
    /// removal, arriving afterwards from another author, from bringing the
    /// deployment back. Cleared by the put that wins.
    pub deployment_tombstones: BTreeMap<String, Hlc>,
    /// Rollout and partition policy, keyed by deployment name (N1 P3/P4).
    pub deployment_policies: BTreeMap<String, DeploymentPolicyEntry>,
    /// Recent versions and tags, keyed by deployment name (Phase B, E1).
    pub deployment_history: BTreeMap<String, DeploymentHistory>,
    /// Settings past the policy's fixed fields, by deployment (Phase B, E2).
    pub deployment_knobs: BTreeMap<String, DeploymentKnobsEntry>,
    /// The latest canary promotion, by deployment (Phase B, E2).
    pub deployment_promotions: BTreeMap<String, DeploymentPromotionEntry>,
    /// Schedules that fire a service, keyed by name (N1 P5).
    pub service_schedules: BTreeMap<String, ServiceScheduleEntry>,
    /// The service registry, one record per node (N3 P1).
    ///
    /// Keyed by the node that authored it, because a node can only say
    /// where its own work is. The catalogue a resolver reads is the union
    /// across this map, which is why every node can answer authoritatively
    /// with no round trip — the property Nomad structurally cannot have,
    /// since its check results never leave the client that measured them.
    pub registry: BTreeMap<NodeId, mesh_proto::NodeRegistrations>,
    /// What deployment instances each node currently runs, and their
    /// health (C2 P3). One record per node, LWW — the input a reconciler
    /// gates a rolling update on, and the reason health needs no central
    /// prober: every node has every node's answer about its own instances.
    pub deployment_health: BTreeMap<NodeId, mesh_proto::NodeInstanceHealth>,
    /// Fact collectors: what each node runs to describe itself (M18 §4.5).
    pub collectors: BTreeMap<String, FactCollectorEntry>,
    /// The fleet's self-description: subject → collector → latest document
    /// (M18 §4.4).
    ///
    /// Nested rather than keyed by a pair, because both reads are real:
    /// "everything known about hermes" is a drill-in, and "every node's
    /// `system` document" is a `fact.*` selector, and the outer map serves
    /// the first while the inner one keeps the second a walk rather than a
    /// scan of a composite key.
    pub facts: BTreeMap<mesh_proto::FactSubject, BTreeMap<String, FactEntry>>,
    /// What accelerators each node has, as that node says so (C5 §9.1).
    /// One record per node, LWW, republished only when the hardware or a
    /// device's health changes — so `meshnode nodes --devices` answers from
    /// *any* node without asking anybody, which is the whole reason the
    /// inventory is replicated rather than probed on demand.
    pub devices: BTreeMap<NodeId, mesh_proto::DeviceInventory>,
    /// Usage counters: `(node, principal, window)` → running totals
    /// (C0 P3, consumed by C1 §5.2).
    ///
    /// A G-counter partitioned by author, so the merge is a per-component
    /// max and needs no coordination at all. **Every read filters by
    /// [`mesh_proto::usage_window_is_live`]** — a window past the decay
    /// horizon contributes to nothing — which is what makes compacting
    /// those ops away invisible rather than divergent.
    pub usage: BTreeMap<(NodeId, mesh_proto::UserId, u64), mesh_proto::UsagePartial>,
    /// Applied high-water mark per author (gapless *modulo* `pruned`).
    pub applied: BTreeMap<NodeId, u64>,
    /// The latest timestamp each author has applied under (SEC-5). An op
    /// dated no later than its author's previous one is applied inert: an
    /// honest node's timestamps only go forward (`author_op` takes the
    /// timestamp and the sequence number under one lock), and one that goes
    /// back is a node reaching for a moment when somebody still had a
    /// grant.
    pub author_clock: BTreeMap<NodeId, Hlc>,
    /// The earliest `MemberAdd` seen for each node (SEC-5). A minimum, so it
    /// does not depend on the order adds and re-adds replay in; an author's
    /// first op here may not predate it.
    pub first_added: BTreeMap<NodeId, Hlc>,
    /// Per author, the sequence numbers that author has vouched are gone
    /// on purpose (C0 §4). Learned from its checkpoints and from nowhere
    /// else — a hole nobody signed for still parks, which is what keeps a
    /// genuinely lost op from being silently skipped.
    ///
    /// Empty on every mesh that has never compacted, which is every mesh
    /// before C0 and every quiet mesh after it.
    pub pruned: crate::compact::PrunedMap,
    /// Per author, the lowest `seq` whose body a pre-1.7 peer has no
    /// variant for — the point past which that peer can no longer follow
    /// that author (F0 §3.3). Empty in any mesh that has never authored a
    /// post-1.6 entity kind, which is every mesh until M10 lands.
    ///
    /// Kept here rather than derived on demand because the answer is
    /// needed on the join path, where scanning the log per join would be
    /// the same latent cost F0.4 just removed from the gossip path.
    pub legacy_cut: BTreeMap<NodeId, u64>,
    /// Set once the founder has reset this mesh. Every node that sees it
    /// leaves (`Node`'s reset watch); nothing else reads it.
    pub mesh_reset: Option<MeshResetEntry>,
}

impl MeshState {
    /// May the scheduler place work on this node?
    ///
    /// **Absence means yes.** A mesh whose owner has never expressed an
    /// opinion behaves exactly as it did before the decision existed, which
    /// is what makes this safe to add to a running fleet: the veto is
    /// opt-in, per node, and the only way to acquire one is for an admin to
    /// sign it.
    pub fn may_execute(&self, node: &NodeId) -> bool {
        // Quarantine is checked here so **every** placement path inherits
        // it: `may_execute` is the one filter membership applies when it
        // builds the candidate list, and adding a second question beside
        // it would be a second thing to forget (E2.4).
        !self.is_quarantined(node) && self.node_execution.get(node).is_none_or(|d| d.allowed)
    }

    /// Is this node isolated pending investigation? (E2.4)
    pub fn is_quarantined(&self, node: &NodeId) -> bool {
        self.quarantined.contains_key(node)
    }

    /// Why it is isolated, for the operator staring at a refusal.
    pub fn quarantine(&self, node: &NodeId) -> Option<&QuarantineEntry> {
        self.quarantined.get(node)
    }

    /// The tags an admin assigned to this node, if any.
    pub fn assigned(&self, node: &NodeId) -> Option<&AssignedTagsEntry> {
        self.assigned_tags.get(node)
    }

    /// What a node announced, plus what an admin assigned it — the list
    /// every `tag=` in the product resolves against.
    ///
    /// Order is announced-then-assigned with duplicates dropped, so a tag
    /// the machine already carries is not doubled when an operator assigns
    /// it too, and the node's own list keeps the position it announced.
    pub fn merged_tags(announced: &[String], assigned: &[String]) -> Vec<String> {
        let mut out = announced.to_vec();
        for t in assigned {
            if !out.iter().any(|x| x == t) {
                out.push(t.clone());
            }
        }
        out
    }

    /// Re-merge one member's tag list from its two halves.
    ///
    /// Called from both sides of the merge — the node re-announcing, and an
    /// admin changing the assignment — because either can move.
    fn remerge_tags(&mut self, node: &NodeId) {
        let assigned = self
            .assigned_tags
            .get(node)
            .map(|e| e.tags.clone())
            .unwrap_or_default();
        if let Some(entry) = self.members.get_mut(node) {
            entry.info.tags = Self::merged_tags(&entry.announced_tags, &assigned);
        }
    }

    /// Why the owner is not sending work to this node, when they are not.
    pub fn execution_refusal(&self, node: &NodeId) -> Option<&NodeExecutionEntry> {
        self.node_execution.get(node).filter(|d| !d.allowed)
    }

    /// The claim holding `name` right now, if any: the freshest claim whose
    /// job is still non-terminal here, or too young for its summary to
    /// have arrived. A claim on a finished, cleared or force-cancelled job
    /// holds nothing.
    pub fn singleton_holder(&self, name: &str, now_micros: u64) -> Option<&SingletonEntry> {
        let entry = self.singletons.get(name)?;
        match self.jobs.get(&entry.job) {
            Some(job) if !job.state.is_terminal() => Some(entry),
            Some(_) => None,
            None if now_micros.saturating_sub(entry.at_micros) < SINGLETON_CLAIM_GRACE_MICROS => {
                Some(entry)
            }
            None => None,
        }
    }

    pub fn is_active_member(&self, id: &NodeId) -> bool {
        !self.revoked.contains_key(id) && self.members.get(id).is_some_and(|m| !m.left)
    }

    /// When this node stopped being able to speak for itself, if it has
    /// (ENG-16): the instant it was revoked, or the instant it left.
    ///
    /// Revocation wins where a node has both, because it is the stronger
    /// statement and the one that cannot be undone by coming back.
    ///
    /// A node that is merely *unreachable* is not gone: it is expected
    /// back, its jobs may still be running, and settling them would be a
    /// lie that a returning node then has to argue with. Only the two
    /// facts that are permanent count here.
    pub fn gone_at(&self, id: &NodeId) -> Option<Hlc> {
        if let Some((_, at)) = self.revoked.get(id) {
            return Some(*at);
        }
        self.members
            .get(id)
            .filter(|m| m.left)
            .and_then(|m| m.left_hlc)
    }

    /// The current version of a live service by name.
    pub fn service_current(&self, name: &str) -> Option<&ServiceVersion> {
        self.services.get(name).and_then(|s| s.current())
    }

    /// Holders of a bundle that are currently active members.
    pub fn bundle_holders(&self, id: &BundleId) -> Vec<NodeId> {
        self.bundles
            .get(id)
            .map(|b| {
                b.holders
                    .iter()
                    .filter(|n| self.is_active_member(n))
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every bundle something in replicated state still needs — the set a
    /// content sweep must not touch however aggressive its window (M31 A2).
    ///
    /// The retention window answers *"how long do we keep what nobody is
    /// using"*. It is never allowed to answer *"is anybody using this"*,
    /// because a window that can delete a running deployment's code is not
    /// a retention policy, it is an outage on a timer. So the two questions
    /// are separated: this function is the second one, and the sweep may
    /// only ever consider what this set leaves out.
    ///
    /// What holds a reference:
    ///
    /// - **Every version of every live service.** All of them, not just the
    ///   head: rolling back to `collector@6` is an ordinary Tuesday, and a
    ///   rollback that has to refetch from a peer is a rollback that fails
    ///   when the peer is the thing that broke.
    /// - **Every service version a deployment pins**, even where the
    ///   service itself has been removed. A tombstoned service with a live
    ///   deployment still pinned to it is a mess, but it is a *running*
    ///   mess, and reclaiming its code would turn a mess into an outage.
    /// - **The head of every live Git source**, which is what the next sync
    ///   diffs against.
    /// - **Releases, but only the ones that are still answers**: the version
    ///   this node is running (so a rollback is local) and the newest
    ///   published version (so the next upgrade does not refetch what it
    ///   already has). The rest are catalogue entries whose bytes re-fetch
    ///   on demand, verified against the record's own minisig and BLAKE3 —
    ///   which is precisely the property that makes reclaiming them safe,
    ///   and why nineteen old releases are not something to keep forever.
    ///
    /// `running_version` is this node's own `--version`. `None` protects no
    /// release on that ground, which is the right answer for a caller that
    /// does not know rather than a reason to guess.
    /// How many versions, counting down from the head, keep their bytes so
    /// that a rollback is still a local operation (ENG-19).
    ///
    /// Five, not one: rolling back is what this window is for, and an
    /// operator who has to roll back twice in an afternoon is having the
    /// kind of day where the second one must not need a re-upload. Not
    /// fifty either — every version kept is a bundle's worth of disk on
    /// every node that ever held it, and the reason this number exists is
    /// that "all of them" was the previous answer.
    ///
    /// A version an operator wants to keep past the window is a version
    /// something should *point at*: a deployment pinned to it is protected
    /// by name, whatever its age.
    pub const ROLLBACK_VERSIONS: usize = 5;

    pub fn referenced_bundles(&self, running_version: Option<&str>) -> BTreeSet<BundleId> {
        let mut keep = BTreeSet::new();
        for entry in self.services.values() {
            if entry.removed {
                continue;
            }
            // **Not every version** (ENG-19). Protecting the whole history
            // meant a CI job that publishes on every commit filled
            // `content/` and never gave a byte back, until the store hit
            // its floor and bundle fetches started being refused — on a
            // mesh where all but the newest few versions had not been run
            // in months.
            //
            // What is kept is what could still be *asked for*: the head,
            // and a rollback window below it. A deployment pinned to an
            // older version is protected separately, below, so a pin
            // outside the window is still safe — that loop is what makes
            // this one safe to narrow.
            keep.extend(
                entry
                    .versions
                    .range(..=entry.head)
                    .rev()
                    .take(Self::ROLLBACK_VERSIONS)
                    .map(|(_, v)| v.bundle_id),
            );
            // A version *above* the head is one published but not promoted
            // — a canary, or a publish mid-rollout. Nothing has decided
            // against it yet, so it is not history.
            keep.extend(
                entry
                    .versions
                    .range((entry.head.saturating_add(1))..)
                    .map(|(_, v)| v.bundle_id),
            );
        }
        for entry in self.deployments.values() {
            if entry.removed {
                continue;
            }
            let d = &entry.deployment;
            if let Some(version) = self
                .services
                .get(&d.service)
                .and_then(|s| s.versions.get(&d.service_version))
            {
                keep.insert(version.bundle_id);
            }
        }
        for entry in self.sources.values() {
            if entry.removed {
                continue;
            }
            keep.extend(entry.last.as_ref().map(|l| l.bundle_id));
        }
        // Releases: the running one and the newest one, across every
        // architecture published for each. A node that reclaimed the arch
        // it does not run has reclaimed nothing it would ever have used.
        let newest = self
            .releases
            .values()
            .map(|r| r.version.as_str())
            .max_by(|a, b| mesh_proto::version::version_cmp(a, b))
            .map(str::to_string);
        for version in [running_version.map(str::to_string), newest]
            .into_iter()
            .flatten()
        {
            keep.extend(crate::release::records_for(&self.releases, &version).map(|r| r.bundle_id));
        }
        keep
    }

    /// The SSH-form release keys this mesh trusts (Y6).
    ///
    /// The same `release_keys` map the minisign keys live in, filtered by
    /// shape — a minisign line starts `RW`, an SSH one names its algorithm
    /// first, and the two can never be mistaken for each other. One record
    /// type, one admin verb, two kinds of signature.
    ///
    /// **A mesh with none behaves exactly as it does today.** A mesh with
    /// one requires a valid co-signature on every release: opt-in,
    /// fleet-wide, and fail-closed the moment it is opted into.
    pub fn trusted_ssh_release_keys(&self) -> Vec<String> {
        self.release_keys
            .values()
            .filter(|e| e.revoked.is_none())
            .map(|e| e.key.pubkey.clone())
            .filter(|line| crate::sshsig::is_ssh_key_line(line))
            .collect()
    }

    /// The minisign release keys this mesh trusts.
    pub fn trusted_minisign_release_keys(&self) -> Vec<String> {
        self.release_keys
            .values()
            .filter(|e| e.revoked.is_none())
            .map(|e| e.key.pubkey.clone())
            .filter(|line| !crate::sshsig::is_ssh_key_line(line))
            .collect()
    }

    /// The attestation authorities this mesh currently trusts, as
    /// [`crate::attest::verify`] wants them (Y2/Y3).
    ///
    /// Removed roots are left out rather than marked, because a verifier
    /// has no use for an authority it must not use — and the tombstone
    /// stays in state so an auditor can still see when trust ended.
    pub fn trusted_attest_roots(&self) -> Vec<(String, String, Vec<u8>)> {
        self.attest_roots
            .iter()
            .filter(|(_, e)| !e.removed)
            .map(|(id, e)| (id.clone(), e.root.label.clone(), e.root.certificate.clone()))
            .collect()
    }

    /// The username a principal goes by, for the queue and for reports.
    pub fn username(&self, id: &mesh_proto::UserId) -> Option<&str> {
        self.users.get(id).map(|e| e.record.username.as_str())
    }

    /// One principal's usage summed across every node, over the windows
    /// that are still read (C0 P3; the input C1's DRF wants).
    ///
    /// The filter is the whole contract: two nodes with different op-logs —
    /// one compacted, one not — return the same numbers, because what is
    /// summed is decided by the clock and a mesh-wide constant, never by
    /// what happens to be on disk.
    pub fn usage_of(&self, principal: &mesh_proto::UserId, now_micros: u64) -> (u64, u64, u64) {
        let mut totals = (0u64, 0u64, 0u64);
        for ((_, who, window), partial) in &self.usage {
            if who != principal || !mesh_proto::usage_window_is_live(*window, now_micros) {
                continue;
            }
            totals.0 = totals.0.saturating_add(partial.cpu_secs);
            totals.1 = totals.1.saturating_add(partial.mem_mb_secs);
            totals.2 = totals.2.saturating_add(partial.gpu_secs);
        }
        totals
    }

    /// Every principal with live usage, and their summed totals.
    pub fn usage_by_principal(
        &self,
        now_micros: u64,
    ) -> BTreeMap<mesh_proto::UserId, (u64, u64, u64)> {
        let mut out: BTreeMap<mesh_proto::UserId, (u64, u64, u64)> = BTreeMap::new();
        for ((_, who, window), partial) in &self.usage {
            if !mesh_proto::usage_window_is_live(*window, now_micros) {
                continue;
            }
            let e = out.entry(*who).or_default();
            e.0 = e.0.saturating_add(partial.cpu_secs);
            e.1 = e.1.saturating_add(partial.mem_mb_secs);
            e.2 = e.2.saturating_add(partial.gpu_secs);
        }
        out
    }

    /// [`Self::usage_by_principal`], skipping one node's contribution.
    ///
    /// The caller is that node, and it has fresher numbers of its own in
    /// its meter (C1 P4). Adding the replicated copy on top would count the
    /// same seconds twice and hand the busiest principal a share that
    /// climbs by itself.
    pub fn usage_by_principal_excluding(
        &self,
        skip: &NodeId,
        now_micros: u64,
    ) -> BTreeMap<mesh_proto::UserId, (u64, u64, u64)> {
        let mut out: BTreeMap<mesh_proto::UserId, (u64, u64, u64)> = BTreeMap::new();
        for ((node, who, window), partial) in &self.usage {
            if node == skip || !mesh_proto::usage_window_is_live(*window, now_micros) {
                continue;
            }
            let e = out.entry(*who).or_default();
            e.0 = e.0.saturating_add(partial.cpu_secs);
            e.1 = e.1.saturating_add(partial.mem_mb_secs);
            e.2 = e.2.saturating_add(partial.gpu_secs);
        }
        out
    }

    /// One node's own live usage windows, keyed for merging against a
    /// fresher local reading (C1 P4).
    pub fn own_usage_windows(
        &self,
        me: &NodeId,
        now_micros: u64,
    ) -> BTreeMap<(mesh_proto::UserId, u64), mesh_proto::UsagePartial> {
        self.usage
            .iter()
            .filter(|((node, _, w), _)| {
                node == me && mesh_proto::usage_window_is_live(*w, now_micros)
            })
            .map(|((_, who, window), partial)| ((*who, *window), *partial))
            .collect()
    }

    pub fn invite_uses_left(&self, id: &InviteId) -> Option<u32> {
        self.invites.get(id).map(|inv| {
            if inv.revoked {
                0
            } else {
                inv.public
                    .uses
                    .saturating_sub(u32::try_from(inv.consumed_by.len()).unwrap_or(u32::MAX))
            }
        })
    }
}

/// What [`StateEngine::try_apply`] may settle rather than park or report.
#[derive(Debug, Clone, Copy)]
struct ApplyMode {
    /// An orphaned group reference (ENG-3).
    release_orphans: bool,
    /// Any op that would park — this node's own, on its disk (ENG-4).
    settle_parked: bool,
    /// This node authoring: a body-level refusal is reported, not counted.
    authoring: bool,
    /// The op came off this node's own store, so its signature was checked
    /// when it arrived and is not checked again (**H-4**, ENG-20).
    ///
    /// Boot re-verified every op ever recorded: an Ed25519 verification and
    /// a `signing_bytes()` allocation per op, every start, growing with the
    /// node's history rather than with anything it is about to do. On a
    /// mesh with a year of audit rows that is the whole of a slow start.
    ///
    /// **What this does and does not assume.** It assumes the local store
    /// is as trustworthy as the process reading it — which it is: it lives
    /// in the node's own data dir, and anyone who can rewrite it can
    /// rewrite the binary, the identity key and the genesis document
    /// beside it. It does *not* extend to anything arriving from a peer, a
    /// restore, or a rebuild: every one of those goes through the full
    /// check, because the store is only trusted to give back what this node
    /// verified and put there. Corruption is a different question and redb
    /// answers it with its own page checksums.
    trusted: bool,
}

/// Why an op could not be applied right now.
#[derive(Debug, PartialEq, Eq)]
enum ApplyOutcome {
    Applied,
    /// Authentic, in order, and authorized — but this build has no code for
    /// its body (F0). The watermark advances and the op is persisted and
    /// forwarded verbatim; only *interpretation* is missing, and
    /// interpretation is local. This is the existing "applied as a no-op so
    /// a malicious author cannot wedge sync" rule (see the module header),
    /// extended from ops we refuse to honor to ops we cannot decode.
    AppliedOpaque,
    /// Authentic, in order and authorized, but dated before its author's
    /// previous op (SEC-5). Applied as a no-op exactly as an opaque op is —
    /// the watermark advances and it is carried — so a bad timestamp can
    /// neither take effect nor wedge the author's sequence.
    AppliedInert,
    /// Duplicate or stale — safely ignorable.
    Ignored,
    /// Parked, *and it taught the merge something on the way past* (C0).
    ///
    /// A checkpoint is understood before the gapless test, so an op that
    /// parks on its own sequence number may still have moved another
    /// author's — or its own — watermark over a compacted stretch. The
    /// fixpoint loop's "did anything happen" flag has to count that, or a
    /// batch whose *first* present op sits above a pruned run makes no
    /// progress on the first pass, the loop concludes it is stuck, and the
    /// whole batch is dropped. That is the exact shape of a joiner reading
    /// a log whose author compacted its earliest ops.
    ///
    /// Bounded: a checkpoint only reports this the first time its ranges
    /// are new, so it cannot spin the loop.
    ParkedLearned,
    /// Author unknown *yet*: may become valid once other authors' ops land
    /// (e.g. the MemberAdd that introduces this author). Parked and retried.
    Parked,
}

/// Outcome of validating a user-signed governance body (MASTERPLAN §7).
#[derive(Debug, PartialEq, Eq)]
enum SignerCheck {
    Authorized,
    /// Signer not known yet — retry once more state lands.
    Park,
    /// Cryptographically or policy-invalid — never applies.
    Reject(&'static str),
}

/// Is this user known, active, and in role at the op's HLC? The role half
/// of [`check_user_signed`].
fn check_user_role(
    state: &MeshState,
    founder: &[u8; 32],
    user: &UserId,
    allowed: &[Role],
    at_micros: u64,
) -> SignerCheck {
    if user.0 != *founder && !state.users.contains_key(user) {
        return SignerCheck::Park;
    }
    if !rbac::is_user_active(state, founder, user) {
        return SignerCheck::Reject("user is revoked");
    }
    if !rbac::has_role(state, founder, user, allowed, at_micros) {
        return SignerCheck::Reject("user lacks the required role");
    }
    SignerCheck::Authorized
}

/// Whether an author may publish facts about a subject (§4.4).
enum FactAuthority {
    Allowed,
    /// The subject is not known here *yet* — a member record may simply not
    /// have arrived.
    Park,
    Reject(&'static str),
}

/// `MemberUpdate`'s rule: a node speaks for itself, and for nothing else. There is no founder exception here — unlike `MemberUpdate`, where
/// the founder may repair a member record at bootstrap, nobody needs to
/// author another machine's facts, and an exception would be a way to lie
/// about a fleet.
fn may_publish_facts(
    state: &MeshState,
    author: &NodeId,
    subject: &mesh_proto::FactSubject,
) -> FactAuthority {
    match subject {
        mesh_proto::FactSubject::Node(node) => {
            if node != author {
                return FactAuthority::Reject("only a node may publish its own facts");
            }
            if state.revoked.contains_key(node) {
                return FactAuthority::Reject("author is revoked");
            }
            match state.members.get(node) {
                Some(_) => FactAuthority::Allowed,
                // The `MemberAdd` may not have arrived yet.
                None => FactAuthority::Park,
            }
        }
        // The host inventory was removed, so there is no host a node could
        // be a vantage point for. Rejected rather than parked: no record
        // will ever arrive to make one of these valid.
        mesh_proto::FactSubject::Host(_) => FactAuthority::Reject("the host inventory was removed"),
    }
}

/// Settle one summary against an executor that is gone for good (ENG-16).
///
/// A job is reported only by the node that runs it — that rule is what
/// makes a summary worth believing. Its cost is that a node which is
/// revoked, reimaged or simply gone leaves every job it held reading
/// `running` for the life of the mesh: no other node may write the record,
/// `jobs clear` will not take it (the watermark only forgets jobs that
/// *finished*), and a fan-out waiting on one of them never completes. The
/// only cure was an admin force-cancel, one job at a time, by hand.
///
/// **Derived, not authored.** Nothing signs this and no op carries it: "the
/// executor has left or been revoked" is already replicated, already
/// converged, and already carries an instant of its own. Every node
/// computes the same settlement from the same state, so there is no
/// authority question, no new wire type, and nothing to park on.
///
/// `at` is that instant — the revocation's HLC, or the max of the
/// `MemberLeft` ops — and never a local clock, which would give every node
/// a different `finished_at_micros` for the same job.
///
/// **`Failed`, not a new state.** `JobState` crosses the wire inside every
/// `JobSummary`; a variant an older node cannot decode would break the op
/// it travels in rather than the field it sits in. `sweep_stranded` already
/// closes its own version of this case as failed, for the same reason and
/// with the same meaning: the work held a slot on a node that died under
/// it.
fn settle_lost(summary: &mut JobSummary, at: Hlc) -> bool {
    // The executor's own word wins if it managed to get one out: a node
    // that reported success and *then* left really did succeed.
    if summary.state.is_terminal() || summary.updated_hlc >= at {
        return false;
    }
    summary.state = JobState::Failed;
    summary.finished_at_micros = Some(at.wall_micros);
    summary.updated_hlc = at;
    true
}

/// Apply a forced cancel to one summary, if it still outranks what the
/// executor has said. Returns whether the summary changed.
///
/// The override loses to any summary write with a later HLC — an executor
/// that was merely unreachable and eventually reports the real outcome
/// wins, because a record of what actually happened beats a record of
/// what somebody demanded (see `OpBody::JobCancelForce`).
fn apply_force_cancel(summary: &mut JobSummary, at: Hlc) -> bool {
    if summary.state.is_terminal() || summary.updated_hlc >= at {
        return false;
    }
    summary.state = JobState::Cancelled;
    summary.finished_at_micros = Some(at.wall_micros);
    summary.updated_hlc = at;
    true
}

/// Verify an embedded user signature and the signer's role at the op's HLC.
fn check_user_signed(
    state: &MeshState,
    founder: &[u8; 32],
    signer: &UserId,
    allowed: &[Role],
    at_micros: u64,
    msg: &[u8],
    sig: &[u8],
) -> SignerCheck {
    let Ok(sig64) = <[u8; 64]>::try_from(sig) else {
        return SignerCheck::Reject("malformed user signature");
    };
    if !verify_signature(&signer.0, msg, &sig64) {
        return SignerCheck::Reject("invalid user signature");
    }
    check_user_role(state, founder, signer, allowed, at_micros)
}

/// Validate a Git-source-authorized service version (MASTERPLAN §8.1): a
/// version with an *empty* signature is legitimate only when it names a
/// source whose user-signed record pre-authorizes bumping this service, and
/// that user is still active. This is the delegation that lets a watcher
/// node bump services without holding a user key — meshes that cannot
/// accept node-driven bumps leave the source's `bump` list empty.
fn check_source_bump(
    state: &MeshState,
    founder: &[u8; 32],
    version: &ServiceVersion,
) -> SignerCheck {
    let Some(src_name) = &version.source else {
        return SignerCheck::Reject("unsigned service version names no source");
    };
    let Some(entry) = state.sources.get(src_name) else {
        return SignerCheck::Park; // the SourceAdd may not have arrived yet
    };
    if entry.removed {
        return SignerCheck::Reject("source has been removed");
    }
    if entry.source.created_by != version.created_by {
        return SignerCheck::Reject("version creator does not match the source creator");
    }
    if !entry.source.bump.iter().any(|s| s == &version.name) {
        return SignerCheck::Reject("source does not authorize bumping this service");
    }
    if !rbac::is_user_active(state, founder, &version.created_by) {
        return SignerCheck::Reject("source creator is revoked");
    }
    SignerCheck::Authorized
}

/// Record an applied op's watermark, plus the per-author cut point a
/// pre-1.7 peer stops at (F0 §3.3). One place, so the two can never
/// disagree about what has been applied.
fn note_applied(state: &mut MeshState, op: &OpEnvelope) {
    // Step over any run of this author's vouched-for holes that now sits
    // immediately ahead of the mark, so a peer that filled the last op
    // before a compacted stretch does not park on the stretch itself.
    let mark = match state.pruned.get(&op.author) {
        Some(pruned) => pruned.advance(op.seq),
        None => op.seq,
    };
    state.applied.insert(op.author, mark);
    state
        .author_clock
        .entry(op.author)
        .and_modify(|at| *at = (*at).max(op.hlc))
        .or_insert(op.hlc);
    if !op.v1_sendable() {
        state
            .legacy_cut
            .entry(op.author)
            .and_modify(|seq| *seq = (*seq).min(op.seq))
            .or_insert(op.seq);
    }
}

pub struct StateEngine {
    store: OpStore,
    state: RwLock<MeshState>,
    /// Ops whose merge panicked and was contained (SEC-22).
    contained_panics: std::sync::atomic::AtomicU64,
    /// How many audit rows stay in memory ([`AUDIT_RESIDENT`]; tests lower
    /// it).
    audit_resident: std::sync::atomic::AtomicUsize,
    /// How far each peer's log has been pruned here, by the checkpoint seq
    /// last acted on (ENG-3). Memory only: a restart prunes again from the
    /// newest checkpoint, which is idempotent.
    peer_pruned: Mutex<BTreeMap<NodeId, u64>>,
    clock: HlcClock,
    node_key: Keypair,
    genesis: Genesis,
    /// Runs and jobs touched since the history indexer last drained
    /// (M14 §2). Bounded by *churn*, not by history: it holds the ids of
    /// things in flight, never the ids of things at rest.
    dirty: Mutex<Dirty>,
    /// Guards author_op so (seq assignment + persist + apply) is atomic.
    author_lock: Mutex<()>,
    /// Authors whose ops arrived dated past this machine's allowance, how
    /// far ahead the latest was, and when it was seen (SEC-5). Local and
    /// ephemeral: it is `doctor`'s answer to "why is that node's work not
    /// showing up here".
    ahead: Mutex<BTreeMap<NodeId, (u64, std::time::Instant)>>,
    /// Folded audit rows (SEC-6, `crate::audit`).
    audit_denials: crate::audit::Denials,
    /// Why the last attempt to author an op failed, until one succeeds
    /// (SEC-6). A refusal by the rate limiter is not a failure to write and
    /// does not count.
    write_failure: Mutex<Option<String>>,
    /// Why the op-log last refused a write, until it has been reopened and
    /// takes writes again (ENG-4). Narrower than `write_failure`: only the
    /// store itself, never a refusal to author.
    store_failure: Mutex<Option<String>>,
    /// This node's own ops that were written, did not apply, and could not
    /// be taken back off the disk (ENG-4). Removed first when the store
    /// reopens, before any new op of this node's can reuse their numbers.
    untaken: Mutex<Vec<mesh_proto::SeqRange>>,
    /// Set while this node, restored from a backup or rebuilt, has not yet
    /// been shown its own history by a peer (ENG-13). Persisted beside the
    /// store, so a restart does not forget it.
    restore_hold: Mutex<Option<RestoreHold>>,
    hold_path: std::path::PathBuf,
    /// Peers' ops that were applied here and then could not be written
    /// (ENG-4). The version vector already claims them, so nothing would
    /// fetch them again; they are written first when the store reopens.
    unwritten: Mutex<Vec<OpEnvelope>>,
    /// A failure every write reports while set (tests: a full disk).
    injected_failure: Mutex<Option<String>>,
    /// Ops the most recent apply batch could not take, and the batch's
    /// size (ENG-21).
    ///
    /// **A gauge, deliberately, and not a counter.** A parked op is not
    /// consumed: it is dropped and fetched again on the next sync, so a
    /// counter incremented per parking would climb with the *retry rate*
    /// and read as a flood when one op is stuck. What an operator needs to
    /// see is the standing backlog — "eleven ops have been waiting for
    /// authorization context for an hour" — and that is what this is.
    parked_last_batch: std::sync::atomic::AtomicU64,
    /// Per-entity op-rate limiter (M31 A3).
    ///
    /// **On the engine rather than on the daemon**, because the engine is
    /// where an op actually becomes an op: every path that grows this
    /// node's log goes through `author_op`, and a limiter anywhere higher
    /// would govern the paths somebody remembered to route through it.
    rate: crate::backpressure::RateGuard,
    /// One monotonic counter per [`StateTopic`] (**H-8**).
    ///
    /// **The engine's answer to "has anything happened?"** — the question
    /// it could not previously be asked, which is why every client that
    /// wanted to know polled for the full answer instead and paid for it
    /// whether or not anything had moved.
    ///
    /// Bumped in [`Self::try_apply`], which is the single funnel both
    /// locally authored and replicated ops go through, so there is no
    /// second path that could change state without saying so. Atomics
    /// rather than a lock because the one reader is a status request that
    /// must never queue behind replication — the whole point is that
    /// asking is cheaper than not asking.
    ///
    /// Node-local and not replicated: it counts what *this* node has
    /// applied, which is exactly what a client attached to this node
    /// should be refetching against.
    ///
    /// `watch` channels rather than plain atomics so the signal can also be
    /// *awaited* (**M-13**). The workflow driver woke once a second per
    /// active run to re-evaluate a graph that had usually not changed;
    /// what it was really waiting for was a job reaching a terminal state,
    /// which is a `Jobs` bump. A `watch` gives that to it without a second
    /// notification mechanism, and without the missed-edge problem a
    /// `Notify` would have.
    topics: Vec<watch::Sender<u64>>,
}

/// A node that must write nothing of its own until a peer has shown it its
/// own history (ENG-13).
///
/// A restored node holds the op-log of the day its backup was taken. Peers
/// hold everything it wrote after that, so the next sequence number it
/// would pick is one they already have — and they drop the new op as a
/// duplicate, silently and for ever. Held, it fetches its own later ops
/// like anyone else's and picks up after the last of them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreHold {
    /// `restore` or `rebuild`.
    pub reason: String,
    pub since_micros: u64,
}

impl RestoreHold {
    const FILE: &str = "restore-hold.json";

    pub fn path(data_dir: &std::path::Path) -> std::path::PathBuf {
        data_dir.join("state").join(Self::FILE)
    }

    fn load(data_dir: &std::path::Path) -> Option<RestoreHold> {
        serde_json::from_slice(&std::fs::read(Self::path(data_dir)).ok()?).ok()
    }

    /// Hold `data_dir`'s node until it has caught up with a peer.
    pub fn place(data_dir: &std::path::Path, reason: &str) -> Result<()> {
        let hold = RestoreHold {
            reason: reason.to_string(),
            since_micros: mesh_proto::now_micros(),
        };
        let path = Self::path(data_dir);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(&hold)
            .map_err(|e| CoreError::State(format!("encode restore hold: {e}")))?;
        std::fs::write(&path, bytes)?;
        std::fs::File::open(&path)?.sync_all()?;
        Ok(())
    }

    /// "restored from a backup" / "rebuilt from a peer".
    pub fn what(&self) -> &'static str {
        match self.reason.as_str() {
            "rebuild" => "rebuilt from a peer",
            _ => "restored from a backup",
        }
    }
}

/// How hard an ingest should try to get its ops onto the disk (**H-3**).
///
/// The distinction is **provenance, not importance**: see
/// [`crate::store::OpStore::append_relaxed`] for why an op this node
/// authored can never be written lazily, however trivial its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Persist {
    /// Replay at boot: the ops are this node's log, read back.
    Replay,
    /// This node's own ops, which `commit_own` wrote durably before
    /// applying them (ENG-4). Nothing to write, and a refusal is a refusal.
    Authored,
    /// Nothing to write: the fuzzer's.
    Held,
    /// A peer's. Written now, flushed on the next `Immediate` commit.
    Relaxed,
}

/// A topic's slot in [`StateEngine::topics`].
fn topic_index(topic: mesh_proto::StateTopic) -> usize {
    mesh_proto::StateTopic::ALL
        .iter()
        .position(|t| *t == topic)
        .unwrap_or(0)
}

/// What changed since the last drain.
#[derive(Debug, Default)]
pub struct Dirty {
    pub runs: BTreeSet<RunId>,
    pub jobs: BTreeSet<JobId>,
    /// `(subject label, collector)` pairs whose latest document changed
    /// (M18 §4.8).
    pub facts: BTreeSet<(String, String)>,
    /// Set when the change set was dropped for being too large to be worth
    /// carrying — the indexer answers by walking everything once. A backstop
    /// for an indexer that stalled, not a path anything normally takes.
    pub overflowed: bool,
    /// Audit rows applied since the last drain, as `(author, seq)` in the
    /// op-log (ENG-3).
    pub audit: Vec<(NodeId, u64)>,
    /// The audit half of `overflowed`: the indexer rescans the op-log.
    pub audit_overflowed: bool,
}

impl Dirty {
    pub fn is_empty(&self) -> bool {
        !self.overflowed
            && !self.audit_overflowed
            && self.runs.is_empty()
            && self.jobs.is_empty()
            && self.facts.is_empty()
            && self.audit.is_empty()
    }
}

/// Audit rows kept in memory (ENG-3). Every node used to hold every row it
/// had ever seen — about 450 bytes each, for ever. The newest fifty
/// thousand cover every in-process reader (they look at the last few
/// hundred) and `meshnode audit` on a busy day; older rows are answered
/// from the history index.
pub const AUDIT_RESIDENT: usize = 50_000;

/// Ids held before the change set gives up and asks for a full pass. At
/// ~48 bytes an id this is a couple of megabytes — small next to the cost
/// of the rescan it prevents, and small enough that the backstop itself
/// can never be the memory problem.
const DIRTY_MAX: usize = 50_000;

/// How many bytes of checkpoints one sync batch may carry ahead of the ops
/// it is explaining (C0 P2).
///
/// A quarter of the batch's own byte budget, which leaves the ops three
/// quarters and keeps the whole frame inside `MAX_FRAME_LEN`. It only binds
/// on a mesh where many authors have compacted *and* one peer is behind on
/// all of them at once — a node returning from a long outage — and there it
/// costs an extra round trip rather than an undeliverable frame.
const CHECKPOINT_BUDGET_BYTES: usize = crate::store::SYNC_BATCH_BYTES / 4;

impl std::fmt::Debug for StateEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StateEngine(node={})", self.node_key.node_id().short())
    }
}

impl StateEngine {
    /// Open the store and replay the full op-log.
    pub fn open(
        data_dir: &std::path::Path,
        node_key: Keypair,
        genesis: Genesis,
    ) -> Result<Arc<Self>> {
        let store = OpStore::open(data_dir)?;
        let engine = Arc::new(Self {
            store,
            state: RwLock::new(MeshState::default()),
            clock: HlcClock::new(),
            node_key,
            genesis,
            dirty: Mutex::new(Dirty::default()),
            // **Off until a daemon turns it on.** `StateEngine::open` is
            // also how offline tools hold a data dir — `restore`, a backup,
            // a repair — and a tool replaying a log must never be rate
            // limited by the config of whichever node's directory it
            // happens to be pointed at. `Node::start` applies
            // `[state] op_rate_per_minute`; nothing else does.
            rate: crate::backpressure::RateGuard::new(0),
            parked_last_batch: std::sync::atomic::AtomicU64::new(0),
            topics: mesh_proto::StateTopic::ALL
                .iter()
                .map(|_| watch::channel(0u64).0)
                .collect(),
            ahead: Mutex::new(BTreeMap::new()),
            audit_denials: crate::audit::Denials::default(),
            write_failure: Mutex::new(None),
            store_failure: Mutex::new(None),
            untaken: Mutex::new(Vec::new()),
            restore_hold: Mutex::new(RestoreHold::load(data_dir)),
            hold_path: RestoreHold::path(data_dir),
            unwritten: Mutex::new(Vec::new()),
            injected_failure: Mutex::new(None),
            contained_panics: std::sync::atomic::AtomicU64::new(0),
            audit_resident: std::sync::atomic::AtomicUsize::new(AUDIT_RESIDENT),
            peer_pruned: Mutex::new(BTreeMap::new()),
            // author_lock must come last to satisfy field order
            author_lock: Mutex::new(()),
        });
        let ops = engine.store.load_all()?;
        engine.ingest_internal(ops, Persist::Replay)?;
        Ok(engine)
    }

    pub fn clock(&self) -> &HlcClock {
        &self.clock
    }

    pub fn audit_denials(&self) -> &crate::audit::Denials {
        &self.audit_denials
    }

    /// Why this node could not write its last op, if it could not (SEC-6),
    /// or why its op-log cannot be written at all (ENG-4). Audit-worthy
    /// actions are refused while this is set.
    pub fn write_failure(&self) -> Option<String> {
        self.write_failure
            .lock()
            .clone()
            .or_else(|| self.store_failure())
    }

    /// Why the op-log cannot be written, until it has been reopened and
    /// takes writes again (ENG-4).
    pub fn store_failure(&self) -> Option<String> {
        self.store_failure.lock().clone()
    }

    /// Why this node is writing nothing of its own yet, if it is (ENG-13).
    pub fn restore_hold(&self) -> Option<RestoreHold> {
        self.restore_hold.lock().clone()
    }

    /// Has a peer whose version vector is `theirs` shown this node every op
    /// of its own that it holds?
    pub fn holds_own_history_of(&self, theirs: &VersionVector) -> bool {
        let me = self.node_id();
        let ours = self.read(|s| s.applied.get(&me).copied().unwrap_or(0));
        let peer = theirs
            .iter()
            .find(|(a, _)| *a == me)
            .map_or(0, |(_, seq)| *seq);
        ours >= peer
    }

    /// Lift the restore hold: this node may write again.
    pub fn release_restore_hold(&self, why: &str) -> Result<()> {
        let mut hold = self.restore_hold.lock();
        let Some(was) = hold.take() else {
            return Ok(());
        };
        if let Err(e) = std::fs::remove_file(&self.hold_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            *hold = Some(was);
            return Err(e.into());
        }
        let me = self.node_id();
        tracing::info!(
            own_seq = self.read(|s| s.applied.get(&me).copied().unwrap_or(0)),
            "this node was {} and has caught up ({why}); writing again",
            was.what()
        );
        Ok(())
    }

    fn note_store_failure(&self, e: &CoreError) {
        if matches!(e, CoreError::Store(_) | CoreError::Io(_)) {
            *self.store_failure.lock() = Some(e.to_string());
        }
    }

    /// The oldest audit id the in-memory tail holds, once the tail has
    /// been trimmed — the point older rows are read from — or `None` while
    /// it still holds every row (ENG-3).
    pub fn audit_tail_start(&self) -> Option<mesh_proto::AuditId> {
        let resident = self
            .audit_resident
            .load(std::sync::atomic::Ordering::Relaxed);
        self.read(|s| {
            (s.audit.len() >= resident)
                .then(|| s.audit.keys().next().copied())
                .flatten()
        })
    }

    /// Keep only `n` audit rows in memory (tests; [`AUDIT_RESIDENT`]).
    pub fn set_audit_resident_for_test(&self, n: usize) {
        self.audit_resident
            .store(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// Every audit row in the op-log, as the signed ops that carry them
    /// (ENG-3): what the history index is rebuilt from. Bodies are peeked
    /// before they are decoded, so the rest of the log costs a varint each.
    pub fn audit_ops(&self) -> Result<Vec<OpEnvelope>> {
        // Filtered as it goes, so the whole log is never resident at once
        // (ENG-20) — only the audit ops, which is what the caller wanted.
        let mut out = Vec::new();
        self.store.for_each_op(|op| {
            if op.body_index() == Some(mesh_proto::OPBODY_AUDITAPPEND_INDEX) {
                out.push(op);
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// One op, by where it sits in the log.
    pub fn op_at(&self, author: &NodeId, seq: u64) -> Result<Option<OpEnvelope>> {
        self.store.get(author, seq)
    }

    /// Ops this node holds by one author — a test seam for ENG-3's peer
    /// pruning.
    pub fn stored_ops_by(&self, author: &NodeId) -> Result<u64> {
        self.store.count_for(author)
    }

    /// How many times the op-log file has been rewritten to shrink it — a
    /// test seam for ENG-5.
    #[doc(hidden)]
    pub fn store_reclaims(&self) -> u64 {
        self.store.reclaims()
    }

    /// Make every write fail with `why`, as a full disk would, until called
    /// again with `None` (tests).
    pub fn inject_write_failure_for_test(&self, why: Option<&str>) {
        *self.injected_failure.lock() = why.map(str::to_string);
    }

    fn injected_failure(&self) -> Result<()> {
        match self.injected_failure.lock().clone() {
            Some(why) => Err(CoreError::Store(why)),
            None => Ok(()),
        }
    }

    fn note_write<T>(&self, result: &Result<T>) {
        let mut failure = self.write_failure.lock();
        match result {
            Ok(_) => *failure = None,
            Err(CoreError::Backpressure { .. }) => {}
            Err(e) => *failure = Some(e.to_string()),
        }
    }

    fn note_ahead(&self, author: NodeId, by_micros: u64) {
        let mut ahead = self.ahead.lock();
        if !ahead.contains_key(&author) {
            warn!(
                author = %author.short(),
                ahead_secs = by_micros / 1_000_000,
                "ops dated ahead of this machine's clock are held until it catches up"
            );
        }
        ahead.insert(author, (by_micros, std::time::Instant::now()));
    }

    /// Nodes whose ops arrived dated more than
    /// [`crate::hlc::MAX_AHEAD_MICROS`] ahead of this machine within the
    /// last `within`, and by how much (SEC-5). Either their clock is wrong
    /// or this one is; `doctor` says which to check.
    pub fn clock_ahead(&self, within: std::time::Duration) -> Vec<(NodeId, u64)> {
        let mut ahead = self.ahead.lock();
        ahead.retain(|_, (_, seen)| seen.elapsed() <= within);
        ahead.iter().map(|(id, (by, _))| (*id, *by)).collect()
    }

    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    pub fn node_id(&self) -> NodeId {
        self.node_key.node_id()
    }

    /// This node's change-signal counters, one per [`StateTopic`]
    /// (**H-8**).
    ///
    /// Monotonic and node-local. A client remembers the vector it last
    /// saw, asks for it again — one small request, no collection built,
    /// no state lock taken — and refetches only the topics whose counter
    /// moved. On an idle mesh nothing moves and the client does no work at
    /// all, which is the case the polling design got most wrong.
    pub fn topic_versions(&self) -> mesh_proto::StateVersions {
        mesh_proto::StateVersions {
            counters: self.topics.iter().map(|c| *c.borrow()).collect(),
        }
    }

    /// Await changes to one topic (**M-13**).
    ///
    /// The push half of the same signal [`Self::topic_versions`] polls. A
    /// receiver never misses an edge — it carries the latest value, so a
    /// caller that was busy when the bump happened sees it on its next
    /// `changed()` rather than sleeping through it.
    pub fn watch_topic(&self, topic: mesh_proto::StateTopic) -> watch::Receiver<u64> {
        let idx = topic_index(topic);
        self.topics[idx].subscribe()
    }

    /// Note that a topic changed. Called from `try_apply` after an op has
    /// actually been applied — never before, so a parked or ignored op
    /// cannot make a client refetch for nothing.
    fn bump_topic(&self, topic: mesh_proto::StateTopic) {
        self.topics[topic_index(topic)].send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Note that a topic changed for a reason **no op records**.
    ///
    /// The change signal was built as a fold over [`mesh_proto::OpBody`],
    /// on the reasoning that a client should refetch a collection exactly
    /// when something authored a change to it. That is true of every
    /// collection whose rows are op-derived, and false of the one that is
    /// not: `NodeRow` carries liveness, sync lag and legacy lag, which are
    /// ephemeral gossip state deliberately kept out of the op-log
    /// (`crate::gossip`, MASTERPLAN §4.3). Nothing authored an op when a
    /// peer went suspect, so the `Nodes` counter never moved, so Director
    /// never refetched — and every node card sat frozen on whatever
    /// liveness happened to hold at the last membership op, which on a
    /// mesh that had finished joining was for ever. The mesh reported
    /// itself healthy in the same header, from the same `liveness_of`, one
    /// poll apart.
    ///
    /// `status`, `top` and `totals` were exempted from the signal for this
    /// exact reason and say so; the node table was not, and needed to be
    /// told rather than exempted, because it is mostly op-derived and
    /// refetching it every tick is what **H-8** exists to avoid.
    ///
    /// Callers must bump only on an **observed transition**, never once per
    /// tick per peer: a counter that moves every second is a counter that
    /// has stopped saying anything.
    pub fn note_ephemeral_change(&self, topic: mesh_proto::StateTopic) {
        self.bump_topic(topic);
    }

    /// Note an audit row for the history indexer, by where it sits in the
    /// op-log: the indexer reads the signed op back rather than this
    /// holding a copy (ENG-3).
    fn mark_audit(&self, author: NodeId, seq: u64) {
        let mut dirty = self.dirty.lock();
        if dirty.audit.len() >= DIRTY_MAX {
            dirty.audit.clear();
            dirty.audit_overflowed = true;
            return;
        }
        dirty.audit.push((author, seq));
    }

    /// Settle every job still credited to `gone` that can no longer be
    /// reported, returning the ones that changed (ENG-16).
    ///
    /// Called from the two apply arms that make a node unable to speak —
    /// `MemberRevoke` and `MemberLeft` — and from `JobUpsert`, because ops
    /// replay per author and an executor's summary is in a different log
    /// from the op that retired it. Whichever lands second has to produce
    /// the same state, which is the same rule the clear watermark and the
    /// force-cancel map already follow.
    fn settle_jobs_lost(&self, state: &mut MeshState, gone: &NodeId) -> Vec<JobId> {
        let Some(at) = state.gone_at(gone) else {
            return Vec::new();
        };
        let mut settled = Vec::new();
        for (id, summary) in state.jobs.iter_mut() {
            if summary.executor == *gone && settle_lost(summary, at) {
                settled.push(*id);
            }
        }
        // A settled job is terminal, so a standing clear can now forget it —
        // the same follow-up `JobUpsert` and `JobCancelForce` both do.
        let mark = state.jobs_cleared_before;
        state.jobs.retain(|_, j| !j.cleared_by(mark));
        settled
    }

    fn mark_job(&self, job: JobId) {
        let mut dirty = self.dirty.lock();
        if dirty.jobs.len() >= DIRTY_MAX {
            dirty.runs.clear();
            dirty.jobs.clear();
            dirty.overflowed = true;
            return;
        }
        dirty.jobs.insert(job);
    }

    /// Note that a subject's facts changed, for the history indexer.
    fn mark_fact(&self, subject: String, collector: String) {
        let mut dirty = self.dirty.lock();
        if dirty.facts.len() >= DIRTY_MAX {
            dirty.runs.clear();
            dirty.jobs.clear();
            dirty.facts.clear();
            dirty.overflowed = true;
            return;
        }
        dirty.facts.insert((subject, collector));
    }

    /// Take everything that has changed since the last call. The indexer is
    /// the only caller; taking rather than peeking means a change is handed
    /// out once, and a failed index pass puts it back.
    pub fn drain_dirty(&self) -> Dirty {
        std::mem::take(&mut *self.dirty.lock())
    }

    /// Hand un-indexed work back after a failed pass, so nothing is lost to
    /// a transient write error.
    pub fn requeue_dirty(&self, back: Dirty) {
        let mut dirty = self.dirty.lock();
        if back.audit_overflowed || dirty.audit_overflowed {
            dirty.audit.clear();
            dirty.audit_overflowed = true;
        } else {
            dirty.audit.extend(back.audit);
        }
        if back.overflowed || dirty.overflowed {
            dirty.runs.clear();
            dirty.jobs.clear();
            dirty.facts.clear();
            dirty.overflowed = true;
            return;
        }
        dirty.runs.extend(back.runs);
        dirty.jobs.extend(back.jobs);
        dirty.facts.extend(back.facts);
    }

    /// Every fact observation in this node's log, oldest first (M18 §4.8).
    ///
    /// The op-log **is** the fact history — every `FactsPublish` stays in it
    /// forever, signed by the node that observed it — so rebuilding the
    /// derived index is a walk of the log rather than an approximation from
    /// current state. That is what makes `history.redb` genuinely
    /// disposable here: delete it and `facts diff` comes back complete,
    /// not truncated to whatever the node happens to believe now.
    ///
    /// Called once at boot; incremental updates come through the change
    /// set. A full walk is affordable because the whole point of §4.2 is
    /// that these ops are rare.
    pub fn fact_observations(&self) -> Result<Vec<(String, String, u64, String)>> {
        let mut out = Vec::new();
        // **Peek before decoding** (**H-4**). This walks the whole log, and
        // a `FactsPublish` is a small minority of it — decoding every other
        // op's body just to discard it was most of the cost of the pass.
        //
        // **And streamed, not collected** (ENG-20). `load_all` materialised
        // the entire log as `OpEnvelope`s first, so the peek saved the
        // decode and paid for the allocation anyway — a second full copy of
        // the log in memory at boot, on top of the one replay had just
        // finished with. What survives the walk now is the fact ops, which
        // is what the caller asked for.
        self.store.for_each_op(|envelope| {
            if envelope.body_index() != Some(mesh_proto::OPBODY_FACTSPUBLISH_INDEX) {
                return Ok(());
            }
            let Some(OpBody::FactsPublish {
                subject,
                collector,
                doc,
                collected_at_micros,
                ..
            }) = envelope.decode_body()
            else {
                return Ok(());
            };
            let Ok(text) = String::from_utf8(doc) else {
                return Ok(());
            };
            out.push((subject.label(), collector, collected_at_micros, text));
            Ok(())
        })?;
        out.sort_by_key(|(_, _, when, _)| *when);
        Ok(out)
    }

    /// Release the plans of runs that are closed, cold, and beyond the
    /// resident budget. Returns how many were released.
    ///
    /// Three conditions, each load-bearing:
    ///
    /// - **Closed.** An open run is being driven and its plan is in use.
    /// - **Cold.** `min_age` past the finish, so a `runs show` or a retry
    ///   right after a run ends never pays for a lookup.
    /// - **Beyond the budget.** The newest `keep` closed runs stay resident
    ///   whatever their age, because those are the ones people look at.
    ///
    /// `keep == 0` means never shed.
    pub fn shed_cold_runs(&self, keep: usize, min_age_micros: u64) -> usize {
        if keep == 0 {
            return 0;
        }
        let now = mesh_proto::now_micros();

        // **A read pass before the write lock** (**M-7**). This ran every
        // two seconds and took the *write* lock — the one that blocks every
        // reader and every arriving op — to walk the whole run map and,
        // almost always, discover the budget was not exceeded and return 0.
        // The early return was after the walk, under the write lock.
        //
        // Counting resident candidates under the read lock first makes the
        // common case cost a read-lock walk and nothing else; the write
        // lock is taken only when something is actually going to be shed.
        let resident = self.read(|s| {
            s.runs
                .values()
                .filter(|e| e.closed.is_some() && !e.is_shed() && e.plan_op.is_some())
                .count()
        });
        if resident <= keep {
            return 0;
        }

        let mut state = self.state.write();

        // Closed, resident, and old enough — newest first, so the ones that
        // survive the budget are the ones most likely to be read.
        let mut candidates: Vec<(u64, RunId)> = state
            .runs
            .iter()
            .filter(|(_, e)| e.closed.is_some() && !e.is_shed() && e.plan_op.is_some())
            .map(|(id, e)| {
                let when = e
                    .record
                    .as_ref()
                    .and_then(|r| r.finished_at_micros)
                    .unwrap_or(e.summary.created_at_micros);
                (when, *id)
            })
            .filter(|(when, _)| now.saturating_sub(*when) >= min_age_micros)
            .collect();
        if candidates.len() <= keep {
            return 0;
        }
        candidates.sort_unstable_by_key(|(when, _)| std::cmp::Reverse(*when));

        let mut shed = 0;
        for (_, id) in candidates.into_iter().skip(keep) {
            if let Some(entry) = state.runs.get_mut(&id)
                && entry.shed()
            {
                shed += 1;
            }
        }
        shed
    }

    /// Read access to materialized state.
    pub fn read<R>(&self, f: impl FnOnce(&MeshState) -> R) -> R {
        f(&self.state.read())
    }

    pub fn version_vector(&self) -> VersionVector {
        self.read(|s| s.applied.iter().map(|(a, q)| (*a, *q)).collect())
    }

    /// Author, sign, persist, and apply a new op from this node.
    /// The op-rate limiter, at the one place an op becomes an op (M31 A3).
    ///
    /// **The entity is the op's own compaction key.** That is not a
    /// convenience: the compaction key is already the log's answer to
    /// *"what is this op about"*, it is already an exhaustive match nobody
    /// can add an entity kind without touching, and reusing it means the
    /// limiter and the compactor can never disagree about what one entity
    /// is. An op with no key — `Grow`, the audit trail and its kin — is
    /// budgeted by *kind* instead, which is coarser and correct: those are
    /// exactly the ops that never compact, so a flood of them is permanent.
    ///
    /// One exemption, and it is narrow: ops that keep the mesh *working*.
    /// Refusing a `MemberAdd` during a join, or a compaction checkpoint,
    /// would turn a growth problem into an availability problem — the
    /// control becoming the outage. They are rare by construction, so
    /// exempting them costs nothing and protects the case where the
    /// limiter is wrong.
    fn admit_or_refuse(&self, body: &OpBody) -> Result<()> {
        // `author_op` is hot, so the two free checks come first: the
        // limiter being off costs an atomic read, and the exemption list
        // costs a string compare against a `&'static str`. Everything
        // below takes the state lock.
        if !self.rate.is_on() || is_keepalive_op(body) || UNLIMITED_KINDS.contains(&body.kind()) {
            return Ok(());
        }
        // Classified **inside** the read rather than against a cloned
        // policy: `compaction` borrows it, and cloning a `RetentionPolicy`
        // per authored op would be a per-op allocation on the hottest path
        // in the engine to answer a question that never leaves this scope.
        let entity = self.read(
            |s| match crate::compact::compaction(body, &s.retention.policy) {
                crate::compact::Compaction::Grow => format!("kind:{}", body.kind()),
                crate::compact::Compaction::Lww(key)
                | crate::compact::Compaction::Decaying { key, .. }
                | crate::compact::Compaction::Closed { key, .. } => format!("{}:{}", key.0, key.1),
            },
        );
        let verdict = self.rate.admit(&entity, mesh_proto::now_micros());
        if verdict.is_yes() {
            return Ok(());
        }
        Err(CoreError::Backpressure {
            entity,
            message: verdict.message(),
        })
    }

    /// The limiter, for the surfaces that report it (`doctor`, Director).
    pub fn rate_guard(&self) -> &crate::backpressure::RateGuard {
        &self.rate
    }

    pub fn author_op(&self, body: OpBody) -> Result<OpEnvelope> {
        let result = self.author_op_inner(body);
        self.note_write(&result);
        result
    }

    fn author_op_inner(&self, body: OpBody) -> Result<OpEnvelope> {
        self.admit_or_refuse(&body)?;
        self.injected_failure()?;
        let _guard = self.author_lock.lock();
        let author = self.node_key.node_id();
        let seq = self.read(|s| s.applied.get(&author).copied().unwrap_or(0)) + 1;
        let hlc = self.clock.now();
        // The signed unit is the *encoded* body, which is also what the
        // envelope carries — so signing and framing never diverge.
        let encoded =
            postcard::to_stdvec(&body).map_err(|e| CoreError::State(format!("encode op: {e}")))?;
        let signature = self
            .node_key
            .sign(&mesh_proto::op_signing_bytes_raw(
                &author, seq, &hlc, &encoded,
            ))
            .to_vec();
        let op = OpEnvelope {
            author,
            seq,
            hlc,
            body: encoded,
            signature,
        };
        self.commit_own(std::slice::from_ref(&op))?;
        Ok(op)
    }

    /// Why this node may not author at all right now, if it may not.
    ///
    /// The two reasons the apply path refuses an op for its *author* rather
    /// than its body, asked before anything is written (ENG-4): an op that
    /// is on disk has spent its sequence number, so it must not be one the
    /// apply path is about to turn away.
    fn own_author_gate(&self) -> Result<()> {
        let me = self.node_id();
        if let Some(hold) = self.restore_hold() {
            return Err(CoreError::State(format!(
                "this node was {} and writes nothing until a peer has shown it its own history \
                 — a sequence number written now could be one its peers already hold",
                hold.what()
            )));
        }
        self.read(|s| {
            if s.revoked.contains_key(&me) {
                return Err(CoreError::State(
                    "this node has been revoked and authors nothing".into(),
                ));
            }
            if !s.is_active_member(&me) && me != self.genesis.body.founder_node {
                return Err(CoreError::State(
                    "this node is not a member of its mesh yet".into(),
                ));
            }
            Ok(())
        })
    }

    /// Put this node's own ops on disk, and only then into state (ENG-4).
    ///
    /// The order used to be the reverse: applied in memory, then written.
    /// When the write failed — a full disk — the ops stayed applied with
    /// nothing on disk: peers saw sequence numbers this node could never
    /// serve and parked behind them for ever, and a restart handed the same
    /// numbers to different ops. Written first, a failed write leaves
    /// nothing behind, and the next attempt reuses the numbers honestly.
    ///
    /// An op that is on disk has spent its number, so it is settled in state
    /// whatever the apply path makes of it — without effect, and loudly, if
    /// it did not apply, since that is a bug and not a condition.
    fn commit_own(&self, ops: &[OpEnvelope]) -> Result<()> {
        let Some(last) = ops.last().map(|op| op.seq) else {
            return Ok(());
        };
        self.own_author_gate()?;
        // A failed store stays failed until `recover_store` has reopened it
        // and settled what it owes; nothing else writes around that.
        if let Some(why) = self.store_failure() {
            return Err(CoreError::Store(why));
        }
        if let Err(e) = self.store.append(ops) {
            self.note_store_failure(&e);
            return Err(e);
        }
        let applied = self.ingest_internal(ops.to_vec(), Persist::Authored);
        let me = self.node_id();
        let upto = self.read(|s| s.applied.get(&me).copied().unwrap_or(0));
        if upto >= last {
            return applied.map(|_| ());
        }
        // What did not apply is on the disk and nowhere else: it was never
        // counted, so never served. Taken back off, its sequence numbers
        // are unspent and the refusal is what it was before the write came
        // first — nothing happened.
        let unapplied = mesh_proto::SeqRange::new(upto + 1, last);
        if let Err(e) = self.store.prune(&me, &[unapplied]) {
            self.note_store_failure(&e);
            self.untaken.lock().push(unapplied);
        }
        applied?;
        Err(CoreError::State(if ops.len() == 1 {
            "locally authored op was rejected — this is a bug".into()
        } else {
            format!(
                "{} of {} locally authored ops were rejected — this is a bug",
                last - upto,
                ops.len()
            )
        }))
    }

    /// Author many ops as one batch: one lock, one apply pass, one durable
    /// commit.
    ///
    /// [`author_op`](Self::author_op) is the right shape for a single change
    /// and the wrong one for five thousand — it persists through
    /// `store.append`, which opens and commits a redb write transaction per
    /// call. Importing a 5,000-host Ansible inventory therefore cost 5,146
    /// transactions and about eight seconds, nearly all of it waiting on
    /// fsync. Nothing about the ops changes: same consecutive per-author
    /// sequence numbers, same order, same signatures. Only the number of
    /// times the store is asked to durably land them does.
    pub fn author_ops(&self, bodies: Vec<OpBody>) -> Result<Vec<OpEnvelope>> {
        let result = self.author_ops_inner(bodies);
        self.note_write(&result);
        result
    }

    fn author_ops_inner(&self, bodies: Vec<OpBody>) -> Result<Vec<OpEnvelope>> {
        if bodies.is_empty() {
            return Ok(Vec::new());
        }
        // Checked per body, not per batch: a batch is one call and many
        // ops, and the log grows by the ops. Refused whole rather than in
        // part, because half a batch is a state nothing downstream models.
        for body in &bodies {
            self.admit_or_refuse(body)?;
        }
        self.injected_failure()?;
        let _guard = self.author_lock.lock();
        let author = self.node_key.node_id();
        let mut seq = self.read(|s| s.applied.get(&author).copied().unwrap_or(0));
        let mut ops = Vec::with_capacity(bodies.len());
        for body in bodies {
            seq += 1;
            let hlc = self.clock.now();
            let encoded = postcard::to_stdvec(&body)
                .map_err(|e| CoreError::State(format!("encode op: {e}")))?;
            let signature = self
                .node_key
                .sign(&mesh_proto::op_signing_bytes_raw(
                    &author, seq, &hlc, &encoded,
                ))
                .to_vec();
            ops.push(OpEnvelope {
                author,
                seq,
                hlc,
                body: encoded,
                signature,
            });
        }
        self.commit_own(&ops)?;
        Ok(ops)
    }

    /// Force any relaxed writes to disk (**H-3**).
    ///
    /// Peer ops are written without waiting for the disk; this is the timer
    /// that bounds how long they can sit unflushed. Cheap when there is
    /// nothing outstanding — an empty commit.
    pub fn flush(&self) -> Result<()> {
        let result = self.store.flush();
        if let Err(e) = &result {
            self.note_store_failure(e);
        }
        result
    }

    /// Ingest remote ops: verify, apply in order, persist what applied.
    /// Returns how many were newly applied.
    pub fn ingest(&self, ops: Vec<OpEnvelope>) -> Result<usize> {
        // Not while the op-log cannot be written (ENG-4): applied here and
        // not stored, a peer's ops would be claimed and never served.
        // Refused, they stay missing and are fetched once it can.
        if let Some(why) = self.store_failure() {
            return Err(CoreError::Store(format!(
                "not taking ops while this node's op-log cannot be written: {why}"
            )));
        }
        let result = self.ingest_internal(ops, Persist::Relaxed);
        // The node that could not write them is what has to be reported.
        if let Err(e) = &result {
            self.note_store_failure(e);
        }
        result
    }

    /// Make the op-log's writes fail as on a full disk (tests; see
    /// [`crate::store::OpStore::fill_disk_for_test`]).
    #[doc(hidden)]
    pub fn fill_disk_for_test(&self, full: bool) {
        self.store.fill_disk_for_test(full);
    }

    /// Whether the op-log is open — false only after a reopen failed.
    pub fn store_is_open(&self) -> bool {
        self.store.is_open()
    }

    /// Reopen the op-log after a failed write, and prove it takes writes
    /// again (ENG-4). Clears the write failure when it does.
    pub fn recover_store(&self) -> Result<()> {
        self.store.reopen()?;
        // What this node wrote and took back goes first, then what it
        // applied and never wrote: until each is settled this node's disk
        // and its memory disagree.
        let untaken = std::mem::take(&mut *self.untaken.lock());
        if let Err(e) = self.store.prune(&self.node_id(), &untaken) {
            self.untaken.lock().extend(untaken);
            return Err(e);
        }
        let unwritten = std::mem::take(&mut *self.unwritten.lock());
        if let Err(e) = self.store.append(&unwritten) {
            self.unwritten.lock().extend(unwritten);
            return Err(e);
        }
        self.store.flush()?;
        *self.store_failure.lock() = None;
        *self.write_failure.lock() = None;
        Ok(())
    }

    /// Ops the most recent apply batch could not take, because the thing
    /// that would authorize them has not arrived yet (ENG-21).
    ///
    /// Standing, not cumulative: see the field's note. Zero on a healthy
    /// node, and a number that stays up is a node that is missing
    /// membership or grant ops a peer has and it does not.
    pub fn parked_ops(&self) -> u64 {
        self.parked_last_batch
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many ops' merges have panicked and been contained since this
    /// engine opened. Anything but zero is a bug to report (SEC-22).
    pub fn contained_panics(&self) -> u64 {
        self.contained_panics
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Merge ops without writing them anywhere — the fuzzing harness's
    /// way in (SEC-22), which must not grow a store for as long as it runs.
    pub(crate) fn apply_unpersisted(&self, ops: Vec<OpEnvelope>) -> Result<usize> {
        self.ingest_internal(ops, Persist::Held)
    }

    /// Ingest ops received from a pre-1.7 peer in the V1 `Vec<Op>` shape.
    pub fn ingest_v1(&self, ops: Vec<mesh_proto::Op>) -> Result<usize> {
        self.ingest(ops.iter().map(OpEnvelope::from_op).collect())
    }

    /// Ops a peer with `their_vv` is missing.
    ///
    /// **A batch that crosses a compacted stretch carries the checkpoint
    /// that explains it (C0 P2).** Without that, a peer behind on a
    /// compacted author receives ops whose predecessors no longer exist,
    /// parks every one of them, advances nothing, and is served the
    /// identical batch on the next tick — forever, whenever the author has
    /// more than one batch of surviving ops above the hole. The checkpoint
    /// is at the top of that author's log and would otherwise be the last
    /// thing to arrive.
    ///
    /// It is prepended rather than left in sequence order because order
    /// inside a batch does not matter to the merge — `ingest` re-derives
    /// per-author order and runs to a fixpoint — while *presence* is the
    /// whole of what unsticks the peer.
    pub fn missing_for(&self, their_vv: &VersionVector) -> Result<(Vec<OpEnvelope>, bool)> {
        self.missing_for_with(&self.version_vector(), their_vv)
    }

    /// [`Self::missing_for`] against a version vector the caller already
    /// has (**M-1**).
    ///
    /// One sync request rebuilt this node's whole version vector three
    /// times — once to report lag, once inside `missing_for`, once for the
    /// response — and each rebuild walks the `applied` map and allocates a
    /// `Vec` as long as the membership.
    pub fn missing_for_with(
        &self,
        ours: &VersionVector,
        their_vv: &VersionVector,
    ) -> Result<(Vec<OpEnvelope>, bool)> {
        let (mut ops, more) = self.store.missing_for(ours, their_vv)?;

        // Which compacted authors is this peer behind on, and does the
        // batch already carry their checkpoint?
        let wanted: Vec<(NodeId, u64)> = self.read(|s| {
            s.pruned
                .iter()
                .filter(|(author, pruned)| {
                    let their_seq = their_vv
                        .iter()
                        .find(|(a, _)| a == *author)
                        .map(|(_, q)| *q)
                        .unwrap_or(0);
                    let ours = s.applied.get(*author).copied().unwrap_or(0);
                    // Only when the peer is actually behind, and only when
                    // what it is missing reaches into the compacted region.
                    their_seq < ours && pruned.ranges().iter().any(|r| r.hi > their_seq)
                })
                .filter_map(|(author, pruned)| {
                    let seq = pruned.checkpoint_seq();
                    (seq > 0).then_some((*author, seq))
                })
                .collect()
        });

        // Bounded, because a frame is. On a large mesh where many authors
        // have compacted, prepending every checkpoint unconditionally could
        // push the batch past `MAX_FRAME_LEN` and make it undeliverable —
        // the same failure the byte-bounded sync batch exists to prevent.
        // A peer that gets some of them makes progress on those authors and
        // is offered the rest next round, so truncating costs a tick and
        // never convergence — provided the requester is told to come back.
        let mut added = 0usize;
        let mut truncated = false;
        let mut more = more;
        for (author, seq) in wanted {
            if ops.iter().any(|o| o.author == author && o.seq == seq) {
                continue;
            }
            let Some(op) = self.store.get(&author, seq)? else {
                continue;
            };
            let size = postcard::to_stdvec(&op).map(|b| b.len()).unwrap_or(0);
            if added + size > CHECKPOINT_BUDGET_BYTES && added > 0 {
                truncated = true;
                break;
            }
            added += size;
            ops.insert(0, op);
        }
        if truncated {
            more = true;
        }
        Ok((ops, more))
    }

    pub fn all_ops(&self) -> Result<Vec<OpEnvelope>> {
        self.store.load_all()
    }

    /// Every stored op's encoded bytes, in order, without decoding them
    /// (ENG-20). See [`crate::store::Store::for_each_op_bytes`].
    pub fn for_each_op_bytes(&self, f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        self.store.for_each_op_bytes(f)
    }

    /// How many ops are stored — a table length, not a scan.
    pub fn op_count(&self) -> Result<u64> {
        self.store.len()
    }

    /// The op-log store's size on disk (ENG-21). A metadata read, so a
    /// scrape can ask for it without walking anything.
    pub fn store_bytes_on_disk(&self) -> u64 {
        self.store.bytes_on_disk()
    }

    /// Audit entries with the signatures that prove them (M31 B4).
    ///
    /// **Read from the op-log, not from materialized state**, and that is
    /// the whole point: `MeshState.audit` holds the projection, and the
    /// projection has no envelope. Non-repudiation lives in the signature
    /// over the op, so the only place to get it is the log.
    ///
    /// The cost is a full scan, and it is paid deliberately. `AuditAppend`
    /// is `Grow` and never compacts, so every entry is still there — which
    /// is what makes the export complete, and also what makes the scan
    /// proportional to the whole audit history. It is called by
    /// `meshware-archive ingest` on a timer, pages by instant, and is not
    /// on any interactive path. `meshnode audit` still answers from state.
    ///
    /// Oldest first, so a caller can page with the last row's instant and a
    /// crash mid-page costs a re-read rather than a gap.
    pub fn signed_audit_rows(
        &self,
        since_micros: u64,
        limit: u32,
    ) -> Result<Vec<mesh_proto::SignedAuditRow>> {
        // **Peeked before decoded, and never all at once** (ENG-20). This
        // runs on the sink timer — every 30 s on a node with an export
        // configured — and it used to `load_all` the whole op-log into a
        // `Vec<OpEnvelope>` and then `postcard::from_bytes` *every* body in
        // it to find the audit ones. On a mesh whose log is mostly job
        // records that is a full decode of the log twice a minute to
        // usually emit nothing, because a caught-up sink filters the lot
        // out on `since_micros` afterwards.
        //
        // The variant index is readable from the encoded body without
        // decoding it, so a non-audit op now costs a varint read. An
        // unknown body from a newer build has no index and is skipped,
        // exactly as F0 requires.
        let mut out: Vec<mesh_proto::SignedAuditRow> = Vec::new();
        self.store.for_each_op(|op| {
            if op.body_index() != Some(mesh_proto::OPBODY_AUDITAPPEND_INDEX) {
                return Ok(());
            }
            let Ok(OpBody::AuditAppend { entry }) = postcard::from_bytes(&op.body) else {
                return Ok(());
            };
            if let Some(row) =
                (entry.at_micros > since_micros).then(|| mesh_proto::SignedAuditRow {
                    entry,
                    author: op.author,
                    seq: op.seq,
                    hlc: op.hlc,
                    signature: op.signature,
                    // Re-framed from the envelope's own fields, which is
                    // not the same as *deriving* them later: these are the
                    // bytes this node signed, assembled from the parts it
                    // signed them over.
                    signing_bytes: mesh_proto::op_signing_bytes_raw(
                        &op.author, op.seq, &op.hlc, &op.body,
                    ),
                })
            {
                out.push(row);
            }
            Ok(())
        })?;
        out.sort_by_key(|r| (r.entry.at_micros, r.entry.audit_id.0));
        out.truncate(limit.max(1) as usize);
        Ok(out)
    }

    /// One author's surviving ops, in seq order. A test and diagnostics
    /// seam over [`crate::store::OpStore::load_author`].
    pub fn load_author_ops(&self, author: NodeId) -> Vec<OpEnvelope> {
        self.store.load_author(&author).unwrap_or_default()
    }

    // -- C0 op-log compaction (C0-C9 §4) -----------------------------------

    /// Record what a checkpoint vouches is absent, and step the author's
    /// high-water mark over any run of it that now sits immediately ahead.
    ///
    /// Called from `try_apply` with the state write lock held, before the
    /// gapless test. Everything it refuses is refused loudly enough to
    /// diagnose and quietly enough not to fill a log: a malformed or
    /// out-of-domain checkpoint is dropped and the op continues through the
    /// normal path, where it simply merges to nothing.
    fn note_checkpoint(
        &self,
        state: &mut MeshState,
        op: &OpEnvelope,
        checkpoint: &mesh_proto::Checkpoint,
    ) -> bool {
        // An author is the only authority on its own sequence numbers, and
        // the ranges in a checkpoint are always its own by construction —
        // there is no field naming another author, and there must never be.
        if state.revoked.contains_key(&op.author) {
            return false;
        }
        if !(state.is_active_member(&op.author) || op.author == self.genesis.body.founder_node) {
            // Not yet authorized; the fixpoint loop will bring the op back
            // once the author's `MemberAdd` has merged.
            return false;
        }
        if !checkpoint.is_well_formed() {
            warn!(
                author = %op.author.short(),
                seq = op.seq,
                "malformed checkpoint dropped — its holes are not vouched for"
            );
            return false;
        }
        let pruned = state.pruned.entry(op.author).or_default();
        let changed = pruned.insert_all(&checkpoint.subsumed);
        pruned.note_checkpoint_at(op.seq);
        if !changed {
            return false; // nothing new
        }
        // A pre-1.7 peer has no variant for `Checkpoint`, so the one op
        // that could explain this author's holes is withheld from it by the
        // V1 downgrade — and the ops themselves are gone. It can therefore
        // never follow this author past the first pruned seq, which is
        // precisely what `legacy_cut` already means and already reports
        // (F0 §3.3). Recording it here turns a peer that would park in
        // silence into one the operator can see is a release behind, and
        // makes `legacy_join_refusal` say so before an invite is spent.
        if let Some(first) = checkpoint.subsumed.first() {
            state
                .legacy_cut
                .entry(op.author)
                .and_modify(|seq| *seq = (*seq).min(first.lo))
                .or_insert(first.lo);
        }

        let applied = state.applied.get(&op.author).copied().unwrap_or(0);
        let advanced = state
            .pruned
            .get(&op.author)
            .map(|p| p.advance(applied))
            .unwrap_or(applied);
        if advanced > applied {
            state.applied.insert(op.author, advanced);
            debug!(
                author = %op.author.short(),
                from = applied,
                to = advanced,
                "stepped over a compacted stretch of a peer's log"
            );
        }
        true
    }

    /// What a compaction pass would drop from **this node's own** log right
    /// now, at `horizon_secs`. Pure: touches nothing.
    pub fn compaction_plan(&self, horizon_secs: u64) -> Result<crate::compact::CompactionPlan> {
        let me = self.node_id();
        let ops = self.store.load_author(&me)?;
        // This machine's clock, not the logical one: a logical clock can be
        // carried forward by what it observes, and retention measured
        // against it retired history early (SEC-5).
        let now = mesh_proto::now_micros();
        let already = self.read(|s| {
            s.pruned
                .get(&me)
                .map(|p| p.ranges().to_vec())
                .unwrap_or_default()
        });
        let retention = self.retention();
        let gate = self.export_gate();
        let groups = self.retirable_groups();
        Ok(crate::compact::plan_with(
            &ops,
            now,
            horizon_secs,
            &already,
            &retention,
            &gate,
            &groups,
        ))
    }

    /// The fan-outs and gangs whose records may now leave the log with
    /// their window: closed, never adopted (ENG-3). See
    /// [`crate::compact::RetirableGroups`].
    pub fn retirable_groups(&self) -> crate::compact::RetirableGroups {
        self.read(|s| crate::compact::RetirableGroups {
            fanouts: s
                .fanouts
                .iter()
                .filter(|(_, e)| e.closed.is_some() && e.adopters.is_empty())
                .filter_map(|(id, e)| Some((*id, e.closed_at_micros?)))
                .collect(),
            gangs: s
                .gangs
                .iter()
                .filter(|(_, e)| e.closed.is_some() && e.adopters.is_empty())
                .filter_map(|(id, e)| Some((*id, e.closed_at_micros?)))
                .collect(),
        })
    }

    /// Drop what each peer's newest applied checkpoint vouches is gone from
    /// this node's copy of that peer's log (ENG-3).
    ///
    /// Compaction was per-author and stopped there: the author pruned its
    /// own log and every other node kept every op it had ever received,
    /// and served all of it to each new joiner. A peer's checkpoint is
    /// the author's statement that those ops are dead weight, and a
    /// receiver already steps over them as if delivered, so dropping them
    /// here changes no state — it only stops every node paying to store
    /// what the author no longer does.
    ///
    /// **Only a checkpoint this node has applied.** A checkpoint learned
    /// ahead of a gap is not in this node's store yet; pruning by it would
    /// leave holes that, after a restart, nothing on disk explains. Once
    /// applied, it was written before this prune commits, so the store
    /// always holds the checkpoint that vouches for its own holes.
    ///
    /// Returns how many ops were dropped.
    pub fn prune_peer_logs(&self) -> Result<u64> {
        let me = self.node_id();
        let ready: Vec<(NodeId, u64)> = self.read(|s| {
            s.pruned
                .iter()
                .filter(|(author, _)| **author != me && !s.revoked.contains_key(*author))
                .map(|(author, set)| (*author, set.checkpoint_seq()))
                .filter(|(author, at)| {
                    *at > 0 && s.applied.get(author).is_some_and(|applied| applied >= at)
                })
                .collect()
        });
        let mut dropped = 0u64;
        for (author, at) in ready {
            if self
                .peer_pruned
                .lock()
                .get(&author)
                .is_some_and(|done| *done >= at)
            {
                continue;
            }
            let Some(op) = self.store.get(&author, at)? else {
                continue;
            };
            let Some(OpBody::Checkpoint { checkpoint }) = op.decode_body() else {
                continue;
            };
            if !checkpoint.is_well_formed() {
                continue;
            }
            dropped += self.store.prune(&author, &checkpoint.subsumed)?;
            self.peer_pruned.lock().insert(author, at);
        }
        if dropped > 0 {
            if let Err(e) = self.store.reclaim_if_worthwhile() {
                debug!(error = %e, "op-log reclaim declined; the file will catch up later");
            }
            tracing::info!(
                dropped,
                "dropped peers' compacted ops from this node's copy"
            );
        }
        Ok(dropped)
    }

    /// Compact this node's own authored log: author a checkpoint that
    /// vouches for the holes, then make them.
    ///
    /// **The order is the invariant.** The checkpoint is durable and
    /// replicating before a single byte is dropped, so the worst outcome of
    /// a crash in the middle is a log that still holds ops a checkpoint
    /// already declared gone — which every receiver, including this node on
    /// restart, treats correctly: `applied` steps over a hole that is not
    /// there yet, and the leftover ops merge to nothing when they are
    /// re-read. The reverse order would produce a hole nobody vouched for,
    /// and that parks a peer's sync on this author forever.
    ///
    /// Returns `(dropped, checkpoint_seq)`.
    pub fn compact_self(&self, horizon_secs: u64) -> Result<(u64, u64)> {
        self.compact_self_at(horizon_secs, mesh_proto::now_micros())
    }

    /// [`Self::compact_self`] as if it were `now` — a test seam, since
    /// close times come from the clock and a window cannot be waited out.
    #[doc(hidden)]
    pub fn compact_self_at(&self, horizon_secs: u64, now: u64) -> Result<(u64, u64)> {
        let me = self.node_id();
        let ops = self.store.load_author(&me)?;
        let already = self.read(|s| {
            s.pruned
                .get(&me)
                .map(|p| p.ranges().to_vec())
                .unwrap_or_default()
        });
        let retention = self.retention();
        let gate = self.export_gate();
        let groups = self.retirable_groups();
        let mut plan = crate::compact::plan_with(
            &ops,
            now,
            horizon_secs,
            &already,
            &retention,
            &gate,
            &groups,
        );

        // **Nothing new, nothing written** (ENG-5). `plan.is_empty()` is
        // false for ever once one pass has pruned anything — it counts the
        // ranges already gone — and the previous checkpoint below was
        // always "new". So every idle hour wrote a fresh, ever-larger
        // checkpoint that every peer keeps, and rewrote the file.
        //
        // What a checkpoint already vouches for is not new either: those
        // ops are only still here because a pass stopped between its
        // checkpoint and its prune, and they go without another one.
        let (vouched, fresh): (Vec<u64>, Vec<u64>) = plan
            .drop
            .iter()
            .partition(|seq| plan.already.iter().any(|r| r.contains(**seq)));
        if fresh.is_empty() {
            if vouched.is_empty() {
                return Ok((0, 0));
            }
            let dropped = self.store.prune(&me, &mesh_proto::seq_ranges(&vouched))?;
            tracing::info!(
                dropped,
                "finished pruning what the last checkpoint vouched for"
            );
            return Ok((dropped, 0));
        }

        // The previous checkpoint is fully subsumed by the one about to be
        // written — cumulative by construction — so it goes too. Without
        // this the pass leaks exactly one op per run, forever, which is the
        // bug every log-compaction implementation writes once.
        if let Some(prev) = ops
            .iter()
            .rev()
            .find(|o| matches!(o.decode_body(), Some(OpBody::Checkpoint { .. })))
            && !plan.covers(prev.seq)
        {
            plan.drop.push(prev.seq);
            plan.drop.sort_unstable();
        }

        let through = ops.last().map(|o| o.seq).unwrap_or(0);
        let ops_after = plan.ops_before.saturating_sub(plan.dropped_total()) + 1;
        let body = crate::compact::checkpoint_body(&plan, through, horizon_secs, ops_after);
        let checkpoint = self.author_op(body)?;

        let ranges = plan.ranges();
        let before_bytes = self.store.bytes_on_disk();
        let dropped = self.store.prune(&me, &ranges)?;
        // Reclaim outside the prune transaction, and never let a refusal
        // fail the pass: the holes are made and vouched for either way, and
        // the file catching up is a courtesy to `df`, not a correctness
        // property. Only when it gives back enough to be worth stopping the
        // store for (ENG-5).
        let shrank = match self.store.reclaim_if_worthwhile() {
            Ok(shrank) => shrank,
            Err(e) => {
                debug!(error = %e, "op-log reclaim declined; the file will catch up later");
                false
            }
        };
        tracing::info!(
            dropped,
            kept = ops_after,
            horizon_secs,
            checkpoint_seq = checkpoint.seq,
            before_bytes,
            after_bytes = self.store.bytes_on_disk(),
            shrank,
            "compacted this node's op-log"
        );
        Ok((dropped, checkpoint.seq))
    }

    /// Drop usage windows nobody reads any more from memory (C0 P3).
    ///
    /// Purely a local economy, and deliberately not a merge rule: the
    /// entries this removes contribute to no answer, so removing them is
    /// invisible to every reader and to every peer. A replay after restart
    /// may put some of them back — the op can still be in the log — and
    /// that is invisible for the same reason.
    ///
    /// Returns how many were released.
    pub fn expire_usage(&self, now_micros: u64) -> usize {
        let mut state = self.state.write();
        let before = state.usage.len();
        state
            .usage
            .retain(|(_, _, window), _| mesh_proto::usage_window_is_live(*window, now_micros));
        before - state.usage.len()
    }

    /// Drop closed jobs past the mesh's retention window from memory
    /// (M31 A1).
    ///
    /// The read-side half of [`Compaction::Closed`], and it exists for C9
    /// rather than for memory. Compaction removes the op; without this, the
    /// node that authored it keeps the entry in its map until it restarts,
    /// while a node that *did* restart replays a log the op is gone from —
    /// and the two answer "show me that job" differently. Sweeping both on
    /// the same replicated clock is what makes them agree.
    ///
    /// Exactly [`JobsClear`]'s predicate, on a cutoff derived from policy
    /// rather than from a button: terminal only, by finish instant, and a
    /// terminal summary carrying no finish time is kept rather than guessed
    /// away.
    ///
    /// Runs are deliberately **not** swept. A run entry can never be
    /// removed — the apply path parks `WorkflowAdopt` and `WorkflowClosed`
    /// on a run it does not know, and those ops are authored by other nodes
    /// — so retiring one would wedge a peer's stream. See the note on
    /// `WorkflowRunUpsert` in `compact::compaction`.
    ///
    /// Returns how many were released.
    ///
    /// [`Compaction::Closed`]: crate::compact::Compaction::Closed
    /// [`JobsClear`]: mesh_proto::OpBody::JobsClear
    pub fn expire_closed(&self, now_micros: u64) -> usize {
        let Some(cutoff) = self
            .retention()
            .window(mesh_proto::RetentionTable::Runs)
            .cutoff_micros(now_micros)
        else {
            return 0; // `forever`: an operator typed it
        };
        let mut state = self.state.write();
        let before = state.jobs.len() + state.fanouts.len() + state.gangs.len();
        state.jobs.retain(|_, j| !j.cleared_by(cutoff));
        // Closed fan-outs and gangs go with their jobs (ENG-3). An adopted
        // one goes from memory too, although its plan stays in the log: an
        // op that later names it is released rather than parked, by the
        // rule in `ingest_internal`, because it is older than the window.
        let past = |closed_at: Option<u64>| closed_at.is_some_and(|at| at < cutoff);
        state.fanouts.retain(|_, f| !past(f.closed_at_micros));
        state.gangs.retain(|_, g| !past(g.closed_at_micros));
        let released = before - (state.jobs.len() + state.fanouts.len() + state.gangs.len());
        // Audit rows past a finite audit window leave the tail too, so a
        // node that has not restarted answers like one that has.
        if let Some(audit_cutoff) = state
            .retention
            .policy
            .window(mesh_proto::RetentionTable::Audit)
            .cutoff_micros(now_micros)
        {
            state.audit.retain(|_, e| e.at_micros >= audit_cutoff);
        }
        // And facts past the `facts` window (ENG-19). The one retention
        // knob with no enforcement path at all: LWW keeps a subject's
        // newest document, which bounds a subject that is *still being
        // collected* and does nothing for one that is not. A host taken out
        // of the inventory, a collector an admin removed, a machine that
        // left — each leaves its last document in memory and in the log for
        // the life of the mesh.
        //
        // Read off `collected_at_micros`, the instant the collector
        // observed it, which is the same instant `compact::compaction`
        // expires the op at — so what a long-running node holds and what a
        // freshly replayed one holds are the same set.
        //
        // A live fact is never at risk: it is republished every collection
        // interval, and the default window is a year.
        if let Some(facts_cutoff) = state
            .retention
            .policy
            .window(mesh_proto::RetentionTable::Facts)
            .cutoff_micros(now_micros)
        {
            for collectors in state.facts.values_mut() {
                collectors.retain(|_, e| e.collected_at_micros >= facts_cutoff);
            }
            state.facts.retain(|_, collectors| !collectors.is_empty());
        }
        released
    }

    /// Publish this node's running totals for one principal in one window
    /// (C0 P3 shape; C1 P1/P2 are what will call it from the job path).
    ///
    /// Idempotent by construction: re-publishing the same totals merges to
    /// the same value everywhere, so a caller that is unsure whether its
    /// last publish landed should simply publish again.
    pub fn publish_usage(&self, usage: mesh_proto::UsagePartial) -> Result<OpEnvelope> {
        self.author_op(OpBody::UsageAdd {
            usage: Box::new(usage),
        })
    }

    /// What is in the op-log, by entity (M31 P0).
    ///
    /// Scans the whole log, every author included — the only way to answer
    /// *"what is this made of, and how fast is it growing"* without keeping
    /// a second set of counters that would drift from the first. Called by
    /// `doctor`, by `meshnode state show`, and behind a cache by `/metrics`;
    /// never on a path a job waits on.
    pub fn census(&self) -> Result<crate::compact::OpCensus> {
        let ops = self.store.load_all()?;
        let now = mesh_proto::now_micros();
        let retention = self.retention();
        let groups = self.retirable_groups();
        Ok(crate::compact::census_with(
            &ops,
            now,
            self.store.bytes_on_disk(),
            &retention,
            &groups,
        ))
    }

    /// The retention gate across every sink that asks for one (M31 B2,
    /// §19 Q3).
    ///
    /// **The strictest ceiling and the slowest sink**, deliberately. With
    /// two gating sinks, a record is held until *both* have taken it —
    /// otherwise the faster one's progress would authorise deleting
    /// something the slower one still needs — and the ceiling is the
    /// smallest, so no sink can extend another's hold beyond what its own
    /// operator agreed to.
    ///
    /// Reads only replicated state, so two nodes compute the same gate.
    pub fn export_gate(&self) -> crate::compact::ExportGate {
        self.read(|s| {
            let mut hold_days = 0u32;
            let mut through = u64::MAX;
            for entry in s.sinks.values().filter(|e| e.is_armed()) {
                let days = entry.sink.on_failure.hold_for_export_days;
                if days == 0 {
                    continue;
                }
                // The smallest ceiling wins: a sink cannot extend another
                // sink's hold past what its own operator signed for.
                hold_days = if hold_days == 0 {
                    days
                } else {
                    hold_days.min(days)
                };
                // The slowest gating sink sets the watermark.
                through = through.min(s.sink_progress.get(&entry.sink.name).copied().unwrap_or(0));
            }
            if hold_days == 0 {
                return crate::compact::ExportGate::default();
            }
            crate::compact::ExportGate {
                exported_through_micros: if through == u64::MAX { 0 } else { through },
                hold_micros: u64::from(hold_days) * 86_400 * 1_000_000,
            }
        })
    }

    /// The mesh's retention policy, or the shipped defaults when nobody has
    /// signed one (M31 A1).
    ///
    /// **Read from replicated state, never from node config.** That is C9,
    /// and it is the property the whole phase turns on: two nodes with
    /// different `meshnode.toml` files and the same signed policy drop
    /// exactly the same ops and answer exactly the same questions.
    pub fn retention(&self) -> mesh_proto::RetentionPolicy {
        self.read(|s| s.retention.policy.clone())
    }

    /// Op-log figures for `meshnode state show`.
    pub fn op_stats(&self, horizon_secs: u64) -> Result<crate::compact::OpStats> {
        let me = self.node_id();
        let plan = self.compaction_plan(horizon_secs)?;
        let now = mesh_proto::now_micros();
        let (pruned_mine, pruned_mesh, usage_live, usage_expired) = self.read(|s| {
            let live = s
                .usage
                .keys()
                .filter(|(_, _, w)| mesh_proto::usage_window_is_live(*w, now))
                .count() as u64;
            (
                s.pruned.get(&me).map(|p| p.count()).unwrap_or(0),
                s.pruned.values().map(|p| p.count()).sum::<u64>(),
                live,
                s.usage.len() as u64 - live,
            )
        });
        // What is droppable *now* excludes what a previous pass already
        // took — `plan.drop` is cumulative, and the operator wants the
        // number that would change if they ran it.
        let droppable_now = plan.dropped_total().saturating_sub(pruned_mine);
        Ok(crate::compact::OpStats {
            ops_total: self.store.len()?,
            ops_mine: self.store.count_for(&me)?,
            pruned_mine,
            droppable_now,
            held_by_horizon: plan.held_by_horizon,
            horizon_secs,
            pruned_mesh,
            usage_live,
            usage_expired,
            bytes_on_disk: self.store.bytes_on_disk(),
        })
    }

    fn ingest_internal(&self, mut ops: Vec<OpEnvelope>, persist: Persist) -> Result<usize> {
        // Per-author sequence order; author interleaving handled by parking.
        ops.sort_by_key(|op| (op.author, op.seq));
        let mut pending = ops;
        let mut applied_total = 0usize;
        let mut opaque_total = 0usize;
        let mut to_persist: Vec<OpEnvelope> = Vec::new();

        // Fixpoint loop: each pass applies what it can; ops whose authors
        // become members mid-batch succeed on a later pass.
        //
        // Once nothing more applies, one pass releases *orphans* (ENG-3):
        // an adoption or close of a fan-out or gang this node does not
        // know, written by this node or dated before the runs window.
        // Their plan was retired, and nothing will ever bring it back; left
        // parked, they would hold up every later op of their author — on
        // this node's own log, after a restart, that is a sequence number
        // issued twice. Any progress returns the loop to ordinary passes,
        // so an orphan is only released when its plan is not in the batch.
        //
        // At replay the same pass settles any op of this node's own that
        // still parks (ENG-4): it is on this node's disk, it may have been
        // served, and nothing will ever unpark it — left parked, its
        // sequence number would be issued again. Ops just authored are not
        // settled but taken back (`commit_own`), and this node's own ops
        // fetched back from peers are not either: what they wait for may
        // simply be in a later batch.
        let own_on_disk = persist == Persist::Replay;
        let mut release = false;
        loop {
            let mut parked: Vec<OpEnvelope> = Vec::new();
            let mut progressed = false;
            for op in pending.drain(..) {
                if persist == Persist::Replay {
                    // Replay at boot: tell the start deadline it moved.
                    crate::vitals::startup_progress(1);
                }
                // A release pass reconsiders only what it may settle — an
                // orphan, or one of this node's own ops on disk: everything
                // else would park again, after paying for its signature a
                // second time.
                let settle_own = own_on_disk && op.author == self.node_id();
                if release
                    && !settle_own
                    && !op
                        .body_index()
                        .is_some_and(|i| mesh_proto::OPBODY_GROUP_REFERENCE_INDICES.contains(&i))
                {
                    parked.push(op);
                    continue;
                }
                let mode = ApplyMode {
                    release_orphans: release,
                    settle_parked: release && settle_own,
                    authoring: persist == Persist::Authored,
                    // Replay, and only replay: these ops came out of this
                    // node's own store and were verified on the way in
                    // (**H-4**, ENG-20). An op from a peer, a restore or a
                    // rebuild is not `Persist::Replay` and pays the check.
                    trusted: persist == Persist::Replay,
                };
                match self.try_apply(&op, mode)? {
                    outcome @ (ApplyOutcome::Applied
                    | ApplyOutcome::AppliedOpaque
                    | ApplyOutcome::AppliedInert) => {
                        applied_total += 1;
                        if outcome == ApplyOutcome::AppliedOpaque {
                            opaque_total += 1;
                        }
                        progressed = true;
                        self.clock.observe(op.hlc);
                        if persist == Persist::Relaxed {
                            to_persist.push(op);
                        }
                    }
                    ApplyOutcome::Ignored => {}
                    ApplyOutcome::ParkedLearned => {
                        progressed = true;
                        parked.push(op);
                    }
                    ApplyOutcome::Parked => parked.push(op),
                }
            }
            if !parked.is_empty() && (progressed || !release) {
                release = !progressed;
                pending = parked;
                continue;
            }
            // Recorded on every batch, including the empty case: a gauge
            // that is only written when it is non-zero never comes back
            // down, and would report a backlog that cleared an hour ago.
            self.parked_last_batch
                .store(parked.len() as u64, std::sync::atomic::Ordering::Relaxed);
            if !parked.is_empty() {
                debug!(
                    count = parked.len(),
                    "ops parked awaiting authorization context"
                );
            }
            break;
        }

        // One line per batch, not one per op: a node that is genuinely a
        // release behind would otherwise fill its log with the same fact.
        if opaque_total > 0 {
            warn!(
                count = opaque_total,
                "carried ops this build cannot interpret — state for those entity kinds \
                 is not materialized here; upgrade meshnode to see them"
            );
        }

        match persist {
            Persist::Replay | Persist::Authored | Persist::Held => {}
            // A peer's: re-fetchable, so the flush can wait for the timer.
            Persist::Relaxed if !to_persist.is_empty() => {
                if let Err(e) = self.store.append_relaxed(&to_persist) {
                    // Applied and not written. Kept for the reopen, or the
                    // store would have a hole this node never refills.
                    self.unwritten.lock().extend(to_persist);
                    return Err(e);
                }
            }
            _ => {}
        }
        Ok(applied_total)
    }

    /// Is this op an adoption or close of a group this node has no plan
    /// for, that may be applied without effect rather than parked (ENG-3)?
    /// See the fixpoint loop in `ingest_internal` for when it is asked.
    fn is_releasable_orphan(&self, state: &MeshState, op: &OpEnvelope, body: &OpBody) -> bool {
        let unknown = match body {
            OpBody::FanoutAdopt { group, .. } | OpBody::FanoutClosed { group, .. } => {
                !state.fanouts.contains_key(group)
            }
            OpBody::GangAdopt { gang, .. } | OpBody::GangClosed { gang, .. } => {
                !state.gangs.contains_key(gang)
            }
            _ => return false,
        };
        if !unknown {
            return false;
        }
        // This node's own, from its own log — not fetched back from peers
        // after a restore, where the plan may simply be in a later batch.
        (op.author == self.node_id() && self.restore_hold.lock().is_none())
            || state
                .retention
                .policy
                .window(mesh_proto::RetentionTable::Runs)
                .cutoff_micros(mesh_proto::now_micros())
                .is_some_and(|cutoff| op.hlc.wall_micros < cutoff)
    }

    fn try_apply(&self, op: &OpEnvelope, mode: ApplyMode) -> Result<ApplyOutcome> {
        let ApplyMode {
            release_orphans,
            settle_parked,
            authoring,
            trusted,
        } = mode;
        // 0. Who wrote it, before any cryptography (SEC-10). An op by a
        //    revoked author never applies and an op by an author this node
        //    does not know yet only parks — neither outcome needs its
        //    signature checked, and checking it first let anyone who could
        //    get ops in front of this node spend its CPU on Ed25519 for
        //    authors it would never accept. The same questions are asked
        //    again under the write lock below.
        {
            let state = self.state.read();
            if state.revoked.contains_key(&op.author) {
                return Ok(ApplyOutcome::Ignored);
            }
            if !state.is_active_member(&op.author) && op.author != self.genesis.body.founder_node {
                return Ok(ApplyOutcome::Parked);
            }
        }

        // 1. Signature. Covers the *encoded* body, so an op this build has
        //    no variant for is still provably authentic — which is exactly
        //    what makes storing and forwarding it safe (F0 §3.2).
        //
        //    Skipped for an op replayed from this node's own store, which
        //    was verified when it arrived (`trusted`, **H-4**). The shape
        //    check is kept either way: it is a length comparison, and a
        //    malformed signature on disk is a corrupt record whatever its
        //    provenance.
        let sig: [u8; 64] = match op.signature.as_slice().try_into() {
            Ok(s) => s,
            Err(_) => {
                warn!(author = %op.author.short(), seq = op.seq, "op with malformed signature");
                return Ok(ApplyOutcome::Ignored);
            }
        };
        if !trusted && !verify_signature(&op.author.0, &op.signing_bytes(), &sig) {
            warn!(author = %op.author.short(), seq = op.seq, "op with invalid signature");
            return Ok(ApplyOutcome::Ignored);
        }

        let mut state = self.state.write();

        // 2. C0 — a checkpoint is the one op that has to be understood
        //    *before* the gapless test rather than after it, because what
        //    it describes lies behind it: the author's own seqs 1..=100 are
        //    gone and the op saying so is seq 101. Tested in seq order it
        //    would park on the very hole it exists to explain.
        //
        //    Taking it early is safe because a checkpoint is
        //    self-authenticating — the signature over the encoded body was
        //    verified in step 1, and an author is the only authority on its
        //    own sequence numbers. It is also idempotent: the op still
        //    applies normally below when its turn comes, and re-inserting
        //    the same ranges changes nothing.
        //
        // **Peeked, not decoded** (**H-4**). This ran `decode_body()` on
        // every op purely to ask whether it was a checkpoint, and then the
        // real decode ran again at step 5 — so a boot replaying a million
        // ops decoded two million bodies to apply one million. The varint
        // peek `op_body_index` already existed for exactly this shape of
        // question; the constant it is compared against is pinned by a test
        // beside the enum.
        let mut learned = false;
        if op.body_index() == Some(mesh_proto::OPBODY_CHECKPOINT_INDEX)
            && let Some(OpBody::Checkpoint { checkpoint }) = op.decode_body()
        {
            learned = self.note_checkpoint(&mut state, op, &checkpoint);
        }

        // 3. Gapless per-author ordering.
        let expected = state.applied.get(&op.author).copied().unwrap_or(0) + 1;
        if op.seq < expected {
            return Ok(ApplyOutcome::Ignored); // duplicate
        }
        if op.seq > expected {
            // gap: earlier op not yet seen
            return Ok(if learned {
                ApplyOutcome::ParkedLearned
            } else {
                ApplyOutcome::Parked
            });
        }

        // 4. Author authorization: revoked authors are dead forever; anyone
        //    else must be an active member or the founder node.
        if state.revoked.contains_key(&op.author) {
            return Ok(ApplyOutcome::Ignored);
        }
        let author_ok =
            state.is_active_member(&op.author) || op.author == self.genesis.body.founder_node;
        if !author_ok {
            return Ok(ApplyOutcome::Parked);
        }

        // 4b. Time (SEC-5). Ops are authorized, and last-writer-wins races
        //     are decided, at the time their author put on them, so that
        //     time has to be believable:
        //
        //     * not ahead of this machine by more than the allowance — such
        //       an op waits, parked, until real time catches up, and
        //       `doctor` names the node whose clock is wrong;
        //     * not before its author's previous op — applied inert, so a
        //       node cannot date a grant to a moment its signer still held
        //       admin, and the sequence still moves;
        //     * for an author's first op here, not before the node was
        //       first added — parked, because the add may simply not have
        //       replayed yet.
        let now = mesh_proto::now_micros();
        if op.hlc.wall_micros > now.saturating_add(crate::hlc::MAX_AHEAD_MICROS) {
            self.note_ahead(op.author, op.hlc.wall_micros - now);
            return Ok(ApplyOutcome::Parked);
        }
        match state.author_clock.get(&op.author) {
            Some(last) if op.hlc <= *last => {
                warn!(
                    author = %op.author.short(),
                    seq = op.seq,
                    "op dated before its author's previous one; applied without effect"
                );
                note_applied(&mut state, op);
                return Ok(ApplyOutcome::AppliedInert);
            }
            Some(_) => {}
            None => {
                if op.author != self.genesis.body.founder_node
                    && state
                        .first_added
                        .get(&op.author)
                        .is_none_or(|added| op.hlc < *added)
                {
                    return Ok(ApplyOutcome::Parked);
                }
            }
        }

        // 5. Decode. An op from a newer meshnode is authentic, in order and
        //    authorized, but carries an entity kind this build has no code
        //    for. Advance the watermark and keep it verbatim so it still
        //    reaches everyone else — convergence is about *carrying* ops;
        //    interpretation is local (F0 §3.2).
        let Some(body) = op.decode_body() else {
            debug!(
                author = %op.author.short(),
                seq = op.seq,
                index = op.body_index().unwrap_or_default(),
                "op body is from a newer protocol; carried opaquely"
            );
            note_applied(&mut state, op);
            return Ok(ApplyOutcome::AppliedOpaque);
        };

        // 5b. An orphaned adoption or close, when the caller has run out of
        //     everything else (ENG-3).
        if release_orphans && self.is_releasable_orphan(&state, op, &body) {
            debug!(
                author = %op.author.short(),
                seq = op.seq,
                kind = body.kind(),
                "released an op for a retired fan-out or gang"
            );
            note_applied(&mut state, op);
            return Ok(ApplyOutcome::AppliedInert);
        }

        // 6. Body-specific rules + merge.
        //
        // **Contained** (SEC-22). This runs under the state write lock, on
        // bytes any member can author, on every node that receives them. A
        // panic here used to unwind through the guard and poison the lock,
        // and every later read on that node panicked in turn — one op that
        // tripped a bug took down every node it reached. The guard now
        // stays outside the unwind: a panicking op is logged, applied
        // without further effect so it is not retried, and carried on like
        // any other. A panic is deterministic in the op and the state
        // before it, so every node that meets it stops at the same place.
        let merged = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.merge_body(&mut state, op, &body)
        }));
        match merged {
            Ok(Ok(None)) => {}
            // Refused or stale at the body: a no-op that still counts, as
            // the module header says. Returned without being counted, it
            // left a hole in its author's sequence, and every later op of
            // that author parked behind it on this node for ever — two
            // admins editing one quota was enough. This node's own
            // authoring is the exception: a refusal there is reported, and
            // the op taken back (`commit_own`).
            Ok(Ok(Some(ApplyOutcome::Ignored))) if !authoring => {
                debug!(
                    author = %op.author.short(),
                    seq = op.seq,
                    kind = body.kind(),
                    "op refused or stale; counted without effect"
                );
                note_applied(&mut state, op);
                return Ok(ApplyOutcome::AppliedInert);
            }
            // Dropped by its own arm, which forgot the count as often as it
            // remembered it.
            Ok(Ok(Some(ApplyOutcome::Applied))) => {
                note_applied(&mut state, op);
                return Ok(ApplyOutcome::Applied);
            }
            // This node's own op, on disk, that nothing will ever unpark:
            // applied without effect rather than holding up every later op
            // of ours — after a restart that would be a spent sequence
            // number issued again (ENG-4). Only once nothing else applies.
            Ok(Ok(Some(ApplyOutcome::Parked))) if settle_parked => {
                warn!(
                    seq = op.seq,
                    kind = body.kind(),
                    "an op this node wrote cannot apply; recorded without effect"
                );
                note_applied(&mut state, op);
                return Ok(ApplyOutcome::AppliedInert);
            }
            Ok(Ok(Some(outcome))) => return Ok(outcome),
            Ok(Err(e)) => return Err(e),
            Err(panic) => {
                let why = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                tracing::error!(
                    author = %op.author.short(),
                    seq = op.seq,
                    kind = body.kind(),
                    "applying this op panicked; it was skipped: {why}"
                );
                self.contained_panics
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                note_applied(&mut state, op);
                return Ok(ApplyOutcome::AppliedInert);
            }
        }

        note_applied(&mut state, op);
        // The change signal (**H-8**), on the one path every applied op
        // reaches — local and replicated alike. After the merge, so a
        // client that reacts to the bump reads the state the bump is
        // about; and after `note_applied`, so it is never told about an op
        // that then turned out not to apply.
        //
        // The state write lock is still held here, and deliberately: a
        // reader that takes the state lock and *then* reads a counter can
        // never see a version newer than the state it just read, which is
        // the one ordering that would cause a client to skip a refetch it
        // needed.
        drop(state);
        self.bump_topic(body.topic());
        Ok(ApplyOutcome::Applied)
    }

    /// Step 6 of [`Self::try_apply`]: what one authentic, in-order,
    /// authorized op does to the state. `None` is "applied"; `Some` is any
    /// other outcome the body decided on.
    fn merge_body(
        &self,
        state: &mut MeshState,
        op: &OpEnvelope,
        body: &OpBody,
    ) -> Result<Option<ApplyOutcome>> {
        #[cfg(test)]
        if let OpBody::SourceSynced { name, .. } = body
            && name == "__panic_in_merge__"
        {
            panic!("a merge rule tripped over its input");
        }
        match body {
            OpBody::MemberAdd { member, .. } => {
                // A name that could repaint a terminal is cleaned at merge
                // (SEC-9) — deterministically, so every node stores the same
                // name. An honest acceptor already refused it.
                let mut member = member.clone();
                member.name =
                    mesh_proto::text::clean(&member.name, mesh_proto::text::MAX_NAME_CHARS);
                let member = &member;
                state
                    .first_added
                    .entry(member.node_id)
                    .and_modify(|at| *at = (*at).min(op.hlc))
                    .or_insert(op.hlc);
                if state.revoked.contains_key(&member.node_id) {
                    // Revocation is terminal; re-adds are ignored.
                } else if let Some(existing) = state.members.get(&member.node_id) {
                    // Add-wins/idempotent; a re-add after `left` reactivates.
                    if existing.left && op.hlc > existing.updated_hlc {
                        let entry = MemberEntry {
                            info: member.clone(),
                            announced_tags: member.tags.clone(),
                            added_hlc: op.hlc,
                            updated_hlc: op.hlc,
                            left: false,
                            // It is here again, so there is no departure to
                            // strand jobs at. Jobs already settled stay
                            // settled: they are terminal, and a terminal
                            // record is not reopened by the machine coming
                            // back.
                            left_hlc: None,
                        };
                        state.members.insert(member.node_id, entry);
                        // A machine that left and came back is the same
                        // machine to whoever labelled it, so the assignment
                        // it had is put back on immediately.
                        state.remerge_tags(&member.node_id);
                    }
                } else {
                    state.members.insert(
                        member.node_id,
                        MemberEntry {
                            info: member.clone(),
                            announced_tags: member.tags.clone(),
                            added_hlc: op.hlc,
                            updated_hlc: op.hlc,
                            left: false,
                            left_hlc: None,
                        },
                    );
                    // An assignment can land before the node it names —
                    // ops replay per author, and an operator can label a
                    // machine that has not finished joining. Merging on
                    // arrival is what makes the order not matter.
                    state.remerge_tags(&member.node_id);
                }
            }
            OpBody::MemberUpdate {
                node_id,
                name,
                addrs,
                tags,
                slots,
            } => {
                // Only the node itself (or founder) may update its record.
                if op.author != *node_id && op.author != self.genesis.body.founder_node {
                    return Ok(Some(ApplyOutcome::Ignored));
                }
                if let Some(entry) = state.members.get_mut(node_id)
                    && op.hlc > entry.updated_hlc
                {
                    // A hazardous name is not taken (SEC-9); the rest of the
                    // announcement is.
                    if mesh_proto::text::name_problem(name).is_none() {
                        entry.info.name = name.clone();
                    }
                    entry.info.addrs = addrs.clone();
                    // The node's own list is recorded as announced, and the
                    // merged one is rebuilt from it — a re-announcement
                    // must not drop the admin's assignment, which is
                    // exactly what assigning into `MemberUpdate` itself
                    // would have done at the next boot.
                    entry.announced_tags = tags.clone();
                    entry.info.slots = *slots;
                    entry.updated_hlc = op.hlc;
                    state.remerge_tags(node_id);
                }
            }
            OpBody::MemberRevoke { revocation } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &revocation.by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &revocation.signing_bytes(),
                    &revocation.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "node revocation dropped");
                    }
                    SignerCheck::Authorized => {
                        state
                            .revoked
                            .entry(revocation.node_id)
                            .or_insert_with(|| (revocation.reason.clone(), op.hlc));
                        // A revoked node authors nothing ever again, so the
                        // jobs it was running are settled here rather than
                        // left at `running` for the life of the mesh
                        // (ENG-16).
                        for id in self.settle_jobs_lost(state, &revocation.node_id) {
                            self.mark_job(id);
                        }
                    }
                }
            }
            OpBody::NodeQuarantineSet { quarantine } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &quarantine.by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &quarantine.signing_bytes(),
                    &quarantine.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(node = %quarantine.node_id.short(), why, "quarantine dropped");
                    }
                    SignerCheck::Authorized => {
                        let newer = state
                            .quarantined
                            .get(&quarantine.node_id)
                            .is_none_or(|e| op.hlc > e.hlc);
                        // A lift is LWW against the same clock as an
                        // imposition, so two admins disagreeing converge on
                        // whoever spoke last rather than on who is luckier.
                        if newer {
                            if quarantine.quarantined {
                                state.quarantined.insert(
                                    quarantine.node_id,
                                    QuarantineEntry {
                                        reason: quarantine.reason.clone(),
                                        by: quarantine.by,
                                        at_micros: quarantine.at_micros,
                                        hlc: op.hlc,
                                    },
                                );
                            } else {
                                state.quarantined.remove(&quarantine.node_id);
                            }
                        }
                    }
                }
            }
            OpBody::NodeTagsSet { assignment } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &assignment.by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &assignment.signing_bytes(),
                    &assignment.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(node = %assignment.node_id.short(), why, "tag assignment dropped");
                    }
                    SignerCheck::Authorized => {
                        // The refusals the daemon makes on the way in are
                        // re-made here, because an op arrives from the
                        // network as well as from a local caller and a
                        // peer running a build that let something through
                        // must not be able to write `cap.cores=999` into
                        // this node's idea of the fleet.
                        let clean: Vec<String> = assignment
                            .tags
                            .iter()
                            .filter(|t| mesh_proto::assignable_tag(t).is_ok())
                            .cloned()
                            .collect();
                        // LWW per node, like every other standing decision:
                        // two admins disagreeing is one assignment that
                        // changed, and the later signature is the answer.
                        let newer = state
                            .assigned_tags
                            .get(&assignment.node_id)
                            .is_none_or(|e| op.hlc > e.hlc);
                        if newer {
                            if clean.is_empty() {
                                state.assigned_tags.remove(&assignment.node_id);
                            } else {
                                state.assigned_tags.insert(
                                    assignment.node_id,
                                    AssignedTagsEntry {
                                        tags: clean,
                                        by: assignment.by,
                                        at_micros: assignment.at_micros,
                                        hlc: op.hlc,
                                    },
                                );
                            }
                            state.remerge_tags(&assignment.node_id);
                        }
                    }
                }
            }
            OpBody::NodeExecutionSet { decision } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &decision.decided_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &decision.signing_bytes(),
                    &decision.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "node execution decision dropped");
                    }
                    SignerCheck::Authorized => {
                        // LWW by HLC, and *not* keyed on the author: two
                        // admins disagreeing is one decision that changed,
                        // not two decisions to reconcile.
                        let entry = state.node_execution.entry(decision.node_id);
                        match entry {
                            std::collections::btree_map::Entry::Occupied(mut e)
                                if op.hlc > e.get().hlc =>
                            {
                                e.insert(NodeExecutionEntry {
                                    allowed: decision.allowed,
                                    decided_by: decision.decided_by,
                                    decided_at_micros: decision.decided_at_micros,
                                    reason: decision.reason.clone(),
                                    hlc: op.hlc,
                                });
                            }
                            std::collections::btree_map::Entry::Occupied(_) => {}
                            std::collections::btree_map::Entry::Vacant(e) => {
                                e.insert(NodeExecutionEntry {
                                    allowed: decision.allowed,
                                    decided_by: decision.decided_by,
                                    decided_at_micros: decision.decided_at_micros,
                                    reason: decision.reason.clone(),
                                    hlc: op.hlc,
                                });
                            }
                        }
                    }
                }
            }
            OpBody::MemberLeft { node_id } => {
                // Only the node itself (or the founder) may mark departure.
                if op.author != *node_id && op.author != self.genesis.body.founder_node {
                    return Ok(Some(ApplyOutcome::Ignored));
                }
                if let Some(entry) = state.members.get_mut(node_id)
                    && op.hlc > entry.updated_hlc
                {
                    entry.left = true;
                    entry.updated_hlc = op.hlc;
                }
                // The departure instant, merged by max so every node
                // converges on one whatever order the ops arrive in
                // (ENG-16). Kept apart from `updated_hlc`, which moves for
                // reasons that have nothing to do with leaving.
                if let Some(entry) = state.members.get_mut(node_id) {
                    entry.left_hlc = Some(match entry.left_hlc {
                        Some(at) => at.max(op.hlc),
                        None => op.hlc,
                    });
                }
                // Jobs this node was running can never be reported again.
                for id in self.settle_jobs_lost(state, node_id) {
                    self.mark_job(id);
                }
            }
            OpBody::InviteCreate { invite } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &invite.created_by_user,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &invite.signing_bytes(),
                    &invite.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "invite record dropped");
                    }
                    SignerCheck::Authorized => {
                        if !state.invites.contains_key(&invite.invite_id) {
                            let early = state
                                .pending_invites
                                .remove(&invite.invite_id)
                                .unwrap_or_default();
                            state.invites.insert(
                                invite.invite_id,
                                InviteEntry {
                                    public: invite.clone(),
                                    consumed_by: early.consumed_by,
                                    revoked: early.revoked,
                                },
                            );
                        }
                    }
                }
            }
            OpBody::InviteConsume { invite_id, node_id } => {
                // The consume is authored by whichever member admitted the
                // joiner, not by the invite's creator, so it can arrive
                // first. Held, not dropped (SEC-17).
                match state.invites.get_mut(invite_id) {
                    Some(inv) => {
                        inv.consumed_by.insert(*node_id);
                    }
                    None => {
                        state
                            .pending_invites
                            .entry(*invite_id)
                            .or_default()
                            .consumed_by
                            .insert(*node_id);
                    }
                }
            }
            OpBody::InviteRevoke {
                invite_id,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &invite_revoke_signing_bytes(invite_id, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "invite revoke dropped");
                    }
                    SignerCheck::Authorized => match state.invites.get_mut(invite_id) {
                        Some(inv) => inv.revoked = true,
                        None => {
                            state.pending_invites.entry(*invite_id).or_default().revoked = true;
                        }
                    },
                }
            }
            OpBody::JobUpsert { summary } => {
                // Only the executor may report its own jobs.
                if op.author != summary.executor {
                    return Ok(Some(ApplyOutcome::Ignored));
                }
                match state.jobs.get(&summary.job_id) {
                    Some(existing) if existing.updated_hlc >= summary.updated_hlc => {}
                    _ => {
                        state.jobs.insert(summary.job_id, summary.clone());
                        self.mark_job(summary.job_id);
                    }
                }
                // A force-cancel may have arrived (or replayed) before the
                // job it overrides; settle it now. Newer executor reports
                // pass through untouched — see `apply_force_cancel`.
                if let Some(&at) = state.jobs_force_cancelled.get(&summary.job_id)
                    && let Some(entry) = state.jobs.get_mut(&summary.job_id)
                    && apply_force_cancel(entry, at)
                {
                    self.mark_job(summary.job_id);
                }
                // …and the same question for an executor that has gone for
                // good (ENG-16). Reached on every boot, because the op that
                // retired the executor is in another author's log and
                // nothing orders the two: without this, replaying the
                // executor's last `running` report after its revocation put
                // the job back to `running` on every start.
                if let Some(at) = state.gone_at(&summary.executor)
                    && let Some(entry) = state.jobs.get_mut(&summary.job_id)
                    && settle_lost(entry, at)
                {
                    self.mark_job(summary.job_id);
                }
                // Then the standing clear, re-settled against the job's
                // *latest* report rather than against this one in
                // isolation. Reached on every boot, because replay is per
                // author: the clear and this op are in different logs and
                // nothing orders them.
                //
                // **Why after the insert and not a guard before it.** A
                // job is a sequence of upserts — queued, running,
                // finished — and only the last one carries the finish time
                // the watermark is compared against. Dropping a *terminal*
                // report on its own, which is what a guard here can see,
                // left the earlier `running` report standing as the job's
                // whole story: a node replaying an executor whose id sorts
                // after the clear's author would resurrect every job that
                // clear forgot, stuck at `running` for as long as the
                // op-log kept the pair. The same hole swallowed a terminal
                // report that merely *arrived* late, from an executor that
                // had been unreachable when the button was pressed. The
                // map, not the op, is what the clear has to be applied to.
                let mark = state.jobs_cleared_before;
                if state
                    .jobs
                    .get(&summary.job_id)
                    .is_some_and(|entry| entry.cleared_by(mark))
                {
                    state.jobs.remove(&summary.job_id);
                }
            }
            OpBody::JobsClear {
                before_micros,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &jobs_clear_signing_bytes(*before_micros, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "jobs clear dropped");
                    }
                    SignerCheck::Authorized => {
                        // Merged by max, so two admins clearing concurrently
                        // converge on the later watermark rather than on
                        // whichever op happened to arrive second.
                        state.jobs_cleared_before = state.jobs_cleared_before.max(*before_micros);
                        let mark = state.jobs_cleared_before;
                        state.jobs.retain(|_, j| !j.cleared_by(mark));
                        // **Overrides are kept, not pruned with their
                        // jobs.** A forced cancel is the fact that made
                        // the job terminal, and therefore the fact that
                        // made it clearable at all; the emptied map is not
                        // a record of it. Dropping the override here left
                        // a boot that replayed the executor's `JobUpsert`
                        // after this op — per-author replay, so an
                        // executor whose node id sorts after the clear's
                        // author is enough — holding a `running` summary
                        // with nothing left to settle it and nothing able
                        // to clear it: `cleared_by` wants a terminal state,
                        // and the force was carrying it. That is the ghost
                        // that survived Cancel, survived Clear history, and
                        // came back on the next start.
                        //
                        // The map is bounded by the same thing the op log
                        // is: one entry per force op, and the op itself is
                        // LWW per job under compaction.
                    }
                }
            }
            OpBody::JobCancelForce {
                job_id,
                by,
                signature,
            } => {
                // Any node may author this — the executor of a job that
                // will never report again is exactly the node that cannot.
                // The authority is the user's signature, checked here on
                // every peer, and it is *operator/admin* authority
                // (`rbac::CANCEL_ANY`), not the broader submit-and-cancel
                // set: a force rewrites another node's record, and the
                // scope rule must be decidable from roles alone — an
                // ownership check would need a summary that may not have
                // arrived, and an op parked on something that may never
                // arrive wedges its author's whole log behind it.
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::CANCEL_ANY,
                    op.hlc.wall_micros,
                    &job_cancel_force_signing_bytes(job_id, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "forced job cancel dropped");
                    }
                    SignerCheck::Authorized => {
                        // Recorded even when the job is not here yet: the
                        // executor's `JobUpsert` is another author's log
                        // and may replay after this op. `JobUpsert` checks
                        // the map, which is what makes the pair commute.
                        let at = state.jobs_force_cancelled.entry(*job_id).or_insert(op.hlc);
                        *at = (*at).max(op.hlc);
                        let at = *at;
                        if let Some(entry) = state.jobs.get_mut(job_id)
                            && apply_force_cancel(entry, at)
                        {
                            self.mark_job(*job_id);
                        }
                        // Then the standing clear, for the same reason
                        // `JobUpsert` re-settles it: the force is what
                        // finishes the job, so a job the watermark could
                        // not touch while it was `running` becomes
                        // forgettable the moment this lands — whichever
                        // side of the clear the force arrives on.
                        let mark = state.jobs_cleared_before;
                        if state
                            .jobs
                            .get(job_id)
                            .is_some_and(|entry| entry.cleared_by(mark))
                        {
                            state.jobs.remove(job_id);
                        }
                    }
                }
            }
            OpBody::BundleAnnounce { meta } => {
                // Content-addressed: the id fully determines the content, so a
                // re-announce is idempotent. Any member may announce.
                state
                    .bundles
                    .entry(meta.bundle_id)
                    .or_insert_with(|| BundleEntry {
                        meta: meta.clone(),
                        holders: BTreeSet::new(),
                    });
            }
            OpBody::BundleHold { bundle_id, node_id } => {
                // A node vouches only for its own copy.
                if op.author != *node_id {
                    return Ok(Some(ApplyOutcome::Ignored));
                }
                if let Some(entry) = state.bundles.get_mut(bundle_id) {
                    entry.holders.insert(*node_id);
                }
                // Unknown bundle: announce has a lower seq from the same author
                // in the common case; gapless ordering makes this safe to drop.
            }
            OpBody::ServiceVersionAdd { version } => {
                let founder = self.genesis.body.founder_user;
                // Two authorization paths: a user-signed version (the normal
                // case), or a Git-source auto-bump with an empty signature
                // delegated through the source record (MASTERPLAN §8.1).
                let verdict = if version.signature.is_empty() {
                    check_source_bump(state, &founder, version)
                } else {
                    check_user_signed(
                        state,
                        &founder,
                        &version.created_by,
                        rbac::CONTENT,
                        op.hlc.wall_micros,
                        &version.signing_bytes(),
                        &version.signature,
                    )
                };
                match verdict {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), service = %version.name, why,
                            "service version dropped");
                    }
                    SignerCheck::Authorized => {
                        // Grants can be scoped to service-name globs. Source
                        // bumps were scope-checked when the source merged.
                        if !version.signature.is_empty()
                            && !rbac::can_manage_service(
                                state,
                                &founder,
                                &version.created_by,
                                &version.name,
                                op.hlc.wall_micros,
                            )
                        {
                            warn!(service = %version.name,
                                "service version dropped: grant does not cover this service");
                        } else {
                            let entry = state.services.entry(version.name.clone()).or_default();
                            // Immutable: first writer of a (name, version) wins.
                            entry
                                .versions
                                .entry(version.version)
                                .or_insert_with(|| version.clone());
                            entry.removed = false;
                        }
                    }
                }
            }
            OpBody::ServiceHead {
                name,
                version,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                // A head move with an empty signature is a Git auto-bump: it
                // may only land on a version that itself carries the source's
                // delegation (MASTERPLAN §8.1).
                let verdict = if signature.is_empty() {
                    match state
                        .services
                        .get(name)
                        .and_then(|e| e.versions.get(version))
                    {
                        None => SignerCheck::Park, // version op not applied yet
                        Some(target) if target.created_by != *by => {
                            SignerCheck::Reject("head move signer is not the version creator")
                        }
                        Some(target) => check_source_bump(state, &founder, target),
                    }
                } else {
                    check_user_signed(
                        state,
                        &founder,
                        by,
                        rbac::CONTENT,
                        op.hlc.wall_micros,
                        &service_head_signing_bytes(name, *version, by),
                        signature,
                    )
                };
                match verdict {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(service = %name, why, "service head move dropped");
                    }
                    SignerCheck::Authorized => {
                        if !signature.is_empty()
                            && !rbac::can_manage_service(
                                state,
                                &founder,
                                by,
                                name,
                                op.hlc.wall_micros,
                            )
                        {
                            warn!(service = %name,
                                "service head move dropped: grant does not cover this service");
                        } else {
                            let entry = state.services.entry(name.clone()).or_default();
                            // Advance or rewind the head; only to a known version.
                            if entry.versions.contains_key(version) {
                                entry.head = *version;
                                entry.removed = false;
                            }
                        }
                    }
                }
            }
            OpBody::ServiceRemove {
                name,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::CONTENT,
                    op.hlc.wall_micros,
                    &service_remove_signing_bytes(name, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(service = %name, why, "service remove dropped");
                    }
                    SignerCheck::Authorized => {
                        if rbac::can_manage_service(state, &founder, by, name, op.hlc.wall_micros)
                            && let Some(entry) = state.services.get_mut(name)
                        {
                            entry.removed = true;
                        }
                    }
                }
            }
            // -- M3 governance ------------------------------------------------
            OpBody::UserAdd { record } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &record.added_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &record.signing_bytes(),
                    &record.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(user = %record.username, why, "user add dropped");
                    }
                    SignerCheck::Authorized => {
                        // First add wins; revocation is terminal — a revoked
                        // key is never resurrected by a re-add.
                        state
                            .users
                            .entry(record.user_id)
                            .or_insert_with(|| UserEntry {
                                record: record.clone(),
                                revoked: None,
                            });
                    }
                }
            }
            OpBody::ServiceAccountAdd { account } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &account.created_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &account.signing_bytes(),
                    &account.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(account = %account.name, why, "service account add dropped");
                    }
                    SignerCheck::Authorized => {
                        // First add wins, matching `UserAdd`: the marker
                        // describes a principal rather than creating one,
                        // and re-describing it must not reset its expiry.
                        state
                            .service_accounts
                            .entry(account.user_id)
                            .or_insert_with(|| account.clone());
                    }
                }
            }
            OpBody::OidcProviderPut { provider } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &provider.configured_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &provider.signing_bytes(),
                    &provider.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(provider = %provider.name, why, "oidc provider dropped");
                    }
                    SignerCheck::Authorized => {
                        let e = state.oidc_providers.entry(provider.name.clone());
                        match e {
                            std::collections::btree_map::Entry::Occupied(mut o)
                                if op.hlc > o.get().1 =>
                            {
                                o.insert((provider.clone(), op.hlc));
                            }
                            std::collections::btree_map::Entry::Vacant(v) => {
                                v.insert((provider.clone(), op.hlc));
                            }
                            _ => {}
                        }
                    }
                }
            }
            OpBody::OidcProviderRemove {
                name,
                by,
                at_micros,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                let bytes = mesh_proto::oidc_provider_remove_signing_bytes(name, by, *at_micros);
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &bytes,
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(provider = %name, why, "oidc provider removal dropped");
                    }
                    SignerCheck::Authorized => {
                        if state
                            .oidc_providers
                            .get(name)
                            .is_some_and(|(_, hlc)| op.hlc > *hlc)
                        {
                            state.oidc_providers.remove(name);
                        }
                    }
                }
            }
            OpBody::OidcAssertionPut { assertion } => {
                // No user signature to check: this is the *authoring node's*
                // claim, and the op envelope's node signature — already
                // verified before apply — is its provenance. What must be
                // checked is that a node is not claiming to have been some
                // other node when it verified.
                if assertion.verified_by != op.author {
                    warn!(
                        author = %op.author.short(),
                        claimed = %assertion.verified_by.short(),
                        "oidc assertion dropped: a node may only report its own verification"
                    );
                    return Ok(Some(ApplyOutcome::Applied));
                }
                let e = state.oidc_assertions.entry(assertion.user_id);
                match e {
                    std::collections::btree_map::Entry::Occupied(mut o) if op.hlc > o.get().1 => {
                        o.insert((assertion.clone(), op.hlc));
                    }
                    std::collections::btree_map::Entry::Vacant(v) => {
                        v.insert((assertion.clone(), op.hlc));
                    }
                    _ => {}
                }
            }
            OpBody::UserRevoke { revocation } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &revocation.revoked_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &revocation.signing_bytes(),
                    &revocation.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "user revoke dropped");
                    }
                    SignerCheck::Authorized => {
                        if revocation.user_id.0 == founder {
                            // The founder is the genesis trust anchor.
                            warn!("refusing to revoke the founding admin");
                        } else if let Some(entry) = state.users.get_mut(&revocation.user_id) {
                            entry
                                .revoked
                                .get_or_insert((revocation.reason.clone(), op.hlc));
                        } else {
                            return Ok(Some(ApplyOutcome::Parked));
                        }
                    }
                }
            }
            OpBody::GrantAdd { grant } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &grant.granted_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &grant.signing_bytes(),
                    &grant.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "grant dropped");
                    }
                    SignerCheck::Authorized => {
                        // The grantee must exist (or be the founder) so a
                        // grant cannot dangle; park until the UserAdd lands.
                        if grant.user_id.0 != founder && !state.users.contains_key(&grant.user_id) {
                            return Ok(Some(ApplyOutcome::Parked));
                        }
                        // A service account may not hold admin (E1-P4).
                        // Enforced here as well as at the API, because a
                        // check that only runs where a person typed is a
                        // check a modified client walks around — the same
                        // reason every governance signature is re-verified
                        // at merge rather than trusted from the sender.
                        //
                        // Ordering note: `ServiceAccountAdd` and `GrantAdd`
                        // can arrive in either order. If the marker has not
                        // landed yet this admits the grant, and the marker's
                        // own arrival does not retroactively strip it —
                        // which is why `serviceaccounts create` authors the
                        // marker *before* the grant, and why the API check
                        // is the one that actually protects the flow.
                        if let Err(why) =
                            rbac::grant_is_permitted(state, &grant.user_id, grant.role)
                        {
                            warn!(why, "grant dropped");
                            return Ok(Some(ApplyOutcome::Applied));
                        }
                        state
                            .grants
                            .entry(grant.grant_id)
                            .or_insert_with(|| GrantEntry {
                                grant: grant.clone(),
                                revoked: false,
                            });
                    }
                }
            }
            OpBody::GrantRevoke { revocation } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &revocation.revoked_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &revocation.signing_bytes(),
                    &revocation.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "grant revoke dropped");
                    }
                    SignerCheck::Authorized => match state.grants.get_mut(&revocation.grant_id) {
                        Some(entry) => entry.revoked = true,
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            // -- M23 enrolment (§3.2) --------------------------------------
            //
            // The only user-signed body in this file whose signer needs no
            // role, because the op confers none: it puts a row in a queue
            // that a human must still act on. What is checked is exactly
            // what is claimed — that the key signed for itself, and that
            // the node authoring the op is the node the person says they
            // asked from.
            OpBody::EnrolRequest { request } => {
                let Ok(sig64) = <[u8; 64]>::try_from(request.signature.as_slice()) else {
                    warn!("enrolment request dropped: malformed signature");
                    note_applied(state, op);
                    return Ok(Some(ApplyOutcome::Applied));
                };
                if !verify_signature(&request.user_id.0, &request.signing_bytes(), &sig64) {
                    warn!("enrolment request dropped: not signed by the key it names");
                } else if op.author != request.from_node {
                    // A node may carry anyone's request, but it may not
                    // claim the request came from somewhere else — the
                    // origin is the one piece of context an admin reads
                    // that the requester cannot vouch for themselves.
                    warn!(author = %op.author.short(), "enrolment request dropped: author is not the origin node");
                } else if request.suggested_username.len()
                    > mesh_proto::enrol::MAX_SUGGESTED_USERNAME
                {
                    warn!("enrolment request dropped: suggested username too long");
                } else if !state.is_active_member(&op.author) {
                    // The member op may simply not have arrived yet.
                    return Ok(Some(ApplyOutcome::Parked));
                } else if state.users.contains_key(&request.user_id) {
                    // Already a user. Nothing to ask for; nothing to show.
                    debug!("enrolment request ignored: already a mesh user");
                } else if request.is_expired_at(op.hlc.wall_micros) {
                    debug!("enrolment request ignored: already expired when it arrived");
                } else {
                    match state.enrolments.get_mut(&request.user_id) {
                        Some(entry) if op.hlc <= entry.updated_hlc => {
                            return Ok(Some(ApplyOutcome::Ignored));
                        }
                        Some(entry) => {
                            // Keep the decline: whether it still suppresses
                            // this ask is decided by the timestamps, in
                            // `is_declined`, so asking again after a no is
                            // visible rather than silently swallowed.
                            entry.request = (**request).clone();
                            entry.updated_hlc = op.hlc;
                        }
                        None => {
                            state.enrolments.insert(
                                request.user_id,
                                EnrolmentEntry {
                                    request: (**request).clone(),
                                    declined: None,
                                    updated_hlc: op.hlc,
                                },
                            );
                        }
                    }
                }
            }
            OpBody::EnrolDecline { decline } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &decline.declined_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &decline.signing_bytes(),
                    &decline.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "enrolment decline dropped");
                    }
                    // `MAX_DECLINE_REASON` named a bound that nothing
                    // enforced (S-Misc). Enforced here, where its sibling
                    // `MAX_SUGGESTED_USERNAME` already is: the reason is
                    // free text an admin types and every node stores, and a
                    // constant that is not a rule is worse than no constant.
                    SignerCheck::Authorized
                        if decline.reason.len() > mesh_proto::enrol::MAX_DECLINE_REASON =>
                    {
                        warn!("enrolment decline dropped: reason too long");
                    }
                    SignerCheck::Authorized => match state.enrolments.get_mut(&decline.user_id) {
                        // Latest decision wins, so an admin who declines
                        // and then changes their mind is not fighting an
                        // older tombstone.
                        Some(entry) => {
                            let newer = entry
                                .declined
                                .as_ref()
                                .is_none_or(|(_, _, at)| *at <= decline.at_micros);
                            if newer {
                                entry.declined = Some((
                                    decline.declined_by,
                                    decline.reason.clone(),
                                    decline.at_micros,
                                ));
                            }
                        }
                        // Nothing to decline. Not parked: a request that
                        // expired out of the queue is gone for good, and
                        // parking would retry this decline forever.
                        None => debug!("enrolment decline ignored: no such request"),
                    },
                }
            }
            // -- M24 fleet upgrade (§4) ------------------------------------
            //
            // Admin-signed, verified here as every governance body is. What
            // makes it safe to *replicate* an authorization to replace a
            // binary is that acting on it is a separate, driven step: a
            // node that merges this does nothing, and goes on doing nothing
            // until a person points a rollout at it.
            //
            // Both variants land here, and the *variant is the flag*: a
            // forced plan is one whose encoded bytes are identical to an
            // ordinary one's and whose signature is not, so `force` is set
            // before the check rather than after it. A plan signed as
            // ordinary and republished as forced fails that check and is
            // dropped, which is the point of separating the domains.
            OpBody::UpgradePlanPut { plan } | OpBody::UpgradePlanForce { plan } => {
                let force = matches!(body, OpBody::UpgradePlanForce { .. });
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &plan.created_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &plan.signing_bytes_forced(force),
                    &plan.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "upgrade plan dropped");
                    }
                    SignerCheck::Authorized => {
                        // `any` is refused at merge time and not only at the
                        // door it came through: a plan naming a whole mesh
                        // is the one thing §4 says must not exist, so no
                        // node accepts one however it was authored.
                        if crate::membership::Selector::parse(&plan.selector)
                            == crate::membership::Selector::Any
                        {
                            warn!("upgrade plan dropped: a plan may not name the whole mesh");
                        } else {
                            match state.upgrade_plans.get_mut(&plan.plan_id) {
                                // Immutable once written: an edit would let
                                // a plan a node already read change under
                                // it. Cancel and publish another.
                                Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                                None => {
                                    state.upgrade_plans.insert(
                                        plan.plan_id,
                                        UpgradePlanEntry {
                                            plan: (**plan).clone(),
                                            force,
                                            cancelled: None,
                                            updated_hlc: op.hlc,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // A release record is a *catalogue entry*, and appending one
            // grants nothing: it says where signed bytes for a version
            // live, and every node still verifies the release signature
            // itself before those bytes go anywhere near its own binary.
            OpBody::ReleasePublish { release } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &release.published_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &release.signing_bytes(),
                    &release.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(version = %release.version, why, "release publish dropped");
                    }
                    // First publish of a version wins. A version names one
                    // exact binary *per architecture* for the life of the
                    // mesh; letting a second record replace it would mean
                    // "0.6.0" meant different bytes depending on when a node
                    // asked. A second architecture is not a second binary
                    // for the same machines — it lands under its own key
                    // (`crate::release::release_key`).
                    SignerCheck::Authorized => {
                        let key = crate::release::release_key(&state.releases, release);
                        state
                            .releases
                            .entry(key)
                            .or_insert_with(|| (**release).clone());
                    }
                }
            }
            OpBody::UpgradePlanCancel {
                plan_id,
                by,
                at_micros,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &upgrade_cancel_signing_bytes(plan_id, by, *at_micros),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "upgrade plan cancel dropped");
                    }
                    SignerCheck::Authorized => match state.upgrade_plans.get_mut(plan_id) {
                        // First cancel wins; a withdrawal is terminal, for
                        // the same reason a revocation is.
                        Some(entry) => {
                            entry.cancelled.get_or_insert((*by, *at_micros));
                            entry.updated_hlc = op.hlc;
                        }
                        // The plan may simply not have arrived here yet, and
                        // a cancel that lands first must not be lost —
                        // otherwise a withdrawn rollout comes back on the
                        // slowest node in the mesh.
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            // Trusting a release key is an admin act, verified here as
            // every governance body is. What makes it safe to replicate is
            // that it authorizes nothing by itself: it answers "does this
            // mesh count that signature", and a release still needs an
            // admin-signed record *and* a binary signed by one of these.
            OpBody::ReleaseKeyPut { key } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &key.added_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &key.signing_bytes(),
                    &key.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "release key dropped");
                    }
                    SignerCheck::Authorized => {
                        // Shape-checked at merge as well as at the door it
                        // came through: a malformed key would verify
                        // nothing on every node at once, quietly.
                        if !ReleaseKey::looks_like_a_key(&key.pubkey) {
                            warn!(
                                "release key dropped: neither a minisign `RW…` line nor an SSH \
                                 public key"
                            );
                        } else {
                            match state.release_keys.get_mut(&key.pubkey) {
                                // Trust withdrawn, then granted again.
                                // Ordered by the record's *own* timestamp
                                // rather than by arrival: a `Put` that was
                                // already in flight when the revocation
                                // was signed is older than it and must not
                                // resurrect the key on the slowest node in
                                // the mesh, while an admin deliberately
                                // trusting it again afterwards is newer
                                // and must work. Revoking by accident is a
                                // mistake to correct, not a key to burn.
                                Some(entry)
                                    if entry
                                        .revoked
                                        .is_some_and(|(_, at)| key.added_at_micros <= at) =>
                                {
                                    return Ok(Some(ApplyOutcome::Ignored));
                                }
                                Some(entry) => {
                                    entry.revoked = None;
                                    entry.key = (**key).clone();
                                    entry.updated_hlc = op.hlc;
                                }
                                None => {
                                    state.release_keys.insert(
                                        key.pubkey.clone(),
                                        ReleaseKeyEntry {
                                            key: (**key).clone(),
                                            revoked: None,
                                            updated_hlc: op.hlc,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
            OpBody::ReleaseKeyRevoke {
                pubkey,
                by,
                at_micros,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &release_key_revoke_signing_bytes(pubkey, by, *at_micros),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "release key revocation dropped");
                    }
                    SignerCheck::Authorized => match state.release_keys.get_mut(pubkey) {
                        Some(entry) => {
                            entry.revoked.get_or_insert((*by, *at_micros));
                            entry.updated_hlc = op.hlc;
                        }
                        // The key may simply not have arrived here yet, and
                        // a revocation that lands first must not be lost —
                        // otherwise withdrawn trust comes back on the
                        // slowest node in the mesh.
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            OpBody::DeviceInventoryPut { inventory } => {
                // **Self-description, enforced.** A node may only say what
                // hardware *it* has: an operator elsewhere asserting a GPU
                // onto a box is how work lands where it cannot run, and it
                // is the same rule that rejects `cap.*` tags from a
                // user-supplied tag list. Unsigned, because it authorizes
                // nothing.
                if inventory.node_id != op.author {
                    return Err(CoreError::State(
                        "device inventory must be authored by the node it describes".into(),
                    ));
                }
                // LWW by the publish instant, so a stale republish after a
                // partition heals cannot resurrect a card that has since
                // been pulled out of the machine.
                match state.devices.get(&op.author) {
                    Some(cur) if cur.published_at_micros >= inventory.published_at_micros => {}
                    _ => {
                        state.devices.insert(op.author, (**inventory).clone());
                    }
                }
            }
            OpBody::UsageAdd { usage } => {
                // Grow-only per component, so delivery order, duplicates and
                // re-sends after a crash are all the same thing. The author
                // is the node that did the work; nothing here is signed by a
                // user because nothing here authorizes anything — it is a
                // number a node published about its own execution.
                state
                    .usage
                    .entry((op.author, usage.principal, usage.window))
                    .and_modify(|cur| cur.absorb(usage))
                    .or_insert(**usage);
            }
            // C0: the merge already happened in step 2, before the gapless
            // test — a checkpoint has to be understood ahead of its own
            // position in the log. Nothing left to do here but consume it,
            // and the arm is spelled out rather than folded into a wildcard
            // so that adding an entity kind without deciding how it merges
            // still fails to compile.
            OpBody::Checkpoint { .. } => {}
            OpBody::SingletonClaim {
                name,
                job,
                user,
                at_micros,
            } => {
                // LWW per name by HLC. Whether the claim *holds* is not
                // decided here: a claim naming a finished job is simply a
                // free name, and no tombstone is needed to say so.
                let fresher = state.singletons.get(name).is_none_or(|e| op.hlc > e.hlc);
                if fresher {
                    state.singletons.insert(
                        name.clone(),
                        SingletonEntry {
                            job: *job,
                            user: *user,
                            holder: op.author,
                            at_micros: *at_micros,
                            hlc: op.hlc,
                        },
                    );
                }
            }
            OpBody::SecretPut { record } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &record.created_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &record.signing_bytes(),
                    &record.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(secret = %record.name, why, "secret put dropped");
                    }
                    SignerCheck::Authorized => {
                        let wraps_of = |author: NodeId| {
                            record
                                .wraps
                                .iter()
                                .map(|w| ((w.node_id, author), w.clone()))
                                .collect::<BTreeMap<_, _>>()
                        };
                        let existing = state.secrets.get_mut(&record.name);
                        match existing
                            .as_ref()
                            .map(|e| secret_order(record).cmp(&secret_order(&e.record)))
                        {
                            Some(std::cmp::Ordering::Less) => {}
                            // The same signed record again, from this author
                            // or another: its wraps join the ones already
                            // here rather than replacing them.
                            Some(std::cmp::Ordering::Equal) => {
                                if let Some(entry) = existing {
                                    for (k, w) in wraps_of(op.author) {
                                        entry.sealed.entry(k).or_insert(w);
                                    }
                                    entry.rebuild_wraps();
                                }
                            }
                            Some(std::cmp::Ordering::Greater) | None => {
                                let removed_at_micros = existing.map_or(0, |e| e.removed_at_micros);
                                let mut entry = SecretEntry {
                                    record: record.clone(),
                                    removed: false,
                                    updated_hlc: op.hlc,
                                    removed_at_micros,
                                    sealed: wraps_of(op.author),
                                    heals: BTreeMap::new(),
                                };
                                entry.rebuild_wraps();
                                entry.settle_removed();
                                state.secrets.insert(record.name.clone(), entry);
                            }
                        }
                    }
                }
            }
            OpBody::SecretRemove { removal } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &removal.by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &removal.signing_bytes(),
                    &removal.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(secret = %removal.name, why, "secret remove dropped");
                    }
                    // Ordered by the signed `at_micros` against the record's
                    // signed `created_at_micros`, not by HLC: a removal
                    // re-authored later by some member must not take away a
                    // secret that was set again since (SEC-17).
                    SignerCheck::Authorized => match state.secrets.get_mut(&removal.name) {
                        Some(entry) => {
                            entry.removed_at_micros =
                                entry.removed_at_micros.max(removal.at_micros);
                            entry.settle_removed();
                        }
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            OpBody::SecretRewrap {
                name,
                version,
                wraps,
            } => {
                // Node-authored healing for late joiners, with no user
                // present. Any member may provide wraps, because nothing
                // here can check one and the recipient can (SEC-17); see
                // `SecretEntry::heals` for why it is one statement per
                // author rather than first-come-first-served.
                let Some(entry) = state.secrets.get(name) else {
                    return Ok(Some(ApplyOutcome::Parked));
                };
                if entry.record.version != *version {
                    // Against a superseded version: nothing to do.
                } else if wraps
                    .iter()
                    .filter(|w| w.node_id != op.author)
                    .any(|w| !state.members.contains_key(&w.node_id))
                {
                    // A recipient whose MemberAdd has not landed here yet:
                    // the author saw it, so it is coming.
                    return Ok(Some(ApplyOutcome::Parked));
                } else {
                    let asker = state.is_active_member(&op.author);
                    let statement = crate::secrets::read_statement(&op.author, wraps);
                    if let Some(entry) = state.secrets.get_mut(name) {
                        match statement {
                            // A current node: this op is everything it
                            // provides, and whether it is asking.
                            Some(wants) => {
                                let heal = HealStatement {
                                    wraps: wraps
                                        .iter()
                                        .filter(|w| w.node_id != op.author)
                                        .map(|w| (w.node_id, w.clone()))
                                        .collect(),
                                    wants: if asker { wants } else { 0 },
                                };
                                if heal.wraps.is_empty() && heal.wants == 0 {
                                    entry.heals.remove(&op.author);
                                } else {
                                    entry.heals.insert(op.author, heal);
                                }
                            }
                            // An older node's rewrap, which carried only
                            // the recipients missing at the time: added to,
                            // never restated.
                            None => {
                                let heal = entry.heals.entry(op.author).or_default();
                                for w in wraps.iter().filter(|w| w.node_id != op.author) {
                                    heal.wraps.entry(w.node_id).or_insert_with(|| w.clone());
                                }
                            }
                        }
                        entry.rebuild_wraps();
                    }
                }
            }
            OpBody::SecretScopeSet { scope } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &scope.by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &scope.signing_bytes(),
                    &scope.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(secret = %scope.name, why, "secret scope dropped");
                    }
                    // The newest by what the admin signed, so a scope
                    // re-sent later by some member changes nothing.
                    SignerCheck::Authorized => {
                        let newer = state.secret_scopes.get(&scope.name).is_none_or(|e| {
                            (scope.at_micros, scope.signature.as_slice())
                                > (e.at_micros, e.signature.as_slice())
                        });
                        if newer {
                            state
                                .secret_scopes
                                .insert(scope.name.clone(), (**scope).clone());
                        }
                    }
                }
            }
            // Founder only, not `ADMIN_ONLY`: ending the mesh for every
            // member is the one act no grant should be able to hand on. The
            // founder is whoever genesis names, so every node checks this
            // against nothing but its own copy — no user table, no parking.
            OpBody::MeshReset { reset } => {
                let genesis = &self.genesis.body;
                let refused = if reset.mesh_id != genesis.mesh_id {
                    Some("it names a different mesh")
                } else if reset.by.0 != genesis.founder_user {
                    Some("it is not signed by the founder")
                } else if !<[u8; 64]>::try_from(reset.signature.as_slice()).is_ok_and(|sig| {
                    verify_signature(&genesis.founder_user, &reset.signing_bytes(), &sig)
                }) {
                    Some("the founder's signature does not verify")
                } else {
                    None
                };
                match refused {
                    Some(why) => warn!(author = %op.author.short(), why, "mesh reset dropped"),
                    // The first one stands. A second is the same order
                    // again, and the node is already on its way out.
                    None if state.mesh_reset.is_none() => {
                        state.mesh_reset = Some(MeshResetEntry {
                            reset: reset.clone(),
                            author: op.author,
                            hlc: op.hlc,
                        });
                    }
                    None => {}
                }
            }
            OpBody::AuditAppend { entry } => {
                // A node may only append audit events as itself.
                if entry.node != op.author {
                    warn!(author = %op.author.short(), "audit entry dropped: node mismatch");
                } else {
                    state
                        .audit
                        .entry(entry.audit_id)
                        .or_insert_with(|| entry.clone());
                    // Only the newest rows stay in memory (ENG-3); the
                    // history index answers for the rest. Keeping the
                    // largest ids rather than the latest arrivals makes the
                    // tail the same on every node whatever order the rows
                    // came in.
                    let resident = self
                        .audit_resident
                        .load(std::sync::atomic::Ordering::Relaxed);
                    while state.audit.len() > resident {
                        state.audit.pop_first();
                    }
                    self.mark_audit(op.author, op.seq);
                }
            }
            // -- M5 fan-out ownership (MASTERPLAN §10.3) ----------------------
            OpBody::FanoutPlan { plan } => {
                // Only the origin may publish its own plan; first write wins
                // (the plan is immutable — progress lives in job summaries).
                if op.author != plan.submitter {
                    warn!(group = %plan.group.short(), "fan-out plan dropped: author mismatch");
                } else {
                    state
                        .fanouts
                        .entry(plan.group)
                        .or_insert_with(|| FanoutEntry {
                            plan: (**plan).clone(),
                            adopters: BTreeSet::new(),
                            adopted: None,
                            closed: None,
                            closed_at_micros: None,
                        });
                }
            }
            // -- C4 gangs (C0-C9 §8): the fan-out ownership triple again ------
            OpBody::GangPlan { plan } => {
                // Only the origin may publish its own plan; first write
                // wins. The plan is immutable — whether the gang started
                // lives in the members' job summaries, and how it ended
                // lives in `GangClosed`.
                if op.author != plan.submitter {
                    warn!(gang = %plan.gang.short(), "gang plan dropped: author mismatch");
                } else if plan.members.is_empty() {
                    // A gang with no members is not a gang, and admitting
                    // one would give the adoption scan a plan it can never
                    // finish driving.
                    warn!(gang = %plan.gang.short(), "gang plan dropped: no members");
                } else {
                    state.gangs.entry(plan.gang).or_insert_with(|| GangEntry {
                        plan: (**plan).clone(),
                        adopters: BTreeSet::new(),
                        adopted: None,
                        closed: None,
                        closed_at_micros: None,
                    });
                }
            }
            OpBody::GangAdopt { gang, by } => {
                if op.author != *by {
                    warn!(gang = %gang.short(), "gang adoption dropped: author mismatch");
                } else {
                    let Some(entry) = state.gangs.get_mut(gang) else {
                        return Ok(Some(ApplyOutcome::Parked)); // plan not seen yet
                    };
                    entry.adopters.insert(*by);
                    // Freshest claim owns the gang (LWW by HLC). Racing
                    // adopters are harmless: a re-drive re-offers, and an
                    // offer is idempotent per gang on every holder.
                    if entry.adopted.is_none_or(|(_, hlc)| op.hlc > hlc) {
                        entry.adopted = Some((*by, op.hlc));
                    }
                }
            }
            OpBody::GangClosed {
                gang,
                outcome,
                rounds,
            } => {
                let Some(entry) = state.gangs.get_mut(gang) else {
                    return Ok(Some(ApplyOutcome::Parked));
                };
                if op.author != entry.plan.submitter && !entry.adopters.contains(&op.author) {
                    warn!(gang = %gang.short(), "gang close dropped: not an owner");
                } else {
                    if entry.closed.is_none() {
                        entry.closed = Some((*outcome, *rounds));
                    }
                    entry.closed_at_micros = Some(
                        entry
                            .closed_at_micros
                            .map_or(op.hlc.wall_micros, |at| at.min(op.hlc.wall_micros)),
                    );
                }
            }
            OpBody::FanoutAdopt { group, by } => {
                if op.author != *by {
                    warn!(group = %group.short(), "adoption dropped: author mismatch");
                } else {
                    let Some(entry) = state.fanouts.get_mut(group) else {
                        return Ok(Some(ApplyOutcome::Parked)); // plan not seen yet
                    };
                    entry.adopters.insert(*by);
                    // Freshest claim owns the group (LWW by HLC). Racing
                    // adopters are harmless: dispatch is idempotent by job id.
                    if entry.adopted.is_none_or(|(_, hlc)| op.hlc > hlc) {
                        entry.adopted = Some((*by, op.hlc));
                    }
                }
            }
            OpBody::FanoutClosed {
                group,
                succeeded,
                failed,
                skipped,
            } => {
                let Some(entry) = state.fanouts.get_mut(group) else {
                    return Ok(Some(ApplyOutcome::Parked));
                };
                // Only the origin or a recorded adopter may close. An
                // adopter's FanoutAdopt precedes its FanoutClosed in its own
                // gapless op sequence, so this check is deterministic.
                if op.author != entry.plan.submitter && !entry.adopters.contains(&op.author) {
                    warn!(group = %group.short(), "fan-out close dropped: not an owner");
                } else {
                    if entry.closed.is_none() {
                        entry.closed = Some(FanoutOutcome {
                            succeeded: *succeeded,
                            failed: *failed,
                            skipped: *skipped,
                        });
                    }
                    entry.closed_at_micros = Some(
                        entry
                            .closed_at_micros
                            .map_or(op.hlc.wall_micros, |at| at.min(op.hlc.wall_micros)),
                    );
                }
            }
            // -- M5 Git sources (MASTERPLAN §8.1) -----------------------------
            OpBody::SourceAdd { source } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &source.created_by,
                    rbac::CONTENT,
                    op.hlc.wall_micros,
                    &source.signing_bytes(),
                    &source.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(source = %source.name, why, "git source dropped");
                    }
                    SignerCheck::Authorized => {
                        // Signed by somebody entitled to author it, which
                        // says nothing about whether a node should run
                        // `git clone` on it (S5). The name becomes a
                        // directory and the url becomes an argument, both in
                        // the daemon process; a client that skipped its own
                        // check does not get to skip this one.
                        let shape = source.validate();
                        // The bump list pre-authorizes future service
                        // versions, so the creator's grants must cover every
                        // bumped service — same bar as creating them by hand.
                        let covered = source.bump.iter().all(|svc| {
                            rbac::can_manage_service(
                                state,
                                &founder,
                                &source.created_by,
                                svc,
                                op.hlc.wall_micros,
                            )
                        });
                        if let Err(why) = shape {
                            warn!(source = %source.name, why, "git source dropped");
                        } else if !covered {
                            warn!(source = %source.name,
                                "git source dropped: grant does not cover a bumped service");
                        } else {
                            match state.sources.get_mut(&source.name) {
                                Some(entry) if op.hlc <= entry.updated_hlc => {}
                                Some(entry) => {
                                    entry.source = source.clone();
                                    entry.removed = false;
                                    entry.updated_hlc = op.hlc;
                                }
                                None => {
                                    state.sources.insert(
                                        source.name.clone(),
                                        SourceEntry {
                                            source: source.clone(),
                                            removed: false,
                                            updated_hlc: op.hlc,
                                            last: None,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
            OpBody::SourceRemove {
                name,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::CONTENT,
                    op.hlc.wall_micros,
                    &mesh_proto::source_remove_signing_bytes(name, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(source = %name, why, "git source remove dropped");
                    }
                    SignerCheck::Authorized => match state.sources.get_mut(name) {
                        Some(entry) => {
                            if op.hlc > entry.updated_hlc {
                                entry.removed = true;
                                entry.updated_hlc = op.hlc;
                            }
                        }
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            OpBody::SourceSynced {
                name,
                commit,
                bundle_id,
            } => {
                // Node-authored status from whichever watcher synced; the
                // bundle itself was announced through the normal content ops.
                let Some(entry) = state.sources.get_mut(name) else {
                    return Ok(Some(ApplyOutcome::Parked));
                };
                if !entry.removed && entry.last.as_ref().is_none_or(|l| op.hlc > l.at) {
                    entry.last = Some(SourceSyncStatus {
                        commit: commit.clone(),
                        bundle_id: *bundle_id,
                        by: op.author,
                        at: op.hlc,
                    });
                }
            }
            OpBody::ReleaseCosign { cosign } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &cosign.published_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &cosign.signing_bytes(),
                    &cosign.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "release co-signature dropped");
                    }
                    SignerCheck::Authorized => {
                        // The admin signature says who published it. The
                        // *authority* is the SSH key inside the document,
                        // checked here against what this mesh trusts —
                        // which is why an admin publishing a co-signature
                        // from an untrusted key records nothing.
                        let trusted = state.trusted_ssh_release_keys();
                        let Some((line, present)) =
                            crate::sshsig::match_trusted(&cosign.armored, &trusted)
                        else {
                            warn!(
                                digest = %cosign.key(),
                                "release co-signature not recorded: no release key this mesh \
                                 trusts made it"
                            );
                            return Ok(Some(ApplyOutcome::Applied));
                        };
                        let key = cosign.key();
                        match state.release_cosignatures.get_mut(&key) {
                            Some(entry) if op.hlc > entry.updated_hlc => {
                                entry.cosign = (**cosign).clone();
                                entry.signed_by_key = line;
                                entry.user_present = present;
                                entry.updated_hlc = op.hlc;
                            }
                            Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                            None => {
                                state.release_cosignatures.insert(
                                    key,
                                    CosignEntry {
                                        cosign: (**cosign).clone(),
                                        signed_by_key: line,
                                        user_present: present,
                                        updated_hlc: op.hlc,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            OpBody::AttestRootAdd { root } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &root.added_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &root.signing_bytes(),
                    &root.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "attestation root dropped");
                    }
                    SignerCheck::Authorized => {
                        let id = root.id();
                        match state.attest_roots.get_mut(&id) {
                            Some(entry) if op.hlc > entry.updated_hlc => {
                                entry.root = (**root).clone();
                                // Re-adding an authority that was removed
                                // trusts it again: the id is the
                                // certificate, so this is the same
                                // certificate being trusted a second time,
                                // deliberately.
                                entry.removed = false;
                                entry.updated_hlc = op.hlc;
                            }
                            Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                            None => {
                                state.attest_roots.insert(
                                    id,
                                    AttestRootEntry {
                                        root: (**root).clone(),
                                        removed: false,
                                        updated_hlc: op.hlc,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            OpBody::AttestRootRemove {
                id,
                by,
                at_micros,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &mesh_proto::attest_root_remove_signing_bytes(id, by, *at_micros),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(root = %id, why, "attestation root removal dropped");
                    }
                    SignerCheck::Authorized => match state.attest_roots.get_mut(id) {
                        Some(entry) if op.hlc > entry.updated_hlc => {
                            entry.removed = true;
                            entry.updated_hlc = op.hlc;
                        }
                        Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                        // Removing a root that never arrived: park, because
                        // the add may still be behind it and a removal that
                        // vanished would silently re-trust it.
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            OpBody::UserHardwareAttest { attestation } => {
                let founder = self.genesis.body.founder_user;
                // Self-signed, and that is the control: an admin who could
                // attest into somebody else's account would be able to
                // manufacture the very column that constrains them.
                match check_user_signed(
                    state,
                    &founder,
                    &attestation.user_id,
                    rbac::READ,
                    op.hlc.wall_micros,
                    &attestation.signing_bytes(),
                    &attestation.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "hardware attestation dropped");
                    }
                    SignerCheck::Authorized => {
                        let roots = state.trusted_attest_roots();
                        let attested = match crate::attest::verify(attestation, &roots) {
                            Ok(a) => a,
                            Err(refusal) => {
                                // Not an error about the person: their key,
                                // their grants and their access are
                                // untouched. What they do not get is the
                                // column.
                                warn!(
                                    user = %attestation.user_id.short(),
                                    why = %refusal.message(),
                                    "hardware attestation not recorded"
                                );
                                return Ok(Some(ApplyOutcome::Applied));
                            }
                        };
                        match state.attestations.get_mut(&attestation.user_id) {
                            Some(entry) if op.hlc > entry.updated_hlc => {
                                entry.attestation = (**attestation).clone();
                                entry.attested = attested;
                                entry.revoked = false;
                                entry.revoked_reason = String::new();
                                entry.updated_hlc = op.hlc;
                            }
                            Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                            None => {
                                state.attestations.insert(
                                    attestation.user_id,
                                    AttestationEntry {
                                        attestation: (**attestation).clone(),
                                        attested,
                                        revoked: false,
                                        revoked_reason: String::new(),
                                        updated_hlc: op.hlc,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            OpBody::UserHardwareRevoke { revocation } => {
                let founder = self.genesis.body.founder_user;
                // The owner, or an admin. An attestation nobody can
                // withdraw because its owner left is a column that lies.
                let allowed = if revocation.revoked_by == revocation.user_id {
                    rbac::READ
                } else {
                    rbac::ADMIN_ONLY
                };
                match check_user_signed(
                    state,
                    &founder,
                    &revocation.revoked_by,
                    allowed,
                    op.hlc.wall_micros,
                    &revocation.signing_bytes(),
                    &revocation.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why,
                            "hardware attestation revoke dropped");
                    }
                    SignerCheck::Authorized => {
                        match state.attestations.get_mut(&revocation.user_id) {
                            Some(entry) if op.hlc > entry.updated_hlc => {
                                entry.revoked = true;
                                entry.revoked_reason = revocation.reason.clone();
                                entry.updated_hlc = op.hlc;
                            }
                            Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                            None => return Ok(Some(ApplyOutcome::Parked)),
                        }
                    }
                }
            }
            // -- M17 the fabric (§3.4) ---------------------------------
            //
            // Admin-signed and verified here on every node, exactly as a
            // change-control policy is. The one difference worth naming:
            // this entity only ever *grants*, and sessions are refused
            // without a grant — so a node that drops one of these (a bad
            // signature, an author who is not an admin) refuses terminals
            // rather than allowing them. Every failure path is closed.
            OpBody::SessionPolicyPut { policy } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &policy.created_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &policy.signing_bytes(),
                    &policy.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), policy = %policy.id, why,
                            "session policy dropped");
                    }
                    SignerCheck::Authorized => match state.session_policies.get_mut(&policy.id) {
                        Some(entry) if op.hlc > entry.updated_hlc => {
                            entry.policy = (**policy).clone();
                            entry.removed = false;
                            entry.updated_hlc = op.hlc;
                        }
                        Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                        None => {
                            state.session_policies.insert(
                                policy.id.clone(),
                                SessionPolicyEntry {
                                    policy: (**policy).clone(),
                                    removed: false,
                                    updated_hlc: op.hlc,
                                },
                            );
                        }
                    },
                }
            }
            OpBody::SessionPolicyRemove { id, by, signature } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &mesh_proto::session_policy_remove_signing_bytes(id, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(policy = %id, why, "session policy remove dropped");
                    }
                    SignerCheck::Authorized => match state.session_policies.get_mut(id) {
                        Some(entry) => {
                            if op.hlc > entry.updated_hlc {
                                entry.removed = true;
                                entry.updated_hlc = op.hlc;
                            }
                        }
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            // -- M31 B1 sinks (M31 §12) -----------------------------------
            //
            // `SchedulePut`'s shape verbatim, with one difference that is
            // the point of the phase: `rbac::ADMIN_ONLY` rather than the
            // broader schedule set. A schedule runs work the mesh already
            // permits; a sink copies every command line, every submitter
            // identity and — once B2 lands — every byte of stdout to a host
            // an operator typed into a text box.
            OpBody::SinkPut { sink } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &sink.created_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &sink.signing_bytes(),
                    &sink.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(sink = %sink.name, why, "sink dropped");
                    }
                    SignerCheck::Authorized => {
                        // Re-checked on every applier, not only where it was
                        // created: a literal credential must never reach a
                        // replicated record, whoever signed it (C6).
                        if let Some(why) = sink.refusal() {
                            warn!(sink = %sink.name, why, "sink refused");
                        } else {
                            match state.sinks.get_mut(&sink.name) {
                                Some(entry) if op.hlc <= entry.updated_hlc => {}
                                Some(entry) => {
                                    entry.sink = (**sink).clone();
                                    entry.removed = false;
                                    entry.updated_hlc = op.hlc;
                                }
                                None => {
                                    state.sinks.insert(
                                        sink.name.clone(),
                                        SinkEntry {
                                            sink: (**sink).clone(),
                                            removed: false,
                                            updated_hlc: op.hlc,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // M31 B2 §19 Q3. Node-authored and unsigned; merged by **max**,
            // which is what makes it converge without coordination — a
            // duplicate delivery, an out-of-order arrival or a re-send
            // after a crash can never move it backwards, exactly as
            // `UsagePartial` argues for its own counters.
            OpBody::SinkProgress {
                sink,
                exported_through_micros,
            } => {
                let entry = state.sink_progress.entry(sink.clone()).or_default();
                *entry = (*entry).max(*exported_through_micros);
            }
            OpBody::SinkRemove {
                name,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &mesh_proto::sink_remove_signing_bytes(name, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(sink = %name, why, "sink remove dropped");
                    }
                    SignerCheck::Authorized => match state.sinks.get_mut(name) {
                        Some(entry) => {
                            if op.hlc > entry.updated_hlc {
                                entry.removed = true;
                                entry.updated_hlc = op.hlc;
                            }
                        }
                        None => return Ok(Some(ApplyOutcome::Parked)),
                    },
                }
            }
            // -- M31 A1 retention (M31 §9) --------------------------------
            //
            // Admin-signed, LWW by HLC, one record for the whole mesh — the
            // `policy.create` shape exactly. The audit floor is re-checked
            // *here*, on every node that applies the op, and not only in
            // the tool that wrote it: a floor the writer enforces is a
            // floor, and a floor every applier enforces is a rule.
            OpBody::RetentionPut { policy } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &policy.created_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &policy.signing_bytes(),
                    &policy.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "retention policy dropped");
                    }
                    SignerCheck::Authorized => {
                        if let Some(why) = policy.refusal() {
                            // Signed by an admin and still refused. Dropped
                            // rather than parked: parking waits for the
                            // world to change, and nothing about this op
                            // will ever become acceptable.
                            warn!(why, "retention policy refused by the floor");
                        } else if op.hlc > state.retention.updated_hlc {
                            state.retention.policy = (**policy).clone();
                            state.retention.updated_hlc = op.hlc;
                        }
                    }
                }
            }
            OpBody::RetentionReset {
                by,
                at_micros,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &mesh_proto::retention_reset_signing_bytes(by, *at_micros),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(why, "retention reset dropped");
                    }
                    SignerCheck::Authorized => {
                        if op.hlc > state.retention.updated_hlc {
                            // Back to the shipped defaults, which are
                            // finite — a reset loosens the window, it never
                            // removes one.
                            state.retention = RetentionEntry {
                                policy: mesh_proto::RetentionPolicy::default(),
                                updated_hlc: op.hlc,
                            };
                        }
                    }
                }
            }
            // -- C2 deployments (C0-C9 §6) -------------------------------
            //
            // Governed exactly as a schedule is (`rbac::DEPLOYMENTS`, LWW by
            // HLC per name, tombstone via `DeploymentRemove`) — a deployment
            // grants no new power, so the door is the door for running the
            // service it names once.
            OpBody::DeploymentPut { deployment } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &deployment.created_by,
                    rbac::DEPLOYMENTS,
                    op.hlc.wall_micros,
                    &deployment.signing_bytes(),
                    &deployment.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(deployment = %deployment.name, why, "deployment dropped");
                    }
                    SignerCheck::Authorized => match state.deployments.get_mut(&deployment.name) {
                        Some(entry) if op.hlc <= entry.updated_hlc => {}
                        Some(entry) => {
                            entry.deployment = (**deployment).clone();
                            entry.removed = false;
                            entry.updated_hlc = op.hlc;
                        }
                        None => {
                            // A removal applied with nothing to remove still
                            // counts: a put from before it has lost.
                            let stale = state
                                .deployment_tombstones
                                .get(&deployment.name)
                                .is_some_and(|removed| op.hlc <= *removed);
                            if !stale {
                                state.deployment_tombstones.remove(&deployment.name);
                                state.deployments.insert(
                                    deployment.name.clone(),
                                    DeploymentEntry {
                                        deployment: (**deployment).clone(),
                                        removed: false,
                                        updated_hlc: op.hlc,
                                    },
                                );
                            }
                        }
                    },
                }
            }
            // N1 P5. Governed exactly as a deployment is (`rbac::SCHEDULES`,
            // LWW by HLC per name, tombstone via the remove) — the record
            // is a standing authorization to run one service, which is the
            // power the creator already had.
            OpBody::ServiceSchedulePut { schedule } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &schedule.created_by,
                    rbac::SCHEDULES,
                    op.hlc.wall_micros,
                    &schedule.signing_bytes(),
                    &schedule.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(schedule = %schedule.name, why, "service schedule dropped");
                    }
                    SignerCheck::Authorized => {
                        match state.service_schedules.get_mut(&schedule.name) {
                            Some(entry) if op.hlc <= entry.updated_hlc => {}
                            Some(entry) => {
                                entry.schedule = (**schedule).clone();
                                entry.removed = false;
                                entry.updated_hlc = op.hlc;
                            }
                            None => {
                                state.service_schedules.insert(
                                    schedule.name.clone(),
                                    ServiceScheduleEntry {
                                        schedule: (**schedule).clone(),
                                        removed: false,
                                        updated_hlc: op.hlc,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            OpBody::ServiceScheduleRemove {
                name,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::SCHEDULES,
                    op.hlc.wall_micros,
                    &mesh_proto::service_schedule_remove_signing_bytes(name, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(schedule = %name, why, "service schedule remove dropped");
                    }
                    SignerCheck::Authorized => {
                        if let Some(entry) = state.service_schedules.get_mut(name)
                            && op.hlc > entry.updated_hlc
                        {
                            entry.removed = true;
                            entry.updated_hlc = op.hlc;
                        }
                    }
                }
            }
            // N1 P3/P4. Governed exactly as the deployment is
            // (`rbac::DEPLOYMENTS`, LWW by HLC per name): the policy says
            // how carefully the deployment rolls, which is strictly less
            // power than saying what it runs.
            OpBody::DeploymentPolicyPut { policy } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &policy.set_by,
                    rbac::DEPLOYMENTS,
                    op.hlc.wall_micros,
                    &policy.signing_bytes(),
                    &policy.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(deployment = %policy.deployment, why, "deployment policy dropped");
                    }
                    SignerCheck::Authorized => {
                        match state.deployment_policies.get_mut(&policy.deployment) {
                            Some(entry) if op.hlc > entry.updated_hlc => {
                                entry.policy = (**policy).clone();
                                entry.updated_hlc = op.hlc;
                            }
                            Some(_) => return Ok(Some(ApplyOutcome::Ignored)),
                            None => {
                                state.deployment_policies.insert(
                                    policy.deployment.clone(),
                                    DeploymentPolicyEntry {
                                        policy: (**policy).clone(),
                                        updated_hlc: op.hlc,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            OpBody::DeploymentRemove {
                name,
                by,
                signature,
            } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    by,
                    rbac::DEPLOYMENTS,
                    op.hlc.wall_micros,
                    &mesh_proto::deployment_remove_signing_bytes(name, by),
                    signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(deployment = %name, why, "deployment remove dropped");
                    }
                    SignerCheck::Authorized => {
                        match state.deployments.get_mut(name) {
                            Some(entry) => {
                                if op.hlc > entry.updated_hlc {
                                    entry.removed = true;
                                    entry.updated_hlc = op.hlc;
                                }
                            }
                            // **Not parked (2026-09-14).** A removal with
                            // nothing to remove used to wait for its put. That
                            // wait was safe while every put stayed in the log;
                            // compaction drops a deployment's superseded puts
                            // and keeps its removals, so after put → remove →
                            // put the put it waited for is gone for good — and
                            // replay is gapless per author, so every later op
                            // this node had written parked behind it. On
                            // restart the deployments map came back empty, and
                            // the next op the node authored reused a sequence
                            // number already on disk. LWW never needed the
                            // wait: remembering when the name was removed
                            // decides a late, older put just as well.
                            None => {
                                let at = state
                                    .deployment_tombstones
                                    .entry(name.clone())
                                    .or_insert(op.hlc);
                                if op.hlc > *at {
                                    *at = op.hlc;
                                }
                            }
                        }
                        // A floor whether or not this removal won: a put
                        // after it is a new deployment, and the versions and
                        // tags from before it were the old one's.
                        state
                            .deployment_history
                            .entry(name.clone())
                            .or_default()
                            .raise_floor(op.hlc);
                        if state
                            .deployment_promotions
                            .get(name)
                            .is_some_and(|p| p.updated_hlc <= op.hlc)
                        {
                            state.deployment_promotions.remove(name);
                        }
                    }
                }
            }
            // -- N3 P1 the service registry ------------------------------
            //
            // Node-authored about its *own* instances only, unsigned, LWW by
            // the publish instant — `DeploymentHealthPut` verbatim, and for
            // the same reasons. A node can only ever say where its own work
            // is (it allocated the port) and how its own work is doing (a
            // check from elsewhere measures the network more than the
            // service), so `op.author` is the key and there is nothing to
            // authorize: a node claiming another node's instances would be
            // claiming something it has no way to know.
            //
            // A stale republish after a partition heals cannot talk a
            // registration backwards past a newer one.
            OpBody::ShedPut { shed } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &shed.created_by,
                    rbac::ADMIN_ONLY,
                    op.hlc.wall_micros,
                    &shed.signing_bytes(),
                    &shed.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), why, "shed record dropped");
                    }
                    SignerCheck::Authorized => match &state.shed {
                        Some(entry) if op.hlc <= entry.updated_hlc => {
                            return Ok(Some(ApplyOutcome::Ignored));
                        }
                        _ => {
                            state.shed = Some(ShedEntry {
                                shed: (**shed).clone(),
                                updated_hlc: op.hlc,
                            });
                        }
                    },
                }
            }
            // -- Phase B, E1 deployment history ---------------------------
            //
            // Both halves are checked exactly as their own ops are, because
            // a version is only ever the two records a user already signed:
            // a node cannot write history nobody published.
            OpBody::DeploymentVersionPut { version } => {
                let founder = self.genesis.body.founder_user;
                let dep = &version.deployment;
                let policy = &version.policy;
                let checks = [
                    check_user_signed(
                        state,
                        &founder,
                        &dep.created_by,
                        rbac::DEPLOYMENTS,
                        op.hlc.wall_micros,
                        &dep.signing_bytes(),
                        &dep.signature,
                    ),
                    check_user_signed(
                        state,
                        &founder,
                        &policy.set_by,
                        rbac::DEPLOYMENTS,
                        op.hlc.wall_micros,
                        &policy.signing_bytes(),
                        &policy.signature,
                    ),
                ];
                if checks.iter().any(|c| matches!(c, SignerCheck::Park)) {
                    return Ok(Some(ApplyOutcome::Parked));
                }
                if let Some(SignerCheck::Reject(why)) =
                    checks.iter().find(|c| matches!(c, SignerCheck::Reject(_)))
                {
                    warn!(deployment = %dep.name, why, "deployment version dropped");
                } else if policy.deployment != dep.name {
                    warn!(deployment = %dep.name, "deployment version pairs another deployment's policy; dropped");
                } else {
                    state
                        .deployment_history
                        .entry(dep.name.clone())
                        .or_default()
                        .record(version, op.hlc);
                }
            }
            // Phase B, E2. Governed as the policy is: whoever may edit the
            // deployment. A promotion is scoped by the removal floor, so one
            // made for a deployment since removed cannot promote the canaries
            // of a new one that reuses the name and the generation number.
            OpBody::DeploymentKnobsPut { knobs } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &knobs.set_by,
                    rbac::DEPLOYMENTS,
                    op.hlc.wall_micros,
                    &knobs.signing_bytes(),
                    &knobs.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(deployment = %knobs.deployment, why, "deployment knobs dropped");
                    }
                    SignerCheck::Authorized => {
                        match state.deployment_knobs.get(&knobs.deployment) {
                            Some(cur) if cur.updated_hlc >= op.hlc => {
                                return Ok(Some(ApplyOutcome::Ignored));
                            }
                            _ => {
                                state.deployment_knobs.insert(
                                    knobs.deployment.clone(),
                                    DeploymentKnobsEntry {
                                        knobs: (**knobs).clone(),
                                        updated_hlc: op.hlc,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            OpBody::DeploymentPromote { promotion } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &promotion.by,
                    rbac::DEPLOYMENTS,
                    op.hlc.wall_micros,
                    &promotion.signing_bytes(),
                    &promotion.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(deployment = %promotion.deployment, why, "deployment promotion dropped");
                    }
                    SignerCheck::Authorized => {
                        let floor = state
                            .deployment_history
                            .get(&promotion.deployment)
                            .map(|h| h.floor)
                            .unwrap_or_default();
                        let stale = op.hlc <= floor
                            || state
                                .deployment_promotions
                                .get(&promotion.deployment)
                                .is_some_and(|p| p.updated_hlc >= op.hlc);
                        if stale {
                            return Ok(Some(ApplyOutcome::Ignored));
                        }
                        state.deployment_promotions.insert(
                            promotion.deployment.clone(),
                            DeploymentPromotionEntry {
                                promotion: (**promotion).clone(),
                                updated_hlc: op.hlc,
                            },
                        );
                    }
                }
            }
            OpBody::DeploymentTagPut { tag } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &tag.set_by,
                    rbac::DEPLOYMENTS,
                    op.hlc.wall_micros,
                    &tag.signing_bytes(),
                    &tag.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(deployment = %tag.deployment, why, "deployment tag dropped");
                    }
                    SignerCheck::Authorized => {
                        let foreign = tag
                            .version
                            .as_ref()
                            .is_some_and(|v| v.deployment.name != tag.deployment);
                        if !mesh_proto::is_valid_tag_name(&tag.tag) || foreign {
                            warn!(deployment = %tag.deployment, "deployment tag malformed; dropped");
                            return Ok(Some(ApplyOutcome::Ignored));
                        }
                        let history = state
                            .deployment_history
                            .entry(tag.deployment.clone())
                            .or_default();
                        if op.hlc <= history.floor
                            || history
                                .tags
                                .get(&tag.tag)
                                .is_some_and(|t| t.updated_hlc >= op.hlc)
                        {
                            return Ok(Some(ApplyOutcome::Ignored));
                        }
                        history.tags.insert(
                            tag.tag.clone(),
                            DeploymentTagEntry {
                                tag: (**tag).clone(),
                                updated_hlc: op.hlc,
                            },
                        );
                    }
                }
            }
            OpBody::RegistryPut { registrations } => match state.registry.get(&op.author) {
                Some(cur) if cur.published_at_micros >= registrations.published_at_micros => {}
                _ => {
                    state.registry.insert(op.author, (**registrations).clone());
                }
            },
            OpBody::DeploymentHealthPut { health } => {
                // Node-authored about its own instances only, unsigned, LWW
                // by the publish instant — a stale republish after a
                // partition heals cannot talk an instance's health
                // backwards past a newer reading.
                match state.deployment_health.get(&op.author) {
                    Some(cur) if cur.published_at_micros >= health.published_at_micros => {}
                    _ => {
                        state
                            .deployment_health
                            .insert(op.author, (**health).clone());
                    }
                }
            }
            // -- M18 facts (M17-M21 plan §4) ------------------------------
            //
            // `rbac::COLLECTORS`: deciding what a node reports about itself
            // is operational data, not governance. LWW by HLC per name, no
            // tombstone; see `FactCollectorEntry`.
            OpBody::FactCollectorPut { collector } => {
                let founder = self.genesis.body.founder_user;
                match check_user_signed(
                    state,
                    &founder,
                    &collector.created_by,
                    rbac::COLLECTORS,
                    op.hlc.wall_micros,
                    &collector.signing_bytes(),
                    &collector.signature,
                ) {
                    SignerCheck::Park => return Ok(Some(ApplyOutcome::Parked)),
                    SignerCheck::Reject(why) => {
                        warn!(author = %op.author.short(), collector = %collector.name, why,
                            "fact collector dropped");
                    }
                    SignerCheck::Authorized => match state.collectors.get_mut(&collector.name) {
                        Some(entry) if op.hlc <= entry.updated_hlc => {
                            return Ok(Some(ApplyOutcome::Ignored));
                        }
                        Some(entry) => {
                            entry.collector = (**collector).clone();
                            entry.updated_hlc = op.hlc;
                        }
                        None => {
                            state.collectors.insert(
                                collector.name.clone(),
                                FactCollectorEntry {
                                    collector: (**collector).clone(),
                                    updated_hlc: op.hlc,
                                },
                            );
                        }
                    },
                }
            }
            // A machine describing itself is not a governance act, so this
            // body carries no user signature (§4.4). What replaces it is
            // `MemberUpdate`'s rule — **only the subject may speak for the
            // subject** — extended to hosts by M12's vantage-point rule:
            // for a host, the author must be a node the host's
            // `reachable_from` selector admits.
            //
            // A node that fails the check has its op applied as a *no-op*,
            // not parked: parking blocks the gapless per-author watermark,
            // and one node publishing about another must never be able to
            // wedge its own sync.
            OpBody::FactsPublish {
                subject,
                collector,
                doc,
                digest,
                collected_at_micros,
            } => {
                // Every refusal below falls through to the end of the arm,
                // which means the op is **applied as a no-op**: the
                // watermark advances, state is untouched, and a loud
                // warning is emitted. That is the module's existing rule
                // for an unauthorized governance body, and it is
                // load-bearing here for the same reason — parking or
                // ignoring an op a node keeps re-sending would let one
                // misbehaving publisher wedge its own stream on every
                // other node forever.
                let refuse = |why: &str| {
                    warn!(author = %op.author.short(), subject = %subject.label(),
                        collector = %collector, why, "fact publish dropped");
                };
                if doc.len() > mesh_proto::MAX_FACT_DOC {
                    refuse("document is over the cap");
                } else if blake3::hash(doc).as_bytes() != digest {
                    refuse("document does not match its digest");
                } else {
                    match may_publish_facts(state, &op.author, subject) {
                        FactAuthority::Park => return Ok(Some(ApplyOutcome::Parked)),
                        FactAuthority::Reject(why) => refuse(why),
                        FactAuthority::Allowed => {
                            match std::str::from_utf8(doc).ok().and_then(|text| {
                                serde_json::from_str::<serde_json::Value>(text)
                                    .ok()
                                    .map(|parsed| (text, parsed))
                            }) {
                                None => refuse("document is not JSON"),
                                Some((text, parsed)) => {
                                    let by_collector =
                                        state.facts.entry(subject.clone()).or_default();
                                    match by_collector.get_mut(collector) {
                                        // Stale by LWW — genuinely ignorable,
                                        // and the only path here that is.
                                        Some(entry) if op.hlc <= entry.updated_hlc => {
                                            return Ok(Some(ApplyOutcome::Ignored));
                                        }
                                        Some(entry) => {
                                            entry.doc = text.to_string();
                                            entry.digest = *digest;
                                            entry.collected_at_micros = *collected_at_micros;
                                            entry.author = op.author;
                                            entry.updated_hlc = op.hlc;
                                            entry.parsed = parsed;
                                        }
                                        None => {
                                            by_collector.insert(
                                                collector.clone(),
                                                FactEntry {
                                                    doc: text.to_string(),
                                                    digest: *digest,
                                                    collected_at_micros: *collected_at_micros,
                                                    author: op.author,
                                                    updated_hlc: op.hlc,
                                                    parsed,
                                                },
                                            );
                                        }
                                    }
                                    self.mark_fact(subject.label(), collector.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }
}

/// Ops that keep the mesh working, exempt from the rate limiter (M31 A3).
///
/// Matched on `OpBody::kind()` rather than on the variant, because this is
/// a *policy* list and not a classification: the right default for an op
/// nobody has thought about is "subject to the limit", and a new control op
/// added later being limited at sixty a minute is survivable in a way that
/// a silently-exempt new op kind is not.
///
/// Everything here is authored rarely and load-bearing when it is:
/// membership and revocation (a join or an eviction must not be refused
/// because something else was noisy), compaction checkpoints (refusing one
/// makes the log grow, which is the opposite of the point), and the
/// retention policy itself (the op an operator publishes *to fix* a growth
/// problem must not be refused by the growth problem).
fn is_keepalive_op(body: &OpBody) -> bool {
    KEEPALIVE_KINDS.contains(&body.kind())
}

/// Kinds the op-rate limiter never refuses because refusing them loses
/// something that cannot be had back (SEC-6): an audit row the limiter
/// drops is an action nobody can account for. What keeps the audit trail
/// bounded is `crate::audit`'s folding of repeated denials, not a budget.
pub const UNLIMITED_KINDS: &[&str] = &["AuditAppend"];

/// The exempt kinds, by the name [`OpBody::kind`] returns.
///
/// Every string here **must** be one `kind()` actually produces — an entry
/// that does not match anything is an exemption that silently is not one,
/// and nothing would fail. `keepalive_kinds_are_real_kinds` is the test
/// that makes a rename break the build instead of the control.
pub const KEEPALIVE_KINDS: &[&str] = &[
    "MemberAdd",
    "MemberLeft",
    "MemberUpdate",
    "MemberRevoke",
    "UserRevoke",
    "GrantRevoke",
    "Checkpoint",
    "RetentionPut",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genesis::Genesis;

    fn engine_with_founder() -> (tempfile::TempDir, Arc<StateEngine>, Keypair, Keypair) {
        let dir = tempfile::tempdir().expect("tempdir");
        let founder_user = Keypair::generate().expect("user key");
        let founder_node = Keypair::generate().expect("node key");
        let genesis =
            Genesis::create("test-mesh", &founder_user, founder_node.node_id()).expect("genesis");
        let engine = StateEngine::open(dir.path(), founder_node.clone(), genesis).expect("open");
        (dir, engine, founder_node, founder_user)
    }

    /// Sign an op body as `key`. mesh-core cannot reach for
    /// `mesh-testkit`'s equivalent from inside its own unit tests — that
    /// would be two copies of this crate in one dependency graph — so the
    /// three lines live here too.
    fn seal(key: &Keypair, seq: u64, hlc: Hlc, body: &OpBody) -> OpEnvelope {
        let bytes = postcard::to_stdvec(body).expect("encode body");
        let signature = key
            .sign(&mesh_proto::op_signing_bytes_raw(
                &key.node_id(),
                seq,
                &hlc,
                &bytes,
            ))
            .to_vec();
        OpEnvelope {
            author: key.node_id(),
            seq,
            hlc,
            body: bytes,
            signature,
        }
    }

    fn member_info(key: &Keypair, name: &str) -> MemberInfo {
        MemberInfo {
            node_id: key.node_id(),
            name: name.into(),
            addrs: vec!["127.0.0.1:7777".into()],
            tags: vec![],
            slots: 4,
            joined_at_micros: mesh_proto::now_micros(),
        }
    }

    /// A body-level refusal is a no-op that still counts (module header):
    /// the author's next op applies. One that did not count parked every
    /// later op of its author, on every node, for ever.
    #[test]
    fn a_refused_body_does_not_wedge_its_author() {
        let (_dir, engine, founder, _fuser) = engine_with_founder();
        let beta = Keypair::generate().expect("beta");
        let gamma = Keypair::generate().expect("gamma");
        for (key, name) in [(&founder, "alpha"), (&beta, "beta"), (&gamma, "gamma")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let base = mesh_proto::now_micros() + 1;
        // gamma may not rewrite beta's record.
        let stray = seal(
            &gamma,
            1,
            hlc_at(base),
            &OpBody::MemberUpdate {
                node_id: beta.node_id(),
                name: "hijacked".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
            },
        );
        let own = seal(
            &gamma,
            2,
            hlc_at(base + 1),
            &OpBody::MemberUpdate {
                node_id: gamma.node_id(),
                name: "gamma2".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
            },
        );
        engine.ingest(vec![stray]).expect("ingest");
        engine.ingest(vec![own]).expect("ingest");
        engine.read(|s| {
            assert_eq!(s.members[&beta.node_id()].info.name, "beta");
            assert_eq!(s.applied.get(&gamma.node_id()).copied(), Some(2));
            assert_eq!(s.members[&gamma.node_id()].info.name, "gamma2");
        });
    }

    #[test]
    fn founder_bootstraps_and_state_replays() {
        let (dir, engine, founder, _fuser) = engine_with_founder();
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            })
            .expect("self-add");
        assert!(engine.read(|s| s.is_active_member(&founder.node_id())));

        // Reopen from disk: state must replay identically.
        let genesis = engine.genesis().clone();
        drop(engine);
        let engine2 = StateEngine::open(dir.path(), founder.clone(), genesis).expect("reopen");
        assert!(engine2.read(|s| s.is_active_member(&founder.node_id())));
    }

    #[test]
    fn unauthorized_author_is_parked_not_applied() {
        let (_dir, engine, _founder, _fuser) = engine_with_founder();
        let stranger = Keypair::generate().expect("stranger");
        let body = OpBody::MemberAdd {
            member: member_info(&stranger, "stranger"),
            invite: None,
        };
        let hlc = Hlc {
            wall_micros: 1,
            logical: 0,
        };
        let op = seal(&stranger, 1, hlc, &body);
        let applied = engine.ingest(vec![op]).expect("ingest");
        assert_eq!(applied, 0);
        assert!(!engine.read(|s| s.is_active_member(&stranger.node_id())));
    }

    #[test]
    fn tampered_op_ignored() {
        let (_dir, engine, founder, _fuser) = engine_with_founder();
        let body = OpBody::MemberAdd {
            member: member_info(&founder, "alpha"),
            invite: None,
        };
        let hlc = Hlc {
            wall_micros: 1,
            logical: 0,
        };
        let mut op = seal(&founder, 1, hlc, &body);
        op.signature[0] ^= 0xff;
        assert_eq!(engine.ingest(vec![op]).expect("ingest"), 0);
    }

    /// **The invariant that makes trusting replay sound** (ENG-20 / H-4).
    ///
    /// Boot no longer re-verifies the signature of an op read back from
    /// this node's own store, which is only correct if nothing unverified
    /// can get into that store. It cannot: an op is persisted from the
    /// ingest path *after* it applies, and applying is what the check
    /// gates. A parked one is held in memory and never written at all.
    ///
    /// This is asserted rather than argued because it is the sort of
    /// ordering a later refactor changes without noticing what it was for.
    #[test]
    fn an_op_that_fails_verification_never_reaches_the_store() {
        let (_dir, engine, founder, _fuser) = engine_with_founder();
        let before = engine.all_ops().expect("ops").len();

        let good = seal(
            &founder,
            1,
            hlc_at(mesh_proto::now_micros() + 1),
            &OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            },
        );
        let mut bad = seal(
            &founder,
            2,
            hlc_at(mesh_proto::now_micros() + 2),
            &OpBody::MemberUpdate {
                node_id: founder.node_id(),
                name: "forged".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
            },
        );
        bad.signature[0] ^= 0xff;
        let bad_seq = bad.seq;

        engine.ingest(vec![good, bad]).expect("ingest");

        let stored = engine.all_ops().expect("ops");
        assert!(
            stored.len() > before,
            "the well-signed op should have been kept"
        );
        assert!(
            !stored
                .iter()
                .any(|op| op.author == founder.node_id() && op.seq == bad_seq),
            "an op that failed verification must never be written to the store, because replay trusts what it finds there"
        );
        assert_eq!(
            engine.read(|s| s.members[&founder.node_id()].info.name.clone()),
            "alpha"
        );
    }

    /// …and the check is still paid by everything that is not a replay. A
    /// peer's op is verified however many times this node has seen its
    /// author.
    #[test]
    fn a_peers_op_is_still_verified() {
        let (_dir, engine, founder, _fuser) = engine_with_founder();
        let peer = Keypair::generate().expect("peer");
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            })
            .expect("self-add");
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&peer, "beta"),
                invite: None,
            })
            .expect("add peer");

        let mut forged = seal(
            &peer,
            1,
            hlc_at(mesh_proto::now_micros() + 1),
            &OpBody::MemberUpdate {
                node_id: peer.node_id(),
                name: "forged".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
            },
        );
        forged.signature[7] ^= 0xff;
        assert_eq!(engine.ingest(vec![forged]).expect("ingest"), 0);
        assert_eq!(
            engine.read(|s| s.members[&peer.node_id()].info.name.clone()),
            "beta"
        );
    }

    // -- ENG-19: what a live service keeps on disk --------------------------
    //
    // `referenced_bundles` is a pure function of state, so these build the
    // state directly. A signed publish per version would be forty lines of
    // ceremony testing the op path instead of the question.

    /// A live service with `count` versions, head at the last.
    fn service_with_versions(state: &mut MeshState, name: &str, count: u32) {
        let mut versions = BTreeMap::new();
        for v in 1..=count {
            versions.insert(
                v,
                mesh_proto::ServiceVersion {
                    name: name.to_string(),
                    version: v,
                    runtime: mesh_proto::content::Runtime::Bash,
                    bundle_id: BundleId([v as u8; 32]),
                    working_dir: String::new(),
                    entrypoint: "run.sh".into(),
                    contract: None,
                    env: vec![],
                    description: String::new(),
                    tags: vec![],
                    created_at_micros: u64::from(v),
                    created_by: UserId([1u8; 32]),
                    source: None,
                    signature: vec![],
                },
            );
        }
        state.services.insert(
            name.to_string(),
            ServiceEntry {
                versions,
                head: count,
                removed: false,
            },
        );
    }

    /// A CI job that publishes on every commit used to fill `content/` and
    /// never give a byte back: every version of every live service was
    /// protected from the sweep for ever, and once the store hit its floor
    /// the node started refusing bundle fetches — on a mesh where all but
    /// the newest few versions had not been run in months.
    #[test]
    fn a_service_keeps_its_head_and_a_rollback_window_not_its_whole_history() {
        let mut state = MeshState::default();
        service_with_versions(&mut state, "collector", 40);

        let keep = state.referenced_bundles(None);
        assert_eq!(
            keep.len(),
            MeshState::ROLLBACK_VERSIONS,
            "a forty-version service should keep a window, not forty bundles"
        );
        // The head and the versions immediately below it — the ones a
        // rollback would reach for.
        for v in 36..=40u8 {
            assert!(keep.contains(&BundleId([v; 32])), "v{v} should be kept");
        }
        assert!(
            !keep.contains(&BundleId([35u8; 32])),
            "v35 is outside the window and nothing points at it"
        );
    }

    /// The narrowing is only safe because a pin is protected by name. A
    /// deployment held at an old version must keep its bytes however far
    /// below the head it has fallen — otherwise this change would break
    /// exactly the deployments it is meant to leave alone.
    #[test]
    fn a_deployment_pinned_below_the_window_still_protects_its_bundle() {
        let mut state = MeshState::default();
        service_with_versions(&mut state, "collector", 40);
        state.deployments.insert(
            "pinned".into(),
            DeploymentEntry {
                deployment: mesh_proto::Deployment {
                    name: "pinned".into(),
                    service: "collector".into(),
                    service_version: 3,
                    count: mesh_proto::deployment::Count::Fixed(1),
                    targets: "any".into(),
                    restart: Default::default(),
                    health: None,
                    update: Default::default(),
                    generation: 0,
                    inputs: vec![],
                    paused: false,
                    description: String::new(),
                    created_by: UserId([0u8; 32]),
                    created_at_micros: 1,
                    signature: vec![],
                },
                removed: false,
                updated_hlc: Hlc::default(),
            },
        );

        let keep = state.referenced_bundles(None);
        assert!(
            keep.contains(&BundleId([3u8; 32])),
            "a deployment's version is protected by name, whatever its age"
        );
        assert_eq!(keep.len(), MeshState::ROLLBACK_VERSIONS + 1);
    }

    /// A version published *above* the head is a canary or a publish
    /// mid-rollout. Nothing has decided against it, so it is not history
    /// and the window does not apply to it.
    #[test]
    fn a_version_above_the_head_is_not_history() {
        let mut state = MeshState::default();
        service_with_versions(&mut state, "collector", 40);
        if let Some(entry) = state.services.get_mut("collector") {
            entry.head = 38;
        }

        let keep = state.referenced_bundles(None);
        for v in [39u8, 40] {
            assert!(
                keep.contains(&BundleId([v; 32])),
                "v{v} is above the head and not yet superseded"
            );
        }
    }

    fn signed_node_revocation(
        target: NodeId,
        by: &Keypair,
        reason: &str,
    ) -> mesh_proto::NodeRevocation {
        let mut revocation = mesh_proto::NodeRevocation {
            node_id: target,
            reason: reason.into(),
            by: by.user_id(),
            at_micros: mesh_proto::now_micros(),
            signature: Vec::new(),
        };
        revocation.signature = by.sign(&revocation.signing_bytes()).to_vec();
        revocation
    }

    #[test]
    fn revoke_is_terminal() {
        let (_dir, engine, founder, fuser) = engine_with_founder();
        let other = Keypair::generate().expect("other");
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            })
            .expect("self-add");
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&other, "beta"),
                invite: None,
            })
            .expect("add beta");
        assert!(engine.read(|s| s.is_active_member(&other.node_id())));
        engine
            .author_op(OpBody::MemberRevoke {
                revocation: signed_node_revocation(other.node_id(), &fuser, "test"),
            })
            .expect("revoke");
        assert!(!engine.read(|s| s.is_active_member(&other.node_id())));
        // Re-add after revoke is ignored.
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&other, "beta"),
                invite: None,
            })
            .expect("re-add attempt");
        assert!(!engine.read(|s| s.is_active_member(&other.node_id())));
    }

    #[test]
    fn unauthorized_revocation_is_neutered() {
        let (_dir, engine, founder, _fuser) = engine_with_founder();
        let other = Keypair::generate().expect("other");
        let mallory = Keypair::generate().expect("mallory user");
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            })
            .expect("self-add");
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&other, "beta"),
                invite: None,
            })
            .expect("add beta");
        // A validly-signed revocation by a user nobody knows: parks, never
        // applies, and the member stays active.
        let body = OpBody::MemberRevoke {
            revocation: signed_node_revocation(other.node_id(), &mallory, "hostile"),
        };
        let hlc = engine.clock().now();
        let seq = engine.read(|s| s.applied.get(&founder.node_id()).copied().unwrap_or(0)) + 1;
        let op = seal(&founder, seq, hlc, &body);
        assert_eq!(engine.ingest(vec![op]).expect("ingest"), 0);
        assert!(engine.read(|s| s.is_active_member(&other.node_id())));
    }

    // -- Y6: the release co-signature ---------------------------------------

    /// The real `ssh-keygen -Y sign` document from `sshsig.rs`, and the
    /// bytes it was made over. A real signature rather than a fixture,
    /// because a fixture can only prove two pieces of your own code agree.
    const COSIGN: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgOYX+nApb0Wtu+BNpzM5ctXC/TR
J5cvMtq5RTK3ilrr4AAAAQbWVzaHdhcmUtcmVsZWFzZQAAAAAAAAAGc2hhNTEyAAAAUwAA
AAtzc2gtZWQyNTUxOQAAAEApYrIdyXMSB5fw7iwK0ajsTdMVtZ6wuzWxpTx1QRywkKUycC
Q1eRQJw01dNk0A0MrfyJSMsTBJmA3V6Frbf0wH
-----END SSH SIGNATURE-----";
    const COSIGN_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDmF/pwKW9FrbvgTaczOXLVwv00SeXLzLauUUyt4pa6+ meshware release test";

    fn trust_release_key(engine: &StateEngine, user: &Keypair, pubkey: &str) {
        let mut key = mesh_proto::ReleaseKey {
            pubkey: pubkey.into(),
            label: "test".into(),
            added_by: user.user_id(),
            added_at_micros: mesh_proto::now_micros(),
            signature: Vec::new(),
        };
        key.signature = user.sign(&key.signing_bytes()).to_vec();
        engine
            .author_op(OpBody::ReleaseKeyPut { key: Box::new(key) })
            .expect("trust key");
    }

    fn publish_cosign(engine: &StateEngine, user: &Keypair, armored: &str) {
        let mut cosign = mesh_proto::ReleaseCosignature {
            digest: [5u8; 32],
            armored: armored.into(),
            published_by: user.user_id(),
            published_at_micros: mesh_proto::now_micros(),
            signature: Vec::new(),
        };
        cosign.signature = user.sign(&cosign.signing_bytes()).to_vec();
        engine
            .author_op(OpBody::ReleaseCosign {
                cosign: Box::new(cosign),
            })
            .expect("publish");
    }

    /// One list, two kinds of key, told apart by shape. This is what lets
    /// Y6 add a whole second signature scheme without a second record type
    /// or a second admin verb.
    #[test]
    fn ssh_and_minisign_release_keys_share_one_list_and_are_told_apart() {
        let (_dir, engine, _node, user) = engine_with_founder();
        trust_release_key(&engine, &user, COSIGN_KEY);
        trust_release_key(
            &engine,
            &user,
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
        );
        engine.read(|s| {
            assert_eq!(s.trusted_ssh_release_keys().len(), 1);
            assert_eq!(s.trusted_minisign_release_keys().len(), 1);
        });
    }

    /// A co-signature made by a key this mesh trusts is recorded, with the
    /// key that made it — so a key that later stops being trusted visibly
    /// stops vouching.
    #[test]
    fn a_cosignature_from_a_trusted_key_is_recorded_with_that_key() {
        let (_dir, engine, _node, user) = engine_with_founder();
        trust_release_key(&engine, &user, COSIGN_KEY);
        publish_cosign(&engine, &user, COSIGN);

        let entry = engine.read(|s| {
            s.release_cosignatures
                .get(&mesh_proto::hex32(&[5u8; 32]))
                .cloned()
        });
        let entry = entry.expect("recorded");
        assert_eq!(entry.signed_by_key, COSIGN_KEY);
        assert!(
            !entry.user_present,
            "an ordinary ssh-ed25519 key cannot claim a human was there, and \
             cannot-say must never read as yes"
        );
    }

    /// **The authority is the key inside the document, not the admin who
    /// published it.** An admin can publish anything; what they cannot do
    /// is make the mesh record a co-signature from a key nobody trusts.
    #[test]
    fn an_admin_cannot_publish_a_cosignature_from_an_untrusted_key() {
        let (_dir, engine, _node, user) = engine_with_founder();
        // Note: no release key trusted at all.
        publish_cosign(&engine, &user, COSIGN);
        assert!(
            engine.read(|s| s.release_cosignatures.is_empty()),
            "a co-signature the mesh cannot attribute is not a co-signature"
        );

        // ...and with a *different* key trusted, still nothing.
        trust_release_key(
            &engine,
            &user,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA other",
        );
        publish_cosign(&engine, &user, COSIGN);
        assert!(engine.read(|s| s.release_cosignatures.is_empty()));
    }

    /// Garbage in an op from another node is refused, not stored and not a
    /// panic.
    #[test]
    fn a_cosignature_that_is_not_a_signature_records_nothing() {
        let (_dir, engine, _node, user) = engine_with_founder();
        trust_release_key(&engine, &user, COSIGN_KEY);
        publish_cosign(&engine, &user, "not a signature at all");
        assert!(engine.read(|s| s.release_cosignatures.is_empty()));
    }

    fn job_summary(job: JobId, executor: NodeId, state: JobState, hlc: Hlc) -> JobSummary {
        JobSummary {
            job_id: job,
            cmd_preview: "sleep 999".into(),
            runtime: "bash".into(),
            submitter: executor,
            submitter_user: UserId([9u8; 32]),
            executor,
            state,
            exit_code: None,
            created_at_micros: hlc.wall_micros,
            started_at_micros: Some(hlc.wall_micros),
            finished_at_micros: state.is_terminal().then_some(hlc.wall_micros),
            fanout: None,
            updated_hlc: hlc,
        }
    }

    fn force_cancel_body(job: JobId, by: &Keypair) -> OpBody {
        let by_id = by.user_id();
        let signature = by
            .sign(&job_cancel_force_signing_bytes(&job, &by_id))
            .to_vec();
        OpBody::JobCancelForce {
            job_id: job,
            by: by_id,
            signature,
        }
    }

    fn hlc_at(wall_micros: u64) -> Hlc {
        Hlc {
            wall_micros,
            logical: 0,
        }
    }

    /// The scenario the op exists for: an executor published `running` and
    /// then vanished for good, so nobody who is allowed to write the
    /// summary will ever write it again.
    #[test]
    fn force_cancel_overrides_a_silent_executor() {
        let (_dir, engine, founder, fuser) = engine_with_founder();
        let exec = Keypair::generate().expect("exec");
        for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let job = JobId::generate();
        // After the members were added: an op may not predate its
        // author's admission, nor its author's previous op (SEC-5).
        let base = mesh_proto::now_micros() + 1;
        let t1 = hlc_at(base);
        let upsert = seal(
            &exec,
            1,
            t1,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Running, t1),
            },
        );
        assert_eq!(engine.ingest(vec![upsert]).expect("ingest"), 1);
        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").state),
            JobState::Running
        );

        engine
            .author_op(force_cancel_body(job, &fuser))
            .expect("force cancel");
        let summary = engine.read(|s| s.jobs.get(&job).cloned().expect("job"));
        assert_eq!(summary.state, JobState::Cancelled);
        assert!(
            summary.finished_at_micros.is_some(),
            "a forced cancel must finish the job, or a jobs clear can never forget it"
        );
    }

    /// Per-author replay means the force op and the `JobUpsert` it
    /// overrides can land in either order on any given boot; both orders
    /// must settle on the same summary.
    #[test]
    fn force_cancel_commutes_with_the_job_it_overrides() {
        let exec = Keypair::generate().expect("exec");
        let job = JobId::generate();
        let settle = |flip: bool| {
            let (_dir, engine, founder, fuser) = engine_with_founder();
            for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
                engine
                    .author_op(OpBody::MemberAdd {
                        member: member_info(key, name),
                        invite: None,
                    })
                    .expect("add member");
            }
            // After the members were added: an op may not predate its
            // author's admission, nor its author's previous op (SEC-5).
            let base = mesh_proto::now_micros() + 1;
            let t1 = hlc_at(base);
            let t2 = hlc_at(base + 10);
            let upsert = seal(
                &exec,
                1,
                t1,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Running, t1),
                },
            );
            let seq = engine.read(|s| s.applied.get(&founder.node_id()).copied().unwrap_or(0)) + 1;
            let force = seal(&founder, seq, t2, &force_cancel_body(job, &fuser));
            let batch = if flip {
                vec![force, upsert]
            } else {
                vec![upsert, force]
            };
            assert_eq!(engine.ingest(batch).expect("ingest"), 2);
            engine.read(|s| s.jobs.get(&job).expect("job").state)
        };
        assert_eq!(settle(false), JobState::Cancelled);
        assert_eq!(settle(true), JobState::Cancelled);
    }

    // -- ENG-16: jobs whose executor is gone for good ----------------------
    //
    // Same shape of problem as the forced cancel above, and deliberately a
    // different answer. A force is a *decision* somebody makes and signs;
    // this is a *fact* every node can already see — the executor has been
    // revoked, or has left — so it is derived rather than authored, and the
    // laws it has to obey are the laws of a derivation: same answer on
    // every node, same answer whatever order the ops arrive in, and no
    // answer at all where the fact does not hold.

    /// A mesh with the founder and one other member, that member holding
    /// one running job.
    fn mesh_with_a_running_job(
        job: JobId,
    ) -> (
        tempfile::TempDir,
        Arc<StateEngine>,
        Keypair,
        Keypair,
        Keypair,
    ) {
        let (dir, engine, founder, fuser) = engine_with_founder();
        let exec = Keypair::generate().expect("exec");
        for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let t1 = hlc_at(mesh_proto::now_micros() + 1);
        let upsert = seal(
            &exec,
            1,
            t1,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Running, t1),
            },
        );
        assert_eq!(engine.ingest(vec![upsert]).expect("ingest"), 1);
        (dir, engine, founder, fuser, exec)
    }

    /// The case the whole thing exists for: a machine is re-imaged, the
    /// admin revokes it, and the jobs it was running stop reading
    /// `running` — without anybody force-cancelling them one at a time.
    #[test]
    fn revoking_an_executor_settles_the_jobs_it_was_running() {
        let job = JobId::generate();
        let (_dir, engine, _founder, fuser, exec) = mesh_with_a_running_job(job);

        engine
            .author_op(OpBody::MemberRevoke {
                revocation: signed_node_revocation(exec.node_id(), &fuser, "re-imaged"),
            })
            .expect("revoke");

        let summary = engine.read(|s| s.jobs.get(&job).cloned().expect("job"));
        assert_eq!(summary.state, JobState::Failed);
        assert!(
            summary.finished_at_micros.is_some(),
            "an unsettled job can never be forgotten by a jobs clear"
        );
    }

    /// A node that leaves politely is just as unable to report again.
    #[test]
    fn a_member_that_leaves_settles_the_jobs_it_was_running() {
        let job = JobId::generate();
        let (_dir, engine, _founder, _fuser, exec) = mesh_with_a_running_job(job);

        let left = seal(
            &exec,
            2,
            hlc_at(mesh_proto::now_micros() + 100),
            &OpBody::MemberLeft {
                node_id: exec.node_id(),
            },
        );
        assert_eq!(engine.ingest(vec![left]).expect("ingest"), 1);

        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").state),
            JobState::Failed
        );
    }

    /// **The law that makes this safe to derive.** Ops replay per author,
    /// so on some boots the op that retires an executor lands *before* the
    /// executor's own `running` report rather than after it. Neither order
    /// may leave a job sitting at `running` with nobody able to report it.
    ///
    /// The two orders do not produce identical maps, and that is a rule
    /// older than this one: a revoked author is dead forever (§4), so its
    /// ops are ignored outright once the revocation is in state, and the
    /// report never lands at all. Absent and settled are both *finished*;
    /// `running` is the only outcome that is wrong.
    #[test]
    fn neither_replay_order_leaves_the_job_running() {
        let exec = Keypair::generate().expect("exec");
        let job = JobId::generate();
        let settle = |revoke_first: bool| {
            let (_dir, engine, founder, fuser) = engine_with_founder();
            for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
                engine
                    .author_op(OpBody::MemberAdd {
                        member: member_info(key, name),
                        invite: None,
                    })
                    .expect("add member");
            }
            let base = mesh_proto::now_micros() + 1;
            let t1 = hlc_at(base);
            let t2 = hlc_at(base + 10);
            let upsert = seal(
                &exec,
                1,
                t1,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Running, t1),
                },
            );
            let seq = engine.read(|s| s.applied.get(&founder.node_id()).copied().unwrap_or(0)) + 1;
            let revoke = seal(
                &founder,
                seq,
                t2,
                &OpBody::MemberRevoke {
                    revocation: signed_node_revocation(exec.node_id(), &fuser, "re-imaged"),
                },
            );
            // Ingested one at a time, not as one batch: a batch is
            // applied per author in node-id order, so the order inside the
            // `Vec` is not the order things happen in — which is the whole
            // property under test, and would otherwise be decided by two
            // freshly generated keys.
            let (first, second) = if revoke_first {
                (revoke, upsert)
            } else {
                (upsert, revoke)
            };
            engine.ingest(vec![first]).expect("ingest");
            engine.ingest(vec![second]).expect("ingest");
            engine.read(|s| s.jobs.get(&job).cloned())
        };

        // Report first: the settlement is what finishes it.
        let settled = settle(false).expect("the report landed");
        assert_eq!(settled.state, JobState::Failed);
        assert!(settled.finished_at_micros.is_some());

        // Revocation first: the report is refused, so there is no row to
        // be stuck — and certainly not a running one.
        match settle(true) {
            None => {}
            Some(summary) => assert!(
                summary.state.is_terminal(),
                "a job whose executor is revoked must never read running: {summary:?}"
            ),
        }
    }

    /// The settlement instant comes from the departure, not from a local
    /// clock — so two nodes that apply the same ops agree on the same
    /// `finished_at_micros`, and a history clear forgets the same jobs on
    /// both. A wall-clock settlement would differ by milliseconds per node
    /// and put a job on either side of the watermark depending on where it
    /// was asked.
    #[test]
    fn the_settlement_instant_is_the_departures_and_not_a_local_clock() {
        let job = JobId::generate();
        let (_dir, engine, _founder, fuser, exec) = mesh_with_a_running_job(job);

        engine
            .author_op(OpBody::MemberRevoke {
                revocation: signed_node_revocation(exec.node_id(), &fuser, "re-imaged"),
            })
            .expect("revoke");

        let (settled, revoked_at) = engine.read(|s| {
            (
                s.jobs.get(&job).cloned().expect("job"),
                s.gone_at(&exec.node_id()).expect("gone"),
            )
        });
        assert_eq!(settled.finished_at_micros, Some(revoked_at.wall_micros));
        assert_eq!(settled.updated_hlc, revoked_at);
    }

    /// An executor that got its last word out wins. A node that reported
    /// `succeeded` and *then* left really did succeed, and a settlement
    /// that overwrote that would be inventing a failure.
    #[test]
    fn a_report_the_executor_did_manage_to_send_outranks_the_settlement() {
        let job = JobId::generate();
        let (_dir, engine, _founder, fuser, exec) = mesh_with_a_running_job(job);

        let done_at = hlc_at(mesh_proto::now_micros() + 10_000_000);
        let done = seal(
            &exec,
            2,
            done_at,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Succeeded, done_at),
            },
        );
        assert_eq!(engine.ingest(vec![done]).expect("ingest"), 1);

        engine
            .author_op(OpBody::MemberRevoke {
                revocation: signed_node_revocation(exec.node_id(), &fuser, "re-imaged"),
            })
            .expect("revoke");

        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").state),
            JobState::Succeeded,
            "the executor's own report must survive its departure"
        );
    }

    /// A node that is merely *unreachable* has not gone anywhere. Its jobs
    /// may well still be running, and settling them would be a lie that the
    /// node has to argue with when it comes back.
    #[test]
    fn an_unreachable_executor_is_not_a_departed_one() {
        let job = JobId::generate();
        let (_dir, engine, _founder, _fuser, exec) = mesh_with_a_running_job(job);
        engine.read(|s| {
            assert!(s.gone_at(&exec.node_id()).is_none());
            assert_eq!(s.jobs.get(&job).expect("job").state, JobState::Running);
        });
    }

    /// The departure instant is the *departure's*, not whatever last
    /// touched the member record. An admin retagging a node that left — or
    /// any other late patch — must not move the finish time of jobs that
    /// were settled at it, or a history clear forgets a different set of
    /// jobs on every node.
    #[test]
    fn a_later_patch_to_a_departed_member_does_not_move_the_settlement() {
        let job = JobId::generate();
        let (_dir, engine, _founder, _fuser, exec) = mesh_with_a_running_job(job);

        let left = seal(
            &exec,
            2,
            hlc_at(mesh_proto::now_micros() + 100),
            &OpBody::MemberLeft {
                node_id: exec.node_id(),
            },
        );
        assert_eq!(engine.ingest(vec![left]).expect("ingest"), 1);
        let settled_at = engine.read(|s| s.jobs.get(&job).expect("job").finished_at_micros);
        assert!(settled_at.is_some());

        // Something else writes the member record afterwards, moving
        // `updated_hlc` well past the departure.
        let patch = seal(
            &exec,
            3,
            hlc_at(mesh_proto::now_micros() + 5_000_000),
            &OpBody::MemberUpdate {
                node_id: exec.node_id(),
                name: "beta".into(),
                addrs: vec!["127.0.0.1:7778".into()],
                tags: vec![],
                slots: 4,
            },
        );
        let _ = engine.ingest(vec![patch]);

        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").finished_at_micros),
            settled_at,
            "the settlement instant must not drift"
        );
    }

    fn jobs_clear_body(before_micros: u64, by: &Keypair) -> OpBody {
        let by_id = by.user_id();
        let signature = by
            .sign(&jobs_clear_signing_bytes(before_micros, &by_id))
            .to_vec();
        OpBody::JobsClear {
            before_micros,
            by: by_id,
            signature,
        }
    }

    /// Two nodes, one clear, and the replay order the op-log actually
    /// uses: ops are walked per author (`author || seq`), so an executor
    /// whose node id sorts *after* the clear's author has every one of its
    /// `JobUpsert`s replayed on the far side of the watermark.
    ///
    /// The job's own reports still arrive in order — `running`, then
    /// `succeeded` — and it is the pair that has to settle, not each op
    /// alone. Checking the watermark against a single op let the terminal
    /// report be dropped while the `running` one stood, which is how a
    /// one-second script came to be listed as running eight hours later.
    #[test]
    fn a_clear_forgets_a_job_whose_reports_replay_after_it() {
        let exec = Keypair::generate().expect("exec");
        let job = JobId::generate();
        // Both replay orders: the clear before the executor's reports (the
        // broken one) and after them.
        let settle = |clear_first: bool| {
            let (_dir, engine, founder, fuser) = engine_with_founder();
            for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
                engine
                    .author_op(OpBody::MemberAdd {
                        member: member_info(key, name),
                        invite: None,
                    })
                    .expect("add member");
            }
            // After the members were added: an op may not predate its
            // author's admission, nor its author's previous op (SEC-5).
            let base = mesh_proto::now_micros() + 1;
            let t1 = hlc_at(base);
            let t2 = hlc_at(base + 10);
            // The clear is drawn after the job finished, so it forgets it.
            let t3 = hlc_at(base + 20);
            let started = seal(
                &exec,
                1,
                t1,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Running, t1),
                },
            );
            let finished = seal(
                &exec,
                2,
                t2,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Succeeded, t2),
                },
            );
            let seq = engine.read(|s| s.applied.get(&founder.node_id()).copied().unwrap_or(0)) + 1;
            let clear = seal(&founder, seq, t3, &jobs_clear_body(t3.wall_micros, &fuser));
            // Ingested one at a time, because a single batch would be
            // sorted into `(author, seq)` order and the point is the order.
            let batch = if clear_first {
                vec![clear, started, finished]
            } else {
                vec![started, finished, clear]
            };
            for op in batch {
                engine.ingest(vec![op]).expect("ingest");
            }
            engine.read(|s| s.jobs.get(&job).cloned())
        };
        assert!(
            settle(false).is_none(),
            "a cleared job survived its own clear"
        );
        assert!(
            settle(true).is_none(),
            "a cleared job came back as a ghost when its reports replayed after the clear"
        );
    }

    /// The same hole without a restart: the executor was unreachable when
    /// the button was pressed, so the map still held `running`, and its
    /// terminal report only arrived afterwards. The report is older than
    /// the watermark, so the job is forgotten — what it must not do is
    /// leave the `running` row behind.
    #[test]
    fn a_late_terminal_report_does_not_strand_a_cleared_job() {
        let (_dir, engine, founder, fuser) = engine_with_founder();
        let exec = Keypair::generate().expect("exec");
        for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let job = JobId::generate();
        // After the members were added: an op may not predate its
        // author's admission, nor its author's previous op (SEC-5).
        let base = mesh_proto::now_micros() + 1;
        let t1 = hlc_at(base);
        let t2 = hlc_at(base + 10);
        let t3 = hlc_at(base + 20);
        engine
            .ingest(vec![seal(
                &exec,
                1,
                t1,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Running, t1),
                },
            )])
            .expect("ingest");

        // Pressed while the map still says running: the clear spares it,
        // which is the promise the button makes.
        engine
            .author_op(jobs_clear_body(t3.wall_micros, &fuser))
            .expect("clear");
        assert_eq!(
            engine.read(|s| s.jobs.get(&job).map(|j| j.state)),
            Some(JobState::Running),
            "the clear ate work it could not see the end of"
        );

        // The executor reconnects and reports a finish that predates the
        // watermark. The job is forgotten — not stranded.
        engine
            .ingest(vec![seal(
                &exec,
                2,
                t2,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Succeeded, t2),
                },
            )])
            .expect("ingest");
        assert!(
            engine.read(|s| !s.jobs.contains_key(&job)),
            "a late report left the job listed as running for good"
        );
    }

    /// The ghost the Director actually shows: an executor published
    /// `running` and went quiet, the operator pressed **Cancel** (which is
    /// a force op — the executor cannot be told) and then **Clear
    /// history**, and the row came back at `running` on the next boot.
    ///
    /// Why it came back: the force is what made the job terminal, and
    /// therefore what made it clearable. Dropping the force on the
    /// watermark threw away the only record of that — so a boot that
    /// replayed the executor's `JobUpsert` after the clear (per-author
    /// replay, so the executor's node id sorting after the clear's author
    /// is enough) re-inserted a job nothing could settle and nothing could
    /// clear: `cleared_by` needs a terminal state the force was carrying.
    #[test]
    fn a_forced_cancel_survives_the_clear_that_forgets_its_job() {
        let exec = Keypair::generate().expect("exec");
        let job = JobId::generate();
        // Every replay order, not just the one boot happened to pick:
        // three ops in three logs, and the operator's two are only
        // ordered relative to each other.
        let settle = |order: [usize; 3]| {
            let (_dir, engine, founder, fuser) = engine_with_founder();
            for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
                engine
                    .author_op(OpBody::MemberAdd {
                        member: member_info(key, name),
                        invite: None,
                    })
                    .expect("add member");
            }
            // After the members were added: an op may not predate its
            // author's admission, nor its author's previous op (SEC-5).
            let base = mesh_proto::now_micros() + 1;
            let t1 = hlc_at(base);
            let t2 = hlc_at(base + 10);
            let t3 = hlc_at(base + 20);
            let running = seal(
                &exec,
                1,
                t1,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Running, t1),
                },
            );
            let seq = engine.read(|s| s.applied.get(&founder.node_id()).copied().unwrap_or(0)) + 1;
            let force = seal(&founder, seq, t2, &force_cancel_body(job, &fuser));
            let clear = seal(
                &founder,
                seq + 1,
                t3,
                &jobs_clear_body(t3.wall_micros, &fuser),
            );
            // One at a time: a single batch would be sorted back into
            // `(author, seq)` order, and the order is the point.
            let ops = [running, force, clear];
            let mut ops = ops.map(Some);
            for i in order {
                engine
                    .ingest(vec![ops[i].take().expect("each op once")])
                    .expect("ingest");
            }
            engine.read(|s| s.jobs.get(&job).cloned())
        };
        // The operator pressed Cancel before Clear, so the force always
        // precedes the clear; the executor's report floats.
        for order in [[0, 1, 2], [1, 0, 2], [1, 2, 0]] {
            assert!(
                settle(order).is_none(),
                "the cancelled-and-cleared job came back as a ghost, replay order {order:?}"
            );
        }
    }

    /// The clear and the force from *different nodes*, which is when the
    /// two can genuinely land in either order: an admin clears from the
    /// Director while a second admin (or the same one at another console)
    /// forces a job whose executor went quiet. The force is older than the
    /// watermark, and used to be dropped on sight for being so — which
    /// threw away the one fact that could finish the job, leaving the
    /// executor's `running` report standing.
    #[test]
    fn a_force_older_than_the_clear_still_finishes_its_job() {
        let exec = Keypair::generate().expect("exec");
        let job = JobId::generate();
        let settle = |clear_first: bool| {
            let (_dir, engine, founder, fuser) = engine_with_founder();
            for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
                engine
                    .author_op(OpBody::MemberAdd {
                        member: member_info(key, name),
                        invite: None,
                    })
                    .expect("add member");
            }
            // After the members were added: an op may not predate its
            // author's admission, nor its author's previous op (SEC-5).
            let base = mesh_proto::now_micros() + 1;
            let t1 = hlc_at(base);
            let t2 = hlc_at(base + 10);
            let t3 = hlc_at(base + 20);
            let running = seal(
                &exec,
                1,
                t1,
                &OpBody::JobUpsert {
                    summary: job_summary(job, exec.node_id(), JobState::Running, t1),
                },
            );
            // Authored by another node, signed by the admin: any node may
            // carry the force, and that is exactly why its log can replay
            // on the far side of the clear.
            let force = seal(&exec, 2, t2, &force_cancel_body(job, &fuser));
            let seq = engine.read(|s| s.applied.get(&founder.node_id()).copied().unwrap_or(0)) + 1;
            let clear = seal(&founder, seq, t3, &jobs_clear_body(t3.wall_micros, &fuser));
            let batch = if clear_first {
                vec![clear, running, force]
            } else {
                vec![running, force, clear]
            };
            for op in batch {
                engine.ingest(vec![op]).expect("ingest");
            }
            engine.read(|s| s.jobs.get(&job).cloned())
        };
        assert!(
            settle(false).is_none(),
            "a forced cancel did not let the clear forget its job"
        );
        assert!(
            settle(true).is_none(),
            "a force older than the watermark was thrown away, stranding its job at running"
        );
    }

    /// An executor that was merely unreachable and later reports the real
    /// outcome wins over the force: what happened beats what was demanded.
    #[test]
    fn a_late_executor_report_beats_the_force() {
        let (_dir, engine, founder, fuser) = engine_with_founder();
        let exec = Keypair::generate().expect("exec");
        for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let job = JobId::generate();
        // After the members were added: an op may not predate its
        // author's admission, nor its author's previous op (SEC-5).
        let base = mesh_proto::now_micros() + 1;
        let t1 = hlc_at(base);
        let upsert = seal(
            &exec,
            1,
            t1,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Running, t1),
            },
        );
        engine.ingest(vec![upsert]).expect("ingest");
        engine
            .author_op(force_cancel_body(job, &fuser))
            .expect("force cancel");

        // The executor reconnects and tells the mesh what actually
        // happened, with a later clock than the override's.
        let t3 = hlc_at(mesh_proto::now_micros() + 1_000_000);
        let report = seal(
            &exec,
            2,
            t3,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Succeeded, t3),
            },
        );
        assert_eq!(engine.ingest(vec![report]).expect("ingest"), 1);
        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").state),
            JobState::Succeeded
        );
    }

    /// Register a user and one unscoped role grant, both founder-signed.
    fn add_role_user(
        engine: &Arc<StateEngine>,
        fuser: &Keypair,
        user: &Keypair,
        username: &str,
        role: Role,
    ) {
        let mut record = UserRecord {
            user_id: user.user_id(),
            username: username.into(),
            created_at_micros: mesh_proto::now_micros(),
            added_by: fuser.user_id(),
            signature: Vec::new(),
        };
        record.signature = fuser.sign(&record.signing_bytes()).to_vec();
        engine
            .author_op(OpBody::UserAdd { record })
            .expect("user add");
        let mut grant = Grant {
            grant_id: GrantId::generate(),
            user_id: user.user_id(),
            role,
            nodes: "*".into(),
            services: "*".into(),
            expires_at_micros: None,
            created_at_micros: mesh_proto::now_micros(),
            granted_by: fuser.user_id(),
            signature: Vec::new(),
        };
        grant.signature = fuser.sign(&grant.signing_bytes()).to_vec();
        engine
            .author_op(OpBody::GrantAdd { grant })
            .expect("grant add");
    }

    /// The force is operator/admin authority: a developer's signature is
    /// valid and their role real, and the op is still consumed without
    /// effect — a force rewrites another node's record, which is not a
    /// developer's to rewrite (even for their own job; the scope rule must
    /// be decidable from roles alone, see the apply arm).
    #[test]
    fn a_developer_signed_force_cancel_is_dropped() {
        let (_dir, engine, founder, fuser) = engine_with_founder();
        let exec = Keypair::generate().expect("exec");
        for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let dev = Keypair::generate().expect("dev user");
        add_role_user(&engine, &fuser, &dev, "dev", Role::Developer);
        let job = JobId::generate();
        // After the members were added: an op may not predate its
        // author's admission, nor its author's previous op (SEC-5).
        let base = mesh_proto::now_micros() + 1;
        let t1 = hlc_at(base);
        let upsert = seal(
            &exec,
            1,
            t1,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Running, t1),
            },
        );
        engine.ingest(vec![upsert]).expect("ingest");

        engine
            .author_op(force_cancel_body(job, &dev))
            .expect("the op is consumed, only its effect is refused");
        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").state),
            JobState::Running,
            "a developer-signed force must not rewrite the record"
        );
        assert!(
            engine.read(|s| s.jobs_force_cancelled.is_empty()),
            "and must leave no pending override behind"
        );
    }

    /// Operators hold the same force authority admins do — the day-2 crew
    /// ends stuck jobs without paging an admin.
    #[test]
    fn an_operator_may_force_cancel_any_job() {
        let (_dir, engine, founder, fuser) = engine_with_founder();
        let exec = Keypair::generate().expect("exec");
        for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let oper = Keypair::generate().expect("operator user");
        add_role_user(&engine, &fuser, &oper, "oper", Role::Operator);
        let job = JobId::generate();
        // After the members were added: an op may not predate its
        // author's admission, nor its author's previous op (SEC-5).
        let base = mesh_proto::now_micros() + 1;
        let t1 = hlc_at(base);
        let upsert = seal(
            &exec,
            1,
            t1,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Running, t1),
            },
        );
        engine.ingest(vec![upsert]).expect("ingest");

        engine
            .author_op(force_cancel_body(job, &oper))
            .expect("force cancel");
        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").state),
            JobState::Cancelled
        );
    }

    /// A force signed by a user nobody knows parks rather than applies —
    /// the override is an authority the signature has to carry.
    #[test]
    fn unauthorized_force_cancel_is_inert() {
        let (_dir, engine, founder, _fuser) = engine_with_founder();
        let exec = Keypair::generate().expect("exec");
        let mallory = Keypair::generate().expect("mallory user");
        for (key, name) in [(&founder, "alpha"), (&exec, "beta")] {
            engine
                .author_op(OpBody::MemberAdd {
                    member: member_info(key, name),
                    invite: None,
                })
                .expect("add member");
        }
        let job = JobId::generate();
        // After the members were added: an op may not predate its
        // author's admission, nor its author's previous op (SEC-5).
        let base = mesh_proto::now_micros() + 1;
        let t1 = hlc_at(base);
        let upsert = seal(
            &exec,
            1,
            t1,
            &OpBody::JobUpsert {
                summary: job_summary(job, exec.node_id(), JobState::Running, t1),
            },
        );
        engine.ingest(vec![upsert]).expect("ingest");
        let seq = engine.read(|s| s.applied.get(&founder.node_id()).copied().unwrap_or(0)) + 1;
        let op = seal(
            &founder,
            seq,
            engine.clock().now(),
            &force_cancel_body(job, &mallory),
        );
        assert_eq!(engine.ingest(vec![op]).expect("ingest"), 0);
        assert_eq!(
            engine.read(|s| s.jobs.get(&job).expect("job").state),
            JobState::Running
        );
    }

    #[test]
    fn cross_engine_sync_converges() {
        let (_d1, a, founder, _fuser) = engine_with_founder();
        a.author_op(OpBody::MemberAdd {
            member: member_info(&founder, "alpha"),
            invite: None,
        })
        .expect("self-add");

        // Second engine, same genesis, different node key.
        let d2 = tempfile::tempdir().expect("tempdir");
        let b_key = Keypair::generate().expect("b key");
        let b = StateEngine::open(d2.path(), b_key.clone(), a.genesis().clone()).expect("open b");

        // Founder adds b, then b syncs everything from a.
        a.author_op(OpBody::MemberAdd {
            member: member_info(&b_key, "beta"),
            invite: None,
        })
        .expect("add beta");
        let (missing, more) = a.missing_for(&b.version_vector()).expect("missing");
        assert!(!more);
        b.ingest(missing).expect("ingest");
        assert!(b.read(|s| s.is_active_member(&b_key.node_id())));

        // b authors an op; a ingests it back.
        b.author_op(OpBody::MemberUpdate {
            node_id: b_key.node_id(),
            name: "beta".into(),
            addrs: vec!["10.0.0.9:7777".into()],
            tags: vec!["gpu".into()],
            slots: 4,
        })
        .expect("b updates");
        let (missing, _) = b.missing_for(&a.version_vector()).expect("missing back");
        a.ingest(missing).expect("a ingests");
        let tags = a.read(|s| s.members[&b_key.node_id()].info.tags.clone());
        assert_eq!(tags, vec!["gpu".to_string()]);
    }

    /// A batch is bounded by bytes as well as by count, because the count
    /// bounds the wrong thing: 512 large ops is an undeliverable frame, and
    /// an undeliverable frame is not slow sync but *stopped* sync — the
    /// peer never gets past the point in the log where the big ops start.
    #[test]
    fn a_sync_batch_stays_inside_one_frame() {
        let (_d, a, founder, founder_user) = engine_with_founder();
        a.author_op(OpBody::MemberAdd {
            member: member_info(&founder, "alpha"),
            invite: None,
        })
        .expect("self-add");

        // 64 audit entries of 64 KiB: well under the count bound, well over
        // what one frame holds.
        let bodies: Vec<OpBody> = (0..64)
            .map(|i| OpBody::AuditAppend {
                entry: mesh_proto::AuditEntry {
                    audit_id: mesh_proto::AuditId::generate(),
                    at_micros: mesh_proto::now_micros(),
                    user: founder_user.user_id(),
                    username: "bulk".into(),
                    action: "test.bulk".into(),
                    target: format!("{i}"),
                    node: founder.node_id(),
                    outcome: "ok".into(),
                    detail: "x".repeat(64 * 1024),
                    request_sig: Vec::new(),
                },
            })
            .collect();
        let total = bodies.len();
        a.author_ops(bodies).expect("author bulk");

        // Drain from empty, checking every batch is a frame that could
        // actually be written.
        let mut progress = crate::membership::SnapshotProgress::default();
        let mut have: mesh_proto::VersionVector = Vec::new();
        let mut drained = 0usize;
        let mut batches = 0usize;
        loop {
            let (ops, more) = a.missing_for(&have).expect("missing");
            assert!(!ops.is_empty(), "a non-terminal batch must carry ops");
            assert!(ops.len() <= crate::store::SYNC_BATCH);
            let encoded = postcard::to_stdvec(&ops).expect("encode batch").len();
            assert!(
                encoded <= mesh_proto::MAX_FRAME_LEN as usize,
                "batch of {} ops is {encoded} bytes — larger than a frame",
                ops.len()
            );
            drained += ops.len();
            batches += 1;
            progress.observe(&ops);
            have = progress.vv();
            if !more {
                break;
            }
            assert!(batches < 100, "drain is not terminating");
        }
        assert!(batches > 1, "the bulk should not have fit in one batch");
        assert_eq!(drained, a.all_ops().expect("all").len());
        assert!(drained > total);
    }

    /// A batch is not a shortcut past the log: the ops it authors have to be
    /// the same ops, numbered the same way, or a peer's gapless per-author
    /// check parks them forever.
    ///
    /// This is the property any bulk publish leans on, so it is worth
    /// stating separately from the speed that motivated it.
    #[test]
    fn a_batch_authors_consecutive_ops_that_a_peer_converges_on() {
        let (_d1, a, founder, fuser) = engine_with_founder();
        a.author_op(OpBody::MemberAdd {
            member: member_info(&founder, "alpha"),
            invite: None,
        })
        .expect("self-add");
        let before = a.read(|s| s.applied[&founder.node_id()]);

        let grants: Vec<OpBody> = (0..500u64)
            .map(|i| {
                let mut grant = mesh_proto::Grant {
                    grant_id: GrantId::generate(),
                    user_id: fuser.user_id(),
                    role: Role::Viewer,
                    nodes: format!("name=n{i:03}"),
                    services: "*".into(),
                    expires_at_micros: None,
                    created_at_micros: i + 1,
                    granted_by: fuser.user_id(),
                    signature: Vec::new(),
                };
                grant.signature = fuser.sign(&grant.signing_bytes()).to_vec();
                OpBody::GrantAdd { grant }
            })
            .collect();
        let ops = a.author_ops(grants).expect("batch");
        assert_eq!(ops.len(), 500);

        // Consecutive, starting where the log left off — no gaps, no reuse.
        for (i, op) in ops.iter().enumerate() {
            assert_eq!(op.author, founder.node_id());
            assert_eq!(op.seq, before + 1 + i as u64, "op {i} seq");
        }
        assert_eq!(a.read(|s| s.applied[&founder.node_id()]), before + 500);
        assert_eq!(a.read(|s| s.grants.len()), 500);

        // A peer that has never seen any of it ends up with all of it.
        let d2 = tempfile::tempdir().expect("tempdir");
        let b_key = Keypair::generate().expect("b key");
        let b = StateEngine::open(d2.path(), b_key, a.genesis().clone()).expect("open b");
        let (missing, _) = a.missing_for(&b.version_vector()).expect("missing");
        b.ingest(missing).expect("ingest batch");
        assert_eq!(b.read(|s| s.grants.len()), 500);
    }

    // -- SEC-17: invites and secret wraps that arrive in any order ----------

    /// Engine plus members `names[i]` for fresh node keys (the engine's own
    /// node is added first as "alpha").
    fn engine_with_members(
        names: &[&str],
    ) -> (
        tempfile::TempDir,
        Arc<StateEngine>,
        Keypair,
        Keypair,
        Vec<Keypair>,
    ) {
        let (dir, engine, founder, fuser) = engine_with_founder();
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            })
            .expect("self-add");
        let keys: Vec<Keypair> = names
            .iter()
            .map(|name| {
                let key = Keypair::generate().expect("key");
                engine
                    .author_op(OpBody::MemberAdd {
                        member: member_info(&key, name),
                        invite: None,
                    })
                    .expect("add member");
                key
            })
            .collect();
        (dir, engine, founder, fuser, keys)
    }

    fn next_seq(engine: &StateEngine, key: &Keypair) -> u64 {
        engine.read(|s| s.applied.get(&key.node_id()).copied().unwrap_or(0)) + 1
    }

    #[test]
    fn an_invite_use_and_revoke_that_arrive_before_the_invite_are_kept() {
        let (_dir, engine, _founder, fuser, keys) = engine_with_members(&["minter", "door"]);
        let (minter, door) = (&keys[0], &keys[1]);
        let mesh_id = engine.genesis().body.mesh_id;
        let params = crate::membership::InviteParams {
            ttl_secs: 600,
            uses: 3,
            tags: vec![],
            bootstrap: vec!["127.0.0.1:1".into()],
        };
        let (_token, public) =
            crate::membership::build_invite(mesh_id, &fuser, &params, minter.node_id())
                .expect("invite");
        let id = public.invite_id;
        let base = mesh_proto::now_micros() + 1;

        // The node that admitted a joiner reports the use; the create is
        // still on its way from the node that minted it.
        let joiner = Keypair::generate().expect("joiner").node_id();
        let consume = seal(
            door,
            1,
            hlc_at(base),
            &OpBody::InviteConsume {
                invite_id: id,
                node_id: joiner,
            },
        );
        assert_eq!(engine.ingest(vec![consume]).expect("ingest"), 1);
        assert_eq!(engine.read(|s| s.invite_uses_left(&id)), None);

        // An admin elsewhere revokes it, also before the create lands.
        let by = fuser.user_id();
        engine
            .author_op(OpBody::InviteRevoke {
                invite_id: id,
                by,
                signature: fuser
                    .sign(&mesh_proto::invite_revoke_signing_bytes(&id, &by))
                    .to_vec(),
            })
            .expect("revoke");

        let create = seal(
            minter,
            next_seq(&engine, minter),
            hlc_at(base + 10),
            &OpBody::InviteCreate { invite: public },
        );
        assert_eq!(engine.ingest(vec![create]).expect("ingest"), 1);
        engine.read(|s| {
            let inv = &s.invites[&id];
            assert!(
                inv.consumed_by.contains(&joiner),
                "the early use was dropped"
            );
            assert!(inv.revoked, "the early revoke was dropped");
            assert!(s.pending_invites.is_empty());
            assert_eq!(s.invite_uses_left(&id), Some(0));
        });
    }

    #[test]
    fn an_early_use_counts_against_the_budget() {
        let (_dir, engine, _founder, fuser, keys) = engine_with_members(&["minter", "door"]);
        let (minter, door) = (&keys[0], &keys[1]);
        let params = crate::membership::InviteParams {
            ttl_secs: 600,
            uses: 2,
            tags: vec![],
            bootstrap: vec!["127.0.0.1:1".into()],
        };
        let (_token, public) = crate::membership::build_invite(
            engine.genesis().body.mesh_id,
            &fuser,
            &params,
            minter.node_id(),
        )
        .expect("invite");
        let id = public.invite_id;
        let base = mesh_proto::now_micros() + 1;
        let consume = seal(
            door,
            1,
            hlc_at(base),
            &OpBody::InviteConsume {
                invite_id: id,
                node_id: door.node_id(),
            },
        );
        let create = seal(
            minter,
            1,
            hlc_at(base),
            &OpBody::InviteCreate { invite: public },
        );
        // One batch, sorted by author: which one goes first depends on the
        // two keys, and neither order may lose the use.
        assert_eq!(engine.ingest(vec![consume, create]).expect("ingest"), 2);
        assert_eq!(engine.read(|s| s.invite_uses_left(&id)), Some(1));
    }

    #[test]
    fn an_invite_nobody_published_is_refused_and_a_revoked_one_too() {
        let (_dir, engine, founder, fuser) = engine_with_founder();
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            })
            .expect("self-add");
        let params = crate::membership::InviteParams {
            ttl_secs: 600,
            uses: 5,
            tags: vec![],
            bootstrap: vec!["127.0.0.1:1".into()],
        };
        let (token, public) = crate::membership::build_invite(
            engine.genesis().body.mesh_id,
            &fuser,
            &params,
            founder.node_id(),
        )
        .expect("invite");
        let err = crate::membership::validate_token(&engine, &token).expect_err("unpublished");
        assert!(
            err.to_string().contains("no record of that invite"),
            "{err}"
        );
        assert!(crate::membership::invite_not_yet_known(&engine, &token));

        // A stranger's token is not worth a sync.
        let mut forged = token.clone();
        forged.uses = 50;
        assert!(!crate::membership::invite_not_yet_known(&engine, &forged));

        engine
            .author_op(OpBody::InviteCreate { invite: public })
            .expect("publish");
        crate::membership::validate_token(&engine, &token).expect("published");
        assert!(!crate::membership::invite_not_yet_known(&engine, &token));

        let by = fuser.user_id();
        engine
            .author_op(OpBody::InviteRevoke {
                invite_id: token.invite_id,
                by,
                signature: fuser
                    .sign(&mesh_proto::invite_revoke_signing_bytes(
                        &token.invite_id,
                        &by,
                    ))
                    .to_vec(),
            })
            .expect("revoke");
        let err = crate::membership::validate_token(&engine, &token).expect_err("revoked");
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    fn signed_secret(
        fuser: &Keypair,
        version: u32,
        created_at_micros: u64,
        recipients: &[NodeId],
    ) -> SecretRecord {
        let mut record = crate::secrets::seal_secret(
            "db-password",
            version,
            b"hunter2-hunter2",
            recipients,
            fuser.user_id(),
        )
        .expect("seal");
        record.created_at_micros = created_at_micros;
        record.signature = fuser.sign(&record.signing_bytes()).to_vec();
        record
    }

    /// A wrap addressed to `to` that opens nothing.
    fn junk_wrap(to: NodeId) -> mesh_proto::SecretWrap {
        let other = crate::secrets::make_wraps(&[7u8; 32], &[to]).expect("wrap");
        other.into_iter().next().expect("one")
    }

    fn secret_of(engine: &StateEngine) -> SecretRecord {
        engine.read(|s| s.secrets["db-password"].record.clone())
    }

    #[test]
    fn a_planted_wrap_does_not_lock_a_joiner_out_whichever_lands_first() {
        let (founder_user, holder, mallory, late) = (
            Keypair::generate().expect("user"),
            Keypair::generate().expect("holder"),
            Keypair::generate().expect("mallory"),
            Keypair::generate().expect("late"),
        );
        let record = signed_secret(&founder_user, 1, 1_000, &[holder.node_id()]);
        let real = crate::secrets::rewrap_for(&record, &holder, &[late.node_id()]).expect("heal");
        let junk = junk_wrap(late.node_id());
        let settle = |flip: bool| {
            let dir = tempfile::tempdir().expect("tempdir");
            let node = Keypair::generate().expect("node");
            let genesis =
                Genesis::create("test-mesh", &founder_user, node.node_id()).expect("genesis");
            let engine = StateEngine::open(dir.path(), node.clone(), genesis).expect("open");
            for (key, name) in [
                (&node, "alpha"),
                (&holder, "holder"),
                (&mallory, "mallory"),
                (&late, "late"),
            ] {
                engine
                    .author_op(OpBody::MemberAdd {
                        member: member_info(key, name),
                        invite: None,
                    })
                    .expect("add");
            }
            engine
                .author_op(OpBody::SecretPut {
                    record: record.clone(),
                })
                .expect("put");
            let base = mesh_proto::now_micros() + 1;
            // An older node's plain rewrap, and a current node's statement.
            let planted = seal(
                &mallory,
                1,
                hlc_at(base),
                &OpBody::SecretRewrap {
                    name: record.name.clone(),
                    version: 1,
                    wraps: vec![junk.clone()],
                },
            );
            let mut statement = real.clone();
            statement.push(crate::secrets::statement_marker(holder.node_id(), 0));
            let healed = seal(
                &holder,
                1,
                hlc_at(base),
                &OpBody::SecretRewrap {
                    name: record.name.clone(),
                    version: 1,
                    wraps: statement,
                },
            );
            for op in if flip {
                [healed, planted]
            } else {
                [planted, healed]
            } {
                assert_eq!(engine.ingest(vec![op]).expect("ingest"), 1);
            }
            let got = secret_of(&engine);
            assert_eq!(
                crate::secrets::open_secret(&got, &late)
                    .expect("the joiner was locked out")
                    .as_slice(),
                b"hunter2-hunter2"
            );
            got.wraps
        };
        assert_eq!(settle(false), settle(true), "the order changed the outcome");
    }

    #[test]
    fn a_replayed_secret_cannot_replace_its_wraps_or_undo_a_reset() {
        let (_dir, engine, _founder, fuser, keys) = engine_with_members(&["holder", "mallory"]);
        let (holder, mallory) = (&keys[0], &keys[1]);
        let t0 = mesh_proto::now_micros();
        let v1 = signed_secret(&fuser, 1, t0, &[holder.node_id()]);
        engine
            .author_op(OpBody::SecretPut { record: v1.clone() })
            .expect("put");

        // The same signed record, re-authored later with junk for the one
        // node that could read it.
        let base = mesh_proto::now_micros() + 1;
        let mut replay = v1.clone();
        replay.wraps = vec![junk_wrap(holder.node_id())];
        let replayed = seal(
            mallory,
            1,
            hlc_at(base),
            &OpBody::SecretPut { record: replay },
        );
        assert_eq!(engine.ingest(vec![replayed]).expect("ingest"), 1);
        let got = secret_of(&engine);
        assert_eq!(got.wraps.len(), 2, "the copies' wraps should both be kept");
        assert!(crate::secrets::open_secret(&got, holder).is_ok());

        // Removed, then set again: a replay of the removal changes nothing.
        let mut removal = mesh_proto::SecretRemoval {
            name: "db-password".into(),
            by: fuser.user_id(),
            at_micros: t0 + 10,
            signature: Vec::new(),
        };
        removal.signature = fuser.sign(&removal.signing_bytes()).to_vec();
        engine
            .author_op(OpBody::SecretRemove {
                removal: removal.clone(),
            })
            .expect("remove");
        assert!(engine.read(|s| s.secrets["db-password"].removed));
        let v2 = signed_secret(&fuser, 2, t0 + 20, &[holder.node_id()]);
        engine
            .author_op(OpBody::SecretPut { record: v2 })
            .expect("set again");
        assert!(!engine.read(|s| s.secrets["db-password"].removed));
        let again = seal(
            mallory,
            2,
            hlc_at(base + 1_000),
            &OpBody::SecretRemove { removal },
        );
        assert_eq!(engine.ingest(vec![again]).expect("ingest"), 1);
        assert!(
            !engine.read(|s| s.secrets["db-password"].removed),
            "a replayed removal took the secret away again"
        );

        // And the old version, replayed, does not come back.
        let old = seal(
            mallory,
            3,
            hlc_at(base + 2_000),
            &OpBody::SecretPut { record: v1 },
        );
        assert_eq!(engine.ingest(vec![old]).expect("ingest"), 1);
        assert_eq!(secret_of(&engine).version, 2);
    }

    #[test]
    fn a_statement_restates_and_retracts() {
        let (_dir, engine, _founder, fuser, keys) = engine_with_members(&["holder", "late"]);
        let (holder, late) = (&keys[0], &keys[1]);
        let record = signed_secret(&fuser, 1, mesh_proto::now_micros(), &[holder.node_id()]);
        engine
            .author_op(OpBody::SecretPut {
                record: record.clone(),
            })
            .expect("put");
        let base = mesh_proto::now_micros() + 1;
        let rewrap = |key: &Keypair,
                      seq: u64,
                      at: u64,
                      mut wraps: Vec<mesh_proto::SecretWrap>,
                      wants: Option<u32>| {
            if let Some(wants) = wants {
                wraps.push(crate::secrets::statement_marker(key.node_id(), wants));
            }
            seal(
                key,
                seq,
                hlc_at(at),
                &OpBody::SecretRewrap {
                    name: "db-password".into(),
                    version: 1,
                    wraps,
                },
            )
        };
        // The joiner asks twice.
        for (seq, n) in [(1, 1), (2, 2)] {
            let op = rewrap(late, seq, base + seq, vec![], Some(n));
            assert_eq!(engine.ingest(vec![op]).expect("ingest"), 1);
        }
        assert_eq!(
            engine.read(|s| s.secrets["db-password"].wants(&late.node_id())),
            2
        );
        // The marker is never a wrap.
        assert!(
            secret_of(&engine)
                .wraps
                .iter()
                .all(|w| !w.wrapped_key.is_empty())
        );

        // The holder heals it; its next statement restates the heal.
        let real = crate::secrets::rewrap_for(&record, holder, &[late.node_id()]).expect("heal");
        let op = rewrap(holder, 1, base + 10, real.clone(), Some(0));
        assert_eq!(engine.ingest(vec![op]).expect("ingest"), 1);
        let op = rewrap(holder, 2, base + 11, real, Some(0));
        assert_eq!(engine.ingest(vec![op]).expect("ingest"), 1);
        assert_eq!(secret_of(&engine).wraps.len(), 2);
        assert!(crate::secrets::open_secret(&secret_of(&engine), late).is_ok());

        // The joiner can open it now and says so.
        let op = rewrap(late, 3, base + 20, vec![], Some(0));
        assert_eq!(engine.ingest(vec![op]).expect("ingest"), 1);
        engine.read(|s| {
            let e = &s.secrets["db-password"];
            assert_eq!(e.wants(&late.node_id()), 0);
            assert!(!e.heals.contains_key(&late.node_id()));
        });

        // A heal for somebody this node has never heard of waits for them.
        let stranger = Keypair::generate().expect("stranger");
        let early = crate::secrets::rewrap_for(&record, holder, &[stranger.node_id()]).expect("w");
        let op = rewrap(holder, 3, base + 30, early, Some(0));
        assert_eq!(engine.ingest(vec![op]).expect("ingest"), 0);
    }

    #[test]
    fn a_merge_that_panics_is_contained_and_the_node_carries_on() {
        let (_dir, engine, founder, _fuser) = engine_with_founder();
        engine
            .author_op(OpBody::MemberAdd {
                member: member_info(&founder, "alpha"),
                invite: None,
            })
            .expect("self-add");
        let seq = next_seq(&engine, &founder);
        let bad = seal(
            &founder,
            seq,
            hlc_at(mesh_proto::now_micros() + 1),
            &OpBody::SourceSynced {
                name: "__panic_in_merge__".into(),
                commit: "c".into(),
                bundle_id: BundleId([1; 32]),
            },
        );
        assert_eq!(engine.ingest(vec![bad]).expect("ingest"), 1);
        assert_eq!(engine.contained_panics(), 1);
        // Not poisoned, and the sequence moved past it.
        assert_eq!(engine.read(|s| s.applied[&founder.node_id()]), seq);
        engine
            .author_op(OpBody::MemberUpdate {
                node_id: founder.node_id(),
                name: "still-here".into(),
                addrs: vec![],
                tags: vec![],
                slots: 1,
            })
            .expect("the engine still takes ops");
        assert_eq!(
            engine.read(|s| s.members[&founder.node_id()].info.name.clone()),
            "still-here"
        );
    }
}
