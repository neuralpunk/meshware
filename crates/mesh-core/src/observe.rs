//! Group work, measured; and the id that ties a log back to one request
//! (ENG-21).
//!
//! ## Group work
//!
//! A fan-out, a parallel run and a gang are the three ways one submission
//! becomes many jobs, and none of them was measured. `meshware_jobs_*`
//! counts what *this node executes*, which is the wrong half of the
//! question for all three: the owner of a 200-target fan-out executes
//! none of it, so on the machine a person is actually looking at the whole
//! run is invisible.
//!
//! Counted where it happens, never derived from replicated state — the
//! rule [`crate::jobs::JobMeters`] was written to enforce, and for the
//! same reason: a plan record can be retired by retention, and a counter
//! derived from what is still held falls when it is.
//!
//! ## Correlation
//!
//! There is a second question a dashboard cannot answer: *what did this
//! node do for that request?* Spans answer it, and they need something to
//! join on.
//!
//! **Two ids, and neither is invented here twice.** Within a node, a
//! request carries a [`TraceId`] minted at the door it came in by, so
//! every line logged while handling it — through six layers and two
//! spawned tasks — carries the same `trace=`. Across nodes, the join is
//! the *job*, *group* or *gang* id, which is already a mesh-wide
//! identifier that the plan replicates and the executor journals. A
//! synthetic id pushed over the wire would need a protocol field to carry
//! something those ids already say.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::latency::Histogram;

/// The three ways one submission becomes many jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    /// One command, many nodes (M5).
    Fanout,
    /// One item list, many workers pulling chunks (M9).
    Parallel,
    /// N jobs that must run at the same time or not at all (C4).
    Gang,
}

impl GroupKind {
    /// The `kind` label. Stable: it is what a dashboard selects on.
    pub fn as_str(self) -> &'static str {
        match self {
            GroupKind::Fanout => "fanout",
            GroupKind::Parallel => "parallel",
            GroupKind::Gang => "gang",
        }
    }
}

/// How a group run ended, from the owner's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOutcome {
    /// Every target ran and none failed.
    Completed,
    /// It ran, and at least one target failed.
    Failed,
    /// It stopped early on purpose — `--halt-on-fail`, a won race, a
    /// gang that could not seat every member.
    Halted,
}

impl GroupOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            GroupOutcome::Completed => "completed",
            GroupOutcome::Failed => "failed",
            GroupOutcome::Halted => "halted",
        }
    }
}

/// What one kind of group run has done on this node since it started.
#[derive(Debug)]
struct KindMeters {
    started: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    halted: AtomicU64,
    /// Sub-jobs, by how each ended. `skipped` is the one that is hard to
    /// see any other way: a halted fan-out's remaining targets never
    /// became jobs anywhere, so nothing else in the mesh records them.
    subjobs_succeeded: AtomicU64,
    subjobs_failed: AtomicU64,
    subjobs_skipped: AtomicU64,
    /// Wall time from the plan being replicated to the run ending. On
    /// [`crate::latency::WORK_BUCKETS`]: a fan-out that runs for an hour
    /// against latency buckets is one observation in `+Inf`.
    duration: Histogram,
}

impl Default for KindMeters {
    fn default() -> KindMeters {
        KindMeters {
            started: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            halted: AtomicU64::new(0),
            subjobs_succeeded: AtomicU64::new(0),
            subjobs_failed: AtomicU64::new(0),
            subjobs_skipped: AtomicU64::new(0),
            duration: Histogram::for_work(),
        }
    }
}

/// Fan-out, parallel and gang runs this node owns.
///
/// **Owns**, not takes part in: a worker that ran four chunks of somebody
/// else's parallel run counts four jobs and no run, because counting the
/// run on every participant would report one submission as N.
#[derive(Debug, Default)]
pub struct GroupMeters {
    fanout: KindMeters,
    parallel: KindMeters,
    gang: KindMeters,
}

impl GroupMeters {
    fn kind(&self, kind: GroupKind) -> &KindMeters {
        match kind {
            GroupKind::Fanout => &self.fanout,
            GroupKind::Parallel => &self.parallel,
            GroupKind::Gang => &self.gang,
        }
    }

    /// A run this node owns has been planned and is about to dispatch.
    pub fn started(&self, kind: GroupKind) {
        self.kind(kind).started.fetch_add(1, Ordering::Relaxed);
    }

    /// A run this node owns is over, however it ended.
    pub fn finished(
        &self,
        kind: GroupKind,
        outcome: GroupOutcome,
        elapsed: std::time::Duration,
        succeeded: u64,
        failed: u64,
        skipped: u64,
    ) {
        let m = self.kind(kind);
        let counter = match outcome {
            GroupOutcome::Completed => &m.completed,
            GroupOutcome::Failed => &m.failed,
            GroupOutcome::Halted => &m.halted,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        m.subjobs_succeeded.fetch_add(succeeded, Ordering::Relaxed);
        m.subjobs_failed.fetch_add(failed, Ordering::Relaxed);
        m.subjobs_skipped.fetch_add(skipped, Ordering::Relaxed);
        m.duration.record(elapsed);
    }

    /// Render every group series. All three kinds are always present, at
    /// zero if need be: a series that appears only once it is non-zero is
    /// one no dashboard can be built on before the incident.
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write as _;
        let kinds = [
            (GroupKind::Fanout, &self.fanout),
            (GroupKind::Parallel, &self.parallel),
            (GroupKind::Gang, &self.gang),
        ];

        let _ = writeln!(
            out,
            "# HELP meshware_group_runs_started_total Fan-out, parallel and gang runs this node \
             began owning, by kind."
        );
        let _ = writeln!(out, "# TYPE meshware_group_runs_started_total counter");
        for (kind, m) in kinds {
            let _ = writeln!(
                out,
                "meshware_group_runs_started_total{{kind=\"{}\"}} {}",
                kind.as_str(),
                m.started.load(Ordering::Relaxed)
            );
        }

        let _ = writeln!(
            out,
            "# HELP meshware_group_runs_finished_total Group runs this node owned that ended, by \
             kind and how they ended. `halted` stopped early on purpose; `failed` ran and lost a \
             target."
        );
        let _ = writeln!(out, "# TYPE meshware_group_runs_finished_total counter");
        for (kind, m) in kinds {
            for (outcome, counter) in [
                (GroupOutcome::Completed, &m.completed),
                (GroupOutcome::Failed, &m.failed),
                (GroupOutcome::Halted, &m.halted),
            ] {
                let _ = writeln!(
                    out,
                    "meshware_group_runs_finished_total{{kind=\"{}\",outcome=\"{}\"}} {}",
                    kind.as_str(),
                    outcome.as_str(),
                    counter.load(Ordering::Relaxed)
                );
            }
        }

        let _ = writeln!(
            out,
            "# HELP meshware_group_subjobs_total Sub-jobs of the group runs this node owned, by \
             kind and outcome. `skipped` never became a job on any node, so nothing else in the \
             mesh records it."
        );
        let _ = writeln!(out, "# TYPE meshware_group_subjobs_total counter");
        for (kind, m) in kinds {
            for (outcome, counter) in [
                ("succeeded", &m.subjobs_succeeded),
                ("failed", &m.subjobs_failed),
                ("skipped", &m.subjobs_skipped),
            ] {
                let _ = writeln!(
                    out,
                    "meshware_group_subjobs_total{{kind=\"{}\",outcome=\"{outcome}\"}} {}",
                    kind.as_str(),
                    counter.load(Ordering::Relaxed)
                );
            }
        }

        crate::latency::render_family(
            out,
            "meshware_group_run",
            "Wall time a group run this node owned took, from the plan being replicated to the \
             last target reporting.",
            "kind",
            &kinds
                .iter()
                .map(|(kind, m)| (kind.as_str(), &m.duration))
                .collect::<Vec<_>>(),
        );
    }
}

/// A short id for one request, to join a log on.
///
/// **Not a UUID and not a hash.** It is read by a person pasting it into
/// `grep`, it lives for the length of one request, and it is never
/// persisted or compared across nodes — so the bar it has to clear is
/// "two requests in flight on one node at one time do not collide", which
/// a counter clears exactly and a random id only probably. The process
/// start supplies the high bits so that two runs of the daemon in one
/// day's logs do not both start at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceId(u64);

static NEXT_TRACE: AtomicU64 = AtomicU64::new(0);

impl TraceId {
    /// The next id. Monotonic within a process, and cheap enough to mint
    /// on every request including the ones that turn out to be refused.
    #[allow(clippy::new_without_default)]
    pub fn new() -> TraceId {
        let n = NEXT_TRACE.fetch_add(1, Ordering::Relaxed);
        // Seeded once, from the clock, so ids are not the same sequence on
        // every boot — the one property that matters when two days of a
        // node's logs are in the same place.
        static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let seed = *SEED.get_or_init(|| mesh_proto::now_micros() & 0xFFFF);
        TraceId((seed << 24) | (n & 0xFF_FFFF))
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:010x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The regression these meters exist for, in miniature: what a node
    /// *owned* is counted as it happens, so nothing that later forgets the
    /// plan can make the count fall.
    #[test]
    fn group_counters_only_go_up() {
        let m = GroupMeters::default();
        m.started(GroupKind::Fanout);
        m.finished(
            GroupKind::Fanout,
            GroupOutcome::Halted,
            Duration::from_secs(90),
            3,
            1,
            6,
        );
        let mut out = String::new();
        m.render(&mut out);

        assert!(out.contains("meshware_group_runs_started_total{kind=\"fanout\"} 1"));
        assert!(
            out.contains(
                "meshware_group_runs_finished_total{kind=\"fanout\",outcome=\"halted\"} 1"
            )
        );
        assert!(
            out.contains("meshware_group_subjobs_total{kind=\"fanout\",outcome=\"skipped\"} 6")
        );
        // The kinds that did nothing are still published, at zero.
        assert!(out.contains("meshware_group_runs_started_total{kind=\"gang\"} 0"));
        assert!(out.contains("meshware_group_runs_started_total{kind=\"parallel\"} 0"));
    }

    /// A 90-second run has to land in a bucket. Against the latency
    /// ladder every group run lands in `+Inf`, which reports "slower than
    /// ten seconds" for everything and is indistinguishable from a
    /// histogram that is broken.
    #[test]
    fn a_run_that_takes_minutes_lands_in_a_bucket() {
        let m = GroupMeters::default();
        m.finished(
            GroupKind::Gang,
            GroupOutcome::Completed,
            Duration::from_secs(90),
            4,
            0,
            0,
        );
        let mut out = String::new();
        m.render(&mut out);
        assert!(
            out.contains("meshware_group_run_seconds_bucket{kind=\"gang\",le=\"300\"} 1"),
            "90s should be under the five-minute edge:\n{out}"
        );
        assert!(
            out.contains("meshware_group_run_seconds_bucket{kind=\"gang\",le=\"60\"} 0"),
            "…and over the one-minute edge:\n{out}"
        );
    }

    /// One `# TYPE` per family, whatever the number of members: Prometheus
    /// keeps the first declaration of a name and drops the rest, so a
    /// second header would publish one kind and lose the other two.
    #[test]
    fn one_declaration_per_family() {
        let mut out = String::new();
        GroupMeters::default().render(&mut out);
        for name in [
            "meshware_group_runs_started_total",
            "meshware_group_runs_finished_total",
            "meshware_group_subjobs_total",
            "meshware_group_run_seconds",
        ] {
            assert_eq!(
                out.matches(&format!("# TYPE {name} ")).count(),
                1,
                "{name} is declared more than once"
            );
        }
    }

    #[test]
    fn trace_ids_do_not_repeat() {
        let ids: std::collections::HashSet<String> =
            (0..1000).map(|_| TraceId::new().to_string()).collect();
        assert_eq!(ids.len(), 1000);
    }
}
