//! Schedules: a workflow that runs itself (M8 §3).
//!
//! A schedule is a **replicated, user-signed entity of its own** — never a
//! field on [`crate::WorkflowVersion`], whose signature covers its whole
//! postcard encoding, and never a field on a run, which it outlives many
//! times over. The record says *what to run* and *when*; everything about
//! *how* is the workflow's, and everything about *who* is re-derived per
//! fire from this record's own signature (§3.4).
//!
//! ## Time
//!
//! The mesh carries **no timezone database**: a fixed UTC offset is the
//! whole vocabulary, because a DST transition that silently moves a
//! production change by an hour is not a feature worth a data dependency.
//! [`Trigger::Daily`] is a weekday bitmask, minutes since local midnight,
//! and minutes east of UTC.
//!
//! An occurrence is the instant the expression **names**, never the moment
//! a node noticed it was due. That is what makes [`crate::RunId::derive`]
//! agree across nodes whose clocks do not, and it bounds clock skew to
//! shifting *when* a schedule fires rather than *what* fires or *how many
//! times* (§3.7).

use serde::{Deserialize, Serialize};

use crate::{JobPriority, UserId};

/// The floor on how often a schedule may fire, in seconds.
///
/// Two reasons, both concrete. Cron's own resolution is a minute, so
/// nothing below this is a schedule anyone asked for. And a run entry is
/// **never removed** from replicated state — dropping one would wedge its
/// author's op stream (see `mesh_core::state::RunEntry`) — so every fire
/// costs a permanent map entry on every node in the mesh. A ten-second
/// schedule is an unbounded memory cost by construction, and refusing it at
/// create time is the only place the refusal is cheap.
pub const MIN_INTERVAL_SECS: u64 = 60;

/// How far past an occurrence a catch-up fire is still worth doing, by
/// default: an hour. "The box was rebooting at 03:00, still take the backup
/// at 03:10" is the case this exists for.
pub const DEFAULT_GRACE_SECS: u64 = 3600;

/// Job environment key naming the schedule that fired a run.
///
/// Under the workflow engine's already-reserved `MESHWARE_WF_` prefix
/// ([`crate::WF_ENV_PREFIX`]), so it costs no wire change and no new
/// reserved namespace — and a step can tell it was run by a schedule
/// rather than by a person.
pub const WF_SCHEDULE: &str = "MESHWARE_WF_SCHEDULE";

/// When a schedule fires.
///
/// An enum rather than a cron string, so the forms this does not yet
/// support are an **appended variant** rather than a string-format
/// migration — and so a webhook trigger, when it comes, is a variant here
/// rather than a second entity (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// Every `secs`, anchored on the **unix epoch** — not on the moment the
    /// schedule was created or the node last restarted. Anchoring is what
    /// makes two nodes agree on which instants are occurrences at all.
    Every { secs: u64 },
    /// At a time of day, on a set of weekdays, in a fixed UTC offset.
    Daily {
        /// Minutes since local midnight, `0..=1439`.
        minute_of_day: u16,
        /// Weekday bitmask, bit 0 = Monday … bit 6 = Sunday. Zero is
        /// rejected at parse time — a schedule that can never fire is a
        /// typo, not a configuration.
        days: u8,
        /// Minutes east of UTC. Fixed, never a timezone name.
        utc_offset_minutes: i16,
    },
}

impl Trigger {
    /// The first occurrence strictly after `after_micros`, or `None` when
    /// the trigger can never fire.
    pub fn next_after(&self, after_micros: u64) -> Option<u64> {
        match *self {
            Trigger::Every { secs } => {
                let period = secs.max(1).checked_mul(1_000_000)?;
                let elapsed = after_micros / period;
                elapsed.checked_add(1)?.checked_mul(period)
            }
            Trigger::Daily {
                minute_of_day,
                days,
                utc_offset_minutes,
            } => {
                if days == 0 {
                    return None;
                }
                let (day, minute) = local_parts(after_micros, utc_offset_minutes);
                // Today if the time has not passed yet, else walk forward.
                // Seven days is enough to reach any set bit; the eighth
                // covers "today, later" having already been rejected.
                for ahead in 0..8i64 {
                    let candidate_day = day + ahead;
                    if ahead == 0 && u32::from(minute_of_day) <= u32::from(minute) {
                        continue;
                    }
                    if !day_set(days, weekday_of(candidate_day)) {
                        continue;
                    }
                    return instant_of(candidate_day, minute_of_day, utc_offset_minutes);
                }
                None
            }
        }
    }

    /// The most recent occurrence at or before `at_micros`, or `None` when
    /// there is none within a bounded lookback.
    ///
    /// Bounded deliberately: catch-up asks "did we miss the *last* one",
    /// never "enumerate everything since the epoch".
    pub fn previous_at_or_before(&self, at_micros: u64) -> Option<u64> {
        match *self {
            Trigger::Every { secs } => {
                let period = secs.max(1).checked_mul(1_000_000)?;
                Some(at_micros / period * period)
            }
            Trigger::Daily {
                minute_of_day,
                days,
                utc_offset_minutes,
            } => {
                if days == 0 {
                    return None;
                }
                let (day, minute) = local_parts(at_micros, utc_offset_minutes);
                for back in 0..8i64 {
                    let candidate_day = day - back;
                    if back == 0 && u32::from(minute_of_day) > u32::from(minute) {
                        continue;
                    }
                    if !day_set(days, weekday_of(candidate_day)) {
                        continue;
                    }
                    return instant_of(candidate_day, minute_of_day, utc_offset_minutes);
                }
                None
            }
        }
    }

    /// The next `count` occurrences after `after_micros`. What
    /// `meshnode schedule create --dry-run` prints, and the only honest way
    /// to answer "when will this actually run".
    pub fn upcoming(&self, after_micros: u64, count: usize) -> Vec<u64> {
        let mut out = Vec::with_capacity(count);
        let mut cursor = after_micros;
        for _ in 0..count {
            match self.next_after(cursor) {
                Some(next) => {
                    out.push(next);
                    cursor = next;
                }
                None => break,
            }
        }
        out
    }

    /// Minutes east of UTC this trigger is expressed in.
    ///
    /// Every surface that prints an occurrence needs it: someone who wrote
    /// `at: "03:00"` with `tz: "+02:00"` must be shown 03:00, not the
    /// 01:00 UTC it happens to land on. Showing the wrong one is how a
    /// person "corrects" a schedule that was already right.
    pub fn utc_offset_minutes(&self) -> i16 {
        match *self {
            // An interval has no local time to speak of; UTC is the honest
            // frame for it.
            Trigger::Every { .. } => 0,
            Trigger::Daily {
                utc_offset_minutes, ..
            } => utc_offset_minutes,
        }
    }

    /// The shortest gap between two consecutive occurrences, for the
    /// [`MIN_INTERVAL_SECS`] check.
    pub fn min_interval_secs(&self) -> u64 {
        match *self {
            Trigger::Every { secs } => secs,
            // Two fires on one day is not expressible, so the tightest a
            // daily trigger can be is 24h.
            Trigger::Daily { .. } => 24 * 3600,
        }
    }

    /// One line a human reads in a table: `every 15m`, `03:00 mon-fri +02:00`.
    pub fn describe(&self) -> String {
        match *self {
            Trigger::Every { secs } => format!("every {}", humanize_secs(secs)),
            Trigger::Daily {
                minute_of_day,
                days,
                utc_offset_minutes,
            } => {
                let time = format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60);
                let mut out = time;
                if days != 0b0111_1111 {
                    out.push(' ');
                    out.push_str(&describe_days(days));
                }
                if utc_offset_minutes != 0 {
                    out.push(' ');
                    out.push_str(&describe_offset(utc_offset_minutes));
                }
                out
            }
        }
    }
}

/// What to do about an occurrence nobody was up to fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CatchUp {
    /// Do nothing. The mesh has no memory of intent, and a rolling upgrade
    /// arriving four hours late is a surprise, not a recovery.
    #[default]
    Skip,
    /// Fire the **most recent** missed occurrence, and only within
    /// `grace_secs`. Never a backlog — see the type's `Skip` note and §3.11.
    Once,
}

impl CatchUp {
    pub fn as_str(&self) -> &'static str {
        match self {
            CatchUp::Skip => "skip",
            CatchUp::Once => "once",
        }
    }
}

/// What to do when the previous run has not finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Overlap {
    /// Skip this occurrence. A 70-minute rolling upgrade on an hourly
    /// schedule must not stack.
    #[default]
    Skip,
    /// Run anyway, for genuinely independent work.
    Allow,
}

impl Overlap {
    pub fn as_str(&self) -> &'static str {
        match self {
            Overlap::Skip => "skip",
            Overlap::Allow => "allow",
        }
    }
}

/// A schedule: a standing, user-signed authorization to run one workflow on
/// a recurring instant (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    /// Unique. The handle every command, audit entry and run correlation
    /// uses.
    pub name: String,
    pub workflow: String,
    /// A pinned version, or `None` for whatever the head is at fire time —
    /// matching what a human typing `workflow run <name>` gets.
    pub workflow_version: Option<u32>,
    /// Run inputs, validated against the workflow's contract at create
    /// time. The point of a schedule is that nobody is watching when it
    /// runs, so a schema error has to surface while somebody is.
    pub inputs: Vec<(String, String)>,
    pub when: Trigger,
    pub catch_up: CatchUp,
    /// How far past an occurrence [`CatchUp::Once`] will still fire it.
    pub grace_secs: u64,
    pub overlap: Overlap,
    /// Paused schedules stay in state, keep their history, and fire
    /// nothing. Pausing is an edit, so it re-signs the record.
    pub paused: bool,
    pub priority: JobPriority,
    /// Whole-run timeout, independent of any step's.
    pub timeout_secs: Option<u64>,
    pub description: String,
    /// The user whose authority every fire of this schedule carries.
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`],
    /// verified at merge time on every node **and again on every executor
    /// at every fire** (§3.4).
    pub signature: Vec<u8>,
}

impl Schedule {
    /// The bytes the creator's signature covers: the whole record with the
    /// signature blanked.
    ///
    /// Everything is covered, `paused` included. Unlike a workflow's
    /// layout, there is no field here that must stay mutable without a
    /// human — a schedule is only ever changed by the person who owns it,
    /// and re-signing is what makes that provable.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-schedule-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// Is `occurrence_micros` an instant this schedule actually names?
    ///
    /// The anti-replay check that replaces envelope freshness on the
    /// executor (§3.4): a plan whose run id was derived from an instant the
    /// expression does not yield is not something this schedule could have
    /// produced, whoever signed it.
    pub fn names_occurrence(&self, occurrence_micros: u64) -> bool {
        // `next_after` is strict, so ask about the instant just before.
        self.when
            .next_after(occurrence_micros.saturating_sub(1))
            .is_some_and(|next| next == occurrence_micros)
    }

    /// `workflow` as a human writes it: `deploy` or `deploy@4`.
    pub fn workflow_ref(&self) -> String {
        match self.workflow_version {
            Some(v) => format!("{}@{v}", self.workflow),
            None => self.workflow.clone(),
        }
    }
}

/// Signing bytes for a schedule removal. Mirrors
/// [`crate::source_remove_signing_bytes`].
pub fn schedule_remove_signing_bytes(name: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-schedule-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&by.0);
    buf
}

/// Schedule names share the rules host names use: something a person types
/// on a command line and reads back in a table.
pub fn is_valid_schedule_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

// ---------------------------------------------------------------------------
// Civil-time arithmetic, in the one shape the mesh permits
// ---------------------------------------------------------------------------
//
// Days since the epoch and minutes since local midnight. No year, no month, no calendar —
// those are the two cron fields M8 §3.8 defers, and deferring them is what
// keeps this file free of a date-library dependency.

/// `(days since epoch, minute of local day)` at an instant, in an offset.
fn local_parts(at_micros: u64, utc_offset_minutes: i16) -> (i64, u16) {
    let secs = (at_micros / 1_000_000) as i64 + i64::from(utc_offset_minutes) * 60;
    let day = secs.div_euclid(86_400);
    let minute = (secs.rem_euclid(86_400) / 60) as u16;
    (day, minute)
}

/// Weekday of a day-since-epoch, 0 = Monday. 1970-01-01 was a Thursday, so
/// the anchor is `+3`.
fn weekday_of(day: i64) -> u32 {
    (day + 3).rem_euclid(7) as u32
}

fn day_set(days: u8, weekday: u32) -> bool {
    days & (1u8 << weekday) != 0
}

/// The UTC instant of a local minute on a local day.
fn instant_of(day: i64, minute_of_day: u16, utc_offset_minutes: i16) -> Option<u64> {
    let local_secs = day.checked_mul(86_400)? + i64::from(minute_of_day) * 60;
    let utc_secs = local_secs - i64::from(utc_offset_minutes) * 60;
    u64::try_from(utc_secs).ok()?.checked_mul(1_000_000)
}

fn humanize_secs(secs: u64) -> String {
    if secs >= 86_400 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs >= 3600 && secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
/// Every day, as a weekday bitmask.
const ALL_DAYS: u8 = 0b0111_1111;

/// Parse `15:30`, `1530` or `9` into minutes since midnight.
pub fn parse_minute(text: &str) -> Option<u16> {
    let text = text.trim();
    let (h, m) = match text.split_once(':') {
        Some((h, m)) => (h.parse::<u16>().ok()?, m.parse::<u16>().ok()?),
        None if text.len() == 4 => (text[..2].parse().ok()?, text[2..].parse().ok()?),
        None => (text.parse::<u16>().ok()?, 0),
    };
    (h <= 24 && m < 60).then_some((h % 24) * 60 + m)
}

/// Parse `+02:00`, `-0500`, `+2`, `utc`, `z` into minutes east of UTC.
pub fn parse_offset(text: &str) -> Option<i16> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("utc") || text.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let text = text
        .strip_prefix("UTC")
        .or_else(|| text.strip_prefix("utc"))
        .unwrap_or(text);
    let (sign, rest) = match text.chars().next()? {
        '+' => (1i16, &text[1..]),
        '-' => (-1i16, &text[1..]),
        _ => (1i16, text),
    };
    let minutes = i16::try_from(parse_minute(rest)?).ok()?;
    (minutes <= 14 * 60).then_some(sign * minutes)
}

/// Parse `mon,tue,wed`, `mon-fri`, `weekends`, `*` into a weekday bitmask,
/// bit 0 = Monday.
pub fn parse_days(text: &str) -> Option<u8> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() || text == "*" || text == "any" || text == "daily" || text == "every day" {
        return Some(ALL_DAYS);
    }
    if text == "weekdays" {
        return Some(0b0001_1111);
    }
    if text == "weekends" {
        return Some(0b0110_0000);
    }
    let index = |name: &str| DAY_NAMES.iter().position(|d| name.starts_with(d));
    let mut mask = 0u8;
    for part in text.split(',') {
        let part = part.trim();
        match part.split_once('-') {
            Some((from, to)) => {
                let (from, to) = (index(from.trim())?, index(to.trim())?);
                let mut d = from;
                loop {
                    mask |= 1 << d;
                    if d == to {
                        break;
                    }
                    d = (d + 1) % 7;
                }
            }
            None => mask |= 1 << index(part)?,
        }
    }
    (mask != 0).then_some(mask)
}

/// A weekday bitmask as the grammar [`parse_days`] reads back.
///
/// Public because N1 P5 renders a schedule to the TOML a person edits, and
/// a renderer that could not produce a string its own parser accepts would
/// break the round trip `schedule show --output toml | schedule put` is
/// supposed to have.
pub fn render_days(days: u8) -> String {
    describe_days(days)
}

/// A fixed UTC offset as [`parse_offset`] reads back.
pub fn render_offset(minutes: i16) -> String {
    describe_offset(minutes)
}

fn describe_days(days: u8) -> String {
    if days == 0b0001_1111 {
        return "mon-fri".to_string();
    }
    if days == 0b0110_0000 {
        return "sat,sun".to_string();
    }
    (0..7)
        .filter(|d| day_set(days, *d))
        .map(|d| DAY_NAMES[d as usize])
        .collect::<Vec<_>>()
        .join(",")
}

fn describe_offset(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_parsing_covers_the_shapes_operators_type() {
        assert_eq!(parse_days("mon-fri"), Some(0b0001_1111));
        assert_eq!(parse_days("weekdays"), Some(0b0001_1111));
        assert_eq!(parse_days("sat,sun"), Some(0b0110_0000));
        assert_eq!(parse_days("weekends"), Some(0b0110_0000));
        assert_eq!(parse_days("fri-mon"), Some(0b0111_0001));
        assert_eq!(parse_days("*"), Some(ALL_DAYS));
        assert_eq!(parse_days("nonsense"), None);
    }

    #[test]
    fn time_and_offset_parsing() {
        assert_eq!(parse_minute("15:30"), Some(930));
        assert_eq!(parse_minute("1530"), Some(930));
        assert_eq!(parse_minute("9"), Some(540));
        assert_eq!(parse_minute("25:00"), None);
        assert_eq!(parse_offset("+02:00"), Some(120));
        assert_eq!(parse_offset("-0500"), Some(-300));
        assert_eq!(parse_offset("UTC"), Some(0));
        assert_eq!(parse_offset("+15:00"), None);
    }

    /// 2026-01-05T00:00:00Z was a Monday. Every case below is anchored on
    /// it, because a weekday bug that only shows up in one timezone is the
    /// exact bug this arithmetic exists to avoid.
    const MON_MIDNIGHT: u64 = 1_767_571_200_000_000;
    const MINUTE: u64 = 60_000_000;
    const HOUR: u64 = 3_600_000_000;
    const DAY: u64 = 86_400_000_000;

    #[test]
    fn the_anchor_really_is_a_monday() {
        let (day, minute) = local_parts(MON_MIDNIGHT, 0);
        assert_eq!(weekday_of(day), 0, "0 = Monday");
        assert_eq!(minute, 0);
    }

    #[test]
    fn an_interval_is_anchored_on_the_epoch_not_on_this_node() {
        let every = Trigger::Every { secs: 900 };
        // Whatever instant you ask from, the answer is a multiple of the
        // period — so two nodes that started at different times still agree
        // on which instants are occurrences.
        for offset in [0u64, 1, 61 * 1_000_000, 899 * 1_000_000] {
            let next = every.next_after(MON_MIDNIGHT + offset).expect("next");
            assert_eq!(next % (900 * 1_000_000), 0);
            assert!(next > MON_MIDNIGHT + offset);
        }
    }

    #[test]
    fn a_daily_trigger_fires_at_the_named_local_time() {
        let at_3am = Trigger::Daily {
            minute_of_day: 180,
            days: 0b0111_1111,
            utc_offset_minutes: 0,
        };
        let next = at_3am.next_after(MON_MIDNIGHT).expect("next");
        assert_eq!(next, MON_MIDNIGHT + 3 * HOUR);
        // Just after it fires, the next one is tomorrow.
        assert_eq!(
            at_3am.next_after(next).expect("next"),
            MON_MIDNIGHT + DAY + 3 * HOUR
        );
    }

    #[test]
    fn weekday_narrowing_skips_the_weekend() {
        let weeknights = Trigger::Daily {
            minute_of_day: 180,
            days: 0b0001_1111, // mon-fri
            utc_offset_minutes: 0,
        };
        // Friday 03:00 → Monday 03:00, not Saturday.
        let friday_3am = MON_MIDNIGHT + 4 * DAY + 3 * HOUR;
        assert_eq!(
            weeknights.next_after(friday_3am).expect("next"),
            MON_MIDNIGHT + 7 * DAY + 3 * HOUR
        );
    }

    #[test]
    fn an_offset_moves_the_instant_not_the_weekday_rule() {
        // 03:00 at +02:00 is 01:00 UTC.
        let berlin_3am = Trigger::Daily {
            minute_of_day: 180,
            days: 0b0111_1111,
            utc_offset_minutes: 120,
        };
        assert_eq!(
            berlin_3am.next_after(MON_MIDNIGHT).expect("next"),
            MON_MIDNIGHT + HOUR
        );

        // And a local time early enough to fall on the previous UTC day
        // still lands on the right local weekday: 00:30 local at +02:00 on
        // Monday is 22:30 UTC on Sunday.
        let berlin_early = Trigger::Daily {
            minute_of_day: 30,
            days: 0b0000_0001, // Monday only
            utc_offset_minutes: 120,
        };
        let next = berlin_early.next_after(MON_MIDNIGHT - DAY).expect("next");
        assert_eq!(next, MON_MIDNIGHT - 90 * MINUTE);
        let (day, _) = local_parts(next, 120);
        assert_eq!(weekday_of(day), 0, "it must be Monday *locally*");
    }

    #[test]
    fn previous_is_the_inverse_of_next() {
        for trigger in [
            Trigger::Every { secs: 900 },
            Trigger::Daily {
                minute_of_day: 180,
                days: 0b0001_1111,
                utc_offset_minutes: -300,
            },
        ] {
            let next = trigger.next_after(MON_MIDNIGHT).expect("next");
            assert_eq!(
                trigger.previous_at_or_before(next),
                Some(next),
                "an occurrence is its own most-recent occurrence"
            );
            assert!(
                trigger.previous_at_or_before(next - 1).expect("previous") < next,
                "one microsecond earlier must land on the one before"
            );
        }
    }

    #[test]
    fn a_schedule_only_names_instants_its_expression_yields() {
        let s = sample(Trigger::Daily {
            minute_of_day: 180,
            days: 0b0001_1111,
            utc_offset_minutes: 0,
        });
        let real = s.when.next_after(MON_MIDNIGHT).expect("next");
        assert!(s.names_occurrence(real));
        // A minute either side is not an occurrence, and this is the check
        // that stops a stolen plan being replayed at an arbitrary time.
        assert!(!s.names_occurrence(real + MINUTE));
        assert!(!s.names_occurrence(real - MINUTE));
        // Neither is the same time on a Saturday.
        assert!(!s.names_occurrence(real + 5 * DAY));
    }

    #[test]
    fn a_trigger_that_can_never_fire_says_so() {
        let never = Trigger::Daily {
            minute_of_day: 180,
            days: 0,
            utc_offset_minutes: 0,
        };
        assert_eq!(never.next_after(MON_MIDNIGHT), None);
        assert!(never.upcoming(MON_MIDNIGHT, 5).is_empty());
    }

    #[test]
    fn signing_covers_everything_including_paused() {
        let a = sample(Trigger::Every { secs: 900 });
        let mut b = a.clone();
        b.paused = true;
        assert_ne!(
            a.signing_bytes(),
            b.signing_bytes(),
            "pausing must be a signed act, not a field anyone can flip"
        );
    }

    #[test]
    fn descriptions_read_the_way_a_person_wrote_them() {
        assert_eq!(Trigger::Every { secs: 900 }.describe(), "every 15m");
        assert_eq!(Trigger::Every { secs: 7200 }.describe(), "every 2h");
        assert_eq!(
            Trigger::Daily {
                minute_of_day: 180,
                days: 0b0001_1111,
                utc_offset_minutes: 120
            }
            .describe(),
            "03:00 mon-fri +02:00"
        );
    }

    fn sample(when: Trigger) -> Schedule {
        Schedule {
            name: "nightly".into(),
            workflow: "deploy".into(),
            workflow_version: None,
            inputs: Vec::new(),
            when,
            catch_up: CatchUp::Skip,
            grace_secs: DEFAULT_GRACE_SECS,
            overlap: Overlap::Skip,
            paused: false,
            priority: JobPriority::Normal,
            timeout_secs: None,
            description: String::new(),
            created_by: UserId([7; 32]),
            created_at_micros: MON_MIDNIGHT,
            signature: vec![0; 64],
        }
    }
}

// ---------------------------------------------------------------------------
// N1 P5 — cron, standalone: a schedule that fires a service
// ---------------------------------------------------------------------------

/// Job environment key naming the schedule that fired a job.
pub const SCHEDULE_ENV: &str = "MESHWARE_SCHEDULE";
/// Job environment key carrying the occurrence instant, in unix micros.
///
/// The instant the expression **named**, never the moment a node noticed it
/// was due — so a job can log the time it was meant to run and two nodes
/// that disagree about the clock still agree about this.
pub const SCHEDULE_OCCURRENCE_ENV: &str = "MESHWARE_SCHEDULE_OCCURRENCE";

/// A schedule that fires a **service**, not a workflow (N1 P5).
///
/// ## Why this is a new record rather than an edit to [`Schedule`]
///
/// Cron was removed on 2026-09-12 for one reason: it could fire nothing but
/// a workflow, and the workflow engine went. Everything else it needed
/// survived intact — rendezvous election, derived occurrence ids,
/// catch-up, overlap, the freeze-window check — so this phase is the
/// trigger arithmetic reattached to the thing the product still has.
///
/// [`Schedule`] itself stays exactly where it is, unedited, and its op
/// variants stay retired at indices 44–45. It is a signed struct: growing
/// it would break every signature ever made over one and make an older node
/// misdecode every record from here on (the argument
/// [`crate::DeploymentPolicy`] documents, measured). A record whose *target*
/// is a different kind of thing is not a field change anyway — it is a
/// different record, and this is it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSchedule {
    /// Unique among service schedules. The handle every command, audit
    /// entry and job correlation uses.
    pub name: String,
    pub service: String,
    /// A pinned version, or `None` for whatever the head is at fire time —
    /// matching what a person typing `service run <name>` gets.
    ///
    /// **Unlike a deployment, a floating head is allowed here.** A
    /// deployment is what is supposed to stay up and cannot be a moving
    /// target; a nightly job is a thing that happens once and is allowed
    /// to be whatever the current version is when it does.
    pub service_version: Option<u32>,
    /// Inputs, validated against the service's contract at create time.
    /// The point of a schedule is that nobody is watching when it runs, so
    /// a schema error has to surface while somebody is.
    pub inputs: Vec<(String, String)>,
    /// Where it may run, in the selector grammar, as the **string** — so a
    /// new selector form is free and degrades closed on an old node, the
    /// same rule `Deployment::targets` follows.
    pub targets: String,
    pub when: Trigger,
    pub catch_up: CatchUp,
    /// How far past an occurrence [`CatchUp::Once`] will still fire it.
    pub grace_secs: u64,
    pub overlap: Overlap,
    /// Paused schedules stay in state, keep their history, and fire
    /// nothing. Pausing is an edit, so it re-signs the record.
    pub paused: bool,
    pub priority: JobPriority,
    pub timeout_secs: Option<u64>,
    pub description: String,
    /// The user whose authority every fire carries.
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Ed25519 over [`Self::signing_bytes`], verified at merge time on
    /// every node **and again on the executor at every fire**.
    pub signature: Vec<u8>,
}

impl ServiceSchedule {
    /// The bytes the creator's signature covers: the whole record with the
    /// signature blanked. `paused` included — a schedule is only ever
    /// changed by somebody who owns it, and re-signing is what makes that
    /// provable.
    ///
    /// A distinct domain tag from [`Schedule::signing_bytes`], so a
    /// signature over one can never be replayed as the other.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-service-schedule-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// Is `occurrence_micros` an instant this schedule actually names?
    ///
    /// The anti-replay check the executor applies at every fire: a job
    /// whose id was derived from an instant the expression does not yield
    /// is not something this schedule could have produced, whoever signed
    /// it.
    pub fn names_occurrence(&self, occurrence_micros: u64) -> bool {
        self.when
            .next_after(occurrence_micros.saturating_sub(1))
            .is_some_and(|next| next == occurrence_micros)
    }

    /// `service` as a person writes it: `backup` or `backup@4`.
    pub fn service_ref(&self) -> String {
        match self.service_version {
            Some(v) => format!("{}@{v}", self.service),
            None => self.service.clone(),
        }
    }
}

/// Signing bytes for a service-schedule removal. Distinct tag from the
/// record's, so a put signature can never be replayed as a remove.
pub fn service_schedule_remove_signing_bytes(name: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-service-schedule-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&by.0);
    buf
}

/// One schedule, as a list or detail view sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceScheduleRow {
    pub name: String,
    /// `backup@4`, as written.
    pub service: String,
    pub targets: String,
    /// The trigger in the words the file used: `every 10m`, `daily 03:00`.
    pub when: String,
    /// `active`, `paused`, or `orphaned` (the service it names is gone).
    pub status: String,
    /// The next instant it names, or `None` when it can never fire again.
    pub next_micros: Option<u64>,
    /// The most recent occurrence that actually fired.
    pub last_micros: Option<u64>,
    /// The node currently elected to fire it, if any is live.
    pub firer: Option<String>,
    pub created_by_username: String,
    pub created_at_micros: u64,
    /// The record as TOML, present on a detail view so an editor and
    /// `--output toml` render from one fetch.
    pub source: Option<String>,
}

/// The answer to a dry-run schedule put: what the record parsed to, when it
/// would next fire, and what is wrong with it (N1 P5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceScheduleCheck {
    pub name: String,
    pub service: String,
    pub targets: String,
    pub when: String,
    /// Nodes the selector matches right now.
    pub matching_nodes: u32,
    /// The next instant it names, or `None` when it can never fire.
    pub next_micros: Option<u64>,
    pub issues: Vec<String>,
}
