//! Where a job's output is, and why you cannot see it (M31 A2).
//!
//! ## The rule this file exists to enforce
//!
//! **Losing the record is unacceptable; losing the payload is routine**
//! (M31 §4 decision 9). A run from 2024 whose output aged out reads
//! *"output expired"*. It does not read *"run not found"*, it does not
//! read *"failed"*, and it does not disappear.
//!
//! That distinction is the difference between a system people trust and one
//! they work around. An operator who has been told a successful job failed —
//! which is what the product did before this, because a missing `status.json`
//! fell through to a fabricated [`crate::JobState::Failed`] — does not go on
//! to believe *"expired"* either.
//!
//! ## Why it is an enum and not a boolean
//!
//! Because *gone* and *not here right now* are different facts with
//! different actions, and conflating them poisons both:
//!
//! - **Expired** is terminal. The bytes are not coming back, and the only
//!   remaining copy is wherever the operator chose to send one.
//! - **Unavailable** is temporary. The producing node is asleep, and the
//!   output returns when it does. Saying *"expired"* for a machine that is
//!   merely offline teaches operators the states are approximate.
//!
//! There is a third reason for the enum, and it is the one that costs
//! nothing now and everything later: **B3 adds a fourth variant**
//! ([`OutputAvailability::Elsewhere`], already here and already rendered).
//! A boolean would have meant touching every renderer twice.

use serde::{Deserialize, Serialize};

use crate::NodeId;

/// Why output that once existed is no longer readable.
///
/// Carried alongside the instant rather than folded into a sentence,
/// because a UI needs to *style* these differently and a script needs to
/// branch on them. A pre-rendered string can do neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpiryReason {
    /// `[retention]` pruned it on the producing node — the ordinary path,
    /// and the one an operator configured themselves.
    Retention,
    /// A declared output over [`crate::INLINE_OUTPUT_MAX`] spilled to the
    /// object store and its 15-minute TTL passed (M9 §5.5). Objects are
    /// *transport*, not storage; this is them working as designed.
    ObjectTtl,
    /// The node that held it has left the mesh or been revoked. Distinct
    /// from `Unavailable` because it is not coming back: nothing about
    /// waiting will help.
    ProducerGone,
}

impl ExpiryReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExpiryReason::Retention => "retention",
            ExpiryReason::ObjectTtl => "object-ttl",
            ExpiryReason::ProducerGone => "producer-gone",
        }
    }

    /// The half-sentence that follows *"output expired"*.
    pub fn explain(self) -> &'static str {
        match self {
            ExpiryReason::Retention => {
                "the node that ran it keeps job output for a limited time (`[retention]`)"
            }
            ExpiryReason::ObjectTtl => {
                "large declared outputs are transport, not storage, and this one's window passed"
            }
            ExpiryReason::ProducerGone => "the node that ran it is no longer in this mesh",
        }
    }
}

/// Where a job's stdout and stderr are, from the asking node's point of
/// view.
///
/// **Resolved per read, never stored.** It is a statement about the world
/// right now — which node is up, what this node holds — and caching it
/// would mean serving an answer about a machine that has since come back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputAvailability {
    /// The bytes are readable, here or from a reachable producer.
    Available,
    /// The producing node is not reachable right now. **Temporary**, and
    /// the name is carried so the message can say which machine to go and
    /// look at rather than printing a 52-character node id.
    Unavailable { node: NodeId, node_name: String },
    /// The bytes are gone. `when_micros` is when they became eligible to
    /// go — computed from the finish instant and the window, so it is an
    /// honest *"at or after this date"* rather than a fabricated deletion
    /// timestamp nothing recorded.
    Expired {
        when_micros: u64,
        reason: ExpiryReason,
    },
    /// A sink has a copy and it can be fetched by id (M31 B3).
    ///
    /// **Declared now, populated in B3**, exactly as P0 reserved the
    /// archive's locator columns: the variant costs nothing today and every
    /// renderer already handles it, so B3 is a fetch path rather than a
    /// second pass over every screen in the product.
    Elsewhere { sink: String, key: String },
}

impl OutputAvailability {
    /// Is there anything to read?
    pub fn is_available(&self) -> bool {
        matches!(self, OutputAvailability::Available)
    }

    /// Will waiting help?
    ///
    /// The question a retry loop and a "try again" button both need, and
    /// the one a boolean `available` could not answer.
    pub fn may_return(&self) -> bool {
        matches!(
            self,
            OutputAvailability::Available | OutputAvailability::Unavailable { .. }
        )
    }

    /// A stable tag for scripts and for `--output json`.
    pub fn as_str(&self) -> &'static str {
        match self {
            OutputAvailability::Available => "available",
            OutputAvailability::Unavailable { .. } => "unavailable",
            OutputAvailability::Expired { .. } => "expired",
            OutputAvailability::Elsewhere { .. } => "elsewhere",
        }
    }

    /// The one sentence every reader prints.
    ///
    /// **One string, one meaning, defined once.** The rule A2 exists for is
    /// exactly the kind that holds in three renderers and fails in the
    /// fourth, so no caller composes its own — `meshnode job show`, the log
    /// stream and Director both render this function's output.
    ///
    /// `now_micros` is only used to date the expiry; pass the caller's
    /// clock so a test can pin it.
    pub fn message(&self) -> String {
        match self {
            OutputAvailability::Available => String::new(),
            OutputAvailability::Unavailable { node, node_name } => format!(
                "output unavailable — node `{}` is offline. The record is complete; the log \
                 comes back when that machine does.",
                if node_name.is_empty() {
                    node.short()
                } else {
                    node_name.clone()
                }
            ),
            OutputAvailability::Expired {
                when_micros,
                reason,
            } => format!(
                "output expired {} — {}. The record below is complete.",
                crate::availability::day_of(*when_micros),
                reason.explain()
            ),
            OutputAvailability::Elsewhere { sink, .. } => {
                format!("output is in `{sink}` — open it from there, or click through to fetch it.")
            }
        }
    }

    /// The short form for a table cell, where a sentence will not fit.
    pub fn short(&self) -> String {
        match self {
            OutputAvailability::Available => "—".into(),
            OutputAvailability::Unavailable { node, node_name } => format!(
                "unavailable ({})",
                if node_name.is_empty() {
                    node.short()
                } else {
                    node_name.clone()
                }
            ),
            OutputAvailability::Expired { when_micros, .. } => {
                format!("expired {}", day_of(*when_micros))
            }
            OutputAvailability::Elsewhere { sink, .. } => format!("in {sink}"),
        }
    }
}

/// `YYYY-MM-DD` in UTC, for the one date these messages carry.
///
/// Local to this module and deliberately not the CLI's formatter: this
/// string crosses IPC into Director, so it must not depend on
/// the timezone of whichever process happened to render it. A run did not
/// expire on a different day because somebody flew to Berlin.
pub fn day_of(micros: u64) -> String {
    if micros == 0 {
        return "at some point".to_string();
    }
    let days = (micros / 1_000_000) as i64 / 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Why a query returned fewer rows than the window asked for (M31 A2
/// item 4).
///
/// **An empty list that means *"we threw it away on the schedule you set"*
/// and an empty list that means *"nothing matched"* must never look
/// alike.** That is the expired-versus-not-found rule one level up, and it
/// is what makes a retention window a decision an operator can live with
/// rather than a hole they trip into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KnownAbsence {
    /// The oldest instant this mesh can still answer for. Rows before it
    /// were dropped by the retention window, not missing by accident.
    pub retained_from_micros: u64,
    /// True when the caller's window actually reached past it — so a
    /// caller can render the note only when it applies rather than
    /// explaining retention to somebody asking about yesterday.
    pub window_reached_past: bool,
}

impl KnownAbsence {
    /// The note to print under an empty or short result, if there is one.
    pub fn note(&self) -> Option<String> {
        self.window_reached_past.then(|| {
            format!(
                "runs and jobs before {} are not in this mesh any more — the retention window \
                 dropped them (`meshnode retention show`). This is not an empty result; it is \
                 a known absence.",
                day_of(self.retained_from_micros)
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400 * 1_000_000;
    /// 2026-06-07T00:00:00Z — the date in A2's own exit demo.
    const JUNE_7: u64 = 1_780_790_400_000_000;

    #[test]
    fn expired_and_unavailable_are_different_values_not_different_prose() {
        let expired = OutputAvailability::Expired {
            when_micros: JUNE_7,
            reason: ExpiryReason::Retention,
        };
        let offline = OutputAvailability::Unavailable {
            node: NodeId([7u8; 32]),
            node_name: "worker-7".into(),
        };
        assert_ne!(expired, offline);
        assert_eq!(expired.as_str(), "expired");
        assert_eq!(offline.as_str(), "unavailable");

        // The property a retry loop and a "try again" button both need, and
        // the one a boolean `available` could not have answered.
        assert!(!expired.may_return());
        assert!(offline.may_return());
        assert!(OutputAvailability::Available.may_return());
    }

    #[test]
    fn the_messages_say_which_of_the_two_happened() {
        let msg = OutputAvailability::Expired {
            when_micros: JUNE_7,
            reason: ExpiryReason::Retention,
        }
        .message();
        assert!(msg.contains("output expired 2026-06-07"), "{msg}");
        assert!(msg.contains("retention"), "{msg}");
        // Decision 9, in the sentence itself: the record survived.
        assert!(msg.contains("record below is complete"), "{msg}");

        let msg = OutputAvailability::Unavailable {
            node: NodeId([7u8; 32]),
            node_name: "worker-7".into(),
        }
        .message();
        assert!(msg.contains("worker-7"), "{msg}");
        assert!(msg.contains("offline"), "{msg}");
        assert!(msg.contains("comes back"), "{msg}");
        // And it must not claim the thing that is not true.
        assert!(!msg.contains("expired"), "{msg}");
    }

    /// A node with no name falls back to its short id rather than printing
    /// an empty backtick pair.
    #[test]
    fn a_nameless_node_still_names_itself() {
        let msg = OutputAvailability::Unavailable {
            node: NodeId([7u8; 32]),
            node_name: String::new(),
        }
        .message();
        assert!(!msg.contains("``"), "{msg}");
        assert!(msg.contains(&NodeId([7u8; 32]).short()), "{msg}");
    }

    /// B3's variant is already here and already renders, which is the whole
    /// reason this is an enum rather than a boolean.
    #[test]
    fn the_b3_variant_renders_without_touching_any_renderer_again() {
        let a = OutputAvailability::Elsewhere {
            sink: "warehouse".into(),
            key: "runs/2026/06/07/job-abc".into(),
        };
        assert_eq!(a.as_str(), "elsewhere");
        assert!(a.message().contains("warehouse"));
        assert!(a.short().contains("warehouse"));
        // Not available *here*, and not gone either.
        assert!(!a.is_available());
        assert!(!a.may_return());
    }

    #[test]
    fn every_reason_explains_itself() {
        for reason in [
            ExpiryReason::Retention,
            ExpiryReason::ObjectTtl,
            ExpiryReason::ProducerGone,
        ] {
            assert!(!reason.as_str().is_empty());
            assert!(
                reason.explain().len() > 20,
                "{} explains nothing",
                reason.as_str()
            );
        }
        // The one that must not read as temporary.
        assert!(
            !ExpiryReason::ProducerGone.explain().contains("offline"),
            "a departed node must not read like a sleeping one"
        );
    }

    #[test]
    fn a_known_absence_is_a_different_value_from_an_empty_result() {
        let nothing_matched = KnownAbsence::default();
        assert_eq!(nothing_matched.note(), None);

        let dropped = KnownAbsence {
            retained_from_micros: JUNE_7,
            window_reached_past: true,
        };
        let note = dropped.note().expect("a reached-past window has a note");
        assert!(note.contains("2026-06-07"), "{note}");
        assert!(note.contains("known absence"), "{note}");
        assert!(note.contains("retention"), "{note}");
        // The two are not equal, which is the assertion the doc asks for.
        assert_ne!(nothing_matched, dropped);
    }

    #[test]
    fn dates_are_utc_and_do_not_move_with_the_reader() {
        assert_eq!(day_of(JUNE_7), "2026-06-07");
        assert_eq!(day_of(JUNE_7 + DAY - 1), "2026-06-07");
        assert_eq!(day_of(JUNE_7 + DAY), "2026-06-08");
        assert_eq!(day_of(0), "at some point");
    }
}
