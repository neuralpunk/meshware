//! Scale certification (MASTERPLAN D12, §4.3, M6): real in-process nodes —
//! real QUIC, real gossip, real stores — at 100 nodes in every CI run and
//! at 1,000 nodes as the release-gating certification (`--ignored`; run
//! with `cargo test --release --test scale -- --ignored`).
//!
//! Convergence = every node's materialized state knows every member. The
//! measurement starts once the last join returns and ends when the slowest
//! node catches up.
//!
//! **Built like the product** (ENG-25). This is the harness the scale and
//! dispatch claims are published from, so it has to be linked the way the
//! shipped binary is — and that binary sets `mimalloc` as its global
//! allocator, for reasons measured with this very test. Without the line
//! below, the numbers here would be the default allocator's and the
//! numbers in the release would be mimalloc's, which is the shape of
//! problem the whole item is about: a proof point describing a build
//! nobody runs.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::time::{Duration, Instant};

use mesh_proto::{IpcRequest, JobPriority, JobState};
use mesh_testkit::{TestNode, founder_user_key, join_new_node, run_to_end, spawn_founder};

#[cfg(unix)]
fn raise_nofile() {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let mut lim = getrlimit(Resource::Nofile);
    lim.current = lim.maximum;
    let _ = setrlimit(Resource::Nofile, lim);
}
#[cfg(not(unix))]
fn raise_nofile() {}

fn run_req(command: &str, on: Option<&str>) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: on.map(String::from),
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

/// Build an n-node mesh (founder + n-1 concurrent joins), then measure how
/// long the slowest node takes to materialize the full member set.
async fn certify(n: usize, join_fanout: usize, target: Duration) {
    raise_nofile();
    let founder = spawn_founder("s0", "scale").await;

    let names: Vec<String> = (1..n).map(|i| format!("s{i}")).collect();
    let t_join = Instant::now();
    let mut nodes: Vec<TestNode> = {
        use futures::StreamExt;
        futures::stream::iter(
            names
                .iter()
                .map(|name| join_new_node(name, &founder, vec![])),
        )
        .buffer_unordered(join_fanout)
        .collect()
        .await
    };
    let join_time = t_join.elapsed();
    nodes.push(founder);
    eprintln!("[scale {n}] all nodes joined+started in {join_time:?}");

    // Convergence: every node's state holds all n member records.
    let t0 = Instant::now();
    let mut last_report = Instant::now();
    let converged = loop {
        let behind = nodes
            .iter()
            .filter(|node| node.node.engine().read(|s| s.members.len() < n))
            .count();
        if behind == 0 {
            break true;
        }
        if t0.elapsed() > target {
            eprintln!("[scale {n}] TIMEOUT: {behind} node(s) still behind");
            break false;
        }
        if last_report.elapsed() > Duration::from_secs(5) {
            let detail: Vec<String> = nodes
                .iter()
                .filter(|node| node.node.engine().read(|s| s.members.len() < n))
                .take(8)
                .map(|node| {
                    format!(
                        "{}={}",
                        node.config().node_name,
                        node.node.engine().read(|s| s.members.len())
                    )
                })
                .collect();
            eprintln!(
                "[scale {n}] {:?}: {behind} node(s) still catching up [{}]",
                t0.elapsed(),
                detail.join(" ")
            );
            last_report = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let convergence = t0.elapsed();
    eprintln!(
        "[scale {n}] full membership convergence in {convergence:?} after the last join \
         (target {target:?})"
    );
    assert!(
        converged,
        "{n}-node mesh did not converge within {target:?}"
    );

    // Full state convergence: every node holds the identical op set (same
    // version vector = same materialized state, byte for byte — the merge
    // is deterministic, MASTERPLAN §4.4). Membership converged above; the
    // final boot announcements may still be in flight, so this is a second
    // bounded measurement, not an instant assertion.
    let t1 = Instant::now();
    let state_converged = loop {
        let reference = nodes[0].node.engine().version_vector();
        let behind = nodes
            .iter()
            .filter(|node| node.node.engine().version_vector() != reference)
            .count();
        if behind == 0 {
            break true;
        }
        if t1.elapsed() > target {
            eprintln!("[scale {n}] STATE TIMEOUT: {behind} node(s) differ");
            break false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    eprintln!(
        "[scale {n}] full op-log convergence in {:?} more (target {target:?})",
        t1.elapsed()
    );
    assert!(
        state_converged,
        "{n}-node mesh never reached identical op sets"
    );

    // The mesh still does its job: run a command and time the hot path.
    let user = founder_user_key(nodes.last().expect("founder")).clone();
    let founder_node = nodes.last().expect("founder");
    let t_run = Instant::now();
    match run_to_end(
        founder_node,
        &user,
        run_req("echo scale-ok", Some("name=s0")),
    )
    .await
    {
        mesh_testkit::RunOutcome::Done { state, stdout, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("scale-ok"));
        }
        other => panic!("job on the {n}-node mesh failed: {other:?}"),
    }
    eprintln!(
        "[scale {n}] submit→complete round trip on the loaded mesh: {:?}",
        t_run.elapsed()
    );

    // Tear down concurrently — 1,000 sequential stops would dominate runtime.
    use futures::StreamExt;
    futures::stream::iter(nodes.iter())
        .for_each_concurrent(64, |node| async move {
            node.stop().await;
        })
        .await;
}

/// Always-on CI scale gate (MASTERPLAN §4.3: sims at 100 from M1).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn hundred_node_convergence() {
    certify(100, 16, Duration::from_secs(60)).await;
}

/// The 1,000-node certification (MASTERPLAN D12, M6 release gate).
/// Heavy: run explicitly, in release mode, on a machine with headroom:
/// `cargo test --release -p mesh-core --test scale -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
#[ignore = "1,000-node certification — run explicitly in release mode"]
async fn thousand_node_certification() {
    certify(1000, 32, Duration::from_secs(120)).await;
}

/// First-byte latency stays inside the §11 envelope on a small live mesh:
/// job spawn overhead and streaming must not regress as features pile on.
#[tokio::test(flavor = "multi_thread")]
async fn submit_latency_envelope() {
    let a = spawn_founder("lat", "latency").await;
    let user = founder_user_key(&a).clone();
    // Warm up (identity caches, file creation paths).
    let _ = run_to_end(&a, &user, run_req("true", None)).await;

    let t = Instant::now();
    let outcome = run_to_end(&a, &user, run_req("echo b", None)).await;
    let elapsed = t.elapsed();
    match outcome {
        mesh_testkit::RunOutcome::Done { state, .. } => assert_eq!(state, JobState::Succeeded),
        other => panic!("{other:?}"),
    }
    eprintln!("[latency] warm submit→complete: {elapsed:?}");
    // §11 targets 25 ms spawn overhead + 100 ms first byte on release
    // builds; debug CI gets a generous ceiling that still catches an
    // order-of-magnitude regression.
    assert!(
        elapsed < Duration::from_millis(1500),
        "warm submit→complete took {elapsed:?}"
    );
    a.stop().await;
}
