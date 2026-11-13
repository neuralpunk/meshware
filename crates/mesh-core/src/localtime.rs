//! What time it is *here* — the local wall clock, and when it next moves.
//!
//! Quiet hours are the first thing in this engine that has to reason about
//! a **human** clock. Everywhere else, time is a monotonic instant or a
//! microsecond epoch stamp, and both are deliberately timezone-free: an HLC
//! that respected daylight saving would be a bug. "Thirty percent of the
//! slots until eleven at night" is the opposite — it is a statement about
//! the household the machine sits in, and eleven at night is eleven at
//! night in Chicago whatever UTC thinks.
//!
//! ## Why this is not a dependency
//!
//! `chrono` is already in the workspace graph, but only under Director, via
//! `mesh-archive → datafusion → arrow`. `meshnode` is a single binary people
//! copy onto machines by hand — its size is a feature, not a statistic
//! (`Cargo.toml`, release profile) — and pulling a general date-time library
//! into the node to answer two questions is the wrong trade twice over,
//! because the second question is one a date-time library answers badly:
//!
//! - **What is the UTC offset here, now?** Every library answers this.
//! - **When does that offset next change?** Almost none expose it, and it
//!   is the question that matters here: a quiet window that ends at 23:00
//!   has to schedule its own wake-up, and a wake-up computed from a
//!   pre-transition offset lands an hour wrong twice a year.
//!
//! The zoneinfo file answers both directly — it *is* a transition table —
//! so this module reads it and stops there. It is not a calendar library
//! and must not grow into one.
//!
//! ## What it reads
//!
//! `TZ`, then `/etc/localtime`, in that order, per the usual Unix rule:
//!
//! - `TZ=UTC` / `TZ=` / unset-and-no-zoneinfo → UTC, offset zero.
//! - `TZ=:/path` or `TZ=Region/City` → that zoneinfo file.
//! - `TZ=<abbr><offset>` (the POSIX form, e.g. `EST5`) → a fixed offset.
//!   The DST half of the POSIX form (`EST5EDT,M3.2.0,M11.1.0`) is
//!   deliberately **not** implemented: parsing a rule string is a second
//!   implementation of the thing the transition table already contains, and
//!   every system that has a `TZ` rule also has the file it was derived
//!   from. The base offset is used and the rule ignored, which is a
//!   documented hour of error in an exotic configuration rather than a
//!   silent one everywhere.
//! - Anything unreadable or malformed → UTC, with a warning. A node that
//!   cannot tell the time locally must not refuse to run work; it falls
//!   back to a clock everybody agrees on and says so.
//!
//! Zoneinfo is read **once at startup** and cached. The file changes when
//! the tzdata package updates, which is not a thing to poll a filesystem
//! for; a node that has been up across a tzdata release restarts eventually
//! like every other daemon.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing::{debug, warn};

/// Seconds in a day. Named because a `86_400` in an arithmetic expression
/// about wall-clock time is exactly where an off-by-an-hour hides.
pub const DAY: i64 = 86_400;

/// A resolved local clock: the transition table for this machine's zone.
///
/// Cheap to consult and immutable once built, which is what lets the
/// scheduler ask "what time is it here" on every admission decision without
/// touching the filesystem.
#[derive(Debug, Clone, Default)]
pub struct LocalZone {
    /// Transition instants (UTC seconds), strictly ascending.
    transitions: Vec<i64>,
    /// `offsets[i]` is the UTC offset in effect **from** `transitions[i]`
    /// until `transitions[i + 1]`. Always the same length as
    /// `transitions`.
    offsets: Vec<i32>,
    /// The offset in effect before the first transition — and the whole
    /// answer for a fixed-offset zone, which has no transitions at all.
    first: i32,
    /// Where this came from, for `meshnode status` and for the operator
    /// staring at a window that fired an hour late.
    pub name: String,
}

impl LocalZone {
    /// A zone with one fixed offset and no transitions.
    pub fn fixed(offset_secs: i32, name: impl Into<String>) -> LocalZone {
        LocalZone {
            transitions: Vec::new(),
            offsets: Vec::new(),
            first: offset_secs,
            name: name.into(),
        }
    }

    /// UTC, for tests and for every fallback in this module.
    pub fn utc() -> LocalZone {
        LocalZone::fixed(0, "UTC")
    }

    /// A hand-built transition table.
    ///
    /// Only tests build one of these: the two days a year a zone changes
    /// offset are the two days quiet hours is hardest to get right, and
    /// waiting for one of them to come round is not a test strategy.
    #[cfg(test)]
    pub(crate) fn from_transitions(
        transitions: Vec<i64>,
        offsets: Vec<i32>,
        first: i32,
    ) -> LocalZone {
        assert_eq!(transitions.len(), offsets.len());
        LocalZone {
            transitions,
            offsets,
            first,
            name: "test".to_string(),
        }
    }

    /// The UTC offset in effect at `unix` seconds.
    pub fn offset_at(&self, unix: i64) -> i32 {
        match self.transitions.binary_search(&unix) {
            // Exactly on a transition: the new offset is already in effect.
            Ok(i) => self.offsets[i],
            Err(0) => self.first,
            Err(i) => self.offsets[i - 1],
        }
    }

    /// Local wall-clock seconds since the local midnight that precedes
    /// `unix`, in `0..DAY`.
    ///
    /// `rem_euclid` rather than `%`: local seconds go negative west of
    /// Greenwich before 1970 and, far more relevantly, `i64 % DAY` is
    /// negative for any negative operand, which would put 23:30 at
    /// `-1800` and quietly invert every window comparison.
    pub fn minute_of_day(&self, unix: i64) -> u32 {
        let local = unix + i64::from(self.offset_at(unix));
        let secs = local.rem_euclid(DAY);
        u32::try_from(secs / 60).unwrap_or(0)
    }

    /// The next instant strictly after `unix` at which the offset changes,
    /// if the table has one.
    ///
    /// This is the half a date-time library will not tell you, and the
    /// reason quiet hours can schedule a single exact wake-up instead of
    /// polling the clock every minute forever.
    pub fn next_transition_after(&self, unix: i64) -> Option<i64> {
        let idx = self.transitions.partition_point(|t| *t <= unix);
        self.transitions.get(idx).copied()
    }

    /// This machine's zone, read once and cached.
    pub fn local() -> &'static LocalZone {
        static ZONE: OnceLock<LocalZone> = OnceLock::new();
        ZONE.get_or_init(|| {
            let zone = detect();
            debug!(zone = %zone.name, "local clock resolved");
            zone
        })
    }
}

/// Resolve this machine's zone from the environment, then the filesystem.
fn detect() -> LocalZone {
    match std::env::var("TZ") {
        Ok(tz) => {
            let tz = tz.trim();
            if tz.is_empty() || tz.eq_ignore_ascii_case("UTC") || tz.eq_ignore_ascii_case("UCT") {
                return LocalZone::utc();
            }
            // `TZ=:Europe/Paris` and `TZ=Europe/Paris` mean the same file;
            // the colon is the "this is a path, not a rule" marker and is
            // optional on every system that matters.
            let spec = tz.strip_prefix(':').unwrap_or(tz);
            if let Some(zone) = zone_from_spec(spec) {
                return zone;
            }
            if let Some(zone) = posix_fixed(tz) {
                return zone;
            }
            warn!("TZ={tz} is neither a zoneinfo name nor a fixed offset — using UTC");
            LocalZone::utc()
        }
        Err(_) => match read_zone(Path::new("/etc/localtime")) {
            Ok(mut zone) => {
                zone.name = system_zone_name().unwrap_or_else(|| "local".to_string());
                zone
            }
            Err(e) => {
                debug!("no readable /etc/localtime ({e}) — using UTC");
                LocalZone::utc()
            }
        },
    }
}

/// A zoneinfo name or an absolute path, if it resolves to a readable file.
fn zone_from_spec(spec: &str) -> Option<LocalZone> {
    let path = if spec.starts_with('/') {
        PathBuf::from(spec)
    } else {
        // Reject traversal before touching the filesystem: `TZ` is
        // ordinary process environment and this reads a file from it.
        if spec.contains("..") || spec.is_empty() {
            return None;
        }
        zoneinfo_root()?.join(spec)
    };
    match read_zone(&path) {
        Ok(mut zone) => {
            zone.name = spec.to_string();
            Some(zone)
        }
        Err(e) => {
            debug!(path = %path.display(), "zoneinfo unreadable: {e}");
            None
        }
    }
}

fn zoneinfo_root() -> Option<PathBuf> {
    for root in ["/usr/share/zoneinfo", "/share/zoneinfo", "/etc/zoneinfo"] {
        let p = Path::new(root);
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
    }
    None
}

/// The zone's *name*, from the symlink `/etc/localtime` almost always is.
/// Display only — the offsets come from the file's own table either way.
fn system_zone_name() -> Option<String> {
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let s = target.to_string_lossy();
        if let Some((_, name)) = s.split_once("zoneinfo/") {
            return Some(name.trim_start_matches('/').to_string());
        }
    }
    // Debian/Ubuntu keep the name here even when /etc/localtime is a copy.
    std::fs::read_to_string("/etc/timezone")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The POSIX `TZ` form's *base* offset: `EST5`, `<-04>4`, `CET-1`.
///
/// POSIX signs are backwards from everything else — `EST5` is five hours
/// **west**, i.e. `-05:00` — which is precisely the kind of detail that is
/// wrong in half the code that reimplements it, so it is stated here rather
/// than implied by the arithmetic.
fn posix_fixed(tz: &str) -> Option<LocalZone> {
    let rest = if let Some(after) = tz.strip_prefix('<') {
        // `<-04>4` — an angle-bracketed abbreviation, then the offset.
        after.split_once('>').map(|(_, r)| r)?
    } else {
        let digits = tz.find(|c: char| c == '+' || c == '-' || c.is_ascii_digit())?;
        // An abbreviation is at least three characters; anything shorter
        // means this is not a POSIX spec.
        if digits < 3 {
            return None;
        }
        &tz[digits..]
    };
    // Stop at the DST abbreviation or the rule: only the base offset is
    // honoured (see the module docs).
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == ':' || c == '+' || c == '-'))
        .unwrap_or(rest.len());
    let (spec, ignored) = rest.split_at(end);
    let (sign, digits) = match spec.strip_prefix('-') {
        Some(d) => (1i32, d), // POSIX: west is positive, so `-` is *east*
        None => (-1i32, spec.strip_prefix('+').unwrap_or(spec)),
    };
    let mut parts = digits.split(':');
    let hours: i32 = parts.next().filter(|s| !s.is_empty())?.parse().ok()?;
    let minutes: i32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let seconds: i32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    if hours > 24 || minutes > 59 || seconds > 59 {
        return None;
    }
    if !ignored.is_empty() {
        warn!(
            "TZ={tz} carries a daylight-saving rule; meshware honours the base offset only \
             — install tzdata and unset TZ for exact transitions"
        );
    }
    Some(LocalZone::fixed(
        sign * (hours * 3600 + minutes * 60 + seconds),
        tz.to_string(),
    ))
}

// ---------------------------------------------------------------------------
// TZif (RFC 8536)
// ---------------------------------------------------------------------------

/// Largest zoneinfo file this will read. Real ones are a few kilobytes;
/// the cap is here because this parses a file whose path can come from the
/// environment, and a bound is cheaper than trusting one.
const MAX_TZIF: u64 = 1024 * 1024;

/// Parse a TZif v1/v2/v3 file into a transition table.
///
/// Version 2+ files carry the whole table **twice**: a 32-bit block for
/// ancient readers, then a 64-bit block that supersedes it. The second
/// block is the one to use — a 32-bit table runs out in 2038, which is
/// inside the working life of a machine somebody is installing today — so
/// this parses the first block only far enough to know where the second
/// one starts, then parses that.
fn read_zone(path: &Path) -> std::io::Result<LocalZone> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_TZIF {
        return Err(std::io::Error::other("zoneinfo file implausibly large"));
    }
    let bytes = std::fs::read(path)?;
    parse_tzif(&bytes).ok_or_else(|| std::io::Error::other("not a valid TZif file"))
}

/// Header counts, in the order RFC 8536 §3.1 lays them out.
#[derive(Debug, Clone, Copy)]
struct Counts {
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn be32(b: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?))
}

fn be64(b: &[u8]) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(..8)?.try_into().ok()?))
}

/// Read the 44-byte header at `at`, returning the counts and the version.
fn header(bytes: &[u8], at: usize) -> Option<(Counts, u8)> {
    let h = bytes.get(at..at + 44)?;
    if &h[..4] != b"TZif" {
        return None;
    }
    let version = h[4];
    let n = |i: usize| -> Option<usize> { usize::try_from(be32(&h[20 + i * 4..])?).ok() };
    Some((
        Counts {
            isutcnt: n(0)?,
            isstdcnt: n(1)?,
            leapcnt: n(2)?,
            timecnt: n(3)?,
            typecnt: n(4)?,
            charcnt: n(5)?,
        },
        version,
    ))
}

/// Byte length of the data block that follows a header, for a given
/// transition-time width (4 for the v1 block, 8 for the v2+ one).
fn block_len(c: &Counts, time_size: usize) -> usize {
    c.timecnt * time_size          // transition times
        + c.timecnt                // per-transition type index
        + c.typecnt * 6            // ttinfo: utoff(4) isdst(1) desigidx(1)
        + c.charcnt                // designation strings
        + c.leapcnt * (time_size + 4)
        + c.isstdcnt
        + c.isutcnt
}

fn parse_tzif(bytes: &[u8]) -> Option<LocalZone> {
    let (v1, version) = header(bytes, 0)?;
    if version == 0 {
        return data_block(bytes, 44, &v1, 4);
    }
    // v2+: skip the legacy block and its header, then parse the 64-bit one.
    let second = 44 + block_len(&v1, 4);
    let (v2, _) = header(bytes, second)?;
    data_block(bytes, second + 44, &v2, 8)
}

/// Parse one data block into transitions and offsets.
fn data_block(bytes: &[u8], at: usize, c: &Counts, time_size: usize) -> Option<LocalZone> {
    let end = at.checked_add(block_len(c, time_size))?;
    if end > bytes.len() || c.typecnt == 0 {
        return None;
    }
    let times = bytes.get(at..at + c.timecnt * time_size)?;
    let indices = bytes.get(at + c.timecnt * time_size..at + c.timecnt * (time_size + 1))?;
    let ttinfo_at = at + c.timecnt * (time_size + 1);

    // The `utoff` of each local time type. `isdst` and the designation are
    // read past but not kept: nothing here renders an abbreviation, and a
    // window that runs "while the household is awake" does not care whether
    // the clock calls itself CST or CDT — only what it reads.
    let mut utoff = Vec::with_capacity(c.typecnt);
    for i in 0..c.typecnt {
        let rec = bytes.get(ttinfo_at + i * 6..ttinfo_at + i * 6 + 6)?;
        utoff.push(be32(rec)? as i32);
    }

    let mut transitions = Vec::with_capacity(c.timecnt);
    let mut offsets = Vec::with_capacity(c.timecnt);
    for i in 0..c.timecnt {
        let raw = &times[i * time_size..];
        let t = if time_size == 8 {
            be64(raw)?
        } else {
            i64::from(be32(raw)? as i32)
        };
        // RFC 8536 requires ascending, distinct transition times. A file
        // that violates it is corrupt; binary search over an unsorted table
        // silently returns the wrong offset, so refuse instead.
        if let Some(prev) = transitions.last()
            && t <= *prev
        {
            return None;
        }
        let ty = *indices.get(i)? as usize;
        transitions.push(t);
        offsets.push(*utoff.get(ty)?);
    }

    // Before the first transition, the rule is "the first non-DST type, or
    // failing that the first type" (RFC 8536 §3.2). Reading `isdst` for
    // this one case is cheaper than being wrong about pre-1901 timestamps
    // that nothing here will ever ask about — so take the simple half of
    // the rule and use type 0.
    let first = *utoff.first()?;
    Some(LocalZone {
        transitions,
        offsets,
        first,
        name: "local".to_string(),
    })
}

/// Seconds since the Unix epoch, right now.
///
/// The engine stamps everything in microseconds (`mesh_proto::now_micros`);
/// wall-clock reasoning wants seconds, and doing the division in one place
/// keeps the two from drifting apart in somebody's head.
pub fn now_unix() -> i64 {
    i64::try_from(mesh_proto::now_micros() / 1_000_000).unwrap_or(0)
}

/// Civil date for a day count since the Unix epoch (proleptic Gregorian).
/// Howard Hinnant's algorithm; no tzdata involved, the offset is the
/// caller's business.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// An instant (Unix microseconds) as a person reads it on this machine:
/// `HH:MM` when it falls on today's local date, `YYYY-MM-DD HH:MM` when it
/// does not — the shape a drain deadline or a run's finish time wants.
pub fn describe_instant(micros: u64) -> String {
    let zone = LocalZone::local();
    let unix = i64::try_from(micros / 1_000_000).unwrap_or(0);
    let local = unix + i64::from(zone.offset_at(unix));
    let today = now_unix() + i64::from(zone.offset_at(now_unix()));
    let minute = ((local.rem_euclid(DAY)) / 60) as u32;
    if local.div_euclid(DAY) == today.div_euclid(DAY) {
        return hhmm(minute);
    }
    let (y, m, d) = civil_from_days(local.div_euclid(DAY));
    format!("{y:04}-{m:02}-{d:02} {}", hhmm(minute))
}

/// The next instant at which the local clock reads `HH:MM`, as Unix
/// microseconds — later today, or tomorrow if that minute has passed.
/// What `drain --until 06:00` means, in the machine's own zone.
pub fn next_local_hhmm(hour: u32, minute: u32) -> u64 {
    let zone = LocalZone::local();
    let now = now_unix();
    let target = hour * 60 + minute;
    let current = zone.minute_of_day(now);
    let ahead = if target > current {
        i64::from(target - current) * 60
    } else {
        DAY - i64::from(current - target) * 60
    };
    // Seconds into the current minute are dropped so the deadline lands
    // on the minute rather than `n` seconds past it.
    let secs = now - now.rem_euclid(60) + ahead;
    u64::try_from(secs).unwrap_or(0).saturating_mul(1_000_000)
}

/// Render a minute-of-day as `HH:MM`, for every message that has to say
/// when something ends.
pub fn hhmm(minute_of_day: u32) -> String {
    format!("{:02}:{:02}", (minute_of_day / 60) % 24, minute_of_day % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_zone_has_no_transitions() {
        let z = LocalZone::fixed(-5 * 3600, "test");
        assert_eq!(z.offset_at(0), -5 * 3600);
        assert_eq!(z.next_transition_after(0), None);
        // 1970-01-01T00:00:00Z is 19:00 on 1969-12-31 in UTC-5.
        assert_eq!(z.minute_of_day(0), 19 * 60);
    }

    #[test]
    fn minute_of_day_never_goes_negative() {
        // The bug this pins: `local % DAY` is negative for any instant
        // whose local time is before the epoch, which put every evening
        // window on the wrong side of every comparison.
        let z = LocalZone::fixed(-6 * 3600, "test");
        for unix in [0, 1, 3600, -1, -86_399] {
            let m = z.minute_of_day(unix);
            assert!(m < 1440, "minute_of_day({unix}) = {m}");
        }
    }

    #[test]
    fn offsets_follow_the_transition_table() {
        let z = LocalZone::from_transitions(vec![100, 200, 300], vec![3600, 7200, 3600], 0);
        assert_eq!(z.offset_at(50), 0, "before the first transition");
        assert_eq!(z.offset_at(100), 3600, "exactly on a transition");
        assert_eq!(z.offset_at(150), 3600);
        assert_eq!(z.offset_at(250), 7200);
        assert_eq!(z.offset_at(10_000), 3600, "after the last transition");
    }

    #[test]
    fn next_transition_is_strictly_after() {
        let z = LocalZone::from_transitions(vec![100, 200], vec![1, 2], 0);
        assert_eq!(z.next_transition_after(0), Some(100));
        // Strictly after, or a wake-up scheduled *at* a transition would
        // reschedule itself for the same instant, forever.
        assert_eq!(z.next_transition_after(100), Some(200));
        assert_eq!(z.next_transition_after(200), None);
    }

    #[test]
    fn posix_signs_are_backwards_and_we_know_it() {
        // POSIX `EST5` is five hours *west*: UTC-5.
        assert_eq!(posix_fixed("EST5").map(|z| z.offset_at(0)), Some(-5 * 3600));
        // `CET-1` is one hour *east*: UTC+1.
        assert_eq!(posix_fixed("CET-1").map(|z| z.offset_at(0)), Some(3600));
        assert_eq!(
            posix_fixed("IST-5:30").map(|z| z.offset_at(0)),
            Some(5 * 3600 + 1800)
        );
        assert_eq!(
            posix_fixed("<-04>4").map(|z| z.offset_at(0)),
            Some(-4 * 3600)
        );
        // The base offset of a rule form is honoured; the rule is not.
        assert_eq!(
            posix_fixed("EST5EDT,M3.2.0,M11.1.0").map(|z| z.offset_at(0)),
            Some(-5 * 3600)
        );
        assert!(posix_fixed("Europe/Paris").is_none());
        assert!(posix_fixed("UTC").is_none());
    }

    #[test]
    fn a_real_zoneinfo_file_parses_if_this_machine_has_one() {
        // Not an assertion about the build machine: containers routinely
        // ship without tzdata, and the fallback is the tested path there.
        let Ok(zone) = read_zone(Path::new("/usr/share/zoneinfo/UTC")) else {
            return;
        };
        assert_eq!(zone.offset_at(now_unix()), 0);
    }

    #[test]
    fn a_zone_with_daylight_saving_actually_changes_offset() {
        let Ok(zone) = read_zone(Path::new("/usr/share/zoneinfo/America/New_York")) else {
            return;
        };
        // 2026-01-15T12:00:00Z (EST, -5) and 2026-07-15T12:00:00Z (EDT, -4).
        let winter = 1_768_478_400;
        let summer = 1_784_116_800;
        assert_eq!(zone.offset_at(winter), -5 * 3600);
        assert_eq!(zone.offset_at(summer), -4 * 3600);
        let next = zone
            .next_transition_after(winter)
            .expect("New York has transitions after 2026");
        assert!(next > winter && next < summer);
    }

    #[test]
    fn garbage_is_refused_rather_than_guessed_at() {
        assert!(parse_tzif(b"").is_none());
        assert!(parse_tzif(b"TZif").is_none());
        assert!(parse_tzif(&[0u8; 44]).is_none(), "no magic");
        let mut almost = *b"TZif2";
        almost[4] = b'2';
        assert!(parse_tzif(&almost).is_none(), "truncated header");
    }

    #[test]
    fn hhmm_wraps_at_midnight() {
        assert_eq!(hhmm(0), "00:00");
        assert_eq!(hhmm(8 * 60), "08:00");
        assert_eq!(hhmm(23 * 60 + 59), "23:59");
        assert_eq!(hhmm(1440), "00:00");
    }
}
