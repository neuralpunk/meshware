//! Is this daemon actually working? (ENG-1, ENG-2)
//!
//! A node whose process is up is not necessarily a node that works. The
//! IPC server can stop accepting, the gossip loop can wedge behind a stuck
//! peer or a held lock, a panic can end a task — and the process carries
//! on, answering `/healthz` and feeding systemd's watchdog from a bare
//! timer, so nothing restarts it and nothing reports it.
//!
//! This module is where the loops that matter say so. Each one holds a
//! [`Pulse`]:
//!
//! - **dropping** it marks the loop *ended* — a return, a panic unwinding
//!   through it, or an abort all do that, with nothing to remember;
//! - a loop that runs on a timer also calls [`Pulse::beat`] each round,
//!   and a silence longer than its allowance marks it *stalled*.
//!
//! [`Vitals::check`] reads them. The daemon feeds the watchdog only while
//! the answer is healthy, `/healthz` fails while it is not, and a loop
//! that has *ended* ends the daemon with a failure — an ended loop never
//! comes back, and a restart is the only thing that brings it back.
//!
//! Atomics only on the beat path: a beat is a clock read and a store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// The shared half of one loop's pulse.
#[derive(Debug)]
struct Beat {
    name: &'static str,
    /// The longest silence that still counts as working; `None` for a loop
    /// that waits on events and may legitimately be quiet for ever.
    stall_after: Option<Duration>,
    /// Milliseconds since [`Vitals::epoch`] at the last beat.
    last_ms: AtomicU64,
    ended: AtomicBool,
    epoch: Instant,
}

impl Beat {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Held by a loop for as long as it runs. See the module docs.
#[derive(Debug)]
pub struct Pulse(Arc<Beat>);

impl Pulse {
    /// The loop made progress.
    pub fn beat(&self) {
        self.0.last_ms.store(self.0.now_ms(), Ordering::Relaxed);
    }

    /// A pulse nobody watches, for a caller with no [`Vitals`] to report to
    /// (a test driving a loop directly).
    pub fn detached(name: &'static str) -> Self {
        Pulse(Arc::new(Beat {
            name,
            stall_after: None,
            last_ms: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            epoch: Instant::now(),
        }))
    }

    /// The watching half, for a loop created before its [`Vitals`] was
    /// (the transport's accept loop): hand it to [`Vitals::adopt`].
    pub fn monitor(&self) -> Monitor {
        Monitor(self.0.clone())
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        self.0.ended.store(true, Ordering::Relaxed);
    }
}

/// The watching half of a [`Pulse`].
#[derive(Debug, Clone)]
pub struct Monitor(Arc<Beat>);

/// What is wrong with one loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The loop is gone and will not come back.
    Ended(&'static str),
    /// The loop has not beaten for this long.
    Stalled(&'static str, Duration),
}

impl Problem {
    pub fn is_ended(&self) -> bool {
        matches!(self, Problem::Ended(_))
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::Ended(name) => write!(f, "the {name} loop has ended"),
            Problem::Stalled(name, quiet) => write!(
                f,
                "the {name} loop has made no progress for {}s",
                quiet.as_secs()
            ),
        }
    }
}

/// Every critical loop of one node.
#[derive(Debug)]
pub struct Vitals {
    epoch: Instant,
    loops: Mutex<Vec<Monitor>>,
    /// Set once the node is stopping on purpose: loops ending then is the
    /// point, not a failure.
    stopping: AtomicBool,
}

impl Default for Vitals {
    fn default() -> Self {
        Self::new()
    }
}

impl Vitals {
    pub fn new() -> Self {
        Vitals {
            epoch: Instant::now(),
            loops: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
        }
    }

    /// Start watching a loop. `stall_after` is `None` for a loop that waits
    /// on events; see [`Beat::stall_after`].
    pub fn register(&self, name: &'static str, stall_after: Option<Duration>) -> Pulse {
        let beat = Arc::new(Beat {
            name,
            stall_after,
            last_ms: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            epoch: self.epoch,
        });
        let pulse = Pulse(beat);
        pulse.beat();
        self.adopt(pulse.monitor());
        pulse
    }

    /// Watch a pulse created elsewhere ([`Pulse::monitor`]).
    pub fn adopt(&self, monitor: Monitor) {
        self.loops.lock().push(monitor);
    }

    /// Say that the node is stopping; from now on nothing is a problem.
    pub fn stopping(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    /// What is wrong right now. Empty means healthy, and so does a node
    /// that is stopping.
    ///
    /// A beat stamped against a different epoch (a pulse adopted from a
    /// loop created earlier) is measured on its own clock, which is why
    /// the silence is computed per beat rather than from `self.epoch`.
    pub fn check(&self) -> Vec<Problem> {
        if self.is_stopping() {
            return Vec::new();
        }
        let loops = self.loops.lock();
        let mut problems = Vec::new();
        for Monitor(beat) in loops.iter() {
            if beat.ended.load(Ordering::Relaxed) {
                problems.push(Problem::Ended(beat.name));
                continue;
            }
            if let Some(allowed) = beat.stall_after {
                let quiet_ms = beat
                    .now_ms()
                    .saturating_sub(beat.last_ms.load(Ordering::Relaxed));
                let quiet = Duration::from_millis(quiet_ms);
                if quiet > allowed {
                    problems.push(Problem::Stalled(beat.name, quiet));
                }
            }
        }
        problems
    }

    /// The loops being watched, by name — for `doctor` and the tests.
    pub fn names(&self) -> Vec<&'static str> {
        self.loops.lock().iter().map(|Monitor(b)| b.name).collect()
    }
}

/// Anything replay or repair did at startup, counted so the service
/// manager's start deadline can be extended **only while it moves**
/// (ENG-1).
///
/// Process-wide on purpose: it is read by the one thing a process has one
/// of — its conversation with systemd — and bumped from places (the store's
/// repair callback, the replay loop) that have no daemon to hand it to.
static STARTUP_PROGRESS: AtomicU64 = AtomicU64::new(0);

/// Note that startup work advanced.
pub fn startup_progress(units: u64) {
    STARTUP_PROGRESS.fetch_add(units, Ordering::Relaxed);
}

/// The startup progress counter's current value.
pub fn startup_progress_value() -> u64 {
    STARTUP_PROGRESS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_pulse_is_an_ended_loop() {
        let vitals = Vitals::new();
        let pulse = vitals.register("ipc", None);
        assert!(vitals.check().is_empty());
        drop(pulse);
        assert_eq!(vitals.check(), vec![Problem::Ended("ipc")]);
    }

    #[test]
    fn a_panic_ends_the_loop_it_unwinds_through() {
        let vitals = Arc::new(Vitals::new());
        let pulse = vitals.register("gossip", None);
        let joined = std::thread::spawn(move || {
            let _held = pulse;
            panic!("boom");
        })
        .join();
        assert!(joined.is_err());
        assert!(vitals.check().iter().any(Problem::is_ended));
    }

    #[test]
    fn silence_past_the_allowance_is_a_stall_and_a_beat_clears_it() {
        let vitals = Vitals::new();
        let pulse = vitals.register("gossip", Some(Duration::from_millis(20)));
        std::thread::sleep(Duration::from_millis(40));
        let problems = vitals.check();
        assert!(
            matches!(problems.as_slice(), [Problem::Stalled("gossip", _)]),
            "{problems:?}"
        );
        pulse.beat();
        assert!(vitals.check().is_empty());
    }

    #[test]
    fn an_event_loop_may_be_quiet_for_ever() {
        let vitals = Vitals::new();
        let _pulse = vitals.register("dispatch", None);
        std::thread::sleep(Duration::from_millis(20));
        assert!(vitals.check().is_empty());
    }

    #[test]
    fn nothing_is_a_problem_once_the_node_is_stopping() {
        let vitals = Vitals::new();
        let pulse = vitals.register("ipc", None);
        vitals.stopping();
        drop(pulse);
        assert!(vitals.check().is_empty());
    }

    #[test]
    fn an_adopted_pulse_is_watched_like_a_registered_one() {
        let vitals = Vitals::new();
        let pulse = Pulse::detached("transport");
        vitals.adopt(pulse.monitor());
        assert!(vitals.check().is_empty());
        drop(pulse);
        assert_eq!(vitals.check(), vec![Problem::Ended("transport")]);
    }
}
