//! C1 P1/P2 — the live usage counter, and the write rate that made C0 a
//! prerequisite (C0–C9 §5.2).
//!
//! # Why this is not one op per job
//!
//! The obvious implementation publishes a counter update as each job
//! finishes. On a mesh doing thousands of jobs a day that is thousands of
//! replicated ops a day, forever, which is precisely the "first entity
//! whose write rate is proportional to cluster throughput" that C0 exists
//! to survive — and surviving it is not the same as causing it needlessly.
//!
//! So charges accumulate **in memory**, and a flush publishes one op per
//! `(principal, window)` that actually changed. The write rate is therefore
//! bounded by *principals*, not by jobs: a node running ten thousand jobs
//! an hour for three users publishes at most a handful of ops a minute, and
//! an idle node publishes none at all.
//!
//! # Why losing a flush is survivable
//!
//! A partial is a **running total**, not a delta, and merges by per-
//! component max. A crash between a charge and its flush loses that
//! interval's accumulation and nothing else: the next flush publishes the
//! new total, which is what every reader wants anyway. Nothing double-
//! counts, nothing goes backwards, and no delivery guarantee is needed. A
//! delta-based counter would need exactly-once delivery, which anti-entropy
//! does not promise.
//!
//! The one honest consequence: totals reset on restart, because the
//! accumulator is memory. The node re-reads its own last published totals
//! from replicated state on start, so what it resumes from is what the mesh
//! already believes — see [`UsageMeter::resume`].

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;

use mesh_proto::{OpBody, UsagePartial, UserId};
use tracing::{debug, warn};

use crate::accounting::Chargeable;
use crate::state::StateEngine;

/// How often accumulated charges are published.
///
/// Not a setting, for the reason the compaction interval is not one: it
/// changes how *promptly* a share becomes visible and nothing else an
/// operator can act on. Thirty seconds is well inside a gossip round, so
/// "every node agrees on their shares within a gossip round" — C1 P2's
/// demo — holds with room to spare.
pub const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// This node's running totals per `(principal, window)`, and the publisher
/// that keeps the mesh's copy in step with them.
#[derive(Debug)]
pub struct UsageMeter {
    engine: Arc<StateEngine>,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The running total for each window this node has charged.
    totals: BTreeMap<(UserId, u64), UsagePartial>,
    /// What was last published, so a flush writes only what moved.
    published: BTreeMap<(UserId, u64), UsagePartial>,
}

impl UsageMeter {
    pub fn new(engine: Arc<StateEngine>) -> Arc<UsageMeter> {
        let meter = Arc::new(UsageMeter {
            engine,
            inner: Mutex::new(Inner::default()),
        });
        meter.resume();
        meter
    }

    /// Adopt this node's own published totals as the starting point.
    ///
    /// Without this, a restart would publish totals that start again from
    /// zero — and because the merge takes the max, the mesh would keep the
    /// old, larger number while this node slowly climbed back to it,
    /// silently under-charging the principal for a whole window. Resuming
    /// from what the mesh already believes makes a restart cost nothing.
    fn resume(self: &Arc<Self>) {
        let me = self.engine.node_id();
        let now = mesh_proto::now_micros();
        let mine: Vec<UsagePartial> = self.engine.read(|s| {
            s.usage
                .iter()
                .filter(|((node, _, w), _)| {
                    *node == me && mesh_proto::usage_window_is_live(*w, now)
                })
                .map(|(_, partial)| *partial)
                .collect()
        });
        if mine.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        for partial in mine {
            let key = (partial.principal, partial.window);
            inner.totals.insert(key, partial);
            inner.published.insert(key, partial);
        }
        debug!(
            windows = inner.totals.len(),
            "resumed usage counters from replicated state"
        );
    }

    /// Charge one finished job.
    pub fn charge(&self, c: &Chargeable) {
        let mut inner = self.inner.lock();
        inner
            .totals
            .entry((c.principal, c.window))
            .or_insert_with(|| UsagePartial::empty(c.principal, c.window))
            .charge(
                c.charge.cpu_secs,
                c.charge.mem_mb_secs,
                c.charge.gpu_secs,
                c.charge.wall_secs,
            );
    }

    /// Publish every window whose total moved since the last flush.
    ///
    /// Returns how many ops were authored. Zero on an idle node, which is
    /// the number that matters: accounting must not make a quiet mesh
    /// chatty.
    pub fn flush(&self) -> usize {
        let pending: Vec<UsagePartial> = {
            let inner = self.inner.lock();
            inner
                .totals
                .iter()
                .filter(|(key, total)| inner.published.get(*key) != Some(*total))
                .map(|(_, total)| *total)
                .collect()
        };
        if pending.is_empty() {
            return 0;
        }
        let bodies: Vec<OpBody> = pending
            .iter()
            .map(|partial| OpBody::UsageAdd {
                usage: Box::new(*partial),
            })
            .collect();
        let n = bodies.len();
        if let Err(e) = self.engine.author_ops(bodies) {
            // Never fatal. The totals stay pending and the next flush
            // publishes them — a running total is idempotent, so a failed
            // publish costs latency and nothing else.
            warn!(error = %e, windows = n, "could not publish usage counters");
            return 0;
        }
        let mut inner = self.inner.lock();
        for partial in pending {
            inner
                .published
                .insert((partial.principal, partial.window), partial);
        }
        n
    }

    /// Drop windows nobody reads any more from the accumulator.
    ///
    /// The same local economy as `StateEngine::expire_usage`, and invisible
    /// for the same reason: a window past the decay horizon contributes to
    /// no answer, so forgetting it changes none.
    pub fn expire(&self, now_micros: u64) -> usize {
        let mut inner = self.inner.lock();
        let before = inner.totals.len();
        inner
            .totals
            .retain(|(_, w), _| mesh_proto::usage_window_is_live(*w, now_micros));
        inner
            .published
            .retain(|(_, w), _| mesh_proto::usage_window_is_live(*w, now_micros));
        before - inner.totals.len()
    }

    /// This node's own totals, unpublished ones included. Diagnostics.
    pub fn totals(&self) -> Vec<UsagePartial> {
        self.inner.lock().totals.values().copied().collect()
    }
}

// ---------------------------------------------------------------------------
// Reporting (C1 P1) and dominant share (the input C1 P4 orders a queue by)
// ---------------------------------------------------------------------------

/// A principal's **dominant share**: the largest fraction of any single
/// resource they hold, out of what the whole mesh is using (C1 §5.2).
///
/// This is the quantity Dominant Resource Fairness is named for, and the
/// reason it is the right fit here rather than a weighted sum: it needs no
/// exchange rate between a core and a gigabyte. A principal using 20% of
/// the CPU and 60% of the memory has a dominant share of 60%, and DRF
/// treats them as having 60% of the cluster — which is the honest answer,
/// because the memory is what everybody else cannot have.
///
/// Returned in tenths of a percent so ordering is stable and integral;
/// floating point in a scheduling comparison is how two nodes order a queue
/// differently on the same inputs.
pub fn dominant_share_permille(mine: &Totals, whole: &Totals) -> u32 {
    let ratio = |part: u64, total: u64| -> u32 {
        part.saturating_mul(1000)
            .checked_div(total)
            .unwrap_or(0)
            .min(1000) as u32
    };
    ratio(mine.cpu_secs, whole.cpu_secs)
        .max(ratio(mine.mem_mb_secs, whole.mem_mb_secs))
        .max(ratio(mine.gpu_secs, whole.gpu_secs))
}

/// A resource vector, summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub cpu_secs: u64,
    pub mem_mb_secs: u64,
    pub gpu_secs: u64,
    pub wall_secs: u64,
    pub jobs: u64,
}

/// Every principal's dominant share, weighted so that each gets an equal
/// slice of the pool (C1 P4).
///
/// A weight of `1000 / n` rather than none at all, so the numbers keep the
/// meaning `meshnode queue` has always printed: a principal exactly on an
/// equal share reads 1000. Only the ordering is load-bearing — lower is
/// further behind, and further behind runs first. (Quotas once supplied
/// per-principal weights here; they were removed.)
pub fn weighted_shares(usage: &BTreeMap<UserId, (u64, u64, u64)>) -> BTreeMap<UserId, u32> {
    let mut whole = Totals::default();
    for (cpu, mem, gpu) in usage.values() {
        whole.cpu_secs = whole.cpu_secs.saturating_add(*cpu);
        whole.mem_mb_secs = whole.mem_mb_secs.saturating_add(*mem);
        whole.gpu_secs = whole.gpu_secs.saturating_add(*gpu);
    }
    let principals = usage.len() as u32;
    let weight = 1000u32.checked_div(principals).unwrap_or(1000).max(1);

    usage
        .iter()
        .map(|(who, (cpu, mem, gpu))| {
            let mine = Totals {
                cpu_secs: *cpu,
                mem_mb_secs: *mem,
                gpu_secs: *gpu,
                ..Default::default()
            };
            let share = dominant_share_permille(&mine, &whole);
            // Integer throughout: two nodes must reach the same ordering
            // from the same inputs, and floating point in a scheduling
            // comparison is how they stop doing that.
            (*who, (u64::from(share) * 1000 / u64::from(weight)) as u32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::accounting::Charge;
    use crate::genesis::Genesis;
    use crate::identity::Keypair;
    use mesh_proto::MemberInfo;
    use mesh_proto::usage_window;

    fn engine() -> (tempfile::TempDir, Arc<StateEngine>, Keypair) {
        let dir = tempfile::tempdir().unwrap();
        let user = Keypair::generate().unwrap();
        let node = Keypair::generate().unwrap();
        let genesis = Genesis::create("usage-mesh", &user, node.node_id()).unwrap();
        let e = StateEngine::open(dir.path(), node.clone(), genesis).unwrap();
        e.author_op(OpBody::MemberAdd {
            member: MemberInfo {
                node_id: node.node_id(),
                name: "alpha".into(),
                addrs: vec![],
                tags: vec![],
                slots: 8,
                joined_at_micros: mesh_proto::now_micros(),
            },
            invite: None,
        })
        .unwrap();
        (dir, e, node)
    }

    fn charge(who: UserId, window: u64, cpu: u64) -> Chargeable {
        Chargeable {
            principal: who,
            window,
            charge: Charge {
                wall_secs: cpu,
                cpu_secs: cpu,
                mem_mb_secs: 0,
                gpu_secs: 0,
                measured_cpu_secs: None,
            },
        }
    }

    #[test]
    fn a_thousand_jobs_publish_one_op() {
        // The whole point of the meter, and the reason C0 came first.
        let (_d, engine, node) = engine();
        let meter = UsageMeter::new(engine.clone());
        let who = UserId([1u8; 32]);
        let before = engine.all_ops().unwrap().len();
        for _ in 0..1_000 {
            meter.charge(&charge(who, 42, 3));
        }
        assert_eq!(meter.flush(), 1);
        assert_eq!(engine.all_ops().unwrap().len(), before + 1);

        let total = engine.read(|s| s.usage.get(&(node.node_id(), who, 42)).copied());
        let total = total.expect("the window was published");
        assert_eq!(total.cpu_secs, 3_000);
        assert_eq!(total.jobs, 1_000);
    }

    #[test]
    fn an_idle_node_publishes_nothing() {
        let (_d, engine, _) = engine();
        let meter = UsageMeter::new(engine.clone());
        assert_eq!(meter.flush(), 0);
        meter.charge(&charge(UserId([1u8; 32]), 1, 5));
        assert_eq!(meter.flush(), 1);
        // Nothing moved since; nothing to say.
        assert_eq!(meter.flush(), 0);
        assert_eq!(meter.flush(), 0);
    }

    #[test]
    fn each_principal_and_window_is_its_own_op() {
        let (_d, engine, _) = engine();
        let meter = UsageMeter::new(engine.clone());
        meter.charge(&charge(UserId([1u8; 32]), 1, 5));
        meter.charge(&charge(UserId([2u8; 32]), 1, 5));
        meter.charge(&charge(UserId([1u8; 32]), 2, 5));
        assert_eq!(meter.flush(), 3);
    }

    #[test]
    fn a_restart_resumes_from_what_the_mesh_already_believes() {
        // Otherwise the max-merge keeps the old, larger total while this
        // node climbs back to it — silently free work for one window.
        let (_d, engine, node) = engine();
        let who = UserId([7u8; 32]);
        let window = usage_window(mesh_proto::now_micros());
        {
            let meter = UsageMeter::new(engine.clone());
            for _ in 0..10 {
                meter.charge(&charge(who, window, 100));
            }
            assert_eq!(meter.flush(), 1);
        }
        // A fresh meter over the same engine: the process restarted.
        let meter = UsageMeter::new(engine.clone());
        meter.charge(&charge(who, window, 1));
        assert_eq!(meter.flush(), 1);
        let total = engine
            .read(|s| s.usage.get(&(node.node_id(), who, window)).copied())
            .unwrap();
        assert_eq!(total.cpu_secs, 1_001, "the restart lost the earlier total");
        assert_eq!(total.jobs, 11);
    }
}
