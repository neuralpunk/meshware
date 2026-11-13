//! M7 relay (MASTERPLAN M7): a public member with `[wan] relay = "on"`
//! auto-manages the reserved `relay` tag; symmetric-NATed members register
//! with it and announce `relay:<id>` candidates; everything a mesh does —
//! jobs with streamed logs, cancel, bundle fetch, `drain --move` — works
//! between two symmetric-NATed nodes through the relay; and a dead relay
//! fails over to the other one.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::transport::{ConnKind, DialRoute};
use mesh_core::wan::Reachability;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState, Liveness, RELAY_TAG, Runtime};
use mesh_testkit::nat::{NatProfile, TestNat};
use mesh_testkit::{
    RunOutcome, TestNode, drain_to_end, founder_user_key, ipc_as, join_new_node_natted,
    join_new_node_natted_with, join_new_node_with, run_to_end, spawn_founder_with, stream_as,
    wait_until,
};

/// The relay carrying `viewer`'s pooled connection to `peer`, if relayed.
fn relay_in_use(viewer: &TestNode, peer: &mesh_proto::NodeId) -> Option<mesh_proto::NodeId> {
    let t = viewer.node.transport();
    let pooled = t.pooled_conn(peer)?;
    if pooled.kind != ConnKind::Relayed {
        return None;
    }
    t.meshsock()
        .route_by_addr(pooled.conn.remote_address())
        .map(|(relay, _)| relay)
        .or(match t.last_good_route(peer) {
            Some(DialRoute::Relay(r)) => Some(r),
            _ => None,
        })
}

fn run_req(command: &str, on: &str) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: Some(on.into()),
        env: vec![],
        timeout_secs: Some(120),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

/// Founder that relays: on loopback it verifies itself public as soon as a
/// joiner dials it.
async fn spawn_relay(name: &str) -> TestNode {
    mesh_testkit::init_tracing();
    spawn_founder_with(name, "hyacinth", vec![], |c| {
        c.wan.relay = true;
    })
    .await
}

async fn wait_relay_tag(node: &TestNode, relay: &TestNode) {
    let id = relay.node.node_id();
    let engine = node.node.engine().clone();
    let ok = wait_until(Duration::from_secs(30), || {
        engine.read(|s| {
            s.members
                .get(&id)
                .is_some_and(|m| m.info.tags.iter().any(|t| t == RELAY_TAG))
        })
    })
    .await;
    assert!(
        ok,
        "{} should see {} relay-tagged",
        node.config().node_name,
        relay.config().node_name
    );
}

async fn wait_registered(natted: &TestNode, relay: &TestNode) {
    let id = relay.node.node_id();
    let wan = natted.node.wan().clone();
    let engine = natted.node.engine().clone();
    let me = natted.node.node_id();
    let ok = wait_until(Duration::from_secs(40), || {
        wan.relays().contains(&id)
            && engine.read(|s| {
                s.members
                    .get(&me)
                    .is_some_and(|m| m.info.addrs.contains(&mesh_proto::relay_candidate(&id)))
            })
    })
    .await;
    assert!(
        ok,
        "{} should register with relay {} (reachability {:?}, relays {:?})",
        natted.config().node_name,
        relay.config().node_name,
        wan.reachability(),
        wan.relays()
    );
}

/// `viewer` sees `target` alive *and* knows a `relay:` candidate for it
/// (relay candidates travel by gossip; until they land, a dial can only
/// try black holes).
async fn wait_alive(viewer: &TestNode, target: &TestNode) {
    let id = target.node.node_id();
    let gossip = viewer.node.gossip().clone();
    let engine = viewer.node.engine().clone();
    let ok = wait_until(Duration::from_secs(40), || {
        engine.read(|s| {
            s.is_active_member(&id)
                && s.members
                    .get(&id)
                    .is_some_and(|m| m.info.addrs.iter().any(|a| a.starts_with("relay:")))
        }) && gossip.liveness_of(&id) == Liveness::Alive
    })
    .await;
    assert!(
        ok,
        "{} should see {} alive",
        viewer.config().node_name,
        target.config().node_name
    );
}

#[tokio::test]
async fn relay_tag_auto_managed_and_natted_members_register() {
    let relay = spawn_relay("relay").await;
    let nat_a = TestNat::new(NatProfile::Symmetric);
    let nat_b = TestNat::new(NatProfile::Symmetric);
    let a = join_new_node_natted("a", &relay, &nat_a).await;
    let b = join_new_node_natted("b", &relay, &nat_b).await;

    // The relay verifies itself public through its joiners' observations
    // and announces the tag; the NATed nodes see it and register.
    wait_relay_tag(&a, &relay).await;
    wait_relay_tag(&b, &relay).await;
    wait_registered(&a, &relay).await;
    wait_registered(&b, &relay).await;
    let ok = wait_until(Duration::from_secs(10), || {
        relay.node.transport().relay().snapshot().clients == 2
    })
    .await;
    assert!(ok, "relay hub should hold two registrations");
    assert_eq!(relay.node.wan().reachability(), Reachability::Public);
    assert!(
        relay.node.wan().relays().is_empty(),
        "a relay needs no relay"
    );

    relay.node.shutdown();
    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn everything_works_between_symmetric_nats_via_relay() {
    let relay = spawn_relay("relay").await;
    let nat_a = TestNat::new(NatProfile::Symmetric);
    let nat_b = TestNat::new(NatProfile::Symmetric);
    // One executor slot each so the drain --move part can queue work.
    let a = join_new_node_natted_with("a", &relay, &nat_a, |c| {
        c.limits.slots = 1;
        c.limits.queue = 8;
    })
    .await;
    let b = join_new_node_natted_with("b", &relay, &nat_b, |c| {
        c.limits.slots = 1;
        c.limits.queue = 8;
    })
    .await;
    let a_id = a.node.node_id();
    let b_id = b.node.node_id();
    let key = founder_user_key(&relay);

    wait_registered(&a, &relay).await;
    wait_registered(&b, &relay).await;
    wait_alive(&a, &b).await;

    // -- job + streamed logs, A → B: direct fails (black holes), punch
    // fails (both symmetric), the relay carries it.
    match run_to_end(
        &a,
        key,
        run_req("echo relayed-hello; echo err >&2", "name=b"),
    )
    .await
    {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert!(stdout.contains("relayed-hello"), "stdout: {stdout}");
        }
        other => panic!("relayed run failed: {other:?}"),
    }
    assert_eq!(a.node.transport().conn_kind(&b_id), Some(ConnKind::Relayed));
    assert!(a.node.transport().meshsock().relayed_sent() > 0);
    assert!(b.node.transport().meshsock().relayed_recv() > 0);
    assert!(relay.node.transport().relay().snapshot().frames_relayed > 0);
    assert_eq!(
        a.node.transport().meshsock().dropped(),
        0,
        "no drops on an idle relay path"
    );

    // -- cancel a running job on B from A.
    let mut rx = stream_as(&a, key, run_req("sleep 30", "name=b")).await;
    let mut job_id = None;
    while let Some(frame) = rx.recv().await {
        if let IpcResponse::JobStarted { job_id: id, .. } = frame {
            job_id = Some(id);
            break;
        }
    }
    let job_id = job_id.expect("job started on b");
    let engine_a = a.node.engine().clone();
    assert!(
        wait_until(Duration::from_secs(15), || {
            engine_a.read(|s| {
                s.jobs
                    .get(&job_id)
                    .is_some_and(|j| j.state == JobState::Running)
            })
        })
        .await
    );
    let resp = ipc_as(&a, key, IpcRequest::JobCancel { job_id }).await;
    assert!(
        matches!(resp, IpcResponse::Ok),
        "cancel via relay: {resp:?}"
    );
    assert!(
        wait_until(Duration::from_secs(20), || {
            engine_a.read(|s| s.jobs.get(&job_id).is_some_and(|j| j.state.is_terminal()))
        })
        .await,
        "cancelled job reaches a terminal state"
    );

    // -- content fetch: a bundle pushed on A runs on B (B pulls it from A
    // over the relayed path — A's connection to B is pooled on B too).
    let bundle = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(bundle.path().join("scripts")).unwrap();
    let script = bundle.path().join("scripts/hello.sh");
    std::fs::write(&script, "#!/bin/bash\necho hello-from-bundle\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(a.node.engine(), &content, bundle.path()).unwrap();
    mesh_core::services::create_service_as(
        a.node.engine(),
        key,
        "greet".into(),
        Runtime::Bash,
        push.bundle_id,
        "scripts".into(),
        "hello.sh".into(),
        None,
        vec![],
        String::new(),
        vec![],
    )
    .unwrap();
    let engine_b = b.node.engine().clone();
    assert!(
        wait_until(Duration::from_secs(20), || {
            engine_b.read(|s| s.service_current("greet").is_some())
        })
        .await,
        "service replicates to b"
    );
    let svc = IpcRequest::RunService {
        name: "greet".into(),
        on: Some("name=b".into()),
        inputs: vec![],
        inventory: None,
        timeout_secs: Some(120),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    match run_to_end(&a, key, svc).await {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded, "stdout: {stdout}");
            assert!(stdout.contains("hello-from-bundle"), "stdout: {stdout}");
        }
        other => panic!("service run via relay failed: {other:?}"),
    }

    // -- drain --move toward the NATed node: a job queued on A (its single
    // slot busy) whose selector `tag=exec` only matched A at submit time
    // moves to B once B gains the tag and A drains.
    let resp = ipc_as(
        &a,
        key,
        IpcRequest::TagsUpdate {
            add: vec!["exec".into()],
            remove: vec![],
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok));
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine_a.read(|s| {
                s.members
                    .get(&a_id)
                    .is_some_and(|m| m.info.tags.iter().any(|t| t == "exec"))
            })
        })
        .await
    );
    let mut busy = stream_as(&a, key, run_req("sleep 6", "tag=exec")).await;
    let mut busy_id = None;
    while let Some(frame) = busy.recv().await {
        if let IpcResponse::JobStarted {
            job_id, executor, ..
        } = frame
        {
            assert_eq!(executor, a_id, "only a carries the tag yet");
            busy_id = Some(job_id);
            break;
        }
    }
    let busy_id = busy_id.expect("busy job started");
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine_a.read(|s| {
                s.jobs
                    .get(&busy_id)
                    .is_some_and(|j| j.state == JobState::Running)
            })
        })
        .await
    );
    let mut queued = stream_as(&a, key, run_req("echo moved-over-relay", "tag=exec")).await;
    let mut queued_id = None;
    while let Some(frame) = queued.recv().await {
        if let IpcResponse::JobStarted {
            job_id, executor, ..
        } = frame
        {
            assert_eq!(executor, a_id);
            queued_id = Some(job_id);
            break;
        }
    }
    let queued_id = queued_id.expect("second job accepted (queued)");
    assert!(
        wait_until(Duration::from_secs(10), || {
            engine_a.read(|s| {
                s.jobs
                    .get(&queued_id)
                    .is_some_and(|j| j.state == JobState::Queued)
            })
        })
        .await,
        "second job should be queued behind the busy slot"
    );
    // Now B becomes a candidate too.
    let resp = ipc_as(
        &b,
        key,
        IpcRequest::TagsUpdate {
            add: vec!["exec".into()],
            remove: vec![],
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok));
    assert!(
        wait_until(Duration::from_secs(20), || {
            engine_a.read(|s| {
                s.members
                    .get(&b_id)
                    .is_some_and(|m| m.info.tags.iter().any(|t| t == "exec"))
            })
        })
        .await,
        "a sees b's new tag"
    );
    let moved = drain_to_end(&a, key, true).await.expect("drain --move");
    assert_eq!(moved, 1, "the queued job moves to b via the relay");
    let executor = engine_a.read(|s| s.jobs.get(&queued_id).map(|j| j.executor));
    // A learns the moved job's summary from B (gossip over the relay).
    let ok = wait_until(Duration::from_secs(30), || {
        engine_a.read(|s| {
            s.jobs
                .get(&queued_id)
                .is_some_and(|j| j.executor == b_id && j.state == JobState::Succeeded)
        })
    })
    .await;
    assert!(ok, "moved job runs on b (executor {executor:?})");

    relay.node.shutdown();
    a.node.shutdown();
    b.node.shutdown();
}

#[tokio::test]
async fn relay_failover() {
    let r1 = spawn_relay("relay1").await;
    // A second public relay, joined normally.
    let r2 = join_new_node_with("relay2", &r1, vec![], |c| {
        c.wan.relay = true;
    })
    .await;
    let nat_a = TestNat::new(NatProfile::Symmetric);
    let nat_b = TestNat::new(NatProfile::Symmetric);
    let a = join_new_node_natted("a", &r1, &nat_a).await;
    let b = join_new_node_natted("b", &r1, &nat_b).await;
    let key = founder_user_key(&r1);
    let b_id = b.node.node_id();
    let r1_id = r1.node.node_id();
    let r2_id = r2.node.node_id();

    wait_relay_tag(&a, &r1).await;
    wait_relay_tag(&a, &r2).await;
    // Both NATed nodes register with both relays (k = 2).
    let a_wan = a.node.wan().clone();
    let b_wan = b.node.wan().clone();
    assert!(
        wait_until(Duration::from_secs(40), || {
            a_wan.relays().len() == 2 && b_wan.relays().len() == 2
        })
        .await,
        "both register with both relays (a: {:?}, b: {:?})",
        a_wan.relays(),
        b_wan.relays()
    );
    wait_alive(&a, &b).await;

    match run_to_end(&a, key, run_req("echo before", "name=b")).await {
        RunOutcome::Done { state, .. } => assert_eq!(state, JobState::Succeeded),
        other => panic!("before failover: {other:?}"),
    }
    // Which relay carries a ↔ b right now? The pooled connection may be
    // one a dialed (last-good route) or one b dialed first (inbound); its
    // synthetic address names the relay either way.
    let used = relay_in_use(&a, &b_id).expect("a ↔ b rides on a relay");
    assert!(used == r1_id || used == r2_id);

    // Kill the relay in use.
    if used == r1_id {
        r1.stop().await;
    } else {
        r2.stop().await;
    }
    // The relayed path through it dies; the next dial fails over.
    let transport_a = a.node.transport().clone();
    let closed = wait_until(Duration::from_secs(30), || {
        transport_a.pooled_conn(&b_id).is_none()
    })
    .await;
    assert!(
        closed,
        "the relayed connection through the dead relay closes"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        if let RunOutcome::Done { state, .. } =
            run_to_end(&a, key, run_req("echo after", "name=b")).await
            && state == JobState::Succeeded
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(ok, "work flows through the surviving relay");
    let now = relay_in_use(&a, &b_id);
    assert!(
        now.is_some_and(|r| r != used),
        "path should now be via the other relay: {now:?} (dead: {})",
        used.short()
    );

    if used == r1_id {
        r2.node.shutdown();
    } else {
        r1.node.shutdown();
    }
    a.node.shutdown();
    b.node.shutdown();
}
