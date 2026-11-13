//! Placement legibility (C0–C9 §12): the scheduler's own arithmetic, kept.
//!
//! `score_full()` computes a weighted sum over eight terms for every
//! candidate and then throws all of it away except the winner. *"Why did my
//! job land there?"* is the most common operational question in distributed
//! computing and essentially no product answers it — and this one already
//! computed the answer, on every placement, since M5.
//!
//! ## The explanation is data, not a log line
//!
//! A log line is grep-able once and useless a week later. [`PlacementExplain`]
//! is a structure on the job record: the CLI tables it, Director draws a
//! stacked bar per candidate from it, and the what-if planner produces the
//! *same* structure for a job nobody has submitted.
//!
//! ## Five candidates, and only five
//!
//! A full breakdown for a thousand candidates on every placement is a
//! memory leak with a chart on it. Five answers the question — the winner
//! and the four it beat — and is bounded enough to keep on the job record
//! forever.

use serde::{Deserialize, Serialize};

use crate::NodeId;

/// How many candidates keep their breakdown.
pub const EXPLAIN_CANDIDATES: usize = 5;

/// One scoring term for one candidate.
///
/// Both the raw measurement and the weighted contribution, because they
/// answer different questions: `raw` is *"how loaded was that box"* and
/// `weighted` is *"how much did that cost it"*. A chart that plotted only
/// one of them would be lying in one of the two directions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreTerm {
    /// `load` | `warmth` | `failure` | `rtt` | `memory` | `disk` |
    /// `locality` | `device` | `cpu`. An older node's record may also carry
    /// `vantage`, the retired inventory term.
    pub name: String,
    /// The penalty before weighting, in `0.0..=1.0`. Lower is better,
    /// throughout: every term is a *penalty*, so a stacked bar reads
    /// "shortest wins" with no per-term sign to remember.
    pub raw: f64,
    /// `raw × weight` — what this term actually contributed to the total.
    pub weighted: f64,
    /// The weight applied, so a reader can tell a small term from a
    /// lightly-weighted one without a table of constants.
    pub weight: f64,
    /// True when the input was not measurable and the neutral default
    /// stood in (M9 §5.2). A chart that does not mark this makes an
    /// unmeasured node look like a mediocre one.
    pub unknown: bool,
}

/// One candidate's full breakdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateScore {
    pub node: NodeId,
    /// The node's name at the time of placement. Kept as a string because
    /// this record outlives the member — answering "why did it land there"
    /// for a job that ran last Tuesday must not depend on that box still
    /// being in the mesh.
    pub name: String,
    /// The weighted sum. Lower is better.
    pub total: f64,
    pub terms: Vec<ScoreTerm>,
}

/// Why a job landed where it did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementExplain {
    /// Best-first, capped at [`EXPLAIN_CANDIDATES`].
    pub candidates: Vec<CandidateScore>,
    /// The node the scheduler picked. Not always `candidates[0]`: a
    /// definitive `Busy` refusal spills to the next candidate, and the
    /// record has to say which one actually took the work or the
    /// explanation contradicts the job.
    pub chosen: Option<NodeId>,
    /// How many candidates were scored in total, of which
    /// [`EXPLAIN_CANDIDATES`] were kept. "The best of 340" is a different
    /// statement from "the only one".
    pub considered: u32,
    /// The job spilled past its best candidate because that node answered
    /// `Busy`. Recorded because otherwise the chart shows the scheduler
    /// picking the second-best node for no visible reason.
    pub spilled: bool,
    pub at_micros: u64,
}

impl PlacementExplain {
    pub fn chosen_score(&self) -> Option<&CandidateScore> {
        let chosen = self.chosen?;
        self.candidates.iter().find(|c| c.node == chosen)
    }

    /// The term that cost the runner-up the most against the winner — the
    /// one-line answer to "why not that one instead".
    ///
    /// `None` when there was no runner-up, or when the two are identical
    /// in every term (in which case the tiebreak was the node id, and
    /// saying so honestly beats inventing a reason).
    pub fn decisive_term(&self) -> Option<(String, f64)> {
        let winner = self.chosen_score().or_else(|| self.candidates.first())?;
        let runner_up = self.candidates.iter().find(|c| c.node != winner.node)?;
        let mut best: Option<(String, f64)> = None;
        for term in &runner_up.terms {
            let mine = winner
                .terms
                .iter()
                .find(|t| t.name == term.name)
                .map(|t| t.weighted)
                .unwrap_or(0.0);
            let delta = term.weighted - mine;
            if delta > 0.0 && best.as_ref().is_none_or(|(_, d)| delta > *d) {
                best = Some((term.name.clone(), delta));
            }
        }
        best
    }

    /// The runner-up: the best candidate that is not the one chosen.
    pub fn runner_up(&self) -> Option<&CandidateScore> {
        let winner = self.chosen_score().or_else(|| self.candidates.first())?;
        self.candidates.iter().find(|c| c.node != winner.node)
    }

    /// **Why the work landed there, in one sentence** (L2 P4).
    ///
    /// The same words in the CLI, in Director's hover and in `--output
    /// json`, because they are composed here and nowhere else. A screen and
    /// a terminal that explain one placement two ways are two explanations
    /// a person then has to reconcile, and the reconciling is the work this
    /// milestone exists to remove.
    ///
    /// Every branch is a *different fact about the mesh*, not a phrasing of
    /// one: a spill means the winner was not the best, a lone candidate
    /// means there was no contest at all, and an exact tie means the
    /// tiebreak was the node id — which is worth saying, because inventing
    /// a reason there would be the only dishonest thing this function
    /// could do.
    pub fn why(&self) -> Option<String> {
        let winner = self.chosen_score().or_else(|| self.candidates.first())?;
        let n = self.considered;
        let of = format!("{n} candidate{}", if n == 1 { "" } else { "s" });

        if self.spilled {
            let best = self.candidates.first().map(|c| c.name.as_str());
            return Some(match best {
                Some(best) if best != winner.name => format!(
                    "{} took the work after {best}, the best of {of}, answered Busy.",
                    winner.name
                ),
                _ => format!(
                    "{} took the work after the best of {of} answered Busy.",
                    winner.name
                ),
            });
        }
        if n <= 1 {
            // Filtering happens before scoring, so a single candidate is a
            // selector that matched one node — not a contest it won.
            return Some(format!(
                "{} was the only candidate. Nothing was compared: the selector left one node standing.",
                winner.name
            ));
        }
        Some(match (self.decisive_term(), self.runner_up()) {
            (Some((term, delta)), Some(up)) => format!(
                "{} scored lowest of {of}, and lower is better. The runner-up, {}, lost mostly on {term} (+{delta:.2}).",
                winner.name, up.name
            ),
            (_, Some(up)) => format!(
                "{} scored lowest of {of}, and lower is better. Nothing separated it from {} — the tiebreak was the node id.",
                winner.name, up.name
            ),
            _ => format!(
                "{} scored lowest of {of}, and lower is better.",
                winner.name
            ),
        })
    }
}

/// What one scoring term measures, in the words a person would use.
///
/// The chart and the table print these names — `warmth`, `locality`, `rtt` —
/// because they are the scheduler's own vocabulary and renaming them would
/// make the code and the screen disagree. What was missing is anywhere to
/// find out what they *mean* without reading `scheduler.rs`, which is L2
/// P4's whole complaint: the explanation existed and was filed behind a
/// command nobody was told about.
///
/// `None` for a term this build does not know — a newer node's placement
/// record read by an older reader, which says nothing rather than guessing.
pub fn term_gloss(name: &str) -> Option<&'static str> {
    Some(match name {
        "load" => "Jobs already on that machine, against the slots it advertises.",
        "cpu" => "What the machine is actually doing right now — mesh work or not.",
        "warmth" => "Whether it already holds this job's bundle. A cold node fetches first.",
        "failure" => "How many of that node's recent jobs ended badly.",
        "rtt" => "Round-trip time from this node to that one.",
        "memory" => "How much of its advertised memory is already spoken for.",
        "disk" => "How much of its data volume is already spoken for.",
        "locality" => "How much of this job's declared input it does not already hold.",
        // Retired with the host inventory; a placement recorded by an older
        // node may still carry it.
        "vantage" => "How many of the targeted inventory hosts it had not lately been reaching.",
        "device" => "How busy the best accelerator on it is that could serve this job.",
        "affinity" => "How little it matches the machines this deployment said it prefers.",
        "spread" => "How far past its share of instances that machine's group already is.",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Phase C, E7/E8 — what a deployment asks of where its instances go
// ---------------------------------------------------------------------------

/// The reserved key a deployment's placement rules travel in (Phase C).
///
/// **In the policy side-car's `instance_env`, like `[deployment.resources]`**,
/// for the reasons that carrier exists: no signed struct grows, an older node
/// carries the entry without reading it, and a version (E1) holds it — so a
/// rollback restores the rules a generation was published with. It is not
/// part of what [`crate::DeploymentPolicy`]'s comparison calls a roll: a rule
/// about where the *next* instance goes restarts nothing that is running.
pub const REQ_PLACEMENT: &str = "MESHWARE_TASK_PLACEMENT";

/// The most affinities one deployment may declare. Each is a selector
/// evaluated against every candidate on every placement.
pub const MAX_AFFINITIES: usize = 16;

/// A deployment's placement rules (Phase C, E7 and E8) — Nomad's
/// `distinct_hosts` and `distinct_property` constraints, and its `affinity`
/// and `spread` blocks.
///
/// The constraints are **hard**: a machine that breaks one is not a
/// candidate, and a slot with no candidate left waits and says why. The
/// other two are **scores**, terms beside the scheduler's own, so they
/// decide between machines that could all run the instance and never make
/// one that cannot run it eligible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementSpec {
    /// At most one instance of this deployment per machine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub distinct_hosts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct_property: Option<DistinctProperty>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affinity: Vec<Affinity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread: Option<Spread>,
}

/// At most `limit` instances on machines sharing one value of `attribute`:
/// one per rack, two per house.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistinctProperty {
    pub attribute: String,
    pub limit: u32,
}

/// Prefer machines `select` matches, by `weight` from -100 (avoid) to 100.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Affinity {
    pub select: String,
    pub weight: i32,
}

/// Share instances out across the values of `attribute`: by the percentages
/// in `targets`, or evenly across every value the candidates have when it
/// is empty. Values not named share whatever percentage is left, together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spread {
    pub attribute: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub targets: std::collections::BTreeMap<String, u32>,
}

impl PlacementSpec {
    pub fn is_empty(&self) -> bool {
        self == &PlacementSpec::default()
    }

    /// The reserved entry, or nothing when the deployment states no rule.
    pub fn to_env(&self) -> Vec<(String, String)> {
        if self.is_empty() {
            return Vec::new();
        }
        vec![(
            REQ_PLACEMENT.to_string(),
            serde_json::to_string(self).unwrap_or_default(),
        )]
    }

    /// Read the rules back off an instance environment.
    ///
    /// Unknown keys are ignored rather than refused, so a rule a newer
    /// build adds reads here as the rules this build knows — which is the
    /// same degradation every other side-car entry makes.
    pub fn from_env(env: &[(String, String)]) -> PlacementSpec {
        env.iter()
            .find(|(k, _)| k == REQ_PLACEMENT)
            .and_then(|(_, v)| serde_json::from_str(v).ok())
            .unwrap_or_default()
    }
}

/// Is `attribute` something every machine has one value of: `name`,
/// `tag.<key>` (the value of a `<key>=<value>` tag, `cap.` ones included),
/// or `fact.<key>`?
pub fn is_valid_attribute(attribute: &str) -> bool {
    let key = |k: &str| {
        !k.is_empty()
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    attribute == "name"
        || attribute.strip_prefix("tag.").is_some_and(key)
        || attribute.strip_prefix("fact.").is_some_and(key)
}

// ---------------------------------------------------------------------------
// The what-if planner (§12.3 P3)
// ---------------------------------------------------------------------------

/// A duration distribution over an action's history.
///
/// **A range, never a time.** A planner that says "17 minutes" is wrong
/// precisely often enough to be untrusted forever; one that says "12–25
/// minutes, p50 16" is right and stays trusted. That is the whole
/// justification for this type existing instead of a `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurationRange {
    pub p10_secs: u64,
    pub p50_secs: u64,
    pub p90_secs: u64,
    /// How many historical runs this came from. Below a handful it is a
    /// guess wearing a distribution's clothes, and the UI says so.
    pub samples: u32,
}

impl DurationRange {
    /// Percentiles from raw durations. Nearest-rank, which needs no
    /// interpolation and cannot invent a value between two observations
    /// that never happened.
    pub fn from_secs(mut samples: Vec<u64>) -> Option<DurationRange> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let at = |p: f64| -> u64 {
            let idx = ((samples.len() as f64 * p).ceil() as usize).saturating_sub(1);
            samples[idx.min(samples.len() - 1)]
        };
        Some(DurationRange {
            p10_secs: at(0.10),
            p50_secs: at(0.50),
            p90_secs: at(0.90),
            samples: samples.len() as u32,
        })
    }

    /// *"from 30 past runs"* — where the range came from (L2 P4).
    ///
    /// Shown beside every estimate this type produces, because a spread
    /// over three runs and a spread over three hundred are different
    /// claims and a reader cannot tell them apart from the range alone.
    pub fn provenance(&self) -> String {
        match self.samples {
            1 => "from 1 past run".to_string(),
            n => format!("from {n} past runs"),
        }
    }

    /// The whole estimate, in one sentence a hover can carry (L2 P4).
    pub fn why(&self) -> String {
        format!(
            "{} — {}. A range, never a time: an estimate that names one number \
             is wrong often enough to stop being read.",
            self.describe(),
            self.provenance()
        )
    }

    /// `12–25 minutes, p50 16` in one string.
    pub fn describe(&self) -> String {
        let unit = |secs: u64| -> String {
            if secs < 90 {
                format!("{secs}s")
            } else if secs < 5400 {
                format!("{}m", (secs + 30) / 60)
            } else {
                format!("{:.1}h", secs as f64 / 3600.0)
            }
        };
        format!(
            "{}–{}, p50 {}",
            unit(self.p10_secs),
            unit(self.p90_secs),
            unit(self.p50_secs)
        )
    }
}

/// Whose work this submission would push back, and by how long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelayedPrincipal {
    pub user: crate::UserId,
    pub name: String,
    /// Jobs of theirs currently queued behind the point this one would
    /// enter.
    pub jobs: u32,
    /// How much later they would start, from the same distribution the
    /// estimate itself uses.
    pub by_secs: u64,
}

/// The answer to *"if I launch this now, when does it finish and what does
/// it delay?"*.
///
/// M11's `--dry-run` answers *targets, blast radius, verdict* — the
/// governance question. This is the other one, and it needs different
/// machinery: capacity vectors, live pressure, queue depth, C1's fair-share
/// ordering, and historical durations.
///
/// **A simulation over the real scorer**, never a second model of the
/// scheduler: the projection calls `score_full()` against a projected
/// state, so it cannot drift from what the scheduler will actually do. A
/// second model would agree on the day it was written and diverge silently
/// forever after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanEstimate {
    /// Where it would land, with the same breakdown a real placement gets.
    pub placement: PlacementExplain,
    /// How long the work itself takes, from the archive. `None` when
    /// nothing like this has run here before — and saying "no history"
    /// is the honest answer, not extrapolating from one sample.
    pub duration: Option<DurationRange>,
    /// How long before it *starts*: zero if a slot is free right now, the
    /// projected queue drain otherwise.
    pub wait_secs: u64,
    /// Wait + duration, as a range.
    pub finish: Option<DurationRange>,
    /// Whose work this pushes back (C1's ordering, applied forward).
    pub delays: Vec<DelayedPrincipal>,
    /// Slots free across the candidates right now, and how many the
    /// submission wants.
    pub slots_free: u32,
    pub slots_wanted: u32,
    /// Present when the submission would be refused outright — no
    /// candidate, no quota, no device. A planner that reports a finish time
    /// for work that would never start is worse than one that says so.
    pub refusal: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn term(name: &str, raw: f64, weight: f64) -> ScoreTerm {
        ScoreTerm {
            name: name.into(),
            raw,
            weighted: raw * weight,
            weight,
            unknown: false,
        }
    }

    fn candidate(id: u8, name: &str, terms: Vec<ScoreTerm>) -> CandidateScore {
        CandidateScore {
            node: NodeId([id; 32]),
            name: name.into(),
            total: terms.iter().map(|t| t.weighted).sum(),
            terms,
        }
    }

    #[test]
    fn the_decisive_term_is_the_one_that_cost_the_runner_up_most() {
        let explain = PlacementExplain {
            candidates: vec![
                candidate(1, "a", vec![term("load", 0.1, 1.0), term("rtt", 0.5, 0.2)]),
                candidate(2, "b", vec![term("load", 0.9, 1.0), term("rtt", 0.0, 0.2)]),
            ],
            chosen: Some(NodeId([1; 32])),
            considered: 2,
            spilled: false,
            at_micros: 0,
        };
        let (name, delta) = explain.decisive_term().unwrap();
        assert_eq!(name, "load");
        assert!((delta - 0.8).abs() < 1e-9);
    }

    #[test]
    fn an_identical_pair_admits_it_had_no_reason() {
        let terms = vec![term("load", 0.5, 1.0)];
        let explain = PlacementExplain {
            candidates: vec![
                candidate(1, "a", terms.clone()),
                candidate(2, "b", terms.clone()),
            ],
            chosen: Some(NodeId([1; 32])),
            considered: 2,
            spilled: false,
            at_micros: 0,
        };
        assert_eq!(explain.decisive_term(), None);
    }

    #[test]
    fn a_lone_candidate_has_nothing_to_explain() {
        let explain = PlacementExplain {
            candidates: vec![candidate(1, "a", vec![term("load", 0.5, 1.0)])],
            chosen: Some(NodeId([1; 32])),
            considered: 1,
            spilled: false,
            at_micros: 0,
        };
        assert_eq!(explain.decisive_term(), None);
    }

    #[test]
    fn percentiles_come_from_observations_and_never_between_them() {
        let range = DurationRange::from_secs(vec![10, 20, 30, 40, 50]).unwrap();
        assert_eq!(range.p50_secs, 30);
        assert_eq!(range.p10_secs, 10);
        assert_eq!(range.p90_secs, 50);
        assert_eq!(range.samples, 5);
        assert!(DurationRange::from_secs(Vec::new()).is_none());
    }

    #[test]
    fn one_sample_is_a_flat_distribution_not_a_guess() {
        let range = DurationRange::from_secs(vec![42]).unwrap();
        assert_eq!(
            (range.p10_secs, range.p50_secs, range.p90_secs),
            (42, 42, 42)
        );
        assert_eq!(range.samples, 1);
    }

    // -- L2 P4: the sentences ---------------------------------------------

    fn two_way() -> PlacementExplain {
        PlacementExplain {
            candidates: vec![
                candidate(
                    1,
                    "anvil",
                    vec![term("load", 0.1, 1.0), term("rtt", 0.5, 0.2)],
                ),
                candidate(
                    2,
                    "forge",
                    vec![term("load", 0.9, 1.0), term("rtt", 0.0, 0.2)],
                ),
            ],
            chosen: Some(NodeId([1; 32])),
            considered: 4,
            spilled: false,
            at_micros: 0,
        }
    }

    /// L2 P4's exit demo: hovering "ran on anvil" has to say why anvil won
    /// *and* what the runner-up lost on.
    #[test]
    fn why_names_the_winner_the_runner_up_and_the_term() {
        let why = two_way().why().unwrap();
        assert_eq!(
            why,
            "anvil scored lowest of 4 candidates, and lower is better. \
             The runner-up, forge, lost mostly on load (+0.80)."
        );
    }

    /// A spill is a different fact, not a phrasing: the winner was *not*
    /// the best candidate, and a sentence that did not say so would
    /// contradict the chart above it.
    #[test]
    fn a_spill_says_the_winner_was_not_the_best() {
        let mut explain = two_way();
        explain.chosen = Some(NodeId([2; 32]));
        explain.spilled = true;
        let why = explain.why().unwrap();
        assert_eq!(
            why,
            "forge took the work after anvil, the best of 4 candidates, answered Busy."
        );
    }

    /// Filtering happens before scoring, so one candidate is a selector
    /// that matched one node — never a contest it won.
    #[test]
    fn a_lone_candidate_says_there_was_no_contest() {
        let explain = PlacementExplain {
            candidates: vec![candidate(1, "anvil", vec![term("load", 0.5, 1.0)])],
            chosen: Some(NodeId([1; 32])),
            considered: 1,
            spilled: false,
            at_micros: 0,
        };
        assert!(
            explain.why().unwrap().contains("was the only candidate"),
            "{:?}",
            explain.why()
        );
    }

    /// An exact tie admits the tiebreak was the node id. Inventing a reason
    /// here is the only dishonest thing this function could do.
    #[test]
    fn an_exact_tie_names_the_tiebreak() {
        let terms = vec![term("load", 0.5, 1.0)];
        let mut explain = two_way();
        explain.candidates = vec![
            candidate(1, "anvil", terms.clone()),
            candidate(2, "forge", terms),
        ];
        assert!(
            explain
                .why()
                .unwrap()
                .contains("the tiebreak was the node id"),
            "{:?}",
            explain.why()
        );
    }

    #[test]
    fn every_term_the_scheduler_emits_has_words_for_it() {
        // The list `score_full` builds, in its order.
        for name in [
            "load", "warmth", "failure", "rtt", "memory", "disk", "locality", "device", "cpu",
        ] {
            assert!(term_gloss(name).is_some(), "{name} has no gloss");
        }
        // And a term from a newer node says nothing rather than guessing.
        assert_eq!(term_gloss("entropy"), None);
    }

    #[test]
    fn a_range_says_how_many_runs_it_came_from() {
        let one = DurationRange {
            p10_secs: 5,
            p50_secs: 5,
            p90_secs: 5,
            samples: 1,
        };
        assert_eq!(one.provenance(), "from 1 past run");
        assert_eq!(
            DurationRange { samples: 30, ..one }.provenance(),
            "from 30 past runs"
        );
    }

    #[test]
    fn a_range_reads_as_a_range() {
        let range = DurationRange {
            p10_secs: 720,
            p50_secs: 960,
            p90_secs: 1500,
            samples: 30,
        };
        assert_eq!(range.describe(), "12m–25m, p50 16m");
    }
}

// ---------------------------------------------------------------------------
// N1 P2 — why a placement could not be satisfied
// ---------------------------------------------------------------------------

/// The gate a node failed on its way to being a candidate.
///
/// A node fails **at most one**: the audit that produces these stops at the
/// first gate that rejects a node, so the counts partition the mesh rather
/// than overlapping, and "4 of 7 excluded" adds up. The order is the order
/// the filter applies, which is also the order a person wants the answer
/// in — whether a node is *in* the mesh comes before whether it is *big
/// enough for this*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Exclusion {
    /// Left the mesh, or had its membership revoked.
    Gone,
    /// The mesh's admins have not granted it execution
    /// (`MeshState::may_execute`). The owner's veto, before anything the
    /// node says about itself.
    NotPermitted,
    /// Carries `no-execute`, or is draining.
    NotExecuting,
    /// The selector did not match it.
    Selector,
    /// Suspect or dead.
    NotAlive,
    /// Its advertised hardware is too small. **Permanent** — a 48 GB task
    /// never fits a 16 GB box, however idle it gets.
    Hardware,
    /// Its hardware is big enough; its free memory or disk is not, right
    /// now. **Transient** — this one clears itself.
    Busy,
    /// It advertises no device matching the request.
    Devices,
    /// It does not offer a volume this job claims by name (N5 P1).
    ///
    /// The device rule again: a node running this build advertises its
    /// volume list, so an absent name is a statement rather than an
    /// absence of measurement. A stateful job placed on a node without its
    /// data directory would start and find an empty one, which is worse
    /// than not starting.
    Volumes,
    /// It advertises no dynamic port range, so it cannot allocate one
    /// (N2 P3).
    ///
    /// The **device** rule rather than the memory rule: an unadvertised
    /// range is a statement, not an absence of measurement. Every node
    /// running this build announces one, so a missing tag means a node too
    /// old to allocate a port — and treating that as neutral would place a
    /// web server on it and have the bind fail with nothing to say why.
    Ports,
    /// It cannot run this kind of work: no container engine, no `python3`,
    /// no `ansible-playbook` (T3).
    ///
    /// **Appended, never inserted.** This enum is indexed by `as usize`
    /// into a fixed-size tally and travels in IPC, so a variant added
    /// anywhere but the end would renumber every reason a running daemon
    /// has already counted.
    ///
    /// The **memory** rule where the tag is absent entirely, which is the
    /// opposite of [`Exclusion::Ports`] above and is argued in
    /// `crate::runtimes::from_tags`: a node that announces no `cap.rt` is
    /// running a build from before this existed and may well have a
    /// container plugin configured, so excluding it would shrink a mesh's
    /// capacity on upgrade. A node that *does* announce is taken at its
    /// word, in both directions.
    Runtime,
}

impl Exclusion {
    /// Does waiting fix it?
    ///
    /// The difference between *"this will never place"* and *"this will
    /// place itself when a node frees up"*, which is the single most
    /// useful thing a refusal can tell someone.
    pub fn transient(&self) -> bool {
        matches!(self, Exclusion::Busy | Exclusion::NotAlive)
    }

    /// The reason as a stable slug, for JSON and for Director.
    pub fn as_str(&self) -> &'static str {
        match self {
            Exclusion::Gone => "gone",
            Exclusion::NotPermitted => "not-permitted",
            Exclusion::NotExecuting => "not-executing",
            Exclusion::Selector => "selector",
            Exclusion::NotAlive => "not-alive",
            Exclusion::Hardware => "hardware",
            Exclusion::Busy => "busy",
            Exclusion::Devices => "devices",
            Exclusion::Ports => "ports",
            Exclusion::Runtime => "runtime",
            Exclusion::Volumes => "volumes",
        }
    }
}

/// Why a placement cannot be satisfied right now, counted by reason.
///
/// **Built by the same filter that does the excluding** (N1 P2's landmine).
/// `membership::audit_feasible` is `resolve_feasible_with_devices` — one
/// function, one set of predicates — so the sentence a refusal prints
/// cannot drift from the decision the scheduler made. A second explainer
/// would agree on the day it was written and then disagree forever, and
/// the disagreement would arrive as a bug report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacementRefusal {
    /// The selector, as written.
    pub selector: String,
    /// Every member looked at, including the ones that were never
    /// plausible. "4 of 7" needs the 7.
    pub considered: u32,
    /// How many survived every gate.
    pub feasible: u32,
    /// Reason → count, in [`Exclusion`] order, zero counts omitted.
    pub excluded: Vec<(Exclusion, u32)>,
    /// What the work asked for, when it asked for anything.
    pub needs_cpus: u32,
    pub needs_mem_mb: u64,
    pub needs_disk_mb: u64,
    /// The most any one excluded node had free, so the refusal can say
    /// *"needs 512M, most free 384M"* rather than only "memory". `None`
    /// when nothing was excluded for that dimension, or when no excluded
    /// node reported it.
    pub most_free_mem_mb: Option<u64>,
    pub most_free_disk_mb: Option<u64>,
    pub most_cores: Option<u32>,
    /// How many labelled ports the work asked for (N2 P3), so a refusal can
    /// say what was wanted as well as who could not give it.
    pub needs_ports: u32,
    /// The volumes the work claims by name (N5 P1).
    pub needs_volumes: Vec<String>,
    /// The runtime the work needs, when it needs a particular one (T3), so
    /// a refusal can say *"needs a container engine"* rather than only
    /// "runtime".
    ///
    /// `Option` because most work states none: an inline script is bash and
    /// says so by being a script, and a refusal that named a runtime for it
    /// would be inventing a requirement nobody wrote.
    #[serde(default)]
    pub needs_runtime: Option<crate::content::Runtime>,
}

impl PlacementRefusal {
    /// Is anything at all placeable?
    pub fn any_feasible(&self) -> bool {
        self.feasible > 0
    }

    /// Will waiting fix this?
    ///
    /// True when every reason a node was excluded is one that clears on
    /// its own. A refusal that is purely transient is not an error to fix,
    /// it is a queue to watch — and saying so stops somebody editing a
    /// working file.
    pub fn transient(&self) -> bool {
        !self.excluded.is_empty() && self.excluded.iter().all(|(r, _)| r.transient())
    }

    /// The refusal as the lines a person reads.
    ///
    /// Here rather than in either front end because the CLI prints them
    /// and Director shows them, and two renderings of one refusal would
    /// eventually say different things about the same mesh.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (reason, n) in &self.excluded {
            let nodes = if *n == 1 { "node" } else { "nodes" };
            out.push(match reason {
                Exclusion::Selector => format!(
                    "selector {} excluded {n} of {} {nodes}",
                    self.selector, self.considered
                ),
                Exclusion::Gone => format!("{n} {nodes} have left the mesh"),
                Exclusion::NotPermitted => {
                    format!("{n} {nodes} are not permitted to execute (no grant)")
                }
                Exclusion::NotExecuting => {
                    format!("{n} {nodes} are draining or tagged no-execute")
                }
                Exclusion::NotAlive => format!("{n} {nodes} are suspect or dead"),
                Exclusion::Hardware => {
                    format!("{n} {nodes} are too small for this: {}", self.needs_line())
                }
                Exclusion::Busy => format!("{}: {n} {nodes}", self.busy_line()),
                Exclusion::Devices => format!("{n} {nodes} advertise no matching device"),
                Exclusion::Volumes => format!(
                    "{n} {nodes} do not offer {}",
                    if self.needs_volumes.len() == 1 {
                        format!("volume {:?}", self.needs_volumes[0])
                    } else {
                        format!("volumes {}", self.needs_volumes.join(", "))
                    }
                ),
                Exclusion::Ports => format!(
                    "{n} {nodes} advertise no dynamic port range (this needs {}), \
                     which usually means they are running an older build",
                    if self.needs_ports == 1 {
                        "1 port".to_string()
                    } else {
                        format!("{} ports", self.needs_ports)
                    }
                ),
                // The sentence says what is *missing on the machine*, not
                // what the job asked for. "cannot run container work" sends
                // somebody to `meshnode runtimes` and then to `dnf install
                // podman`; "needs runtime container" sends them to re-read
                // their own file, which was never wrong.
                Exclusion::Runtime => format!(
                    "{n} {nodes} cannot run {} work — {}",
                    self.needs_runtime
                        .map(|r| r.as_str())
                        .unwrap_or("this kind of"),
                    match self.needs_runtime {
                        Some(crate::content::Runtime::Container) =>
                            "install podman or docker there, or target nodes that have one",
                        Some(crate::content::Runtime::Python) =>
                            "install python3 there, or target nodes that have it",
                        Some(crate::content::Runtime::Ansible) =>
                            "install ansible there, or target nodes that have it",
                        _ => "`meshnode runtimes` on one of them says what it found",
                    }
                ),
            });
        }
        if self.feasible == 0 {
            out.push(if self.transient() {
                "waiting for capacity — this will place itself when a node frees up".into()
            } else {
                "no node in this mesh can take it as written".into()
            });
        }
        out
    }

    fn needs_line(&self) -> String {
        let mut parts = Vec::new();
        if self.needs_cpus > 0 {
            parts.push(format!("needs {} cpu", self.needs_cpus));
        }
        if self.needs_mem_mb > 0 {
            parts.push(format!("needs {}", mb(self.needs_mem_mb)));
        }
        if self.needs_disk_mb > 0 {
            parts.push(format!("needs {} disk", mb(self.needs_disk_mb)));
        }
        if parts.is_empty() {
            "declared requirements".into()
        } else {
            parts.join(", ")
        }
    }

    fn busy_line(&self) -> String {
        // The *most free* of the excluded nodes, not the least: the
        // question a stalled rollout asks is "how close am I", and the
        // closest node is the one that answers it.
        match (self.most_free_mem_mb, self.most_free_disk_mb) {
            (Some(free), _) if self.needs_mem_mb > 0 => format!(
                "memory exhausted (needs {}, most free {})",
                mb(self.needs_mem_mb),
                mb(free)
            ),
            (_, Some(free)) if self.needs_disk_mb > 0 => format!(
                "disk exhausted (needs {}, most free {})",
                mb(self.needs_disk_mb),
                mb(free)
            ),
            _ => "no free memory or disk right now".into(),
        }
    }
}

/// Megabytes as a person writes them. Local to the refusal text so
/// `mesh-proto` keeps its no-dependency-for-formatting property.
fn mb(n: u64) -> String {
    if n >= 1024 && n.is_multiple_of(1024) {
        format!("{}G", n / 1024)
    } else if n >= 1024 {
        format!("{:.1}G", n as f64 / 1024.0)
    } else {
        format!("{n}M")
    }
}

// ---------------------------------------------------------------------------
// N6 — pack or spread (§12)
// ---------------------------------------------------------------------------

/// How a node fills the mesh: keep every box cool, or fill this one first.
///
/// **One sign flip over the same fit score**, which is what makes it worth
/// having: there is no second scheduler, no second ranking, and no second
/// explanation. `jobs explain` shows the same ten terms either way, and the
/// chart a person reads is still the decision the scheduler made.
///
/// The two answers are both right, for different meshes:
///
/// - **Spread** (the default) keeps every machine responsive. It is the
///   right answer when the nodes are also somebody's desktop, which is the
///   mesh meshware is actually for.
/// - **Pack** fills one machine before touching the next. It is the right
///   answer when the nodes are cloud instances you are paying for by the
///   hour and want to be able to *switch off* — a mesh at 20% on ten boxes
///   costs ten boxes; the same work packed onto three costs three.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FitPolicy {
    /// Prefer the emptiest machine. Busy is a penalty.
    #[default]
    Spread,
    /// Prefer the fullest machine that still fits. Busy is a *credit*.
    Pack,
}

impl FitPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            FitPolicy::Spread => "spread",
            FitPolicy::Pack => "pack",
        }
    }

    /// `None` for anything else — the caller decides whether an unknown
    /// word is a refusal or a fallback, and for `[compute] placement` it is
    /// a fallback with a warning, because a typo must not silently invert
    /// every placement decision on the node.
    pub fn parse(s: &str) -> Option<FitPolicy> {
        match s.trim().to_ascii_lowercase().as_str() {
            "spread" => Some(FitPolicy::Spread),
            "pack" => Some(FitPolicy::Pack),
            _ => None,
        }
    }

    /// Re-read one busyness measurement under this policy.
    ///
    /// **`1 - raw`, never `-raw`.** The difference is the whole
    /// correctness of the thing. Every term here is a penalty in `[0, 1]`
    /// and an unmeasurable one contributes the midpoint, `0.5`, so that a
    /// node nobody can measure is neither preferred nor punished. Negating
    /// would put the busyness terms on `[-1, 0]` while every other term
    /// stayed on `[0, 1]`, and — worse — would move the unknown
    /// contribution from the midpoint to an *advantage*, so packing would
    /// systematically prefer the nodes it knows least about.
    ///
    /// Reflecting keeps the range, keeps every weight positive and
    /// comparable, and keeps `0.5` exactly where it was.
    pub fn fit(&self, raw: f64) -> f64 {
        match self {
            FitPolicy::Spread => raw,
            FitPolicy::Pack => 1.0 - raw,
        }
    }
}

#[cfg(test)]
mod fit_tests {
    use super::*;

    #[test]
    fn packing_reflects_rather_than_negates() {
        for (raw, packed) in [(0.0, 1.0), (0.25, 0.75), (1.0, 0.0)] {
            assert_eq!(FitPolicy::Spread.fit(raw), raw);
            assert!((FitPolicy::Pack.fit(raw) - packed).abs() < f64::EPSILON);
        }
        // The property the whole design rests on: an unmeasurable node is
        // neutral under *both* policies. Negating would have made packing
        // prefer the nodes it knows least about.
        assert_eq!(FitPolicy::Pack.fit(0.5), 0.5);
        assert_eq!(FitPolicy::Spread.fit(0.5), 0.5);
    }

    #[test]
    fn the_words_round_trip_and_a_typo_is_not_a_policy() {
        for p in [FitPolicy::Spread, FitPolicy::Pack] {
            assert_eq!(FitPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(FitPolicy::parse("  PACK "), Some(FitPolicy::Pack));
        assert_eq!(FitPolicy::parse("packed"), None);
        assert_eq!(FitPolicy::parse(""), None);
        assert_eq!(FitPolicy::default(), FitPolicy::Spread);
    }
}
