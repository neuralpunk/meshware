//! Per-entity op-rate limits, refused at the origin (M31 A3).
//!
//! ## Why this exists at all
//!
//! A1 governs the **steady state**: a retention window bounds what a
//! healthy mesh keeps. Nothing governed the **bad day** — somebody loops a
//! workflow, or a fact collector flaps across a thousand machines, and the
//! op-log grows at a rate no retention window can outrun. Policy is a
//! budget; this is a circuit breaker, and a system with a budget and no
//! breaker fails in exactly one way.
//!
//! ## Three properties, and the second is the one that keeps it switched on
//!
//! 1. **Refused where it was produced.** The node that authored the op is
//!    the node that says no, so the bug gets fixed rather than absorbed by
//!    every other machine in the mesh. That is `MAX_FACT_DOC`'s precedent,
//!    generalised past facts.
//!
//! 2. **A legitimate burst is not refused.** A thousand-item fan-out
//!    authoring a thousand job ops in two seconds is *work*, not a loop, and
//!    a control that refuses it is a control operators disable — after which
//!    it protects nothing. Hence a token bucket with a real burst
//!    allowance rather than a fixed window: the limit is a sustained *rate*,
//!    and the burst is free.
//!
//! 3. **Per entity, not per node.** A budget shared across a mesh's whole
//!    workload would make one busy workflow starve everything else. The key
//!    is the op's own compaction key — the identity the log already uses to
//!    decide what supersedes what — so "this entity is churning" is
//!    expressed in the vocabulary the log already has.
//!
//! ## What is deliberately *not* here
//!
//! **No per-refusal audit entry.** `AuditAppend` is `Grow` and never
//! compacts (§1.1), so auditing every refusal would let a runaway loop turn
//! a log-growth problem into a *permanent* log-growth problem — the control
//! becoming the incident. One entry when an entity enters the pressured
//! state, one when it is paused, and nothing while it stays there. This is
//! the same rule B3 applies to `history.replay`: audit the transition, not
//! the traffic.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Ops per minute one entity may author on this node.
///
/// **Four a second, sustained, for a *single* entity** — and the number is
/// picked off the busiest legitimate writer in the tree rather than out of
/// the air. A workflow's drive loop polls at `DRIVE_POLL` (one second) and
/// publishes its run record when something changed, at most twice per
/// poll: two ops a second, on one key, for as long as the run is active.
/// That is the ceiling this has to clear without complaint, so it clears it
/// twice over.
///
/// Everything else is far below it. A job's whole life is a handful of ops
/// under its own key; a fact collector publishes on change and has its own
/// flap guard; usage counters flush every thirty seconds. An entity
/// sustaining four ops a second is not doing work anybody asked for.
pub const DEFAULT_ENTITY_OPS_PER_MINUTE: u32 = 240;

/// Burst allowance as a multiple of the per-minute rate.
///
/// Four minutes' worth — 960 ops — available instantly. Property 2 above is
/// the whole reason this is not 1: the failure mode of a tight limiter is
/// not a flood that gets through, it is an operator switching it off.
///
/// The other side of the trade is what a runaway spends before it is
/// stopped, and it is small: a loop at a hundred ops a second burns the
/// whole allowance in ten seconds, and 960 ops is around 150 KB — nothing
/// against the log §1 measures in hundreds of megabytes. Refusing *early*
/// buys nothing here and costs the fan-out case everything.
pub const BURST_FACTOR: f64 = 4.0;

/// Refusals inside one pressure episode before the entity is paused
/// outright.
///
/// Once an entity has been told no this many times it is not a burst that
/// overshot; it is a loop, and continuing to evaluate it per op is work
/// spent on a caller that is not listening.
pub const PAUSE_AFTER_REFUSALS: u32 = 60;

/// How long an auto-pause lasts.
///
/// Five minutes, then it is allowed to try again — long enough that a
/// wedged loop stops costing anything, short enough that a fixed deployment
/// recovers without an operator being paged to un-pause it. **Auto-pause is
/// a brake, not a tombstone**; a control that requires human action to undo
/// is a control that turns a transient fault into an outage.
pub const PAUSE_SECS: u64 = 300;

/// What [`RateGuard::admit`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Author it.
    Yes,
    /// Over the sustained rate. The message names the entity and the limit,
    /// because a refusal that does not say what tripped is a bug report
    /// nobody can act on.
    Refused {
        entity: String,
        limit_per_minute: u32,
        /// What this entity has actually been doing, per minute, over the
        /// window — the number that makes the limit arguable.
        observed_per_minute: u32,
    },
    /// Paused after sustained refusal. Distinct from `Refused` so a caller
    /// can say *"paused until 14:32"* rather than repeating the same
    /// sentence sixty times a second.
    Paused {
        entity: String,
        until_micros: u64,
        observed_per_minute: u32,
    },
}

impl Admit {
    pub fn is_yes(&self) -> bool {
        matches!(self, Admit::Yes)
    }

    /// The one sentence a person reads. Composed here so the CLI, Director
    /// and the audit entry never write three different ones.
    pub fn message(&self) -> String {
        match self {
            Admit::Yes => String::new(),
            Admit::Refused {
                entity,
                limit_per_minute,
                observed_per_minute,
            } => format!(
                "`{entity}` is authoring {observed_per_minute} ops a minute, over this node's \
                 limit of {limit_per_minute}. The op was refused here, where it was produced, \
                 rather than replicated to every machine in the mesh."
            ),
            Admit::Paused {
                entity,
                until_micros,
                observed_per_minute,
            } => format!(
                "`{entity}` is paused until {} after sustained refusal at \
                 {observed_per_minute} ops a minute. It will be allowed to try again then; \
                 nothing has to be un-paused by hand.",
                mesh_proto::availability::day_of(*until_micros)
            ),
        }
    }
}

/// One entity's budget and its recent behaviour.
#[derive(Debug, Clone)]
struct Bucket {
    /// Tokens remaining, in ops.
    tokens: f64,
    last_micros: u64,
    /// Ops admitted plus refused since `episode_started_micros` — the
    /// numerator of `observed_per_minute`.
    attempts: u64,
    episode_started_micros: u64,
    /// Consecutive refusals in the current pressure episode.
    refusals: u32,
    /// Set while auto-paused.
    paused_until_micros: u64,
    /// Total refusals over this entity's life on this node, for `doctor`.
    refused_total: u64,
    /// Set when this entity entered pressure and nothing has audited it
    /// yet. Drained by the daemon; see [`RateGuard::drain_new_episodes`].
    unreported: bool,
}

/// One entity's pressure, as `doctor` and Director report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pressure {
    pub entity: String,
    pub observed_per_minute: u32,
    pub refused_total: u64,
    /// Non-zero while auto-paused.
    pub paused_until_micros: u64,
}

/// The limiter. One per node, consulted by whatever authors ops.
#[derive(Debug, Default)]
pub struct RateGuard {
    entities: Mutex<BTreeMap<String, Bucket>>,
    limit_per_minute: Mutex<u32>,
    /// Ops refused, and entities paused, since this process started
    /// (ENG-21).
    ///
    /// Beside the per-entity `refused_total`, not derived from it:
    /// `forget` drops an entity when its definition changes and `sweep`
    /// drops idle ones, so a sum over the map falls — and a counter that
    /// falls is read by Prometheus as a reset and turned into a spike of
    /// refusals that never happened. The per-entity number answers *who*;
    /// this one answers *how often*, which is the one a graph needs.
    refused_total: AtomicU64,
    paused_total: AtomicU64,
}

impl RateGuard {
    pub fn new(limit_per_minute: u32) -> RateGuard {
        RateGuard {
            entities: Mutex::new(BTreeMap::new()),
            limit_per_minute: Mutex::new(limit_per_minute),
            refused_total: AtomicU64::new(0),
            paused_total: AtomicU64::new(0),
        }
    }

    pub fn limit(&self) -> u32 {
        *self.limit_per_minute.lock()
    }

    /// Ops this limiter has refused since the process started.
    pub fn refused_total(&self) -> u64 {
        self.refused_total.load(Ordering::Relaxed)
    }

    /// Times it has put an entity in a pause since the process started.
    pub fn paused_total(&self) -> u64 {
        self.paused_total.load(Ordering::Relaxed)
    }

    /// Apply the configured limit. `0` switches the limiter off entirely —
    /// see [`crate::config::StateSection::op_rate_per_minute`] for why that
    /// escape hatch exists.
    pub fn set_limit(&self, limit_per_minute: u32) {
        *self.limit_per_minute.lock() = limit_per_minute;
    }

    /// Is the limiter on at all?
    pub fn is_on(&self) -> bool {
        self.limit() > 0
    }

    /// May `entity` author one op now?
    ///
    /// A token bucket, refilled continuously. The arithmetic is deliberately
    /// boring: a fixed window would let twice the limit through at a window
    /// boundary, and a leaky bucket would smooth away exactly the burst
    /// property 2 exists to protect.
    pub fn admit(&self, entity: &str, now_micros: u64) -> Admit {
        let limit = self.limit();
        if limit == 0 {
            return Admit::Yes;
        }
        let rate_per_micro = f64::from(limit) / 60_000_000.0;
        let capacity = f64::from(limit) * BURST_FACTOR;

        let mut entities = self.entities.lock();
        let bucket = entities.entry(entity.to_string()).or_insert(Bucket {
            tokens: capacity,
            last_micros: now_micros,
            attempts: 0,
            episode_started_micros: now_micros,
            refusals: 0,
            paused_until_micros: 0,
            refused_total: 0,
            unreported: false,
        });

        // Refill for the time that passed. `saturating_sub` because a clock
        // that went backwards must not mint tokens.
        let elapsed = now_micros.saturating_sub(bucket.last_micros);
        bucket.tokens = (bucket.tokens + elapsed as f64 * rate_per_micro).min(capacity);
        bucket.last_micros = now_micros;
        bucket.attempts += 1;

        let observed = observed_per_minute(bucket, now_micros);

        if bucket.paused_until_micros > now_micros {
            return Admit::Paused {
                entity: entity.to_string(),
                until_micros: bucket.paused_until_micros,
                observed_per_minute: observed,
            };
        }
        // The pause expired: a clean slate, so a fixed deployment is not
        // judged on what the broken one did.
        if bucket.paused_until_micros != 0 {
            bucket.paused_until_micros = 0;
            bucket.refusals = 0;
            bucket.attempts = 1;
            bucket.episode_started_micros = now_micros;
            bucket.tokens = capacity;
        }

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            // Back under the limit for a full bucket's worth of headroom:
            // the episode is over. Without this a single overshoot would
            // count towards a pause for ever.
            if bucket.tokens >= capacity - 1.0 {
                bucket.refusals = 0;
            }
            return Admit::Yes;
        }

        bucket.refusals += 1;
        bucket.refused_total += 1;
        self.refused_total.fetch_add(1, Ordering::Relaxed);
        // One report per episode, set on the transition into pressure.
        if bucket.refusals == 1 {
            bucket.unreported = true;
        }
        if bucket.refusals >= PAUSE_AFTER_REFUSALS {
            bucket.paused_until_micros = now_micros + PAUSE_SECS * 1_000_000;
            self.paused_total.fetch_add(1, Ordering::Relaxed);
            return Admit::Paused {
                entity: entity.to_string(),
                until_micros: bucket.paused_until_micros,
                observed_per_minute: observed,
            };
        }
        Admit::Refused {
            entity: entity.to_string(),
            limit_per_minute: limit,
            observed_per_minute: observed,
        }
    }

    /// Is this the *first* refusal of an episode?
    ///
    /// The caller audits on a transition and stays quiet afterwards, because
    /// `AuditAppend` never compacts and a per-refusal entry would turn a
    /// growth problem into a permanent one.
    pub fn just_entered_pressure(&self, entity: &str) -> bool {
        self.entities
            .lock()
            .get(entity)
            .is_some_and(|b| b.refusals == 1)
    }

    /// Entities under pressure now, worst first — what `doctor` prints and
    /// Director shows.
    pub fn pressured(&self, now_micros: u64) -> Vec<Pressure> {
        let mut out: Vec<Pressure> = self
            .entities
            .lock()
            .iter()
            .filter(|(_, b)| b.refused_total > 0)
            .map(|(entity, b)| Pressure {
                entity: entity.clone(),
                observed_per_minute: observed_per_minute(b, now_micros),
                refused_total: b.refused_total,
                paused_until_micros: if b.paused_until_micros > now_micros {
                    b.paused_until_micros
                } else {
                    0
                },
            })
            .collect();
        out.sort_by(|a, b| {
            b.refused_total
                .cmp(&a.refused_total)
                .then(a.entity.cmp(&b.entity))
        });
        out
    }

    /// Episodes that have started since the last drain, for the caller to
    /// audit — **once each**.
    ///
    /// The audit rule, and it is the same one B3 applies to
    /// `history.replay`: `AuditAppend` is `Grow` and never compacts, so an
    /// entry per refused op would let a runaway loop turn a log-growth
    /// problem into a *permanent* one. The control would become the
    /// incident. So the transition is audited and the traffic is not, and
    /// the count of what was refused rides on the one entry.
    ///
    /// Drained rather than read, so a caller that crashes between the read
    /// and the write loses one entry rather than repeating it for ever.
    pub fn drain_new_episodes(&self, now_micros: u64) -> Vec<Pressure> {
        let mut entities = self.entities.lock();
        let mut out = Vec::new();
        for (entity, b) in entities.iter_mut() {
            if !b.unreported {
                continue;
            }
            b.unreported = false;
            out.push(Pressure {
                entity: entity.clone(),
                observed_per_minute: observed_per_minute(b, now_micros),
                refused_total: b.refused_total,
                paused_until_micros: if b.paused_until_micros > now_micros {
                    b.paused_until_micros
                } else {
                    0
                },
            });
        }
        out
    }

    /// Forget an entity — called when whatever defines it changes, so an
    /// edited workflow is not judged on what the old one did. The same rule
    /// `FlapGuard::forget` follows, for the same reason.
    pub fn forget(&self, entity: &str) {
        self.entities.lock().remove(entity);
    }

    /// Drop entities that have been quiet for a while, so a long-lived node
    /// does not accumulate a map entry per job it ever ran.
    ///
    /// Anything with refusals on the record is kept: that is the diagnosis,
    /// and throwing it away is how *"it was fine when I looked"* happens.
    pub fn sweep(&self, now_micros: u64, idle_secs: u64) {
        let cutoff = now_micros.saturating_sub(idle_secs * 1_000_000);
        self.entities
            .lock()
            .retain(|_, b| b.refused_total > 0 || b.last_micros >= cutoff);
    }
}

/// Ops a minute over the current episode.
///
/// Reported rather than measured against, and the distinction matters: the
/// bucket decides, and this is the number a person reads when they ask why.
/// Floored at one second of observation so a two-op burst in the first
/// millisecond does not report as sixty thousand a minute.
fn observed_per_minute(bucket: &Bucket, now_micros: u64) -> u32 {
    let elapsed = now_micros
        .saturating_sub(bucket.episode_started_micros)
        .max(1_000_000);
    let per_minute = bucket.attempts as f64 * 60_000_000.0 / elapsed as f64;
    per_minute.min(f64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000;

    fn guard() -> RateGuard {
        RateGuard::new(DEFAULT_ENTITY_OPS_PER_MINUTE)
    }

    /// The burst allowance, derived rather than written down: these tests
    /// exist to pin behaviour, and a literal here would make tuning the
    /// constants a test-editing exercise instead of a decision.
    fn burst() -> u32 {
        (f64::from(DEFAULT_ENTITY_OPS_PER_MINUTE) * BURST_FACTOR) as u32
    }

    /// **The regression that makes operators disable the feature.**
    ///
    /// A fan-out authoring a burst of job ops is work somebody asked for. If
    /// this test ever fails, the control is about to be switched off in
    /// every deployment that has one, after which it protects nothing.
    #[test]
    fn a_legitimate_burst_under_the_limit_is_not_refused() {
        let g = guard();
        let now = 1_000 * SEC;
        // Two minutes' worth, instantly — the full burst allowance.
        for i in 0..burst() {
            assert!(
                g.admit("run:deploy", now).is_yes(),
                "op {i} of a legitimate burst was refused"
            );
        }
    }

    /// **The limit is set off the busiest legitimate writer in the tree,
    /// and this is that writer.**
    ///
    /// A workflow's drive loop polls once a second and publishes its run
    /// record when something changed, at most twice a poll — two ops a
    /// second on one key, for as long as the run is active. An hour of that
    /// must not be refused once. If this test fails, either the drive loop
    /// got busier or the limit got tighter, and one of them is wrong.
    #[test]
    fn an_hour_of_a_workflows_drive_loop_is_never_refused() {
        let g = guard();
        let mut now = 1_000 * SEC;
        for second in 0..3_600 {
            for op in 0..2 {
                assert!(
                    g.admit("run:long-running", now).is_yes(),
                    "op {op} at second {second} of an active run was refused"
                );
            }
            now += SEC;
        }
        assert!(g.pressured(now).is_empty());
    }

    /// Sustained, though, is a different thing, and it is refused by name.
    #[test]
    fn a_loop_is_refused_and_the_refusal_names_the_entity_and_the_limit() {
        let g = guard();
        let now = 1_000 * SEC;
        for _ in 0..burst() {
            assert!(g.admit("run:looping", now).is_yes());
        }
        let refused = g.admit("run:looping", now);
        let Admit::Refused {
            entity,
            limit_per_minute,
            observed_per_minute,
        } = &refused
        else {
            panic!("expected a refusal, got {refused:?}");
        };
        assert_eq!(entity, "run:looping");
        assert_eq!(*limit_per_minute, DEFAULT_ENTITY_OPS_PER_MINUTE);
        assert!(*observed_per_minute > DEFAULT_ENTITY_OPS_PER_MINUTE);
        let message = refused.message();
        assert!(message.contains("run:looping"), "{message}");
        assert!(
            message.contains(&DEFAULT_ENTITY_OPS_PER_MINUTE.to_string()),
            "{message}"
        );
        // And it says *where* it was refused, which is the property that
        // gets the bug fixed instead of absorbed.
        assert!(message.contains("where it was produced"), "{message}");
    }

    /// One entity's loop must not spend another entity's budget.
    #[test]
    fn one_entity_under_pressure_does_not_starve_another() {
        let g = guard();
        let now = 1_000 * SEC;
        for _ in 0..(burst() * 2) {
            let _ = g.admit("run:looping", now);
        }
        assert!(
            g.admit("run:innocent", now).is_yes(),
            "a second entity was starved by the first"
        );
    }

    /// The rate is sustained, so time is what buys headroom back.
    #[test]
    fn waiting_earns_the_budget_back_at_the_stated_rate() {
        let g = guard();
        let mut now = 1_000 * SEC;
        for _ in 0..burst() {
            assert!(g.admit("job:x", now).is_yes());
        }
        assert!(!g.admit("job:x", now).is_yes());
        // A second's refill is `limit / 60` ops — no more, no less.
        now += SEC;
        let earned = DEFAULT_ENTITY_OPS_PER_MINUTE / 60;
        for i in 0..earned {
            assert!(
                g.admit("job:x", now).is_yes(),
                "op {i} of a second's refill"
            );
        }
        assert!(
            !g.admit("job:x", now).is_yes(),
            "more than a second was earned"
        );
    }

    /// Sustained refusal stops costing per-op work, and **the pause is a
    /// brake rather than a tombstone**: it expires on its own.
    #[test]
    fn a_sustained_loop_is_paused_and_the_pause_expires_by_itself() {
        let g = guard();
        let now = 1_000 * SEC;
        let mut paused_until = 0;
        for _ in 0..(burst() + PAUSE_AFTER_REFUSALS) {
            if let Admit::Paused { until_micros, .. } = g.admit("run:hot", now) {
                paused_until = until_micros;
            }
        }
        assert!(paused_until > now, "the entity was never paused");
        assert!(matches!(g.admit("run:hot", now), Admit::Paused { .. }));

        // …and afterwards it may try again, with a clean slate, without
        // anybody being paged to un-pause it.
        let after = paused_until + SEC;
        assert!(
            g.admit("run:hot", after).is_yes(),
            "an expired pause must let the entity try again"
        );
    }

    /// Episodes are drained once, so a loop that is refused ten thousand
    /// times leaves one audit entry and not ten thousand.
    #[test]
    fn an_episode_is_drained_once_and_carries_its_own_count() {
        let g = guard();
        let now = 1_000 * SEC;
        for _ in 0..(burst() + 20) {
            let _ = g.admit("run:hot", now);
        }
        let first = g.drain_new_episodes(now);
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].refused_total >= 20, "{first:?}");
        assert!(
            g.drain_new_episodes(now).is_empty(),
            "an episode must be reported once, not once per drain"
        );

        // A *new* episode after recovery is a new report — the same entity
        // looping again next week is news.
        let later = now + 3_600 * SEC;
        for _ in 0..(burst() + 5) {
            let _ = g.admit("run:hot", later);
        }
        assert_eq!(g.drain_new_episodes(later).len(), 1);
    }

    /// The audit rule: one entry on the transition, none while it lasts.
    /// `AuditAppend` never compacts, so a per-refusal entry would turn a
    /// growth problem into a permanent one — the control becoming the
    /// incident.
    #[test]
    fn only_the_first_refusal_of_an_episode_is_worth_auditing() {
        let g = guard();
        let now = 1_000 * SEC;
        for _ in 0..burst() {
            assert!(g.admit("fact:disk:node-a", now).is_yes());
        }
        let _ = g.admit("fact:disk:node-a", now);
        assert!(g.just_entered_pressure("fact:disk:node-a"));
        for _ in 0..10 {
            let _ = g.admit("fact:disk:node-a", now);
        }
        assert!(
            !g.just_entered_pressure("fact:disk:node-a"),
            "an episode must be audited once, not once per op"
        );
    }

    /// What `doctor` prints: the offender, what it is doing, and how much
    /// has been refused — worst first.
    #[test]
    fn pressure_is_reported_worst_first_and_keeps_the_diagnosis() {
        let g = guard();
        let now = 1_000 * SEC;
        for _ in 0..(burst() + 40) {
            let _ = g.admit("run:worse", now);
        }
        for _ in 0..(burst() + 5) {
            let _ = g.admit("run:bad", now);
        }
        let _ = g.admit("run:fine", now);

        let rows = g.pressured(now);
        assert_eq!(rows.len(), 2, "only entities that were refused: {rows:?}");
        assert_eq!(rows[0].entity, "run:worse");
        assert_eq!(rows[1].entity, "run:bad");
        assert!(rows[0].refused_total > rows[1].refused_total);

        // And the diagnosis survives a sweep, because "it was fine when I
        // looked" is the failure mode of throwing it away.
        g.sweep(now + 3_600 * SEC, 60);
        assert_eq!(g.pressured(now + 3_600 * SEC).len(), 2);
    }

    /// A quiet entity is forgotten, so a node that has run a million jobs
    /// does not carry a million map entries.
    #[test]
    fn quiet_entities_are_swept_and_busy_ones_are_kept() {
        let g = guard();
        let now = 1_000 * SEC;
        assert!(g.admit("job:done", now).is_yes());
        g.sweep(now + 3_600 * SEC, 60);
        // Nothing to report either way — but the entry is gone, so the next
        // op starts from a full bucket.
        assert!(g.pressured(now).is_empty());
        assert!(g.admit("job:done", now + 3_600 * SEC).is_yes());
    }

    /// An edited entity is not judged on what the old one did.
    #[test]
    fn forgetting_an_entity_clears_its_record() {
        let g = guard();
        let now = 1_000 * SEC;
        for _ in 0..(burst() + 60) {
            let _ = g.admit("run:edited", now);
        }
        assert!(!g.pressured(now).is_empty());
        g.forget("run:edited");
        assert!(g.pressured(now).is_empty());
        assert!(g.admit("run:edited", now).is_yes());
    }

    /// …and the *counter* still remembers, which is a different question
    /// (ENG-21).
    ///
    /// The per-entity record is diagnosis and is meant to be cleared. The
    /// scraped counter is how often this limiter has bitten, and a counter
    /// that falls is read by Prometheus as a reset and turned into a spike
    /// of refusals that never happened — so it cannot be a sum over a map
    /// that `forget` and `sweep` take entries out of.
    #[test]
    fn the_scraped_counter_does_not_fall_when_an_entity_is_forgotten() {
        let g = guard();
        let now = 1_000 * SEC;
        for _ in 0..(burst() + 60) {
            let _ = g.admit("run:edited", now);
        }
        let refused = g.refused_total();
        assert!(refused > 0, "the premise: the limiter refused something");
        assert!(g.paused_total() > 0, "…and paused the entity");

        g.forget("run:edited");
        g.sweep(now + 3_600 * SEC, 60);
        assert!(
            g.pressured(now).is_empty(),
            "the premise: the per-entity record really did go"
        );
        assert_eq!(
            g.refused_total(),
            refused,
            "a counter must never fall when a record is forgotten"
        );
    }
}
