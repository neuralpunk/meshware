//! C5 — devices: the resource model, reconsidered (C0–C9 §9).
//!
//! `cap.gpus` used to appear in exactly three places in the tree: a comment
//! predicting it and two tests asserting the parser tolerated it. There was
//! no discovery, no allocation, no isolation and no scheduling. These tests
//! pin what replaced that, in the order §14.4 names it — device leases
//! under concurrent admission, a `SIGKILL`ed job's device reusable
//! immediately, and driver-change re-advertisement — with the encoding
//! decision of §9.1 tested first, because it is the one this milestone
//! could not revisit later.
//!
//! Devices are **stated rather than probed** here, for the same reason
//! capacity is in the M9 tests: every in-process node shares one host and
//! CI has no accelerator in it. Everything downstream of the probe is the
//! production path.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{DeviceRequest, IpcRequest, IpcResponse, JobPriority, JobState, Liveness};
use mesh_testkit::{
    RunOutcome, TestNode, advertise_gpus, capacity, founder_user_key, gpu, ipc_as,
    join_new_node_sized, run_to_end, spawn_founder_sized, stream_as, wait_until,
};

/// A run that asks for accelerators. The request rides `MESHWARE_TASK_DEVICES`
/// exactly as the CLI puts it there, so these tests exercise the same path a
/// person does.
fn gpu_req(command: &str, devices: DeviceRequest) -> IpcRequest {
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env: devices.to_env(),
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    }
}

fn want(count: u32) -> DeviceRequest {
    DeviceRequest {
        count,
        ..Default::default()
    }
}

/// Where a reading from the NVIDIA probe counts its ordinals.
fn nvidia_gpu() -> mesh_proto::OrdinalSpace {
    mesh_proto::OrdinalSpace {
        vendor: "nvidia".into(),
        kind: mesh_proto::DeviceKind::Gpu,
    }
}

async fn all_alive(nodes: &[&TestNode]) -> bool {
    let n = nodes.len();
    wait_until(Duration::from_secs(30), || {
        nodes.iter().all(|a| {
            a.node.engine().read(|s| s.members.len() == n)
                && nodes.iter().all(|b| {
                    b.node.node_id() == a.node.node_id()
                        || matches!(
                            a.node.gossip().liveness_of(&b.node.node_id()),
                            Liveness::Alive
                        )
                })
        })
    })
    .await
}

/// Wait until `viewer` can see `n` devices belonging to `owner`.
async fn devices_visible(viewer: &TestNode, owner: &TestNode, n: usize) -> bool {
    let id = owner.node.node_id();
    wait_until(Duration::from_secs(30), || {
        viewer
            .node
            .engine()
            .read(|s| s.devices.get(&id).map(|inv| inv.devices.len()) == Some(n))
    })
    .await
}

// ---------------------------------------------------------------------------
// P1 — the encoding, which is the whole argument (§9.1)
// ---------------------------------------------------------------------------

/// **The property the entity exists for**: `meshnode nodes --devices` lists
/// every accelerator in the mesh, *from any node*, with no discovery of its
/// own and nobody asked.
///
/// This is what a `cap.gpu.0.model=…` tag namespace could not have given —
/// not because a string cannot hold a model name, but because a lease has
/// to refer to a device *identity*, and the row below carries one.
#[tokio::test(flavor = "multi_thread")]
async fn every_accelerator_is_visible_from_every_node() {
    let plain = spawn_founder_sized("plain", "c5-encoding", capacity(8, 16, 200)).await;
    let boxx = join_new_node_sized("gpubox", &plain, capacity(32, 128, 900)).await;
    assert!(all_alive(&[&plain, &boxx]).await, "membership");
    let user = founder_user_key(&plain).clone();

    advertise_gpus(&boxx, 2, "NVIDIA GeForce RTX 4090", 24_564);
    assert!(
        devices_visible(&plain, &boxx, 2).await,
        "the inventory did not reach the other node"
    );

    // Asked of the node that has *no* accelerators, which is the whole
    // point: the answer is replicated state, not a probe.
    let IpcResponse::Devices(rows) =
        ipc_as(&plain, &user, IpcRequest::Devices { node: None }).await
    else {
        panic!("expected a device list");
    };
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|r| r.node_name == "gpubox"));
    assert_eq!(rows[0].model, "NVIDIA GeForce RTX 4090");
    assert_eq!(rows[0].mem_mb, 24_564);
    assert_eq!(rows[0].uuid, "GPU-test-0");
    // A hold is node-local, so the node answering about somebody else's
    // devices says nothing about who holds them rather than guessing.
    assert!(rows.iter().all(|r| r.held_by.is_none()));

    // And the coarse filter tags rode the ordinary member update, so a
    // selector can answer "has a GPU" without parsing any of that.
    let tags = plain.node.engine().read(|s| {
        s.members
            .get(&boxx.node.node_id())
            .map(|m| m.info.tags.clone())
            .unwrap_or_default()
    });
    assert!(tags.iter().any(|t| t == "cap.gpus=2"), "{tags:?}");
    assert!(tags.iter().any(|t| t == "cap.vram_mb=24564"), "{tags:?}");
}

/// A node may only describe its own hardware. An inventory authored *about*
/// another node is rejected at merge — the same rule that keeps `cap.*` tags
/// out of a user-supplied tag list, and for the same reason: an operator
/// elsewhere asserting a GPU onto a box is how work lands where it cannot
/// run.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_cannot_advertise_somebody_elses_hardware() {
    let a = spawn_founder_sized("alpha", "c5-authorship", capacity(8, 16, 200)).await;
    let b = join_new_node_sized("bravo", &a, capacity(8, 16, 200)).await;
    assert!(all_alive(&[&a, &b]).await, "membership");

    // `alpha` claims `bravo` has four A100s.
    let forged = mesh_proto::DeviceInventory::new(
        b.node.node_id(),
        (0..4).map(|i| gpu(i, "NVIDIA A100", 40_960)).collect(),
        mesh_proto::now_micros(),
    );
    let err = a
        .node
        .engine()
        .author_op(mesh_proto::OpBody::DeviceInventoryPut {
            inventory: Box::new(forged),
        });
    assert!(err.is_err(), "a forged inventory must not be authored");
    assert!(
        a.node
            .engine()
            .read(|s| !s.devices.contains_key(&b.node.node_id())),
        "and it must not land in state either"
    );
}

// ---------------------------------------------------------------------------
// P3 — admission (§9.3)
// ---------------------------------------------------------------------------

/// **A 2-GPU job is refused by the 1-GPU node and admitted by the 4-GPU
/// node.**
///
/// A filter, not a penalty. A node without the cards is not a bad candidate
/// for a two-GPU job, it is not a candidate — and the *submitting* node here
/// is the small one, which would otherwise win on RTT.
#[tokio::test(flavor = "multi_thread")]
async fn a_two_gpu_job_lands_only_where_two_gpus_are() {
    let small = spawn_founder_sized("small", "c5-admission", capacity(8, 16, 200)).await;
    let big = join_new_node_sized("big", &small, capacity(32, 128, 900)).await;
    assert!(all_alive(&[&small, &big]).await, "membership");
    let user = founder_user_key(&small).clone();

    advertise_gpus(&small, 1, "NVIDIA GeForce RTX 4090", 24_564);
    advertise_gpus(&big, 4, "NVIDIA A100-SXM4-40GB", 40_960);
    assert!(devices_visible(&small, &big, 4).await, "inventory");

    let outcome = run_to_end(
        &small,
        &user,
        gpu_req("echo $CUDA_VISIBLE_DEVICES", want(2)),
    )
    .await;
    let (stdout, job_id) = match outcome {
        RunOutcome::Done {
            stdout,
            state,
            job_id,
            ..
        } => {
            assert_eq!(state, JobState::Succeeded);
            (stdout, job_id.expect("job id"))
        }
        other => panic!("expected a run: {other:?}"),
    };
    let executor = small
        .node
        .engine()
        .read(|s| s.jobs.get(&job_id).map(|j| j.executor));
    assert_eq!(
        executor,
        Some(big.node.node_id()),
        "a two-GPU job must not land on the one-GPU node"
    );
    // …and it was given exactly two specific devices.
    let seen: Vec<&str> = stdout.trim().split(',').filter(|s| !s.is_empty()).collect();
    assert_eq!(seen.len(), 2, "expected two devices, got {stdout:?}");
}

/// A mesh with no such hardware refuses by *name*, not by timing out or by
/// queueing forever. "No node advertises 8×gpu" and "no room right now" are
/// different problems with different fixes, and the message says which.
#[tokio::test(flavor = "multi_thread")]
async fn a_mesh_without_the_hardware_says_so() {
    let a = spawn_founder_sized("alpha", "c5-nohw", capacity(8, 16, 200)).await;
    let user = founder_user_key(&a).clone();
    advertise_gpus(&a, 2, "NVIDIA GeForce RTX 4090", 24_564);

    let outcome = run_to_end(&a, &user, gpu_req("echo never", want(8))).await;
    match outcome {
        RunOutcome::Error(message) => {
            assert!(
                message.contains("8×gpu"),
                "the refusal must name what was missing: {message}"
            );
        }
        other => panic!("expected a refusal naming the hardware: {other:?}"),
    }
}

/// The VRAM filter is **per device, never a total**. Four 12 GB cards have
/// 48 GB between them and cannot run a 40 GB task — a filter that summed
/// would send the job to a node that then refused it.
#[tokio::test(flavor = "multi_thread")]
async fn vram_is_per_device_and_the_filter_knows_it() {
    let many = spawn_founder_sized("many", "c5-vram", capacity(16, 64, 400)).await;
    let user = founder_user_key(&many).clone();
    many.node
        .advertise_devices_for_test((0..4).map(|i| gpu(i, "Tesla K80", 12_288)).collect());

    let big = DeviceRequest {
        count: 1,
        vram_mb: 40_000,
        ..Default::default()
    };
    match run_to_end(&many, &user, gpu_req("echo never", big)).await {
        RunOutcome::Error(message) => assert!(message.contains("≥40000MB"), "{message}"),
        other => panic!("48 GB of small cards is not one 40 GB card: {other:?}"),
    }
}

/// A device request nothing in the mesh matches is refused **by name**,
/// even when the coarse tag says the node has a GPU.
///
/// The gap this covers: `cap.gpus` is a count, so a node with a 4090 looks
/// like a candidate for a job that asked for an A100, and the refusal then
/// arrives from *admission* rather than from placement. Reporting that as
/// "every node is saturated" would be true about the executor's answer and
/// useless about the cause.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_nothing_matches_is_refused_by_name() {
    let boxx = spawn_founder_sized("gpubox", "c5-nomodel", capacity(16, 64, 400)).await;
    let user = founder_user_key(&boxx).clone();
    advertise_gpus(&boxx, 2, "NVIDIA GeForce RTX 4090", 24_564);

    let mut req = DeviceRequest::default();
    req.apply_selector("model=~A100").expect("selector");
    match run_to_end(&boxx, &user, gpu_req("echo never", req)).await {
        RunOutcome::Error(message) => assert!(
            message.contains("A100"),
            "the refusal must name the request: {message}"
        ),
        other => panic!("a 4090 is not an A100: {other:?}"),
    }
}

/// A model selector picks the right card out of a mixed box. `~4090` means
/// "contains 4090", which is what somebody typing a model fragment means.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_selector_picks_the_right_card() {
    let mixed = spawn_founder_sized("mixed", "c5-model", capacity(16, 64, 400)).await;
    let user = founder_user_key(&mixed).clone();
    mixed.node.advertise_devices_for_test(vec![
        gpu(0, "NVIDIA A100-SXM4-40GB", 40_960),
        gpu(1, "NVIDIA GeForce RTX 4090", 24_564),
    ]);

    let mut req = DeviceRequest::default();
    req.apply_selector("model=~4090").expect("selector");
    let outcome = run_to_end(&mixed, &user, gpu_req("echo $CUDA_VISIBLE_DEVICES", req)).await;
    match outcome {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert_eq!(stdout.trim(), "1", "the 4090 is device 1");
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P4 — allocation and isolation (§9.3, §14.4)
// ---------------------------------------------------------------------------

/// **Two single-GPU jobs on a 2-GPU node each see exactly one device, and
/// not the same one.**
///
/// This is the isolation claim, and it is tested through the environment the
/// vendor runtimes actually read rather than through the ledger, because the
/// ledger being right about a mask the task never sees would be worth
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn two_jobs_on_a_two_gpu_node_get_one_device_each() {
    let boxx = spawn_founder_sized("gpubox", "c5-isolation", capacity(16, 64, 400)).await;
    let user = founder_user_key(&boxx).clone();
    advertise_gpus(&boxx, 2, "NVIDIA GeForce RTX 4090", 24_564);

    // Both in flight at once: the second is submitted while the first still
    // holds its device, which is the only arrangement that tests anything.
    let first = stream_as(
        &boxx,
        &user,
        gpu_req("echo $CUDA_VISIBLE_DEVICES; sleep 2", want(1)),
    )
    .await;
    // Let the first job take its device before the second asks.
    assert!(
        wait_until(Duration::from_secs(10), || !boxx
            .node
            .jobs()
            .device_holders()
            .is_empty())
        .await,
        "the first job never took a device"
    );
    let second = run_to_end(&boxx, &user, gpu_req("echo $CUDA_VISIBLE_DEVICES", want(1))).await;

    let second_device = match second {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            stdout.trim().to_string()
        }
        other => panic!("the second single-GPU job should fit: {other:?}"),
    };

    let mut rx = first;
    let mut first_out = String::new();
    while let Some(msg) = rx.recv().await {
        if let IpcResponse::LogChunk { stream: 1, data } = msg {
            first_out.push_str(&String::from_utf8_lossy(&data));
        }
    }
    let first_device = first_out.trim().to_string();
    assert!(!first_device.is_empty() && !second_device.is_empty());
    assert_ne!(
        first_device, second_device,
        "two jobs were handed the same card"
    );
    assert!(!first_device.contains(','), "one device, not both");
    assert!(!second_device.contains(','), "one device, not both");
}

/// A job that asked for **no** devices sees none — the masks are set to
/// empty rather than left unset.
///
/// Unset means *every device on the box* to every vendor runtime there is.
/// Isolation that only holds for the jobs that asked for it is not
/// isolation.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_asked_for_nothing_sees_nothing() {
    let boxx = spawn_founder_sized("gpubox", "c5-masks", capacity(16, 64, 400)).await;
    let user = founder_user_key(&boxx).clone();
    advertise_gpus(&boxx, 2, "NVIDIA GeForce RTX 4090", 24_564);

    let outcome = run_to_end(
        &boxx,
        &user,
        IpcRequest::Run {
            command: "echo \"[${CUDA_VISIBLE_DEVICES-unset}][${HIP_VISIBLE_DEVICES-unset}]\""
                .into(),
            on: None,
            env: Vec::new(),
            timeout_secs: Some(60),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    match outcome {
        RunOutcome::Done { stdout, .. } => {
            assert_eq!(stdout.trim(), "[][]", "the masks must be empty, not unset");
        }
        other => panic!("{other:?}"),
    }
}

/// A job cannot talk its way into a device by setting the mask itself.
///
/// `CUDA_VISIBLE_DEVICES` is not a reserved key — it is the *vendor's*
/// variable, and reserving it would break every legitimate use of it inside
/// a task's own subprocesses. What stops it from being a way in is
/// ordering: the allocation is applied last, after the submission's own
/// environment, so what a task sees is what it was given.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_cannot_grant_itself_a_device_through_its_own_env() {
    let boxx = spawn_founder_sized("gpubox", "c5-forge", capacity(16, 64, 400)).await;
    let user = founder_user_key(&boxx).clone();
    advertise_gpus(&boxx, 2, "NVIDIA GeForce RTX 4090", 24_564);

    let outcome = run_to_end(
        &boxx,
        &user,
        IpcRequest::Run {
            command: "echo [$CUDA_VISIBLE_DEVICES]".into(),
            on: None,
            env: vec![("CUDA_VISIBLE_DEVICES".to_string(), "0,1".to_string())],
            timeout_secs: Some(60),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    match outcome {
        RunOutcome::Done { stdout, .. } => assert_eq!(
            stdout.trim(),
            "[]",
            "a job that asked for no devices must see none, whatever it set"
        ),
        other => panic!("{other:?}"),
    }
}

/// **§14.4: a `SIGKILL`ed job's device is reusable immediately.**
///
/// A release is a table edit, not a driver call, which is what makes this
/// true rather than eventually true. The job here is cancelled — the harshest
/// ending a test can arrange in-process — and the ledger is empty the moment
/// the executor notices.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_jobs_device_comes_straight_back() {
    let boxx = spawn_founder_sized("gpubox", "c5-release", capacity(16, 64, 400)).await;
    let user = founder_user_key(&boxx).clone();
    advertise_gpus(&boxx, 1, "NVIDIA GeForce RTX 4090", 24_564);

    let mut rx = stream_as(&boxx, &user, gpu_req("sleep 120", want(1))).await;
    let mut job_id = None;
    while let Some(msg) = rx.recv().await {
        if let IpcResponse::JobStarted { job_id: id, .. } = msg {
            job_id = Some(id);
            break;
        }
    }
    let job_id = job_id.expect("the job started");
    assert!(
        wait_until(Duration::from_secs(10), || boxx
            .node
            .jobs()
            .device_holders()
            .len()
            == 1)
        .await,
        "the job never took its device"
    );

    ipc_as(&boxx, &user, IpcRequest::JobCancel { job_id }).await;
    assert!(
        wait_until(Duration::from_secs(15), || boxx
            .node
            .jobs()
            .device_holders()
            .is_empty())
        .await,
        "the device was still held after the job was cancelled"
    );

    // And it is immediately reusable — not merely unheld.
    let outcome = run_to_end(&boxx, &user, gpu_req("echo $CUDA_VISIBLE_DEVICES", want(1))).await;
    match outcome {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert_eq!(stdout.trim(), "0");
        }
        other => panic!("the freed device was not reusable: {other:?}"),
    }
}

/// **§14.4: device leases under concurrent admission.** Three single-GPU
/// jobs against two cards: two run, and the third does not get a card that
/// was already promised.
///
/// The check and the reservation are under one lock, which is why this
/// cannot come out as three jobs sharing two devices. Nothing here asserts
/// *which* two win — that is a scheduling decision — only that the mesh
/// never hands the same device to two exclusive holders.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_admissions_never_double_book_a_device() {
    let boxx = spawn_founder_sized("gpubox", "c5-concurrent", capacity(16, 64, 400)).await;
    let user = founder_user_key(&boxx).clone();
    advertise_gpus(&boxx, 2, "NVIDIA GeForce RTX 4090", 24_564);

    let mut streams = Vec::new();
    for _ in 0..3 {
        streams.push(stream_as(&boxx, &user, gpu_req("sleep 3", want(1))).await);
    }
    // Whatever the outcome for the third, the ledger must never show one
    // index held twice — that is the invariant, and it is checked while the
    // jobs are still in flight.
    assert!(
        wait_until(Duration::from_secs(10), || {
            !boxx.node.jobs().device_holders().is_empty()
        })
        .await,
        "nothing took a device"
    );
    for _ in 0..20 {
        let holders = boxx.node.jobs().device_holders();
        assert!(
            holders.len() <= 2,
            "two cards cannot be held three times: {holders:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for mut rx in streams {
        while rx.recv().await.is_some() {}
    }
}

/// A job that queues for a device gets it when the device comes back — and
/// **not before**.
///
/// The gap this closes: admission checks devices when a job *arrives*, and
/// a job that then waits in the queue can have its card taken by something
/// that started in between. Promoting it anyway would start a GPU job with
/// no GPU, with the authoritative check silently skipped because it lives
/// at acceptance and this is a promotion.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_gpu_job_waits_for_a_card_rather_than_starting_without_one() {
    // One slot and one card, so the second job must queue on both counts.
    let boxx = spawn_founder_sized("gpubox", "c5-queue", capacity(1, 8, 100)).await;
    let user = founder_user_key(&boxx).clone();
    advertise_gpus(&boxx, 1, "NVIDIA GeForce RTX 4090", 24_564);

    let first = stream_as(&boxx, &user, gpu_req("sleep 2", want(1))).await;
    assert!(
        wait_until(Duration::from_secs(10), || boxx
            .node
            .jobs()
            .device_holders()
            .len()
            == 1)
        .await,
        "the first job never took the card"
    );

    // The second asks for the same single card and must come back with it —
    // eventually, and holding it exclusively, never sharing it.
    let second = run_to_end(&boxx, &user, gpu_req("echo $CUDA_VISIBLE_DEVICES", want(1))).await;
    match second {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert_eq!(stdout.trim(), "0", "it must get the real card, not none");
        }
        other => panic!("{other:?}"),
    }

    let mut rx = first;
    while rx.recv().await.is_some() {}
    assert!(
        boxx.node.jobs().device_holders().is_empty(),
        "every hold should be back once both jobs are done"
    );
}

/// A device the driver has taken out of service is neither advertised nor
/// allocated. Degraded is different: idling a working card over one
/// corrected ECC error is the wrong trade, so it stays schedulable.
#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_device_is_not_advertised_and_a_degraded_one_still_is() {
    let boxx = spawn_founder_sized("gpubox", "c5-health", capacity(16, 64, 400)).await;
    let user = founder_user_key(&boxx).clone();

    let mut dead = gpu(0, "NVIDIA GeForce RTX 4090", 24_564);
    dead.health = mesh_proto::DeviceHealth::Unavailable {
        reason: "3 uncorrected ECC error(s)".into(),
    };
    let mut degraded = gpu(1, "NVIDIA GeForce RTX 4090", 24_564);
    degraded.health = mesh_proto::DeviceHealth::Degraded {
        reason: "1 corrected ECC error".into(),
    };
    boxx.node.advertise_devices_for_test(vec![dead, degraded]);

    // The filter tag counts one card, not two.
    let tags = boxx.node.engine().read(|s| {
        s.members
            .get(&boxx.node.node_id())
            .map(|m| m.info.tags.clone())
            .unwrap_or_default()
    });
    assert!(tags.iter().any(|t| t == "cap.gpus=1"), "{tags:?}");

    // Both appear in the inventory — an operator has to be able to see the
    // broken one, and the reason — but only the degraded one is allocated.
    let IpcResponse::Devices(rows) = ipc_as(&boxx, &user, IpcRequest::Devices { node: None }).await
    else {
        panic!("expected a device list");
    };
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r.health == "unavailable"
        && r.health_reason.as_deref() == Some("3 uncorrected ECC error(s)")));

    let outcome = run_to_end(&boxx, &user, gpu_req("echo $CUDA_VISIBLE_DEVICES", want(1))).await;
    match outcome {
        RunOutcome::Done { stdout, .. } => assert_eq!(
            stdout.trim(),
            "1",
            "the unavailable card must not be handed out"
        ),
        other => panic!("{other:?}"),
    }
}

/// A gang whose origin died holding device reservations does not park the
/// cards: the lease's TTL takes them back, and the devices go with it.
///
/// The same failure model C4 states for cores, one level down — there is no
/// distributed cleanup path because there is nothing to clean up. Asserted
/// through the ledger rather than through a job, because the point is that
/// *nobody has to notice*.
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_gang_lease_gives_its_devices_back() {
    let boxx = spawn_founder_sized("gpubox", "c5-expiry", capacity(16, 64, 400)).await;
    advertise_gpus(&boxx, 1, "NVIDIA GeForce RTX 4090", 24_564);

    // A one-second offer for a gang whose origin then vanishes.
    let ticket = mesh_proto::GangTicket::new(
        JobPriority::Normal,
        mesh_proto::Hlc {
            wall_micros: mesh_proto::now_micros(),
            logical: 0,
        },
        mesh_proto::GangId([9; 16]),
    );
    let reply = boxx.node.jobs().offer_lease(
        ticket.gang,
        ticket,
        mesh_proto::Requirements {
            cpus: 1,
            mem_mb: 0,
            disk_mb: 0,
        },
        1,
        1,
        Some(want(1)),
    );
    assert!(
        matches!(reply, mesh_proto::GangOfferReply::Accepted { .. }),
        "{reply:?}"
    );
    assert_eq!(
        boxx.node.jobs().device_holders().len(),
        1,
        "the card is held"
    );

    // Nobody commits, nobody releases, nobody is watching.
    assert!(
        wait_until(Duration::from_secs(20), || boxx
            .node
            .jobs()
            .device_holders()
            .is_empty())
        .await,
        "the expired lease kept its card"
    );

    // And the card is genuinely usable again, not merely unlisted.
    let user = founder_user_key(&boxx).clone();
    match run_to_end(&boxx, &user, gpu_req("echo $CUDA_VISIBLE_DEVICES", want(1))).await {
        RunOutcome::Done { stdout, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            assert_eq!(stdout.trim(), "0");
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P6 — day two (§14.4)
// ---------------------------------------------------------------------------

/// **§14.4: driver-change re-advertisement.** A card pulled from a running
/// box stops being advertised, mesh-wide, without a restart — both the
/// entity a drill-in reads and the tag a selector filters on.
///
/// Publishing one without the other is how a mesh ends up sending GPU work
/// to a node that no longer has one, so both halves are asserted.
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_change_re_advertises_without_a_restart() {
    let plain = spawn_founder_sized("plain", "c5-day2", capacity(8, 16, 200)).await;
    let boxx = join_new_node_sized("gpubox", &plain, capacity(32, 128, 900)).await;
    assert!(all_alive(&[&plain, &boxx]).await, "membership");

    advertise_gpus(&boxx, 4, "NVIDIA A100-SXM4-40GB", 40_960);
    assert!(devices_visible(&plain, &boxx, 4).await, "initial inventory");

    // Somebody pulls two cards. No restart, no reload — the re-probe path.
    advertise_gpus(&boxx, 2, "NVIDIA A100-SXM4-40GB", 40_960);
    assert!(
        devices_visible(&plain, &boxx, 2).await,
        "the entity did not follow the hardware"
    );
    assert!(
        wait_until(Duration::from_secs(30), || {
            plain.node.engine().read(|s| {
                s.members
                    .get(&boxx.node.node_id())
                    .is_some_and(|m| m.info.tags.iter().any(|t| t == "cap.gpus=2"))
            })
        })
        .await,
        "the filter tag did not follow the hardware"
    );

    // And the mesh now refuses a four-GPU job it would have accepted a
    // moment ago, which is the only observable that matters.
    let user = founder_user_key(&plain).clone();
    match run_to_end(&plain, &user, gpu_req("echo never", want(4))).await {
        RunOutcome::Error(message) => assert!(message.contains("4×gpu"), "{message}"),
        other => panic!("expected a refusal after the cards were pulled: {other:?}"),
    }
}

/// Live telemetry moves placement: given two GPU nodes, work goes to the one
/// whose cards are actually idle rather than to the one with fewer jobs.
///
/// The term is a *term* rather than a filter, deliberately — a busy device is
/// a real candidate when every device is busy — so this asserts the ordering
/// it produces rather than an exclusion.
#[tokio::test(flavor = "multi_thread")]
async fn placement_prefers_the_node_whose_gpus_are_idle() {
    let busy = spawn_founder_sized("busy", "c5-pressure", capacity(16, 64, 400)).await;
    let idle = join_new_node_sized("idle", &busy, capacity(16, 64, 400)).await;
    assert!(all_alive(&[&busy, &idle]).await, "membership");
    let user = founder_user_key(&busy).clone();

    advertise_gpus(&busy, 1, "NVIDIA A100-SXM4-40GB", 40_960);
    advertise_gpus(&idle, 1, "NVIDIA A100-SXM4-40GB", 40_960);
    assert!(devices_visible(&busy, &idle, 1).await, "inventory");

    // The submitting node's own card is saturated; the other's is free. Note
    // that `busy` would otherwise win outright on RTT.
    busy.node
        .set_device_pressure_for_test(vec![mesh_proto::DevicePressure {
            index: 0,
            space: Some(nvidia_gpu()),
            util_pct: Some(99.0),
            mem_free_mb: Some(128),
            temp_c: Some(84.0),
            throttled: Some(true),
            processes: 2,
        }]);
    idle.node
        .set_device_pressure_for_test(vec![mesh_proto::DevicePressure {
            index: 0,
            space: Some(nvidia_gpu()),
            util_pct: Some(0.0),
            mem_free_mb: Some(40_000),
            temp_c: Some(31.0),
            throttled: Some(false),
            processes: 0,
        }]);
    // Placement reads a peer's telemetry from the gossip tick, so wait for
    // one to have carried it.
    assert!(
        wait_until(Duration::from_secs(30), || {
            !busy
                .node
                .gossip()
                .device_pressure_of(&idle.node.node_id())
                .is_empty()
        })
        .await,
        "device telemetry never reached the other node"
    );

    let outcome = run_to_end(&busy, &user, gpu_req("echo placed", want(1))).await;
    let job_id = match outcome {
        RunOutcome::Done { job_id, state, .. } => {
            assert_eq!(state, JobState::Succeeded);
            job_id.expect("job id")
        }
        other => panic!("{other:?}"),
    };
    let executor = busy
        .node
        .engine()
        .read(|s| s.jobs.get(&job_id).map(|j| j.executor));
    assert_eq!(
        executor,
        Some(idle.node.node_id()),
        "work must go to the idle GPU, not the near one"
    );
}

/// **A Ryzen desktop with a graphics card**: a discrete NVIDIA card and an
/// AMD integrated GPU, both device 0. Each card's reading reaches the rest
/// of the mesh as its own, and a hold on one is not shown on the other.
///
/// Before 1.35 a reading carried only the ordinal, so both cards were shown
/// with whichever reading came first — the NVIDIA card's throttling was the
/// AMD card's too, and Director listed the same GPU as needing attention
/// twice.
#[tokio::test(flavor = "multi_thread")]
async fn two_vendors_device_zero_keep_their_own_readings() {
    let plain = spawn_founder_sized("plain", "c5-mixed", capacity(8, 16, 200)).await;
    let desk = join_new_node_sized("desktop", &plain, capacity(16, 64, 400)).await;
    assert!(all_alive(&[&plain, &desk]).await, "membership");
    let user = founder_user_key(&plain).clone();

    let nvidia = gpu(0, "NVIDIA GeForce RTX 4090", 24_564);
    let amd = mesh_proto::Device {
        vendor: "amd".into(),
        model: "Radeon 680M".into(),
        uuid: "amd-test-0".into(),
        mem_mb: 512,
        ..gpu(0, "", 0)
    };
    desk.node
        .advertise_devices_for_test(vec![nvidia.clone(), amd.clone()]);
    assert!(devices_visible(&plain, &desk, 2).await, "inventory");

    let reading =
        |card: &mesh_proto::Device, util: f64, throttled: bool| mesh_proto::DevicePressure {
            index: 0,
            space: Some(mesh_proto::OrdinalSpace::of(card)),
            util_pct: Some(util),
            mem_free_mb: None,
            temp_c: None,
            throttled: Some(throttled),
            processes: 0,
        };
    let readings = vec![reading(&nvidia, 97.0, true), reading(&amd, 3.0, false)];
    // Set on every poll: the node's own sampler replaces them with what
    // this machine's probe finds, which in CI is nothing.
    assert!(
        wait_until(Duration::from_secs(30), || {
            desk.node.set_device_pressure_for_test(readings.clone());
            plain
                .node
                .gossip()
                .device_pressure_of(&desk.node.node_id())
                .len()
                == 2
        })
        .await,
        "the readings never reached the other node"
    );
    let IpcResponse::Devices(rows) =
        ipc_as(&plain, &user, IpcRequest::Devices { node: None }).await
    else {
        panic!("expected a device list");
    };
    let row = |vendor: &str| rows.iter().find(|r| r.vendor == vendor).unwrap();
    assert_eq!(row("nvidia").util_pct, Some(97.0), "{rows:?}");
    assert_eq!(row("nvidia").throttled, Some(true), "{rows:?}");
    assert_eq!(row("amd").util_pct, Some(3.0), "{rows:?}");
    assert_eq!(row("amd").throttled, Some(false), "{rows:?}");

    // A job holding the NVIDIA card holds that card only.
    let on_nvidia = DeviceRequest {
        count: 1,
        vendor: Some("nvidia".into()),
        ..Default::default()
    };
    let mut rx = stream_as(&desk, &user, gpu_req("sleep 120", on_nvidia)).await;
    let mut job_id = None;
    while let Some(msg) = rx.recv().await {
        if let IpcResponse::JobStarted { job_id: id, .. } = msg {
            job_id = Some(id);
            break;
        }
    }
    let job_id = job_id.expect("the job started");
    assert!(
        wait_until(Duration::from_secs(10), || !desk
            .node
            .jobs()
            .device_holders()
            .is_empty())
        .await,
        "the job never took its device"
    );
    let IpcResponse::Devices(rows) = ipc_as(&desk, &user, IpcRequest::Devices { node: None }).await
    else {
        panic!("expected a device list");
    };
    let row = |vendor: &str| rows.iter().find(|r| r.vendor == vendor).unwrap();
    assert!(row("nvidia").held_by.is_some(), "{rows:?}");
    assert!(row("amd").held_by.is_none(), "{rows:?}");
    ipc_as(&desk, &user, IpcRequest::JobCancel { job_id }).await;
}

// ---------------------------------------------------------------------------
// L2 P1 — the totals strip (L1–L5 §6.2)
// ---------------------------------------------------------------------------

/// **L2 P1's exit demo, as a test.** The one-line spec sheet's GPU figures
/// have to agree with the sum of `meshnode nodes --devices`, and they have
/// to be right *from a node that owns no accelerators* — because the whole
/// claim is that the strip is a fold over replicated state (D-L2.1) rather
/// than a probe or an aggregation service.
///
/// VRAM is the **sum** here, unlike `cap.vram_mb`, which is the largest
/// single card. Both are right and they answer different questions: a
/// filter asks whether one device can hold a model, and a spec sheet asks
/// how much accelerator was bought.
#[tokio::test(flavor = "multi_thread")]
async fn totals_gpu_figures_agree_with_the_device_list() {
    let plain = spawn_founder_sized("plain", "l2-totals", capacity(8, 16, 200)).await;
    let boxx = join_new_node_sized("gpubox", &plain, capacity(32, 128, 900)).await;
    let second = join_new_node_sized("gpubox2", &plain, capacity(32, 128, 900)).await;
    assert!(all_alive(&[&plain, &boxx, &second]).await, "membership");
    let user = founder_user_key(&plain).clone();

    advertise_gpus(&boxx, 4, "NVIDIA A100-SXM4-40GB", 40_960);
    advertise_gpus(&second, 2, "NVIDIA GeForce RTX 4090", 24_564);
    assert!(devices_visible(&plain, &boxx, 4).await, "inventory 1");
    assert!(devices_visible(&plain, &second, 2).await, "inventory 2");

    // Asked of the node with no accelerators of its own.
    let IpcResponse::Devices(rows) =
        ipc_as(&plain, &user, IpcRequest::Devices { node: None }).await
    else {
        panic!("expected a device list");
    };
    let IpcResponse::Totals(totals) = ipc_as(&plain, &user, IpcRequest::Totals).await else {
        panic!("expected totals");
    };

    let listed_vram: u64 = rows.iter().map(|r| r.mem_mb).sum();
    assert_eq!(usize::try_from(totals.gpus).unwrap(), rows.len());
    assert_eq!(totals.vram_mb, listed_vram);
    assert_eq!(totals.gpus, 6);
    assert_eq!(totals.vram_mb, 4 * 40_960 + 2 * 24_564);

    // And the sentence a person reads says the same thing the figures do.
    let strip = totals.strip();
    assert!(strip.contains("3 machines"), "{strip}");
    assert!(strip.contains("6 GPUs (208 GB VRAM)"), "{strip}");
    // Every live node advertised `cap.*`, so nothing is qualified.
    assert_eq!(totals.capacity_from, totals.nodes_live);
    assert_eq!(totals.coverage(), None);
}

/// A card the driver has taken out of service is not capacity anybody can
/// spend, so it leaves the spec sheet — the same rule `TopRow::devices`
/// already follows, applied to the figure that gets shared.
#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_card_leaves_the_spec_sheet() {
    let plain = spawn_founder_sized("plain", "l2-totals-health", capacity(8, 16, 200)).await;
    let boxx = join_new_node_sized("gpubox", &plain, capacity(32, 128, 900)).await;
    assert!(all_alive(&[&plain, &boxx]).await, "membership");
    let user = founder_user_key(&plain).clone();

    let mut cards = vec![
        gpu(0, "NVIDIA A100-SXM4-40GB", 40_960),
        gpu(1, "NVIDIA A100-SXM4-40GB", 40_960),
    ];
    cards[1].health = mesh_proto::DeviceHealth::Unavailable {
        reason: "fell off the bus".into(),
    };
    boxx.node.advertise_devices_for_test(cards);
    assert!(devices_visible(&plain, &boxx, 2).await, "inventory");

    let IpcResponse::Totals(totals) = ipc_as(&plain, &user, IpcRequest::Totals).await else {
        panic!("expected totals");
    };
    assert_eq!(totals.gpus, 1, "the dead card is not capacity");
    assert_eq!(totals.vram_mb, 40_960);
}
