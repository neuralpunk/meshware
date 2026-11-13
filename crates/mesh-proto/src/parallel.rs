//! Parallel jobs: one logical job whose input is cut into chunks that
//! workers pull (MASTERPLAN M9 §5.4).
//!
//! The mesh does not create parallelism; it routes parallelism that already
//! exists. A parallel job says "run this command once per item", the owner
//! cuts the item list into chunks, and every worker pulls the next chunk
//! when it finishes one. Nobody decides that a 32-core node should do twice
//! the work of a 16-core node — it simply pulls twice as often.
//!
//! Nothing here is replicated. The chunk queue lives in the owner's memory
//! and the op-log never sees a chunk: per-chunk records would be
//! time-series data in a state store, at a thousand times the volume of the
//! job records that belong there (M9 §3.4, §7).

use serde::{Deserialize, Serialize};

use crate::content::ServiceExec;
use crate::objects::{Capture, ObjectRef};
use crate::{JobId, NodeId, SignedIpc, UserId};

/// The placeholder replaced by each item, as in GNU parallel.
pub const ITEM_PLACEHOLDER: &str = "{}";

/// Every item's value reaches its command *and* its script in the
/// environment under this name. A Library script with no declared input for
/// the item can simply read it, and a command can use it instead of `{}`
/// where shell quoting would be awkward.
pub const ITEM_ENV: &str = "MESHWARE_ITEM";

/// A batched chunk's items, one JSON string per line, in the order the
/// results are expected back.
///
/// Set only for a task that asked to be batched (`MESHWARE_TASK_BATCH`),
/// and it is the *only* signal that distinguishes the two shapes: a task
/// that finds this variable set is being handed the whole chunk and must
/// answer for all of it; one that does not gets [`ITEM_ENV`] and answers
/// for one item, exactly as it always did.
pub const ITEMS_ENV: &str = "MESHWARE_ITEMS";

/// Where a batched task writes its answers: one JSON object per line,
/// `{"i": <index into the chunk>, "code": <exit code>, "out": "...",
/// "err": "..."}`.
///
/// **Per-item, not per-chunk.** The whole risk of batching is that one bad
/// item takes its ninety-nine neighbours down with it, so the protocol is
/// built around the task reporting each item separately — a task that
/// catches its own exceptions turns a poisoned item into one failed row
/// instead of a failed chunk. An item the task never reports is failed with
/// whatever the process wrote, which is the honest reading of "the
/// interpreter died half way through".
pub const RESULTS_ENV: &str = "MESHWARE_RESULTS";

/// `--halt-on-fail N|N%` on a parallel job, as a reserved environment key
/// the owner reads off before anything is dispatched. Travels in the
/// environment for the reason every other origin-only flag does: the
/// signed `IpcRequest::Parallel` is re-decoded by workers, and growing it
/// would break a mixed-version mesh.
pub const REQ_HALT_ON_FAIL: &str = "MESHWARE_TASK_HALT_ON_FAIL";

/// `--resume JOB`: the parallel job this submission re-runs the unfinished
/// items of. Recorded on the new run, never acted on — the link is the
/// whole feature, and the CLI did the item arithmetic before submitting.
pub const REQ_RESUMES: &str = "MESHWARE_TASK_RESUMES";

/// The BLAKE3 digest of an item's stdout, normalised the way L3 D-L3.2
/// says: trailing whitespace stripped per line, trailing newlines
/// collapsed. Two hosts that print the same thing with a different
/// trailing newline are the same answer.
pub fn stdout_digest(stdout: &str) -> [u8; 32] {
    let mut lines: Vec<&str> = stdout.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut hasher = blake3::Hasher::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\n");
        }
        hasher.update(line.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// One item's outcome as the owning node recorded it — what `--resume`
/// and `results diff` read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelItemRecord {
    pub item: String,
    /// `None` = could not be spawned, or timed out.
    pub exit_code: Option<i32>,
    pub node_name: String,
    pub micros: u64,
    /// [`stdout_digest`] of the inline stdout. For a `map` item whose bytes
    /// stayed in an object store this is the digest of the empty string,
    /// and `output` is the thing to compare instead.
    pub stdout_digest: [u8; 32],
    /// Where a `map` item's output stayed (content-addressed, so two runs
    /// that produced the same bytes carry the same id).
    pub output: Option<crate::ObjectId>,
}

impl ParallelItemRecord {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// A parallel run as its owner remembers it: the request, so the run can
/// be resubmitted, and every item's outcome, so only the unfinished part
/// of it need be.
///
/// Node-local, in the owner's history index, never replicated — the same
/// rule as fan-out result sets (M14). Written once when the run starts
/// (the request alone) and once when it ends (with the outcomes), so a
/// run whose owner died mid-way still says what it was, and a `--resume`
/// of it re-runs everything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelRecord {
    pub job: JobId,
    pub owner: NodeId,
    pub owner_name: String,
    pub submitter_user: UserId,
    pub created_at_micros: u64,
    pub finished_at_micros: Option<u64>,
    /// The request, verbatim.
    pub command: String,
    pub service: Option<String>,
    pub inputs: Vec<(String, String)>,
    pub item_input: Option<String>,
    pub items: Vec<String>,
    pub on: Option<String>,
    /// The environment as submitted — reserved keys included, so a resume
    /// carries the same requirements, cache claim and inventory selector.
    pub env: Vec<(String, String)>,
    pub timeout_secs: Option<u64>,
    pub chunk: Option<u32>,
    pub capture: Capture,
    pub reduce: Option<String>,
    pub reduce_on: Option<String>,
    /// The run this one resumed, if any.
    pub resumes: Option<JobId>,
    /// Why the owner stopped handing out work early, if it did.
    pub halted: Option<String>,
    pub outcomes: Vec<ParallelItemRecord>,
}

impl ParallelRecord {
    /// Items with no successful outcome, in submission order and without
    /// duplicates — what a `--resume` submits.
    pub fn unfinished_items(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for item in &self.items {
            if out.contains(item) {
                continue;
            }
            let done = self
                .outcomes
                .iter()
                .any(|o| &o.item == item && o.succeeded());
            if !done {
                out.push(item.clone());
            }
        }
        out
    }

    pub fn succeeded(&self) -> usize {
        self.outcomes.iter().filter(|o| o.succeeded()).count()
    }

    pub fn failed(&self) -> usize {
        self.outcomes.len() - self.succeeded()
    }

    /// Items that never reported at all — an owner that died, or a halt.
    pub fn missing(&self) -> usize {
        self.items
            .iter()
            .filter(|i| !self.outcomes.iter().any(|o| &o.item == *i))
            .count()
    }

    pub fn what(&self) -> String {
        match &self.service {
            Some(s) => s.clone(),
            None => self.command.clone(),
        }
    }
}

/// Per-item output kept and shipped back to the owner. Enough to see what
/// happened, bounded so one chatty item cannot fill a frame.
pub const MAX_ITEM_OUTPUT: usize = 64 * 1024;

/// The plan carries its own authorization. A worker executes for a *user*,
/// not for whichever node asked it to, so it re-verifies the user's signed
/// envelope and its own view of that user's grants before running a single
/// item — the same rule as an ordinary job (MASTERPLAN §7.2).
///
/// It also carries the full item list and the worker set, which is what
/// makes the job survive its owner: every participant holds enough to
/// rebuild the queue, and they all rank the same successor from the same
/// job id (M9 §5.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelPlan {
    pub job: JobId,
    /// Command template; [`ITEM_PLACEHOLDER`] is replaced by each item.
    /// Empty when this job runs a Library script instead ([`Self::exec`]).
    pub command: String,
    /// A Library script to run once per item, instead of a shell command.
    /// Workers materialize its bundle exactly as they would for an ordinary
    /// service job, so anything runnable on Run is runnable per item.
    pub exec: Option<Box<ServiceExec>>,
    /// Which of the script's inputs receives each item. `None` appends the
    /// item as a positional argument, which is what a script with no
    /// declared inputs expects.
    pub item_input: Option<String>,
    pub items: Vec<String>,
    /// Environment for each item, including the reserved `MESHWARE_TASK_*`
    /// requirement keys (M9 §5.3).
    pub env: Vec<(String, String)>,
    /// Per-item timeout in seconds; `None` = none.
    pub timeout_secs: Option<u64>,
    /// Items per chunk when the operator fixed it; `None` = adaptive.
    pub chunk: Option<u32>,
    /// Where each item's output goes: inline in the report (`parallel`) or
    /// into the producing node's ephemeral object store (`map`, M9 §5.5).
    pub capture: Capture,
    /// Objects every item must have locally before it runs, materialized
    /// once per chunk into the directory named by
    /// [`crate::objects::INPUTS_ENV`].
    ///
    /// This is the task→task edge — a task declaring its data
    /// dependencies, which is what rung 3 is (M9 §2). A reduce step is not
    /// a special kind of job because of it: it is a one-item parallel job
    /// whose inputs happen to be every output of the map before it.
    ///
    /// It rides here rather than on `JobSpec` for §5.3's reason, reaching
    /// the opposite conclusion for a good cause: `JobSpec` is decoded by
    /// *every* peer, 1.5 included, so growing it would break ordinary jobs
    /// in a mixed mesh — while a `ParallelPlan` only ever travels inside
    /// `Request::ParallelOffer`, which is gated at `COMPUTE_MINOR` already.
    pub inputs: Vec<ObjectRef>,
    /// Every node that was offered this job, the original owner included.
    /// Successor ranking runs over the live members of this set, so every
    /// worker reaches the same answer with no coordination.
    pub workers: Vec<NodeId>,
    pub submitter_user: UserId,
    /// The user's signed `IpcRequest::Parallel`.
    pub auth: SignedIpc,
}

/// What a worker is handed on a pull: a slice of the plan's item list.
/// Deliberately tiny — the worker already has the items, the command and
/// the authorization, so a pull costs two integers rather than a copy of
/// the work.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ChunkAssignment {
    pub job: JobId,
    /// Chunk number, unique within the job and stable across re-issue.
    pub chunk: u32,
    /// Index of the first item, into [`ParallelPlan::items`].
    pub start: u32,
    pub count: u32,
}

impl ChunkAssignment {
    /// The items this chunk covers, from the plan the worker holds.
    /// Empty if the assignment does not fit the plan — a mismatch means the
    /// two sides disagree about the job, and running nothing is the safe
    /// reading.
    pub fn slice<'a>(&self, items: &'a [String]) -> &'a [String] {
        let start = self.start as usize;
        let end = start.saturating_add(self.count as usize);
        items.get(start..end).unwrap_or(&[])
    }

    /// Item indices this chunk covers.
    pub fn indices(&self) -> std::ops::Range<u32> {
        self.start..self.start.saturating_add(self.count)
    }
}

/// What one item did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemOutcome {
    pub item: String,
    /// `None` when the item could not be spawned or timed out.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Wall time, so the owner can size the next chunk (M9 §5.4).
    pub micros: u64,
    /// Under [`Capture::Object`], the output that stayed on the node that
    /// produced it — this reference is all that travels to the owner, and
    /// it is the difference between a data plane and a funnel (M9 §5.5).
    ///
    /// A **failed item produces no object**: half-written output is not
    /// something a reduce step should be handed, and the operator needs
    /// the bytes on screen rather than in a store, so a failure keeps its
    /// bounded inline stdout/stderr instead.
    pub output: Option<ObjectRef>,
}

impl ItemOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// A worker's report for one finished chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkReport {
    pub job: JobId,
    pub chunk: u32,
    pub outcomes: Vec<ItemOutcome>,
}

/// How much of a parallel job each node has done — the view that makes work
/// stealing legible, since nothing else in the system would show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeProgress {
    pub node: NodeId,
    pub name: String,
    /// Items completed by this node.
    pub items: u64,
    /// Chunks completed by this node.
    pub chunks: u32,
    /// Items per second, over this node's completed chunks.
    pub items_per_sec: f64,
}

/// Truncate output to [`MAX_ITEM_OUTPUT`], marking the cut so nobody reads
/// a truncated log as a complete one.
pub fn bound_output(raw: &[u8]) -> String {
    if raw.len() <= MAX_ITEM_OUTPUT {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let kept = String::from_utf8_lossy(&raw[..MAX_ITEM_OUTPUT]).into_owned();
    format!("{kept}\n… [truncated at {MAX_ITEM_OUTPUT} bytes]")
}

impl ParallelPlan {
    /// The command line for one item.
    ///
    /// `{}` substitution always applies. What changes with the plan is the
    /// *no-placeholder* case: GNU parallel appends the item as a positional
    /// argument, and that is right for `parallel -a hosts.txt -- ./check.sh`
    /// — but wrong for a task that declares [`Self::inputs`], whose data
    /// arrives in a directory rather than in argv. There the item is a
    /// label, it reaches the command as [`ITEM_ENV`] like everything else,
    /// and appending it would hand the command a filename that does not
    /// exist. A reduce is the obvious case: its input is every output of
    /// the map before it, not the word naming the map.
    pub fn command_for(&self, item: &str) -> String {
        // "The item is a label" has to mean it for both branches. This used
        // to skip only the *append*, so a reduce whose command happened to
        // contain `{}` still had it replaced — with the job id of the map,
        // because that is the reduce's one nominal item. The failure was
        // silent and bewildering: an ordinary jq merge containing an empty
        // object literal came back as
        // `reduce .[] as $x (job-agqignet6…; …)` and would not compile.
        if !self.inputs.is_empty() {
            self.command.clone()
        } else if self.command.contains(ITEM_PLACEHOLDER) {
            self.command.replace(ITEM_PLACEHOLDER, item)
        } else {
            format!("{} {item}", self.command)
        }
    }

    /// What this job runs, for a job row or a log line.
    pub fn what(&self) -> String {
        match &self.exec {
            Some(exec) => format!("{} ({})", exec.service_name, exec.runtime.as_str()),
            None => self.command.clone(),
        }
    }
}

/// Substitute an item into a command template. A template with no
/// placeholder gets the item appended, which is what GNU parallel does.
pub fn expand(command: &str, item: &str) -> String {
    if command.contains(ITEM_PLACEHOLDER) {
        command.replace(ITEM_PLACEHOLDER, item)
    } else {
        format!("{command} {item}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_with_declared_inputs_takes_no_positional_item() {
        let plan = |inputs: Vec<ObjectRef>, command: &str| ParallelPlan {
            job: JobId::generate(),
            command: command.into(),
            exec: None,
            item_input: None,
            items: vec!["x".into()],
            env: vec![],
            timeout_secs: None,
            chunk: None,
            capture: Capture::Object,
            inputs,
            workers: vec![],
            submitter_user: UserId([0; 32]),
            auth: SignedIpc {
                user: UserId([0; 32]),
                at_micros: 0,
                nonce: 0,
                payload: vec![],
                signature: vec![],
            },
        };
        let one = ObjectRef {
            id: crate::ObjectId([1; 32]),
            size: 1,
            holder: NodeId([1; 32]),
        };
        // No inputs: GNU parallel's rule, the item is appended.
        assert_eq!(plan(vec![], "wc -l").command_for("a"), "wc -l a");
        // Inputs declared: the data is in $MESHWARE_INPUTS, and appending
        // the item would hand the command a filename that does not exist.
        assert_eq!(plan(vec![one], "wc -l").command_for("a"), "wc -l");
        // **Reversed 2026-09-08.** This used to assert the opposite —
        // "substitution is unaffected either way, it is explicit" — on the
        // reasoning that a written `{}` means what it says. It does not
        // survive contact with the only thing that sets `inputs`: the
        // reduce, whose single item is the *map's job id*, synthesised for
        // legibility and never supplied by anybody. A user merging with jq
        // wrote an ordinary empty object literal and got the job id spliced
        // into the middle of their expression.
        //
        // Nothing else produces a plan with inputs — every other submission
        // path sets `inputs: Vec::new()` — so the rule this replaces was
        // protecting a case that cannot occur.
        assert_eq!(plan(vec![one], "wc -l {}").command_for("a"), "wc -l {}");
        // The jq shape that found it.
        assert_eq!(
            plan(vec![one], "jq -s 'reduce .[] as $x ({}; . + $x)'").command_for("job-abc"),
            "jq -s 'reduce .[] as $x ({}; . + $x)'"
        );
    }

    #[test]
    fn placeholder_substitution_matches_gnu_parallel() {
        assert_eq!(expand("echo {}", "a"), "echo a");
        assert_eq!(expand("cp {} {}.bak", "f"), "cp f f.bak");
        // No placeholder: the item is appended, as GNU parallel does.
        assert_eq!(expand("echo", "a"), "echo a");
        // The item is substituted verbatim; quoting is the caller's job,
        // exactly as it is with `parallel`.
        assert_eq!(expand("echo '{}'", "a b"), "echo 'a b'");
    }

    #[test]
    fn an_assignment_slices_the_plan_it_was_cut_from() {
        let items: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let a = ChunkAssignment {
            job: JobId::generate(),
            chunk: 0,
            start: 3,
            count: 4,
        };
        assert_eq!(a.slice(&items), &items[3..7]);
        assert_eq!(a.indices().collect::<Vec<_>>(), vec![3, 4, 5, 6]);
        // A worker holding a different list runs nothing rather than
        // guessing which items were meant.
        assert!(a.slice(&items[..2]).is_empty());
    }

    #[test]
    fn output_is_bounded_and_says_so() {
        let short = bound_output(b"hello");
        assert_eq!(short, "hello");
        let long = vec![b'x'; MAX_ITEM_OUTPUT + 10];
        let bounded = bound_output(&long);
        assert!(bounded.len() > MAX_ITEM_OUTPUT);
        assert!(bounded.contains("truncated"));
        assert!(bounded.starts_with("xxx"));
    }
}
