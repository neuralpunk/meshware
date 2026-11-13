//! ENG-8 — a job ends when its main process does; and ENG-26, a follower
//! that has been abandoned lets go of its file handles.
//!
//! The executor waited for the job's stdout and stderr to close, and so did
//! every process that inherited them. `nohup ./svc &` in a script — common
//! in admin automation — left the job running for ever: it held a slot, its
//! fan-out never completed, and a cancel did nothing.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use mesh_proto::{IpcRequest, JobPriority, JobState};
use mesh_testkit::{RunOutcome, founder_user_key, run_to_end, spawn_founder};

fn run(command: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(120),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

/// Is `pid` gone, or a zombie waiting to be reaped?
fn is_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // `pid (comm) S …`: the state is the first field after the name.
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
    }
}

fn pid_in(stdout: &str) -> u32 {
    stdout
        .split_whitespace()
        .find_map(|w| w.strip_prefix("bg=").and_then(|p| p.parse().ok()))
        .unwrap_or_else(|| panic!("no pid in {stdout:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_process_does_not_hold_the_job_open() {
    let node = spawn_founder("NIM", "eng8-bg").await;
    let admin = founder_user_key(&node).clone();

    let began = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_to_end(&node, &admin, run("sleep 300 & echo bg=$! && echo done")),
    )
    .await
    .expect("the job never ended while its background child ran");
    let RunOutcome::Done {
        stdout,
        state,
        job_id,
        ..
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded, "{stdout}");
    assert!(stdout.contains("done"), "{stdout}");
    // …and its log says why it ended the way it did.
    let stderr = std::fs::read_to_string(
        node.node
            .jobs()
            .job_dir_of(&job_id.unwrap())
            .join("stderr.log"),
    )
    .unwrap_or_default();
    assert!(
        stderr.contains("processes it started still held its output"),
        "{stderr}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "took {:?}",
        began.elapsed()
    );
    // The child it left behind was stopped with it.
    let pid = pid_in(&stdout);
    let gone = mesh_testkit::wait_until(Duration::from_secs(10), || is_gone(pid)).await;
    assert!(gone, "the background child {pid} is still running");

    // The slot is free again: the next job starts at once.
    let RunOutcome::Done { state, .. } = run_to_end(&node, &admin, run("true")).await else {
        panic!("the follow-up job did not run");
    };
    assert_eq!(state, JobState::Succeeded);
    node.stop().await;
}

/// A child that left the job's process group cannot be reached by the group
/// signal, and on a node without cgroups nothing else reaches it either. The
/// job still ends — its output is simply not collected past that point.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_that_leaves_the_group_does_not_hold_the_job_open() {
    let node = spawn_founder("NIM", "eng8-setsid").await;
    let admin = founder_user_key(&node).clone();

    let began = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_to_end(
            &node,
            &admin,
            run("setsid sleep 300 & echo bg=$! && echo done"),
        ),
    )
    .await
    .expect("the job never ended while its detached child ran");
    let RunOutcome::Done { stdout, state, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded, "{stdout}");
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "took {:?}",
        began.elapsed()
    );
    // Whatever cgroups could or could not do here, leave nothing behind.
    let pid = pid_in(&stdout);
    if !is_gone(pid) {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }
    node.stop().await;
}

/// How many of this process's descriptors point at `path`.
///
/// `/proc/self/fd` is the only place that answers "is this file still
/// open", and it is what makes the leak below observable at all — the task
/// that leaks is a detached `tokio::spawn` with no handle anybody holds.
fn open_handles_to(path: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| std::fs::read_link(e.path()).is_ok_and(|target| target == path))
        .count()
}

/// ENG-26 — a `meshnode logs -f` somebody walked away from stops costing
/// something.
///
/// The follower noticed a hangup only when a `send` failed, and a quiet
/// job sends nothing. Someone who pressed ^C while watching a service that
/// logs once an hour left the task, its channel and **two open file
/// handles** behind until the job ended — which for a service is never. A
/// long-lived node with people watching services leaks a follower per
/// abandoned window and runs out of descriptors long before anything else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_abandoned_follower_releases_its_file_handles() {
    let node = spawn_founder("follow", "eng26-follow").await;
    let admin = founder_user_key(&node).clone();

    // A job that runs for a while and writes nothing after the first line:
    // the quiet case, which is the one that leaked.
    let mut frames = mesh_testkit::stream_as(&node, &admin, run("echo started; sleep 120")).await;
    let mut job = None;
    while let Some(frame) = frames.recv().await {
        if let mesh_proto::IpcResponse::JobStarted { job_id, .. } = frame {
            job = Some(job_id);
            break;
        }
    }
    let job = job.expect("the job never started");
    let stdout = node.node.jobs().job_dir_of(&job).join("stdout.log");

    // A follower of its own, which is what `meshnode logs -f` opens.
    let rx = node.node.jobs().follow(job, 0, 0, true);
    let opened =
        mesh_testkit::wait_until(Duration::from_secs(20), || open_handles_to(&stdout) > 0).await;
    assert!(opened, "the follower never opened the log");
    let held = open_handles_to(&stdout);

    // The reader walks away. Nothing else changes: the job runs on, and it
    // writes nothing more — so no `send` will ever fail and notice.
    drop(rx);
    drop(frames);

    let released =
        mesh_testkit::wait_until(Duration::from_secs(30), || open_handles_to(&stdout) < held).await;
    assert!(
        released,
        "the follower still holds {held} handle(s) on the job's log after its reader left"
    );

    let _ = mesh_testkit::ipc_as(&node, &admin, IpcRequest::JobCancel { job_id: job }).await;
    node.stop().await;
}
