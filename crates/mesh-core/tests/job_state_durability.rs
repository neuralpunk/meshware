//! ENG-16 — what a node leaves behind when it stops badly.
//!
//! Three separate leaks, all of them invisible until a node crashes:
//!
//! - **Job state was written in place.** `spec.json` and `status.json` were
//!   truncated and then written, with no fsync. A power cut in that window
//!   left a job record that could not be read — and the orphan sweep skips
//!   what it cannot read, so the job stayed `running` mesh-wide for ever.
//! - **Nothing knew what to kill.** Every job runs in its own process
//!   group, but the pgid lived only in memory. A daemon that crashed left
//!   its jobs running with nothing that knew their pids, on a node that had
//!   already decided the slots were free.
//! - **A timed-out parallel item was abandoned, not killed.** The timeout
//!   dropped the future; the process tree under it carried on.
//!
//! The replicated half — settling jobs whose executor is revoked or has
//! left — is a property of the merge rules and is tested where those live,
//! in `state.rs`.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, JobId, JobPayload, JobPriority, JobSpec, JobState, SignedIpc};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder_with};

fn envelope(user: &Keypair, command: &str, nonce: u64) -> SignedIpc {
    let req = IpcRequest::Run {
        command: command.into(),
        on: None,
        env: vec![],
        timeout_secs: Some(600),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let payload = postcard::to_stdvec(&req).unwrap();
    let at_micros = mesh_proto::now_micros();
    let signature = user
        .sign(&mesh_proto::ipc_signing_bytes(
            &user.user_id(),
            at_micros,
            nonce,
            &payload,
        ))
        .to_vec();
    SignedIpc {
        user: user.user_id(),
        at_micros,
        nonce,
        payload,
        signature,
    }
}

fn accept(node: &TestNode, command: &str, nonce: u64) -> JobId {
    let user = founder_user_key(node);
    let job_id = JobId::generate();
    node.node
        .jobs()
        .accept(JobSpec {
            job_id,
            payload: JobPayload::Command(command.into()),
            env: vec![],
            timeout_secs: Some(600),
            priority: JobPriority::Normal,
            submitter: node.node.node_id(),
            submitter_user: user.user_id(),
            auth: envelope(user, command, nonce),
            fanout: None,
            created_at_micros: mesh_proto::now_micros(),
        })
        .expect("accept the job");
    job_id
}

fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => false,
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X"),
    }
}

/// The job record is never observed half-written: every write lands whole
/// or not at all, and nothing is left beside it for a reader to trip over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn job_state_is_written_whole_and_leaves_no_debris() {
    let node = spawn_founder_with("DUR", "eng16-durable", vec![], |c| c.limits.slots = 2).await;
    let id = accept(&node, "echo hello", 21);

    let done = mesh_testkit::wait_until(Duration::from_secs(30), || {
        node.node
            .jobs()
            .status_of(&id)
            .is_some_and(|s| s.state.is_terminal())
    })
    .await;
    assert!(done, "the job never finished");

    let dir = node.node.jobs().job_dir_of(&id);
    // Both records parse — the point of the temp-file-then-rename.
    let spec: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("spec.json")).expect("spec"))
            .expect("spec.json must parse");
    assert!(spec.get("job_id").is_some(), "{spec}");
    assert!(
        node.node.jobs().status_of(&id).is_some(),
        "status.json must parse"
    );

    // No `.status.json.<pid>.tmp` or similar left in the job directory: a
    // stray would be read as state by something, eventually.
    let stray: Vec<String> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(stray.is_empty(), "temp files left behind: {stray:?}");

    // …and the process-group note is gone, because the job was waited for.
    assert!(
        !dir.join("proc.json").exists(),
        "a finished job must not leave a pid for the next daemon to act on"
    );

    node.stop().await;
}

/// Everything a crashed daemon leaves behind for one job: the spool entry,
/// a status that still says `running`, the process group it was running in,
/// and a real process inside that group.
///
/// Staged by hand rather than by crashing a node, because an in-process
/// node cannot crash: its job task holds the op-log open, so a restart
/// would be waiting on a lock rather than exercising the sweep. What is
/// under test is the *next start*, and this is exactly the state it meets.
fn strand_a_running_job(node: &TestNode, starttime_offset: u64) -> (JobId, u32) {
    use std::os::unix::process::CommandExt as _;

    let user = founder_user_key(node);
    let job_id = JobId::generate();
    let spec = JobSpec {
        job_id,
        payload: JobPayload::Command("sleep 300".into()),
        env: vec![],
        timeout_secs: Some(600),
        priority: JobPriority::Normal,
        submitter: node.node.node_id(),
        submitter_user: user.user_id(),
        auth: envelope(user, "sleep 300", 31),
        fanout: None,
        created_at_micros: mesh_proto::now_micros(),
    };

    // Its own process group, as the executor spawns every job.
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("300");
    cmd.process_group(0);
    // Never waited on, on purpose: it is the leftover the node must find
    // and kill on restart, and that kill is what the test checks.
    #[allow(clippy::zombie_processes)]
    let child = cmd.spawn().expect("spawn the leftover");
    let pid = child.id();

    let dir = node
        .config()
        .data_dir
        .join("jobs")
        .join(job_id.to_display_string());
    std::fs::create_dir_all(dir.join("work")).expect("job dir");
    std::fs::write(
        dir.join("spec.json"),
        serde_json::to_vec(&spec).expect("encode spec"),
    )
    .expect("write spec");
    std::fs::write(
        dir.join("status.json"),
        serde_json::json!({
            "state": "Running",
            "exit_code": null,
            "started_at_micros": mesh_proto::now_micros(),
            "finished_at_micros": null,
            "charge": null,
        })
        .to_string(),
    )
    .expect("write status");
    std::fs::write(
        dir.join("proc.json"),
        serde_json::json!({
            "pid": pid,
            "starttime": starttime(pid).expect("starttime") + starttime_offset,
            "boot_id": std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .expect("boot id")
                .trim(),
        })
        .to_string(),
    )
    .expect("write proc");
    (job_id, pid)
}

/// `/proc/<pid>/stat` field 22, read the way the daemon reads it.
fn starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// A daemon that did not get to stop its jobs leaves them running. The next
/// start finds them through the recorded process group and kills them,
/// rather than leaving a machine with work on it that nothing is tracking.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_kills_what_the_previous_daemon_left_running() {
    let node = spawn_founder_with("LEFT", "eng16-leftovers", vec![], |c| c.limits.slots = 2).await;
    let (id, pid) = strand_a_running_job(&node, 0);
    assert!(alive(pid), "the leftover should be running");

    let node = node.restart().await;

    let killed = mesh_testkit::wait_until(Duration::from_secs(20), || !alive(pid)).await;
    assert!(
        killed,
        "the leftover process group {pid} survived the restart"
    );
    assert_eq!(
        node.node.jobs().status_of(&id).map(|s| s.state),
        Some(JobState::Failed),
        "and the job it belonged to is settled"
    );
    assert!(
        !node
            .config()
            .data_dir
            .join("jobs")
            .join(id.to_display_string())
            .join("proc.json")
            .exists(),
        "the record is consumed, so a later start cannot act on it twice"
    );

    node.stop().await;
}

/// A pid is not a process. A record whose pid has been reused since must be
/// discarded rather than acted on: killing a whole process group by a
/// recycled number would kill a stranger's work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_process_record_is_not_acted_on() {
    let node = spawn_founder_with("STALE", "eng16-stale", vec![], |c| c.limits.slots = 2).await;
    // Same pid, a start time that is not this process's — what a reused pid
    // looks like from here.
    let (id, pid) = strand_a_running_job(&node, 99_999);
    assert!(alive(pid));

    let node = node.restart().await;

    // The job is settled — that never depended on the pid — but the process
    // this node could not recognise is left alone.
    assert_eq!(
        node.node.jobs().status_of(&id).map(|s| s.state),
        Some(JobState::Failed)
    );
    assert!(
        alive(pid),
        "a record that does not match the running process must not be acted on"
    );

    node.stop().await;
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

/// A parallel item that runs past its deadline is killed, tree and all.
/// Before this, the timeout only stopped *waiting*: the item was recorded
/// as timed out and its processes carried on, so a long parallel job could
/// leave hundreds of them on a node that believed the work was over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timed_out_parallel_item_takes_its_children_with_it() {
    let node = spawn_founder_with("ITEM", "eng16-item", vec![], |c| c.limits.slots = 2).await;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("work")).expect("work dir");

    // Writes the pid of a grandchild that outlives its parent's own exit —
    // the shape that made this leak visible, and the shape a process-group
    // kill is needed for. Written to a file rather than stdout, because a
    // timed-out item reports no output at all.
    let outcome = node
        .node
        .jobs()
        .run_item(
            "sleep 300 & echo $! > pid.txt ; sleep 300",
            &[],
            Some(Duration::from_secs(2)),
            dir.path(),
            &mesh_core::sandbox::Grants::default(),
            &founder_user_key(&node).user_id(),
        )
        .await;

    assert!(
        outcome.stderr.contains("timed out"),
        "the item should have timed out: {outcome:?}"
    );
    let child: u32 = std::fs::read_to_string(dir.path().join("work").join("pid.txt"))
        .expect("the item should have written its child's pid")
        .trim()
        .parse()
        .expect("a pid");

    let gone = mesh_testkit::wait_until(Duration::from_secs(15), || !alive(child)).await;
    assert!(gone, "the item timed out but left {child} running under it");

    node.stop().await;
}
