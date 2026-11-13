//! Quiet hours: time-boxed load caps, in the operator's own local time.
//!
//! ```toml
//! [limits]
//! quiet = "08:00-23:00 @ 30%"
//! ```
//!
//! At home, fan noise is a real API. A rack in a basement can run flat out
//! at any hour; the same machine in a living room cannot, and the
//! difference between a mesh that is welcome in the house and one that gets
//! unplugged is whether it knows the difference. This is that dial: thirty
//! percent of the slots while the household is awake, everything after
//! midnight.
//!
//! ## Why this is a local cap and not a smaller announcement
//!
//! A node entering quiet hours could republish its `slots` so the scheduler
//! stops preferring it. It deliberately does not. Two reasons:
//!
//! - **It would be op-log churn on a timer.** Every node in the mesh would
//!   author two `MemberUpdate` ops a day, forever, to say something that
//!   changes back. M9 §3.3 settled this shape of question once already:
//!   state that oscillates on a clock is not state, it is a reading.
//! - **The refusal path already exists and is better.** A quiet node whose
//!   slots are full answers `Busy`, and `Busy` is what the origin already
//!   spills on (MASTERPLAN §10.2) — to the next-best candidate, immediately.
//!   That is the same mechanism the memory floor uses, and reusing it means
//!   quiet hours cost one round trip in the worst case and nothing in the
//!   op-log ever.
//!
//! So a quiet node stays a full member: it gossips, it holds state, it owns
//! and adopts work, it can be the origin of a job. It just declines to
//! *start* more than its share of it, here, now.
//!
//! ## Running work is never killed
//!
//! A cap is a promotion gate, exactly as `[limits] slots` is
//! ([`crate::jobs::JobManager::set_limits`]: "a shrunken slot count never
//! kills running work"). Entering a 0% window at 08:00 does not SIGKILL the
//! transcode that started at 07:58 — it stops the next one starting. A
//! person who wants work to *stop* is asking for a drain, and `meshnode
//! drain` is that verb; quietly redefining a schedule as a killer would
//! make the gentlest-looking setting in the file the most destructive one.

use std::fmt;

use crate::localtime::{self, LocalZone};

/// A window's cap, as a percentage of the node's configured slots.
///
/// `0` is meaningful and supported: "take nothing while I am awake". It is
/// not the same as being drained — the node still holds state, still owns
/// work, and still finishes what it started.
pub type Percent = u8;

/// One window: a local-time span and the share of slots allowed inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Window {
    /// Local minute-of-day the window opens, `0..1440`.
    pub start: u32,
    /// Local minute-of-day it closes, `0..=1440`. A window that closes at
    /// or before it opens wraps past midnight (`23:00-06:00`).
    pub end: u32,
    /// Slot cap inside it, as a percentage of `[limits] slots`.
    pub pct: Percent,
}

impl Window {
    /// Does this window cover `minute` (local minute-of-day)?
    ///
    /// Half-open: `[start, end)`. A window ending at 23:00 does not include
    /// 23:00, so `"00:00-08:00 @ 10%, 08:00-23:00 @ 30%"` has no minute
    /// claimed by two windows and no minute claimed by none.
    pub fn covers(&self, minute: u32) -> bool {
        if self.start == self.end {
            // `00:00-00:00` — the whole day. Written by people who mean
            // "always", and refusing it would be pedantry.
            return true;
        }
        if self.start < self.end {
            minute >= self.start && minute < self.end
        } else {
            // Wraps midnight.
            minute >= self.start || minute < self.end
        }
    }

    /// The next local minute-of-day at which this window's coverage
    /// changes, given the current minute.
    fn next_edge(&self, minute: u32) -> u32 {
        if self.covers(minute) {
            self.end % 1440
        } else {
            self.start % 1440
        }
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{} @ {}%",
            localtime::hhmm(self.start),
            localtime::hhmm(self.end),
            self.pct
        )
    }
}

/// Every quiet window configured on this node. Empty means "no quiet
/// hours", which is the default and costs nothing to evaluate.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct QuietHours {
    windows: Vec<Window>,
}

/// What quiet hours say about right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietState {
    /// The window in force, and its cap.
    pub window: Window,
    /// The cap applied to this node's configured slot count.
    pub slots: u32,
    /// Local minute-of-day the current state ends.
    pub until: u32,
}

impl QuietHours {
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// Parse one or more windows.
    ///
    /// Accepted, because all of these are what people actually type:
    ///
    /// ```text
    /// 08:00-23:00 @ 30%      the documented form
    /// 08:00–23:00 @ 30%      an en dash, because word processors and web pages
    /// 08:00-23:00 @ 30       the % is decoration
    /// 08:00-23:00 30%        so is the @
    /// 8:00-23:00 @ 30%       a bare hour
    /// 23:00-06:00 @ 0%       wraps midnight; takes nothing
    /// 08:00-23:00 @ 30%, 23:00-00:00 @ 60%   several, comma separated
    /// ```
    ///
    /// A window with no cap is refused rather than defaulted. "Quiet from
    /// eight to eleven" reads to one person as *silent* and to another as
    /// *quieter*, and a scheduler must not pick one of those on their
    /// behalf.
    pub fn parse(spec: &str) -> Result<QuietHours, String> {
        let mut windows = Vec::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            windows.push(parse_window(part)?);
        }
        Ok(QuietHours { windows })
    }

    /// Parse several entries, as a TOML array spells them.
    pub fn parse_all<S: AsRef<str>>(specs: &[S]) -> Result<QuietHours, String> {
        let mut windows = Vec::new();
        for spec in specs {
            windows.extend(QuietHours::parse(spec.as_ref())?.windows);
        }
        Ok(QuietHours { windows })
    }

    /// The state in force at `unix` seconds, given `slots` configured.
    ///
    /// `None` means no window covers this instant: the node runs at its
    /// full configured capacity, which is what an unconfigured node does
    /// every second of every day.
    ///
    /// **The tightest cap wins** where windows overlap. Overlap is a
    /// configuration mistake rather than a feature, and of the two ways to
    /// resolve one, the quiet reading is the safe one: a person who wrote
    /// two windows by accident gets a machine that is too polite, not one
    /// that is loud at the hour they were trying to protect.
    pub fn state_at(&self, zone: &LocalZone, unix: i64, slots: u32) -> Option<QuietState> {
        let minute = zone.minute_of_day(unix);
        let window = self
            .windows
            .iter()
            .filter(|w| w.covers(minute))
            .min_by_key(|w| w.pct)?;
        Some(QuietState {
            window: *window,
            slots: capped(slots, window.pct),
            until: self.next_edge(minute),
        })
    }

    /// The effective slot count at `unix`: the cap if one applies, else
    /// `slots` unchanged.
    pub fn slots_at(&self, zone: &LocalZone, unix: i64, slots: u32) -> u32 {
        self.state_at(zone, unix, slots)
            .map_or(slots, |state| state.slots)
    }

    /// The next local minute-of-day at which *any* window's coverage
    /// changes.
    fn next_edge(&self, minute: u32) -> u32 {
        let mut best: Option<u32> = None;
        for w in &self.windows {
            let edge = w.next_edge(minute);
            // Minutes until the edge, wrapping. An edge exactly on the
            // current minute is a full day away, not zero: it is the edge
            // we are standing on, and it has already happened.
            let delta = (edge + 1440 - minute) % 1440;
            let delta = if delta == 0 { 1440 } else { delta };
            let current = best.map_or(u32::MAX, |b: u32| (b + 1440 - minute) % 1440);
            if delta < current {
                best = Some(edge);
            }
        }
        best.unwrap_or(minute)
    }

    /// When the current state next changes, as an absolute instant.
    ///
    /// This is what the daemon sleeps until, and it is why
    /// [`LocalZone::next_transition_after`] exists: on the two days a year
    /// the clocks move, the wall-clock edge and the offset change are
    /// different instants and the earlier one wins. Without that, a window
    /// ending at 23:00 on the night the clocks go back is re-evaluated an
    /// hour late — once a year, in the dark, on the one night the
    /// household would notice.
    pub fn next_change_after(&self, zone: &LocalZone, unix: i64) -> Option<i64> {
        if self.windows.is_empty() {
            return None;
        }
        let minute = zone.minute_of_day(unix);
        let edge = self.next_edge(minute);
        let mins_ahead = i64::from((edge + 1440 - minute) % 1440);
        let mins_ahead = if mins_ahead == 0 { 1440 } else { mins_ahead };
        // Land on the start of the target minute rather than 59 seconds
        // into it, so a 23:00 edge fires at 23:00:00.
        let secs_into_minute = (unix + i64::from(zone.offset_at(unix))).rem_euclid(60);
        let wall = unix + mins_ahead * 60 - secs_into_minute;
        Some(match zone.next_transition_after(unix) {
            Some(t) if t < wall => t,
            _ => wall,
        })
    }

    /// One line for `status`, `top` and the Director panel.
    pub fn describe(&self) -> String {
        if self.windows.is_empty() {
            return "off".to_string();
        }
        self.windows
            .iter()
            .map(Window::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Apply a percentage cap to a slot or queue count.
///
/// Rounds **down**, and does not floor at one. `30%` of three slots is
/// zero, and inventing a slot there would break the promise the number
/// makes: an operator who caps a small node hard means it. `0%` is a real
/// answer, and the node still holds and finishes everything it already has.
pub(crate) fn capped(slots: u32, pct: Percent) -> u32 {
    if pct >= 100 {
        return slots;
    }
    u32::try_from(u64::from(slots) * u64::from(pct) / 100).unwrap_or(slots)
}

fn parse_window(spec: &str) -> Result<Window, String> {
    // The en dash is not an affectation: these strings get written in
    // documentation, pasted out of a web page, and typed on a Mac with
    // smart substitution on. Accepting it costs one character class and
    // saves a support question that looks like a scheduler bug.
    let normalised = spec.replace(['\u{2013}', '\u{2014}'], "-");
    let (span, cap) = match normalised.split_once('@') {
        Some((span, cap)) => (span.trim(), Some(cap.trim())),
        None => {
            // `08:00-23:00 30%` — the `@` is decoration, so accept the
            // form without it by splitting on the last whitespace.
            match normalised.trim().rsplit_once(char::is_whitespace) {
                Some((span, cap)) => (span.trim(), Some(cap.trim())),
                None => (normalised.trim(), None),
            }
        }
    };
    let Some(cap) = cap.filter(|c| !c.is_empty()) else {
        return Err(format!(
            "quiet window `{spec}` has no cap — write `{span} @ 30%`. \
             There is no default on purpose: \"quiet\" means *silent* to one \
             person and *quieter* to another, and the scheduler must not pick"
        ));
    };
    let pct: u32 = cap
        .trim_end_matches('%')
        .trim()
        .parse()
        .map_err(|_| format!("quiet window `{spec}`: `{cap}` is not a percentage"))?;
    if pct > 100 {
        return Err(format!(
            "quiet window `{spec}`: {pct}% is more than all of the slots"
        ));
    }
    let (start, end) = span
        .split_once('-')
        .ok_or_else(|| format!("quiet window `{spec}` needs a span, e.g. `08:00-23:00 @ 30%`"))?;
    Ok(Window {
        start: parse_hhmm(start.trim(), spec)?,
        end: parse_hhmm(end.trim(), spec)?,
        pct: u8::try_from(pct).unwrap_or(100),
    })
}

fn parse_hhmm(s: &str, spec: &str) -> Result<u32, String> {
    let bad = || format!("quiet window `{spec}`: `{s}` is not a time like `08:00`");
    // `24:00` is the honest way to say "the end of the day" and people
    // write it; it is the same instant as `00:00` and normalises to it
    // everywhere except as an end, where `covers` treats `start < end`.
    let (h, m) = match s.split_once(':') {
        Some((h, m)) => (h, m),
        None => (s, "0"),
    };
    let hours: u32 = h.trim().parse().map_err(|_| bad())?;
    let minutes: u32 = m.trim().parse().map_err(|_| bad())?;
    if hours > 24 || minutes > 59 || (hours == 24 && minutes > 0) {
        return Err(bad());
    }
    Ok(hours * 60 + minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> LocalZone {
        LocalZone::utc()
    }

    /// `unix` for a given UTC hour:minute on an arbitrary Thursday.
    fn at(hour: u32, minute: u32) -> i64 {
        1_767_225_600 + i64::from(hour) * 3600 + i64::from(minute) * 60
    }

    #[test]
    fn the_documented_form_parses() {
        let q = QuietHours::parse("08:00-23:00 @ 30%").expect("parses");
        assert_eq!(
            q.windows(),
            [Window {
                start: 480,
                end: 1380,
                pct: 30
            }]
        );
        assert_eq!(q.describe(), "08:00-23:00 @ 30%");
    }

    #[test]
    fn every_shape_people_actually_type() {
        let want = Window {
            start: 480,
            end: 1380,
            pct: 30,
        };
        for spec in [
            "08:00-23:00 @ 30%",
            "08:00\u{2013}23:00 @ 30%",
            "08:00-23:00 @ 30",
            "08:00-23:00 30%",
            "8:00-23:00 @ 30%",
            "  08:00 - 23:00  @  30%  ",
        ] {
            let q = QuietHours::parse(spec).unwrap_or_else(|e| panic!("{spec}: {e}"));
            assert_eq!(q.windows(), [want], "{spec}");
        }
    }

    #[test]
    fn a_window_without_a_cap_is_refused_not_guessed() {
        let e = QuietHours::parse("08:00-23:00").expect_err("no cap");
        assert!(e.contains("no cap"), "{e}");
        assert!(QuietHours::parse("08:00-23:00 @ 101%").is_err());
        assert!(QuietHours::parse("08:00 @ 30%").is_err());
        assert!(QuietHours::parse("25:00-26:00 @ 30%").is_err());
        assert!(QuietHours::parse("08:00-23:00 @ loud").is_err());
    }

    #[test]
    fn nothing_configured_caps_nothing() {
        let q = QuietHours::default();
        assert!(q.is_empty());
        assert_eq!(q.slots_at(&utc(), at(3, 0), 16), 16);
        assert_eq!(q.state_at(&utc(), at(3, 0), 16), None);
        assert_eq!(q.next_change_after(&utc(), at(3, 0)), None);
        assert_eq!(q.describe(), "off");
    }

    #[test]
    fn awake_hours_are_capped_and_the_small_hours_are_not() {
        let q = QuietHours::parse("08:00-23:00 @ 30%").expect("parses");
        // Inside: 30% of 16 slots.
        let state = q.state_at(&utc(), at(12, 0), 16).expect("in the window");
        assert_eq!(state.slots, 4);
        assert_eq!(state.until, 23 * 60);
        // Outside: everything.
        assert_eq!(q.slots_at(&utc(), at(2, 0), 16), 16);
        assert_eq!(q.state_at(&utc(), at(2, 0), 16), None);
    }

    #[test]
    fn the_window_is_half_open_so_no_minute_is_claimed_twice() {
        let q = QuietHours::parse("00:00-08:00 @ 10%, 08:00-23:00 @ 30%").expect("parses");
        assert_eq!(q.slots_at(&utc(), at(7, 59), 100), 10);
        assert_eq!(
            q.slots_at(&utc(), at(8, 0), 100),
            30,
            "08:00 belongs to the second"
        );
        assert_eq!(q.slots_at(&utc(), at(22, 59), 100), 30);
        assert_eq!(q.slots_at(&utc(), at(23, 0), 100), 100, "23:00 is free");
    }

    #[test]
    fn a_window_may_wrap_midnight() {
        let q = QuietHours::parse("23:00-06:00 @ 0%").expect("parses");
        assert_eq!(q.slots_at(&utc(), at(23, 30), 8), 0);
        assert_eq!(q.slots_at(&utc(), at(1, 0), 8), 0);
        assert_eq!(q.slots_at(&utc(), at(5, 59), 8), 0);
        assert_eq!(q.slots_at(&utc(), at(6, 0), 8), 8);
        assert_eq!(q.slots_at(&utc(), at(12, 0), 8), 8);
    }

    #[test]
    fn zero_percent_is_a_real_answer() {
        let q = QuietHours::parse("08:00-23:00 @ 0%").expect("parses");
        assert_eq!(q.slots_at(&utc(), at(12, 0), 64), 0);
    }

    #[test]
    fn a_cap_rounds_down_and_does_not_invent_a_slot() {
        // 30% of 3 is 0.9. An operator who caps a three-slot box at 30%
        // means it; rounding up would break the number's promise.
        assert_eq!(capped(3, 30), 0);
        assert_eq!(capped(4, 30), 1);
        assert_eq!(capped(16, 30), 4);
        assert_eq!(capped(16, 100), 16);
        assert_eq!(capped(16, 0), 0);
    }

    #[test]
    fn the_tightest_overlapping_cap_wins() {
        let q = QuietHours::parse("08:00-23:00 @ 30%, 12:00-14:00 @ 5%").expect("parses");
        assert_eq!(q.slots_at(&utc(), at(9, 0), 100), 30);
        assert_eq!(q.slots_at(&utc(), at(13, 0), 100), 5);
    }

    #[test]
    fn the_next_change_is_the_windows_own_edge() {
        let zone = utc();
        let q = QuietHours::parse("08:00-23:00 @ 30%").expect("parses");
        // 12:00 → the window closes at 23:00, eleven hours out.
        let next = q.next_change_after(&zone, at(12, 0)).expect("an edge");
        assert_eq!(next - at(12, 0), 11 * 3600);
        // 23:30 → the window opens again at 08:00, eight and a half hours out.
        let next = q.next_change_after(&zone, at(23, 30)).expect("an edge");
        assert_eq!(next - at(23, 30), 8 * 3600 + 1800);
    }

    #[test]
    fn an_edge_exactly_now_is_a_day_away_not_zero() {
        // A wake-up scheduled for the instant it is already at would spin.
        let zone = utc();
        let q = QuietHours::parse("08:00-23:00 @ 30%").expect("parses");
        let next = q.next_change_after(&zone, at(8, 0)).expect("an edge");
        assert!(next > at(8, 0));
        assert_eq!(next, at(23, 0));
    }

    #[test]
    fn the_change_lands_on_the_start_of_its_minute() {
        let zone = utc();
        let q = QuietHours::parse("08:00-23:00 @ 30%").expect("parses");
        // 42 seconds past 12:00 → still 23:00:00 exactly.
        let now = at(12, 0) + 42;
        assert_eq!(q.next_change_after(&zone, now), Some(at(23, 0)));
    }

    #[test]
    fn a_daylight_saving_change_wins_when_it_comes_first() {
        // The window's own edge is a day out; the clocks move in an hour.
        // Re-evaluating at the transition is the point: after it, the same
        // UTC instant is a different local minute.
        let zone = LocalZone::from_transitions(vec![at(13, 0)], vec![3600], 0);
        let q = QuietHours::parse("08:00-23:00 @ 30%").expect("parses");
        assert_eq!(q.next_change_after(&zone, at(12, 0)), Some(at(13, 0)));
    }

    #[test]
    fn windows_are_evaluated_in_local_time_not_utc() {
        // 03:00 UTC is 21:00 the previous evening in UTC-6 — inside an
        // 08:00–23:00 window, which UTC would have called free.
        let zone = LocalZone::fixed(-6 * 3600, "test");
        let q = QuietHours::parse("08:00-23:00 @ 30%").expect("parses");
        assert_eq!(q.slots_at(&zone, at(3, 0), 10), 3);
        assert_eq!(q.slots_at(&utc(), at(3, 0), 10), 10);
    }

    #[test]
    fn several_entries_parse_as_one_set() {
        let q = QuietHours::parse_all(&["08:00-23:00 @ 30%", "23:00-00:00 @ 60%"]).expect("parses");
        assert_eq!(q.windows().len(), 2);
        assert_eq!(q.describe(), "08:00-23:00 @ 30%, 23:00-00:00 @ 60%");
    }
}
