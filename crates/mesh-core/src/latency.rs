//! Dispatch latency: the number MASTERPLAN bet #3 is about (N6 §12).
//!
//! > *"dispatch overhead is the product"*
//!
//! Fifty-two metrics existed and this was not among them, which made the
//! central bet the one thing nobody could check. This is the meter.
//!
//! ## What is being measured
//!
//! **From a submission arriving to a node accepting it** — selector match,
//! feasibility, grants, ranking, change control, the cache lookup, and the
//! round trip to whichever node takes it, including every spill past a node
//! that answered `Busy`. It deliberately stops at *accepted*, not at
//! *finished*: what comes after is the workload's time, and folding it in
//! would measure the user's script rather than the scheduler.
//!
//! ## Why a fixed-bucket histogram and not a mean
//!
//! A mean hides the only thing worth seeing here. Dispatch is fast and
//! occasionally is not — a cold connection, a peer mid-gossip, a spill
//! across four busy nodes — and an average over a thousand fast dispatches
//! and ten slow ones reports the fast number. The buckets below are chosen
//! so that the *shape* of that tail is legible: sub-millisecond through ten
//! seconds, which spans "the same machine" to "something is wrong".
//!
//! Prometheus-shaped (`_bucket{le=…}`, `_sum`, `_count`) so a histogram
//! function reads it without a translation layer, and cheap enough to
//! record on every dispatch: one atomic add per bucket edge crossed.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bucket upper bounds for a *latency*, in seconds.
///
/// Nine of them, log-ish, and they stop at 10s on purpose: past ten
/// seconds the interesting question is no longer *how slow* but *what
/// broke*, and `+Inf` answers it.
pub const BUCKETS: [f64; 9] = [0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0];

/// Bucket upper bounds for *how long a piece of work took*, in seconds:
/// one second to four hours.
///
/// A separate ladder because the latency one above is the wrong
/// instrument for this and silently so. A job, a fan-out or a gang runs
/// for seconds to hours, so against [`BUCKETS`] every real observation
/// lands in `+Inf` and the histogram reports that everything is slower
/// than ten seconds — true, useless, and indistinguishable from a
/// histogram that is working. ENG-9 exists because fan-outs run past an
/// hour; the top edge here is past that on purpose, for the same reason
/// [`BUCKETS`] stops at ten seconds.
pub const WORK_BUCKETS: [f64; 10] = [
    1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0, 14400.0,
];

/// A cumulative histogram over a ladder of bucket edges, plus a sum and a
/// count. [`Histogram::default`] measures latencies ([`BUCKETS`]);
/// [`Histogram::for_work`] measures durations ([`WORK_BUCKETS`]).
///
/// Lock-free: dispatch is the hot path this exists to measure, and a mutex
/// on the meter would be overhead the meter then reported as latency.
#[derive(Debug)]
pub struct Histogram {
    /// Upper bounds, ascending. Static rather than owned: every ladder is
    /// a compile-time constant and a scrape renders the edges verbatim.
    edges: &'static [f64],
    /// Cumulative counts, one per edge. `counts[i]` is observations
    /// `<= edges[i]`, filled in at record time so a scrape is a read.
    counts: Vec<AtomicU64>,
    /// Total observations, which is also the `+Inf` bucket.
    count: AtomicU64,
    /// Sum of observations in microseconds, so the accumulator stays
    /// integral. Rendered as seconds.
    sum_micros: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Histogram {
        Histogram::new(&BUCKETS)
    }
}

impl Histogram {
    /// A histogram over `edges`, which must ascend.
    pub fn new(edges: &'static [f64]) -> Histogram {
        Histogram {
            edges,
            counts: (0..edges.len()).map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum_micros: AtomicU64::new(0),
        }
    }

    /// A histogram for how long a piece of work ran, rather than how long
    /// a call took ([`WORK_BUCKETS`]).
    pub fn for_work() -> Histogram {
        Histogram::new(&WORK_BUCKETS)
    }

    pub fn record(&self, elapsed: std::time::Duration) {
        let secs = elapsed.as_secs_f64();
        for (i, edge) in self.edges.iter().enumerate() {
            if secs <= *edge {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// `(upper bound, cumulative count)` per bucket, then the total count
    /// and the sum in seconds.
    pub fn snapshot(&self) -> (Vec<(f64, u64)>, u64, f64) {
        let buckets = self
            .edges
            .iter()
            .enumerate()
            .map(|(i, edge)| (*edge, self.counts[i].load(Ordering::Relaxed)))
            .collect();
        let count = self.count.load(Ordering::Relaxed);
        let sum = self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        (buckets, count, sum)
    }

    /// The bucket a given quantile falls in, as its upper bound in
    /// seconds. `None` when nothing has been recorded.
    ///
    /// **A bucket, not an interpolation.** Interpolating inside a bucket
    /// invents precision the histogram does not have — "p95 = 73ms" from
    /// edges at 50ms and 100ms is a number with no observation behind it.
    /// Reporting the edge is the honest form, and it is what a reader
    /// wants anyway: *under 100ms* is the answer, not a decimal.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let (buckets, count, _) = self.snapshot();
        if count == 0 {
            return None;
        }
        let want = (q.clamp(0.0, 1.0) * count as f64).ceil().max(1.0) as u64;
        buckets
            .iter()
            .find(|(_, n)| *n >= want)
            .map(|(edge, _)| *edge)
    }

    /// `(count, p50, p95)` for a one-line readout, seconds.
    pub fn summary(&self) -> (u64, Option<f64>, Option<f64>) {
        let (_, count, _) = self.snapshot();
        (count, self.quantile(0.5), self.quantile(0.95))
    }

    /// Render as Prometheus text. `name` is the metric's base name.
    pub fn render(&self, out: &mut String, name: &str, help: &str) {
        use std::fmt::Write as _;
        let (buckets, count, sum) = self.snapshot();
        let _ = writeln!(out, "# HELP {name}_seconds {help}");
        let _ = writeln!(out, "# TYPE {name}_seconds histogram");
        for (edge, n) in buckets {
            let _ = writeln!(out, "{name}_seconds_bucket{{le=\"{edge}\"}} {n}");
        }
        let _ = writeln!(out, "{name}_seconds_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(out, "{name}_seconds_sum {sum}");
        let _ = writeln!(out, "{name}_seconds_count {count}");
    }
}

/// Render several histograms as one series, told apart by `label`.
///
/// One `# HELP`/`# TYPE` pair for the family and one set of samples per
/// member, because Prometheus keeps the first declaration of a name and
/// silently drops every later one — rendering each member with its own
/// header would publish one of them and lose the rest.
pub fn render_family(
    out: &mut String,
    name: &str,
    help: &str,
    label: &str,
    members: &[(&str, &Histogram)],
) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "# HELP {name}_seconds {help}");
    let _ = writeln!(out, "# TYPE {name}_seconds histogram");
    for (value, hist) in members {
        let (buckets, count, sum) = hist.snapshot();
        for (edge, n) in buckets {
            let _ = writeln!(
                out,
                "{name}_seconds_bucket{{{label}=\"{value}\",le=\"{edge}\"}} {n}"
            );
        }
        let _ = writeln!(
            out,
            "{name}_seconds_bucket{{{label}=\"{value}\",le=\"+Inf\"}} {count}"
        );
        let _ = writeln!(out, "{name}_seconds_sum{{{label}=\"{value}\"}} {sum}");
        let _ = writeln!(out, "{name}_seconds_count{{{label}=\"{value}\"}} {count}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn buckets_are_cumulative_and_the_tail_lands_in_inf_only() {
        let h = Histogram::default();
        h.record(Duration::from_micros(500)); // 0.0005s
        h.record(Duration::from_millis(7)); // 0.007s
        h.record(Duration::from_secs(30)); // past every edge

        let (buckets, count, sum) = h.snapshot();
        assert_eq!(count, 3);
        // 0.0005 only.
        assert_eq!(buckets[0], (0.001, 1));
        // …still only the first, at 0.005.
        assert_eq!(buckets[1], (0.005, 1));
        // 0.007 joins it at 0.01.
        assert_eq!(buckets[2], (0.01, 2));
        // The 30-second outlier is in none of them, which is the whole
        // point: it shows up only in `+Inf`, where a histogram function
        // reads it as the tail rather than averaging it away.
        assert_eq!(buckets[BUCKETS.len() - 1], (10.0, 2));
        assert!((sum - 30.0075).abs() < 1e-6, "{sum}");
    }

    #[test]
    fn quantiles_report_a_bucket_edge_and_never_invent_one() {
        let h = Histogram::default();
        assert_eq!(h.quantile(0.5), None, "nothing recorded is not zero");

        // 95 fast, 5 slow: the median is the fast bucket and p95 is where
        // the tail actually is, which is the entire reason for not
        // reporting a mean.
        for _ in 0..95 {
            h.record(Duration::from_micros(800));
        }
        for _ in 0..5 {
            h.record(Duration::from_millis(400));
        }
        assert_eq!(h.quantile(0.5), Some(0.001));
        assert_eq!(h.quantile(0.95), Some(0.001));
        assert_eq!(h.quantile(0.99), Some(0.5));
        let (n, p50, p95) = h.summary();
        assert_eq!((n, p50, p95), (100, Some(0.001), Some(0.001)));
    }

    #[test]
    fn an_empty_histogram_renders_zeroes_rather_than_nothing() {
        // A scrape of a node that has dispatched nothing must still carry
        // the series: a metric that appears only once it is non-zero is a
        // metric no dashboard can be built on before the incident.
        let mut out = String::new();
        Histogram::default().render(&mut out, "meshware_dispatch", "help");
        assert!(out.contains("meshware_dispatch_seconds_count 0"));
        assert!(out.contains("le=\"+Inf\"} 0"));
        assert_eq!(out.matches("_bucket{").count(), BUCKETS.len() + 1);
    }

    /// A driver string is not a Prometheus label until it is escaped, and
    /// getting this wrong does not make one metric wrong — it makes the
    /// whole exposition unparseable, taking every other metric with it.
    #[test]
    fn a_hostile_model_string_cannot_break_the_exposition() {
        let escaped = crate::daemon::escape_label(r#"Weird "GPU" \ v2"#);
        assert_eq!(escaped, r#"Weird \"GPU\" \\ v2"#);
        assert_eq!(crate::daemon::escape_label("two\nlines"), r"two\nlines");
        // Ordinary names pass through untouched, which is the common case.
        assert_eq!(
            crate::daemon::escape_label("NVIDIA GeForce RTX 4090"),
            "NVIDIA GeForce RTX 4090"
        );
    }

    #[test]
    fn recording_is_monotonic_across_threads() {
        let h = std::sync::Arc::new(Histogram::default());
        let mut hands = Vec::new();
        for _ in 0..4 {
            let h = h.clone();
            hands.push(std::thread::spawn(move || {
                for _ in 0..250 {
                    h.record(Duration::from_micros(100));
                }
            }));
        }
        for t in hands {
            t.join().expect("join");
        }
        let (buckets, count, _) = h.snapshot();
        assert_eq!(count, 1000);
        assert_eq!(buckets[0].1, 1000);
    }
}
