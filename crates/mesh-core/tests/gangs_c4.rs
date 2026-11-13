//! C4 — gang scheduling: all-or-nothing reservations (C0–C9 §8).
//!
//! Before this milestone nothing in the tree could hold a resource on
//! behalf of a decision not yet made, so *"eight nodes, four cores each,
//! simultaneously, or don't start"* was not expressible — and two large
//! `parallel` jobs could each acquire half the mesh and neither finish.
//!
//! These tests pin the properties §14.4 names, in the order they matter:
//! the livelock test first, because it is the one that would pass by
//! accident under a scheduler that simply got lucky.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_proto::{
    GangOutcome, GangPlacement, IpcRequest, IpcResponse, JobPriority, Liveness, Requirements,
};
use mesh_testkit::{
    TestNode, capacity, founder_user_key, gang_to_end, ipc_as, join_new_node_sized,
    spawn_founder_sized, wait_until,
};

/// A gang submission. `--gang N` rides the environment exactly as the CLI
/// puts it there, so these tests exercise the same path a person does.
fn gang_req(
    command: &str,
    size: u32,
    reqs: Requirements,
    placement: Option<GangPlacement>,
) -> IpcRequest {
    let mut env = reqs.to_env();
    env.push((mesh_proto::REQ_GANG.to_string(), size.to_string()));
    if let Some(p) = placement {
        env.push((
            mesh_proto::REQ_GANG_PLACEMENT.to_string(),
            p.as_str().to_string(),
        ));
    }
    IpcRequest::Run {
        command: command.into(),
        on: None,
        env,
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
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

fn cpus(n: u32) -> Requirements {
    Requirements {
        cpus: n,
        mem_mb: 0,
        disk_mb: 0,
    }
}

// ---------------------------------------------------------------------------
// P2 — all-or-nothing admission
// ---------------------------------------------------------------------------

/// The headline: every member starts, or none does.
#[tokio::test(flavor = "multi_thread")]
async fn a_gang_starts_every_member_together() {
    let a = spawn_founder_sized("alpha", "c4-mesh", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    let c = join_new_node_sized("charlie", &a, capacity(4, 8, 100)).await;
    assert!(
        all_alive(&[&a, &b, &c]).await,
        "membership did not converge"
    );
    let user = founder_user_key(&a).clone();

    let report = gang_to_end(
        &a,
        &user,
        gang_req(
            "echo rank $MESHWARE_GANG_RANK of $MESHWARE_GANG_SIZE",
            3,
            cpus(4),
            None,
        ),
    )
    .await;

    assert!(report.started(), "gang never started: {:?}", report.error);
    assert_eq!(report.members.len(), 3);
    assert!(report.ok(), "{report:?}");
    assert_eq!(report.succeeded, 3);

    // Every rank ran, each was told its own rank, and the ranks are
    // distinct — which is what makes `torchrun --node_rank` work with no
    // meshware-specific code in the script.
    let mut ranks: Vec<String> = report.stdout.iter().map(|s| s.trim().to_string()).collect();
    ranks.sort();
    assert_eq!(
        ranks,
        vec!["rank 0 of 3", "rank 1 of 3", "rank 2 of 3"],
        "{report:?}"
    );
}

/// A mesh too small refuses cleanly. Nothing runs, and the message says
/// what was actually short — the difference between a sizing problem and a
/// queueing one.
#[tokio::test(flavor = "multi_thread")]
async fn a_mesh_too_small_refuses_the_gang_rather_than_half_running_it() {
    let a = spawn_founder_sized("alpha", "c4-small", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    assert!(all_alive(&[&a, &b]).await, "membership did not converge");
    let user = founder_user_key(&a).clone();

    // Six members of four cores each on two four-core nodes.
    let report = gang_to_end(&a, &user, gang_req("echo never", 6, cpus(4), None)).await;
    assert!(
        !report.started(),
        "a six-node gang must not start here: {report:?}"
    );
    let message = report.error.unwrap_or_default();
    assert!(
        message.contains("gang of 6"),
        "the refusal must name what was short: {message}"
    );

    // …and nothing ran. The whole claim of the milestone is that a refused
    // gang leaves no work behind.
    let jobs = a.node.engine().read(|s| s.jobs.len());
    assert_eq!(jobs, 0, "a refused gang must not have started any member");
}

/// `pack` is the default, and it means it: a gang that fits on one node
/// goes on one node, because packing minimises the inter-task network path.
#[tokio::test(flavor = "multi_thread")]
async fn pack_is_the_default_and_spread_is_available() {
    let big = spawn_founder_sized("big", "c4-place", capacity(16, 32, 400)).await;
    let small = join_new_node_sized("small", &big, capacity(4, 8, 100)).await;
    assert!(
        all_alive(&[&big, &small]).await,
        "membership did not converge"
    );
    let user = founder_user_key(&big).clone();

    let packed = gang_to_end(&big, &user, gang_req("echo p", 4, cpus(4), None)).await;
    assert!(packed.ok(), "{packed:?}");
    assert!(
        packed.members.iter().all(|n| n == "big"),
        "pack must fill the biggest node first: {:?}",
        packed.members
    );

    let spread = gang_to_end(
        &big,
        &user,
        gang_req("echo s", 2, cpus(4), Some(GangPlacement::Spread)),
    )
    .await;
    assert!(spread.ok(), "{spread:?}");
    let mut names = spread.members.clone();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        2,
        "spread must use one node per member: {spread:?}"
    );
}

// ---------------------------------------------------------------------------
// P3 — seniority and wound-wait: the important test (§14.4)
// ---------------------------------------------------------------------------

/// **Two gangs that each want most of the mesh both complete, in seniority
/// order, within a bounded number of offer rounds.**
///
/// This is the test the milestone exists for. Under retry-with-backoff the
/// two gangs each acquire half the mesh, refuse each other, release, and do
/// it again forever — the livelock is not a rare race, it is the *expected*
/// behaviour at exactly the moment it matters most. Under wound-wait the
/// senior gang takes the junior one's reservations, runs, and then the
/// junior one runs.
#[tokio::test(flavor = "multi_thread")]
async fn two_large_gangs_both_complete_and_neither_livelocks() {
    let a = spawn_founder_sized("alpha", "c4-livelock", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    let c = join_new_node_sized("charlie", &a, capacity(4, 8, 100)).await;
    let d = join_new_node_sized("delta", &a, capacity(4, 8, 100)).await;
    assert!(
        all_alive(&[&a, &b, &c, &d]).await,
        "membership did not converge"
    );
    let user = founder_user_key(&a).clone();

    // Two gangs of three on a mesh of four: neither fits beside the other,
    // and each can grab enough to block the other.
    let senior = gang_to_end(&a, &user, gang_req("echo first", 3, cpus(4), None));
    // A beat later, so the tickets are unambiguous: the first submission is
    // senior, and seniority is monotonic in submit time. Both are driven
    // concurrently on this node — the contention is the point.
    let junior = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        gang_to_end(&a, &user, gang_req("echo second", 3, cpus(4), None)).await
    };
    let (first, second) = tokio::join!(senior, junior);

    assert!(first.ok(), "the senior gang must run: {first:?}");
    assert!(second.ok(), "the junior gang must run too: {second:?}");

    // Bounded, not merely eventual. A scheduler that livelocked would blow
    // through the round cap and report `never-started` for one of them.
    assert!(
        second.rounds <= mesh_proto::MAX_OFFER_ROUNDS,
        "the junior gang took {} rounds: {second:?}",
        second.rounds
    );
}

/// Priority is the outer key here as it is everywhere else (C1's rule): a
/// later high-priority gang is senior to an earlier normal one.
#[test]
fn priority_outranks_arrival_and_time_breaks_the_rest() {
    let early = mesh_proto::GangTicket::new(
        JobPriority::Normal,
        mesh_proto::Hlc {
            wall_micros: 10,
            logical: 0,
        },
        mesh_proto::GangId([1u8; 16]),
    );
    let late_but_urgent = mesh_proto::GangTicket::new(
        JobPriority::High,
        mesh_proto::Hlc {
            wall_micros: 10_000,
            logical: 0,
        },
        mesh_proto::GangId([2u8; 16]),
    );
    assert!(late_but_urgent.is_senior_to(&early));
    assert!(!early.is_senior_to(&late_but_urgent));
}

// ---------------------------------------------------------------------------
// P1 — the lease itself
// ---------------------------------------------------------------------------

/// A reservation is visible where an operator would look for it, and it is
/// counted as taken rather than free — the column that explains a node with
/// free slots refusing work.
#[tokio::test(flavor = "multi_thread")]
async fn leases_are_visible_and_ordinary_work_does_not_double_book_them() {
    let a = spawn_founder_sized("alpha", "c4-lease", capacity(8, 16, 200)).await;
    let user = founder_user_key(&a).clone();

    let jobs = a.node.jobs();
    let ticket = mesh_proto::GangTicket::new(
        JobPriority::Normal,
        a.node.engine().clock().now(),
        mesh_proto::GangId([3u8; 16]),
    );
    // Hold the whole node, as an offer from some origin would.
    let reply = jobs.offer_lease(ticket.gang, ticket, cpus(8), 1, 30, None);
    assert!(
        matches!(reply, mesh_proto::GangOfferReply::Accepted { .. }),
        "{reply:?}"
    );

    let IpcResponse::Top(rows) = ipc_as(&a, &user, IpcRequest::Top).await else {
        panic!("expected top rows");
    };
    let row = rows.iter().find(|r| r.is_self).unwrap();
    assert_eq!(row.leases, 1);
    assert_eq!(
        row.leased_cpus, 8,
        "the reservation is counted, not ignored"
    );

    // …and ordinary work cannot have those cores. This is the whole of
    // all-or-nothing admission: a lease is a promise the scheduler keeps.
    let outcome = mesh_testkit::run_to_end(
        &a,
        &user,
        IpcRequest::Run {
            command: "echo squeezed".into(),
            on: None,
            env: cpus(8).to_env(),
            timeout_secs: Some(30),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    let message = match outcome {
        mesh_testkit::RunOutcome::Error(m) => m,
        other => panic!("a leased node must not admit the same cores twice: {other:?}"),
    };
    assert!(
        message.contains("no live node") || message.contains("saturated"),
        "unhelpful refusal: {message}"
    );

    // Give it back, and the node is a node again.
    jobs.release_lease(&ticket.gang);
    assert!(jobs.leases().is_empty());
}

/// The failure model, stated as a test: nobody has to notice an origin
/// dying, because the lease returns on its own clock.
#[tokio::test(flavor = "multi_thread")]
async fn a_lease_expires_without_anybody_cleaning_it_up() {
    let a = spawn_founder_sized("alpha", "c4-ttl", capacity(8, 16, 200)).await;
    let jobs = a.node.jobs();
    let ticket = mesh_proto::GangTicket::new(
        JobPriority::Normal,
        a.node.engine().clock().now(),
        mesh_proto::GangId([4u8; 16]),
    );
    // One second, which the holder is free to grant: the cap is a ceiling,
    // not a floor.
    let reply = jobs.offer_lease(ticket.gang, ticket, cpus(8), 1, 1, None);
    assert!(matches!(reply, mesh_proto::GangOfferReply::Accepted { .. }));
    assert_eq!(jobs.leases().len(), 1);

    // The origin dies here. No release is ever sent, and no cleanup path is
    // ever run — there isn't one.
    assert!(
        wait_until(Duration::from_secs(10), || jobs.leases().is_empty()).await,
        "the lease should have expired on its own"
    );
    assert!(jobs.commit_lease(&ticket.gang).is_none(), "and it is gone");
}

/// The holder caps the TTL, whatever the offer asked for. A remote origin
/// naming its own would be a way to park somebody else's cores for an hour.
#[tokio::test(flavor = "multi_thread")]
async fn the_holder_caps_the_lease_it_is_asked_to_keep() {
    let a = spawn_founder_sized("alpha", "c4-cap", capacity(8, 16, 200)).await;
    let jobs = a.node.jobs();
    let ticket = mesh_proto::GangTicket::new(
        JobPriority::Normal,
        a.node.engine().clock().now(),
        mesh_proto::GangId([5u8; 16]),
    );
    jobs.offer_lease(ticket.gang, ticket, cpus(1), 1, 86_400, None);
    let held = jobs.leases();
    assert_eq!(held.len(), 1);
    let ceiling = mesh_proto::now_micros() + mesh_proto::MAX_LEASE_TTL_SECS * 1_000_000 + 1_000_000;
    assert!(
        held[0].expires_at_micros <= ceiling,
        "an hour-long lease was granted: {:?}",
        held[0]
    );
}

// ---------------------------------------------------------------------------
// P5 / P6 — the record, and what it is for
// ---------------------------------------------------------------------------

/// The gang plan replicates, so `gangs ls` and `gangs show` work from any
/// node — and so a successor can tell an uncommitted gang from a committed
/// one without asking anybody.
#[tokio::test(flavor = "multi_thread")]
async fn the_gang_record_replicates_and_reads_back() {
    let a = spawn_founder_sized("alpha", "c4-record", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    assert!(all_alive(&[&a, &b]).await, "membership did not converge");
    let user = founder_user_key(&a).clone();

    let report = gang_to_end(&a, &user, gang_req("echo recorded", 2, cpus(4), None)).await;
    assert!(report.ok(), "{report:?}");
    let gang = report.gang.unwrap();

    // Readable on the origin…
    let IpcResponse::Gangs(rows) = ipc_as(&a, &user, IpcRequest::Gangs { all: true }).await else {
        panic!("expected gang rows");
    };
    let row = rows.iter().find(|r| r.gang == gang).unwrap();
    assert_eq!(row.size, 2);
    assert_eq!(row.outcome, Some(GangOutcome::Succeeded));

    // …and on the other node, which never drove it.
    assert!(
        wait_until(Duration::from_secs(20), || {
            b.node.engine().read(|s| s.gangs.contains_key(&gang))
        })
        .await,
        "the plan did not replicate"
    );
    let IpcResponse::GangShow(view) = ipc_as(&b, &user, IpcRequest::GangShow { gang }).await else {
        panic!("expected a gang view");
    };
    assert_eq!(view.members.len(), 2);
    assert_eq!(view.members[0].rank, 0);
    assert!(
        view.members.iter().all(|m| m.state.is_some()),
        "every member should have a job state by now: {view:?}"
    );
    // Nothing was senior to it: it was the only gang in the mesh.
    assert!(view.ahead.is_empty(), "{view:?}");
}

/// A closed gang is out of the way by default and findable with `--all` —
/// the noun is about what is holding resources *now*.
#[tokio::test(flavor = "multi_thread")]
async fn finished_gangs_leave_the_live_list() {
    let a = spawn_founder_sized("alpha", "c4-live", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    assert!(all_alive(&[&a, &b]).await, "membership did not converge");
    let user = founder_user_key(&a).clone();

    let report = gang_to_end(&a, &user, gang_req("echo done", 2, cpus(4), None)).await;
    assert!(report.ok(), "{report:?}");

    let live = match ipc_as(&a, &user, IpcRequest::Gangs { all: false }).await {
        IpcResponse::Gangs(rows) => rows,
        other => panic!("{other:?}"),
    };
    assert!(
        live.is_empty(),
        "a finished gang is not in flight: {live:?}"
    );
    let all = match ipc_as(&a, &user, IpcRequest::Gangs { all: true }).await {
        IpcResponse::Gangs(rows) => rows,
        other => panic!("{other:?}"),
    };
    assert_eq!(all.len(), 1);
}

/// A gang whose member dies is torn down rather than left on a barrier
/// nobody will reach. Partial survival is not offered because nothing in
/// the workload class tolerates it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_member_takes_the_gang_with_it() {
    let a = spawn_founder_sized("alpha", "c4-death", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    assert!(all_alive(&[&a, &b]).await, "membership did not converge");
    let user = founder_user_key(&a).clone();

    // Rank 0 fails immediately; the others would otherwise sit in `sleep`
    // for a minute waiting for a rendezvous that will never happen.
    let report = gang_to_end(
        &a,
        &user,
        gang_req(
            "if [ \"$MESHWARE_GANG_RANK\" = 0 ]; then exit 3; fi; sleep 45",
            2,
            cpus(4),
            None,
        ),
    )
    .await;

    assert!(report.started(), "{report:?}");
    assert_eq!(
        report.outcome,
        Some(GangOutcome::MemberDied),
        "a hung barrier must be reported as one: {report:?}"
    );
    // …and the survivor did not run for its full 45 seconds. The test
    // returning at all is the assertion; the outcome names why.
    assert!(report.failed >= 1, "{report:?}");
}

/// `--gang 1` is a job, and the mesh says so rather than buying a two-phase
/// handshake for nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_gang_of_one_is_just_a_job() {
    let a = spawn_founder_sized("alpha", "c4-one", capacity(4, 8, 100)).await;
    let user = founder_user_key(&a).clone();
    // The daemon-side rule: `gang_size_of` ignores 0 and 1, so this runs
    // down the ordinary single-job path and succeeds.
    let outcome =
        mesh_testkit::run_to_end(&a, &user, gang_req("echo plain", 1, cpus(1), None)).await;
    assert!(
        matches!(
            outcome,
            mesh_testkit::RunOutcome::Done {
                state: mesh_proto::JobState::Succeeded,
                ..
            }
        ),
        "{outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// P5 — origin death, and the failure model that makes it cheap
// ---------------------------------------------------------------------------

/// **The design's most important property, as a test.** The origin dies
/// between the plan and the offer; its leases expire on their own, a
/// successor adopts the plan, sees that the gang never committed, and
/// re-drives the offer phase from scratch.
///
/// Nothing in this path is a cleanup: no node is asked to release anything
/// on a dead origin's behalf, and no state has to be reconciled. That is
/// the whole argument for a TTL — there is nothing to clean up.
#[tokio::test(flavor = "multi_thread")]
async fn an_uncommitted_gang_is_re_driven_after_its_origin_dies() {
    let a = spawn_founder_sized("alpha", "c4-adopt", capacity(4, 8, 100)).await;
    let b = mesh_testkit::join_new_node_with("bravo", &a, vec!["worker".into()], |c| {
        mesh_testkit::apply_capacity_for_test(c, capacity(4, 8, 100));
    })
    .await;
    let c = mesh_testkit::join_new_node_with("charlie", &a, vec!["worker".into()], |c| {
        mesh_testkit::apply_capacity_for_test(c, capacity(4, 8, 100));
    })
    .await;
    assert!(
        all_alive(&[&a, &b, &c]).await,
        "membership did not converge"
    );
    let user = founder_user_key(&a).clone();

    // A plan the origin published and then never acted on — exactly the
    // state a node that died between `GangPlan` and its first offer leaves
    // behind. Built by hand because the interesting case is precisely the
    // one the driver never reaches.
    let request = IpcRequest::Run {
        command: "echo adopted-gang".into(),
        on: Some("worker".into()),
        env: cpus(4).to_env(),
        timeout_secs: Some(60),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let auth = user.sign_request(&request).unwrap();
    let gang = mesh_proto::GangId::generate();
    let members: Vec<mesh_proto::GangMember> = [&b, &c]
        .iter()
        .map(|n| mesh_proto::GangMember {
            job_id: mesh_proto::JobId::generate(),
            node_id: n.node.node_id(),
            node_name: n.node.config().node_name.clone(),
            addr: n.addr_string(),
        })
        .collect();
    let plan = mesh_proto::GangPlanRecord {
        gang,
        ticket: mesh_proto::GangTicket::new(
            JobPriority::Normal,
            a.node.engine().clock().now(),
            gang,
        ),
        payload: mesh_proto::JobPayload::Command("echo adopted-gang".into()),
        env: cpus(4).to_env(),
        timeout_secs: Some(60),
        priority: JobPriority::Normal,
        submitter: a.node.node_id(),
        submitter_user: user.user_id(),
        auth,
        members: members.clone(),
        placement: GangPlacement::Spread,
        restart: false,
        attempt: 0,
        created_at_micros: mesh_proto::now_micros(),
    };
    a.node
        .engine()
        .author_op(mesh_proto::OpBody::GangPlan {
            plan: Box::new(plan),
        })
        .unwrap();

    let (be, ce) = (b.node.engine().clone(), c.node.engine().clone());
    assert!(
        wait_until(Duration::from_secs(20), || {
            be.read(|s| s.gangs.contains_key(&gang)) && ce.read(|s| s.gangs.contains_key(&gang))
        })
        .await,
        "the gang plan should replicate via gossip"
    );

    // The origin dies. It never offered, so there are no leases anywhere —
    // and if there had been, they would have expired without anybody
    // noticing.
    a.node.shutdown();

    // Failure detection, then adoption, then a full offer round on nodes
    // that never heard an offer for this gang before.
    assert!(
        wait_until(Duration::from_secs(120), || {
            be.read(|s| s.gangs.get(&gang).is_some_and(|e| e.closed.is_some()))
        })
        .await,
        "a successor must adopt and re-drive the uncommitted gang"
    );

    let (outcome, adopters) = be.read(|s| {
        let e = &s.gangs[&gang];
        (e.closed.expect("closed"), e.adopters.clone())
    });
    assert!(
        !adopters.is_empty() && !adopters.contains(&a.node.node_id()),
        "adoption must be recorded, and the dead origin cannot be the adopter"
    );
    assert_eq!(
        outcome.0,
        GangOutcome::Succeeded,
        "the re-driven gang should have run: {outcome:?}"
    );
    // Every member really ran, under the adopter's ownership.
    //
    // Waited for rather than read once: the outcome above is authored by
    // the adopter and the member *summaries* are authored by the executors,
    // so the two reach this node on separate gossip rounds. Reading the
    // second the instant the first lands is a race the engine is entitled
    // to lose, and a test that lost it would be reporting replication lag
    // as an adoption bug.
    assert!(
        wait_until(Duration::from_secs(30), || {
            be.read(|s| {
                members.iter().all(|m| {
                    s.jobs
                        .get(&m.job_id)
                        .is_some_and(|j| j.state == mesh_proto::JobState::Succeeded)
                })
            })
        })
        .await,
        "both members should have succeeded"
    );
}

/// **A gang's blast radius is the set it will hold.** Change control has to
/// see all N, or an approver signs off on one machine while eight get held.
///
/// The M11 reasoning applied to C4: a gate judged against a *selector*
/// approves "restart the web tier" and executes "restart everything"; a
/// gate judged against *one node* approves one machine and holds eight.
/// Both are the same bug in different directions.
#[tokio::test(flavor = "multi_thread")]
async fn change_control_sees_every_node_a_gang_will_hold() {
    let a = spawn_founder_sized("alpha", "c4-blast", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    let c = join_new_node_sized("charlie", &a, capacity(4, 8, 100)).await;
    assert!(
        all_alive(&[&a, &b, &c]).await,
        "membership did not converge"
    );
    let user = founder_user_key(&a).clone();

    let mut rx = mesh_testkit::stream_as(
        &a,
        &user,
        IpcRequest::DryRun {
            request: Box::new(gang_req(
                "echo blast",
                3,
                cpus(4),
                Some(GangPlacement::Spread),
            )),
        },
    )
    .await;
    let mut preview = None;
    while let Some(frame) = rx.recv().await {
        if let IpcResponse::JobPreview(p) = frame {
            preview = Some(p);
            break;
        }
    }
    let preview = preview.expect("a dry run answers with a preview");
    assert_eq!(
        preview.blast_radius, 3,
        "a gang of three holds three nodes, and the gate must say so: {:?}",
        preview.targets
    );

    // …and a *packed* gang that lands twice on one node counts that node
    // once. A blast radius that double-counted a machine would trip a
    // ceiling nobody set.
    let big = spawn_founder_sized("big", "c4-blast-pack", capacity(16, 32, 400)).await;
    let user2 = founder_user_key(&big).clone();
    let mut rx = mesh_testkit::stream_as(
        &big,
        &user2,
        IpcRequest::DryRun {
            request: Box::new(gang_req("echo packed", 4, cpus(4), None)),
        },
    )
    .await;
    while let Some(frame) = rx.recv().await {
        if let IpcResponse::JobPreview(p) = frame {
            assert_eq!(
                p.blast_radius, 1,
                "four packed members on one node is one machine touched: {:?}",
                p.targets
            );
            return;
        }
    }
    panic!("a dry run answers with a preview");
}

/// `--gang-restart` re-runs the whole thing, as a **new gang** — because
/// the members that did not die have journaled their job ids, and an
/// executor treats a journaled id as accepted.
#[tokio::test(flavor = "multi_thread")]
async fn gang_restart_re_runs_the_gang_under_a_new_id() {
    let a = spawn_founder_sized("alpha", "c4-restart", capacity(4, 8, 100)).await;
    let b = join_new_node_sized("bravo", &a, capacity(4, 8, 100)).await;
    assert!(all_alive(&[&a, &b]).await, "membership did not converge");
    let user = founder_user_key(&a).clone();

    // Rank 0 fails every time, so the gang exhausts its restarts. That is
    // the case the attempt bound exists for: a crashing script must not
    // restart the gang forever.
    let mut env = cpus(4).to_env();
    env.push((mesh_proto::REQ_GANG.to_string(), "2".into()));
    env.push((mesh_proto::REQ_GANG_RESTART.to_string(), "1".into()));
    let report = gang_to_end(
        &a,
        &user,
        IpcRequest::Run {
            command: "if [ \"$MESHWARE_GANG_RANK\" = 0 ]; then exit 3; fi; sleep 30".into(),
            on: None,
            env,
            timeout_secs: Some(60),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    assert!(report.started(), "{report:?}");

    // Every attempt is its own replicated gang, chained by attempt number,
    // and the chain is bounded.
    let plans: Vec<(u32, GangOutcome)> = a.node.engine().read(|s| {
        let mut v: Vec<(u32, GangOutcome)> = s
            .gangs
            .values()
            .filter_map(|e| e.closed.map(|(o, _)| (e.plan.attempt, o)))
            .collect();
        v.sort_by_key(|(a, _)| *a);
        v
    });
    assert_eq!(
        plans.len() as u32,
        mesh_proto::MAX_GANG_ATTEMPTS,
        "the restart chain must be bounded: {plans:?}"
    );
    assert!(
        plans.iter().all(|(_, o)| *o == GangOutcome::MemberDied),
        "{plans:?}"
    );
    assert_eq!(
        plans.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        (0..mesh_proto::MAX_GANG_ATTEMPTS).collect::<Vec<_>>(),
        "attempts should be numbered from zero: {plans:?}"
    );
}
