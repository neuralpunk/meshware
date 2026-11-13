//! In-process multi-node test harness (MASTERPLAN §16, testing doctrine).
//!
//! Spawns real nodes — real identity, real QUIC on loopback, real genesis,
//! real gossip — inside one process, each with an isolated temp data dir.
//! Simulation-first: mesh behavior is exercised here before any real network.

pub mod coldstart;
pub mod nat;
pub mod netsplit;
pub mod ops;

use std::sync::Arc;
use std::time::Duration;

use mesh_core::config::{NodeConfig, Overrides};
use mesh_core::daemon::Node;
use mesh_core::identity::Keypair;
use mesh_core::membership;
use mesh_proto::{IpcRequest, IpcResponse};
use tokio::sync::mpsc;

/// A running in-process node plus its isolated home directory.
pub struct TestNode {
    pub node: Node,
    _dir: tempfile::TempDir,
    config: NodeConfig,
}

impl std::fmt::Debug for TestNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestNode({})", self.node.node_id().short())
    }
}

impl TestNode {
    pub fn addr_string(&self) -> String {
        format!("127.0.0.1:{}", self.node.listen_addr().port())
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// Stop the node completely and restart it on the same data dir — the
    /// in-process equivalent of a binary upgrade's stop/start cycle
    /// (MASTERPLAN §12 rolling upgrades). Identity, genesis, op-log, and
    /// job journals persist; the restarted node re-announces itself.
    pub async fn restart(self) -> TestNode {
        self.node.stop().await;
        let TestNode { node, _dir, config } = self;
        drop(node); // release the store lock (all task handles were reaped)
        // A short-lived request task may still hold an engine reference for
        // a moment; the store's exclusive lock clears when it drops.
        let mut last: Option<String> = None;
        for _ in 0..40 {
            match Node::start(config.clone()).await {
                Ok(node) => {
                    return TestNode { node, _dir, config };
                }
                Err(e) => {
                    last = Some(e.to_string());
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        panic!(
            "restart on same data dir kept failing: {}",
            last.unwrap_or_default()
        );
    }

    /// Stop the node without restarting (simulates a graceful exit).
    pub async fn stop(&self) {
        self.node.stop().await;
    }
}

/// A resolved test config for a node that has not started yet — for tests
/// that need to drive `join_mesh` themselves rather than through
/// [`join_new_node_with`] (e.g. asserting that a join is *refused*).
pub fn test_config_for(
    dir: &std::path::Path,
    name: &str,
    mesh_name: &str,
    tags: Vec<String>,
) -> NodeConfig {
    test_config(dir, name, mesh_name, tags)
}

fn test_config(
    dir: &std::path::Path,
    name: &str,
    mesh_name: &str,
    tags: Vec<String>,
) -> NodeConfig {
    let mut config = NodeConfig::load(
        None,
        &Overrides {
            data_dir: Some(dir.to_path_buf()),
            listen: Some("127.0.0.1:0".to_string()),
            node_name: Some(name.to_string()),
            mesh_name: Some(mesh_name.to_string()),
            tags: Some(tags),
            advertise: None,
        },
    )
    .expect("test config must resolve");
    // No scrape endpoint by default: hundreds of in-process nodes must not
    // race for one port. Tests that need it opt in with port 0.
    config.http_addr = None;
    config
}

/// Spawn a node that founds its own brand-new mesh.
/// Grow a node's op-log past `bytes` of encoded history.
///
/// Audit entries, because they are what a busy mesh actually accumulates —
/// they were 17% of the op-log that first hit this ceiling in the field —
/// and because `AuditAppend` merges on one rule (the entry names its own
/// author), so a test can author thousands without staging users, grants
/// or signatures that have nothing to do with what it is measuring.
///
/// Returns the number of ops written.
pub fn inflate_oplog(node: &Node, bytes: usize) -> usize {
    // Few large entries rather than many small ones: the test cares about
    // the total, and a 64 KiB `detail` reaches 4 MiB in 64 ops instead of
    // 40,000 signatures.
    const PER_OP: usize = 64 * 1024;
    let author = node.node_id();
    let count = bytes.div_ceil(PER_OP);
    let bodies: Vec<mesh_proto::OpBody> = (0..count)
        .map(|i| mesh_proto::OpBody::AuditAppend {
            entry: mesh_proto::AuditEntry {
                audit_id: mesh_proto::AuditId::generate(),
                at_micros: mesh_proto::now_micros(),
                user: mesh_proto::UserId([0u8; 32]),
                username: "bulk".into(),
                action: "test.bulk".into(),
                target: format!("{i}"),
                node: author,
                outcome: "ok".into(),
                detail: "x".repeat(PER_OP),
                request_sig: Vec::new(),
            },
        })
        .collect();
    node.engine().author_ops(bodies).expect("author bulk ops");
    count
}

pub async fn spawn_founder(name: &str, mesh_name: &str) -> TestNode {
    spawn_founder_tagged(name, mesh_name, vec![]).await
}

pub async fn spawn_founder_tagged(name: &str, mesh_name: &str, tags: Vec<String>) -> TestNode {
    spawn_founder_with(name, mesh_name, tags, |_| {}).await
}

/// Spawn a founder with a hook to adjust the resolved config first
/// (e.g. registering secret provider plugins).
pub async fn spawn_founder_with(
    name: &str,
    mesh_name: &str,
    tags: Vec<String>,
    mutate: impl FnOnce(&mut NodeConfig),
) -> TestNode {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(dir.path(), name, mesh_name, tags);
    mutate(&mut config);
    let node = Node::start(config.clone()).await.expect("node start");
    TestNode {
        node,
        _dir: dir,
        config,
    }
}

/// Mint an invite on `founder` and run a full join for a fresh node, then
/// start it. This exercises the real M1 join path end to end.
pub async fn join_new_node(name: &str, founder: &TestNode, tags: Vec<String>) -> TestNode {
    join_new_node_with(name, founder, tags, |_| {}).await
}

/// [`join_new_node`] with a hook to adjust the joiner's resolved config
/// first (e.g. slots/queue caps for scheduling tests).
pub async fn join_new_node_with(
    name: &str,
    founder: &TestNode,
    tags: Vec<String>,
    mutate: impl FnOnce(&mut NodeConfig),
) -> TestNode {
    // Founder mints an invite pointing at its own address.
    let token = membership::create_invite(
        founder.node.engine(),
        // The founder's user key is the only signer in M1; the test reaches
        // it through the node's identity via a dedicated helper.
        founder_user_key(founder),
        membership::InviteParams {
            ttl_secs: 3600,
            uses: 10,
            tags: vec![],
            bootstrap: vec![founder.addr_string()],
        },
    )
    .expect("create invite");

    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(dir.path(), name, "ignored-will-join", tags);
    mutate(&mut config);
    // Client-side join persists genesis + ops into the new data dir.
    membership::join_mesh(&config, &token.encode(), &[])
        .await
        .expect("join mesh");
    // Now start the joined node as a full member.
    let node = Node::start(config.clone())
        .await
        .expect("start joined node");
    TestNode {
        node,
        _dir: dir,
        config,
    }
}

/// Join a fresh node that lives behind `nat` (MASTERPLAN M7 testkit): the
/// join dial and the daemon's transport both route through the NAT; the
/// node's own listen address is a black hole from the outside.
pub async fn join_new_node_natted(
    name: &str,
    founder: &TestNode,
    nat: &Arc<nat::TestNat>,
) -> TestNode {
    join_new_node_natted_with(name, founder, nat, |_| {}).await
}

pub async fn join_new_node_natted_with(
    name: &str,
    founder: &TestNode,
    nat: &Arc<nat::TestNat>,
    mutate: impl FnOnce(&mut NodeConfig),
) -> TestNode {
    let factory = nat.client_factory();
    join_new_node_with(name, founder, vec![], move |c| {
        c.socket_factory = Some(factory);
        mutate(c);
    })
    .await
}

/// Spawn a founder whose transport sits on `side` of `split` (E1-P1).
pub async fn spawn_founder_split(
    name: &str,
    mesh_name: &str,
    split: &Arc<netsplit::Netsplit>,
    side: netsplit::Side,
) -> TestNode {
    let factory = split.factory(side);
    spawn_founder_with(name, mesh_name, vec![], move |c| {
        c.socket_factory = Some(factory);
    })
    .await
}

/// Join a fresh node onto `side` of `split`.
///
/// The join dial goes through the shim too, so a node cannot be joined
/// across a link that is already cut — which is itself a thing worth being
/// able to test.
pub async fn join_new_node_split(
    name: &str,
    founder: &TestNode,
    split: &Arc<netsplit::Netsplit>,
    side: netsplit::Side,
) -> TestNode {
    join_new_node_split_with(name, founder, split, side, |_| {}).await
}

pub async fn join_new_node_split_with(
    name: &str,
    founder: &TestNode,
    split: &Arc<netsplit::Netsplit>,
    side: netsplit::Side,
    mutate: impl FnOnce(&mut NodeConfig),
) -> TestNode {
    let factory = split.factory(side);
    join_new_node_with(name, founder, vec![], move |c| {
        c.socket_factory = Some(factory);
        mutate(c);
    })
    .await
}

/// A declared hardware capacity for tests (M9 §5.1). Every in-process node
/// shares one host, so a heterogeneous mesh — the case the capacity model
/// exists for — has to be stated rather than probed.
pub fn capacity(cores: u32, mem_gb: u64, disk_gb: u64) -> mesh_proto::Capacity {
    mesh_proto::Capacity {
        cores,
        mem_mb: mem_gb * 1024,
        disk_mb: disk_gb * 1024,
        arch: std::env::consts::ARCH.to_string(),
        ports: Some(mesh_proto::PortRange::default()),
        // Stated per node by the test that wants them, not here: a mesh
        // where every node lent `data` would make a volume claim a filter
        // that filters nothing.
        volumes: Vec::new(),
        // Left unknown, which is neutral (T3 §4.3). A test node announcing
        // an empty *list* would be announcing "can run nothing" and would
        // fail every placement, where announcing *nothing* leaves the
        // pre-T3 behaviour every existing test was written against. A test
        // that cares states it with `runtimes()` below.
        runtimes: Vec::new(),
    }
}

/// A declared runtime list for tests (T3).
///
/// Separate from [`capacity`] for the reason the volume list is: a mesh
/// where every node could run everything makes a runtime filter a filter
/// that filters nothing, and the tests that matter are the ones where a
/// node cannot.
pub fn with_runtimes(
    mut cap: mesh_proto::Capacity,
    runtimes: &[mesh_proto::Runtime],
) -> mesh_proto::Capacity {
    cap.runtimes = runtimes.to_vec();
    cap
}

/// Apply a declared capacity to a config, sizing executor slots from it the
/// way a real node does (slots default to cores, MASTERPLAN §11).
fn apply_capacity(config: &mut NodeConfig, cap: mesh_proto::Capacity) {
    config.limits.slots = cap.cores.max(1);
    config.limits.queue = (config.limits.slots * 4).max(8);
    config.capacity_override = Some(cap);
}

/// [`apply_capacity`] for a test that also needs its own config hook — a
/// sized founder with facts turned off, say.
pub fn apply_capacity_for_test(config: &mut NodeConfig, cap: mesh_proto::Capacity) {
    apply_capacity(config, cap);
}

/// Spawn a founder that announces `cap` as its hardware.
pub async fn spawn_founder_sized(
    name: &str,
    mesh_name: &str,
    cap: mesh_proto::Capacity,
) -> TestNode {
    spawn_founder_with(name, mesh_name, vec![], |c| apply_capacity(c, cap)).await
}

/// Join a node that announces `cap` as its hardware.
pub async fn join_new_node_sized(
    name: &str,
    founder: &TestNode,
    cap: mesh_proto::Capacity,
) -> TestNode {
    join_new_node_with(name, founder, vec![], |c| apply_capacity(c, cap)).await
}

/// A synthetic GPU for tests (C5 §9.1).
///
/// Stated rather than probed for the same reason [`capacity`] is: every
/// in-process node shares one host, and CI has no accelerator in it. What
/// the tests exercise is everything downstream of the probe — the entity,
/// the filter tags, admission, allocation and isolation — which is the half
/// that has invariants.
pub fn gpu(index: u32, model: &str, mem_mb: u64) -> mesh_proto::Device {
    mesh_proto::Device {
        index,
        kind: mesh_proto::DeviceKind::Gpu,
        vendor: "nvidia".into(),
        model: model.into(),
        uuid: format!("GPU-test-{index}"),
        mem_mb,
        numa_node: Some(0),
        health: mesh_proto::DeviceHealth::Ok,
    }
}

/// Give a node `n` identical GPUs and publish the inventory, as a probe
/// would on a box that has them.
pub fn advertise_gpus(node: &TestNode, n: u32, model: &str, mem_mb: u64) {
    node.node
        .advertise_devices_for_test((0..n).map(|i| gpu(i, model, mem_mb)).collect());
}

/// The founder's admin user key, for signing test requests and invites.
pub fn founder_user_key(founder: &TestNode) -> &Keypair {
    founder.node.user_key_for_test()
}

/// Sign `req` as `user` and submit it as a single-response request.
pub async fn ipc_as(node: &TestNode, user: &Keypair, req: IpcRequest) -> IpcResponse {
    let envelope = user.sign_request(&req).expect("sign request");
    node.node.handle_signed(envelope).await
}

/// Sign `req` as `user` and submit it as a streaming request, returning the
/// frame receiver (JobStarted / LogChunk* / JobDone | Error | Denied).
pub async fn stream_as(
    node: &TestNode,
    user: &Keypair,
    req: IpcRequest,
) -> mpsc::Receiver<IpcResponse> {
    let envelope = user.sign_request(&req).expect("sign request");
    let (tx, rx) = mpsc::channel::<IpcResponse>(64);
    let n = node.node.clone();
    tokio::spawn(async move {
        n.handle_signed_stream(envelope, tx).await;
    });
    rx
}

/// Outcome of a streamed run driven to completion.
#[derive(Debug)]
pub enum RunOutcome {
    Done {
        stdout: String,
        state: mesh_proto::JobState,
        code: Option<i32>,
        job_id: Option<mesh_proto::JobId>,
    },
    /// The caller's *grants* do not allow this.
    Denied(String),
    Error(String),
}

/// Drive a streaming run request to completion as `user`.
pub async fn run_to_end(node: &TestNode, user: &Keypair, req: IpcRequest) -> RunOutcome {
    let mut rx = stream_as(node, user, req).await;
    let mut stdout = String::new();
    let mut job_id = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::JobStarted { job_id: id, .. } => job_id = Some(id),
            IpcResponse::LogChunk { stream: 1, data } => {
                stdout.push_str(&String::from_utf8_lossy(&data));
            }
            IpcResponse::LogChunk { .. } => {}
            IpcResponse::JobDone { state, exit_code } => {
                return RunOutcome::Done {
                    stdout,
                    state,
                    code: exit_code,
                    job_id,
                };
            }
            IpcResponse::Denied { message } => return RunOutcome::Denied(message),
            IpcResponse::Error { message } => return RunOutcome::Error(message),
            // M19 rows and assertion summaries ride the same stream. They
            // are not what this helper is looking at, and they are not an
            // error either — `results_to_end` is the one that reads them.
            IpcResponse::Result(_) | IpcResponse::Expected { .. } => {}
            // A `Notice` is context, never an outcome: every client may
            // ignore one, and this helper is a client (C1 P5).
            IpcResponse::Notice { .. } => {}
            other => return RunOutcome::Error(format!("unexpected frame: {other:?}")),
        }
    }
    RunOutcome::Error("stream ended with no result".into())
}

/// What a gang submission did, collected off the stream (C4 §8).
///
/// Distinct from [`RunOutcome`] because the interesting part of a gang
/// happens *before* any log byte: how many offer rounds it took, whether it
/// was told it was behind somebody, and whether every member really started
/// together. A helper that only collected stdout could not tell a gang that
/// ran from a gang that never started.
#[derive(Debug, Default)]
pub struct GangReport {
    pub gang: Option<mesh_proto::GangId>,
    /// Rank-ordered node names, from the `GangStarted` frame. Empty when
    /// the gang never committed — which is itself the assertion most of
    /// these tests want to make.
    pub members: Vec<String>,
    /// Offer rounds the driver needed. 1 on an uncontended mesh.
    pub rounds: u32,
    /// One entry per round that could not hold every reservation:
    /// `(held, size, round, the gang it was told was ahead)`.
    pub waits: Vec<(u32, u32, u32, Option<mesh_proto::GangId>)>,
    pub outcome: Option<mesh_proto::GangOutcome>,
    pub succeeded: u32,
    pub failed: u32,
    /// Per-rank stdout, indexed by the rank the `FanChunk` frames carry.
    pub stdout: Vec<String>,
    pub error: Option<String>,
}

impl GangReport {
    pub fn started(&self) -> bool {
        !self.members.is_empty()
    }

    /// Did every member of the gang succeed?
    pub fn ok(&self) -> bool {
        self.outcome == Some(mesh_proto::GangOutcome::Succeeded)
    }

    /// The gang this one was told was ahead of it, if it ever was.
    pub fn behind(&self) -> Option<mesh_proto::GangId> {
        self.waits.iter().find_map(|(_, _, _, g)| *g)
    }
}

/// Drive a gang submission to completion as `user`.
pub async fn gang_to_end(node: &TestNode, user: &Keypair, req: IpcRequest) -> GangReport {
    let mut rx = stream_as(node, user, req).await;
    let mut report = GangReport::default();
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::GangWaiting {
                gang,
                held,
                size,
                round,
                behind,
            } => {
                report.gang = Some(gang);
                report.waits.push((held, size, round, behind));
            }
            IpcResponse::GangStarted {
                gang,
                members,
                rounds,
            } => {
                report.gang = Some(gang);
                report.rounds = rounds;
                report.stdout = vec![String::new(); members.len()];
                report.members = members.into_iter().map(|m| m.node_name).collect();
            }
            IpcResponse::FanChunk {
                index,
                stream: 1,
                data,
            } => {
                if let Some(slot) = report.stdout.get_mut(index as usize) {
                    slot.push_str(&String::from_utf8_lossy(&data));
                }
            }
            IpcResponse::GangDone {
                gang,
                outcome,
                succeeded,
                failed,
            } => {
                report.gang = Some(gang);
                report.outcome = Some(outcome);
                report.succeeded = succeeded;
                report.failed = failed;
                return report;
            }
            IpcResponse::Error { message } => {
                report.error = Some(message);
                return report;
            }
            // Log noise, per-target rows and notices all ride this stream
            // and none of them is what this helper is looking at.
            _ => {}
        }
    }
    if report.error.is_none() {
        report.error = Some("stream ended with no gang outcome".into());
    }
    report
}

/// Everything a streamed submission *answered* with (M19 §5.4).
///
/// Distinct from [`RunOutcome`], which collects log text: a row is what a
/// job answered, and the difference between the two is the milestone.
#[derive(Debug, Default)]
pub struct ResultOutcome {
    pub group: Option<mesh_proto::FanId>,
    pub rows: Vec<mesh_proto::ResultRow>,
    /// `(expression, passed, failed rows)` when `--expect` was used.
    pub expected: Option<(String, u32, Vec<String>)>,
    pub tally: (u32, u32, u32),
    pub state: Option<mesh_proto::JobState>,
    pub stdout: String,
    pub error: Option<String>,
}

impl ResultOutcome {
    pub fn row(&self, node_name: &str) -> Option<&mesh_proto::ResultRow> {
        self.rows.iter().find(|r| r.node_name == node_name)
    }

    /// One output of one row, by node name and key.
    pub fn output(&self, node_name: &str, key: &str) -> Option<&str> {
        self.row(node_name).and_then(|r| r.output(key))
    }
}

/// Drive a submission to completion and collect its result rows.
pub async fn results_to_end(node: &TestNode, user: &Keypair, req: IpcRequest) -> ResultOutcome {
    let mut rx = stream_as(node, user, req).await;
    let mut out = ResultOutcome::default();
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::FanoutStarted { group, .. } => out.group = Some(group),
            IpcResponse::Result(row) => out.rows.push(*row),
            IpcResponse::Expected {
                expression,
                passed,
                failed,
            } => out.expected = Some((expression, passed, failed)),
            IpcResponse::LogChunk { stream: 1, data } => {
                out.stdout.push_str(&String::from_utf8_lossy(&data));
            }
            IpcResponse::FanoutDone {
                succeeded,
                failed,
                skipped,
            } => {
                out.tally = (succeeded, failed, skipped);
                return out;
            }
            IpcResponse::JobDone { state, .. } => {
                out.state = Some(state);
                return out;
            }
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                out.error = Some(message);
                return out;
            }
            _ => {}
        }
    }
    out.error
        .get_or_insert_with(|| "stream ended with no result".into());
    out
}

/// Drain `node` as `user` (MASTERPLAN §12), driving the stream to the
/// terminal `Ok`. Returns how many queued jobs were moved elsewhere.
pub async fn drain_to_end(
    node: &TestNode,
    user: &Keypair,
    move_pending: bool,
) -> Result<u32, String> {
    let mut rx = stream_as(node, user, IpcRequest::Drain { move_pending }).await;
    let mut moved_total = 0;
    while let Some(msg) = rx.recv().await {
        match msg {
            IpcResponse::DrainProgress { moved, .. } => moved_total = moved,
            IpcResponse::Ok => return Ok(moved_total),
            IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                return Err(message);
            }
            other => return Err(format!("unexpected drain frame: {other:?}")),
        }
    }
    Err("drain stream ended without Ok".into())
}

/// Install a tracing subscriber driven by `RUST_LOG` (no-op when unset or
/// already installed). Tests call it first so `RUST_LOG=mesh_core=debug
/// cargo test … -- --nocapture` shows transport/WAN decisions.
pub fn init_tracing() {
    if std::env::var_os("RUST_LOG").is_none() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

pub async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    loop {
        if predicate() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
