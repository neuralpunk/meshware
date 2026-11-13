//! M6 day-2 operations (MASTERPLAN §12): drain (+ --move), healthz/metrics,
//! backup/restore, and job-history retention.

use std::time::Duration;

use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState};
use mesh_testkit::{
    RunOutcome, drain_to_end, founder_user_key, ipc_as, join_new_node, join_new_node_with,
    run_to_end, spawn_founder, spawn_founder_tagged, spawn_founder_with, stream_as, wait_until,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// Drain finishes running work, refuses new work, and reports idle.
#[tokio::test(flavor = "multi_thread")]
async fn drain_finishes_running_work_and_refuses_new() {
    let a = spawn_founder("a", "m6-drain").await;
    let b = join_new_node("b", &a, vec![]).await;
    let user = founder_user_key(&a).clone();

    assert!(
        wait_until(Duration::from_secs(15), || {
            a.node
                .engine()
                .read(|s| s.members.len() == 2)
                .then_some(())
                .is_some()
                && matches!(
                    a.node.gossip().liveness_of(&b.node.node_id()),
                    mesh_proto::Liveness::Alive
                )
        })
        .await
    );

    // A job runs on b while we drain it.
    let mut rx = stream_as(&b, &user, run_req("sleep 1 && echo survived", None)).await;
    let started = rx.recv().await.expect("first frame");
    assert!(
        matches!(started, IpcResponse::JobStarted { .. }),
        "expected JobStarted, got {started:?}"
    );

    let drainer = {
        let b_node = b.node.clone();
        let user = user.clone();
        tokio::spawn(async move {
            let env = user
                .sign_request(&IpcRequest::Drain {
                    move_pending: false,
                })
                .expect("sign");
            let (tx, mut rx) = tokio::sync::mpsc::channel(64);
            b_node.handle_signed_stream(env, tx).await;
            let mut done = false;
            while let Some(frame) = rx.recv().await {
                if matches!(frame, IpcResponse::Ok) {
                    done = true;
                }
            }
            done
        })
    };

    // While draining, b refuses new work: the running job must still finish.
    let mut got_output = false;
    let mut final_state = None;
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::LogChunk { data, .. } => {
                got_output |= String::from_utf8_lossy(&data).contains("survived");
            }
            IpcResponse::JobDone { state, .. } => {
                final_state = Some(state);
                break;
            }
            _ => {}
        }
    }
    assert!(got_output, "running job's output was lost during drain");
    assert_eq!(final_state, Some(JobState::Succeeded), "drain killed a job");

    assert!(
        drainer.await.expect("drain task"),
        "drain never reported Ok"
    );
    assert!(b.node.is_draining());

    // The mesh sees the draining tag; placement excludes b.
    assert!(
        wait_until(Duration::from_secs(10), || {
            a.node.engine().read(|s| {
                s.members
                    .get(&b.node.node_id())
                    .is_some_and(|m| m.info.tags.iter().any(|t| t == mesh_proto::DRAINING_TAG))
            })
        })
        .await,
        "draining tag never gossiped to a"
    );
    match run_to_end(&a, &user, run_req("echo nope", Some("name=b"))).await {
        RunOutcome::Error(msg) => assert!(
            msg.contains("no live node"),
            "expected no-candidate error, got: {msg}"
        ),
        other => panic!("draining node accepted work: {other:?}"),
    }
}

/// `drain --move` re-dispatches still-queued jobs to another candidate
/// inside their original selector, and nothing runs twice.
#[tokio::test(flavor = "multi_thread")]
async fn drain_move_relocates_queued_jobs() {
    // a: non-executing entry point. b: single-slot worker tagged `w`.
    // c joins later, so queued work can only have piled up on b.
    let a = spawn_founder_tagged("a", "m6-move", vec![mesh_proto::NO_EXECUTE_TAG.into()]).await;
    let b = join_new_node_with("b", &a, vec!["w".into()], |c| {
        c.limits.slots = 1;
        c.limits.queue = 8;
        c.limits.min_free_mem_mb = 0;
    })
    .await;
    let user = founder_user_key(&a).clone();
    assert!(
        wait_until(Duration::from_secs(15), || {
            matches!(
                a.node.gossip().liveness_of(&b.node.node_id()),
                mesh_proto::Liveness::Alive
            )
        })
        .await
    );

    // Fill b's slot, then queue two more behind it (only candidate for w).
    let mut streams = Vec::new();
    for i in 0..3 {
        let cmd = if i == 0 {
            "sleep 2 && echo first".to_string()
        } else {
            format!("echo queued-{i}")
        };
        let mut rx = stream_as(&a, &user, run_req(&cmd, Some("tag=w"))).await;
        let first = rx.recv().await.expect("frame");
        assert!(matches!(first, IpcResponse::JobStarted { .. }), "{first:?}");
        streams.push(rx);
    }
    assert!(
        wait_until(Duration::from_secs(5), || {
            b.node.jobs_for_test().queued_count() == 2
        })
        .await,
        "expected two jobs queued on b"
    );

    // A second worker appears; b drains and hands its queue over.
    let c = join_new_node_with("c", &a, vec!["w".into()], |cfg| {
        cfg.limits.min_free_mem_mb = 0;
    })
    .await;
    assert!(
        wait_until(Duration::from_secs(15), || {
            matches!(
                b.node.gossip().liveness_of(&c.node.node_id()),
                mesh_proto::Liveness::Alive
            )
        })
        .await
    );

    let moved = drain_to_end(&b, &user, true).await.expect("drain --move");
    assert_eq!(moved, 2, "both queued jobs should move to c");

    // All three jobs succeed in replicated state — the moved ones on c.
    assert!(
        wait_until(Duration::from_secs(30), || {
            a.node.engine().read(|s| {
                s.jobs
                    .values()
                    .filter(|j| j.state == JobState::Succeeded)
                    .count()
                    == 3
                    && s.jobs
                        .values()
                        .filter(|j| j.executor == c.node.node_id())
                        .count()
                        == 2
            })
        })
        .await,
        "moved jobs did not finish on c"
    );
    drop(streams);
}

/// /healthz and /metrics answer on the local endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn healthz_and_metrics_serve() {
    let a = spawn_founder_with("a", "m6-http", vec![], |c| {
        c.http_addr = Some("127.0.0.1:0".parse().expect("addr"));
    })
    .await;
    let addr = a.node.http_addr().expect("http endpoint bound");

    let healthz = http_get(addr, "/healthz").await;
    assert!(healthz.contains("200 OK"), "{healthz}");
    assert!(healthz.contains("\"status\":\"ok\""), "{healthz}");

    let metrics = http_get(addr, "/metrics").await;
    assert!(metrics.contains("200 OK"), "{metrics}");
    assert!(metrics.contains("meshware_uptime_seconds"), "{metrics}");
    assert!(metrics.contains("meshware_members_total 1"), "{metrics}");
    assert!(metrics.contains("meshware_build_info"), "{metrics}");

    let missing = http_get(addr, "/nope").await;
    assert!(missing.contains("404"), "{missing}");
}

/// **A node that has stopped working says so** (ENG-2).
///
/// The QUIC accept loop is one of the loops a node cannot work without.
/// Closing the endpoint ends it without the node stopping — the shape of a
/// loop that died on its own — and `/healthz`, which answered "ok" for as
/// long as the process was up, now answers 503 and names the loop.
#[tokio::test(flavor = "multi_thread")]
async fn healthz_fails_when_a_critical_loop_ends() {
    let a = spawn_founder_with("a", "m6-vitals", vec![], |c| {
        c.http_addr = Some("127.0.0.1:0".parse().expect("addr"));
    })
    .await;
    let addr = a.node.http_addr().expect("http endpoint bound");
    let watched = a.node.vitals().names();
    for name in ["transport accept", "wire dispatch", "gossip", "scheduler"] {
        assert!(
            watched.contains(&name),
            "{name} is not watched: {watched:?}"
        );
    }
    assert!(a.node.vitals().check().is_empty());
    let healthz = http_get(addr, "/healthz").await;
    assert!(healthz.contains("200 OK"), "{healthz}");
    assert!(healthz.contains("\"problems\":[]"), "{healthz}");

    a.node.transport().close();
    assert!(
        wait_until(Duration::from_secs(10), || {
            !a.node.vitals().check().is_empty()
        })
        .await,
        "an ended accept loop went unnoticed"
    );
    let healthz = http_get(addr, "/healthz").await;
    assert!(healthz.contains("503 Service Unavailable"), "{healthz}");
    assert!(healthz.contains("\"status\":\"unhealthy\""), "{healthz}");
    assert!(
        healthz.contains("the transport accept loop has ended"),
        "{healthz}"
    );
}

/// **A client that sends nothing is not served forever, and a crowd of
/// them does not lock out the scraper for good** (SEC-23).
#[tokio::test(flavor = "multi_thread")]
async fn the_metrics_listener_bounds_idle_clients() {
    let a = spawn_founder_with("a", "m6-http-idle", vec![], |c| {
        c.http_addr = Some("127.0.0.1:0".parse().expect("addr"));
    })
    .await;
    let addr = a.node.http_addr().expect("http endpoint bound");

    // Forty sockets that never say a word: more than the listener serves.
    let mut idle = Vec::new();
    for _ in 0..40 {
        idle.push(tokio::net::TcpStream::connect(addr).await.expect("connect"));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The ones past the cap are closed straight away…
    let mut closed = 0;
    for s in &mut idle {
        let mut b = [0u8; 1];
        if matches!(
            tokio::time::timeout(Duration::from_millis(50), s.read(&mut b)).await,
            Ok(Ok(0)) | Ok(Err(_))
        ) {
            closed += 1;
        }
    }
    assert!(
        closed >= 8,
        "only {closed} of 40 idle connections were refused"
    );

    // …and the rest after the read deadline, after which a scrape works.
    let mut first = idle.remove(0);
    let mut b = [0u8; 1];
    let ended = tokio::time::timeout(Duration::from_secs(10), first.read(&mut b)).await;
    assert!(
        matches!(ended, Ok(Ok(0)) | Ok(Err(_))),
        "an idle connection was held past the deadline: {ended:?}"
    );
    drop(idle);
    let metrics = http_get(addr, "/metrics").await;
    assert!(metrics.contains("200 OK"), "{metrics}");
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    String::from_utf8_lossy(&buf).into_owned()
}

/// Backup captures identity + genesis + ops; restore rebuilds a working
/// node that still knows everything.
#[tokio::test(flavor = "multi_thread")]
async fn backup_restore_roundtrip() {
    let a = spawn_founder("a", "m6-backup").await;
    let user = founder_user_key(&a).clone();

    // Put real state in: a second user with a grant.
    let other = mesh_core::identity::Keypair::generate().expect("keypair");
    let mut record = mesh_proto::UserRecord {
        user_id: other.user_id(),
        username: "backup-proof".into(),
        created_at_micros: mesh_proto::now_micros(),
        added_by: user.user_id(),
        signature: Vec::new(),
    };
    record.signature = user.sign(&record.signing_bytes()).to_vec();
    assert!(matches!(
        ipc_as(&a, &user, IpcRequest::UserAdd { record }).await,
        IpcResponse::Ok
    ));

    let out = tempfile::tempdir().expect("tempdir");
    let backup_path = out.path().join("node-a.mwb");
    let resp = ipc_as(
        &a,
        &user,
        IpcRequest::Backup {
            path: backup_path.display().to_string(),
        },
    )
    .await;
    let IpcResponse::BackupDone { ops, .. } = resp else {
        panic!("backup failed: {resp:?}");
    };
    assert!(ops > 0);
    let node_id = a.node.node_id();
    a.stop().await;
    drop(a);

    // Restore into a fresh dir and boot it.
    let restored_dir = tempfile::tempdir().expect("tempdir");
    let report = mesh_core::backup::restore(&backup_path, restored_dir.path()).expect("restore");
    assert_eq!(report.mesh_name, "m6-backup");
    assert!(report.ops >= ops);

    // Restoring over an existing mesh is refused.
    assert!(mesh_core::backup::restore(&backup_path, restored_dir.path()).is_err());

    let config = mesh_core::config::NodeConfig::load(
        None,
        &mesh_core::config::Overrides {
            data_dir: Some(restored_dir.path().to_path_buf()),
            listen: Some("127.0.0.1:0".into()),
            node_name: Some("a".into()),
            ..Default::default()
        },
    )
    .expect("config");
    let node = mesh_core::daemon::Node::start(config)
        .await
        .expect("boot restored node");
    assert_eq!(node.node_id(), node_id, "restored node lost its identity");
    let has_user = node.engine().read(|s| {
        s.users
            .values()
            .any(|u| u.record.username == "backup-proof")
    });
    assert!(has_user, "restored state lost the user record");
    node.stop().await;
}

/// Retention prunes terminal job history by size cap, never active jobs.
#[tokio::test(flavor = "multi_thread")]
async fn retention_prunes_terminal_jobs() {
    let a = spawn_founder("a", "m6-retention").await;
    let user = founder_user_key(&a).clone();

    for i in 0..2 {
        match run_to_end(&a, &user, run_req(&format!("echo r{i}"), None)).await {
            RunOutcome::Done { state, .. } => assert_eq!(state, JobState::Succeeded),
            other => panic!("run failed: {other:?}"),
        }
    }
    let jobs = a.node.jobs_for_test();
    // Size cap of one byte: every terminal job must go.
    let pruned = jobs.sweep_retention(mesh_core::config::Retention {
        job_days: 0,
        job_max_bytes: 1,
        ..Default::default()
    });
    assert_eq!(pruned, 2, "expected both finished jobs pruned");
}
