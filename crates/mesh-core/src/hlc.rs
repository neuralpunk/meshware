//! Hybrid logical clock (MASTERPLAN §4.4): wall-clock microseconds plus a
//! logical counter. Monotonic across reads, merges remote observations, and
//! tolerates modest wall-clock skew between nodes.

use parking_lot::Mutex;

use mesh_proto::Hlc;

/// How far ahead of this machine's clock a timestamp may be and still be
/// believed (SEC-5). The same five minutes a signed request is allowed
/// (`daemon::AUTH_SKEW`): a node further out than that already cannot have
/// its requests verified, and its ops wait until real time catches up.
pub const MAX_AHEAD_MICROS: u64 = 5 * 60 * 1_000_000;

#[derive(Debug, Default)]
pub struct HlcClock {
    inner: Mutex<Hlc>,
}

impl HlcClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Produce the next timestamp, strictly greater than any previously
    /// produced or observed.
    pub fn now(&self) -> Hlc {
        let wall = mesh_proto::now_micros();
        let mut last = self.inner.lock();
        if wall > last.wall_micros {
            *last = Hlc {
                wall_micros: wall,
                logical: 0,
            };
        } else if last.logical == u32::MAX {
            // Saturated (SEC-5): step the wall part instead of wrapping,
            // which would sort this timestamp before the last one.
            last.wall_micros = last.wall_micros.saturating_add(1);
            last.logical = 0;
        } else {
            last.logical += 1;
        }
        *last
    }

    /// Fold a remote timestamp into the clock so our next `now()` sorts
    /// after everything we've seen.
    ///
    /// **Clamped** to [`MAX_AHEAD_MICROS`] past this machine's clock
    /// (SEC-5). One far-future timestamp — a VM restored from a snapshot, a
    /// dead CMOS battery — used to drag this clock forward for good, and
    /// with it every op this node authored afterwards: expiring grants
    /// lapsed early, history retired early, and that node's writes won
    /// every last-writer-wins race for as long as the mesh existed.
    pub fn observe(&self, remote: Hlc) {
        let cap = mesh_proto::now_micros().saturating_add(MAX_AHEAD_MICROS);
        let remote = if remote.wall_micros > cap {
            Hlc {
                wall_micros: cap,
                logical: 0,
            }
        } else {
            remote
        };
        let mut last = self.inner.lock();
        if remote > *last {
            *last = remote;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic() {
        let clock = HlcClock::new();
        let mut prev = clock.now();
        for _ in 0..1000 {
            let next = clock.now();
            assert!(next > prev);
            prev = next;
        }
    }

    #[test]
    fn observe_advances() {
        let clock = HlcClock::new();
        let ahead = Hlc {
            wall_micros: mesh_proto::now_micros() + 60_000_000,
            logical: 7,
        };
        clock.observe(ahead);
        assert!(clock.now() > ahead);
    }

    /// SEC-5: a timestamp from far in the future moves the clock only as
    /// far as the allowance, so the next op this node authors is not dated
    /// in the year 584,000.
    #[test]
    fn observe_is_clamped() {
        let clock = HlcClock::new();
        clock.observe(Hlc {
            wall_micros: u64::MAX - 10,
            logical: 0,
        });
        let next = clock.now();
        assert!(next.wall_micros <= mesh_proto::now_micros() + MAX_AHEAD_MICROS);
    }

    /// SEC-5: the logical counter saturates into the wall part rather than
    /// wrapping back below a timestamp already handed out.
    #[test]
    fn the_logical_counter_never_wraps() {
        let clock = HlcClock::new();
        let far = mesh_proto::now_micros() + 60_000_000;
        *clock.inner.lock() = Hlc {
            wall_micros: far,
            logical: u32::MAX - 1,
        };
        let a = clock.now();
        let b = clock.now();
        let c = clock.now();
        assert!(a < b && b < c, "{a:?} {b:?} {c:?}");
        assert_eq!(b.logical, 0);
        assert_eq!(b.wall_micros, far + 1);
    }
}
