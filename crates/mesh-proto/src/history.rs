//! Query shapes for local job history.
//!
//! These are deliberately *summary* rows, not the full replicated records.
//! A job's command, its outputs and its logs all still live where they
//! lived before — in the op-log, on the executing node, in the object
//! store. What this adds is the ability to *find* a job without walking
//! every job the mesh has ever run, and finding is a different job from
//! reading.
//!
//! The split matters for the same reason the rest of the system is shaped
//! the way it is: an index is derived data. Every field here can be
//! recomputed from the op-log, so the index can be deleted, rebuilt, or
//! disagreed with by two nodes without anything being *wrong*. Nothing here
//! is ever gossiped, and no node's index is authoritative for another's.

use serde::{Deserialize, Serialize};

use crate::Hlc;
use crate::ids::{JobId, NodeId, UserId};
use crate::ops::JobState;

/// One job, flattened the same way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryJobRow {
    pub job_id: JobId,
    pub cmd_preview: String,
    pub runtime: String,
    pub submitter: NodeId,
    pub submitter_user: UserId,
    pub executor: NodeId,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub created_at_micros: u64,
    pub started_at_micros: Option<u64>,
    pub finished_at_micros: Option<u64>,
    pub updated_hlc: Hlc,
    /// Where a sink put this job's output, and under what key (M31 P0's
    /// reserved columns, filled by B2).
    ///
    /// **On the index row, not only in the op-log**, and that is §7's third
    /// crossing rather than a convenience: A1 prunes the op-log row long
    /// before anyone asks the six-month question, so a locator that lived
    /// only there would be gone exactly when it was needed. From here
    /// `mesh-archive` copies it into the Parquet row on its next ingest.
    #[serde(default)]
    pub payload_sink: Option<String>,
    #[serde(default)]
    pub payload_key: Option<String>,
    /// True when this row was read out of the Parquet archive rather than
    /// off the local index — the slow tier, which the caller labels rather
    /// than silently mixing in.
    #[serde(default)]
    pub from_archive: bool,
    /// Where this job's output is **right now** (M31 A2, and §14.3 item 7's
    /// fourth state).
    ///
    /// Computed when a page is answered, never stored: it is a fact about
    /// the present — a node that was asleep when the page was built may be
    /// awake by the time anybody clicks. `None` is "nobody worked it out",
    /// which is what a raw index read returns and what a caller renders as
    /// nothing rather than as a claim.
    ///
    /// **The sink's existence never softens `Expired`.** A record whose
    /// payload was not exported and whose local copy has aged out says
    /// expired, with a destination configured and reachable, because a
    /// destination that exists is not a promise it took these bytes.
    #[serde(default)]
    pub output: Option<crate::OutputAvailability>,
}

impl HistoryJobRow {
    pub fn duration_micros(&self) -> Option<u64> {
        let start = self.started_at_micros.unwrap_or(self.created_at_micros);
        self.finished_at_micros.map(|end| end.saturating_sub(start))
    }

    pub fn sort_micros(&self) -> u64 {
        self.finished_at_micros.unwrap_or(self.created_at_micros)
    }
}

/// What to look for. Every field is optional and they compose with AND;
/// an all-default query is "the most recent [`HistoryQuery::limit`] of
/// them", which is what a UI opens with.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HistoryQuery {
    /// A terminal state name: `success`, `failed`, `cancelled`, `running`,
    /// … Matched case-insensitively against the state's own label, so the
    /// CLI and the UI never have to carry a mapping table.
    pub state: Option<String>,
    /// The user who submitted it.
    pub user: Option<UserId>,
    /// The node that executed it.
    pub node: Option<NodeId>,
    /// Inclusive lower bound on [`HistoryJobRow::sort_micros`].
    pub since_micros: Option<u64>,
    /// Inclusive upper bound.
    pub until_micros: Option<u64>,
    /// Case-insensitive substring over the command preview. Applied after
    /// the index narrows the set — this is a convenience filter, not a
    /// search engine.
    pub contains: Option<String>,
    /// Rows to return. 0 is read as [`HistoryQuery::DEFAULT_LIMIT`].
    pub limit: u32,
    /// Rows to skip, for paging.
    pub offset: u32,
}

impl HistoryQuery {
    pub const DEFAULT_LIMIT: u32 = 50;
    /// Hard ceiling per request, so one query cannot decide to materialize
    /// the whole index into an IPC frame.
    pub const MAX_LIMIT: u32 = 5000;

    /// The limit actually applied.
    pub fn effective_limit(&self) -> usize {
        let n = if self.limit == 0 {
            Self::DEFAULT_LIMIT
        } else {
            self.limit
        };
        n.min(Self::MAX_LIMIT) as usize
    }

    /// True when nothing narrows the set — the "just show me the latest"
    /// case, which reads straight off the recency index.
    pub fn is_unfiltered(&self) -> bool {
        self.state.is_none()
            && self.user.is_none()
            && self.node.is_none()
            && self.since_micros.is_none()
            && self.until_micros.is_none()
            && self.contains.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryJobPage {
    pub rows: Vec<HistoryJobRow>,
    pub total: Option<u64>,
    /// Whether an older tier existed and could not be reached, so a caller
    /// renders "nothing found here" rather than "nothing ever happened".
    #[serde(default)]
    pub absence: crate::KnownAbsence,
    /// Which tiers answered this page, and what they could not reach.
    #[serde(default)]
    pub tiers: TierReport,
}

/// Which tiers answered a browse, and what they could not reach
/// (M31 B3 §14.2).
///
/// **Every row says which tier answered it, and the slow tier is visibly
/// slower rather than silently absent.** That is §3's Temporal lesson
/// stated as UI: cheap-to-store and cheap-to-query are different tiers, and
/// the screen that pretends otherwise is the screen that hangs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierReport {
    /// Rows from `history.redb` — instant, recent.
    pub from_index: u64,
    /// Rows from Parquet — complete, slower.
    pub from_archive: u64,
    /// The peer whose archive answered, when it was not this node. Empty
    /// for a local archive or none at all.
    pub archive_peer: String,
    /// True when this node has no archive and no peer answered for it —
    /// so the window shown is the index's, and older rows exist nowhere
    /// this browse could see. **Not the same as "nothing matched"**, and a
    /// caller that conflates them reports data loss.
    pub archive_unreachable: bool,
    /// Why the archive did not answer, verbatim. Empty when it did, or
    /// when there is simply none.
    pub archive_status: String,
}

/// What the local index holds. Answers "is this worth archiving yet" and
/// "how much am I keeping".
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HistoryStats {
    pub jobs: u64,
    /// Oldest and newest indexed instants.
    pub oldest_micros: Option<u64>,
    pub newest_micros: Option<u64>,
    /// Size of `state/history.redb` on disk.
    pub index_bytes: u64,
}

/// The rule [`HistoryQuery::state`] matches by.
///
/// Three things beyond a case-insensitive compare, each of them a place a
/// person would otherwise get an empty result and blame themselves:
/// `failed` finds `failed (compensated)`, because a compensated run *did*
/// fail; the everyday synonyms work (`success`, `ok`, `timeout`); and
/// British and American spellings of "cancelled" both land.
pub fn state_matches(label: &str, wanted: &str) -> bool {
    let wanted = wanted.trim().to_ascii_lowercase();
    let label = label.to_ascii_lowercase();
    if label == wanted {
        return true;
    }
    let head = label.split_whitespace().next().unwrap_or(label.as_str());
    if head == wanted {
        return true;
    }
    matches!(
        (head, wanted.as_str()),
        ("succeeded", "success" | "ok" | "passed" | "green")
            | ("failed", "fail" | "failure" | "error" | "red")
            | ("cancelled", "canceled" | "cancel")
            | ("timed-out", "timeout" | "timedout" | "timed_out")
            | ("running", "active" | "in-flight" | "inflight")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_clamped_not_trusted() {
        let q = HistoryQuery {
            limit: 0,
            ..Default::default()
        };
        assert_eq!(q.effective_limit(), HistoryQuery::DEFAULT_LIMIT as usize);

        let q = HistoryQuery {
            limit: u32::MAX,
            ..Default::default()
        };
        assert_eq!(q.effective_limit(), HistoryQuery::MAX_LIMIT as usize);
    }

    #[test]
    fn state_filters_forgive_the_obvious_spellings() {
        assert!(state_matches(JobState::Succeeded.as_str(), "success"));
        assert!(state_matches(JobState::Succeeded.as_str(), "OK"));
        assert!(state_matches(JobState::Cancelled.as_str(), "canceled"));
        assert!(state_matches(JobState::TimedOut.as_str(), "timeout"));
        assert!(!state_matches(JobState::Succeeded.as_str(), "failed"));
        assert!(!state_matches(JobState::Running.as_str(), "succeeded"));
    }

    #[test]
    fn an_empty_query_is_the_recent_list() {
        assert!(HistoryQuery::default().is_unfiltered());
        assert!(
            !HistoryQuery {
                contains: Some("deploy".into()),
                ..Default::default()
            }
            .is_unfiltered()
        );
    }
}
