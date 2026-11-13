//! Mesh-wide retention: how long the replicated tier keeps closed history
//! (M31 A1).
//!
//! ## Why this is a replicated entity and not a config file
//!
//! `[retention]` in `meshnode.toml` already bounds one machine's disk: it
//! walks the local `jobs_dir` and prunes stdout past thirty days. It touches
//! no replicated byte, and it never could — a node's config is a statement
//! about that node.
//!
//! What C0 left unbounded is the other tier. A finished job's last
//! `JobUpsert` supersedes nothing and is superseded by nothing, so it
//! survives compaction for ever, correctly, under rules that are working
//! exactly as designed. One op per job, on every node, permanently. That is
//! the wall M31 §1 measures, and closing it needs a **window**, not a
//! cleverer compactor.
//!
//! The window has to be replicated, and the reason is C9 rather than
//! convenience: *two nodes must answer every question identically*. A
//! horizon read from node config would mean a run visible on one machine and
//! absent on its neighbour, and a history that depends on who you ask is not
//! one. C0 P3 learned this for the usage decay horizon and named it
//! explicitly; this is the same fix applied to the entity C0 deliberately
//! left alone.
//!
//! ## The two numbers, and why both exist
//!
//! Each table carries a `default` and a `floor`, and the resolution rule is
//! **the mesh policy is the default and a node may keep less, but not
//! more**:
//!
//! ```text
//! effective = clamp(node_config_or_default, floor, default)
//! ```
//!
//! The default protects the mesh from unbounded growth. The floor protects
//! an auditor from a node that quietly shortened its own window. A floor of
//! zero — the shipped value for every table but `audit` — means "no floor",
//! and the two together behave exactly like a plain default until somebody
//! sets one. The machine with the small disk is a real machine, and a
//! mesh-wide policy that fills it is a support ticket.
//!
//! ## Finite by default
//!
//! Every knob here ships with a number in it (M31 §4 decision 7). A product
//! whose default is *"keep everything"* teaches every operator that storage
//! is free until the day it is not, so `0 = forever` is a thing an operator
//! has to **type**. A fresh mesh is bounded before anyone configures
//! anything — and so is one that has merely been upgraded, which is the case
//! that matters, since the meshes with the growth problem are the old ones.
//!
//! ## The audit floor
//!
//! `audit` ships as `forever` and may be set to a finite window, but never
//! one below [`AUDIT_FLOOR_DAYS`]. An audit log you can shorten to nothing
//! is not an audit log. Under `forever` an audit row never leaves the
//! op-log; under a finite window it leaves once older than the window, on
//! every node, and the history index and the in-memory tail follow (ENG-3).

use serde::{Deserialize, Serialize};

use crate::UserId;

/// The shortest audit window an admin may sign, in days (M31 §4 decision 8).
///
/// A year. Long enough to outlive every compliance conversation the number
/// exists for, and short enough that it is a *floor* rather than a policy in
/// disguise. The refusal names it, because a refusal that does not say what
/// the limit is teaches nothing.
pub const AUDIT_FLOOR_DAYS: u32 = 365;

/// Retention defaults, all finite (M31 §4 decision 7, A1 item 3).
pub const DEFAULT_RUNS_DAYS: u32 = 90;
pub const DEFAULT_FACTS_DAYS: u32 = 365;
pub const DEFAULT_JOB_OUTPUT_DAYS: u32 = 30;
pub const DEFAULT_ARCHIVE_DAYS: u32 = 5 * 365;

/// Days an unreferenced bundle stays in the local content store (M31 A2).
///
/// Thirty, the same number `job-output` uses, and picked the same way: long
/// enough that last month's deploy is still a local rollback rather than a
/// refetch, short enough that a build farm publishing nightly does not keep
/// a year of them. The store this bounds is the one that actually grows
/// without limit — nineteen releases of a 30 MB binary is 571 MB of chunks
/// that dedup cannot touch, because a relinked binary shares no 1 MiB
/// boundary with the one before it.
pub const DEFAULT_CONTENT_DAYS: u32 = 30;

/// Days meaning "never drop this". An operator has to type it.
pub const FOREVER: u32 = 0;

/// Which retention window governs a thing.
///
/// An enum rather than a string key, for the same reason `schedule::Trigger`
/// is one: a table added later is an **appended variant** that the exhaustive
/// matches below refuse to compile without, rather than a string somebody
/// forgets to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RetentionTable {
    /// Replicated run and job records — the tier C0 leaves unbounded, and
    /// the only one A1's compaction class actually reads.
    Runs,
    /// Fact history.
    Facts,
    /// Producer-local `stdout.log` / `stderr.log`. Surfaced here so an
    /// operator sees one retention story rather than two; enforced by
    /// `sweep_retention` against the *resolved* window, which is where
    /// "a node may keep less but not more" is applied.
    JobOutput,
    /// The audit log. `forever` by default; a finite window, never below
    /// the floor, retires rows from the op-log (ENG-3).
    Audit,
    /// Parquet, cold. Nothing in the node enforces it — the archive is the
    /// operator's directory — but it is the number `meshware-archive` will
    /// prune against, and it belongs in the same screen.
    Archive,
    /// Local content-store bundles: the chunks under `content/blobs`.
    ///
    /// Node-local bytes, like `JobOutput`, and here for the same reason —
    /// one retention screen rather than two. Enforced by
    /// `ContentStore::sweep` against the *resolved* window, and only ever
    /// against bundles nothing in replicated state still needs: a window
    /// does not outrank a running deployment.
    Content,
}

impl RetentionTable {
    pub const ALL: [RetentionTable; 6] = [
        RetentionTable::Runs,
        RetentionTable::Facts,
        RetentionTable::JobOutput,
        RetentionTable::Audit,
        RetentionTable::Archive,
        RetentionTable::Content,
    ];

    /// The name an operator types and reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            RetentionTable::Runs => "runs",
            RetentionTable::Facts => "facts",
            RetentionTable::JobOutput => "job-output",
            RetentionTable::Audit => "audit",
            RetentionTable::Archive => "archive",
            RetentionTable::Content => "content",
        }
    }

    pub fn parse(s: &str) -> Option<RetentionTable> {
        RetentionTable::ALL
            .into_iter()
            .find(|t| t.as_str().eq_ignore_ascii_case(s))
    }

    /// The shipped default for this table, in days.
    pub fn default_days(self) -> u32 {
        match self {
            RetentionTable::Runs => DEFAULT_RUNS_DAYS,
            RetentionTable::Facts => DEFAULT_FACTS_DAYS,
            RetentionTable::JobOutput => DEFAULT_JOB_OUTPUT_DAYS,
            RetentionTable::Audit => FOREVER,
            RetentionTable::Archive => DEFAULT_ARCHIVE_DAYS,
            RetentionTable::Content => DEFAULT_CONTENT_DAYS,
        }
    }

    /// The shortest window that may ever be signed for this table.
    ///
    /// Zero for everything but the audit log, which is the one place the
    /// product takes the decision out of the operator's hands.
    pub fn floor_days(self) -> u32 {
        match self {
            RetentionTable::Audit => AUDIT_FLOOR_DAYS,
            _ => 0,
        }
    }

    /// Does compaction ever drop this table's ops from the replicated log?
    ///
    /// `Runs` (job records, and closed fan-outs and gangs that were never
    /// adopted) and `Audit` (under a finite window). Saying so here rather
    /// than in `compact.rs` keeps the two halves of A1 from drifting: the
    /// class that drops ops and the policy that authorizes it name the same
    /// tables.
    pub fn bounds_the_op_log(self) -> bool {
        matches!(self, RetentionTable::Runs | RetentionTable::Audit)
    }
}

/// One table's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionWindow {
    /// Days. `0` is [`FOREVER`], and an operator has to type it.
    pub days: u32,
    /// The shortest window a node may resolve to. `0` means no floor.
    ///
    /// Carried alongside the default rather than instead of it, because the
    /// two answer different questions — *"how long does this mesh keep
    /// things"* and *"what may nobody go below"* — and a policy that could
    /// only express one of them would be argued about by the operator with
    /// the small disk and the officer with the retention schedule, for ever.
    pub floor_days: u32,
}

impl RetentionWindow {
    pub fn forever() -> RetentionWindow {
        RetentionWindow {
            days: FOREVER,
            floor_days: 0,
        }
    }

    pub fn is_forever(self) -> bool {
        self.days == FOREVER
    }

    /// Micros before `now` at which this window's contents stop being kept.
    /// `None` when the window is forever.
    /// This window's length in microseconds; `None` for `forever`.
    ///
    /// For the callers that need the *span* rather than a cutoff — a
    /// compaction class, which is a pure function of the op and must not
    /// read a clock, and so expresses an expiry as "the instant this record
    /// describes, plus the window".
    pub fn span_micros(self) -> Option<u64> {
        (!self.is_forever()).then(|| u64::from(self.days) * 86_400 * 1_000_000)
    }

    pub fn cutoff_micros(self, now_micros: u64) -> Option<u64> {
        (!self.is_forever())
            .then(|| now_micros.saturating_sub(u64::from(self.days) * 86_400 * 1_000_000))
    }

    /// Apply a node's own preference to this window (Q8, decided: the mesh
    /// policy is the default and a node may keep **less** but not more).
    ///
    /// `node_days` of `None` means the node said nothing and takes the
    /// default. A node asking for longer than the mesh policy is clamped
    /// down; a node asking for shorter than the floor is clamped up.
    /// Deliberately a clamp rather than a refusal: this runs on every
    /// retention sweep on every node, and a refusal there would be a config
    /// error discovered at 3am by nothing pruning.
    pub fn resolve_for_node(self, node_days: Option<u32>) -> u32 {
        let want = match node_days {
            None => self.days,
            // A node saying `forever` where the mesh says otherwise is
            // asking to keep *more*, which is exactly what the rule
            // forbids — so it takes the mesh default rather than winning.
            Some(FOREVER) if !self.is_forever() => self.days,
            Some(d) => d,
        };
        if self.is_forever() && node_days.is_none() {
            return FOREVER;
        }
        // Clamp to the mesh default above and the floor below. With a
        // forever default there is no upper bound to clamp against.
        let upper = if self.is_forever() {
            want
        } else {
            want.min(self.days)
        };
        upper.max(self.floor_days)
    }
}

/// The mesh's retention policy: one admin-signed, replicated, LWW record.
///
/// One record rather than one per table, because it is one screen and one
/// decision — an operator shortening `runs` wants to see what that does to
/// the projection beside `facts`, not to sign five things.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// `(table, window)` pairs. A `Vec` rather than a map so the postcard
    /// encoding the signature covers is deterministic without depending on
    /// a map's iteration order.
    ///
    /// Kept sorted by [`RetentionTable`]'s ordering. A table absent from the
    /// list takes its shipped default, which is what makes a policy written
    /// by an older build still bound a table it had never heard of.
    pub windows: Vec<(RetentionTable, RetentionWindow)>,
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl Default for RetentionPolicy {
    /// The policy a mesh has before anyone signs one: every shipped default,
    /// unsigned.
    ///
    /// **This is what makes a fresh mesh bounded, and an upgraded one too.**
    /// The meshes with the growth problem are the old ones, and requiring a
    /// `RetentionPut` before anything is bounded would mean A1 fixes nothing
    /// until an operator reads a release note.
    fn default() -> Self {
        RetentionPolicy {
            windows: RetentionTable::ALL
                .into_iter()
                .map(|t| {
                    (
                        t,
                        RetentionWindow {
                            days: t.default_days(),
                            floor_days: t.floor_days(),
                        },
                    )
                })
                .collect(),
            created_by: UserId([0u8; 32]),
            created_at_micros: 0,
            signature: Vec::new(),
        }
    }
}

impl RetentionPolicy {
    /// The window governing one table, falling back to its shipped default.
    pub fn window(&self, table: RetentionTable) -> RetentionWindow {
        self.windows
            .iter()
            .find(|(t, _)| *t == table)
            .map(|(_, w)| *w)
            .unwrap_or(RetentionWindow {
                days: table.default_days(),
                floor_days: table.floor_days(),
            })
    }

    /// Set one window, keeping the list sorted and canonical.
    pub fn set(&mut self, table: RetentionTable, window: RetentionWindow) {
        match self.windows.iter_mut().find(|(t, _)| *t == table) {
            Some(slot) => slot.1 = window,
            None => self.windows.push((table, window)),
        }
        self.windows.sort_by_key(|(t, _)| *t);
    }

    /// Has anyone actually signed this, or is it the shipped default?
    ///
    /// The distinction is what every surface prints: *"defaults"* and
    /// *"set by alice on Tuesday"* are different sentences, and only one of
    /// them means somebody decided.
    pub fn is_signed(&self) -> bool {
        !self.signature.is_empty()
    }

    /// The bytes the admin's signature covers: the whole record with the
    /// signature blanked, exactly as a schedule does it.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        unsigned.windows.sort_by_key(|(t, _)| *t);
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(b"mw-retention-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// Why this policy may not be signed, if it may not.
    ///
    /// Checked before signing *and* again at merge time on every node. The
    /// second check is the one that matters: a floor enforced only by the
    /// tool that writes the policy is a floor, and one enforced by every
    /// node that applies it is a rule.
    pub fn refusal(&self) -> Option<String> {
        for (table, window) in &self.windows {
            let floor = table.floor_days();
            if floor == 0 {
                continue;
            }
            // Forever clears every floor: it keeps strictly more.
            if window.is_forever() {
                continue;
            }
            if window.days < floor {
                return Some(format!(
                    "`{}` may not be shorter than {} days ({} was asked for) — an audit log \
                     you can shorten to nothing is not an audit log",
                    table.as_str(),
                    floor,
                    window.days
                ));
            }
        }
        None
    }
}

/// How a retention window reads in a table or a sentence.
///
/// `forever`, `90d`, `1y`. Years only when they divide evenly, because
/// "1.2y" is a number nobody typed and nobody wants to type back.
pub fn describe_days(days: u32) -> String {
    if days == FOREVER {
        return "forever".to_string();
    }
    if days >= 365 && days.is_multiple_of(365) {
        let years = days / 365;
        return format!("{years}y");
    }
    format!("{days}d")
}

/// Parse what an operator types: `90d`, `1y`, `12w`, `forever`, `0`.
pub fn parse_days(text: &str) -> Result<u32, String> {
    let t = text.trim().to_ascii_lowercase();
    if t == "forever" || t == "never" {
        return Ok(FOREVER);
    }
    let (number, scale) = match t.strip_suffix('y') {
        Some(n) => (n, 365u32),
        None => match t.strip_suffix('w') {
            Some(n) => (n, 7),
            None => match t.strip_suffix('d') {
                Some(n) => (n, 1),
                None => (t.as_str(), 1),
            },
        },
    };
    let n: u32 = number
        .trim()
        .parse()
        .map_err(|_| format!("'{text}' is not a retention window (try 90d, 1y, or forever)"))?;
    n.checked_mul(scale)
        .ok_or_else(|| format!("'{text}' is longer than this can express"))
}

/// Signing bytes for withdrawing the policy back to the shipped defaults.
///
/// A removal rather than a put-of-defaults so `meshnode audit` records what
/// actually happened: "reset retention to defaults" and "signed a policy
/// that happens to equal the defaults" are different acts, and only the
/// first one is reversible by reading the audit log.
pub fn retention_reset_signing_bytes(by: &UserId, at_micros: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-retention-reset-v1");
    buf.extend_from_slice(&by.0);
    buf.extend_from_slice(&at_micros.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_mesh_is_bounded_before_anyone_configures_anything() {
        let p = RetentionPolicy::default();
        assert!(!p.is_signed());
        assert_eq!(p.window(RetentionTable::Runs).days, 90);
        assert_eq!(p.window(RetentionTable::Facts).days, 365);
        assert_eq!(p.window(RetentionTable::JobOutput).days, 30);
        // The one table whose default is forever, and it is the one that
        // must be (§4 decision 8).
        assert!(p.window(RetentionTable::Audit).is_forever());
    }

    #[test]
    fn a_table_the_policy_never_heard_of_still_takes_its_default() {
        // A policy written by a build that only knew about `runs`.
        let p = RetentionPolicy {
            windows: vec![(
                RetentionTable::Runs,
                RetentionWindow {
                    days: 7,
                    floor_days: 0,
                },
            )],
            ..Default::default()
        };
        assert_eq!(p.window(RetentionTable::Runs).days, 7);
        assert_eq!(
            p.window(RetentionTable::Archive).days,
            DEFAULT_ARCHIVE_DAYS,
            "an absent table falls back rather than becoming unbounded"
        );
    }

    /// Q8, decided: the mesh policy is the **default** and a node may keep
    /// less but not more. The machine with the small disk is a real machine.
    #[test]
    fn a_node_may_keep_less_but_never_more() {
        let w = RetentionWindow {
            days: 30,
            floor_days: 0,
        };
        assert_eq!(w.resolve_for_node(None), 30, "silence takes the default");
        assert_eq!(w.resolve_for_node(Some(7)), 7, "less is allowed");
        assert_eq!(w.resolve_for_node(Some(90)), 30, "more is clamped");
        assert_eq!(
            w.resolve_for_node(Some(FOREVER)),
            30,
            "`forever` is the largest ask of all, and is clamped like any other"
        );
    }

    #[test]
    fn a_floor_clamps_a_node_that_asked_for_less() {
        let w = RetentionWindow {
            days: 365,
            floor_days: 90,
        };
        assert_eq!(w.resolve_for_node(Some(7)), 90, "the floor wins");
        assert_eq!(w.resolve_for_node(Some(180)), 180);
        assert_eq!(w.resolve_for_node(None), 365);
    }

    #[test]
    fn a_forever_window_stays_forever_and_still_lets_a_node_keep_less() {
        let w = RetentionWindow::forever();
        assert_eq!(w.resolve_for_node(None), FOREVER);
        assert_eq!(w.resolve_for_node(Some(30)), 30);
    }

    #[test]
    fn the_audit_floor_is_refused_and_the_refusal_names_it() {
        let mut p = RetentionPolicy::default();
        p.set(
            RetentionTable::Audit,
            RetentionWindow {
                days: 30,
                floor_days: AUDIT_FLOOR_DAYS,
            },
        );
        let why = p.refusal().expect("30 days of audit must be refused");
        assert!(
            why.contains("365"),
            "the refusal must name the floor: {why}"
        );
        assert!(why.contains("audit"), "{why}");

        // Raising it is fine, and so is forever.
        p.set(
            RetentionTable::Audit,
            RetentionWindow {
                days: 3 * 365,
                floor_days: AUDIT_FLOOR_DAYS,
            },
        );
        assert!(p.refusal().is_none());
        p.set(RetentionTable::Audit, RetentionWindow::forever());
        assert!(p.refusal().is_none());
    }

    #[test]
    fn signing_bytes_are_stable_under_the_order_windows_were_set_in() {
        let mut a = RetentionPolicy::default();
        a.windows.clear();
        a.set(RetentionTable::Facts, RetentionWindow::forever());
        a.set(
            RetentionTable::Runs,
            RetentionWindow {
                days: 30,
                floor_days: 0,
            },
        );

        let mut b = RetentionPolicy::default();
        b.windows.clear();
        b.set(
            RetentionTable::Runs,
            RetentionWindow {
                days: 30,
                floor_days: 0,
            },
        );
        b.set(RetentionTable::Facts, RetentionWindow::forever());

        assert_eq!(a.signing_bytes(), b.signing_bytes());
    }

    #[test]
    fn windows_round_trip_through_what_an_operator_types() {
        for (text, days) in [
            ("90d", 90),
            ("1y", 365),
            ("5y", 5 * 365),
            ("12w", 84),
            ("30", 30),
            ("forever", FOREVER),
            ("never", FOREVER),
        ] {
            assert_eq!(parse_days(text), Ok(days), "{text}");
        }
        assert_eq!(describe_days(90), "90d");
        assert_eq!(describe_days(365), "1y");
        assert_eq!(describe_days(FOREVER), "forever");
        assert!(parse_days("soon").is_err());
    }

    #[test]
    fn only_the_run_and_audit_tables_bound_the_replicated_log() {
        // The half of A1 that actually drops ops. Stated once, here, so
        // `compact.rs` and the policy cannot drift apart about which tables
        // the compaction class applies to.
        let bounding = [RetentionTable::Runs, RetentionTable::Audit];
        for t in RetentionTable::ALL {
            assert_eq!(
                t.bounds_the_op_log(),
                bounding.contains(&t),
                "{}",
                t.as_str()
            );
        }
    }

    #[test]
    fn table_names_round_trip() {
        for t in RetentionTable::ALL {
            assert_eq!(RetentionTable::parse(t.as_str()), Some(t));
        }
        assert_eq!(RetentionTable::parse("RUNS"), Some(RetentionTable::Runs));
        assert_eq!(RetentionTable::parse("nonsense"), None);
    }
}

// ---------------------------------------------------------------------------
// How hard this node tidies its own content store (M31 A2)
// ---------------------------------------------------------------------------

/// How aggressively a node reclaims its local content store.
///
/// A named level rather than a raw number of days, because the number is not
/// the decision. An operator choosing between *"keep a quarter of history"*
/// and *"keep nothing I am not running"* is picking a **posture**, and the
/// day count that implements it is an implementation detail they should be
/// able to read but not have to invent. `meshnode retention set content=45d`
/// still works for the operator who does want the number — the level is a
/// well-lit path through the same window, not a second mechanism.
///
/// The ladder is deliberately short. Six rungs is enough to express
/// *never / mostly / middle / tidy / lean / nothing*, and every extra rung
/// is another thing to explain and another support conversation about the
/// difference between two settings nobody can tell apart.
///
/// **Nothing here overrides a reference.** Every level is a statement about
/// *unreferenced* bundles; a bundle a live service version, a git source
/// head or this node's own release still needs is kept at every level,
/// including [`ContentPolicy::Ephemeral`]. A retention window that could
/// delete a running deployment's code would not be a retention window, it
/// would be an outage on a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ContentPolicy {
    /// Never reclaim. The pre-A2 behaviour, and now a thing you type.
    Keep,
    /// A quarter. For a node that rolls back to old releases by hand.
    Relaxed,
    /// A month. **The default** — last month's deploy is still local.
    Balanced,
    /// A week. A build farm that publishes far more than it re-runs.
    Tidy,
    /// A day. Effectively a hot cache with an overnight grace period.
    Lean,
    /// Keep only what something still needs. A bundle becomes reclaimable
    /// the moment the last thing referencing it lets go, with no grace
    /// period at all — for the small disk that would rather refetch.
    Ephemeral,
}

impl Default for ContentPolicy {
    /// [`ContentPolicy::Balanced`] — not too passive, not too aggressive.
    fn default() -> Self {
        ContentPolicy::Balanced
    }
}

impl ContentPolicy {
    pub const ALL: [ContentPolicy; 6] = [
        ContentPolicy::Keep,
        ContentPolicy::Relaxed,
        ContentPolicy::Balanced,
        ContentPolicy::Tidy,
        ContentPolicy::Lean,
        ContentPolicy::Ephemeral,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ContentPolicy::Keep => "keep",
            ContentPolicy::Relaxed => "relaxed",
            ContentPolicy::Balanced => "balanced",
            ContentPolicy::Tidy => "tidy",
            ContentPolicy::Lean => "lean",
            ContentPolicy::Ephemeral => "ephemeral",
        }
    }

    pub fn parse(s: &str) -> Option<ContentPolicy> {
        ContentPolicy::ALL
            .into_iter()
            .find(|p| p.as_str().eq_ignore_ascii_case(s.trim()))
    }

    /// The window this level asks for, in days. [`FOREVER`] for `keep`.
    ///
    /// This is a *request*, not the answer: it still goes through
    /// [`RetentionWindow::resolve_for_node`], so the mesh policy can cap a
    /// node that asks to keep more than the mesh allows.
    pub fn days(self) -> u32 {
        match self {
            ContentPolicy::Keep => FOREVER,
            ContentPolicy::Relaxed => 90,
            ContentPolicy::Balanced => 30,
            ContentPolicy::Tidy => 7,
            ContentPolicy::Lean => 1,
            ContentPolicy::Ephemeral => 0,
        }
    }

    /// `Ephemeral` and `Keep` both map to a day count of zero and mean
    /// opposite things, so neither may be read as a bare number.
    ///
    /// `FOREVER` *is* zero — the sentinel this module has always used — and
    /// "reclaim immediately" is also zero days. The two are told apart here,
    /// once, rather than by every caller remembering which zero it holds.
    pub fn reclaims_immediately(self) -> bool {
        matches!(self, ContentPolicy::Ephemeral)
    }

    pub fn never_reclaims(self) -> bool {
        matches!(self, ContentPolicy::Keep)
    }

    /// One line for a settings screen or `--help`.
    pub fn describe(self) -> &'static str {
        match self {
            ContentPolicy::Keep => "never reclaim anything",
            ContentPolicy::Relaxed => "reclaim unused bundles after 90 days",
            ContentPolicy::Balanced => "reclaim unused bundles after 30 days",
            ContentPolicy::Tidy => "reclaim unused bundles after 7 days",
            ContentPolicy::Lean => "reclaim unused bundles after 1 day",
            ContentPolicy::Ephemeral => "keep only what something still needs",
        }
    }
}

impl std::fmt::Display for ContentPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod content_policy_tests {
    use super::*;

    /// `FOREVER` is zero and "reclaim with no grace period" is also zero.
    /// The two are opposite instructions wearing the same number, and this
    /// test exists because that collision already shipped a bug once:
    /// `ephemeral` under a 7-day mesh policy resolved to *seven days*,
    /// because `resolve_for_node` read its zero as "asking to keep more".
    #[test]
    fn keep_and_ephemeral_share_a_number_and_not_a_meaning() {
        assert_eq!(ContentPolicy::Keep.days(), FOREVER);
        assert_eq!(ContentPolicy::Ephemeral.days(), 0);
        assert!(ContentPolicy::Keep.never_reclaims());
        assert!(!ContentPolicy::Keep.reclaims_immediately());
        assert!(ContentPolicy::Ephemeral.reclaims_immediately());
        assert!(!ContentPolicy::Ephemeral.never_reclaims());
    }

    /// The trap itself, asserted, so anybody tempted to route `ephemeral`
    /// through the window arithmetic sees what it does.
    #[test]
    fn resolving_a_zero_window_clamps_up_to_the_mesh_default() {
        let window = RetentionWindow {
            days: 7,
            floor_days: 0,
        };
        assert_eq!(
            window.resolve_for_node(Some(FOREVER)),
            7,
            "a zero request is read as `forever`, which is asking to keep more"
        );
        // Which is correct for `keep`, and catastrophic for `ephemeral` —
        // hence `Node::resolved_content_window` bypassing this entirely.
        assert_eq!(window.resolve_for_node(Some(1)), 1, "a node may keep less");
        assert_eq!(window.resolve_for_node(Some(30)), 7, "and never more");
    }

    #[test]
    fn every_rung_round_trips_through_its_name() {
        for p in ContentPolicy::ALL {
            assert_eq!(ContentPolicy::parse(p.as_str()), Some(p));
            assert_eq!(ContentPolicy::parse(&p.as_str().to_uppercase()), Some(p));
        }
        assert_eq!(ContentPolicy::parse("aggressive"), None);
        assert_eq!(ContentPolicy::default(), ContentPolicy::Balanced);
    }

    /// The ladder has to actually descend, or the names are decoration.
    #[test]
    fn the_ladder_gets_strictly_more_aggressive() {
        let finite: Vec<u32> = ContentPolicy::ALL
            .into_iter()
            .filter(|p| !p.never_reclaims() && !p.reclaims_immediately())
            .map(|p| p.days())
            .collect();
        assert!(
            finite.windows(2).all(|w| w[0] > w[1]),
            "rungs should shorten monotonically, got {finite:?}"
        );
    }
}
