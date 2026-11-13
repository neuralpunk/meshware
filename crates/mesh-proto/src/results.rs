//! Result rows: what a job *answered*, as distinct from what it logged
//! (M19 §5.3, §5.4).
//!
//! A row is one target's outcome — the node, the exit code, and the
//! outputs the job published through the marker convention or got for free
//! (`stdout`, `stdout_json`, `stderr`, `exit_code`). One row per target,
//! one array per fan-out.
//!
//! **Nothing here is ever an op.** A 600-target fan-out would author 600
//! ops that every node in the mesh carries forever to answer a question one
//! person asked once; rows live in the owning node's `history.redb` and
//! nowhere else. That is the third time this argument has been correct —
//! M8 refused an op per schedule fire, M12 refused one per host health
//! sample — and the honest consequence is stated in the CLI: `meshnode
//! results` answers from the node that owns or owned the group, and asked
//! elsewhere it says so and names the owner.

use serde::{Deserialize, Serialize};

use crate::ops::JobState;
use crate::{FanId, JobId, NodeId, UserId};

/// One target's answer.
///
/// `outputs` is ordered declared-first, then the implicit four, exactly as
/// a workflow step's outputs are — because since M19 they are produced by
/// the same code (§5.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultRow {
    pub job_id: JobId,
    pub node: NodeId,
    pub node_name: String,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub outputs: Vec<(String, String)>,
    pub finished_at_micros: Option<u64>,
    /// Set by `--expect` when the assertion ran against this row: `None`
    /// means nothing was asserted, `Some(false)` means this row is why the
    /// command exited non-zero.
    pub expect_passed: Option<bool>,
    /// The evaluator's own sentence about the comparison — what makes a
    /// failed assertion a diagnosis rather than a verdict.
    pub expect_detail: String,
}

impl ResultRow {
    /// One output by key.
    pub fn output(&self, key: &str) -> Option<&str> {
        self.outputs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Did the target's own work succeed? Distinct from `expect_passed`:
    /// a job can exit 0 and still fail the assertion, which is exactly the
    /// case `--expect` exists to catch.
    pub fn succeeded(&self) -> bool {
        self.state == JobState::Succeeded
    }
}

/// Every row of one fan-out, plus who is answering.
///
/// `owner` is named on every response — including a refusal — because
/// "this node has never seen that group" is only useful when it comes with
/// the address of the node that has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultSet {
    pub group: FanId,
    pub owner: NodeId,
    pub owner_name: String,
    pub submitter_user: UserId,
    /// The command or service that produced these rows, for a header line.
    pub preview: String,
    pub created_at_micros: u64,
    pub rows: Vec<ResultRow>,
    /// Targets in the plan that never reported a row.
    pub missing: u32,
}

impl ResultSet {
    pub fn succeeded(&self) -> usize {
        self.rows.iter().filter(|r| r.succeeded()).count()
    }

    pub fn failed(&self) -> usize {
        self.rows.len() - self.succeeded()
    }
}
