//! A task that talks back to the mesh it is running in.
//!
//! A job's environment is cleared apart from a six-name allowlist, so a
//! task calling `meshnode` — or any client of the local socket — could not
//! find its own daemon, and when it did it had nothing to sign with but the
//! executing machine's own key, which no mesh has ever heard of.
//!
//! It worked perfectly on the founder's laptop, which is where anybody
//! building it would try it, and nowhere else. M23 had already written down
//! what that looks like from the outside: "an unenrolled person can sign
//! requests and have every one of them denied, and that state is
//! indistinguishable from the product being broken".
//!
//! What these tests pin is that a step needs *nothing set up*. It is told
//! where its daemon is, and it is given a credential that makes it the
//! person who submitted the job — for exactly as long as the job runs.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{IpcRequest, JobPriority, Liveness};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, join_new_node, run_to_end, spawn_founder, wait_until,
};

/// Submit `command` from `founder`, pinned to run on `target`, and return
/// stdout.
///
/// Submitted as the founder because that is the shape of the real case: a
/// person runs a workflow, and its steps land wherever the scheduler puts
/// them. The identity under test is the one the *task* is handed, which is
/// the executing node's, not the submitter's.
async fn echo_on(founder: &TestNode, target: &TestNode, command: &str) -> String {
    let req = IpcRequest::Run {
        command: command.to_string(),
        on: Some(format!("name={}", target.config().node_name)),
        env: vec![],
        timeout_secs: Some(30),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match run_to_end(founder, founder_user_key(founder), req).await {
        RunOutcome::Done { stdout, .. } => stdout,
        other => panic!("run: {other:?}"),
    }
}

/// A task is told where its own daemon is and which key to sign with — the
/// two things the SDK needs and could previously only guess at.
///
/// The socket is asserted against `socket_path()` exactly rather than
/// "somewhere under the data dir", because under `$XDG_RUNTIME_DIR` it
/// lives outside the home under a hash of it, and guessing is what this
/// test exists to rule out.
#[tokio::test]
async fn a_task_is_told_where_its_own_daemon_is() {
    let founder = spawn_founder("NIM", "sdk-identity").await;
    let worker = join_new_node("merope", &founder, vec![]).await;
    let worker_id = worker.node.node_id();
    assert!(
        wait_until(Duration::from_secs(15), || {
            founder
                .node
                .engine()
                .read(|s| s.is_active_member(&worker_id))
                && matches!(
                    founder.node.gossip().liveness_of(&worker_id),
                    Liveness::Alive
                )
        })
        .await,
        "merope never became schedulable from the founder"
    );

    let out = echo_on(
        &founder,
        &worker,
        "echo \"$MESHWARE_SOCKET|$MESHWARE_USER_IDENTITY\"",
    )
    .await;
    let (socket, identity) = out.trim().split_once('|').expect("both keys set");

    assert_eq!(
        socket,
        worker.config().socket_path().to_string_lossy(),
        "a task must be pointed at the daemon that spawned it"
    );
    // The point of that assertion: on a node whose home is not
    // `~/.meshware`, a client that guessed would have found the wrong
    // daemon or none at all. Every testkit node has a temp home, so this
    // holds only because the executor said where to look.
    assert!(
        !socket.starts_with(
            &dirs::home_dir()
                .unwrap()
                .join(".meshware")
                .to_string_lossy()
                .into_owned()
        ),
        "the test would prove nothing if the worker's home were the default one"
    );

    // The identity is the *job's*, not the machine's. That distinction is
    // the fix: the machine's key is a stranger to the mesh, and the job's
    // key is the person who submitted it.
    assert!(
        identity.starts_with(
            &worker
                .config()
                .data_dir
                .join("jobs")
                .to_string_lossy()
                .into_owned()
        ),
        "a job's credential lives in that job's own directory: {identity}"
    );
    assert_ne!(
        identity,
        worker
            .config()
            .data_dir
            .join("identity")
            .join("user.key")
            .to_string_lossy(),
        "a task must not be handed the machine's own key"
    );

    worker.stop().await;
    founder.stop().await;
}

/// Defaults, not overrides. A step that points `MESHWARE_USER_IDENTITY` at
/// somebody else's key is asking to act as them, and has always been
/// allowed to; the executor filling the key in must not take that away.
#[tokio::test]
async fn a_step_can_still_choose_its_own_identity() {
    let founder = spawn_founder("NIM", "sdk-identity-override").await;

    let req = IpcRequest::Run {
        command: "echo \"$MESHWARE_USER_IDENTITY\"".into(),
        on: None,
        env: vec![("MESHWARE_USER_IDENTITY".into(), "/keys/ci.key".into())],
        timeout_secs: Some(30),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let stdout = match run_to_end(&founder, founder_user_key(&founder), req).await {
        RunOutcome::Done { stdout, .. } => stdout,
        other => panic!("run: {other:?}"),
    };
    assert_eq!(stdout.trim(), "/keys/ci.key");

    founder.stop().await;
}
