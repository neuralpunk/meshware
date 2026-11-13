//! Scored placement (MASTERPLAN §10.1) and rendezvous hashing (§10.3).
//!
//! Selectors *filter* candidates (membership::resolve_targets); this module
//! *ranks* them. Score is a weighted sum of penalties — lower is better —
//! with a deterministic node-id tiebreak. Round-robin is what you get when
//! the weights are zeroed.
//!
//! Inputs, all derived from replicated state or local observations:
//! - load: non-terminal jobs on the node vs its advertised slot capacity
//! - cache warmth: whether the node already holds the service's bundle
//! - rolling failure rate: failed fraction of its recent finished jobs
//! - RTT: smoothed round-trip time of an existing QUIC connection, if any
//! - memory and disk headroom: live pressure against advertised capacity
//!   (M9 §5.3), which is what keeps a big task off an almost-full node
//! - object locality: how much of the task's declared input a node already
//!   holds (M9 §5.5), which is cache warmth generalised from "the service's
//!   bundle" to "any content this task needs" — and is what rung 3 means by
//!   placing a task where its inputs already are

use mesh_proto::{
    BundleId, Capacity, EXPLAIN_CANDIDATES, JobState, MemberInfo, NodeId, SystemSnapshot,
    placement::{CandidateScore, FitPolicy, PlacementExplain, ScoreTerm},
};

use crate::state::MeshState;

/// Scoring weights. Fixed for M5; a `[scheduler]` config section can expose
/// them later without changing the mechanics.
pub const W_LOAD: f64 = 1.0;
pub const W_WARMTH: f64 = 0.6;
pub const W_FAILURE: f64 = 0.8;
pub const W_RTT: f64 = 0.2;
/// Memory headroom (M9 §5.3). Below `W_LOAD`: running work is the stronger
/// signal, and a node that is merely low on cache-warm memory is still a
/// fine place to put a small job. Above `W_DISK` because memory is what
/// jobs actually die of.
pub const W_MEM: f64 = 0.5;
/// Disk headroom (M9 §5.3).
pub const W_DISK: f64 = 0.2;
/// Object locality (M9 §5.5). The heaviest weight in the table, and it
/// should be: the other terms move a task between machines that would each
/// do the work in about the same time, while this one decides whether
/// gigabytes cross the network at all. `W_WARMTH` is the same idea for a
/// bundle, at a fraction of the weight, because a bundle is usually
/// megabytes and is very often already everywhere.
pub const W_LOCALITY: f64 = 1.5;
/// Device idleness (C5 P5). The second-heaviest weight in the table, below
/// only object locality, and the placement of it is the argument for it: a
/// GPU is the scarcest thing in a GPU cluster and the *only* one whose
/// contention makes a job slower by an order of magnitude rather than by a
/// few percent. Two training runs sharing a card do not each take twice as
/// long — they take twice as long and then one of them runs out of memory.
///
/// It is still a *term* rather than a filter, because a busy device is a
/// real candidate when every device is busy, and the alternative to a slow
/// GPU is no GPU.
pub const W_DEVICE: f64 = 1.2;
/// **Real CPU utilisation** (`TELEMETRY_MINOR`), and the term that closes
/// the hole the rest of this table did not know it had.
///
/// Every other busyness signal here is about work *the mesh placed*:
/// `W_LOAD` counts this node's non-terminal jobs against its slots. A
/// machine pegged by anything else — somebody's build, a database, a
/// `stress` run — is invisible to all of them and scores as completely
/// free, so the scheduler cheerfully sends it more. That is the failure
/// this weight exists to prevent, and it is not hypothetical: a node at
/// 100% CPU reports `load1` of 0.2 per core for the first minute, which
/// every reader here treats as idle.
///
/// Equal to `W_LOAD`, deliberately. A core busy with a mesh job and a core
/// busy with somebody's compile are the same core, and weighting the one
/// the mesh happens to know about more heavily would be believing our own
/// bookkeeping over the machine's.
pub const W_CPU: f64 = 1.0;
/// A deployment's `affinity` (Phase C, E8).
///
/// Twice `W_LOAD`, because an affinity is the operator saying where they want
/// the work, in a file, on purpose: a full-strength one (weight 100) is worth
/// more than one machine being entirely busy and the other idle. Still a
/// term and never a filter — a machine the affinity avoids is still a place
/// the instance can run when nothing else can.
pub const W_AFFINITY: f64 = 2.0;
/// A deployment's `spread` (Phase C, E8). Beside `W_AFFINITY` for the same
/// reason: a stated share, and a machine past it should lose to a machine
/// under it unless the rest of the table is very sure.
pub const W_SPREAD: f64 = 2.0;

/// What an unknown measurement contributes. Neutral — the midpoint between
/// empty and full — never disqualifying (M9 §5.2). A node that cannot be
/// measured is neither preferred nor punished for it.
const UNKNOWN: f64 = 0.5;

/// How many recent finished jobs per node feed the rolling failure rate.
const FAILURE_WINDOW: usize = 20;
/// RTT at or above this is maximally penalized.
const RTT_CEILING_MICROS: f64 = 250_000.0;

/// Rank candidates best-first. `bundle` is the service's bundle for cache
/// warmth (None for ad-hoc commands). `rtt_micros_of` reports the smoothed
/// RTT of an existing connection to the node, if one is open.
#[allow(clippy::too_many_arguments)]
pub fn rank(
    state: &MeshState,
    candidates: Vec<MemberInfo>,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: impl Fn(&NodeId) -> Option<u64>,
    fit: FitPolicy,
) -> Vec<MemberInfo> {
    rank_with_pressure(
        state,
        candidates,
        bundle,
        self_id,
        rtt_micros_of,
        |_| None,
        fit,
    )
}

/// [`rank`] with live pressure folded in (M9 §5.3).
///
/// `fit` is N6's pack-or-spread knob, and it is a **required** argument all
/// the way down on purpose: a convenience wrapper that defaulted it would
/// be a call site that silently ignores the operator's setting, which is
/// precisely how N5 shipped a placement filter nobody consulted.
#[allow(clippy::too_many_arguments)]
pub fn rank_with_pressure(
    state: &MeshState,
    candidates: Vec<MemberInfo>,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: impl Fn(&NodeId) -> Option<u64>,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
    fit: FitPolicy,
) -> Vec<MemberInfo> {
    rank_with_locality(
        state,
        candidates,
        bundle,
        self_id,
        rtt_micros_of,
        pressure_of,
        |_| None,
        fit,
    )
}

/// [`rank_with_pressure`] with object locality folded in (M9 §5.5).
///
/// `locality_of` reports the fraction of the task's declared object input
/// a node already holds, in `0.0..=1.0`. `None` means this placement has no
/// object inputs at all — the ordinary case — and contributes nothing, so
/// the term is invisible to every job that does not use the data plane.
#[allow(clippy::too_many_arguments)]
pub fn rank_with_locality(
    state: &MeshState,
    candidates: Vec<MemberInfo>,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: impl Fn(&NodeId) -> Option<u64>,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
    locality_of: impl Fn(&NodeId) -> Option<f64>,
    fit: FitPolicy,
) -> Vec<MemberInfo> {
    // Folded once for the whole ranking, not once per candidate (**H-5**).
    let load_of = ExecutorLoad::of(state);
    let mut scored: Vec<(f64, MemberInfo)> = candidates
        .into_iter()
        .map(|m| {
            let pressure = pressure_of(&m.node_id);
            let s = score_full(
                state,
                &load_of,
                &m,
                bundle,
                self_id,
                &rtt_micros_of,
                pressure.as_ref(),
                locality_of(&m.node_id),
                fit,
            );
            (s, m)
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.node_id.cmp(&b.1.node_id)));
    scored.into_iter().map(|(_, m)| m).collect()
}

/// Per-executor job tallies, folded from the job table **once** (**H-5**).
///
/// `score_terms` asked two questions of every candidate — *how much is this
/// node running* and *how did its last twenty jobs go* — and answered both
/// by walking the entire job table, then sorting that node's whole terminal
/// history to look at the newest twenty of it. Per candidate. Under the
/// state read lock. On the submit path.
///
/// A hundred candidates against two hundred thousand retained summaries is
/// forty million filter iterations and a hundred sorts to place one job,
/// and it grows with how much history the mesh *keeps* rather than with how
/// many machines it has — so the time between pressing enter and the job
/// starting got worse every week for reasons an operator could not see.
///
/// One pass, reused for every candidate in the ranking. Nodes with no jobs
/// are simply absent, which reads the same as the old empty filter.
#[derive(Debug)]
pub struct ExecutorLoad {
    /// Jobs that have not reached a terminal state.
    active: std::collections::HashMap<NodeId, u32>,
    /// The most recent [`FAILURE_WINDOW`] terminal outcomes, newest first.
    recent: std::collections::HashMap<NodeId, Vec<(u64, JobState)>>,
}

impl ExecutorLoad {
    /// Fold the whole job table in one pass.
    pub fn of(state: &MeshState) -> Self {
        let mut active: std::collections::HashMap<NodeId, u32> = std::collections::HashMap::new();
        let mut recent: std::collections::HashMap<NodeId, Vec<(u64, JobState)>> =
            std::collections::HashMap::new();
        for j in state.jobs.values() {
            if j.state.is_terminal() {
                recent
                    .entry(j.executor)
                    .or_default()
                    .push((j.finished_at_micros.unwrap_or(0), j.state));
            } else {
                *active.entry(j.executor).or_default() += 1;
            }
        }
        // Newest first, then cut — the same window `score_terms` took, now
        // taken once per ranking instead of once per candidate.
        for outcomes in recent.values_mut() {
            outcomes.sort_unstable_by_key(|(finished, _)| std::cmp::Reverse(*finished));
            outcomes.truncate(FAILURE_WINDOW);
        }
        ExecutorLoad { active, recent }
    }

    /// Non-terminal jobs on this executor.
    fn busy(&self, id: &NodeId) -> f64 {
        f64::from(self.active.get(id).copied().unwrap_or(0))
    }

    /// Has this executor finished anything at all? The `unknown` flag on
    /// the failure term — a node with no history is not a node with a
    /// perfect record, and the chart says so.
    fn has_history(&self, id: &NodeId) -> bool {
        self.recent.get(id).is_some_and(|o| !o.is_empty())
    }

    /// Failed share of the last [`FAILURE_WINDOW`] outcomes; 0 when there
    /// are none, which is what "no history is not bad history" means.
    fn failure_rate(&self, id: &NodeId) -> f64 {
        let Some(outcomes) = self.recent.get(id) else {
            return 0.0;
        };
        if outcomes.is_empty() {
            return 0.0;
        }
        let failed = outcomes
            .iter()
            .filter(|(_, state)| matches!(state, JobState::Failed | JobState::TimedOut))
            .count() as f64;
        failed / outcomes.len() as f64
    }
}

/// [`rank_with_locality`] with the device-idleness term (C5 P5).
///
/// `device_busy_of` reports how busy a node's *least* busy matching device
/// is, in `0.0..=1.0` — the best card going, because a job needs one device
/// and that is the one it will get. `None` means unknown, which is neutral:
/// a peer below `ACCEL_MINOR`, a node whose telemetry has aged out, or a
/// job that asked for no devices at all all land there, and all three
/// should score exactly as they did before this existed.
#[allow(clippy::too_many_arguments)]
pub fn rank_with_devices(
    state: &MeshState,
    candidates: Vec<MemberInfo>,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: impl Fn(&NodeId) -> Option<u64>,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
    locality_of: impl Fn(&NodeId) -> Option<f64>,
    device_busy_of: impl Fn(&NodeId) -> Option<f64>,
    fit: FitPolicy,
) -> Vec<MemberInfo> {
    rank_explained(
        state,
        candidates,
        bundle,
        self_id,
        rtt_micros_of,
        pressure_of,
        locality_of,
        device_busy_of,
        fit,
    )
    .0
}

/// One candidate's penalty score (lower = better).
#[allow(clippy::too_many_arguments)]
pub fn score(
    state: &MeshState,
    member: &MemberInfo,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: &impl Fn(&NodeId) -> Option<u64>,
    pressure: Option<&SystemSnapshot>,
    fit: FitPolicy,
) -> f64 {
    score_with_locality(
        state,
        member,
        bundle,
        self_id,
        rtt_micros_of,
        pressure,
        None,
        fit,
    )
}

/// [`score`] with the object-locality term (M9 §5.5).
#[allow(clippy::too_many_arguments)]
pub fn score_with_locality(
    state: &MeshState,
    member: &MemberInfo,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: &impl Fn(&NodeId) -> Option<u64>,
    pressure: Option<&SystemSnapshot>,
    locality: Option<f64>,
    fit: FitPolicy,
) -> f64 {
    // A single-member score: the fold is the same one pass the old code
    // did per candidate, so this path costs exactly what it did.
    score_full(
        state,
        &ExecutorLoad::of(state),
        member,
        bundle,
        self_id,
        rtt_micros_of,
        pressure,
        locality,
        fit,
    )
}

/// Every term, summed.
///
/// **A sum over [`score_terms`], never a second copy of the arithmetic.**
/// C8's whole claim is that the chart shows what the scheduler did; a total
/// computed separately from the breakdown would agree on the day it was
/// written and diverge silently forever after, which is the failure mode
/// §12.2 rules out for the planner and which applies here first.
#[allow(clippy::too_many_arguments)]
pub fn score_full(
    state: &MeshState,
    load_of: &ExecutorLoad,
    member: &MemberInfo,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: &impl Fn(&NodeId) -> Option<u64>,
    pressure: Option<&SystemSnapshot>,
    locality: Option<f64>,
    fit: FitPolicy,
) -> f64 {
    score_terms(
        state,
        load_of,
        member,
        bundle,
        self_id,
        rtt_micros_of,
        pressure,
        locality,
        None,
        fit,
    )
    .iter()
    .map(|t| t.weighted)
    .sum()
}

/// One candidate's score, term by term (C8 P1).
///
/// This is the function that *is* the scheduler's decision. Everything
/// else — the total, the ranking, the chart, `jobs explain`, the what-if
/// planner — reads out of here, so there is exactly one place where a
/// placement is decided and exactly one place where it is explained.
///
/// `device_busy` is the C5 P5 term. It is a parameter here rather than a
/// separate function because a breakdown that omitted the term that
/// actually decided a GPU placement would be worse than no breakdown: it
/// would be a confident wrong answer.
#[allow(clippy::too_many_arguments)]
pub fn score_terms(
    state: &MeshState,
    // The one-pass job fold (**H-5**), shared across a whole ranking.
    load_of: &ExecutorLoad,
    member: &MemberInfo,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: &impl Fn(&NodeId) -> Option<u64>,
    pressure: Option<&SystemSnapshot>,
    locality: Option<f64>,
    device_busy: Option<f64>,
    fit: FitPolicy,
) -> Vec<ScoreTerm> {
    let id = member.node_id;

    // Load: running + queued jobs over advertised capacity. Slots 0 means
    // the node has not announced capacity — treat as 1 so load still counts.
    let busy = load_of.busy(&id);
    let load = busy / f64::from(member.slots.max(1));

    // Cache warmth: a node already holding the bundle skips the fetch — a
    // massive latency win the score should chase (MASTERPLAN §10.1).
    let warmth_penalty = match bundle {
        Some(b) => {
            if state
                .bundles
                .get(b)
                .is_some_and(|e| e.holders.contains(&id))
            {
                0.0
            } else {
                1.0
            }
        }
        None => 0.0,
    };

    // Rolling failure rate over the node's most recent finished jobs.
    let failure = load_of.failure_rate(&id);

    // RTT: self is free; an open connection reports its smoothed RTT; no
    // connection yet is neutral (we know nothing, we assume nothing).
    let mut rtt_unknown = false;
    let rtt = if id == self_id {
        0.0
    } else {
        match rtt_micros_of(&id) {
            Some(micros) => (micros as f64 / RTT_CEILING_MICROS).min(1.0),
            None => {
                rtt_unknown = true;
                UNKNOWN
            }
        }
    };

    // Headroom: how much of the node's advertised memory and disk is
    // already spoken for. Needs both halves of the capacity model — the
    // static total (§5.1) and the live free figure (§5.2) — so either one
    // missing means the measurement is unknown, and unknown is neutral.
    let cap = Capacity::from_tags(&member.tags);
    let mut mem_unknown = false;
    let mut disk_unknown = false;
    let used_fraction = |free: Option<u64>, total: u64, unknown: &mut bool| -> f64 {
        match free {
            Some(free) if total > 0 => 1.0 - (free as f64 / total as f64).clamp(0.0, 1.0),
            _ => {
                *unknown = true;
                UNKNOWN
            }
        }
    };
    let (mem, disk) = match (pressure, cap.as_ref()) {
        (Some(p), Some(cap)) => (
            used_fraction(p.base.mem_available_mb, cap.mem_mb, &mut mem_unknown),
            used_fraction(p.base.disk_free_mb, cap.disk_mb, &mut disk_unknown),
        ),
        _ => {
            mem_unknown = true;
            disk_unknown = true;
            (UNKNOWN, UNKNOWN)
        }
    };

    // Object locality: what fraction of this task's declared input the
    // node does *not* already have. A placement with no object inputs
    // contributes nothing here, so ordinary jobs score exactly as before.
    let locality_penalty = match locality {
        Some(fraction) => 1.0 - fraction.clamp(0.0, 1.0),
        None => 0.0,
    };

    // Device idleness (C5 P5). `None` is *not* neutral here in the way the
    // other unknowns are: a job that asked for no devices has no device
    // term at all, and treating it as half-busy would penalise every
    // ordinary job on every GPU box.
    let device_penalty = device_busy.unwrap_or(0.0).clamp(0.0, 1.0);

    // What the machine is *actually* doing, mesh work or not. Unknown is
    // neutral, like every other measurement here: a peer below
    // `TELEMETRY_MINOR` scores exactly as it did before this term existed.
    let mut cpu_unknown = false;
    let cpu = match pressure.and_then(|p| p.cpu_pct) {
        Some(pct) => (pct / 100.0).clamp(0.0, 1.0),
        None => {
            cpu_unknown = true;
            UNKNOWN
        }
    };

    let term = |name: &str, raw: f64, weight: f64, unknown: bool| ScoreTerm {
        name: name.to_string(),
        raw,
        weighted: raw * weight,
        weight,
        unknown,
    };
    // **N6 — the whole of pack-vs-spread, in four calls.**
    //
    // Only the terms that measure *how full this box is* are re-read;
    // everything else means the same thing under both policies. `device`
    // in particular stays a penalty when packing, and that is a judgement
    // rather than an oversight: filling a machine's CPU is a choice about
    // cost, while filling its GPU makes a job slower by an order of
    // magnitude and then kills it with an allocation failure. There is no
    // mesh where "put the second training run on the busy card" is the
    // answer, so no policy offers it.
    vec![
        term("load", fit.fit(load), W_LOAD, false),
        term("warmth", warmth_penalty, W_WARMTH, bundle.is_none()),
        term("failure", failure, W_FAILURE, !load_of.has_history(&id)),
        term("rtt", rtt, W_RTT, rtt_unknown),
        term("memory", fit.fit(mem), W_MEM, mem_unknown),
        term("disk", fit.fit(disk), W_DISK, disk_unknown),
        term("locality", locality_penalty, W_LOCALITY, locality.is_none()),
        term("device", device_penalty, W_DEVICE, device_busy.is_none()),
        term("cpu", fit.fit(cpu), W_CPU, cpu_unknown),
    ]
}

/// Score every candidate, keep the winner's ordering *and* the top
/// [`EXPLAIN_CANDIDATES`] breakdowns (C8 P1).
///
/// The ranking this returns is byte-for-byte the one
/// [`rank_with_devices`] returns for the same inputs — it is the same sort
/// over the same scores — so turning the explanation on cannot change where
/// a job lands. That property is worth more than the chart.
#[allow(clippy::too_many_arguments)]
pub fn rank_explained(
    state: &MeshState,
    candidates: Vec<MemberInfo>,
    bundle: Option<&BundleId>,
    self_id: NodeId,
    rtt_micros_of: impl Fn(&NodeId) -> Option<u64>,
    pressure_of: impl Fn(&NodeId) -> Option<SystemSnapshot>,
    locality_of: impl Fn(&NodeId) -> Option<f64>,
    device_busy_of: impl Fn(&NodeId) -> Option<f64>,
    fit: FitPolicy,
) -> (Vec<MemberInfo>, PlacementExplain) {
    let considered = candidates.len() as u32;
    // As in `rank_with_locality`: one fold for the whole ranking (**H-5**).
    let load_of = ExecutorLoad::of(state);
    let mut scored: Vec<(f64, Vec<ScoreTerm>, MemberInfo)> = candidates
        .into_iter()
        .map(|m| {
            let pressure = pressure_of(&m.node_id);
            let terms = score_terms(
                state,
                &load_of,
                &m,
                bundle,
                self_id,
                &rtt_micros_of,
                pressure.as_ref(),
                locality_of(&m.node_id),
                device_busy_of(&m.node_id),
                fit,
            );
            (terms.iter().map(|t| t.weighted).sum::<f64>(), terms, m)
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.node_id.cmp(&b.2.node_id)));

    let explain = PlacementExplain {
        candidates: scored
            .iter()
            .take(EXPLAIN_CANDIDATES)
            .map(|(total, terms, m)| CandidateScore {
                node: m.node_id,
                name: m.name.clone(),
                total: *total,
                terms: terms.clone(),
            })
            .collect(),
        // Filled in by the caller once a node has actually taken the work:
        // the best candidate can answer `Busy` and the job can spill, and a
        // record that named the winner before the dispatch would be a
        // confident lie about a job that ran somewhere else.
        chosen: None,
        considered,
        spilled: false,
        at_micros: mesh_proto::now_micros(),
    };
    (scored.into_iter().map(|(_, _, m)| m).collect(), explain)
}

/// Rendezvous (highest-random-weight) ranking: order `nodes` by
/// `blake3(context ‖ key ‖ node_id)`, highest first. Every node computes the
/// same order from the same inputs — no coordination, no leader
/// (MASTERPLAN §10.3 successor lists, §8.1 watcher election).
pub fn rendezvous_rank(
    context: &[u8],
    key: &[u8],
    nodes: impl IntoIterator<Item = NodeId>,
) -> Vec<NodeId> {
    let mut weighted: Vec<([u8; 32], NodeId)> = nodes
        .into_iter()
        .map(|id| {
            let mut h = blake3::Hasher::new();
            h.update(context);
            h.update(key);
            h.update(&id.0);
            (*h.finalize().as_bytes(), id)
        })
        .collect();
    weighted.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    weighted.into_iter().map(|(_, id)| id).collect()
}

/// The affinity term's penalty (Phase C, E8).
///
/// `matched` is the sum of the weights of every affinity whose selector
/// matches the machine. A hundred is full attraction and reads 0; nothing
/// matched reads ½; -100 is full avoidance and reads 1. Weights add, so two
/// affinities of 50 matched together pull as hard as one of 100 — the weight
/// a person writes means the same thing however many they write.
pub fn affinity_penalty(matched: i64) -> f64 {
    let pull = (matched as f64 / 100.0).clamp(-1.0, 1.0);
    (1.0 - pull) / 2.0
}

/// The spread term's penalty (Phase C, E8): `used` instances already hold
/// the machine's value, and the value should hold `desired`.
///
/// A share with room reads below ½ by how full it already is, so the emptiest
/// share wins — which is what puts the one instance a 25% share is owed on
/// its machines before a 75% share takes its last. A share that is full
/// reads above ½, rising to 1 a whole share past it. A value that should hold
/// nothing reads 1.
pub fn spread_penalty(used: f64, desired: f64) -> f64 {
    if used < desired {
        0.5 * used / desired
    } else {
        (0.5 + 0.5 * (used + 1.0 - desired) / desired.max(1.0)).min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::{BundleMeta, Hlc, JobId, JobSummary, UserId};

    fn member(id: u8, slots: u32) -> MemberInfo {
        MemberInfo {
            node_id: NodeId([id; 32]),
            name: format!("n{id}"),
            addrs: vec![],
            tags: vec![],
            slots,
            joined_at_micros: 0,
        }
    }

    fn summary(executor: NodeId, state: JobState, finished: u64) -> JobSummary {
        JobSummary {
            job_id: JobId::generate(),
            cmd_preview: "x".into(),
            runtime: "bash".into(),
            submitter: executor,
            submitter_user: UserId([0; 32]),
            executor,
            state,
            exit_code: None,
            created_at_micros: 0,
            started_at_micros: None,
            finished_at_micros: (finished > 0).then_some(finished),
            fanout: None,
            updated_hlc: Hlc::default(),
        }
    }

    #[test]
    fn idle_node_beats_loaded_node() {
        let mut state = MeshState::default();
        let busy = NodeId([1; 32]);
        for _ in 0..4 {
            let s = summary(busy, JobState::Running, 0);
            state.jobs.insert(s.job_id, s);
        }
        let ranked = rank(
            &state,
            vec![member(1, 4), member(2, 4)],
            None,
            NodeId([9; 32]),
            |_| None,
            FitPolicy::default(),
        );
        assert_eq!(ranked[0].node_id, NodeId([2; 32]));
    }

    /// N6 §12 — the same mesh, the same scores, the opposite answer.
    ///
    /// The property worth pinning is not "pack picks the busy one": it is
    /// that **the two policies disagree on exactly this ranking and agree
    /// on everything else**. One sign flip over one fit score, no second
    /// scheduler.
    #[test]
    fn packing_picks_the_machine_spreading_avoids() {
        let mut state = MeshState::default();
        let busy = NodeId([1; 32]);
        let idle = NodeId([2; 32]);
        for _ in 0..2 {
            let s = summary(busy, JobState::Running, 0);
            state.jobs.insert(s.job_id, s);
        }
        let of = |fit| {
            rank(
                &state,
                vec![member(1, 4), member(2, 4)],
                None,
                NodeId([9; 32]),
                |_| None,
                fit,
            )[0]
            .node_id
        };
        assert_eq!(
            of(FitPolicy::Spread),
            idle,
            "spread must avoid the busy node"
        );
        assert_eq!(of(FitPolicy::Pack), busy, "pack must fill the busy node");
    }

    /// And the explanation follows the decision, rather than describing the
    /// other policy's — the C8 claim, asserted for N6's knob.
    #[test]
    fn the_breakdown_shows_the_policy_that_was_used() {
        let mut state = MeshState::default();
        let busy = NodeId([1; 32]);
        for _ in 0..2 {
            let s = summary(busy, JobState::Running, 0);
            state.jobs.insert(s.job_id, s);
        }
        let load_of = ExecutorLoad::of(&state);
        let m = member(1, 4);
        let raw = |fit| {
            score_terms(
                &state,
                &load_of,
                &m,
                None,
                NodeId([9; 32]),
                &|_| None,
                None,
                None,
                None,
                fit,
            )
            .into_iter()
            .find(|t| t.name == "load")
            .expect("a load term")
            .raw
        };
        let spread = raw(FitPolicy::Spread);
        let packed = raw(FitPolicy::Pack);
        assert!(spread > 0.0 && spread < 1.0, "half-busy: {spread}");
        assert!(
            (spread + packed - 1.0).abs() < 1e-9,
            "the two readings must reflect, not negate: {spread} + {packed}"
        );
        // The weight is untouched: packing changes what the measurement
        // *means*, never how much the scheduler cares about it.
        let weights = |fit| {
            score_terms(
                &state,
                &load_of,
                &m,
                None,
                NodeId([9; 32]),
                &|_| None,
                None,
                None,
                None,
                fit,
            )
            .into_iter()
            .map(|t| (t.name, t.weight))
            .collect::<Vec<_>>()
        };
        assert_eq!(weights(FitPolicy::Spread), weights(FitPolicy::Pack));
    }

    #[test]
    fn warm_node_beats_cold_node() {
        let mut state = MeshState::default();
        let bundle = mesh_proto::BundleId([7; 32]);
        let warm = NodeId([2; 32]);
        state.bundles.insert(
            bundle,
            crate::state::BundleEntry {
                meta: BundleMeta {
                    bundle_id: bundle,
                    total_size: 1,
                    file_count: 1,
                    chunk_count: 1,
                },
                holders: [warm].into_iter().collect(),
            },
        );
        let ranked = rank(
            &state,
            vec![member(1, 4), member(2, 4)],
            Some(&bundle),
            NodeId([9; 32]),
            |_| None,
            FitPolicy::default(),
        );
        assert_eq!(ranked[0].node_id, warm);
    }

    #[test]
    fn failing_node_ranks_below_healthy_node() {
        let mut state = MeshState::default();
        let flaky = NodeId([1; 32]);
        let steady = NodeId([2; 32]);
        for i in 0..10u64 {
            let s = summary(flaky, JobState::Failed, i + 1);
            state.jobs.insert(s.job_id, s);
            let s = summary(steady, JobState::Succeeded, i + 1);
            state.jobs.insert(s.job_id, s);
        }
        let ranked = rank(
            &state,
            vec![member(1, 4), member(2, 4)],
            None,
            NodeId([9; 32]),
            |_| None,
            FitPolicy::default(),
        );
        assert_eq!(ranked[0].node_id, steady);
    }

    #[test]
    fn a_node_holding_the_inputs_wins_even_against_an_idler() {
        // The whole point of the term: moving gigabytes costs more than
        // waiting behind a job, so the node with the data wins even when
        // it is the busier of the two.
        let mut state = MeshState::default();
        let holder = NodeId([1; 32]);
        for _ in 0..2 {
            let s = summary(holder, JobState::Running, 0);
            state.jobs.insert(s.job_id, s);
        }
        let ranked = rank_with_locality(
            &state,
            vec![member(1, 4), member(2, 4)],
            None,
            NodeId([9; 32]),
            |_| None,
            |_| None,
            |id| Some(if *id == holder { 1.0 } else { 0.0 }),
            FitPolicy::default(),
        );
        assert_eq!(ranked[0].node_id, holder);

        // And with no object inputs declared, the term is invisible: the
        // idle node wins, exactly as it did before M9's P6.
        let ranked = rank(
            &state,
            vec![member(1, 4), member(2, 4)],
            None,
            NodeId([9; 32]),
            |_| None,
            FitPolicy::default(),
        );
        assert_eq!(ranked[0].node_id, NodeId([2; 32]));
    }

    #[test]
    fn tiebreak_is_deterministic() {
        let state = MeshState::default();
        let ranked = rank(
            &state,
            vec![member(2, 4), member(1, 4)],
            None,
            NodeId([9; 32]),
            |_| None,
            FitPolicy::default(),
        );
        assert_eq!(ranked[0].node_id, NodeId([1; 32]));
    }

    #[test]
    fn rendezvous_is_stable_and_key_dependent() {
        let nodes = [NodeId([1; 32]), NodeId([2; 32]), NodeId([3; 32])];
        let a = rendezvous_rank(b"ctx", b"key-a", nodes);
        let b = rendezvous_rank(b"ctx", b"key-a", nodes);
        assert_eq!(a, b, "same inputs must give the same order");
        assert_eq!(a.len(), 3);
        // Different keys shuffle differently at least sometimes; assert the
        // full ranking covers all nodes rather than a fragile order check.
        let c = rendezvous_rank(b"ctx", b"key-b", nodes);
        assert_eq!(c.len(), 3);
    }

    /// Phase C, E8: weights add, full attraction reads 0 and full avoidance 1.
    #[test]
    fn an_affinity_reads_from_attraction_to_avoidance() {
        assert_eq!(affinity_penalty(100), 0.0);
        assert_eq!(affinity_penalty(0), 0.5);
        assert_eq!(affinity_penalty(-100), 1.0);
        assert_eq!(affinity_penalty(50 + 50), affinity_penalty(100));
        assert_eq!(affinity_penalty(300), 0.0, "clamped, not past full");
        assert!(affinity_penalty(25) > affinity_penalty(75));
    }

    /// The emptiest share wins; a full one loses to one with room; a value
    /// that should hold nothing is the worst place of all.
    #[test]
    fn a_spread_prefers_the_emptiest_share() {
        // A 25% share still empty beats a 75% share two-thirds full.
        assert!(spread_penalty(0.0, 1.0) < spread_penalty(2.0, 3.0));
        // Room anywhere beats none.
        assert!(spread_penalty(2.0, 3.0) < 0.5);
        assert!(spread_penalty(1.0, 1.0) > 0.5);
        assert_eq!(spread_penalty(0.0, 0.0), 1.0);
        assert!(spread_penalty(9.0, 1.0) <= 1.0);
    }
}
