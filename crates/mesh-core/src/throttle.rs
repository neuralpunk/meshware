//! What a caller with no role can make this node do (SEC-19).
//!
//! Two requests are open to anybody holding a key, because the people they
//! are for are not users yet: asking to enrol, and signing in through an
//! identity provider. Asking to enrol authors a replicated op and an audit
//! row; starting a sign-in makes this node call the provider. Both were
//! limited per key only — by the op-rate guard, which keys on the entity
//! the op writes — and a fresh key is free to make, so the limit was no
//! limit at all, and the HTTP API put both within reach of the network.
//!
//! So each is limited twice: per source (an address on the HTTP API, one
//! budget for this machine's own socket), and overall, whatever the
//! source. A person enrolling does it once; a room of people signing in at
//! once is a burst, and the buckets are sized to let one through.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A token bucket per key, with a bound on how many keys it remembers.
#[derive(Debug)]
pub struct Throttle {
    burst: f64,
    per_sec: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
    /// Refusals since this process started (ENG-21).
    ///
    /// Here rather than summed over `buckets` on a scrape, because the map
    /// forgets a refilled bucket and a scrape that summed it would report
    /// a *falling* counter — which Prometheus reads as a reset and turns
    /// into a spike of refusals that never happened. A limiter nobody can
    /// graph is a limiter whose first sign of biting is a support ticket.
    refused: AtomicU64,
}

/// Keys remembered before full buckets are forgotten. A bucket that has
/// refilled says nothing a new one would not.
const MAX_KEYS: usize = 4096;

impl Throttle {
    pub fn new(burst: u32, per_minute: u32) -> Throttle {
        Throttle {
            burst: f64::from(burst.max(1)),
            per_sec: f64::from(per_minute) / 60.0,
            buckets: Mutex::new(HashMap::new()),
            refused: AtomicU64::new(0),
        }
    }

    /// How many callers this limiter has turned away since the process
    /// started.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    fn refuse(&self) -> bool {
        self.refused.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// May `key` go now? Spends a token if so.
    pub fn admit(&self, key: &str) -> bool {
        self.admit_at(key, Instant::now())
    }

    fn admit_at(&self, key: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock();
        if buckets.len() >= MAX_KEYS && !buckets.contains_key(key) {
            let (burst, per_sec) = (self.burst, self.per_sec);
            buckets.retain(|_, (tokens, at)| {
                *tokens + now.saturating_duration_since(*at).as_secs_f64() * per_sec < burst
            });
            // Still full of live buckets: somebody is making keys faster
            // than they refill, and a new one is refused rather than
            // letting the map grow without end.
            if buckets.len() >= MAX_KEYS {
                return self.refuse();
            }
        }
        let (tokens, at) = buckets.entry(key.to_string()).or_insert((self.burst, now));
        *tokens = (*tokens + now.saturating_duration_since(*at).as_secs_f64() * self.per_sec)
            .min(self.burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            self.refuse()
        }
    }
}

/// Where a request came from, as far as limiting it goes.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// This machine: the local socket, or Director in-process.
    Local,
    /// A client of the HTTP API.
    Remote(IpAddr),
}

impl Source {
    /// The budget this source draws on. An IPv6 client is limited by its
    /// /64, which is what one machine is usually handed.
    pub fn key(&self) -> String {
        match self {
            Source::Local => "local".into(),
            Source::Remote(IpAddr::V4(ip)) => ip.to_string(),
            Source::Remote(IpAddr::V6(ip)) => {
                let s = ip.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        }
    }
}

/// The two anonymous doors and their budgets.
#[derive(Debug)]
pub struct AnonLimits {
    pub enrol_source: Throttle,
    pub enrol_all: Throttle,
    pub login_source: Throttle,
    pub login_all: Throttle,
    /// Sign-ins this node started, by device code: a poll has to name one,
    /// or it would be a way to make this node call the provider with codes
    /// it never issued.
    logins: Mutex<HashMap<String, PendingLogin>>,
}

#[derive(Debug)]
struct PendingLogin {
    user: mesh_proto::UserId,
    expires: Instant,
    polled: Option<Instant>,
}

/// How every refusal from these limits begins, so the HTTP API can answer
/// it with `429` rather than a generic `400`.
pub const TOO_MANY: &str = "too many ";

/// Enrolment requests waiting for an admin, past which a new one is
/// refused. An admin's queue with this many in it is not a queue anybody
/// is reading.
pub const MAX_PENDING_ENROLMENTS: usize = 500;

/// Sign-ins remembered at once.
const MAX_LOGINS: usize = 1024;

/// The closest two polls of one sign-in may be.
const MIN_POLL_GAP: Duration = Duration::from_secs(1);

impl Default for AnonLimits {
    fn default() -> Self {
        AnonLimits {
            // One machine asks once; ten covers retries and a shared
            // address, and it comes back at one a minute.
            enrol_source: Throttle::new(10, 1),
            enrol_all: Throttle::new(100, 30),
            login_source: Throttle::new(20, 10),
            login_all: Throttle::new(100, 60),
            logins: Mutex::new(HashMap::new()),
        }
    }
}

impl AnonLimits {
    /// Remember a sign-in this node started, until the provider says its
    /// code expires.
    pub fn login_started(&self, device_code: &str, user: mesh_proto::UserId, expires_in: u64) {
        let now = Instant::now();
        let mut logins = self.logins.lock();
        logins.retain(|_, l| l.expires > now);
        if logins.len() >= MAX_LOGINS {
            // Oldest to expire goes first; the limits above are what keep
            // this from being reached by anyone but a crowd.
            if let Some(code) = logins
                .iter()
                .min_by_key(|(_, l)| l.expires)
                .map(|(c, _)| c.clone())
            {
                logins.remove(&code);
            }
        }
        logins.insert(
            device_code.to_string(),
            PendingLogin {
                user,
                expires: now + Duration::from_secs(expires_in.clamp(1, 3600)),
                polled: None,
            },
        );
    }

    /// May `user` poll the sign-in `device_code` now?
    pub fn may_poll(&self, device_code: &str, user: &mesh_proto::UserId) -> Result<(), String> {
        let now = Instant::now();
        let mut logins = self.logins.lock();
        let Some(login) = logins
            .get_mut(device_code)
            .filter(|l| l.user == *user && l.expires > now)
        else {
            return Err(
                "no sign-in with that code was started on this node by this key — start one \
                 with OidcLoginBegin"
                    .into(),
            );
        };
        if login
            .polled
            .is_some_and(|at| now.saturating_duration_since(at) < MIN_POLL_GAP)
        {
            return Err("polling too fast — wait the interval the provider gave".into());
        }
        login.polled = Some(now);
        Ok(())
    }

    /// The sign-in is over, one way or the other.
    pub fn login_finished(&self, device_code: &str) {
        self.logins.lock().remove(device_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_goes_through_and_then_the_rate_holds() {
        let t = Throttle::new(3, 60);
        let now = Instant::now();
        assert!((0..3).all(|_| t.admit_at("a", now)));
        assert!(!t.admit_at("a", now));
        // Another key has its own bucket.
        assert!(t.admit_at("b", now));
        // One a second refills one token.
        assert!(t.admit_at("a", now + Duration::from_millis(1100)));
        assert!(!t.admit_at("a", now + Duration::from_millis(1200)));
    }

    #[test]
    fn fresh_keys_cannot_grow_the_map_without_end() {
        let t = Throttle::new(1, 1);
        let now = Instant::now();
        for i in 0..MAX_KEYS {
            assert!(t.admit_at(&i.to_string(), now));
        }
        // Every bucket is spent, so none can be forgotten.
        assert!(!t.admit_at("one-more", now));
        // Once they have refilled, they can.
        assert!(t.admit_at("one-more", now + Duration::from_secs(120)));
    }

    /// The limiter counts what it turned away, and keeps counting across
    /// the point where it forgets refilled buckets (ENG-21).
    ///
    /// Without a counter here, the only sign that an anonymous door was
    /// being hammered was that nobody could get through it — and the
    /// bucket map is deliberately forgetful, so nothing summed from it
    /// could be scraped as a counter.
    #[test]
    fn refusals_are_counted_and_the_count_does_not_fall() {
        let t = Throttle::new(2, 0); // two, and no refill
        let now = Instant::now();
        assert!(t.admit_at("one", now));
        assert!(t.admit_at("one", now));
        assert_eq!(t.refused(), 0, "nothing has been refused yet");

        assert!(!t.admit_at("one", now));
        assert!(!t.admit_at("one", now));
        assert_eq!(t.refused(), 2);

        // A different key is a different bucket and is admitted; the count
        // of what was refused stands.
        assert!(t.admit_at("two", now));
        assert_eq!(t.refused(), 2);
    }

    #[test]
    fn a_v6_client_is_limited_by_its_64() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(Source::Remote(a).key(), Source::Remote(b).key());
        assert_ne!(Source::Remote(a).key(), Source::Remote(c).key());
    }

    #[test]
    fn a_poll_has_to_name_a_sign_in_this_node_started_for_this_key() {
        let limits = AnonLimits::default();
        let me = mesh_proto::UserId([1; 32]);
        let other = mesh_proto::UserId([2; 32]);
        assert!(limits.may_poll("code", &me).is_err());
        limits.login_started("code", me, 600);
        assert!(limits.may_poll("code", &other).is_err());
        assert!(limits.may_poll("code", &me).is_ok());
        let err = limits.may_poll("code", &me).unwrap_err();
        assert!(err.contains("too fast"), "{err}");
        limits.login_finished("code");
        assert!(limits.may_poll("code", &me).is_err());
    }
}
