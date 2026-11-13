//! M6 exit demo (MASTERPLAN §17): rolling-upgrade a 5-node mesh under
//! load with zero killed jobs. In-process, the "upgrade" of each node is a
//! full drain → stop → start cycle on the same data dir — exactly what a
//! binary swap does around the process boundary. Protocol compatibility
//! across versions is covered by the wire's negotiate() rules (§6); M6
//! itself changes no wire schema.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mesh_core::daemon::Node;
use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState, Liveness};
use mesh_testkit::{
    TestNode, drain_to_end, founder_user_key, join_new_node, spawn_founder, wait_until,
};
use tokio::sync::{Mutex, mpsc};

const NODES: usize = 5;

fn run_req(command: String) -> IpcRequest {
    IpcRequest::Run {
        command,
        on: None,
        env: vec![],
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

/// What one load job did, and how we know.
///
/// **The two are not the same thing, and conflating them is what made this
/// test lie.** A job is submitted *through* a node, and that node is the
/// one holding the response stream. Roll it, and the stream dies with it —
/// while the job itself, dispatched to some executor and drained to
/// completion before the box went down, succeeds. The claim this test
/// exists to defend is "zero killed jobs", not "zero lost terminal
/// sessions", so a lost stream is a reason to go and *look* rather than a
/// verdict.
///
/// The test used to count it as a killed job, which meant it failed on a
/// busy machine with nothing wrong: slower jobs widen the window between
/// "submitted through n2" and "n2 is now being rolled", and every
/// submission caught in it was scored as a casualty.
struct Outcome {
    /// From the `JobStarted` frame. `None` means the submission never got
    /// as far as a job, which is a real failure with nothing to look up.
    job_id: Option<mesh_proto::JobId>,
    /// What the submitting node managed to tell us before it went away.
    seen: Result<JobState, String>,
}

/// Submit one short job through `entry` and drive it to its terminal state.
async fn submit_and_finish(entry: &Node, user: &Keypair, i: usize) -> Outcome {
    let env = match user.sign_request(&run_req(format!("echo load-{i}"))) {
        Ok(env) => env,
        Err(e) => {
            return Outcome {
                job_id: None,
                seen: Err(e.to_string()),
            };
        }
    };
    let (tx, mut rx) = mpsc::channel::<IpcResponse>(64);
    let node = entry.clone();
    tokio::spawn(async move {
        node.handle_signed_stream(env, tx).await;
    });
    let mut job_id = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { job_id: id, .. } => job_id = Some(id),
            IpcResponse::JobDone { state, .. } => {
                return Outcome {
                    job_id,
                    seen: Ok(state),
                };
            }
            IpcResponse::Error { message } => {
                return Outcome {
                    job_id,
                    seen: Err(message),
                };
            }
            IpcResponse::Denied { message } => {
                return Outcome {
                    job_id,
                    seen: Err(format!("denied: {message}")),
                };
            }
            _ => {}
        }
    }
    Outcome {
        job_id,
        seen: Err("stream ended without JobDone".into()),
    }
}

/// The mesh's own record of what became of a job.
///
/// The op-log is the answer to "did it run", and it outlives whichever
/// node was holding the caller's stream — which is exactly the property
/// that makes a rolling upgrade survivable in the first place.
fn recorded_state(nodes: &[TestNode], job: &mesh_proto::JobId) -> Option<JobState> {
    nodes.iter().find_map(|n| {
        n.node
            .engine()
            .read(|s| s.jobs.get(job).map(|summary| summary.state))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rolling_upgrade_five_nodes_under_load_zero_killed_jobs() {
    // -- build the mesh -----------------------------------------------------
    let founder = spawn_founder("n0", "hyacinth").await;
    let mut nodes: Vec<TestNode> = vec![founder];
    for i in 1..NODES {
        let joined = join_new_node(&format!("n{i}"), &nodes[0], vec![]).await;
        nodes.push(joined);
    }
    let user = founder_user_key(&nodes[0]).clone();

    // Everyone sees everyone alive.
    assert!(
        wait_until(Duration::from_secs(30), || {
            nodes.iter().all(|n| {
                n.node.engine().read(|s| s.members.len() == NODES)
                    && nodes.iter().all(|other| {
                        matches!(
                            n.node.gossip().liveness_of(&other.node.node_id()),
                            Liveness::Alive
                        )
                    })
            })
        })
        .await,
        "mesh never fully converged before the upgrade"
    );

    // -- continuous load ----------------------------------------------------
    // Entry points the load generator may submit through; the node under
    // maintenance is removed from this list while it cycles (users don't
    // point their CLI at a box they are rebooting).
    let entries: Arc<Mutex<Vec<Node>>> =
        Arc::new(Mutex::new(nodes.iter().map(|n| n.node.clone()).collect()));
    let stop = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let load = {
        let entries = entries.clone();
        let stop = stop.clone();
        let completed = completed.clone();
        let user = user.clone();
        tokio::spawn(async move {
            let mut outcomes: Vec<Outcome> = Vec::new();
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let entry = {
                    let list = entries.lock().await;
                    list[i % list.len()].clone()
                };
                outcomes.push(submit_and_finish(&entry, &user, i).await);
                completed.fetch_add(1, Ordering::Relaxed);
                i += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            outcomes
        })
    };

    // -- roll every node ----------------------------------------------------
    for i in 0..NODES {
        let node_id = nodes[i].node.node_id();
        // Take it out of the entry rotation.
        {
            let mut list = entries.lock().await;
            list.retain(|n| n.node_id() != node_id);
        }

        // Drain: running work finishes, queued work moves, nothing is killed.
        drain_to_end(&nodes[i], &user, true)
            .await
            .unwrap_or_else(|e| panic!("drain n{i} failed: {e}"));

        // Stop + start on the same data dir = the upgrade's process cycle.
        let old = nodes.remove(i);
        let upgraded = old.restart().await;
        nodes.insert(i, upgraded);

        // Back in the mesh: peers see it alive with the draining tag gone.
        let upgraded_id = nodes[i].node.node_id();
        assert_eq!(upgraded_id, node_id, "restart changed the node identity");
        assert!(
            wait_until(Duration::from_secs(30), || {
                nodes.iter().enumerate().all(|(j, n)| {
                    j == i
                        || (matches!(n.node.gossip().liveness_of(&upgraded_id), Liveness::Alive)
                            && n.node.engine().read(|s| {
                                s.members.get(&upgraded_id).is_some_and(|m| {
                                    !m.info.tags.iter().any(|t| t == mesh_proto::DRAINING_TAG)
                                })
                            }))
                })
            })
            .await,
            "upgraded n{i} never rejoined cleanly"
        );
        {
            let mut list = entries.lock().await;
            list.push(nodes[i].node.clone());
        }
    }

    // -- verdict --------------------------------------------------------------
    // The roll can be very fast on an idle machine; keep the load running
    // until the sample is meaningful before stopping it.
    assert!(
        wait_until(Duration::from_secs(60), || {
            completed.load(Ordering::Relaxed) >= 30
        })
        .await,
        "load generator too slow ({} jobs)",
        completed.load(Ordering::Relaxed)
    );
    stop.store(true, Ordering::Relaxed);
    let outcomes = load.await.expect("load task");

    // The mesh is whole again.
    assert!(
        wait_until(Duration::from_secs(30), || {
            nodes.iter().all(|n| {
                nodes.iter().all(|other| {
                    matches!(
                        n.node.gossip().liveness_of(&other.node.node_id()),
                        Liveness::Alive
                    )
                })
            })
        })
        .await,
        "mesh did not fully re-converge after the rolling upgrade"
    );

    // -- zero killed jobs, asked of the jobs ----------------------------------
    //
    // Every submission whose stream survived answers for itself. For the
    // rest the mesh answers, once its own record has caught up — a job
    // whose coordinator was rolled mid-stream still has a terminal
    // `JobSummary`, replicated to every member, and that record is the
    // claim. A submission that never reached a job at all has nothing to
    // look up and counts against us, as it should.
    let lost: Vec<mesh_proto::JobId> = outcomes
        .iter()
        .filter(|o| o.seen.is_err())
        .filter_map(|o| o.job_id)
        .collect();
    // **Settled, not merely recorded.** A `JobSummary` appears the moment
    // the job is queued, so waiting for one to exist is waiting for
    // nothing — the first draft of this did exactly that and read a job
    // back as `Running`, which is neither a success to accept nor a
    // failure to report. Terminal is the only state that answers the
    // question, and a job still running here, on a re-converged mesh,
    // long after its executor came back, is a stranded job and belongs in
    // the count below rather than in a timeout nobody reads.
    let settled = wait_until(Duration::from_secs(30), || {
        lost.iter()
            .all(|id| recorded_state(&nodes, id).is_some_and(|state| state.is_terminal()))
    })
    .await;
    if !settled {
        eprintln!(
            "note: {} of {} lost-stream job(s) had not reached a terminal state",
            lost.iter()
                .filter(|id| !recorded_state(&nodes, id).is_some_and(|s| s.is_terminal()))
                .count(),
            lost.len()
        );
    }

    let mut killed = 0usize;
    for (i, outcome) in outcomes.iter().enumerate() {
        let state = match (&outcome.seen, outcome.job_id) {
            (Ok(state), _) => Some(*state),
            (Err(_), Some(id)) => recorded_state(&nodes, &id),
            (Err(_), None) => None,
        };
        if state != Some(JobState::Succeeded) {
            killed += 1;
            eprintln!("job {i}: seen {:?}, recorded {state:?}", outcome.seen);
        }
    }
    assert_eq!(
        killed,
        0,
        "{killed} of {} jobs did not succeed during the rolling upgrade",
        outcomes.len()
    );
}
