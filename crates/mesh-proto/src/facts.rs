//! Facts: a source of truth built by observation (M18 §4).
//!
//! M12 gave the mesh an inventory. It is **what somebody typed**. Every
//! node already runs an agent that can answer questions about its own
//! machine, and until M18 the mesh could not ask any of them: `--on
//! tag=prod` works because a human maintained the tag, and `--on 'the boxes
//! running the bad kernel'` was not expressible at all.
//!
//! ## The rule that decides every borderline case
//!
//! **If it changes when nothing changed, it is not a fact.** Kernel
//! version, OS release, arch, CPU model, total memory, disk layout,
//! installed packages, listening ports as configured — those are
//! properties. Load average, free memory, currently-failed units, GPU
//! utilisation, uptime — those are *measurements*, they belong to M9
//! pressure and M12 host health, and a collector that publishes one is
//! caught mechanically by the flap guard rather than by a code review.
//!
//! This is not fussiness. It is what makes storing a fact document in the
//! op-log affordable: a stable machine authors **one op per collector,
//! ever**, because a publish only happens when the document's digest
//! changes.
//!
//! ## Why the op-log, and not the two obvious alternatives
//!
//! **Overturned: facts as `cap.*`-style reserved tags.** `MemberInfo::tags`
//! is replaced wholesale by every `MemberUpdate` and replicated to every
//! member; a few kilobytes of facts there means every node carries every
//! other node's document inside its membership record, re-authored on every
//! change.
//!
//! **Overturned: facts on a gated request pair, like `PressureSnapshot`.**
//! Facts must survive a reboot (a node that has just restarted is exactly
//! when you want to know what it is), must be selectable when the subject
//! is *offline*, and their history is the point. Long-lived and low volume
//! is the op-log's quadrant.

//! ## What is *not* here
//!
//! Canonicalization, flattening and diffing a fact document are JSON
//! operations, and this crate is the wire schema — it has never depended on
//! `serde_json` and does not start now. They live in `mesh_core::facts`,
//! next to the collector loop and the selector that reads them.

use serde::{Deserialize, Serialize};

use crate::workflow::StepAction;
use crate::{NodeId, UserId};

/// Largest fact document, matching `INLINE_OUTPUT_MAX` because it is the
/// same judgement about the same log (§4.4).
///
/// Over it, the publish is **refused on the node that produced it**, naming
/// the collector and the byte count, and `doctor` reports it. There is no
/// spill-to-object path: a document that does not fit is a collector bug,
/// and giving it somewhere to go would mean the fix never happens.
pub const MAX_FACT_DOC: usize = 64 * 1024;

/// The one collector every node runs without anybody publishing it.
///
/// A fresh node must have facts before anybody has pushed a bundle to it,
/// and the bootstrap ordering problem — "publish a collector to learn what
/// the node is, on a node you cannot select yet" — is solved by not having
/// it (§4.5).
pub const SYSTEM_COLLECTOR: &str = "system";

/// How often the built-in collector re-reads the machine. Slow on purpose:
/// these are properties, and a property that needs a faster cadence than
/// this is a measurement wearing the wrong name.
pub const SYSTEM_INTERVAL_SECS: u64 = 900;

/// A node authors at most one publish per collector per this window,
/// however much the document changes (§4.6).
///
/// A correctness feature, not polish: without it one badly written
/// collector on 1,000 nodes writes an unbounded, uncompactable op-log.
pub const FLAP_WINDOW_SECS: u64 = 3600;

/// The `fact.` selector prefix (§4.7).
pub const FACT_SELECTOR_PREFIX: &str = "fact.";

/// Prefix owned by fact collection. Reserved on the *job environment*
/// exactly as `MESHWARE_TASK_`, `MESHWARE_WF_` and `MESHWARE_INV_` are: set
/// by the node running the collector, rejected from user-supplied env.
pub const FACT_ENV_PREFIX: &str = "MESHWARE_FACT_";

/// Which collector is running, for a script that serves more than one.
pub const FACT_COLLECTOR_ENV: &str = "MESHWARE_FACT_COLLECTOR";

/// Which subject this invocation must describe: a node id, or a host name.
///
/// This is what makes a host collector one script rather than six hundred
/// records — it reads `$MESHWARE_FACT_SUBJECT` and describes that box.
pub const FACT_SUBJECT_ENV: &str = "MESHWARE_FACT_SUBJECT";

/// What a fact document is *about*.
///
/// Designed in from the start rather than added later, for the reason M12
/// gave for `Host::platform`: this body is node-authored and unsigned, so
/// adding a subject dimension afterwards would cost re-encoding rather than
/// invalidation — but the op-log is not compacted, and a mesh would carry
/// both shapes forever.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FactSubject {
    /// A mesh member, described by itself.
    Node(NodeId),
    /// An inventory host, described by a node the host's `reachable_from`
    /// selector admits — M12's vantage-point rule, reused verbatim.
    Host(String),
}

impl FactSubject {
    pub fn kind(&self) -> FactSubjectKind {
        match self {
            FactSubject::Node(_) => FactSubjectKind::Node,
            FactSubject::Host(_) => FactSubjectKind::Host,
        }
    }

    /// How a subject appears in a table, a selector answer or an audit line.
    pub fn label(&self) -> String {
        match self {
            FactSubject::Node(id) => id.to_display_string(),
            FactSubject::Host(name) => name.clone(),
        }
    }
}

/// Which kind of subject a collector describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FactSubjectKind {
    /// The node that runs the collector describes itself.
    #[default]
    Node,
    /// The node that runs the collector describes each host it is a
    /// vantage point for, one document per host.
    Host,
}

impl FactSubjectKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FactSubjectKind::Node => "node",
            FactSubjectKind::Host => "host",
        }
    }

    pub fn parse(s: &str) -> Option<FactSubjectKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "node" | "" => Some(FactSubjectKind::Node),
            "host" | "hosts" => Some(FactSubjectKind::Host),
            _ => None,
        }
    }
}

/// A collector is an ordinary service, and every node runs its own (§4.5).
///
/// `run` is a [`StepAction`] — a `Service`, an `InlineScript`, or a
/// `Command` — reusing the workflow step vocabulary verbatim. **A collector
/// is a script that prints one JSON object.** That is the entire contract,
/// and it is `stdout_json`, which M19 moved to the job layer so every job
/// has it.
///
/// **No election.** Each node runs the collectors whose selector matches it,
/// on its own timer, and publishes about itself. There is nothing to
/// coordinate because a node's facts exist in exactly one place. This
/// mirrors `hosts::probe`, where each vantage point probes its own hosts,
/// and it is deliberately *unlike* M8's `elected_firer`, which exists
/// because a schedule must fire once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactCollector {
    /// Unique, and the key facts are stored under per subject.
    pub name: String,
    /// What produces the JSON object.
    pub run: StepAction,
    /// Which nodes run it — an ordinary node selector.
    pub on: String,
    /// How often, in seconds. Clamped at read time, never at write time, so
    /// an old record stays decodable.
    pub interval_secs: u64,
    pub subject: FactSubjectKind,
    /// A paused collector stays in state, keeps its facts, and runs
    /// nothing. Pausing is an edit, so it re-signs the record — M8's rule
    /// for schedules, and the reason M18 needs no tombstone body of its own.
    pub paused: bool,
    /// Whole-collection timeout. A collector that hangs must not hold a
    /// slot forever.
    pub timeout_secs: Option<u64>,
    pub description: String,
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`],
    /// verified at merge time on every node.
    pub signature: Vec<u8>,
}

impl FactCollector {
    /// The bytes the creator's signature covers: the whole record with the
    /// signature blanked. `paused` included — a collector is only ever
    /// changed by somebody who may change it, and re-signing proves it.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-fact-collector-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// The interval this collector actually runs at.
    ///
    /// Clamped on read so a record written with `interval: 1` does not turn
    /// a fleet into a publish storm, and so a record written with `0` means
    /// "the default" rather than "as fast as possible".
    pub fn interval(&self) -> u64 {
        const MIN: u64 = 60;
        if self.interval_secs == 0 {
            return SYSTEM_INTERVAL_SECS;
        }
        self.interval_secs.max(MIN)
    }
}

/// One collector's answer about one subject, as a reader sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactRow {
    pub subject: FactSubject,
    /// The subject's display name — a node's name rather than its id, which
    /// is what anybody is actually looking for.
    pub subject_name: String,
    pub collector: String,
    /// Canonical JSON, exactly as the publisher hashed it.
    pub doc: String,
    pub digest: String,
    /// When the collector *observed* this, which is not when the op
    /// arrived and is the timestamp a reader needs.
    pub collected_at_micros: u64,
    /// When this node merged the op that carried it.
    pub published_at_micros: u64,
    /// The node that observed and signed it.
    pub author: NodeId,
    pub author_name: String,
}

/// One collector, as `meshnode collectors ls` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactCollectorRow {
    pub name: String,
    pub run: String,
    pub on: String,
    pub interval_secs: u64,
    pub subject: String,
    pub paused: bool,
    /// True for the in-process `system` collector, which is not an op and
    /// cannot be edited or paused.
    pub built_in: bool,
    pub description: String,
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// How many subjects this collector currently has facts for, from the
    /// asking node's own state.
    pub subjects: u32,
    /// Set when this node has reported the collector flapping (§4.6): the
    /// keys that differed between the last two documents.
    pub flapping_keys: Vec<String>,
    /// The record rendered back as the YAML that would produce it.
    ///
    /// Carried on the row the way `HostRow`, `ScheduleRow` and `PolicyRow`
    /// carry theirs: pausing is an *edit*, so `collectors pause` has to
    /// re-sign the record — and re-signing a rendering of a rendering would
    /// change what was signed.
    pub source: String,
}

/// One key that changed between two documents of the same collector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactChange {
    pub key: String,
    /// `None` on a key that did not exist before (or after).
    pub before: Option<String>,
    pub after: Option<String>,
}

impl FactChange {
    pub fn verb(&self) -> &'static str {
        match (&self.before, &self.after) {
            (None, Some(_)) => "added",
            (Some(_), None) => "removed",
            _ => "changed",
        }
    }
}

/// One step of a subject's fact history (§4.8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactDiffRow {
    pub subject_name: String,
    pub collector: String,
    pub at_micros: u64,
    pub changes: Vec<FactChange>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_interval_is_clamped_on_read_not_on_write() {
        let mut c = FactCollector {
            name: "gpu".into(),
            run: StepAction::Command("true".into()),
            on: "any".into(),
            interval_secs: 1,
            subject: FactSubjectKind::Node,
            paused: false,
            timeout_secs: None,
            description: String::new(),
            created_by: UserId([0u8; 32]),
            created_at_micros: 0,
            signature: Vec::new(),
        };
        assert_eq!(c.interval(), 60);
        c.interval_secs = 0;
        assert_eq!(c.interval(), SYSTEM_INTERVAL_SECS);
        c.interval_secs = 3600;
        assert_eq!(c.interval(), 3600);
    }

    /// Two collectors that differ only in `paused` must not share a
    /// signature: pausing is an edit, and an edit that reuses a signature
    /// is an edit nobody authorized.
    #[test]
    fn pausing_is_inside_the_signature() {
        let base = FactCollector {
            name: "gpu".into(),
            run: StepAction::Command("true".into()),
            on: "any".into(),
            interval_secs: 300,
            subject: FactSubjectKind::Node,
            paused: false,
            timeout_secs: None,
            description: String::new(),
            created_by: UserId([0u8; 32]),
            created_at_micros: 7,
            signature: vec![9; 64],
        };
        let paused = FactCollector {
            paused: true,
            ..base.clone()
        };
        assert_ne!(base.signing_bytes(), paused.signing_bytes());
        // The signature itself is blanked, so re-signing is idempotent.
        let resigned = FactCollector {
            signature: vec![1; 64],
            ..base.clone()
        };
        assert_eq!(base.signing_bytes(), resigned.signing_bytes());
    }
}
