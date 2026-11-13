//! C1 P1 — what a job cost, and who is charged for it (C0–C9 §5.2).
//!
//! # Reserved, not consumed
//!
//! The counter a fair-share scheduler reads is **resource-seconds
//! reserved**, and the decision is deliberate enough to state before the
//! code. A principal who holds thirty-two cores and idles them has still
//! denied them to everybody else; charging measured consumption would let
//! them do exactly that, for free, forever, and would make the share a
//! measure of how *inefficient* someone's code is rather than of how much
//! of the cluster they took. Slurm charges allocated CPU-seconds for the
//! same reason. So does this.
//!
//! **Amended from the plan.** §5.4 P1 says "cgroup-derived resource-seconds
//! at exit". Measured consumption is still collected where the kernel will
//! give it up, and it is genuinely useful — it answers *were we efficient*,
//! which is a real question an operator asks about their own fleet. It is
//! simply not the same question as *whose turn*, and only the second one
//! orders a queue. The two numbers are kept side by side and never confused.
//!
//! # What "reserved" means when nobody declared anything
//!
//! Most jobs state no requirements at all — that is the pre-M9 default and
//! still the common case. A job that declared nothing is charged one CPU
//! and no memory, which is the smallest honest reading of "it occupied a
//! slot": it is never zero, because a job that costs nothing would let a
//! principal run an unbounded number of them without ever losing a share.
//!
//! # Measurement, and its limits
//!
//! meshware does not put each job in its own cgroup — it spawns a process
//! *group* and kills it as one (`jobs.rs`), which is the right shape for
//! its threat model and the wrong shape for a per-job accounting file. So
//! measurement reads `/proc/<pid>/stat` while the job runs: `utime + stime`
//! for the leader, plus `cutime + cstime`, which the kernel folds in as the
//! leader reaps its own children.
//!
//! That sampling has one honest limitation and it is worth naming rather
//! than hiding: **the final tick is lost**, because the process is gone
//! before its last slice can be read. A job shorter than one sample
//! interval measures as zero. It is charged the same as any other job — the
//! charge is reservation-based and does not depend on this at all — and the
//! *measured* column simply reports nothing, which is the truthful answer.

use std::path::{Path, PathBuf};

use mesh_proto::{Requirements, UserId, usage_window};

/// How often a running job's CPU consumption is sampled.
pub const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// What one finished job cost, in the units a share is computed from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Charge {
    /// Wall-clock seconds the job held its slot.
    pub wall_secs: u64,
    /// CPU-seconds *reserved*: wall × declared cpus (minimum one).
    pub cpu_secs: u64,
    /// MB-seconds reserved: wall × declared memory.
    pub mem_mb_secs: u64,
    /// GPU-seconds reserved. Always zero until C5 makes a GPU a
    /// schedulable device; the column exists now so the archive schema and
    /// the DRF resource vector do not have to change when it does.
    pub gpu_secs: u64,
    /// CPU-seconds actually burned, where the kernel would say. `None` on a
    /// platform with no `/proc`, and on a job too short to sample.
    pub measured_cpu_secs: Option<u64>,
}

impl Charge {
    /// What a job that ran for `wall_secs` holding `reqs` cost.
    pub fn of(reqs: Requirements, wall_secs: u64, measured_cpu_secs: Option<u64>) -> Charge {
        // A job that declared nothing still occupied a slot. One CPU is the
        // smallest honest reading; zero would be free work.
        let cpus = u64::from(reqs.cpus.max(1));
        Charge {
            wall_secs,
            cpu_secs: wall_secs.saturating_mul(cpus),
            mem_mb_secs: wall_secs.saturating_mul(reqs.mem_mb),
            gpu_secs: 0,
            measured_cpu_secs,
        }
    }

    /// Efficiency, as a percentage of what was reserved. `None` when
    /// nothing was measured or nothing was reserved — the two cases where
    /// a ratio would be an invention.
    pub fn efficiency_pct(&self) -> Option<u32> {
        let measured = self.measured_cpu_secs?;
        if self.cpu_secs == 0 {
            return None;
        }
        Some(((measured.saturating_mul(100)) / self.cpu_secs) as u32)
    }
}

/// Who is charged, in which window, and how much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chargeable {
    pub principal: UserId,
    pub window: u64,
    pub charge: Charge,
}

/// Work out the charge for a job that has just finished.
///
/// `started` and `finished` are the executor's own stamps in micros. A job
/// that never started — refused, cancelled in the queue — is charged
/// nothing, because it held nothing.
pub fn charge_for(
    principal: UserId,
    reqs: Requirements,
    started_at_micros: Option<u64>,
    finished_at_micros: Option<u64>,
    measured_cpu_secs: Option<u64>,
) -> Option<Chargeable> {
    let started = started_at_micros?;
    let finished = finished_at_micros?;
    if finished <= started {
        // Sub-second and clock-skew cases both land here. A job that
        // measured as instantaneous is charged one second rather than
        // nothing: a thousand instant jobs are still a thousand jobs, and
        // rounding them to free is how a principal games a share.
        return Some(Chargeable {
            principal,
            window: usage_window(finished.max(started)),
            charge: Charge::of(reqs, 1, measured_cpu_secs),
        });
    }
    let wall_secs = ((finished - started) / 1_000_000).max(1);
    Some(Chargeable {
        principal,
        // The window the job *finished* in. Charging a long job's whole
        // cost to the window it started in would let a share be dodged by
        // starting work just before a window rolls; charging it where it
        // lands keeps every second accounted exactly once, in one place.
        window: usage_window(finished),
        charge: Charge::of(reqs, wall_secs, measured_cpu_secs),
    })
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Clock ticks per second, for converting `/proc` CPU fields.
///
/// `sysconf(_SC_CLK_TCK)` is 100 on every Linux meshware supports, and
/// hard-coding it avoids a libc call in a path that must never fail loudly.
/// A platform where it is not 100 reports proportionally wrong *measured*
/// CPU and a perfectly correct charge, which is the right way round.
const CLK_TCK: u64 = 100;

/// CPU-seconds a process and its reaped children have consumed, from
/// `/proc/<pid>/stat`. `None` when the process is gone or the file is not
/// there — a dead process is the normal case at exit, not an error.
pub fn measure_cpu_secs(pid: u32) -> Option<u64> {
    measure_cpu_secs_in(Path::new("/proc"), pid)
}

/// The same, rooted at an arbitrary directory, so the parser is testable
/// without a live process.
pub fn measure_cpu_secs_in(proc_root: &Path, pid: u32) -> Option<u64> {
    let path: PathBuf = proc_root.join(pid.to_string()).join("stat");
    let text = std::fs::read_to_string(path).ok()?;
    parse_stat_cpu(&text)
}

/// `utime + stime + cutime + cstime`, in seconds.
///
/// The comm field is parenthesised and may itself contain spaces and
/// parentheses — `(bash -c "a) b")` is a legal process name — so the parse
/// starts after the **last** `)`, which is what every correct `/proc` stat
/// reader does and what naive ones get wrong.
pub fn parse_stat_cpu(text: &str) -> Option<u64> {
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the closing paren, field 0 is `state` (field 3 in the manpage's
    // 1-based numbering), so utime (14) is at index 11.
    let get = |i: usize| -> u64 { fields.get(i).and_then(|f| f.parse().ok()).unwrap_or(0) };
    let ticks = get(11) + get(12) + get(13) + get(14);
    Some(ticks / CLK_TCK)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn an_undeclared_job_is_charged_one_cpu_and_never_nothing() {
        let c = Charge::of(Requirements::default(), 60, None);
        assert_eq!(c.cpu_secs, 60);
        assert_eq!(c.mem_mb_secs, 0);
        assert_eq!(c.wall_secs, 60);
    }

    #[test]
    fn the_charge_is_what_was_reserved_not_what_was_burned() {
        // Thirty-two cores held for a minute and one of them used: the
        // share still says thirty-two core-minutes, because that is what
        // everybody else did not get.
        let reqs = Requirements {
            cpus: 32,
            mem_mb: 1024,
            disk_mb: 0,
        };
        let c = Charge::of(reqs, 60, Some(60));
        assert_eq!(c.cpu_secs, 60 * 32);
        assert_eq!(c.mem_mb_secs, 60 * 1024);
        assert_eq!(c.measured_cpu_secs, Some(60));
        assert_eq!(c.efficiency_pct(), Some(3));
    }

    #[test]
    fn efficiency_is_none_rather_than_invented() {
        let reqs = Requirements {
            cpus: 4,
            ..Default::default()
        };
        assert_eq!(Charge::of(reqs, 10, None).efficiency_pct(), None);
        assert_eq!(Charge::of(reqs, 0, Some(0)).efficiency_pct(), None);
    }

    #[test]
    fn a_job_that_never_started_is_charged_nothing() {
        let who = UserId([1u8; 32]);
        assert!(charge_for(who, Requirements::default(), None, Some(9), None).is_none());
        assert!(charge_for(who, Requirements::default(), Some(9), None, None).is_none());
    }

    #[test]
    fn an_instant_job_still_costs_a_second() {
        let who = UserId([1u8; 32]);
        let c = charge_for(who, Requirements::default(), Some(1_000), Some(1_000), None).unwrap();
        assert_eq!(c.charge.cpu_secs, 1);
        // Sub-second, too: a thousand of these are still a thousand jobs.
        let c = charge_for(who, Requirements::default(), Some(1_000), Some(1_500), None).unwrap();
        assert_eq!(c.charge.wall_secs, 1);
    }

    #[test]
    fn a_job_is_charged_to_the_window_it_finished_in() {
        let who = UserId([1u8; 32]);
        let hour = mesh_proto::USAGE_WINDOW_SECS * 1_000_000;
        // Starts in window 9, finishes in window 11.
        let c = charge_for(
            who,
            Requirements::default(),
            Some(9 * hour + 5),
            Some(11 * hour + 5),
            None,
        )
        .unwrap();
        assert_eq!(c.window, 11);
        assert_eq!(c.charge.wall_secs, 2 * mesh_proto::USAGE_WINDOW_SECS);
    }

    #[test]
    fn a_process_name_with_spaces_and_parens_still_parses() {
        // The classic /proc/stat bug. `comm` is arbitrary bytes in
        // parentheses, so anything that splits on whitespace from the left
        // reads the wrong columns for exactly the processes an operator is
        // most likely to be running.
        let line = "4242 (bash -c \"a) b\") S 1 4242 4242 0 -1 4194304 900 0 0 0 \
                    250 130 40 30 20 0 1 0 100 0 0";
        // utime 250, stime 130, cutime 40, cstime 30 → 450 ticks → 4s.
        assert_eq!(parse_stat_cpu(line), Some(4));
    }

    #[test]
    fn a_short_stat_line_reads_as_zero_rather_than_failing() {
        assert_eq!(parse_stat_cpu("1 (init) S 0"), Some(0));
        assert_eq!(parse_stat_cpu("nonsense with no paren"), None);
    }

    #[test]
    fn a_dead_process_measures_as_nothing_not_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(measure_cpu_secs_in(dir.path(), 999_999), None);
    }
}
