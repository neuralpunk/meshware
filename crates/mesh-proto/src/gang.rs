//! Gang scheduling: all-or-nothing reservations over a set of nodes
//! (C0–C9 §8).
//!
//! A gang answers the one question the engine could not: *"eight nodes,
//! four cores each, simultaneously, or don't start."* Nothing else in the
//! system holds a resource on behalf of a decision that has not been made
//! yet, and that is the whole of what this module adds.
//!
//! Three shapes live here, and the split matters:
//!
//! - [`GangTicket`] — seniority. A total order every node computes
//!   independently from replicated inputs, which is what makes wound-wait
//!   possible without a coordinator.
//! - [`GangPlanRecord`] — the replicated record of a gang, authored by the
//!   origin *before* the first offer goes out. It rides F0 as an ordinary
//!   op body, exactly as a fan-out plan does, so it costs no protocol
//!   minor; it is also what lets a successor re-drive an uncommitted gang
//!   after the origin dies (§8.2 P5).
//! - The lease: node-local, TTL-bounded, and *deliberately absent from the
//!   op-log*. A lease that replicated would need a distributed cleanup path
//!   the moment an origin died mid-offer, and the design's most important
//!   property is that no such path exists — the TTL is the failure model.
//!
//! The runtime side (rank, size, peers) reaches the task through reserved
//! `MESHWARE_GANG_*` environment keys, the `MESHWARE_TASK_*` idiom
//! (M9 §5.3) applied a fifth time and for the fifth time for the same
//! reason: `JobSpec` is re-decoded from a signed envelope on the executing
//! node, so growing it would mean an older executor cannot read a newer
//! origin's submission at all.

use serde::{Deserialize, Serialize};

use crate::{GangId, Hlc, JobId, JobPayload, JobPriority, LeaseId, NodeId, SignedIpc, UserId};

// ---------------------------------------------------------------------------
// Reserved environment (submission side)
// ---------------------------------------------------------------------------

/// `--gang N`: this submission is one member of an N-node gang.
///
/// Read and **removed** by the origin daemon, which is the only node that
/// runs the two-phase driver. What reaches an executor is the
/// `MESHWARE_GANG_*` runtime set below, never this.
pub const REQ_GANG: &str = "MESHWARE_TASK_GANG";

/// `--gang-placement pack|spread`. Absent means [`GangPlacement::Pack`].
pub const REQ_GANG_PLACEMENT: &str = "MESHWARE_TASK_GANG_PLACEMENT";

/// `--gang-restart`: re-run the whole gang when a member dies, instead of
/// failing it. Absent means fail-fast, which is the default because a gang
/// with a dead member is a gang with a hung barrier.
pub const REQ_GANG_RESTART: &str = "MESHWARE_TASK_GANG_RESTART";

// ---------------------------------------------------------------------------
// Reserved environment (runtime side)
// ---------------------------------------------------------------------------

/// Prefix owned by the gang runtime. Reserved on the job environment the
/// same way `MESHWARE_TASK_` is: set by the origin, rejected from
/// user-supplied env.
pub const GANG_ENV_PREFIX: &str = "MESHWARE_GANG_";

/// The gang this task belongs to, in display form.
pub const GANG_ID: &str = "MESHWARE_GANG_ID";
/// How many members the gang has. `--nnodes` in torchrun's spelling.
pub const GANG_SIZE: &str = "MESHWARE_GANG_SIZE";
/// This member's rank, `0..size`. `--node_rank` in torchrun's spelling.
pub const GANG_RANK: &str = "MESHWARE_GANG_RANK";
/// Every member's address, rank-ordered and comma-separated.
pub const GANG_NODES: &str = "MESHWARE_GANG_NODES";
/// Rank 0's `address:port`, for torchrun-shaped rendezvous.
pub const GANG_LEADER: &str = "MESHWARE_GANG_LEADER";

/// Is this environment key part of the gang runtime set?
pub fn is_gang_env(key: &str) -> bool {
    key.starts_with(GANG_ENV_PREFIX)
}

/// How many members `env` asks for, or `None` when this is not a gang
/// submission. Zero and one are not gangs: a one-node gang is a job, and
/// treating it as a gang would buy a two-phase handshake for nothing.
pub fn gang_size_of(env: &[(String, String)]) -> Option<u32> {
    env.iter()
        .find(|(k, _)| k == REQ_GANG)
        .and_then(|(_, v)| v.parse::<u32>().ok())
        .filter(|n| *n > 1)
}

/// Where the members of a gang should go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GangPlacement {
    /// Fewest nodes that can hold the gang — the default. Packing minimises
    /// the inter-task network path, which is what a gang exists for.
    #[default]
    Pack,
    /// One member per node, as widely as the mesh allows. Available for
    /// fault tolerance, and never the default.
    Spread,
}

impl GangPlacement {
    pub fn as_str(&self) -> &'static str {
        match self {
            GangPlacement::Pack => "pack",
            GangPlacement::Spread => "spread",
        }
    }

    pub fn parse(s: &str) -> Option<GangPlacement> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pack" => Some(GangPlacement::Pack),
            "spread" => Some(GangPlacement::Spread),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Lease timing
// ---------------------------------------------------------------------------

/// How long an offer asks a node to hold resources, by default.
///
/// Short, and it should be: a lease is dead weight on the node holding it,
/// and the only thing that has to fit inside it is one round of offers to
/// the rest of the candidate set plus a commit. Ten seconds is roughly two
/// orders of magnitude more than a LAN round trip and still short enough
/// that a dead origin's reservations are gone before anybody files a
/// ticket about them.
pub const DEFAULT_LEASE_TTL_SECS: u64 = 10;

/// The longest lease a node will grant, whatever the offer asks for.
///
/// **The holder caps this, not the asker.** A TTL is a promise the holder
/// keeps, and a scheduler that let a remote origin name an arbitrary one
/// would have handed every node in the mesh a way to park somebody else's
/// cores for an hour. Sixty seconds is generous for an offer round and
/// bounded enough that the worst case of a malicious or buggy origin is a
/// minute of one node's capacity.
pub const MAX_LEASE_TTL_SECS: u64 = 60;

/// How long the origin waits for the whole candidate set to answer before
/// giving up on a round and releasing what it holds.
///
/// Derived from the TTL rather than configured separately, so the two can
/// never drift into the shape where the origin is still waiting for an
/// answer about a lease that has already expired.
pub const OFFER_DEADLINE_SECS: u64 = DEFAULT_LEASE_TTL_SECS / 2;

/// How many offer rounds the origin runs before it reports the gang as
/// never started.
///
/// A bound rather than a timeout, because the property C4 is tested against
/// is *"both gangs complete, in seniority order, within a bounded number of
/// offer rounds"* — and a bound you can count is a bound you can assert.
/// Wound-wait means the senior gang wins its round outright, so a junior
/// gang needs about as many rounds as there are gangs ahead of it.
pub const MAX_OFFER_ROUNDS: u32 = 16;

// ---------------------------------------------------------------------------
// Seniority
// ---------------------------------------------------------------------------

/// A gang's place in the total order that makes wound-wait work (§8.2).
///
/// **Deterministic, total, and computable by every node independently.**
/// That is the entire requirement, and it is why the three fields are these
/// three: the priority class is replicated on the plan, the submit HLC
/// already exists and is already totally ordered across the mesh, and the
/// gang id breaks the last tie without anybody having to agree on anything.
///
/// Lower is *more senior*. `Ord` is derived, so the field order below is
/// the comparison order, and it must not be rearranged casually:
///
/// 1. `class` — priority, inverted at construction so that `high` sorts
///    first. A share answers *whose turn*; a priority answers *this first*,
///    and C1's rule that priority is the outer key holds here too.
/// 2. `submit` — the HLC the origin stamped on the plan. Monotonic in
///    submission time, which is what makes starvation impossible: a gang
///    only ever gets more senior relative to gangs that arrive later.
/// 3. `gang` — the raw id bytes. A tiebreak that never ties.
///
/// **Why seniority and not backoff.** Pure retry-with-backoff livelocks
/// exactly when it matters most: two large gangs, each holding half the
/// mesh, each releasing and re-acquiring forever. Under wound-wait the
/// senior gang always makes progress, so the system always makes progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GangTicket {
    /// Priority, inverted so lower sorts first: `high` = 0, `normal` = 1,
    /// `low` = 2.
    pub class: u8,
    pub submit: Hlc,
    pub gang: GangId,
}

impl GangTicket {
    pub fn new(priority: JobPriority, submit: Hlc, gang: GangId) -> GangTicket {
        GangTicket {
            class: match priority {
                JobPriority::High => 0,
                JobPriority::Normal => 1,
                JobPriority::Low => 2,
            },
            submit,
            gang,
        }
    }

    /// Is this gang senior to `other` — i.e. may it wound `other`'s leases?
    pub fn is_senior_to(&self, other: &GangTicket) -> bool {
        self < other
    }

    pub fn priority(&self) -> JobPriority {
        match self.class {
            0 => JobPriority::High,
            2 => JobPriority::Low,
            _ => JobPriority::Normal,
        }
    }
}

// ---------------------------------------------------------------------------
// The offer protocol (wire, CLUSTER_MINOR)
// ---------------------------------------------------------------------------

/// Why a node would not hold a reservation.
///
/// A refusal is an *answer*, not a transport failure, and it rides the
/// accepted response for the reason [`crate::UpgradeState::refused`] does:
/// the driver has to tell "this node said no" apart from "this node did not
/// reply", and only the node can say which of its own conditions failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GangRefusal {
    /// The node has already promised these resources — to running work, to
    /// queued work, or to another lease it may not yield.
    NoRoom,
    /// Another gang holds the resources and this one may not take them.
    ///
    /// Named, because "who is ahead of me" is the question an operator
    /// staring at a stuck gang actually has, and a scheduler that knows the
    /// answer and does not say it is the thing C8 exists to stop.
    ///
    /// Almost always a gang that is genuinely senior in the ticket order.
    /// The exception is a *junior* gang that has already committed: a
    /// committed lease is a gang that has started elsewhere, and taking it
    /// back would convert a scheduling delay into lost work. Reporting that
    /// case as plain "no room" would be true about capacity and useless
    /// about why, so it is reported here.
    Senior { gang: GangId },
    /// The node is draining, revoked, or otherwise not taking work.
    NotAccepting { reason: String },
}

impl GangRefusal {
    pub fn message(&self) -> String {
        match self {
            GangRefusal::NoRoom => "no room for the reservation".to_string(),
            GangRefusal::Senior { gang } => {
                format!("held by gang {}", gang.short())
            }
            GangRefusal::NotAccepting { reason } => reason.clone(),
        }
    }
}

/// Answer to `Request::GangOffer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GangOfferReply {
    /// The reservation is held. It expires on its own at the TTL the offer
    /// named, so an origin that dies between offer and commit costs the
    /// mesh one TTL and no cleanup path.
    Accepted {
        lease: LeaseId,
    },
    Refused {
        reason: GangRefusal,
    },
}

// ---------------------------------------------------------------------------
// The replicated record
// ---------------------------------------------------------------------------

/// One planned member of a gang. The job id is fixed at plan time for
/// exactly the reason [`crate::PlannedTarget`]'s is: a re-dispatch by an
/// adopting successor must not double-run, and executors treat an
/// already-journaled job id as accepted.
///
/// `rank` is the index in this vector, so the runtime environment is a
/// function of the plan and every driver builds the identical one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GangMember {
    pub job_id: JobId,
    pub node_id: NodeId,
    pub node_name: String,
    /// Where rank 0 tells the others to meet: this member's reachable
    /// `address:port`, resolved on the origin at plan time.
    pub addr: String,
}

/// Everything a successor needs to finish a gang its origin started
/// (§8.2 P5) — and everything `meshnode gangs` needs to render one.
///
/// Replicated as [`crate::OpBody::GangPlan`], authored **before the first
/// offer**. Two properties follow from that ordering and both are
/// load-bearing: a successor can tell an uncommitted gang from a committed
/// one without asking anybody, and the leases the dead origin was holding
/// expire on their own while it works that out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GangPlanRecord {
    pub gang: GangId,
    pub ticket: GangTicket,
    pub payload: JobPayload,
    pub env: Vec<(String, String)>,
    pub timeout_secs: Option<u64>,
    pub priority: JobPriority,
    /// The origin node — the initial owner (MASTERPLAN §10.3).
    pub submitter: NodeId,
    pub submitter_user: UserId,
    /// The user's signed request envelope; travels into every member spec.
    pub auth: SignedIpc,
    /// Rank-ordered. `members.len()` is the gang size, so a plan cannot
    /// disagree with itself about how big the gang is.
    pub members: Vec<GangMember>,
    pub placement: GangPlacement,
    /// Re-run the whole gang if a member dies, rather than failing it.
    pub restart: bool,
    /// Which re-run this is, from zero.
    ///
    /// A restart is a **new gang** — new id, new member job ids — rather
    /// than a re-dispatch of this one, because the members that did not die
    /// have already journaled their job ids and an executor treats a
    /// journaled id as accepted. Re-sending them would ack instantly and
    /// run nothing. Carrying the attempt number is what bounds the chain:
    /// a job that dies on startup would otherwise restart the gang forever.
    pub attempt: u32,
    pub created_at_micros: u64,
}

/// How many times `--gang-restart` will re-run a gang before giving up.
///
/// Small, and it should be: a restart exists for the machine that fell
/// over, not for the training script that crashes on its own input. Three
/// attempts distinguishes those two cases and costs at most three
/// negotiations.
pub const MAX_GANG_ATTEMPTS: u32 = 3;

impl GangPlanRecord {
    pub fn size(&self) -> u32 {
        u32::try_from(self.members.len()).unwrap_or(u32::MAX)
    }

    /// The runtime environment for one member (§8.2), appended to the
    /// submission's own environment.
    ///
    /// This is what makes
    /// `torchrun --nnodes=$MESHWARE_GANG_SIZE --node_rank=$MESHWARE_GANG_RANK`
    /// work with no meshware-specific code in the user's training script,
    /// and a gang whose members cannot find each other is a gang that
    /// exists for nothing.
    pub fn runtime_env(&self, rank: usize) -> Vec<(String, String)> {
        let nodes: Vec<&str> = self.members.iter().map(|m| m.addr.as_str()).collect();
        vec![
            (GANG_ID.to_string(), self.gang.to_display_string()),
            (GANG_SIZE.to_string(), self.size().to_string()),
            (GANG_RANK.to_string(), rank.to_string()),
            (GANG_NODES.to_string(), nodes.join(",")),
            (
                GANG_LEADER.to_string(),
                self.members
                    .first()
                    .map(|m| m.addr.clone())
                    .unwrap_or_default(),
            ),
        ]
    }
}

/// How a gang ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GangOutcome {
    /// Every member ran and every member succeeded.
    Succeeded,
    /// It started, and at least one member did not succeed.
    Failed,
    /// It never started: the offer phase never got `N` acceptances inside
    /// the deadline. **Nothing ran**, which is the guarantee the milestone
    /// is for, so this is a distinct outcome rather than a kind of failure.
    NeverStarted,
    /// A member died while the gang was running and `--gang-restart` was
    /// not set, so the whole gang was torn down.
    MemberDied,
}

impl GangOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            GangOutcome::Succeeded => "succeeded",
            GangOutcome::Failed => "failed",
            GangOutcome::NeverStarted => "never-started",
            GangOutcome::MemberDied => "member-died",
        }
    }
}

// ---------------------------------------------------------------------------
// Views (IPC)
// ---------------------------------------------------------------------------

/// One row of `meshnode gangs ls`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GangRow {
    pub gang: GangId,
    pub ticket: GangTicket,
    pub preview: String,
    pub size: u32,
    pub placement: GangPlacement,
    pub submitter_user: UserId,
    pub username: String,
    /// The node currently responsible for driving it.
    pub owner: NodeId,
    pub owner_name: String,
    /// `offering` while the reservation is in flight, `running` once it
    /// committed, otherwise the outcome.
    pub state: String,
    pub outcome: Option<GangOutcome>,
    /// How many members are currently accounted for: leases held during the
    /// offer phase, running or finished jobs afterwards.
    pub held: u32,
    pub created_at_micros: u64,
}

/// `meshnode gangs show <id>`: the row, plus who is holding what and — when
/// the gang is stuck — who is ahead of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GangView {
    pub row: GangRow,
    pub members: Vec<GangMemberView>,
    /// Gangs senior to this one that are themselves still unfinished, most
    /// senior first. Empty is the answer to "why is mine not running" being
    /// *not seniority*, which is worth being able to say.
    pub ahead: Vec<GangId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GangMemberView {
    pub rank: u32,
    pub job_id: JobId,
    pub node_id: NodeId,
    pub node_name: String,
    pub addr: String,
    /// The member's job state once it has one; `None` while the gang is
    /// still only a reservation.
    pub state: Option<crate::JobState>,
}

/// One soft reservation, as `meshnode top` and the Director report it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LeaseView {
    pub lease: LeaseId,
    pub gang: GangId,
    pub ticket: GangTicket,
    pub cpus: u32,
    pub mem_mb: u64,
    pub expires_at_micros: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hlc(w: u64) -> Hlc {
        Hlc {
            wall_micros: w,
            logical: 0,
        }
    }

    #[test]
    fn priority_is_the_outer_key_and_time_breaks_the_rest() {
        let a = GangTicket::new(JobPriority::Normal, hlc(100), GangId([1u8; 16]));
        let b = GangTicket::new(JobPriority::High, hlc(900), GangId([2u8; 16]));
        // A later high-priority gang outranks an earlier normal one: a
        // priority answers "this first" and must not be overruled by
        // arrival order (C1's rule, applied to gangs).
        assert!(b.is_senior_to(&a), "{b:?} should outrank {a:?}");

        let early = GangTicket::new(JobPriority::Normal, hlc(10), GangId([9u8; 16]));
        let late = GangTicket::new(JobPriority::Normal, hlc(11), GangId([0u8; 16]));
        assert!(early.is_senior_to(&late), "within a class, time decides");

        // The order is total: no two distinct gangs ever tie, so every node
        // computes the same wound-wait decision without coordinating.
        let x = GangTicket::new(JobPriority::Normal, hlc(10), GangId([1u8; 16]));
        let y = GangTicket::new(JobPriority::Normal, hlc(10), GangId([2u8; 16]));
        assert!(x.is_senior_to(&y) ^ y.is_senior_to(&x));
        assert!(!x.is_senior_to(&x), "seniority is irreflexive");
    }

    #[test]
    fn seniority_is_monotonic_in_submit_time() {
        // The starvation argument, as an assertion: a gang can only ever
        // become *more* senior relative to gangs that arrive after it.
        let mine = GangTicket::new(JobPriority::Normal, hlc(500), GangId([7u8; 16]));
        for later in 501..600u64 {
            let other = GangTicket::new(JobPriority::Normal, hlc(later), GangId([0u8; 16]));
            assert!(mine.is_senior_to(&other));
        }
    }

    #[test]
    fn a_gang_of_one_is_not_a_gang() {
        let env = |n: &str| vec![(REQ_GANG.to_string(), n.to_string())];
        assert_eq!(gang_size_of(&env("8")), Some(8));
        assert_eq!(gang_size_of(&env("1")), None);
        assert_eq!(gang_size_of(&env("0")), None);
        assert_eq!(gang_size_of(&env("nonsense")), None);
        assert_eq!(gang_size_of(&[]), None);
    }

    #[test]
    fn placement_round_trips_its_spelling() {
        for p in [GangPlacement::Pack, GangPlacement::Spread] {
            assert_eq!(GangPlacement::parse(p.as_str()), Some(p));
        }
        assert_eq!(GangPlacement::parse("PACK"), Some(GangPlacement::Pack));
        assert_eq!(GangPlacement::parse("binpack"), None);
    }
}
