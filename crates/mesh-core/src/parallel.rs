//! Parallel jobs: the owner's chunk queue and the worker's pull loop
//! (MASTERPLAN M9 §5.4).
//!
//! One logical job, one input list, many chunks. The node that accepted the
//! submission owns the queue — in memory, never replicated, never in the
//! op-log — and every participating node pulls the next chunk each time it
//! finishes one. That is the whole mechanism, and it is why a 32-core node
//! ends up doing twice the work of a 16-core one without anyone computing
//! a split: the fast node simply asks more often.
//!
//! Static partitioning is what this replaces. Cutting 48 items 50/50 across
//! a 32-core and a 16-core box gives about 24 effective threads out of 48 —
//! the small box takes twice as long while the big one idles. Pulling
//! absorbs not just the declared difference in hardware but the undeclared
//! ones too: a node busy with other work, a thermally throttled laptop, a
//! noisy VM neighbour.

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use mesh_proto::{
    ChunkAssignment, ChunkReport, ItemOutcome, JobId, NodeId, NodeProgress, ParallelPlan,
};

/// How much work one chunk should be worth. Small enough that the tail of a
/// job stays balanced, large enough that pull traffic is negligible next to
/// the work itself (M9 §5.4).
pub const TARGET_CHUNK: Duration = Duration::from_secs(5);

/// Chunk size before anything has been measured. One item is the
/// calibration round: the first report tells us what an item costs, and
/// every chunk after that is sized from observation.
const CALIBRATION_ITEMS: u32 = 1;

/// Never hand out more than this many items at once, however cheap they
/// look — a mis-measured item must not swallow the whole list.
const MAX_CHUNK_ITEMS: u32 = 4096;

/// Weight of a new measurement in the per-item duration estimate.
const EWMA_ALPHA: f64 = 0.3;

/// Completed fraction at which straggler speculation starts *while there
/// is still fresh work to hand out* (M9 §5.6).
///
/// 95% is where MapReduce put it in 2004 and where Spark and Hadoop still
/// put it: below the tail an outstanding chunk is simply work in progress,
/// and duplicating it spends a lane that could be doing something new.
///
/// It is not the only gate, and it must not be — that was found by running
/// the exit test rather than by reading the plan. Freeze one node of two
/// mid-job and it keeps whatever share it had taken: 42 items of 400 in the
/// measured case, which pins `done` at 89% and means a threshold of 95% is
/// *never crossed*. The job then waits out the 30-second chunk grace, which
/// is precisely the wall-clock disaster speculation exists to prevent. So
/// the real gate is [`Queue::may_speculate`]: this fraction **or** an
/// exhausted list, whichever comes first.
pub const DEFAULT_SPECULATE_AT: f64 = 0.95;

/// A chunk is only speculated once it has been outstanding for this
/// multiple of what its items should have cost. Without the multiple,
/// reaching the tail would duplicate every chunk that had just been handed
/// out — the ones with no evidence of being slow at all.
const SPECULATE_AFTER: u32 = 2;

/// The straggler policy: when a chunk that is taking too long is worth
/// running a second time somewhere else (M9 §5.6).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Straggler {
    /// Completed fraction at which racing may begin *while there is still
    /// fresh work to hand out*. An exhausted item list opens the gate on
    /// its own — see [`DEFAULT_SPECULATE_AT`].
    pub at: f64,
    /// A chunk is never raced before it has been outstanding this long,
    /// whatever the arithmetic says.
    ///
    /// The arithmetic alone gets this wrong, and it took a real run to see
    /// how. A worker's *first* chunk legitimately includes fetching and
    /// materializing the job's bundle, which can be seconds; the per-item
    /// estimate, taken from a node that already had it, can be tens of
    /// milliseconds. Twice that estimate is a threshold the newcomer trips
    /// every time — so the mesh would race away every chunk from the one
    /// node that was about to start being useful, and the job would run on
    /// a single machine while reporting that it had not.
    pub min_wait: Duration,
}

impl Default for Straggler {
    fn default() -> Self {
        Straggler {
            at: DEFAULT_SPECULATE_AT,
            // One chunk's worth of work is the natural floor: below it,
            // the difference between two nodes is scheduling noise and
            // setup cost, not a straggler.
            min_wait: TARGET_CHUNK,
        }
    }
}

/// A chunk that has been handed out but not reported is re-queued after
/// this long. Idempotent items make re-running safe, and a worker that
/// died mid-chunk would otherwise strand its share of the list forever.
const CHUNK_GRACE: Duration = Duration::from_secs(30);

/// A slice of the item list.
#[derive(Debug, Clone, Copy)]
struct Chunk {
    id: u32,
    start: usize,
    len: usize,
}

impl Chunk {
    fn indices(&self) -> std::ops::Range<u32> {
        let start = u32::try_from(self.start).unwrap_or(u32::MAX);
        start..start.saturating_add(u32::try_from(self.len).unwrap_or(0))
    }
}

#[derive(Debug)]
struct Inflight {
    chunk: Chunk,
    worker: NodeId,
    expires: Instant,
    /// When it was handed out, so the tail can tell a slow chunk from a
    /// fresh one.
    issued: Instant,
    /// A second copy of this chunk is already running elsewhere.
    speculated: bool,
}

/// A duplicate of an outstanding chunk, waiting for a worker that is not
/// the one already running it (M9 §5.6).
#[derive(Debug, Clone, Copy)]
struct Speculation {
    chunk: Chunk,
    /// The worker holding the original. Handing the copy back to it would
    /// buy nothing — it is the node we are trying to race.
    avoid: NodeId,
}

#[derive(Debug, Default, Clone)]
struct NodeStat {
    name: String,
    items: u64,
    chunks: u32,
    micros: u64,
}

/// The owner's mutable queue state.
#[derive(Debug)]
struct Queue {
    /// Index of the first item never handed out.
    cursor: usize,
    /// Chunks whose worker never reported: handed out again, to anyone.
    requeue: VecDeque<Chunk>,
    inflight: HashMap<u32, Inflight>,
    next_chunk: u32,
    chunk_items: u32,
    /// Estimated cost of one item, in microseconds. `None` until the
    /// calibration round reports.
    item_micros: Option<f64>,
    failed: u64,
    nodes: BTreeMap<NodeId, NodeStat>,
    cancelled: bool,
    /// Every item index this queue has counted as finished, whoever
    /// finished it and whichever copy got there first.
    ///
    /// One set does three jobs, and they are the same job: it credits work
    /// a re-homing worker did under a previous owner, it stops a new chunk
    /// spanning an item that is already done, and it is what makes a
    /// speculative duplicate safe — the loser's report finds every index
    /// already accounted and contributes nothing rather than double-counting
    /// (M9 §5.4, §5.6).
    accounted: std::collections::HashSet<u32>,
    /// Duplicates of outstanding tail chunks, waiting for a second worker.
    speculative: VecDeque<Speculation>,
    /// How many chunks have been speculated, cumulatively — the number the
    /// UI shows so a re-run reads as mitigation rather than as a fault.
    speculated: u32,
}

/// One parallel job, from the owner's point of view: the plan every
/// participant holds, plus the queue only the owner has.
#[derive(Debug)]
pub struct ParallelJob {
    pub plan: Arc<ParallelPlan>,
    /// When this owner started handing out work, for throughput.
    started: Instant,
    /// Lanes offered across all workers, for tail-balance sizing.
    lanes: AtomicU32,
    /// When to race a straggler, or `None` when the operator turned
    /// speculation off (M9 §5.6).
    straggler: Option<Straggler>,
    queue: Mutex<Queue>,
    /// Finished items, on their way to whoever is streaming this job.
    results: mpsc::UnboundedSender<ChunkResult>,
    /// Bytes of finished output sent and not yet taken (ENG-26). See
    /// [`Results`] for why the bound lives here and not on the channel.
    outstanding: Arc<AtomicU64>,
}

/// A parallel job's live state, as the stream and the UI see it.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub done: u64,
    pub failed: u64,
    /// Items handed out and not yet reported back.
    pub inflight: u64,
    /// Items per chunk right now — adaptive, so this moves (M9 §5.4).
    pub chunk_items: u32,
    /// Chunks re-run elsewhere to beat a straggler, cumulatively. Shown
    /// so a duplicate reads as mitigation rather than as a fault (§5.6).
    pub speculated: u32,
    pub per_node: Vec<NodeProgress>,
}

/// What a completed chunk did, handed back to the streaming task.
#[derive(Debug)]
pub struct ChunkResult {
    pub worker: NodeId,
    pub worker_name: String,
    pub outcomes: Vec<ItemOutcome>,
}

/// The receiving end of a job's finished items.
/// The owner's end of the finished-chunk stream, with the bytes it is
/// holding tracked (ENG-26).
///
/// **Why the channel cannot simply be bounded.** `report` runs under the
/// queue lock, in a synchronous function, so it can neither `await` a
/// bounded `send` nor block one — and `try_send` on a full channel would
/// mean *dropping a finished chunk's output*, which is somebody's answer.
///
/// So the channel stays unbounded and the backpressure goes where it can
/// actually be applied: [`ParallelJob::next_chunk`] stops handing out work
/// while too much finished output is waiting to be streamed. A client on a
/// slow link used to make the owner hold every chunk's captured stdout in
/// memory — a 100 000-item run with a kilobyte of output each is a
/// hundred megabytes that nothing bounded.
#[derive(Debug)]
pub struct Results {
    rx: mpsc::UnboundedReceiver<ChunkResult>,
    outstanding: Arc<AtomicU64>,
}

impl Results {
    pub async fn recv(&mut self) -> Option<ChunkResult> {
        let got = self.rx.recv().await;
        self.credit(&got);
        got
    }

    pub fn try_recv(&mut self) -> std::result::Result<ChunkResult, mpsc::error::TryRecvError> {
        let got = self.rx.try_recv();
        if let Ok(result) = &got {
            self.outstanding.fetch_sub(
                weigh(result).min(self.outstanding.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
        got
    }

    fn credit(&self, got: &Option<ChunkResult>) {
        if let Some(result) = got {
            // Saturating at zero rather than wrapping: a count that went
            // negative would read as an enormous backlog and stop the run
            // handing out any work at all.
            self.outstanding.fetch_sub(
                weigh(result).min(self.outstanding.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
    }
}

/// What one result costs to hold, near enough.
///
/// The captured output, which is all of it that is not a fixed-size
/// field. Exact would mean serialising the thing to measure it, on the
/// path whose cost is the point.
fn weigh(result: &ChunkResult) -> u64 {
    result
        .outcomes
        .iter()
        .map(|o| (o.stdout.len() + o.stderr.len() + o.item.len()) as u64)
        .sum::<u64>()
        // A floor per item, so a run of a hundred thousand items that
        // captures nothing still counts as holding something.
        .saturating_add(result.outcomes.len() as u64 * 64)
}

/// Finished output the owner may hold before it stops handing out work.
///
/// Sized so an ordinary run never reaches it — a client keeping up drains
/// this in milliseconds — and so a client that has stopped reading costs a
/// bounded amount rather than the whole run's output.
pub const MAX_OUTSTANDING_BYTES: u64 = 64 * 1024 * 1024;

/// What a worker gets when it asks for work.
///
/// The three-way answer exists because of speculation (M9 §5.6). Before
/// P7, "nothing to hand out" and "the job is over" were the same thing, and
/// a lane that got `None` stopped. That is wrong at the tail: the list can
/// be exhausted while a chunk is still outstanding, and the copy that will
/// race it is not eligible *yet* — so every lane would retire in the
/// seconds before the thing they exist to do became possible, and there
/// would be nobody left to run the re-run.
#[derive(Debug)]
pub enum Pull {
    /// Here is a chunk.
    Work(Box<ChunkAssignment>),
    /// Nothing right now, and the job is not over: work is outstanding
    /// elsewhere and may yet come back — as a requeued orphan, or as a
    /// copy to race a straggler. Ask again shortly.
    Wait,
    /// Every item is accounted for. This lane is finished.
    Done,
}

impl Pull {
    /// The chunk, if there is one now.
    ///
    /// This collapses `Wait` and `Done`, which is right for a caller that
    /// only means to drain the queue and wrong for a worker lane — a lane
    /// that treats `Wait` as `Done` retires at the tail and takes the
    /// straggler's re-run with it. Lanes match all three variants.
    pub fn work(self) -> Option<ChunkAssignment> {
        match self {
            Pull::Work(a) => Some(*a),
            Pull::Wait | Pull::Done => None,
        }
    }
}

impl ParallelJob {
    /// Take ownership of `plan`: build its chunk queue and the channel its
    /// finished items flow out on.
    pub fn own(plan: Arc<ParallelPlan>) -> (Arc<ParallelJob>, Results) {
        Self::own_with(plan, Some(Straggler::default()))
    }

    /// [`Self::own`] with the straggler policy stated: `None` turns
    /// speculation off entirely (M9 §5.6).
    pub fn own_with(
        plan: Arc<ParallelPlan>,
        straggler: Option<Straggler>,
    ) -> (Arc<ParallelJob>, Results) {
        let chunk_items = plan.chunk.unwrap_or(CALIBRATION_ITEMS).max(1);
        let (results, rx) = mpsc::unbounded_channel();
        let outstanding = Arc::new(AtomicU64::new(0));
        let job = Arc::new(ParallelJob {
            outstanding: outstanding.clone(),
            plan,
            started: Instant::now(),
            lanes: AtomicU32::new(1),
            straggler,
            queue: Mutex::new(Queue {
                cursor: 0,
                requeue: VecDeque::new(),
                inflight: HashMap::new(),
                next_chunk: 0,
                chunk_items,
                item_micros: None,
                failed: 0,
                nodes: BTreeMap::new(),
                cancelled: false,
                accounted: Default::default(),
                speculative: VecDeque::new(),
                speculated: 0,
            }),
            results,
        });
        (job, Results { rx, outstanding })
    }

    pub fn id(&self) -> JobId {
        self.plan.job
    }

    pub fn items(&self) -> &[String] {
        &self.plan.items
    }

    pub fn total(&self) -> u64 {
        self.plan.items.len() as u64
    }

    /// Credit items a re-homing worker already finished under the previous
    /// owner (M9 §5.4). Their output is gone with that owner, but the work
    /// is not, and re-running it would be the "duplicate execution" an
    /// adoption is supposed to avoid.
    ///
    /// Only items that have not yet been handed out here can be credited:
    /// once a chunk is in flight, letting a late claim retire it would
    /// leave the queue believing work is done that nobody is doing.
    pub fn credit_completed(&self, worker: NodeId, indices: &[u32]) {
        if indices.is_empty() {
            return;
        }
        let mut q = self.queue.lock();
        let mut credited = 0u64;
        for &index in indices {
            if (index as usize) < self.plan.items.len() && q.accounted.insert(index) {
                credited += 1;
            }
        }
        if credited == 0 {
            return;
        }
        let stat = q.nodes.entry(worker).or_default();
        stat.items += credited;
    }

    /// Register how many lanes a worker brings, so chunk sizing can keep
    /// the tail balanced across all of them.
    pub fn add_lanes(&self, lanes: u32) {
        self.lanes.fetch_add(lanes.max(1), Ordering::Relaxed);
    }

    /// Hand `worker` its next chunk, or `None` when the list is exhausted.
    ///
    /// Re-queued chunks go first: they are the oldest outstanding work, and
    /// leaving them until last is how a job ends up waiting on one straggler
    /// slice after everything else has finished.
    pub fn next_chunk(&self, worker: NodeId, worker_name: &str) -> Pull {
        let mut q = self.queue.lock();
        if q.cancelled {
            return Pull::Done;
        }
        // **Backpressure** (ENG-26). Finished output the client has not
        // taken yet is memory this owner is holding, and handing out more
        // work is what produces more of it. `Wait` is the honest answer —
        // the run is not over, there is simply nothing to give this lane
        // right now — and it is the answer a straggler's lane already
        // gets, so every worker and every version already handles it.
        //
        // Never *refuses* a worker permanently: the moment the client
        // reads, the count falls and the next pull is served.
        if self.outstanding.load(Ordering::Relaxed) > MAX_OUTSTANDING_BYTES {
            return Pull::Wait;
        }
        self.maintain(&mut q);
        let chunk = loop {
            match q.requeue.pop_front() {
                Some(chunk) => break chunk,
                None => {
                    // Skip anything already finished — credited by a
                    // re-homing worker, or won by a speculative twin.
                    while q.cursor < self.plan.items.len()
                        && q.accounted.contains(&(q.cursor as u32))
                    {
                        q.cursor += 1;
                    }
                    if q.cursor >= self.plan.items.len() {
                        // Nothing fresh left. This is where the tail lives,
                        // and where a second copy of somebody else's slow
                        // chunk is worth a lane (M9 §5.6).
                        break match self.take_speculation(&mut q, worker) {
                            Some(chunk) => chunk,
                            // Nothing to race *yet*. If anything is still
                            // outstanding, this lane waits for it rather
                            // than retiring — see [`Pull`].
                            None if !q.inflight.is_empty() => return Pull::Wait,
                            None => return Pull::Done,
                        };
                    }
                    // Record the size we settled on: progress reporting —
                    // and the Director view built on it — is meant to show
                    // the adaptation happening, not the seed value.
                    q.chunk_items = self.size_for(&q);
                    let size = q.chunk_items as usize;
                    let start = q.cursor;
                    let mut len = size.min(self.plan.items.len() - start);
                    // A chunk never spans a finished item: stop short.
                    if let Some(hole) =
                        (start..start + len).find(|i| q.accounted.contains(&(*i as u32)))
                    {
                        len = hole - start;
                    }
                    q.cursor = start + len.max(1).min(self.plan.items.len() - start);
                    if len == 0 {
                        continue;
                    }
                    let id = q.next_chunk;
                    q.next_chunk += 1;
                    break Chunk { id, start, len };
                }
            }
        };
        let now = Instant::now();
        let expires = now + self.grace(&q, chunk.len);
        q.inflight.insert(
            chunk.id,
            Inflight {
                chunk,
                worker,
                expires,
                issued: now,
                speculated: false,
            },
        );
        q.nodes.entry(worker).or_insert_with(|| NodeStat {
            name: worker_name.to_string(),
            ..NodeStat::default()
        });
        Pull::Work(Box::new(ChunkAssignment {
            job: self.plan.job,
            chunk: chunk.id,
            start: u32::try_from(chunk.start).unwrap_or(u32::MAX),
            count: u32::try_from(chunk.len).unwrap_or(u32::MAX),
        }))
    }

    /// Take a pending speculative copy this worker is allowed to run.
    ///
    /// "Allowed" means: not the node already running the original, and not
    /// a range that has since finished. A copy gets a **fresh chunk id**
    /// rather than reusing the original's, so both attempts sit in the
    /// inflight map at once and whichever reports first is simply the one
    /// that reports first (§5.6).
    fn take_speculation(&self, q: &mut Queue, worker: NodeId) -> Option<Chunk> {
        for _ in 0..q.speculative.len() {
            let Some(spec) = q.speculative.pop_front() else {
                break;
            };
            if spec.chunk.indices().all(|i| q.accounted.contains(&i)) {
                continue; // the original won while this waited
            }
            if spec.avoid == worker {
                q.speculative.push_back(spec);
                continue;
            }
            let id = q.next_chunk;
            q.next_chunk += 1;
            q.speculated += 1;
            return Some(Chunk { id, ..spec.chunk });
        }
        None
    }

    /// Record a finished chunk. Returns the outcomes to stream, paired with
    /// the item each belongs to. A report for a chunk that was already
    /// re-queued and redone is ignored — that is what makes re-issuing a
    /// chunk safe rather than double-counted.
    pub fn report(&self, worker: NodeId, report: &ChunkReport) -> Option<Vec<ItemOutcome>> {
        let mut q = self.queue.lock();
        let chunk = q.inflight.remove(&report.chunk)?.chunk;
        let mut outcomes = Vec::with_capacity(chunk.len);
        let mut total_micros = 0u64;
        let mut reported = 0usize;
        for (offset, outcome) in report.outcomes.iter().take(chunk.len).enumerate() {
            reported += 1;
            let index = u32::try_from(chunk.start + offset).unwrap_or(u32::MAX);
            total_micros = total_micros.saturating_add(outcome.micros);
            // Already accounted means a speculative twin got here first —
            // or a re-homing worker credited it. Either way this copy is
            // the loser, and a loser contributes nothing: not a count, not
            // a failure, and not a line of output. That is what keeps a
            // duplicate from being reported as a phantom (M9 §5.6).
            if !q.accounted.insert(index) {
                continue;
            }
            let mut outcome = outcome.clone();
            // Trust the owner's own list for item identity: a worker names
            // the item it ran, but the queue is the record of what was asked.
            outcome.item = self.plan.items[chunk.start + offset].clone();
            if !outcome.succeeded() {
                q.failed += 1;
            }
            outcomes.push(outcome);
        }
        // A short report (a worker that died half way) leaves the rest of
        // the chunk unaccounted for: re-queue the remainder.
        if reported < chunk.len {
            q.requeue.push_back(Chunk {
                id: chunk.id,
                start: chunk.start + reported,
                len: chunk.len - reported,
            });
        }
        // Whatever this chunk covered is settled; any other copy of it
        // still running is now pointless work. Retire the twin so the job
        // does not wait on a race it has already won.
        self.retire_covered(&mut q);

        let counted = outcomes.len();
        let stat = q.nodes.entry(worker).or_default();
        stat.items += counted as u64;
        stat.chunks += 1;
        stat.micros = stat.micros.saturating_add(total_micros);
        let worker_name = stat.name.clone();

        if reported > 0 {
            let observed = total_micros as f64 / reported as f64;
            q.item_micros = Some(match q.item_micros {
                Some(prev) => prev * (1.0 - EWMA_ALPHA) + observed * EWMA_ALPHA,
                None => observed,
            });
        }
        drop(q);
        // A closed receiver means the client hung up; the owner task
        // notices separately and cancels. Losing the frame is not an error.
        let result = ChunkResult {
            worker,
            worker_name,
            outcomes: outcomes.clone(),
        };
        self.outstanding
            .fetch_add(weigh(&result), Ordering::Relaxed);
        let _ = self.results.send(result);
        Some(outcomes)
    }

    /// Every chunk this worker holds goes back on the queue — it is gone,
    /// and its share of the list must not go with it.
    pub fn release_worker(&self, worker: &NodeId) {
        let mut q = self.queue.lock();
        let orphaned: Vec<u32> = q
            .inflight
            .iter()
            .filter(|(_, f)| f.worker == *worker)
            .map(|(id, _)| *id)
            .collect();
        for id in orphaned {
            if let Some(f) = q.inflight.remove(&id) {
                q.requeue.push_back(f.chunk);
            }
        }
    }

    pub fn cancel(&self) {
        self.queue.lock().cancelled = true;
    }

    /// Nothing left to hand out and nothing outstanding.
    pub fn finished(&self) -> bool {
        let mut q = self.queue.lock();
        if q.cancelled {
            return true;
        }
        // Every item accounted for is the definitive answer, and it is the
        // one speculation needs: a losing duplicate may still be running
        // somewhere, and the job must not wait on it.
        if q.accounted.len() >= self.plan.items.len() {
            return true;
        }
        self.maintain(&mut q);
        q.cursor >= self.plan.items.len() && q.requeue.is_empty() && q.inflight.is_empty()
    }

    pub fn progress(&self) -> Progress {
        let q = self.queue.lock();
        let inflight = q
            .inflight
            .values()
            .map(|f| {
                f.chunk
                    .indices()
                    .filter(|i| !q.accounted.contains(i))
                    .count() as u64
            })
            .sum();
        // Throughput is items over *elapsed* time, not over the summed
        // duration of the items themselves. The latter measures what one
        // item costs — the same on two identical machines however many
        // lanes each is running — and would report a 32-core node and a
        // 16-core node as equally fast while one did twice the work.
        let elapsed = self.started.elapsed().as_secs_f64().max(1e-6);
        let per_node = q
            .nodes
            .iter()
            .map(|(node, stat)| NodeProgress {
                node: *node,
                name: stat.name.clone(),
                items: stat.items,
                chunks: stat.chunks,
                items_per_sec: stat.items as f64 / elapsed,
            })
            .collect();
        Progress {
            done: q.accounted.len() as u64,
            failed: q.failed,
            inflight,
            chunk_items: q.chunk_items,
            speculated: q.speculated,
            per_node,
        }
    }

    /// Chunk size for the next hand-out, in items.
    ///
    /// Adaptive by default: aim at [`TARGET_CHUNK`] of work, and never hand
    /// out so much that the remaining list cannot keep every lane busy — a
    /// job that gives its last worker a 400-item chunk finishes when *that*
    /// chunk finishes, however idle everyone else is.
    fn size_for(&self, q: &Queue) -> u32 {
        if let Some(fixed) = self.plan.chunk {
            return fixed.max(1);
        }
        let Some(item_micros) = q.item_micros else {
            return CALIBRATION_ITEMS;
        };
        let target = TARGET_CHUNK.as_micros() as f64 / item_micros.max(1.0);
        let by_duration = target.clamp(1.0, f64::from(MAX_CHUNK_ITEMS)) as u32;
        let remaining = self.plan.items.len().saturating_sub(q.cursor) as u32;
        let lanes = self.lanes.load(Ordering::Relaxed).max(1);
        // Two chunks per lane left in the list keeps the tail balanced.
        let by_tail = (remaining / lanes.saturating_mul(2)).max(1);
        by_duration.min(by_tail).max(1)
    }

    /// How long to wait for a chunk before assuming its worker is gone.
    fn grace(&self, q: &Queue, items: usize) -> Duration {
        let expected = match q.item_micros {
            Some(micros) => Duration::from_micros((micros * items as f64) as u64),
            None => TARGET_CHUNK,
        };
        let per_item_timeout = self
            .plan
            .timeout_secs
            .map(|s| Duration::from_secs(s) * items.min(u32::MAX as usize) as u32);
        let floor = per_item_timeout.unwrap_or(CHUNK_GRACE).max(CHUNK_GRACE);
        (expected * 4).max(floor)
    }

    /// Housekeeping run on every pull and every liveness check: reclaim
    /// chunks whose worker went silent, retire chunks a twin already
    /// finished, and — at the tail — line up duplicates of whatever is
    /// still outstanding.
    fn maintain(&self, q: &mut Queue) {
        self.sweep_expired(q);
        self.retire_covered(q);
        self.speculate(q);
    }

    fn sweep_expired(&self, q: &mut Queue) {
        let now = Instant::now();
        let expired: Vec<u32> = q
            .inflight
            .iter()
            .filter(|(_, f)| f.expires <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(f) = q.inflight.remove(&id) {
                // Only the part nobody finished goes back on the queue: a
                // speculative twin may already have covered some of it.
                for (start, len) in unaccounted_runs(&f.chunk, &q.accounted) {
                    q.requeue.push_back(Chunk {
                        id: f.chunk.id,
                        start,
                        len,
                    });
                }
            }
        }
    }

    /// Drop inflight chunks whose every item is already accounted for.
    /// This is how the loser of a speculative race stops being something
    /// the job waits on.
    fn retire_covered(&self, q: &mut Queue) {
        let settled: Vec<u32> = q
            .inflight
            .iter()
            .filter(|(_, f)| f.chunk.indices().all(|i| q.accounted.contains(&i)))
            .map(|(id, _)| *id)
            .collect();
        for id in settled {
            q.inflight.remove(&id);
        }
        q.speculative
            .retain(|spec| !spec.chunk.indices().all(|i| q.accounted.contains(&i)));
    }

    /// Line up a second copy of every straggler (M9 §5.6).
    ///
    /// Two conditions, and both matter. The job must be into its tail —
    /// before that, an outstanding chunk is just work, and duplicating it
    /// spends a lane for nothing. And the chunk must have been outstanding
    /// for [`SPECULATE_AFTER`] times what its items should have cost —
    /// otherwise crossing the threshold would duplicate everything in
    /// flight at that instant, including the chunk handed out a moment ago.
    ///
    /// The precondition for all of this already held before M9: acceptance
    /// is idempotent and items are content-addressed, so running one twice
    /// is safe. Speculation only decides *when* it is worth it.
    fn speculate(&self, q: &mut Queue) {
        let Some(policy) = self.straggler else {
            return;
        };
        let total = self.plan.items.len();
        if total == 0 {
            return;
        }
        // Either gate opens it. The fraction is the classic tail rule; the
        // exhausted list is the one that matters when a node freezes
        // holding a large share, because then `done` never reaches the
        // fraction at all (see [`DEFAULT_SPECULATE_AT`]).
        //
        // Racing on an exhausted list is close to free besides: there is
        // nothing fresh for a lane to do instead, so the only cost of a
        // duplicate is electricity, and the payoff is not waiting out a
        // node that has stopped answering.
        let nothing_fresh = q.cursor >= total && q.requeue.is_empty();
        let at_the_tail = (q.accounted.len() as f64 / total as f64) >= policy.at;
        if !nothing_fresh && !at_the_tail {
            return;
        }
        let now = Instant::now();
        let per_item = match q.item_micros {
            Some(micros) => Duration::from_micros((micros * f64::from(SPECULATE_AFTER)) as u64),
            None => TARGET_CHUNK,
        };
        let candidates: Vec<u32> = q
            .inflight
            .iter()
            .filter(|(_, f)| !f.speculated)
            .filter(|(_, f)| {
                let expected = per_item * u32::try_from(f.chunk.len).unwrap_or(1);
                now.duration_since(f.issued) >= expected.max(policy.min_wait)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in candidates {
            let Some(f) = q.inflight.get_mut(&id) else {
                continue;
            };
            f.speculated = true;
            let (chunk, avoid) = (f.chunk, f.worker);
            // Only the part still outstanding is worth racing.
            for (start, len) in unaccounted_runs(&chunk, &q.accounted) {
                q.speculative.push_back(Speculation {
                    chunk: Chunk {
                        id: chunk.id,
                        start,
                        len,
                    },
                    avoid,
                });
            }
        }
    }
}

/// The contiguous stretches of a chunk that nobody has finished, as
/// `(start, len)` pairs. A chunk is usually either wholly outstanding or
/// wholly done; this exists for the case in between, where a twin covered
/// part of it.
fn unaccounted_runs(
    chunk: &Chunk,
    accounted: &std::collections::HashSet<u32>,
) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut run: Option<(usize, usize)> = None;
    for offset in 0..chunk.len {
        let index = u32::try_from(chunk.start + offset).unwrap_or(u32::MAX);
        if accounted.contains(&index) {
            if let Some(open) = run.take() {
                runs.push(open);
            }
        } else {
            match &mut run {
                Some((_, len)) => *len += 1,
                None => run = Some((chunk.start + offset, 1)),
            }
        }
    }
    if let Some(open) = run {
        runs.push(open);
    }
    runs
}

// ---------------------------------------------------------------------------
// The worker's side
// ---------------------------------------------------------------------------

/// A parallel job this node is *working on* for someone else.
///
/// The worker holds the whole plan, not just the chunk it was handed. That
/// is what lets it keep running when the owner dies: every participant can
/// rebuild the queue, and they all rank the same successor from the same
/// job id, so re-homing needs no coordination round (M9 §5.4).
#[derive(Debug)]
pub struct WorkerJob {
    pub plan: Arc<ParallelPlan>,
    state: Mutex<WorkerState>,
}

#[derive(Debug)]
struct WorkerState {
    owner: NodeId,
    lanes: u32,
    /// Item indices finished here, whoever owned the job at the time.
    completed: std::collections::BTreeSet<u32>,
    /// The owner we have already declared our completed set to. Declaring
    /// it again would credit the same items twice.
    announced_to: Option<NodeId>,
}

impl WorkerJob {
    pub fn new(plan: Arc<ParallelPlan>, owner: NodeId) -> Arc<WorkerJob> {
        Arc::new(WorkerJob {
            plan,
            state: Mutex::new(WorkerState {
                owner,
                lanes: 0,
                completed: std::collections::BTreeSet::new(),
                announced_to: Some(owner),
            }),
        })
    }

    pub fn owner(&self) -> NodeId {
        self.state.lock().owner
    }

    pub fn set_owner(&self, owner: NodeId) {
        self.state.lock().owner = owner;
    }

    pub fn add_lanes(&self, lanes: u32) {
        self.state.lock().lanes += lanes;
    }

    /// A lane stopped. Returns how many are still running.
    pub fn lane_finished(&self) -> u32 {
        let mut state = self.state.lock();
        state.lanes = state.lanes.saturating_sub(1);
        state.lanes
    }

    pub fn record_done(&self, indices: impl IntoIterator<Item = u32>) {
        let mut state = self.state.lock();
        state.completed.extend(indices);
    }

    /// What to declare on the next pull: the items finished here that this
    /// owner cannot know about, and only the first time we ask it.
    pub fn claim_for(&self, owner: NodeId) -> Vec<u32> {
        let mut state = self.state.lock();
        if state.announced_to == Some(owner) {
            return Vec::new();
        }
        state.announced_to = Some(owner);
        state.completed.iter().copied().collect()
    }

    /// Everything finished here, for an adopting owner to credit itself.
    pub fn completed(&self) -> Vec<u32> {
        self.state.lock().completed.iter().copied().collect()
    }
}

/// Parallel jobs this node is working on for other owners.
#[derive(Debug, Default)]
pub struct WorkerRegistry {
    jobs: Mutex<HashMap<JobId, Arc<WorkerJob>>>,
}

impl WorkerRegistry {
    /// Register a plan, or return the one already held. The flag says
    /// whether this offer is new — a re-offer from an adopting owner finds
    /// the job already here and must not start a second set of lanes.
    pub fn offer(&self, plan: Arc<ParallelPlan>, owner: NodeId) -> (Arc<WorkerJob>, bool) {
        let mut jobs = self.jobs.lock();
        match jobs.get(&plan.job) {
            Some(existing) => {
                existing.set_owner(owner);
                (existing.clone(), false)
            }
            None => {
                let job = WorkerJob::new(plan.clone(), owner);
                jobs.insert(plan.job, job.clone());
                (job, true)
            }
        }
    }

    pub fn get(&self, id: &JobId) -> Option<Arc<WorkerJob>> {
        self.jobs.lock().get(id).cloned()
    }

    pub fn remove(&self, id: &JobId) {
        self.jobs.lock().remove(id);
    }
}

/// Who takes over a parallel job when its owner dies (M9 §5.4).
///
/// Rendezvous ranking over the plan's worker set, minus anyone the mesh
/// believes is gone. Every participant computes this from the same inputs
/// and reaches the same answer, so adoption costs no election and no
/// consensus round — the same primitive per-job ownership already uses
/// (MASTERPLAN §10.3).
pub fn successor(plan: &ParallelPlan, is_live: impl Fn(&NodeId) -> bool) -> Option<NodeId> {
    let live: Vec<NodeId> = plan
        .workers
        .iter()
        .copied()
        .filter(|n| is_live(n))
        .collect();
    crate::scheduler::rendezvous_rank(b"parallel", &plan.job.0, live)
        .into_iter()
        .next()
}

/// Every parallel job this node owns right now. Pure memory: a restart
/// forgets them, exactly as it forgets liveness (M9 §5.4).
#[derive(Debug, Default)]
pub struct ParallelRegistry {
    jobs: Mutex<HashMap<JobId, Arc<ParallelJob>>>,
}

impl ParallelRegistry {
    pub fn insert(&self, job: Arc<ParallelJob>) {
        self.jobs.lock().insert(job.id(), job);
    }

    pub fn get(&self, id: &JobId) -> Option<Arc<ParallelJob>> {
        self.jobs.lock().get(id).cloned()
    }

    /// Take ownership only if nobody here already has: the guard against
    /// two adoption paths driving one job.
    pub fn adopt(&self, job: Arc<ParallelJob>) -> bool {
        let mut jobs = self.jobs.lock();
        match jobs.entry(job.id()) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(job);
                true
            }
        }
    }

    pub fn remove(&self, id: &JobId) {
        self.jobs.lock().remove(id);
    }

    /// Stop handing out chunks for every job this node owns. Called on
    /// shutdown: a node on its way out should not keep feeding work to
    /// lanes whose results it will never record.
    pub fn cancel_all(&self) {
        for job in self.jobs.lock().values() {
            job.cancel();
        }
    }

    pub fn len(&self) -> usize {
        self.jobs.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_proto::UserId;

    /// A job plus the results receiver, which tests must keep alive: an
    /// unbounded send to a dropped receiver silently discards the frame.
    fn job_with_results(items: usize, fixed: Option<u32>) -> (Arc<ParallelJob>, Results) {
        ParallelJob::own(Arc::new(ParallelPlan {
            job: JobId::generate(),
            command: "echo {}".into(),
            exec: None,
            item_input: None,
            items: (0..items).map(|i| i.to_string()).collect(),
            env: vec![],
            timeout_secs: None,
            chunk: fixed,
            capture: mesh_proto::Capture::Inline,
            inputs: vec![],
            workers: vec![],
            submitter_user: UserId([0; 32]),
            auth: mesh_proto::SignedIpc {
                user: UserId([0; 32]),
                at_micros: 0,
                nonce: 0,
                payload: vec![],
                signature: vec![],
            },
        }))
    }

    fn report_all(job: &ParallelJob, worker: NodeId, assignment: &ChunkAssignment, micros: u64) {
        let outcomes = assignment
            .slice(job.items())
            .iter()
            .map(|item| ItemOutcome {
                item: item.clone(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                micros,
                output: None,
            })
            .collect();
        job.report(
            worker,
            &ChunkReport {
                job: job.id(),
                chunk: assignment.chunk,
                outcomes,
            },
        );
    }

    #[test]
    fn every_item_is_handed_out_exactly_once() {
        let (job, _results) = job_with_results(100, Some(7));
        let worker = NodeId([1; 32]);
        let mut seen = Vec::new();
        while let Some(a) = job.next_chunk(worker, "w").work() {
            seen.extend(a.slice(job.items()).to_vec());
            report_all(&job, worker, &a, 1_000);
        }
        assert_eq!(seen.len(), 100);
        seen.sort_by_key(|s| s.parse::<usize>().unwrap_or(0));
        seen.dedup();
        assert_eq!(seen.len(), 100, "no item handed out twice");
        assert!(job.finished());
        let p = job.progress();
        assert_eq!((p.done, p.failed, p.inflight), (100, 0, 0));
    }

    #[test]
    fn the_faster_worker_pulls_more() {
        // Two workers, one four times faster. Nobody tells the queue that.
        let (job, _results) = job_with_results(400, Some(1));
        let fast = NodeId([1; 32]);
        let slow = NodeId([2; 32]);
        let (mut fast_items, mut slow_items) = (0, 0);
        // Interleave pulls in proportion to speed: the fast worker finishes
        // four chunks in the time the slow one finishes one.
        loop {
            let mut progressed = false;
            for _ in 0..4 {
                if let Some(a) = job.next_chunk(fast, "fast").work() {
                    fast_items += a.count as usize;
                    report_all(&job, fast, &a, 1_000);
                    progressed = true;
                }
            }
            if let Some(a) = job.next_chunk(slow, "slow").work() {
                slow_items += a.count as usize;
                report_all(&job, slow, &a, 4_000);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        assert_eq!(fast_items + slow_items, 400);
        let ratio = fast_items as f64 / slow_items as f64;
        assert!(
            (3.0..=5.0).contains(&ratio),
            "the 4x-faster worker should pull ~4x the items, got {fast_items}:{slow_items}"
        );
    }

    /// A client that stops reading must not make the owner hold the whole
    /// run's captured output (ENG-26).
    ///
    /// The channel cannot be bounded — `report` runs under the queue lock
    /// in a synchronous function, so it can neither await a bounded send
    /// nor drop a finished chunk, which is somebody's answer — so the
    /// bound is applied where work is handed out instead.
    #[test]
    fn a_client_that_stops_reading_stops_work_being_handed_out() {
        let (job, mut results) = job_with_results(100_000, Some(1));
        let worker = NodeId([7; 32]);

        // The premise: an idle run hands out work freely.
        assert!(
            matches!(job.next_chunk(worker, "w"), Pull::Work(_)),
            "an idle run should hand out work"
        );

        // Report chunks with real captured output, and never read them.
        let mut handed = 1usize;
        let big = "x".repeat(64 * 1024);
        for _ in 0..4_000 {
            let Pull::Work(a) = job.next_chunk(worker, "w") else {
                break;
            };
            handed += 1;
            job.report(
                worker,
                &ChunkReport {
                    job: job.id(),
                    chunk: a.chunk,
                    outcomes: vec![ItemOutcome {
                        item: String::new(),
                        exit_code: Some(0),
                        stdout: big.clone(),
                        stderr: String::new(),
                        micros: 1,
                        output: None,
                    }],
                },
            );
        }
        assert!(
            handed < 4_000,
            "the owner handed out {handed} chunks while holding every result"
        );
        assert!(
            matches!(job.next_chunk(worker, "w"), Pull::Wait),
            "a backed-up run should answer Wait rather than hand out more"
        );

        // …and `Wait` is never a refusal: the moment the client reads, the
        // run continues.
        while results.try_recv().is_ok() {}
        assert!(
            matches!(job.next_chunk(worker, "w"), Pull::Work(_)),
            "draining the results should let work flow again"
        );
    }

    #[test]
    fn chunk_size_adapts_to_what_an_item_costs() {
        let (job, _results) = job_with_results(100_000, None);
        let worker = NodeId([1; 32]);
        // Calibration: one item.
        let first = job.next_chunk(worker, "w").work().expect("first chunk");
        assert_eq!(first.count, 1, "the first chunk calibrates");
        // 1 ms per item → a 5 s chunk is ~5,000 items, capped by the tail
        // rule and the hard maximum.
        report_all(&job, worker, &first, 1_000);
        let second = job.next_chunk(worker, "w").work().expect("second chunk");
        assert!(
            second.count > 100,
            "cheap items should batch up, got {}",
            second.count
        );
        assert!(second.count <= MAX_CHUNK_ITEMS);
        // Progress reports the size actually in use, not the seed.
        assert_eq!(job.progress().chunk_items, second.count);

        // Expensive items go the other way: 10 s each means one at a time.
        let (job, _results) = job_with_results(1_000, None);
        let first = job.next_chunk(worker, "w").work().expect("first chunk");
        report_all(&job, worker, &first, 10_000_000);
        let second = job.next_chunk(worker, "w").work().expect("second chunk");
        assert_eq!(second.count, 1, "a 10 s item is a chunk by itself");
    }

    #[test]
    fn a_lost_worker_returns_its_chunks_to_the_queue() {
        let (job, _results) = job_with_results(10, Some(5));
        let gone = NodeId([1; 32]);
        let alive = NodeId([2; 32]);
        let a = job.next_chunk(gone, "gone").work().expect("chunk");
        assert_eq!(a.count, 5);
        assert!(!job.finished(), "work is outstanding");

        job.release_worker(&gone);
        // The same items come back, to whoever asks next.
        let b = job
            .next_chunk(alive, "alive")
            .work()
            .expect("requeued chunk");
        assert_eq!(
            b.slice(job.items()),
            a.slice(job.items()),
            "the lost slice is handed out again"
        );
        report_all(&job, alive, &b, 1_000);

        // The dead worker's late report is ignored rather than double-counted.
        report_all(&job, gone, &a, 1_000);
        let c = job.next_chunk(alive, "alive").work().expect("second chunk");
        report_all(&job, alive, &c, 1_000);
        assert_eq!(job.progress().done, 10, "each item counted once");
        assert!(job.finished());
    }

    #[test]
    fn a_short_report_requeues_the_remainder() {
        let (job, _results) = job_with_results(10, Some(10));
        let worker = NodeId([1; 32]);
        let a = job.next_chunk(worker, "w").work().expect("chunk");
        // The worker managed three items before dying.
        let outcomes = (0..3)
            .map(|i| ItemOutcome {
                item: i.to_string(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                micros: 1_000,
                output: None,
            })
            .collect();
        job.report(
            worker,
            &ChunkReport {
                job: job.id(),
                chunk: a.chunk,
                outcomes,
            },
        );
        assert!(!job.finished(), "seven items are still owed");
        let b = job.next_chunk(worker, "w").work().expect("remainder");
        assert_eq!(b.count, 7);
        assert_eq!(
            b.slice(job.items())[0],
            "3",
            "the remainder resumes where it stopped"
        );
    }

    #[test]
    fn throughput_reflects_how_much_a_node_did_not_how_fast_one_item_is() {
        // Two workers with identical per-item cost, but one takes twice the
        // items. Throughput must separate them — that is the whole point of
        // the per-node view.
        let (job, _results) = job_with_results(30, Some(1));
        let big = NodeId([1; 32]);
        let small = NodeId([2; 32]);
        for i in 0..30 {
            let worker = if i % 3 == 2 { small } else { big };
            let a = job
                .next_chunk(worker, if worker == big { "big" } else { "small" })
                .work()
                .expect("chunk");
            report_all(&job, worker, &a, 1_000);
        }
        let per_node = job.progress().per_node;
        let rate = |id: NodeId| {
            per_node
                .iter()
                .find(|n| n.node == id)
                .map_or(0.0, |n| n.items_per_sec)
        };
        assert!(
            rate(big) > rate(small) * 1.5,
            "the busier node must read faster"
        );
    }

    #[test]
    fn failures_are_counted_without_stopping_the_job() {
        let (job, _results) = job_with_results(4, Some(2));
        let worker = NodeId([1; 32]);
        let a = job.next_chunk(worker, "w").work().expect("chunk");
        job.report(
            worker,
            &ChunkReport {
                job: job.id(),
                chunk: a.chunk,
                outcomes: vec![
                    ItemOutcome {
                        item: "0".into(),
                        exit_code: Some(1),
                        stdout: String::new(),
                        stderr: "boom".into(),
                        micros: 10,
                        output: None,
                    },
                    ItemOutcome {
                        item: "1".into(),
                        exit_code: Some(0),
                        stdout: String::new(),
                        stderr: String::new(),
                        micros: 10,
                        output: None,
                    },
                ],
            },
        );
        let p = job.progress();
        assert_eq!((p.done, p.failed), (2, 1));
        assert!(
            job.next_chunk(worker, "w").work().is_some(),
            "the job carries on"
        );
    }
}
